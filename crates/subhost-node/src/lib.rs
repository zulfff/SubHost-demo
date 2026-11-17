//! Node bootstrap: genesis, ledger, JSON-RPC, metrics, and peer gossip in one
//! place.
//!
//! This is the single place that decides how a running node is assembled, so the
//! CLI, tests, and container entrypoint cannot drift apart.
//!
//! Scope of the networking: a node publishes every block its own producer
//! seals, and applies blocks received from peers through
//! [`RpcState::accept_block`], which re-validates the whole block. Blocks that
//! arrive before their parent are buffered briefly and applied in height order.
//! There is no peer scoring, fork choice, or block request/response protocol.

use bincode::Options;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use subhost_core::{encode_canonical, Block, BlockHeight, ChainId, GenesisConfig};
use subhost_metrics::{Metrics, MetricsConfig};
use subhost_network::{NetworkConfig, NetworkHandle, NetworkManager, NetworkMessage};
use subhost_rpc::{BlockEvent, RpcServer, RpcServerConfig, RpcState};
use subhost_storage::LedgerStore;
use tracing::{debug, info, warn};

/// Re-exported so callers can configure networking without depending on
/// `subhost-network` directly.
pub use subhost_network::NetworkConfig as P2pConfig;

/// Genesis file name inside the data directory.
pub const GENESIS_FILE_NAME: &str = "genesis.json";

/// Everything needed to start a node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Directory holding `genesis.json` and the ledger file.
    pub data_dir: PathBuf,
    /// JSON-RPC listen address.
    pub rpc_addr: SocketAddr,
    /// Maximum concurrent JSON-RPC connections.
    pub max_rpc_connections: u32,
    /// Optional Prometheus exporter address.
    pub metrics_addr: Option<SocketAddr>,
    /// Whether this node intends to validate, which requires a validator set.
    pub validator: bool,
    /// Chain ID used when the data directory has no genesis file.
    pub fallback_chain_id: ChainId,
    /// Peer gossip settings. `None` keeps the node single-node and offline.
    pub network: Option<NetworkConfig>,
}

impl NodeConfig {
    /// A loopback-only configuration rooted at `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            rpc_addr: SocketAddr::from(([127, 0, 0, 1], 8545)),
            max_rpc_connections: 1_000,
            metrics_addr: None,
            validator: false,
            fallback_chain_id: 1,
            network: None,
        }
    }

    pub fn genesis_path(&self) -> PathBuf {
        self.data_dir.join(GENESIS_FILE_NAME)
    }
}

/// A booted node: ledger restored, genesis applied, ready to serve.
pub struct Node {
    state: RpcState,
    metrics: Metrics,
    config: NodeConfig,
    genesis: Option<GenesisConfig>,
}

