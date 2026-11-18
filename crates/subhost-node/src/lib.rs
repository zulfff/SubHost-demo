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
mod liveness_tests;
#[cfg(test)]
mod testnet_tests;

use bincode::Options;
use consensus_journal::ConsensusJournal;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use subhost_consensus::{
    driver::{
        BlockProposal as DriverProposal, ConsensusDriver, PreparedBlock, TimeoutCertificate,
        ViewChange, Vote, VoteOutcome,
    },
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

// Real-network tests share a four-validator machine budget. Running many debug
// BLS clusters concurrently changes the bounded-delivery assumption exercised
// by their deadlines; each cluster still uses its own multithreaded runtime.
#[cfg(test)]
pub(crate) static NETWORK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        if config.view_timeout_ms == 0 {
            return Err(NodeError::InvalidConsensusConfiguration(
                "view_timeout_ms must be nonzero".into(),
            ));
        }
        if config.single_writer && (config.validator || config.producer_key_file.is_some()) {
            return Err(NodeError::InvalidConsensusConfiguration(
                "single-writer demo cannot hold a validator identity".into(),
            ));
        }
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

        let validator_count = genesis.as_ref().map_or(0, |g| g.initial_validators.len());
        if config.single_writer && validator_count != 0 {
            return Err(NodeError::InvalidConsensusConfiguration(
                "single-writer demo requires genesis without validators".into(),
            ));
        }
        if !config.single_writer && validator_count == 0 {
            return Err(NodeError::InvalidConsensusConfiguration(
                "configure genesis validators or explicitly select --single-writer".into(),
            ));
        }
        let proposal_limit = if validator_count > 0 {
            Some(consensus_block_budget(validator_count)?)
        } else if config.network.is_some() {
            Some(64 * 1024)
        } else {
            None
        };
        let protocol_context =
            genesis.as_ref().map_or(Hash::ZERO, GenesisConfig::consensus_context);

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
        let mut state = RpcState::with_store(store)?.with_consensus_context(protocol_context);
        if let Some(limit) = proposal_limit {
            state = state.with_proposal_bytes_limit(limit);
        }
        if let Some(validators) = validators {
            state = state.with_validators(validators);
        }
        if let Some(authority) = authority {
            state = state.with_authority(authority);
        }

        let (consensus, consensus_journal) = match (driver_registry, producer_identity) {
            (Some(registry), identity) => {
                let binding = protocol_context;
                let mut consensus_config = ConsensusConfig::new(registry.len())?;
                consensus_config.view_timeout_ms = config.view_timeout_ms;
                consensus_config.protocol_context = binding;
                let (journal, driver) = ConsensusJournal::open(
                    &config.data_dir,
                    &state,
                    binding,
                    consensus_config,
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
        self.run_until(shutdown_signal()).await
    }

    /// Run until a controlled shutdown resolves. Every spawned service is
    /// stopped and joined before a normal/error return; cancellation also
    /// aborts the owned service set and stops the RPC listener.
    pub async fn run_until<F>(self, shutdown: F) -> Result<(), NodeError>
    where
        F: std::future::Future<Output = Result<(), NodeError>>,
    {
        let rpc_config = RpcServerConfig {
            listen_addr: self.config.rpc_addr,
            max_connections: self.config.max_rpc_connections,
            security: self.config.rpc_security.clone(),
        };
        info!(chain_id = self.chain_id(), height = self.state.block_height(),
            data_dir = %self.config.data_dir.display(), validator = self.config.validator,
            "starting node");
        let mut services = tokio::task::JoinSet::<Result<(), NodeError>>::new();
        if let Some(addr) = self.config.metrics_addr {
            let metrics = self.metrics.clone();
            services.spawn(async move {
                metrics.serve(MetricsConfig { listen_addr: addr }).await?;
                Err(NodeError::ServiceStopped("metrics exporter"))
            });
        }
        {
            let metrics = self.metrics.clone();
            let state = self.state.clone();
            services.spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    metrics.set_block_height(state.block_height());
                    metrics.set_pending_transactions(state.pending_count());
                }
            });
        }
        match (self.config.network.clone(), self.consensus.clone()) {
            (Some(config), driver) => {
                let state = self.state.clone();
                let journal = self.consensus_journal.clone();
                services.spawn(async move {
                    run_network_inner(state, config, driver, journal).await?;
                    Err(NodeError::ServiceStopped("peer gossip"))
                });
            }
            (None, Some(driver)) => {
                let state = self.state.clone();
                let journal = self.consensus_journal.clone();
                services.spawn(async move {
                    produce_blocks(state, Some(driver), None, journal).await;
                    Err(NodeError::ServiceStopped("consensus producer"))
                });
            }
            (None, None) => {}
        }
        let rpc = RpcServer::new(self.state.clone()).with_metrics(self.metrics.clone());
        let (_, handle) = match rpc.start(rpc_config).await {
            Ok(server) => server,
            Err(error) => {
                services.shutdown().await;
                return Err(error.into());
            }
        };
        let on_drop_handle = handle.clone();
        let _rpc_guard = OnDrop(move || {
            let _ = on_drop_handle.stop();
        });
        let result = tokio::select! {
            result = shutdown => result,
            _ = handle.clone().stopped() => Err(NodeError::ServiceStopped("JSON-RPC server")),
            service = services.join_next() => match service {
                Some(Ok(result)) => result,
                Some(Err(error)) => Err(NodeError::ServiceTask(error.to_string())),
                None => Err(NodeError::ServiceStopped("node service set")),
            },
        };
        let _ = handle.stop();
        handle.stopped().await;
        services.shutdown().await;
        result
    }
}

