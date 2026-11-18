//! Node bootstrap: genesis, ledger, JSON-RPC, metrics, and peer gossip in one
//! place.
//!
//! This is the single place that decides how a running node is assembled, so the
//! CLI, tests, and container entrypoint cannot drift apart.
//!
//! With a genesis validator set, the node exchanges proposals, verified votes
//! and certified commits, and writes blocks through [`RpcState::commit_qc`].
//! Demo chains without validators use [`RpcState::accept_block`] and buffer
//! blocks that arrive before their parent. There are no consensus timeouts,
//! view changes, peer scoring, fork choice, or historical block sync yet.

mod consensus_journal;
#[cfg(test)]
mod testnet_tests;

use bincode::Options;
use consensus_journal::ConsensusJournal;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use subhost_consensus::{
    driver::{BlockProposal as DriverProposal, ConsensusDriver, Vote, VoteOutcome},
    ConsensusConfig, QuorumCertificate, ValidatorRegistry,
};
use subhost_core::{encode_canonical, Address, Block, BlockHeight, ChainId, GenesisConfig, Hash};
use subhost_crypto::{BLSScheme, PrivateKey};
use subhost_metrics::{Metrics, MetricsConfig};
use subhost_network::{NetworkConfig, NetworkHandle, NetworkManager, NetworkMessage};
use subhost_rpc::{
    BlockAuthority, BlockEvent, RpcSecurityConfig, RpcServer, RpcServerConfig, RpcState,
};
use subhost_storage::LedgerStore;
use tracing::{debug, info, warn};

/// Re-exported so callers can configure networking without depending on
/// `subhost-network` directly.
pub use subhost_network::NetworkConfig as P2pConfig;

/// Genesis file name inside the data directory.
pub const GENESIS_FILE_NAME: &str = "genesis.json";

/// Producer key file name inside the data directory.
///
/// One hex-encoded canonical 32-byte BLS scalar, mode `0600`. It is not the
/// wallet keystore and is not encrypted; `ROADMAP.md` S9.6 replaces it.
pub const PRODUCER_KEY_FILE_NAME: &str = "producer.key";