impl Node {
    /// Load genesis, restore or create the ledger, and apply allocations once.
    ///
    /// Genesis allocations are applied only when no ledger exists yet, so a
    /// restart can never re-mint balances over a live chain.
    pub fn bootstrap(config: NodeConfig) -> Result<Self, NodeError> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|source| NodeError::DataDir { path: config.data_dir.clone(), source })?;

        let genesis = load_genesis(&config.genesis_path())?;
        if config.validator {
            // Fail fast rather than starting a validator that can never form a quorum.
            if let Some(genesis) = &genesis {
                genesis.requires_validators()?;
            } else {
                return Err(NodeError::MissingGenesis(config.genesis_path()));
            }
        }

        let chain_id =
            genesis.as_ref().map_or(config.fallback_chain_id, |genesis| genesis.chain_id);
        if chain_id == 0 {
            return Err(NodeError::InvalidChainId);
        }

        let store = LedgerStore::open(chain_id, &config.data_dir)?;
        let fresh = !store.exists();
        let state = RpcState::with_store(store)?;

        if fresh {
            if let Some(genesis) = &genesis {
                if !genesis.allocations.is_empty() {
                    state.seed_accounts(
                        genesis.allocations.iter().map(|(address, balance)| (*address, *balance)),
                    )?;
                    info!(accounts = genesis.allocations.len(), "applied genesis allocations");
                }
            }
        } else {
            info!(height = state.block_height(), "restored existing ledger");
        }

        if genesis.is_none() {
            warn!(
                path = %config.genesis_path().display(),
                chain_id,
                "no genesis file found; starting with an empty allocation set"
            );
        }

        let metrics = Metrics::new()?;
        metrics.set_block_height(state.block_height());
        metrics.set_pending_transactions(state.pending_count());

        Ok(Self { state, metrics, config, genesis })
    }

    pub fn state(&self) -> &RpcState {
        &self.state
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn genesis(&self) -> Option<&GenesisConfig> {
        self.genesis.as_ref()
    }

    pub fn chain_id(&self) -> ChainId {
        self.state.chain_id()
    }

    /// Serve JSON-RPC, and the metrics exporter when configured, until either
    /// stops or the process receives a shutdown signal.
    pub async fn run(self) -> Result<(), NodeError> {
        let rpc_config = RpcServerConfig {
            listen_addr: self.config.rpc_addr,
            max_connections: self.config.max_rpc_connections,
        };
        info!(
            chain_id = self.chain_id(),
            height = self.state.block_height(),
            data_dir = %self.config.data_dir.display(),
            validator = self.config.validator,
            "starting node"
        );

        let metrics_task = self.config.metrics_addr.map(|addr| {
            let metrics = self.metrics.clone();
            tokio::spawn(async move { metrics.serve(MetricsConfig { listen_addr: addr }).await })
        });

        // Keep metrics in step with the ledger while the node serves.
        let observer = {
            let metrics = self.metrics.clone();
            let state = self.state.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    ticker.tick().await;
                    metrics.set_block_height(state.block_height());
                    metrics.set_pending_transactions(state.pending_count());
                }
            })
        };

        let network_task = self.config.network.clone().map(|config| {
            let state = self.state.clone();
            tokio::spawn(async move {
                if let Err(error) = run_network(state, config).await {
                    warn!(%error, "peer gossip stopped");
                }
            })
        });

        let rpc = RpcServer::new(self.state.clone());
        let result = tokio::select! {
            result = rpc.run(rpc_config) => result.map_err(NodeError::from),
            result = shutdown_signal() => result,
        };

        observer.abort();
        if let Some(task) = network_task {
            task.abort();
        }
        if let Some(task) = metrics_task {
            task.abort();
        }
        result
    }
}

/// How far ahead of the local tip a peer block may be buffered.
const MAX_BLOCKS_AHEAD: BlockHeight = 64;

/// Holds peer blocks that arrived before their parent.
///
/// Blocks at or below the next expected height are dropped: they are already
/// committed or cannot be replayed. Blocks further than [`MAX_BLOCKS_AHEAD`] are
/// refused outright so a peer cannot make the node buffer unbounded history.
#[derive(Debug, Default)]
struct BlockBuffer {
    pending: BTreeMap<BlockHeight, Block>,
}

impl BlockBuffer {
    /// Keep a block for a future height, or discard it as stale/far-future.
    fn offer(&mut self, block: Block, next_height: BlockHeight) {
        let height = block.header.height;
        if height < next_height || height > next_height.saturating_add(MAX_BLOCKS_AHEAD) {
            debug!(height, next_height, "discarding an out-of-range peer block");
            return;
        }
        // The first block seen for a height wins; a duplicate cannot replace it.
        self.pending.entry(height).or_insert(block);
    }

    fn pop_next(&mut self, next_height: BlockHeight) -> Option<Block> {
        self.pending.remove(&next_height)
    }

    fn len(&self) -> usize {
        self.pending.len()
    }
}