struct OnDrop<F: Fn()>(F);

impl<F: Fn()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Recovery checkpoint envelope bound shared by proposal budgeting and the journal.
pub const MAX_CONSENSUS_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;

/// Canonical block budget allowing a full target-view proof and journal state.
/// Bounds reserve repeated prepared blocks and quadratic certificate signatures,
/// before the JSON byte-array expansion and signed gossipsub framing.
pub fn consensus_block_budget(validators: usize) -> Result<usize, NodeError> {
    if !(1..=64).contains(&validators) {
        return Err(NodeError::InvalidConsensusConfiguration(
            "supported validator count is 1..=64".into(),
        ));
    }
    let n = validators;
    let wire_fixed = 512 + 512 * n + 128 * n * n;
    let recovery_fixed = 2048 + 1024 * n + 384 * n * n;
    let wire_budget = (subhost_network::MAX_WIRE_BYTES.saturating_sub(512) / 4)
        .checked_sub(wire_fixed)
        .ok_or_else(|| {
            NodeError::InvalidConsensusConfiguration(
                "validator proof cannot fit wire budget".into(),
            )
        })?
        / (n + 2);
    let recovery_budget =
        MAX_CONSENSUS_CHECKPOINT_BYTES.checked_sub(recovery_fixed).ok_or_else(|| {
            NodeError::InvalidConsensusConfiguration(
                "validator proof cannot fit recovery budget".into(),
            )
        })? / (3 * n + 8);
    let budget = wire_budget.min(recovery_budget).min(64 * 1024);
    if budget < 1024 {
        return Err(NodeError::InvalidConsensusConfiguration("insufficient block budget".into()));
    }
    Ok(budget)
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
#[cfg(test)]
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

#[cfg(test)]
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
            result.map_err(NodeError::from)
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
    commit_ready(state, driver, journal)?;
    let initial_view =
        driver.committed_qc().map_or(driver.height(), |qc| qc.view).checked_add(1).ok_or_else(
            || NodeError::InvalidConsensusConfiguration("view counter overflow".into()),
        )?;
    let certified_view = driver.view() == initial_view
        || driver
            .timeout_certificate()
            .is_some_and(|certificate| certificate.view == driver.view());
    // Waiting for a target-view certificate is normal. Do not suppress the
    // signed view-change reports needed to obtain that certificate.
    if driver.open_proposal().is_none() && driver.is_leader() && certified_view {
        if let Some(block) = match driver.preferred_block() {
            Some(block) => Some(block),
            None => state.build_block()?,
        } {
            state.validate_proposal(&block)?;
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
    validate_round_wire(&candidate)?;
    if let Some(journal) = journal {
        if candidate.checkpoint() != driver.checkpoint() {
            journal.save(&candidate)?;
        }
    }
    *driver = candidate;
    Ok(())
}

fn vote_message(vote: &Vote) -> NetworkMessage {
    NetworkMessage::ConsensusVote { data: encode_canonical(vote) }
}

fn decode_consensus<T: DeserializeOwned>(data: &[u8]) -> Result<T, NodeError> {
    if data.is_empty() || data.len() > subhost_network::MAX_WIRE_BYTES {
        return Err(
            subhost_rpc::RpcError::BlockRejected("invalid consensus payload size".into()).into()
        );
    }
    bincode::options()
        .with_fixint_encoding()
        .with_limit(subhost_network::MAX_WIRE_BYTES as u64)
        .deserialize(data)
        .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()).into())
}