/// Everything needed to start a node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Directory holding `genesis.json` and the ledger file.
    pub data_dir: PathBuf,
    /// JSON-RPC listen address.
    pub rpc_addr: SocketAddr,
    /// Maximum concurrent JSON-RPC connections.
    pub max_rpc_connections: u32,
    /// Authentication, rate limit and public-bind policy for JSON-RPC.
    pub rpc_security: RpcSecurityConfig,
    /// Optional Prometheus exporter address.
    pub metrics_addr: Option<SocketAddr>,
    /// Whether this node intends to validate, which requires a validator set.
    pub validator: bool,
    /// Explicit unsigned single-writer demo mode; incompatible with validators.
    pub single_writer: bool,
    /// Base consensus view timeout in milliseconds; must be nonzero.
    pub view_timeout_ms: u64,
    /// Producer key file to sign blocks with. `None` means
    /// `<data_dir>/producer.key` when `validator` is set, and no signing
    /// identity otherwise.
    pub producer_key_file: Option<PathBuf>,
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
            rpc_security: RpcSecurityConfig::default(),
            metrics_addr: None,
            validator: false,
            single_writer: false,
            view_timeout_ms: 2_000,
            producer_key_file: None,
            fallback_chain_id: 1,
            network: None,
        }
    }

    /// The producer key path this node will read, if any.
    pub fn producer_key_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.producer_key_file {
            return Some(path.clone());
        }
        self.validator.then(|| self.data_dir.join(PRODUCER_KEY_FILE_NAME))
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
    /// A driver for every node with a validator set. Observers verify without voting.
    consensus: Option<Arc<Mutex<ConsensusDriver>>>,
    consensus_journal: Option<Arc<ConsensusJournal>>,
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

        // Build the signing registry before touching storage: a genesis set that
        // cannot be trusted should not open a ledger at all.
        let validators = match &genesis {
            Some(genesis) if !genesis.initial_validators.is_empty() => {
                Some(ValidatorRegistry::from_genesis(&genesis.initial_validators)?)
            }
            _ => None,
        };

        let mut producer_identity = None;
        let authority = match (config.producer_key_path(), validators.as_ref()) {
            (None, _) => None,
            (Some(path), Some(validators)) => {
                let (producer, secret) = load_producer(&path, genesis.as_ref())?;
                info!(path = %path.display(), %producer, "loaded the producer key");
                producer_identity = Some((producer, secret.clone()));
                Some(BlockAuthority::new(validators.clone(), producer, secret)?)
            }
            (Some(path), None) => return Err(NodeError::NoValidatorSetForProducer(path)),
        };
        let driver_registry = validators.clone();

        let store = LedgerStore::open(chain_id, &config.data_dir)?;
        let fresh = !store.exists();
        let mut state = RpcState::with_store(store)?;
        if let Some(validators) = validators {
            state = state.with_validators(validators);
        }
        if let Some(authority) = authority {
            state = state.with_authority(authority);
        }

        let (consensus, consensus_journal) = match (driver_registry, producer_identity) {
            (Some(registry), identity) => {
                let binding =
                    consensus_genesis_hash(genesis.as_ref().expect("registry requires genesis"));
                let (journal, driver) = ConsensusJournal::open(
                    &config.data_dir,
                    &state,
                    binding,
                    ConsensusConfig::new(registry.len())?,
                    registry,
                    identity,
                    fresh,
                )?;
                (Some(Arc::new(Mutex::new(driver))), Some(Arc::new(journal)))
            }
            _ => (None, None),
        };

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

        Ok(Self { state, metrics, config, genesis, consensus, consensus_journal })
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

    /// The consensus driver, when genesis declares a validator set.
    pub fn consensus(&self) -> Option<&Arc<Mutex<ConsensusDriver>>> {
        self.consensus.as_ref()
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
            security: self.config.rpc_security.clone(),
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

        let network_task = match (self.config.network.clone(), self.consensus.clone()) {
            (Some(config), Some(driver)) => {
                let state = self.state.clone();
                let journal = self.consensus_journal.clone();
                Some(tokio::spawn(async move {
                    if let Err(error) =
                        run_network_with_journal(state, config, driver, journal).await
                    {
                        warn!(%error, "peer gossip stopped");
                    }
                }))
            }
            (Some(config), None) => {
                let state = self.state.clone();
                Some(tokio::spawn(async move {
                    if let Err(error) = run_network(state, config).await {
                        warn!(%error, "peer gossip stopped");
                    }
                }))
            }
            // A validator without peers still has to produce, so the driver runs
            // even when gossip is disabled.
            (None, Some(driver)) => {
                let state = self.state.clone();
                let journal = self.consensus_journal.clone();
                Some(tokio::spawn(produce_blocks(state, Some(driver), None, journal)))
            }
            (None, None) => None,
        };

        let rpc = RpcServer::new(self.state.clone()).with_metrics(self.metrics.clone());
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

fn consensus_genesis_hash(genesis: &GenesisConfig) -> Hash {
    // HashMap iteration order changes across processes; account allocations
    // must be ordered before binding a recovery file to its genesis.
    let mut allocations: Vec<_> =
        genesis.allocations.iter().map(|(address, balance)| (*address, *balance)).collect();
    allocations.sort_unstable();
    Hash::from_data(&encode_canonical(&(
        genesis.chain_id,
        &genesis.initial_validators,
        allocations,
        genesis.block_time_ms,
        genesis.gas_limit,
        genesis.genesis_time,
    )))
}

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

    #[cfg(test)]
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
    run_network_inner(state, config, None, None).await
}

/// Gossip plus block production for a node that holds a consensus driver.
#[cfg(test)]
async fn run_network_with_driver(
    state: RpcState,
    config: NetworkConfig,
    driver: Arc<Mutex<ConsensusDriver>>,
) -> Result<(), NodeError> {
    run_network_inner(state, config, Some(driver), None).await
}

async fn run_network_with_journal(
    state: RpcState,
    config: NetworkConfig,
    driver: Arc<Mutex<ConsensusDriver>>,
    journal: Option<Arc<ConsensusJournal>>,
) -> Result<(), NodeError> {
    run_network_inner(state, config, Some(driver), journal).await
}

async fn run_network_inner(
    state: RpcState,
    config: NetworkConfig,
    driver: Option<Arc<Mutex<ConsensusDriver>>>,
    journal: Option<Arc<ConsensusJournal>>,
) -> Result<(), NodeError> {
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
        () = pump_gossip(&state, &outbound, &mut inbound, &mut block_events, &mut buffer, driver.as_ref(), journal.as_deref()) => {
            Ok(())
        }
        () = produce_blocks(state.clone(), driver.clone(), Some(outbound.clone()), journal.clone()) => {
            Ok(())
        }
    }
}

/// One proposal ready to broadcast, possibly from an observer.
struct ProducedRound {
    proposal: DriverProposal,
    vote: Option<Vote>,
}

/// Commit a certified candidate before advancing the live consensus driver.
/// Valid votes stay in the live tally on I/O failure, so the ticker can retry.
fn commit_ready(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    journal: Option<&ConsensusJournal>,
) -> Result<(), NodeError> {
    let Some(qc) = driver.pending_qc() else { return Ok(()) };
    let block = driver.open_proposal().expect("pending QC requires a proposal").block.clone();
    let mut candidate = driver.clone();
    if !matches!(candidate.commit_open_proposal()?, VoteOutcome::Committed { .. }) {
        return Ok(());
    }
    if let Some(journal) = journal {
        journal.prepare_commit(driver, block.clone(), qc.clone())?;
    }
    state.commit_qc(block, &qc)?;
    *driver = candidate;
    if let Some(journal) = journal {
        journal.save(driver)?;
    }
    Ok(())
}