/// Apply every buffered block that now extends the local tip.
///
/// A rejection is logged and the block dropped: `accept_block` is deterministic,
/// so retrying the same block cannot succeed.
fn apply_buffered(state: &RpcState, buffer: &mut BlockBuffer) {
    loop {
        let next = state.block_height().saturating_add(1);
        let Some(block) = buffer.pop_next(next) else { break };
        let hash = block.hash();
        match state.accept_block(block) {
            Ok(receipts) => info!(
                height = next,
                %hash,
                transactions = receipts.len(),
                "accepted a peer block"
            ),
            Err(error) => warn!(height = next, %hash, %error, "rejected a peer block"),
        }
    }
}

/// Canonical bytes for a block on the gossip wire.
fn encode_block(block: &Block) -> Vec<u8> {
    encode_canonical(block)
}

/// Decode a peer block with an explicit size limit.
///
/// `bincode::options` must use the same integer encoding as
/// [`subhost_core::encode_canonical`] (fixed width, little endian); the limit
/// stops a short payload from requesting a huge allocation.
fn decode_block(data: &[u8]) -> Result<Block, bincode::Error> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(subhost_network::MAX_TRANSMIT_BYTES as u64)
        .deserialize(data)
}

/// Pump gossip until the process shuts down.
///
/// The swarm and the application loop live in one task, so aborting the task
/// stops both. Nothing is spawned here that could outlive the node.
async fn run_network(state: RpcState, config: NetworkConfig) -> Result<(), NodeError> {
    let (manager, handle) = NetworkManager::new(config)?;
    let local_peer_id = handle.local_peer_id;
    let NetworkHandle { outbound, mut inbound, .. } = handle;
    info!(%local_peer_id, "starting peer gossip");

    let mut block_events = state.subscribe_blocks();
    let mut buffer = BlockBuffer::default();

    tokio::select! {
        result = manager.run() => {
            if let Err(error) = result {
                warn!(%error, "gossip swarm stopped");
            }
            Ok(())
        }
        () = pump_gossip(&state, &outbound, &mut inbound, &mut block_events, &mut buffer) => {
            Ok(())
        }
    }
}