fn current_round_messages(driver: &ConsensusDriver) -> Vec<NetworkMessage> {
    let mut messages = Vec::new();
    if let Some(proposal) = driver.open_proposal().filter(|proposal| !proposal.signature.is_empty())
    {
        messages.push(NetworkMessage::ConsensusProposal { data: encode_canonical(proposal) });
    }
    if let Some(vote) = driver.own_vote() {
        messages.push(vote_message(vote));
    }
    if let Some(prepared) =
        driver.prepared_certificate().filter(|prepared| prepared.qc.view == driver.view())
    {
        messages.push(NetworkMessage::PreparedBlock { data: encode_canonical(&prepared) });
    }
    if let Some(vote) = driver.own_commit_vote() {
        messages.push(vote_message(vote));
    }
    if let Some(change) = driver.own_view_change() {
        messages.push(NetworkMessage::ViewChange { data: encode_canonical(change) });
    }
    if let Some(certificate) = driver.timeout_certificate() {
        messages.push(NetworkMessage::TimeoutCertificate { data: encode_canonical(&certificate) });
    }
    messages
}

fn certified_tip_message(state: &RpcState, driver: &ConsensusDriver) -> Option<NetworkMessage> {
    let qc = driver.committed_qc()?;
    let block = state.block_at_height(driver.height())?;
    Some(NetworkMessage::BlockCommit {
        view: qc.view,
        data: encode_block(&block),
        signatures: qc
            .signatures
            .iter()
            .map(|(address, signature)| (*address.as_bytes(), signature.clone()))
            .collect(),
    })
}

fn validate_round_wire(driver: &ConsensusDriver) -> Result<(), NodeError> {
    for message in current_round_messages(driver) {
        message.validate()?;
    }
    Ok(())
}

fn validate_timeout_blocks(
    state: &RpcState,
    driver: &ConsensusDriver,
    certificate: &TimeoutCertificate,
) -> Result<(), NodeError> {
    for report in &certificate.messages {
        if let Some(prepared) = &report.prepared {
            validate_pending_block(state, driver, &prepared.block)?;
        }
    }
    Ok(())
}