/// Build a proposal or retry a pending certificate without committing alone.
fn produce_once_inner(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    journal: Option<&ConsensusJournal>,
) -> Result<Option<ProducedRound>, NodeError> {
    if driver.open_proposal().is_none() && driver.is_validator() {
        if let Some(block) = state.build_block()? {
            let mut candidate = driver.clone();
            candidate.propose(block)?;
            adopt_pending(driver, candidate, journal)?;
        }
    }
    let round = driver
        .open_proposal()
        .cloned()
        .map(|proposal| ProducedRound { proposal, vote: driver.own_vote().cloned() });
    commit_ready(state, driver, journal)?;
    Ok(round)
}

#[cfg(test)]
fn produce_once(
    state: &RpcState,
    driver: &mut ConsensusDriver,
) -> Result<Option<ProducedRound>, NodeError> {
    produce_once_inner(state, driver, None)
}

fn adopt_pending(
    driver: &mut ConsensusDriver,
    candidate: ConsensusDriver,
    journal: Option<&ConsensusJournal>,
) -> Result<(), NodeError> {
    if let Some(journal) = journal {
        if candidate.checkpoint() != driver.checkpoint() {
            journal.save(&candidate)?;
        }
    }
    *driver = candidate;
    Ok(())
}

fn vote_message(vote: &Vote) -> NetworkMessage {
    NetworkMessage::Vote {
        view: vote.view,
        validator: *vote.validator.as_bytes(),
        block_hash: *vote.block_hash.as_bytes(),
        signature: vote.signature.clone(),
    }
}

fn relay(outbound: &tokio::sync::mpsc::Sender<NetworkMessage>, message: NetworkMessage) {
    if outbound.try_send(message).is_err() {
        debug!("outbound gossip queue is full; consensus message was not relayed");
    }
}