/// Apply inbound blocks and relay locally sealed ones.
async fn pump_gossip(
    state: &RpcState,
    outbound: &tokio::sync::mpsc::Sender<NetworkMessage>,
    inbound: &mut tokio::sync::mpsc::Receiver<NetworkMessage>,
    block_events: &mut tokio::sync::broadcast::Receiver<BlockEvent>,
    buffer: &mut BlockBuffer,
) {
    loop {
        tokio::select! {
            message = inbound.recv() => match message {
                Some(NetworkMessage::BlockProposal { data }) => match decode_block(&data) {
                    Ok(block) => {
                        buffer.offer(block, state.block_height().saturating_add(1));
                        debug!(buffered = buffer.len(), "evaluating a peer block");
                        apply_buffered(state, buffer);
                    }
                    Err(error) => debug!(%error, "discarding an undecodable peer block"),
                },
                // Other flows exist on the wire but are not driven here.
                Some(_) => {}
                None => break,
            },
            event = block_events.recv() => match event {
                // Only relay blocks this node produced; a received block already
                // reached every peer through gossip.
                Ok(BlockEvent::Sealed(block)) => {
                    let message = NetworkMessage::BlockProposal { data: encode_block(&block) };
                    if outbound.try_send(message).is_err() {
                        debug!("outbound gossip queue is full; a block was not relayed");
                    }
                }
                Ok(BlockEvent::Accepted(block)) => {
                    // The local ledger moved; buffered successors may now fit.
                    debug!(height = block.header.height, "local tip advanced from a peer block");
                    apply_buffered(state, buffer);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "block event observer fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

/// Read `genesis.json` when present, validating it before returning it.
fn load_genesis(path: &Path) -> Result<Option<GenesisConfig>, NodeError> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(GenesisConfig::load(path)?))
}

/// Resolve when the process is asked to terminate.
async fn shutdown_signal() -> Result<(), NodeError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).map_err(NodeError::ShutdownSignal)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(NodeError::ShutdownSignal)?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(NodeError::ShutdownSignal)?;
    }
    info!("shutdown signal received; stopping node");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("cannot create data directory {path}: {source}")]
    DataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("a validator node requires a genesis file at {0}")]
    MissingGenesis(PathBuf),

    #[error("chain ID cannot be zero")]
    InvalidChainId,

    #[error(transparent)]
    Genesis(#[from] subhost_core::CoreError),

    #[error(transparent)]
    Storage(#[from] subhost_storage::StorageError),

    #[error(transparent)]
    Rpc(#[from] subhost_rpc::RpcError),

    #[error(transparent)]
    RpcServe(#[from] subhost_rpc::RpcServeError),

    #[error(transparent)]
    Metrics(#[from] subhost_metrics::MetricsError),

    #[error("cannot install shutdown signal handler: {0}")]
    ShutdownSignal(#[source] std::io::Error),

    #[error(transparent)]
    Network(#[from] subhost_network::NetworkError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use subhost_core::{
        Address, Transaction, TransactionSignature, TransactionType, ValidatorInfo,
    };

    fn genesis_with(chain_id: ChainId, allocation: Option<(Address, u128)>) -> GenesisConfig {
        let mut genesis = GenesisConfig { chain_id, ..Default::default() };
        if let Some((address, balance)) = allocation {
            genesis.allocations.insert(address, balance);
        }
        genesis
    }

    #[test]
    fn bootstrap_creates_the_data_directory_and_defaults_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("nested").join("data");
        let node = Node::bootstrap(NodeConfig::new(&data_dir)).unwrap();

        assert!(data_dir.is_dir(), "the data directory must be created");
        assert_eq!(node.chain_id(), 1);
        assert_eq!(node.state().block_height(), 0);
        assert!(node.genesis().is_none());
    }

    #[test]
    fn bootstrap_applies_genesis_allocations_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let address = Address::new([1; 20]);
        genesis_with(7, Some((address, 500))).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        assert_eq!(node.chain_id(), 7, "genesis chain ID must win");
        assert_eq!(node.state().balance(&address), 500);
        drop(node);

        // A restart must not re-apply allocations over the restored ledger.
        let restarted = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        assert_eq!(restarted.state().balance(&address), 500);
        assert!(restarted.state().has_persisted_state());
    }

    #[test]
    fn bootstrap_rejects_an_invalid_genesis_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(GENESIS_FILE_NAME), "{ not json").unwrap();
        assert!(matches!(Node::bootstrap(NodeConfig::new(dir.path())), Err(NodeError::Genesis(_))));
    }

    #[test]
    fn validator_mode_requires_a_genesis_validator_set() {
        let dir = tempfile::tempdir().unwrap();
        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };

        // No genesis at all.
        assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::MissingGenesis(_))));

        // Genesis with an empty validator set.
        genesis_with(1, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::Genesis(_))));

        // Genesis with a real validator boots.
        let mut genesis = genesis_with(1, None);
        genesis.initial_validators = vec![ValidatorInfo {
            address: Address::new([9; 20]),
            public_key: vec![1, 2, 3],
            power: 100,
            proof_of_possession: Vec::new(),
        }];
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let node = Node::bootstrap(config).unwrap();
        assert_eq!(node.genesis().unwrap().total_voting_power(), 100);
    }

    #[test]
    fn a_zero_fallback_chain_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Node::bootstrap(NodeConfig { fallback_chain_id: 0, ..NodeConfig::new(dir.path()) }),
            Err(NodeError::InvalidChainId)
        ));
    }

    #[test]
    fn a_ledger_from_another_chain_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        genesis_with(7, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        node.state().seed_accounts([(Address::new([1; 20]), 1)]).unwrap();
        drop(node);

        // Rewrite genesis to a different chain over the same ledger.
        genesis_with(8, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        assert!(matches!(Node::bootstrap(NodeConfig::new(dir.path())), Err(NodeError::Storage(_))));
    }

    #[test]
    fn metrics_start_in_step_with_the_restored_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        let rendered = String::from_utf8(node.metrics().encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_block_height 0"));
        assert!(rendered.contains("subhost_pending_transactions 0"));
    }

    #[test]
    fn config_exposes_the_genesis_path_under_the_data_dir() {
        let config = NodeConfig::new("/tmp/subhost-node-test");
        assert_eq!(
            config.genesis_path(),
            PathBuf::from("/tmp/subhost-node-test").join(GENESIS_FILE_NAME)
        );
        assert!(config.metrics_addr.is_none());
        assert!(!config.validator);
    }

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn signed_transfer(key: &SigningKey, nonce: u64, value: u128) -> Transaction {
        let public_key = key.verifying_key().to_bytes();
        let mut tx = Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::from_public_key(&public_key),
            to: Some(Address::new([2; 20])),
            value,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            public_key,
            signature: TransactionSignature::EMPTY,
        };
        tx.signature =
            TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
        tx
    }

    /// A producer with two committed blocks, plus a follower at genesis.
    fn chain_with_two_blocks() -> (RpcState, RpcState, Address) {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let producer = RpcState::in_memory(1).unwrap();
        producer.seed_accounts([(sender, 1_000_000)]).unwrap();
        producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        producer.submit_transaction(signed_transfer(&key, 1, 10)).unwrap();

        let follower = RpcState::in_memory(1).unwrap();
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        (producer, follower, sender)
    }

    #[test]
    fn out_of_order_peer_blocks_are_applied_in_height_order() {
        let (producer, follower, sender) = chain_with_two_blocks();
        let first = producer.block_at_height(1).unwrap();
        let second = producer.block_at_height(2).unwrap();

        let mut buffer = BlockBuffer::default();
        buffer.offer(second, 1);
        buffer.offer(first, 1);
        assert_eq!(buffer.len(), 2);
        apply_buffered(&follower, &mut buffer);

        assert_eq!(follower.block_height(), 2);
        // Two transfers pay both the moved value and 21_000 gas each.
        assert_eq!(follower.balance(&sender), 1_000_000 - 20 - 42_000);
        assert_eq!(follower.balance(&Address::new([2; 20])), 20);
        assert_eq!(buffer.len(), 0, "both buffered blocks were consumed");

        // Replaying committed history is discarded, not re-applied.
        buffer.offer(producer.block_at_height(1).unwrap(), follower.block_height() + 1);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn the_buffer_refuses_stale_and_far_future_blocks() {
        let (producer, _, _) = chain_with_two_blocks();
        let first = producer.block_at_height(1).unwrap();
        let mut buffer = BlockBuffer::default();

        // Below the expected height.
        buffer.offer(first.clone(), 5);
        assert_eq!(buffer.len(), 0);

        // Further ahead than the node is willing to hold.
        let mut future = first.clone();
        future.header.height = 1 + MAX_BLOCKS_AHEAD + 1;
        buffer.offer(future, 1);
        assert_eq!(buffer.len(), 0);

        // At the horizon is allowed, one past it is not.
        let mut horizon = first.clone();
        horizon.header.height = 1 + MAX_BLOCKS_AHEAD;
        buffer.offer(horizon.clone(), 1);
        assert_eq!(buffer.len(), 1);

        // A duplicate cannot replace what is already buffered.
        buffer.offer(horizon, 1);
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn apply_buffered_drops_a_block_that_fails_validation() {
        let (producer, follower, sender) = chain_with_two_blocks();
        let mut tampered = producer.block_at_height(1).unwrap();
        tampered.transactions[0].value += 1;

        let mut buffer = BlockBuffer::default();
        buffer.offer(tampered, 1);
        apply_buffered(&follower, &mut buffer);

        assert_eq!(follower.block_height(), 0, "a forged block must not commit");
        assert_eq!(follower.balance(&sender), 1_000_000);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn blocks_survive_the_gossip_encoding() {
        let (producer, _, _) = chain_with_two_blocks();
        let block = producer.block_at_height(1).unwrap();
        let encoded = encode_block(&block);
        assert_eq!(decode_block(&encoded).unwrap(), block);

        // Truncated input is an error, never a partial block.
        assert!(decode_block(&encoded[..encoded.len() / 2]).is_err());
        assert!(decode_block(b"not a block").is_err());
    }

    #[test]
    fn networking_is_off_unless_configured() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        assert!(node.config().network.is_none());

        let config = NodeConfig {
            network: Some(NetworkConfig { enable_mdns: false, ..Default::default() }),
            ..NodeConfig::new(dir.path())
        };
        let node = Node::bootstrap(config).unwrap();
        assert!(node.config().network.is_some());
    }

    /// Two distinct free loopback ports, both bound until chosen.
    ///
    /// Binding both listeners at once guarantees the two ports differ; asking for
    /// one port twice can return the same number and make one node fail to bind.
    fn two_free_loopback_ports() -> (u16, u16) {
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        (first.local_addr().unwrap().port(), second.local_addr().unwrap().port())
    }

    /// Wait until a libp2p TCP listener accepts connections on `port`.
    ///
    /// A follower dials its bootstrap peer once, at startup. On a loaded CI runner
    /// that dial can happen before the producer has bound its listener, and because
    /// there is no redial the mesh never forms. Waiting for the port first removes
    /// that ordering dependency.
    async fn wait_until_listening(port: u16) {
        for _ in 0..250 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no listener on 127.0.0.1:{port} after 5s");
    }

    /// Two real nodes, real libp2p over loopback TCP, real gossip.
    ///
    /// The follower runs the production [`run_network`] task, so a pass covers
    /// the whole path: swarm delivery, `decode_block`, the out-of-order buffer,
    /// `apply_buffered`, and `RpcState::accept_block` with its full
    /// re-validation. Blocks are re-published until the follower catches up,
    /// which makes the test independent of when the gossip mesh forms; duplicate
    /// heights are dropped by the follower rather than applied twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_nodes_exchange_blocks_over_real_gossip() {
        let (producer, follower, sender) = chain_with_two_blocks();
        let (producer_port, follower_port) = two_free_loopback_ports();

        let (manager, handle) = NetworkManager::new(NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{producer_port}"),
            enable_mdns: false,
            ..NetworkConfig::default()
        })
        .unwrap();
        let producer_task = tokio::spawn(manager.run());
        wait_until_listening(producer_port).await;

        let follower_task = tokio::spawn(run_network(
            follower.clone(),
            NetworkConfig {
                listen_addr: format!("/ip4/127.0.0.1/tcp/{follower_port}"),
                bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{producer_port}")],
                enable_mdns: false,
                ..NetworkConfig::default()
            },
        ));

        let first = producer.block_at_height(1).unwrap();
        let second = producer.block_at_height(2).unwrap();

        let mut caught_up = false;
        for _ in 0..100 {
            // Newest first: the follower must buffer height 2 until height 1
            // arrives, then apply both.
            for block in [&second, &first] {
                let message = NetworkMessage::BlockProposal { data: encode_block(block) };
                if handle.outbound.send(message).await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if follower.block_height() == 2 {
                caught_up = true;
                break;
            }
        }

        assert!(
            caught_up,
            "the follower never applied both blocks; it is at height {}",
            follower.block_height()
        );
        assert_eq!(follower.block_at_height(2).unwrap().hash(), second.hash());
        assert_eq!(follower.state_root(), producer.state_root());
        assert_eq!(follower.balance(&sender), producer.balance(&sender));
        for block in [&first, &second] {
            for tx in &block.transactions {
                assert!(
                    follower.receipt(&tx.hash()).is_some(),
                    "the follower did not derive a receipt for {}",
                    tx.hash()
                );
            }
        }

        follower_task.abort();
        producer_task.abort();
    }
}