/// Only reuse execution validation for the complete block already accepted at
/// this same ledger anchor. A matching hash or header alone is insufficient.
fn validate_pending_block(
    state: &RpcState,
    driver: &ConsensusDriver,
    block: &Block,
) -> Result<(), NodeError> {
    let known = driver.open_proposal().is_some_and(|proposal| proposal.block == *block)
        || driver.prepared_certificate().is_some_and(|prepared| prepared.block == *block);
    if !known {
        state.validate_proposal(block)?;
    }
    Ok(())
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
    commit_ready(state, driver, journal)?;
    match message {
        NetworkMessage::ConsensusProposal { data } => {
            let proposal: DriverProposal = decode_consensus(&data)?;
            if proposal.block.header.height <= driver.height() || proposal.view < driver.view() {
                return Ok(None);
            }
            validate_pending_block(state, driver, &proposal.block)?;
            if let Some(tc) = &proposal.timeout_certificate {
                validate_timeout_blocks(state, driver, tc)?;
            }
            let mut candidate = driver.clone();
            let vote = candidate.observe_proposal(proposal)?;
            adopt_pending(driver, candidate, journal)?;
            commit_ready(state, driver, journal)?;
            Ok(vote)
        }
        NetworkMessage::ConsensusVote { data } => {
            let vote: Vote = decode_consensus(&data)?;
            let mut candidate = driver.clone();
            let fresh = candidate.record_vote(vote.clone())?;
            adopt_pending(driver, candidate, journal)?;
            commit_ready(state, driver, journal)?;
            Ok(fresh.then_some(vote))
        }
        NetworkMessage::PreparedBlock { data } => {
            let prepared: PreparedBlock = decode_consensus(&data)?;
            if prepared.block.header.height <= driver.height() || prepared.qc.view < driver.view() {
                return Ok(None);
            }
            validate_pending_block(state, driver, &prepared.block)?;
            let mut candidate = driver.clone();
            let vote = candidate.receive_prepare(prepared)?;
            adopt_pending(driver, candidate, journal)?;
            commit_ready(state, driver, journal)?;
            Ok(vote)
        }
        NetworkMessage::ViewChange { data } => {
            let change: ViewChange = decode_consensus(&data)?;
            if change.height <= driver.height() {
                return Ok(None);
            }
            let mut candidate = driver.clone();
            let fresh = candidate.record_view_change(change.clone())?;
            if fresh {
                if let Some(prepared) = &change.prepared {
                    validate_pending_block(state, driver, &prepared.block)?;
                }
            }
            adopt_pending(driver, candidate, journal)?;
            Ok(None)
        }
        NetworkMessage::TimeoutCertificate { data } => {
            let certificate: TimeoutCertificate = decode_consensus(&data)?;
            if certificate.height <= driver.height() || certificate.view < driver.view() {
                return Ok(None);
            }
            validate_timeout_blocks(state, driver, &certificate)?;
            let mut candidate = driver.clone();
            candidate.receive_timeout_certificate(certificate)?;
            adopt_pending(driver, candidate, journal)?;
            Ok(None)
        }
        NetworkMessage::BlockProposal { .. } | NetworkMessage::Vote { .. } => {
            Err(subhost_rpc::RpcError::BlockRejected("legacy unphased consensus message".into())
                .into())
        }
        NetworkMessage::BlockCommit { view, data, signatures } => {
            let block = decode_block(&data)
                .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()))?;
            if block.header.height <= driver.height() {
                return Ok(None);
            }
            let qc = QuorumCertificate {
                protocol_context: state.consensus_context(),
                view,
                block_hash: block.hash(),
                signatures: signatures
                    .into_iter()
                    .map(|(address, signature)| (Address::new(address), signature))
                    .collect(),
            };
            let mut candidate = driver.clone();
            candidate.receive_commit(block.clone(), qc.clone())?;
            state.validate_certified_block(&block, &qc)?;
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
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let clock = std::time::Instant::now();
    let mut had_pending = false;
    loop {
        ticker.tick().await;
        let result = (|| -> Result<Vec<NetworkMessage>, NodeError> {
            let mut driver =
                driver.lock().map_err(|_| NodeError::ServiceStopped("poisoned consensus lock"))?;
            commit_ready(&state, &mut driver, journal.as_deref())?;
            let mut candidate = driver.clone();
            let pending = state.pending_count() > 0;
            if pending && !had_pending {
                candidate.wake_on_work(clock.elapsed())?;
            }
            had_pending = pending;
            candidate.tick(clock.elapsed())?;
            adopt_pending(&mut driver, candidate, journal.as_deref())?;
            let round = produce_once_inner(&state, &mut driver, journal.as_deref())?;
            let mut messages = current_round_messages(&driver);
            // A just-completed round can still supply its original proposal/vote
            // to a lagging peer before the final certificate is relayed.
            if let Some(round) = round {
                if !round.proposal.signature.is_empty() {
                    let message = NetworkMessage::ConsensusProposal {
                        data: encode_canonical(&round.proposal),
                    };
                    if !messages.contains(&message) {
                        messages.push(message);
                    }
                }
                if let Some(vote) = round.vote {
                    let message = vote_message(&vote);
                    if !messages.contains(&message) {
                        messages.push(message);
                    }
                }
            }
            if let Some(message) = certified_tip_message(&state, &driver) {
                messages.push(message);
            }
            Ok(messages)
        })();
        let messages = match result {
            Ok(messages) => messages,
            Err(error) => {
                warn!(%error, "consensus cycle failed");
                continue;
            }
        };
        if let Some(outbound) = &outbound {
            for message in messages {
                relay(outbound, message);
            }
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
                    let (result, fresh) = match driver.lock() {
                        Ok(mut driver) => {
                            let before = current_round_messages(&driver);
                            let height = driver.height();
                            let result = handle_consensus_message_inner(state, &mut driver, message, journal);
                            let mut fresh = Vec::new();
                            if result.is_ok() {
                                fresh = current_round_messages(&driver).into_iter()
                                    .filter(|message| !before.contains(message)).collect();
                                if driver.height() > height {
                                    if let Some(message) = certified_tip_message(state, &driver) {
                                        fresh.push(message);
                                    }
                                }
                            }
                            (result, fresh)
                        },
                        Err(_) => { warn!("consensus lock poisoned; stopping gossip application"); return; }
                    };
                    // A prepare vote may create our Commit vote or timeout proof.
                    // Emit newly journalled phase outputs immediately, before a
                    // ticker can time out the view that created them.
                    for message in fresh {
                        relay(outbound, message);
                    }
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
    #[error("invalid consensus configuration: {0}")]
    InvalidConsensusConfiguration(String),

    #[error("node service stopped: {0}")]
    ServiceStopped(&'static str),

    #[error("node service task failed: {0}")]
    ServiceTask(String),

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
#[path = "basic_tests.rs"]
mod tests;