/// Validate inbound consensus traffic before voting or publishing it.
fn handle_consensus_message_inner(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    message: NetworkMessage,
    journal: Option<&ConsensusJournal>,
) -> Result<Option<Vote>, NodeError> {
    match message {
        NetworkMessage::BlockProposal { view, data } => {
            let block = decode_block(&data)
                .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()))?;
            if block.header.height <= driver.height() {
                return Ok(None);
            }
            state.validate_proposal(&block)?;
            let mut candidate = driver.clone();
            let vote = candidate.observe_proposal(DriverProposal { view, block })?;
            adopt_pending(driver, candidate, journal)?;
            commit_ready(state, driver, journal)?;
            Ok(vote)
        }
        NetworkMessage::Vote { view, validator, block_hash, signature } => {
            let vote = Vote {
                view,
                validator: Address::new(validator),
                block_hash: Hash::new(block_hash),
                signature,
            };
            let mut candidate = driver.clone();
            let fresh = candidate.record_vote(vote.clone())?;
            adopt_pending(driver, candidate, journal)?;
            commit_ready(state, driver, journal)?;
            Ok(fresh.then_some(vote))
        }
        NetworkMessage::BlockCommit { view, data, signatures } => {
            let block = decode_block(&data)
                .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()))?;
            if block.header.height <= driver.height() {
                return Ok(None);
            }
            let qc = QuorumCertificate {
                view,
                block_hash: block.hash(),
                signatures: signatures
                    .into_iter()
                    .map(|(address, signature)| (Address::new(address), signature))
                    .collect(),
            };
            let mut candidate = driver.clone();
            candidate.receive_commit(block.clone(), qc.clone())?;
            if let Some(journal) = journal {
                journal.prepare_commit(driver, block.clone(), qc.clone())?;
            }
            state.commit_qc(block, &qc)?;
            *driver = candidate;
            if let Some(journal) = journal {
                journal.save(driver)?;
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
fn handle_consensus_message(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    message: NetworkMessage,
) -> Result<Option<Vote>, NodeError> {
    handle_consensus_message_inner(state, driver, message, None)
}

/// Retry proposal/vote/commit dissemination as peers join the mesh.
async fn produce_blocks(
    state: RpcState,
    driver: Option<Arc<Mutex<ConsensusDriver>>>,
    outbound: Option<tokio::sync::mpsc::Sender<NetworkMessage>>,
    journal: Option<Arc<ConsensusJournal>>,
) {
    let Some(driver) = driver else {
        std::future::pending::<()>().await;
        return;
    };
    let block_time = std::time::Duration::from_millis(subhost_core::DEFAULT_BLOCK_TIME_MS.max(100));
    let mut ticker = tokio::time::interval(block_time);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let result = {
            let Ok(mut driver) = driver.lock() else {
                warn!("consensus lock poisoned; stopping producer");
                return;
            };
            produce_once_inner(&state, &mut driver, journal.as_deref()).map(|round| {
                (round, driver.committed_qc().cloned().zip(state.block_at_height(driver.height())))
            })
        };
        let (round, committed) = match result {
            Ok(result) => result,
            Err(error) => {
                warn!(%error, "consensus round failed");
                continue;
            }
        };
        let Some(outbound) = &outbound else { continue };
        if let Some(round) = round {
            relay(
                outbound,
                NetworkMessage::BlockProposal {
                    view: round.proposal.view,
                    data: encode_block(&round.proposal.block),
                },
            );
            if let Some(vote) = round.vote {
                relay(outbound, vote_message(&vote));
            }
        }
        if let Some((qc, block)) = committed {
            relay(
                outbound,
                NetworkMessage::BlockCommit {
                    view: qc.view,
                    data: encode_block(&block),
                    signatures: qc
                        .signatures
                        .into_iter()
                        .map(|(address, signature)| (*address.as_bytes(), signature))
                        .collect(),
                },
            );
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
    driver: Option<&Arc<Mutex<ConsensusDriver>>>,
    journal: Option<&ConsensusJournal>,
) {
    loop {
        tokio::select! {
            message = inbound.recv() => match message {
                Some(message) if driver.is_some() => {
                    let Some(driver) = driver else { continue };
                    let result = match driver.lock() {
                        Ok(mut driver) => handle_consensus_message_inner(state, &mut driver, message, journal),
                        Err(_) => { warn!("consensus lock poisoned; stopping gossip application"); return; }
                    };
                    match result {
                        Ok(Some(vote)) => relay(outbound, vote_message(&vote)),
                        Ok(None) => {},
                        Err(error) => debug!(%error, "discarding invalid consensus traffic"),
                    }
                }
                Some(NetworkMessage::BlockProposal { view: 0, data }) if !state.has_validator_set() => {
                    match decode_block(&data) {
                        Ok(block) => {
                            buffer.offer(block, state.block_height().saturating_add(1));
                            apply_buffered(state, buffer);
                        }
                        Err(error) => debug!(%error, "discarding an undecodable peer block"),
                    }
                }
                // Other flows exist on the wire but are not driven here.
                Some(_) => {}
                None => break,
            },
            event = block_events.recv() => match event {
                // Only relay blocks this node produced; a received block already
                // reached every peer through gossip.
                Ok(BlockEvent::Sealed(block)) => {
                    // Legacy single-writer blocks have no consensus view; the
                    // receiver only decodes the block payload.
                    let message =
                        NetworkMessage::BlockProposal { view: 0, data: encode_block(&block) };
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

/// Read and validate a producer key file.
///
/// The format is one lowercase hex-encoded canonical 32-byte BLS scalar, with an
/// optional `0x` prefix and surrounding whitespace. A file that group or others
/// can read is refused before it is parsed, and a scalar whose encoding does not
/// round-trip is refused so that two files cannot name the same key.
///
/// This is the only loader for the format. `subhost-cli` uses it for `init`, so
/// the genesis writer and the node cannot disagree about what a key file means.
pub fn read_producer_key(path: &Path) -> Result<PrivateKey, NodeError> {
    let metadata = std::fs::metadata(path)
        .map_err(|source| NodeError::ProducerKey { path: path.to_path_buf(), source })?;
    check_producer_permissions(path, &metadata)?;

    let text = std::fs::read_to_string(path)
        .map_err(|source| NodeError::ProducerKey { path: path.to_path_buf(), source })?;
    let text = text.trim();
    let digits = text.strip_prefix("0x").unwrap_or(text);
    let bytes =
        hex::decode(digits).map_err(|_| NodeError::InvalidProducerKey(path.to_path_buf()))?;
    let secret = BLSScheme::decode_secret_key(&bytes)
        .ok_or_else(|| NodeError::InvalidProducerKey(path.to_path_buf()))?;
    // Decoding already round-trips; compare again so a future format change
    // cannot start a node on a different key than the one in the file.
    if BLSScheme::encode_secret_key(&secret) != bytes {
        return Err(NodeError::InvalidProducerKey(path.to_path_buf()));
    }
    Ok(secret)
}

/// Write a producer key file, refusing to overwrite an existing one.
///
/// The file is created with mode `0600` before its contents are written, so it is
/// never readable by another user even for an instant. A partial write is removed
/// instead of being left behind as a half key.
pub fn write_producer_key(path: &Path, secret: &PrivateKey) -> Result<(), NodeError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|source| NodeError::ProducerKey { path: path.to_path_buf(), source })?;

    let mut line = hex::encode(BLSScheme::encode_secret_key(secret));
    line.push('\n');
    if let Err(source) = file.write_all(line.as_bytes()).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(NodeError::ProducerKey { path: path.to_path_buf(), source });
    }
    Ok(())
}

/// Read the producer key and resolve which genesis validator it belongs to.
///
/// The file is the only input: the address is looked up by public key instead of
/// being passed in, so a key file cannot be paired with an address it does not
/// match. Zero or several matches are both errors.
fn load_producer(
    path: &Path,
    genesis: Option<&GenesisConfig>,
) -> Result<(Address, PrivateKey), NodeError> {
    let secret = read_producer_key(path)?;

    let genesis =
        genesis.ok_or_else(|| NodeError::NoValidatorSetForProducer(path.to_path_buf()))?;
    let public_key = BLSScheme::encode_public_key(&BLSScheme::public_key(&secret));
    let mut matches =
        genesis.initial_validators.iter().filter(|validator| validator.public_key == public_key);
    match (matches.next(), matches.next()) {
        (Some(validator), None) => Ok((validator.address, secret)),
        (Some(_), Some(_)) => Err(NodeError::AmbiguousProducer(path.to_path_buf())),
        (None, _) => Err(NodeError::UnknownProducer(path.to_path_buf())),
    }
}

/// Refuse a key file that anyone but its owner can read.
///
/// Creation with `0600` is not enough on its own: a copied or restored file can
/// arrive with a wider mode, and that must fail loudly rather than load.
#[cfg(unix)]
fn check_producer_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<(), NodeError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(NodeError::InsecureProducerKey { path: path.to_path_buf(), mode });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_producer_permissions(
    _path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), NodeError> {
    Ok(())
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
    #[error("consensus journal {path}: {detail}")]
    ConsensusJournal { path: PathBuf, detail: String },

    #[error("cannot create data directory {path}: {source}")]
    DataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("a validator node requires a genesis file at {0}")]
    MissingGenesis(PathBuf),

    #[error("cannot read the producer key file {path}: {source}")]
    ProducerKey {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "producer key file {path} is mode {mode:04o}; it must not be readable by group or others"
    )]
    InsecureProducerKey { path: PathBuf, mode: u32 },

    #[error("producer key file {0} is not a canonical hex-encoded 32-byte BLS scalar")]
    InvalidProducerKey(PathBuf),

    #[error("producer key in {0} does not belong to any genesis validator")]
    UnknownProducer(PathBuf),

    #[error("producer key in {0} matches more than one genesis validator")]
    AmbiguousProducer(PathBuf),

    #[error("a producer key was configured for {0} but the genesis has no validator set")]
    NoValidatorSetForProducer(PathBuf),

    #[error("chain ID cannot be zero")]
    InvalidChainId,

    #[error(transparent)]
    Genesis(#[from] subhost_core::CoreError),

    #[error(transparent)]
    Consensus(#[from] subhost_consensus::ConsensusError),

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

    /// A genesis with one real validator at `[9; 20]`, and its signing secret.
    fn genesis_with_validator(chain_id: ChainId) -> (GenesisConfig, Address, PrivateKey) {
        let (secret, _) = BLSScheme::keygen();
        let address = Address::new([9; 20]);
        let mut genesis = genesis_with(chain_id, None);
        genesis.initial_validators = vec![ValidatorInfo {
            address,
            public_key: BLSScheme::encode_public_key(&BLSScheme::public_key(&secret)),
            power: 100,
            proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                &secret,
            )),
        }];
        (genesis, address, secret)
    }

    /// Write a producer key the way `subhost init --validator` will.
    fn write_test_producer_key(dir: &Path, secret: &PrivateKey) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(PRODUCER_KEY_FILE_NAME);
        std::fs::write(&path, hex::encode(BLSScheme::encode_secret_key(secret))).unwrap();
        set_mode_0600(&path);
        path
    }

    #[cfg(unix)]
    fn set_mode_0600(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_mode_0600(_path: &Path) {}

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
        assert!(matches!(Node::bootstrap(config), Err(NodeError::Genesis(_))));
    }

    #[test]
    fn validator_mode_boots_with_the_default_producer_key() {
        let dir = tempfile::tempdir().unwrap();
        let (genesis, address, secret) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let key_path = write_test_producer_key(dir.path(), &secret);

        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };
        assert_eq!(config.producer_key_path(), Some(key_path));
        let node = Node::bootstrap(config).unwrap();

        assert_eq!(node.genesis().unwrap().total_voting_power(), 100);
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        node.state().seed_accounts([(sender, 1_000_000)]).unwrap();
        node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        assert_eq!(
            node.state().block_height(),
            0,
            "under consensus the RPC submission only admits; the producer commits"
        );

        let driver = node.consensus().expect("a validator node must own a driver");
        let mut driver = driver.lock().unwrap();
        assert!(produce_once(node.state(), &mut driver).unwrap().is_some());
        drop(driver);

        let block = node.state().block_at_height(1).unwrap();
        assert_eq!(
            block.header.validator, address,
            "blocks must be signed as the genesis validator"
        );
        assert_eq!(block.signatures.len(), 1);
    }

    pub(super) fn validator_nodes(count: u8) -> (Vec<tempfile::TempDir>, Vec<Node>) {
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        let keys: Vec<_> = (0..count).map(|_| BLSScheme::keygen().0).collect();
        let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
        genesis.initial_validators = keys
            .iter()
            .enumerate()
            .map(|(index, secret)| ValidatorInfo {
                address: Address::new([index as u8 + 1; 20]),
                public_key: BLSScheme::encode_public_key(&BLSScheme::public_key(secret)),
                power: 100,
                proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                    secret,
                )),
            })
            .collect();
        let dirs: Vec<_> = keys
            .iter()
            .map(|secret| {
                let dir = tempfile::tempdir().unwrap();
                genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
                write_test_producer_key(dir.path(), secret);
                dir
            })
            .collect();
        let nodes = dirs
            .iter()
            .map(|dir| {
                Node::bootstrap(NodeConfig { validator: true, ..NodeConfig::new(dir.path()) })
                    .unwrap()
            })
            .collect();
        (dirs, nodes)
    }

    #[test]
    fn four_validator_nodes_require_three_votes_before_a_ledger_write() {
        for voting_nodes in [4, 3, 2] {
            let (dirs, nodes) = validator_nodes(4);
            let manifests: Vec<_> = dirs
                .iter()
                .map(|dir| {
                    std::fs::read(
                        dir.path()
                            .join(subhost_storage::LEDGER_DIR_NAME)
                            .join(subhost_storage::MANIFEST_FILE_NAME),
                    )
                    .unwrap()
                })
                .collect();
            let tx = signed_transfer(&signing_key(7), 0, 10);
            let hash = nodes[0].state().submit_transaction(tx).unwrap();
            let round =
                produce_once(nodes[0].state(), &mut nodes[0].consensus().unwrap().lock().unwrap())
                    .unwrap()
                    .unwrap();
            let mut votes = vec![round.vote.unwrap()];
            for node in &nodes[1..voting_nodes] {
                let vote = handle_consensus_message(
                    node.state(),
                    &mut node.consensus().unwrap().lock().unwrap(),
                    NetworkMessage::BlockProposal {
                        view: round.proposal.view,
                        data: encode_block(&round.proposal.block),
                    },
                )
                .unwrap()
                .unwrap();
                votes.push(vote);
            }
            for (index, node) in nodes[..voting_nodes].iter().enumerate() {
                for vote in &votes {
                    handle_consensus_message(
                        node.state(),
                        &mut node.consensus().unwrap().lock().unwrap(),
                        vote_message(vote),
                    )
                    .unwrap();
                }
                let driver = node.consensus().unwrap().lock().unwrap();
                if voting_nodes >= 3 {
                    assert_eq!(node.state().block_height(), 1);
                    assert_eq!(driver.height(), 1);
                    assert_eq!(driver.tip(), round.proposal.block.hash());
                    assert!(node.state().receipt(&hash).is_some());
                    assert_eq!(node.state().balance(&Address::new([2; 20])), 10);
                } else {
                    assert_eq!(node.state().block_height(), 0);
                    assert_eq!(driver.height(), 0);
                    assert!(node.state().receipt(&hash).is_none());
                    assert_eq!(node.state().balance(&Address::new([2; 20])), 0);
                    assert_eq!(
                        std::fs::read(
                            dirs[index]
                                .path()
                                .join(subhost_storage::LEDGER_DIR_NAME)
                                .join(subhost_storage::MANIFEST_FILE_NAME)
                        )
                        .unwrap(),
                        manifests[index]
                    );
                }
            }
        }
    }

    #[test]
    fn an_observer_can_commit_a_certified_block_without_a_signing_key() {
        let (dirs, nodes) = validator_nodes(1);
        let observer_dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            dirs[0].path().join(GENESIS_FILE_NAME),
            observer_dir.path().join(GENESIS_FILE_NAME),
        )
        .unwrap();
        let observer = Node::bootstrap(NodeConfig::new(observer_dir.path())).unwrap();
        assert!(!observer.consensus().unwrap().lock().unwrap().is_validator());
        nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
        produce_once(nodes[0].state(), &mut nodes[0].consensus().unwrap().lock().unwrap()).unwrap();
        let qc = nodes[0].consensus().unwrap().lock().unwrap().committed_qc().unwrap().clone();
        let block = nodes[0].state().block_at_height(1).unwrap();
        let mut driver = observer.consensus().unwrap().lock().unwrap();
        let vote = handle_consensus_message(
            observer.state(),
            &mut driver,
            NetworkMessage::BlockProposal { view: 1, data: encode_block(&block) },
        )
        .unwrap();
        assert!(vote.is_none());
        assert_eq!(observer.state().block_height(), 0);
        let message = NetworkMessage::BlockCommit {
            view: qc.view,
            data: encode_block(&block),
            signatures: qc
                .signatures
                .into_iter()
                .map(|(address, signature)| (*address.as_bytes(), signature))
                .collect(),
        };
        assert!(handle_consensus_message(observer.state(), &mut driver, message.clone())
            .unwrap()
            .is_none());
        assert_eq!(observer.state().block_height(), 1);
        assert_eq!(observer.state().tip_hash(), block.hash());
        handle_consensus_message(observer.state(), &mut driver, message).unwrap();
        assert_eq!(observer.state().balance(&Address::new([2; 20])), 10);
    }

    #[test]
    fn a_failed_quorum_write_keeps_the_tip_and_retries_the_certificate() {
        let (dirs, nodes) = validator_nodes(1);
        let node = &nodes[0];
        let tx = signed_transfer(&signing_key(7), 0, 10);
        let hash = node.state().submit_transaction(tx).unwrap();
        let ledger = dirs[0].path().join(subhost_storage::LEDGER_DIR_NAME);
        let saved = dirs[0].path().join("saved-ledger");
        std::fs::rename(&ledger, &saved).unwrap();
        std::fs::write(&ledger, b"block ledger directory creation").unwrap();
        let mut driver = node.consensus().unwrap().lock().unwrap();
        assert!(produce_once(node.state(), &mut driver).is_err());
        assert_eq!(driver.height(), 0);
        assert_eq!(node.state().block_height(), 0);
        assert!(driver.pending_qc().is_some());
        assert!(node.state().receipt(&hash).is_none());
        assert_eq!(node.state().pending_count(), 1);
        let block_hash = driver.open_proposal().unwrap().block.hash();
        std::fs::remove_file(&ledger).unwrap();
        std::fs::rename(&saved, &ledger).unwrap();
        produce_once(node.state(), &mut driver).unwrap();
        assert_eq!(driver.tip(), block_hash);
        assert_eq!(node.state().tip_hash(), block_hash);
        assert_eq!(node.state().pending_count(), 0);
        assert!(node.state().receipt(&hash).is_some());
    }

    #[test]
    fn restarting_a_validator_restores_the_committed_height_and_view() {
        let (_dirs, nodes) = validator_nodes(1);
        let node = &nodes[0];
        node.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
        produce_once(node.state(), &mut node.consensus().unwrap().lock().unwrap()).unwrap();
        let first_hash = node.state().tip_hash();
        let config = node.config().clone();
        drop(nodes);
        let restarted = Node::bootstrap(config).unwrap();
        assert_eq!(restarted.consensus().unwrap().lock().unwrap().view(), 1);
        restarted.state().submit_transaction(signed_transfer(&signing_key(7), 1, 20)).unwrap();
        produce_once(restarted.state(), &mut restarted.consensus().unwrap().lock().unwrap())
            .unwrap();
        assert_eq!(restarted.state().block_height(), 2);
        assert_eq!(restarted.state().block_at_height(2).unwrap().header.parent_hash, first_hash);
    }

    #[test]
    fn invalid_or_unseen_consensus_traffic_cannot_trigger_a_vote() {
        let (_dirs, nodes) = validator_nodes(2);
        nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
        let round =
            produce_once(nodes[0].state(), &mut nodes[0].consensus().unwrap().lock().unwrap())
                .unwrap()
                .unwrap();
        let follower = &nodes[1];
        let mut driver = follower.consensus().unwrap().lock().unwrap();
        assert!(handle_consensus_message(
            follower.state(),
            &mut driver,
            vote_message(round.vote.as_ref().unwrap())
        )
        .is_err());
        let mut tampered = round.proposal.block.clone();
        tampered.transactions[0].value += 1;
        assert!(handle_consensus_message(
            follower.state(),
            &mut driver,
            NetworkMessage::BlockProposal { view: 1, data: encode_block(&tampered) }
        )
        .is_err());
        assert_eq!(driver.view(), 0);
        assert!(driver.open_proposal().is_none());
        let mut malformed = round.vote.unwrap();
        malformed.signature = vec![0xff; 96];
        handle_consensus_message(
            follower.state(),
            &mut driver,
            NetworkMessage::BlockProposal { view: 1, data: encode_block(&round.proposal.block) },
        )
        .unwrap();
        assert!(handle_consensus_message(follower.state(), &mut driver, vote_message(&malformed))
            .is_err());
        assert_eq!(follower.state().block_height(), 0);
        assert!(driver.pending_qc().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_validators_commit_over_real_loopback_gossip() {
        let (_dirs, nodes) = validator_nodes(2);
        let (first_port, second_port) = two_free_loopback_ports();
        let first_task = tokio::spawn(run_network_with_driver(
            nodes[0].state().clone(),
            NetworkConfig {
                listen_addr: format!("/ip4/127.0.0.1/tcp/{first_port}"),
                enable_mdns: false,
                ..Default::default()
            },
            nodes[0].consensus().unwrap().clone(),
        ));
        wait_until_listening(first_port).await;
        let second_task = tokio::spawn(run_network_with_driver(
            nodes[1].state().clone(),
            NetworkConfig {
                listen_addr: format!("/ip4/127.0.0.1/tcp/{second_port}"),
                enable_mdns: false,
                bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{first_port}")],
                ..Default::default()
            },
            nodes[1].consensus().unwrap().clone(),
        ));
        let hash =
            nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while nodes.iter().any(|node| node.state().block_height() != 1) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        first_task.abort();
        second_task.abort();
        assert!(result.is_ok(), "both validators must commit after gossip quorum");
        assert_eq!(nodes[0].state().tip_hash(), nodes[1].state().tip_hash());
        assert_eq!(nodes[0].state().state_root(), nodes[1].state().state_root());
        for node in &nodes {
            assert!(node.state().receipt(&hash).is_some());
        }
    }

    #[test]
    fn validator_mode_without_a_producer_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (genesis, _, _) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };
        assert!(matches!(Node::bootstrap(config), Err(NodeError::ProducerKey { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_producer_key_that_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (genesis, _, secret) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let key_path = write_test_producer_key(dir.path(), &secret);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };
        match Node::bootstrap(config) {
            Err(NodeError::InsecureProducerKey { path, mode }) => {
                assert_eq!(path, key_path);
                assert_eq!(mode, 0o644);
            }
            Err(other) => panic!("expected InsecureProducerKey, got {other:?}"),
            Ok(_) => panic!("a world-readable producer key must not boot a validator"),
        }
    }

    #[test]
    fn a_producer_key_outside_the_genesis_set_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (genesis, _, _) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let (stranger, _) = BLSScheme::keygen();
        write_test_producer_key(dir.path(), &stranger);

        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };
        assert!(matches!(Node::bootstrap(config), Err(NodeError::UnknownProducer(_))));
    }

    #[test]
    fn a_malformed_producer_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (genesis, _, _) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
        std::fs::write(&path, "not-a-scalar").unwrap();
        set_mode_0600(&path);

        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };
        assert!(matches!(Node::bootstrap(config), Err(NodeError::InvalidProducerKey(_))));
    }

    #[test]
    fn the_producer_key_writer_round_trips_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
        let (secret, _) = BLSScheme::keygen();
        write_producer_key(&path, &secret).unwrap();

        let loaded = read_producer_key(&path).unwrap();
        assert_eq!(BLSScheme::encode_secret_key(&loaded), BLSScheme::encode_secret_key(&secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a fresh producer key must be owner-only");
        }

        // `subhost init` calls this on every run: an existing key is the
        // validator identity, so replacing it must fail instead.
        let before = std::fs::read_to_string(&path).unwrap();
        let (other, _) = BLSScheme::keygen();
        assert!(write_producer_key(&path, &other).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_non_validator_node_with_a_validator_set_verifies_peer_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let (genesis, _, secret) = genesis_with_validator(1);
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

        // A follower: no producer key, so it has no signing identity but it still
        // knows who may produce.
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        let registry = ValidatorRegistry::from_genesis(&genesis.initial_validators).unwrap();
        let address = genesis.initial_validators[0].address;
        let producer = RpcState::in_memory(1)
            .unwrap()
            .with_authority(BlockAuthority::new(registry, address, secret.clone()).unwrap());

        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        producer.seed_accounts([(sender, 1_000_000)]).unwrap();
        producer.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        let block = producer.build_block().unwrap().unwrap();
        node.state().seed_accounts([(sender, 1_000_000)]).unwrap();

        let mut unsigned = block.clone();
        unsigned.signatures.clear();
        assert!(matches!(
            node.state().accept_block(unsigned),
            Err(subhost_rpc::RpcError::UnauthorizedProducer)
        ));
        assert_eq!(node.state().block_height(), 0);

        let qc = QuorumCertificate {
            view: 1,
            block_hash: block.hash(),
            signatures: vec![(
                address,
                BLSScheme::encode_signature(&BLSScheme::sign(
                    &secret,
                    &QuorumCertificate::payload_for(1, &block.hash()),
                )),
            )],
        };
        node.state().commit_qc(block, &qc).unwrap();
        assert_eq!(node.state().block_height(), 1);
        assert_eq!(node.state().balance(&Address::new([2; 20])), 10);
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

    pub(super) fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(super) fn signed_transfer(key: &SigningKey, nonce: u64, value: u128) -> Transaction {
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
    pub(super) async fn wait_until_listening(port: u16) {
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
                let message = NetworkMessage::BlockProposal { view: 0, data: encode_block(block) };
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
