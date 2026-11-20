//! Node bootstrap: genesis, ledger, JSON-RPC, metrics, and peer gossip in one
//! place.
//!
//! This is the single place that decides how a running node is assembled, so the
//! CLI, tests, and container entrypoint cannot drift apart.
//!
//! With a genesis validator set, the node exchanges proposals, verified votes
//! and certified commits, and writes blocks through [`RpcState::commit_qc`].
//! Every committed certificate is archived beside the voting journal, and a
//! node that receives a certified tip beyond its next height asks for the
//! missing range instead of dropping it (Q-013 / ADR-007).
//! Demo chains without validators use [`RpcState::accept_block`] and buffer
//! blocks that arrive before their parent. There are no consensus timeouts,
//! peer scoring, or fork choice yet.

mod certified_history;
mod consensus_journal;
#[cfg(test)]
#[path = "consensus_span_tests.rs"]
mod consensus_span_tests;
mod fork_incident;
#[cfg(test)]
mod liveness_tests;
#[cfg(test)]
mod runtime_health_tests;
#[cfg(test)]
mod testnet_tests;

use bincode::Options;
use certified_history::CertifiedHistory;
use consensus_journal::ConsensusJournal;
use fork_incident::{ForkIncident, ForkLog};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use subhost_consensus::{
    driver::{
        BlockProposal as DriverProposal, ConsensusDriver, FinalEvidence, HaltReason, PreparedBlock,
        TimeoutCertificate, ViewChange, Vote, VoteOutcome,
    },
    ConsensusConfig, ConsensusError, QuorumCertificate, ValidatorRegistry,
};
use subhost_core::{encode_canonical, Address, Block, BlockHeight, ChainId, GenesisConfig, Hash};
use subhost_crypto::{BLSScheme, PrivateKey};
use subhost_mempool::MempoolStore;
use subhost_metrics::{HealthStatus, Metrics, MetricsConfig};
use subhost_network::{NetworkConfig, NetworkHandle, NetworkManager, NetworkMessage};
use subhost_rpc::{
    BlockAuthority, BlockEvent, RpcSecurityConfig, RpcServer, RpcServerConfig, RpcState,
};
use subhost_storage::{LedgerStore, PruneOutcome};
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
    /// Blocks to retain automatically, or `None` for today's offline-only prune.
    ///
    /// This is the configuration surface for `SUBHOST-2026-003` fix 3 (Q-036):
    /// `Some(n)` prunes the ledger after a successful commit when
    /// `tip - base_height > n`. The newest snapshot is the replay anchor and the
    /// hard floor, so the honest bound is `n` plus up to one snapshot cadence.
    /// `None` is the default and keeps `subhost db prune --keep-segments` as the
    /// only prune path. Values below the certified-archive window warn at boot;
    /// explicit `--single-writer` demo mode is excluded because there committed
    /// history is not final.
    pub retention_blocks: Option<u64>,
    /// Whether the pending pool is persisted to `<data_dir>/mempool.bin`.
    ///
    /// `false` (the default) keeps today's volatile pool and writes nothing.
    /// `true` attaches the durable store before traffic is served, restores the
    /// snapshot after ledger/journal reconciliation, and makes admission
    /// persist before a transaction hash is acknowledged (Q-016/Q-034). The CLI
    /// flag is `--mempool-persist`.
    pub mempool_persist: bool,
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
            retention_blocks: None,
            mempool_persist: false,
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

/// Node-owned automatic-retention trigger (Q-036).
///
/// The consensus commit path calls [`Self::after_commit`] once per successful
/// commit. When the retained span exceeds the configured bound, the trigger
/// asks storage to prune through [`RpcState::retain_from`], which owns the
/// storage write and the matching in-memory trim under the commit lock.
///
/// A prune is best-effort by contract: a refusal at the snapshot floor or a
/// storage error is logged and never propagated, so it cannot fail an already
/// durable commit. After an attempt that made no progress the trigger waits
/// `max(1, keep / 8)` blocks, so a persistent floor does not turn every commit
/// into a prune attempt.
#[derive(Debug)]
struct RetentionTrigger {
    keep: u64,
    /// Head height at which the next attempt is allowed.
    next_attempt: AtomicU64,
}

impl RetentionTrigger {
    fn new(keep: u64) -> Self {
        Self { keep, next_attempt: AtomicU64::new(0) }
    }

    /// Blocks to wait after an attempt that made no progress.
    fn retry_after(&self) -> u64 {
        (self.keep / 8).max(1)
    }

    /// Try to prune after a successful commit; never returns a failure.
    fn after_commit(&self, state: &RpcState) {
        let Some(outcome) = self.attempt(state) else { return };
        match outcome {
            Ok(PruneOutcome::Pruned { dropped_segments, retained_segments, base_height }) => {
                info!(
                    base_height,
                    dropped_segments,
                    retained_segments,
                    "automatically pruned retained ledger history"
                );
            }
            Ok(PruneOutcome::NothingToPrune { segments }) => {
                debug!(segments, "automatic retention made no change");
            }
            Ok(PruneOutcome::RefusedBySnapshot { snapshot_height, requested_height }) => {
                debug!(
                    snapshot_height,
                    requested_height,
                    "retention is held by the newest snapshot anchor; retrying after the backoff"
                );
            }
            Err(error) => {
                warn!(
                    %error,
                    "automatic ledger retention failed; the commit is unaffected and the \
                     attempt retries after the backoff"
                );
            }
        }
    }

    /// Height to prune to, or `None` while the bound or the backoff says no.
    ///
    /// `tip - base > keep` is the only state that triggers an attempt, and the
    /// backoff can only delay one: it never raises the bound or widens the
    /// retained span. The returned height is strictly above `base`, so storage
    /// always has something to consider.
    fn target(&self, tip: u64, base: u64) -> Option<u64> {
        if tip.saturating_sub(base) <= self.keep {
            return None;
        }
        if tip < self.next_attempt.load(Ordering::SeqCst) {
            return None;
        }
        Some(tip - self.keep)
    }

    /// Apply one attempt's effect on the backoff window.
    ///
    /// `Pruned` is progress, so the next commit may attempt again as soon as
    /// the bound is exceeded. Every other result (a snapshot refusal or a
    /// storage error) waits `max(1, keep / 8)` blocks, so a persistent floor
    /// does not turn every commit into a prune attempt.
    fn record(&self, tip: u64, outcome: &Result<PruneOutcome, subhost_rpc::RpcError>) {
        if matches!(outcome, Ok(PruneOutcome::Pruned { .. })) {
            return;
        }
        self.next_attempt.store(tip.saturating_add(self.retry_after()), Ordering::SeqCst);
    }

    /// One attempt, or `None` while the bound or the backoff says no.
    fn attempt(&self, state: &RpcState) -> Option<Result<PruneOutcome, subhost_rpc::RpcError>> {
        let tip = state.head_height();
        let target = self.target(tip, state.base_height())?;
        let outcome = state.retain_from(target);
        self.record(tip, &outcome);
        Some(outcome)
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
    /// Certified `(block, QC)` history this node committed, served to lagging peers.
    certified_history: Option<Arc<Mutex<CertifiedHistory>>>,
    /// Durable contradiction of local finality; re-halts on every restart.
    fork_log: Option<Arc<Mutex<ForkLog>>>,
    /// Set while a certified range is being adopted; signing stays suppressed.
    catching_up: Arc<AtomicBool>,
    /// Truthful S8 health predicates and proposal-latency observations.
    runtime: Arc<RuntimeHealth>,
    /// Automatic ledger retention, installed by bootstrap when the operator
    /// configured a bound and this is not an explicit single-writer demo.
    retention: Option<Arc<RetentionTrigger>>,
}

impl Node {
    /// Load genesis, restore or create the ledger, and apply allocations once.
    ///
    /// Genesis allocations are applied only when no ledger exists yet, so a
    /// restart can never re-mint balances over a live chain.
    pub fn bootstrap(config: NodeConfig) -> Result<Self, NodeError> {
        Self::bootstrap_inner(config, None)
    }

    /// Bootstrap with an already-unlocked producer key (S9.6, ADR-011).
    ///
    /// The keystore crate unlocks the file; this entry point takes the resulting
    /// key so no password or plaintext file path reaches the node. The key is
    /// validated exactly like a file-loaded one: canonical encoding round-trip,
    /// derived public key, unique genesis membership, and the registry's
    /// proof-of-possession checks. Unlocking bypasses nothing — validator
    /// configuration, journal recovery and certified-history reconciliation all
    /// run unchanged. A non-validator configuration is an error, and the
    /// configured key file is never read when a key is supplied.
    pub fn bootstrap_with_producer_key(
        config: NodeConfig,
        key: PrivateKey,
    ) -> Result<Self, NodeError> {
        if !config.validator {
            return Err(NodeError::InvalidConsensusConfiguration(
                "a supplied producer key requires a validator configuration".into(),
            ));
        }
        Self::bootstrap_inner(config, Some(key))
    }

    fn bootstrap_inner(
        config: NodeConfig,
        supplied_key: Option<PrivateKey>,
    ) -> Result<Self, NodeError> {
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
        if let Some(keep) = config.retention_blocks {
            // The CLI enforces `1..`, but `NodeConfig` is also a library API.
            if keep == 0 {
                return Err(NodeError::InvalidConsensusConfiguration(
                    "retention_blocks must be at least 1; omit it to disable automatic retention"
                        .into(),
                ));
            }
            if config.single_writer {
                warn!(
                    retention_blocks = keep,
                    "automatic retention is disabled in single-writer demo mode"
                );
            } else if keep < certified_history::RETENTION as u64 {
                warn!(
                    retention_blocks = keep,
                    certified_archive_window = certified_history::RETENTION,
                    "retention is below the certified-archive window; this node serves a \
                     degraded sync range"
                );
            }
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
        let authority = match (supplied_key, config.producer_key_path(), validators.as_ref()) {
            // A supplied key wins over any configured file, which is never read.
            (Some(secret), _, Some(validators)) => {
                let (producer, secret) = resolve_supplied_producer(secret, genesis.as_ref())?;
                info!(%producer, "using the supplied producer key");
                producer_identity = Some((producer, secret.clone()));
                Some(BlockAuthority::new(validators.clone(), producer, secret)?)
            }
            (Some(_), _, None) => {
                return Err(NodeError::InvalidConsensusConfiguration(
                    "a supplied producer key requires a genesis validator set".into(),
                ));
            }
            (None, None, _) => None,
            (None, Some(path), Some(validators)) => {
                let (producer, secret) = load_producer(&path, genesis.as_ref())?;
                info!(path = %path.display(), %producer, "loaded the producer key");
                producer_identity = Some((producer, secret.clone()));
                Some(BlockAuthority::new(validators.clone(), producer, secret)?)
            }
            (None, Some(path), None) => return Err(NodeError::NoValidatorSetForProducer(path)),
        };
        let driver_registry = validators.clone();

        let store = LedgerStore::open(chain_id, &config.data_dir)?;
        let fresh = !store.exists();
        // Bind the policy to the store *before* `with_store` loads it: a
        // restored version-2 ledger replays and settles with that policy, and
        // the first persist records it in the manifest (Q-030). Offline tools
        // supply nothing and use the recorded policy.
        let fee_policy = genesis.as_ref().and_then(|genesis| genesis.fee_policy.clone());
        let store = match &fee_policy {
            Some(policy) => store.with_fee_policy(policy.clone())?,
            None => store,
        };
        let mut state = RpcState::with_store(store)?.with_consensus_context(protocol_context);
        // Honour the genesis block gas limit end to end: the fee policy is
        // validated against it and every produced/accepted header carries it.
        // Absent a genesis document the default limit applies unchanged.
        if let Some(genesis) = &genesis {
            state = state.with_block_gas_limit(genesis.gas_limit)?;
        }
        // A fee-enabled genesis owns the fee rules for this chain; the node
        // refuses to extend a legacy ledger with it. Absent policy keeps the
        // legacy fixed-price behaviour byte-for-byte.
        if let Some(policy) = fee_policy {
            state = state.with_fee_policy(policy)?;
        }
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
                // A fee-enabled ledger needs its height-0 state anchor persisted
                // before the first v2 block, even with no allocations: storage
                // refuses a v2 append with no committed snapshot and no verified
                // head (Q-030). `seed_accounts` always persists, so an empty
                // allocation set still writes the anchor.
                if !genesis.allocations.is_empty() || genesis.fee_policy.is_some() {
                    state.seed_accounts(
                        genesis.allocations.iter().map(|(address, balance)| (*address, *balance)),
                    )?;
                    if genesis.allocations.is_empty() {
                        info!("persisted the genesis state anchor for the fee-enabled ledger");
                    } else {
                        info!(accounts = genesis.allocations.len(), "applied genesis allocations");
                    }
                }
            }
        } else {
            info!(height = state.block_height(), "restored existing ledger");
        }

        let certified_history = consensus_journal
            .as_ref()
            .map(|journal| {
                let path = journal.path().with_file_name(certified_history::HISTORY_FILE);
                CertifiedHistory::open(&path, Some(protocol_context))
                    .map(|history| Arc::new(Mutex::new(history)))
            })
            .transpose()?;
        if let Some(history) = &certified_history {
            let history = history.lock().expect("fresh archive lock");
            check_archive_matches_ledger(
                history.path(),
                history.last(),
                state.block_height(),
                state.tip_hash(),
            )?;
            info!(
                retained = history.len(),
                from = ?history.retained_from(),
                tip = ?history.tip(),
                "opened certified history"
            );
        }

        if genesis.is_none() {
            warn!(
                path = %config.genesis_path().display(),
                chain_id,
                "no genesis file found; starting with an empty allocation set"
            );
        }

        let fork_log = consensus_journal
            .as_ref()
            .map(|journal| {
                let path = journal.path().with_file_name(fork_incident::INCIDENT_FILE);
                ForkLog::open(&path, protocol_context).map(|log| Arc::new(Mutex::new(log)))
            })
            .transpose()?;
        // A durable conflict outlives the process: halt before the first tick so
        // a restart cannot sign one view while the incident is still unresolved.
        let unresolved =
            fork_log.as_ref().and_then(|log| log.lock().ok().and_then(|log| log.latest().copied()));
        if let Some(incident) = unresolved {
            let incidents =
                fork_log.as_ref().and_then(|log| log.lock().ok().map(|log| log.len())).unwrap_or(0);
            warn!(
                height = incident.height,
                local_view = incident.local_view,
                incoming_view = incident.incoming_view,
                incidents,
                "durable fork incident recorded; signing and readiness stay halted"
            );
            if let Some(driver) = &consensus {
                if let Ok(mut driver) = driver.lock() {
                    driver.halt(HaltReason::ConflictingFinalEvidence { height: incident.height });
                }
            }
        }

        // Q-016/Q-034 consumer: the store is opt-in, so an operator who never
        // enables `--mempool-persist` keeps today's volatile pool and no
        // `mempool.bin`. The genesis binding is the same protocol context the
        // journal and certified history use, so a fee-policy or calldata-schedule
        // change makes an old snapshot foreign instead of silently reusable.
        let pending_path = config.data_dir.join(subhost_mempool::MEMPOOL_FILE_NAME);
        if config.mempool_persist {
            let store = MempoolStore::open(chain_id, protocol_context, &config.data_dir).map_err(
                |error| NodeError::PendingSnapshot {
                    path: pending_path.clone(),
                    detail: error.to_string(),
                },
            )?;
            state = state.with_pending_store(store);
        }

        // Q-034 requirement 1: a configured pending store is restored only after
        // the ledger, journal, archive, fork log and genesis seeding are
        // reconciled, and before the node serves traffic. Placing it before the
        // metrics construction makes the pending-depth gauge report the restored
        // pool. No store configured keeps today's volatile behaviour.
        //
        // Q-038: a failed restore refuses boot rather than silently starting
        // empty, and the operator-facing error names the file to remove. The
        // path stays in this boot error; RPC persistence errors keep no detail.
        state.restore_pending().map_err(|error| NodeError::PendingSnapshot {
            path: pending_path,
            detail: error.to_string(),
        })?;

        let metrics = Metrics::new()?;
        metrics.set_block_height(state.block_height());
        metrics.set_pending_transactions(state.pending_count());
        metrics.set_finalized_height(state.finalized_height());
        let catching_up = Arc::new(AtomicBool::new(false));
        let runtime = Arc::new(RuntimeHealth::new(catching_up.clone()));
        // Genesis, ledger, registry, journal and archive recovery reconciled
        // above; the block pipeline has not started, so readiness stays false.
        runtime.ledger_loaded.store(true, Ordering::SeqCst);
        runtime.publish(&metrics, false);
        // Q-036: installed once at boot so every commit site, and the tests
        // that drive those sites, uses the same trigger. Single-writer demo
        // mode is excluded because committed history is not final there.
        let retention = config
            .retention_blocks
            .filter(|_| !config.single_writer)
            .map(|keep| Arc::new(RetentionTrigger::new(keep)));

        Ok(Self {
            state,
            metrics,
            config,
            genesis,
            consensus,
            consensus_journal,
            certified_history,
            fork_log,
            catching_up,
            runtime,
            retention,
        })
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
        // Attach the metrics registry before the pipeline starts: the retention
        // trigger prunes through this shared state, so `retain_from` publishes
        // `subhost_ledger_prune_total` and seeds `subhost_ledger_base_height`.
        // `with_metrics` also arms the RPC method instrumentation used below.
        let rpc = RpcServer::new(self.state.clone()).with_metrics(self.metrics.clone());
        let state = rpc.state().clone();
        let retention = self.retention.clone();
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
            let state = state.clone();
            services.spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    metrics.set_block_height(state.block_height());
                    metrics.set_pending_transactions(state.pending_count());
                    // Finalized height only ever comes from a verified final
                    // Commit QC (ADR-013); demo commits leave it at zero.
                    metrics.set_finalized_height(state.finalized_height());
                    // Measured under the commit lock; an unavailable measurement
                    // keeps the previous value instead of publishing a false zero.
                    publish_disk_stats(&metrics, &state);
                }
            });
        }
        match (self.config.network.clone(), self.consensus.clone()) {
            (Some(config), driver) => {
                let state = state.clone();
                let runtime = self.runtime.clone();
                let node_services = NodeServices {
                    journal: self.consensus_journal.clone(),
                    history: self.certified_history.clone(),
                    fork_log: self.fork_log.clone(),
                    catching_up: self.catching_up.clone(),
                    metrics: Some(self.metrics.clone()),
                    runtime: self.runtime.clone(),
                    retention: retention.clone(),
                };
                services.spawn(async move {
                    runtime.pipeline_alive.store(true, Ordering::SeqCst);
                    let outcome = run_network_inner(state, config, driver, node_services).await;
                    runtime.pipeline_alive.store(false, Ordering::SeqCst);
                    outcome?;
                    Err(NodeError::ServiceStopped("peer gossip"))
                });
            }
            (None, Some(driver)) => {
                let state = state.clone();
                let runtime = self.runtime.clone();
                let node_services = NodeServices {
                    journal: self.consensus_journal.clone(),
                    history: self.certified_history.clone(),
                    fork_log: self.fork_log.clone(),
                    catching_up: self.catching_up.clone(),
                    metrics: Some(self.metrics.clone()),
                    runtime: self.runtime.clone(),
                    retention: retention.clone(),
                };
                services.spawn(async move {
                    runtime.pipeline_alive.store(true, Ordering::SeqCst);
                    produce_blocks(state, Some(driver), None, node_services).await;
                    runtime.pipeline_alive.store(false, Ordering::SeqCst);
                    Err(NodeError::ServiceStopped("consensus producer"))
                });
            }
            (None, None) => {}
        }
        let (_, handle) = match rpc.start(rpc_config).await {
            Ok(server) => server,
            Err(error) => {
                self.runtime.pipeline_alive.store(false, Ordering::SeqCst);
                self.runtime.publish(&self.metrics, false);
                services.shutdown().await;
                return Err(error.into());
            }
        };
        // In explicit single-writer demo mode the local sealer/acceptance loop
        // served by RPC is the block pipeline (Q-020).
        if self.consensus.is_none() {
            self.runtime.pipeline_alive.store(true, Ordering::SeqCst);
            self.runtime.publish(&self.metrics, false);
        }
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
        // Controlled shutdown clears readiness before the pipeline stops.
        self.runtime.pipeline_alive.store(false, Ordering::SeqCst);
        self.runtime.publish(&self.metrics, false);
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

/// Node-owned handles shared by the gossip pump and the block producer.
///
/// Grouping them keeps both tasks on the same journal, archive, incident log,
/// suppression flag and metrics instance; a second construction could silently
/// diverge from what the node opened at bootstrap.
#[derive(Clone, Default)]
struct NodeServices {
    journal: Option<Arc<ConsensusJournal>>,
    history: Option<Arc<Mutex<CertifiedHistory>>>,
    fork_log: Option<Arc<Mutex<ForkLog>>>,
    catching_up: Arc<AtomicBool>,
    metrics: Option<Metrics>,
    runtime: Arc<RuntimeHealth>,
    /// Automatic ledger retention, when the operator opted in.
    retention: Option<Arc<RetentionTrigger>>,
}

/// Pump gossip until the process shuts down.
///
/// The swarm and the application loop live in one task, so aborting the task
/// stops both. Nothing is spawned here that could outlive the node.
#[cfg(test)]
async fn run_network(state: RpcState, config: NetworkConfig) -> Result<(), NodeError> {
    run_network_inner(state, config, None, NodeServices::default()).await
}

/// Gossip plus block production for a node that holds a consensus driver.
#[cfg(test)]
async fn run_network_with_driver(
    state: RpcState,
    config: NetworkConfig,
    driver: Arc<Mutex<ConsensusDriver>>,
) -> Result<(), NodeError> {
    run_network_inner(state, config, Some(driver), NodeServices::default()).await
}

#[cfg(test)]
async fn run_network_with_journal(
    state: RpcState,
    config: NetworkConfig,
    driver: Arc<Mutex<ConsensusDriver>>,
    journal: Option<Arc<ConsensusJournal>>,
) -> Result<(), NodeError> {
    run_network_inner(
        state,
        config,
        Some(driver),
        NodeServices { journal, ..NodeServices::default() },
    )
    .await
}

async fn run_network_inner(
    state: RpcState,
    config: NetworkConfig,
    driver: Option<Arc<Mutex<ConsensusDriver>>>,
    services: NodeServices,
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
        () = pump_gossip(
            &state,
            &outbound,
            &mut inbound,
            &mut block_events,
            &mut buffer,
            driver.as_ref(),
            CertifiedSync {
                journal: services.journal.as_deref(),
                history: services.history.as_deref(),
                fork_log: services.fork_log.as_deref(),
                catching_up: &services.catching_up,
                metrics: services.metrics.as_ref(),
                runtime: &services.runtime,
                retention: services.retention.as_deref(),
            },
        ) => {
            Ok(())
        }
        () = produce_blocks(
            state.clone(),
            driver.clone(),
            Some(outbound.clone()),
            services.clone(),
        ) => {
            Ok(())
        }
    }
}

/// One proposal ready to broadcast, possibly from an observer.
struct ProducedRound {
    proposal: DriverProposal,
    vote: Option<Vote>,
}

/// Refuse a restored archive that cannot belong to this ledger.
///
/// A certificate is archived just before the ledger commit, so one item of lead
/// (`archive_tip == ledger_height + 1`) is the normal crash state and is
/// accepted: the retry appends the same height and the archive treats it as a
/// no-op. A larger lead means the files came from different points — a partial
/// restore — and every future commit below the archived tip would fail. Report
/// that at boot instead of wedging the node on its first commit.
fn check_archive_matches_ledger(
    path: &Path,
    last: Option<(BlockHeight, &[u8])>,
    ledger_height: BlockHeight,
    ledger_tip: Hash,
) -> Result<(), NodeError> {
    let Some((tip, block_bytes)) = last else { return Ok(()) };
    let mixed_restore = |detail: String| NodeError::CertifiedHistory {
        path: path.to_path_buf(),
        detail: format!(
            "{detail}; restore the archive, ledger, journal and incident log from \
             the same stopped node"
        ),
    };
    if tip > ledger_height.saturating_add(1) {
        return Err(mixed_restore(format!(
            "archive tip {tip} is ahead of ledger height {ledger_height}"
        )));
    }
    if tip == ledger_height.saturating_add(1) {
        // The normal crash state, but only a real child of this ledger tip may be
        // replayed. Anything else means the files came from different points.
        let parent_matches = decode_block(block_bytes).is_ok_and(|block| {
            block.header.height == tip && block.header.parent_hash == ledger_tip
        });
        if !parent_matches {
            return Err(mixed_restore(format!(
                "the archive records height {tip}, which does not extend the restored ledger tip"
            )));
        }
    }
    Ok(())
}

/// Append a verified `(block, QC)` to the node-owned archive.
///
/// Q-013: the certificate is durable before the ledger append, and serving
/// later re-checks the block hash against the committed ledger.
fn archive_commit(
    history: Option<&Mutex<CertifiedHistory>>,
    block: &Block,
    qc: &QuorumCertificate,
) -> Result<(), NodeError> {
    let Some(history) = history else { return Ok(()) };
    let mut history =
        history.lock().map_err(|_| NodeError::ServiceStopped("poisoned certified history lock"))?;
    history.append(block.header.height, &encode_block(block), &encode_canonical(qc))
}

/// Bounded static classification of consensus-step failures for span fields
/// (Q-035). Local participation/context problems are `context`; everything
/// else is a protocol `rejected`. Raw error text never enters a span.
fn consensus_error_kind(error: &ConsensusError) -> &'static str {
    match error {
        ConsensusError::NoProposal | ConsensusError::NotAValidator | ConsensusError::Halted(_) => {
            "context"
        }
        _ => "rejected",
    }
}

/// Terminal span outcome for a failed consensus step.
fn consensus_outcome(error: &ConsensusError) -> &'static str {
    if consensus_error_kind(error) == "context" {
        "error"
    } else {
        "rejected"
    }
}

/// Commit a certified candidate before advancing the live consensus driver.
/// Valid votes stay in the live tally on I/O failure, so the ticker can retry.
fn commit_ready(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    journal: Option<&ConsensusJournal>,
    history: Option<&Mutex<CertifiedHistory>>,
    retention: Option<&RetentionTrigger>,
) -> Result<(), NodeError> {
    let Some(qc) = driver.pending_qc() else { return Ok(()) };
    let block = driver.open_proposal().expect("pending QC requires a proposal").block.clone();
    // S8.1 (Q-035): one span per commit step, created only once a certificate
    // is actually pending so an idle ticker emits nothing.
    let span = tracing::info_span!(
        "consensus_commit",
        view = qc.view,
        height = block.header.height,
        block_hash = %block.hash(),
        outcome = tracing::field::Empty,
        error_kind = tracing::field::Empty,
    );
    let mut candidate = driver.clone();
    let outcome = {
        let _enter = span.enter();
        let result = candidate.commit_open_proposal();
        match &result {
            Ok(VoteOutcome::Committed { .. }) => {
                span.record("outcome", "ok");
            }
            Ok(_) => {
                span.record("outcome", "pending");
            }
            Err(error) => {
                span.record("outcome", consensus_outcome(error));
                span.record("error_kind", consensus_error_kind(error));
            }
        }
        result
    };
    if !matches!(outcome?, VoteOutcome::Committed { .. }) {
        return Ok(());
    }
    if let Some(journal) = journal {
        journal.prepare_commit(driver, block.clone(), qc.clone())?;
    }
    archive_commit(history, &block, &qc)?;
    state.commit_qc(block, &qc)?;
    *driver = candidate;
    if let Some(journal) = journal {
        journal.save(driver)?;
    }
    // Q-036: the block is durable and the driver advanced; retention is
    // best-effort and can never turn this success into a failure.
    if let Some(trigger) = retention {
        trigger.after_commit(state);
    }
    Ok(())
}

/// Build a proposal or retry a pending certificate without committing alone.
fn produce_once_inner(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    journal: Option<&ConsensusJournal>,
    history: Option<&Mutex<CertifiedHistory>>,
    retention: Option<&RetentionTrigger>,
) -> Result<Option<ProducedRound>, NodeError> {
    commit_ready(state, driver, journal, history, retention)?;
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
            // S8.1 (Q-035): one span per propose step, not per vote.
            let span = tracing::info_span!(
                "consensus_propose",
                view = candidate.view(),
                height = block.header.height,
                block_hash = %block.hash(),
                outcome = tracing::field::Empty,
                error_kind = tracing::field::Empty,
            );
            let proposed = {
                let _enter = span.enter();
                let result = candidate.propose(block);
                match &result {
                    Ok(_) => {
                        span.record("outcome", "ok");
                    }
                    Err(error) => {
                        span.record("outcome", consensus_outcome(error));
                        span.record("error_kind", consensus_error_kind(error));
                    }
                }
                result
            };
            proposed?;
            adopt_pending(driver, candidate, journal)?;
        }
    }
    let round = driver
        .open_proposal()
        .cloned()
        .map(|proposal| ProducedRound { proposal, vote: driver.own_vote().cloned() });
    commit_ready(state, driver, journal, history, retention)?;
    Ok(round)
}

#[cfg(test)]
fn produce_once(
    state: &RpcState,
    driver: &mut ConsensusDriver,
) -> Result<Option<ProducedRound>, NodeError> {
    produce_once_inner(state, driver, None, None, None)
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
    commit: &mut CommitContext<'_>,
) -> Result<Option<Vote>, NodeError> {
    commit_ready(state, driver, commit.journal, commit.history, commit.retention)?;
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
            let view = proposal.view;
            // S8.1 (Q-035): prepare-step span carries the tally, never one
            // span per vote. No error_kind field on prepare spans.
            let span = tracing::info_span!(
                "consensus_prepare",
                view = view,
                block_hash = %proposal.block.hash(),
                votes_have = tracing::field::Empty,
                votes_need = candidate.quorum_threshold() as u64,
                outcome = tracing::field::Empty,
            );
            let vote = {
                let _enter = span.enter();
                let result = candidate.observe_proposal(proposal);
                span.record("votes_have", candidate.prepare_count() as u64);
                match &result {
                    Ok(_) => {
                        span.record("outcome", "ok");
                    }
                    Err(error) => {
                        span.record("outcome", consensus_outcome(error));
                    }
                }
                result
            }?;
            adopt_pending(driver, candidate, commit.journal)?;
            // Authenticated and adopted: start the proposal-to-commit clock.
            if let Some(runtime) = commit.runtime {
                runtime.note_proposal(view);
            }
            commit_ready(state, driver, commit.journal, commit.history, commit.retention)?;
            Ok(vote)
        }
        NetworkMessage::ConsensusVote { data } => {
            let vote: Vote = decode_consensus(&data)?;
            let mut candidate = driver.clone();
            let fresh = candidate.record_vote(vote.clone())?;
            adopt_pending(driver, candidate, commit.journal)?;
            commit_ready(state, driver, commit.journal, commit.history, commit.retention)?;
            Ok(fresh.then_some(vote))
        }
        NetworkMessage::PreparedBlock { data } => {
            let prepared: PreparedBlock = decode_consensus(&data)?;
            if prepared.block.header.height <= driver.height() || prepared.qc.view < driver.view() {
                return Ok(None);
            }
            validate_pending_block(state, driver, &prepared.block)?;
            let mut candidate = driver.clone();
            // S8.1 (Q-035): receiving a prepared block is a prepare-phase step.
            let span = tracing::info_span!(
                "consensus_prepare",
                view = candidate.view(),
                block_hash = %prepared.block.hash(),
                votes_have = tracing::field::Empty,
                votes_need = candidate.quorum_threshold() as u64,
                outcome = tracing::field::Empty,
            );
            let vote = {
                let _enter = span.enter();
                let result = candidate.receive_prepare(prepared);
                span.record("votes_have", candidate.prepare_count() as u64);
                match &result {
                    Ok(_) => {
                        span.record("outcome", "ok");
                    }
                    Err(error) => {
                        span.record("outcome", consensus_outcome(error));
                    }
                }
                result
            }?;
            adopt_pending(driver, candidate, commit.journal)?;
            commit_ready(state, driver, commit.journal, commit.history, commit.retention)?;
            Ok(vote)
        }
        NetworkMessage::ViewChange { data } => {
            let change: ViewChange = decode_consensus(&data)?;
            if change.height <= driver.height() {
                return Ok(None);
            }
            let mut candidate = driver.clone();
            // S8.1 (Q-035): has_prepared is the local lock held when the report
            // arrives; reports_have is the authenticated tally after the step.
            let span = tracing::info_span!(
                "consensus_view_change",
                view = driver.view(),
                target_view = change.view,
                reports_have = tracing::field::Empty,
                reports_need = candidate.quorum_threshold() as u64,
                has_prepared = candidate.prepared_certificate().is_some(),
                outcome = tracing::field::Empty,
            );
            let fresh = {
                let _enter = span.enter();
                let result = candidate.record_view_change(change.clone());
                span.record("reports_have", candidate.view_change_count() as u64);
                match &result {
                    Ok(true) => {
                        span.record("outcome", "ok");
                    }
                    Ok(false) => {
                        span.record("outcome", "rejected");
                    }
                    Err(error) => {
                        span.record("outcome", consensus_outcome(error));
                    }
                }
                result
            }?;
            if fresh {
                if let Some(prepared) = &change.prepared {
                    validate_pending_block(state, driver, &prepared.block)?;
                }
            }
            adopt_pending(driver, candidate, commit.journal)?;
            Ok(None)
        }
        NetworkMessage::TimeoutCertificate { data } => {
            let certificate: TimeoutCertificate = decode_consensus(&data)?;
            if certificate.height <= driver.height() || certificate.view < driver.view() {
                return Ok(None);
            }
            validate_timeout_blocks(state, driver, &certificate)?;
            let mut candidate = driver.clone();
            // S8.1 (Q-035): TC adoption enters certificate.view (not +1); the
            // report count is the certificate's authenticated message list.
            let span = tracing::info_span!(
                "consensus_view_change",
                view = driver.view(),
                target_view = certificate.view,
                reports_have = certificate.messages.len() as u64,
                reports_need = candidate.quorum_threshold() as u64,
                has_prepared = candidate.prepared_certificate().is_some(),
                outcome = tracing::field::Empty,
            );
            {
                let _enter = span.enter();
                let result = candidate.receive_timeout_certificate(certificate);
                match &result {
                    Ok(_) => {
                        span.record("outcome", "ok");
                    }
                    Err(error) => {
                        span.record("outcome", consensus_outcome(error));
                    }
                }
                result
            }?;
            adopt_pending(driver, candidate, commit.journal)?;
            Ok(None)
        }
        NetworkMessage::BlockProposal { .. } | NetworkMessage::Vote { .. } => {
            Err(subhost_rpc::RpcError::BlockRejected("legacy unphased consensus message".into())
                .into())
        }
        NetworkMessage::BlockCommit { view, data, signatures } => {
            let block = decode_block(&data)
                .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()))?;
            let qc = QuorumCertificate {
                protocol_context: state.consensus_context(),
                view,
                block_hash: block.hash(),
                signatures: signatures
                    .into_iter()
                    .map(|(address, signature)| (Address::new(address), signature))
                    .collect(),
            };
            // S3.1/S3.2: authenticate and classify before touching state. A
            // repeated certificate is not a fork, junk is a plain error, and only
            // contradictory authenticated evidence persists an incident + halt.
            route_final_evidence(state, driver, block, qc, commit)
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
    let mut candidates = CandidateBuffer::default();
    let mut commit = CommitContext::empty(&mut candidates);
    handle_consensus_message_inner(state, driver, message, &mut commit)
}

/// No contiguous certified progress for this long abandons the catch-up, so a
/// peer that announces a height and never serves the range cannot pin a
/// validator mute. The next authenticated announcement restarts a fresh run.
const CATCH_UP_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// Catch-up progress owned by the gossip pump: where the authenticated peer tip
/// is, when the current window was requested, and how long the run has stalled.
#[derive(Default)]
struct SyncState {
    /// Newest authenticated certified height; the range target.
    target: Option<BlockHeight>,
    /// Start of the last request, so the retry ticker does not spam peers.
    requested_at: Option<std::time::Instant>,
    /// Last time contiguous history landed; drives the stall budget.
    progressed_at: Option<std::time::Instant>,
}

/// Certified-history inputs the gossip pump needs beyond the ledger view.
struct CertifiedSync<'a> {
    journal: Option<&'a ConsensusJournal>,
    history: Option<&'a Mutex<CertifiedHistory>>,
    fork_log: Option<&'a Mutex<ForkLog>>,
    catching_up: &'a AtomicBool,
    metrics: Option<&'a Metrics>,
    runtime: &'a RuntimeHealth,
    retention: Option<&'a RetentionTrigger>,
}

impl SyncState {
    /// Ask for the next bounded window, if the peer tip is still ahead.
    fn request(&mut self, tip: BlockHeight) -> Option<NetworkMessage> {
        let target = self.target?;
        if target <= tip {
            return None;
        }
        let from = tip.saturating_add(1);
        let to = target.min(from.saturating_add(subhost_network::MAX_SYNC_ITEMS as u64 - 1));
        self.requested_at = Some(std::time::Instant::now());
        Some(NetworkMessage::GetBlocks { from, to })
    }

    /// Retry the window once the previous request has gone unanswered for a while.
    fn retry(&mut self, tip: BlockHeight) -> Option<NetworkMessage> {
        let stale =
            self.requested_at.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
        if !stale {
            return None;
        }
        self.request(tip)
    }

    /// Start (or extend) a catch-up toward an authenticated target.
    fn begin(&mut self, target: BlockHeight) {
        self.target = Some(self.target.map_or(target, |current| current.max(target)));
        if self.progressed_at.is_none() {
            self.progressed_at = Some(std::time::Instant::now());
        }
    }

    /// Record contiguous certified progress; a productive run never expires.
    fn progressed(&mut self) {
        self.progressed_at = Some(std::time::Instant::now());
    }

    /// Abandon a run that has made no progress within [`CATCH_UP_STALL`].
    ///
    /// Returns `true` when the pump must resume signing. The target is dropped
    /// rather than kept: an unserved range must not silence the validator, and a
    /// later authenticated announcement starts a new run.
    fn expire(&mut self, catching_up: &AtomicBool) -> bool {
        let stalled = self.progressed_at.is_some_and(|at| at.elapsed() >= CATCH_UP_STALL);
        if !stalled {
            return false;
        }
        self.target = None;
        self.requested_at = None;
        self.progressed_at = None;
        catching_up.store(false, Ordering::SeqCst);
        true
    }
}

/// Binary item budget whose JSON byte-array expansion still fits the wire cap.
fn sync_payload_budget() -> usize {
    subhost_network::MAX_WIRE_BYTES.saturating_sub(4096) / 4
}

/// Bounded node-owned store for authenticated candidates that arrive past a gap.
///
/// Q-028: the classifier reports `MissingAncestors { from, to }`; the store,
/// eviction and the pop-one apply order stay node-side, next to the archive and
/// journal they must stay reconciled with. No driver-side buffer exists, so the
/// ordered commit path below stays the only way a block reaches the ledger.
const MAX_CANDIDATES: usize = 64;

#[derive(Default)]
struct CandidateBuffer {
    entries: BTreeMap<BlockHeight, (Block, QuorumCertificate)>,
}

impl CandidateBuffer {
    /// Keep a certified candidate until its ancestors arrive. The first candidate
    /// per height wins; the lowest buffered height is evicted at the bound.
    fn offer(&mut self, block: Block, qc: QuorumCertificate) {
        let height = block.header.height;
        if self.entries.contains_key(&height) {
            // The first pair per height wins; keep the drop visible so a peer
            // serving two certified blocks at one height is not silent.
            debug!(height, "dropping a second certified candidate at an occupied height");
            return;
        }
        if self.entries.len() >= MAX_CANDIDATES {
            if let Some(lowest) = self.entries.keys().next().copied() {
                self.entries.remove(&lowest);
            }
        }
        self.entries.insert(height, (block, qc));
    }

    /// Pop the candidate that extends `tip`; callers re-offer nothing behind it.
    fn take_ready(&mut self, tip: BlockHeight) -> Option<(Block, QuorumCertificate)> {
        self.entries.remove(&tip.checked_add(1)?)
    }

    fn highest(&self) -> Option<BlockHeight> {
        self.entries.keys().next_back().copied()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Inputs the certified commit path needs beyond the ledger view.
struct CommitContext<'a> {
    journal: Option<&'a ConsensusJournal>,
    history: Option<&'a Mutex<CertifiedHistory>>,
    fork_log: Option<&'a Mutex<ForkLog>>,
    candidates: &'a mut CandidateBuffer,
    metrics: Option<&'a Metrics>,
    runtime: Option<&'a RuntimeHealth>,
    retention: Option<&'a RetentionTrigger>,
}

impl CommitContext<'_> {
    /// Context for callers that only verify without persisting (unit helpers).
    #[cfg(test)]
    fn empty(candidates: &mut CandidateBuffer) -> CommitContext<'_> {
        CommitContext {
            journal: None,
            history: None,
            fork_log: None,
            candidates,
            metrics: None,
            runtime: None,
            retention: None,
        }
    }
}

/// Truthful runtime predicates for the S8 health snapshot (Q-020, ADR-013).
///
/// `ledger_loaded` is set only after genesis/ledger/registry/journal/archive
/// recovery reconciles. `pipeline_alive` tracks the configured block-pipeline
/// task — not peers, height growth or a timeout tick. `catching_up` is the native
/// S3 syncing predicate, unchanged from S3.3. Readiness is exactly
/// `ledger_loaded && !syncing && consensus_alive`; a safety incident clears
/// `consensus_alive` without being relabelled as a reorg.
#[derive(Default)]
struct RuntimeHealth {
    ledger_loaded: AtomicBool,
    pipeline_alive: AtomicBool,
    catching_up: Arc<AtomicBool>,
    /// First authenticated observation of a proposal per view, for the
    /// proposal-to-final-commit latency sample. Bounded; oldest view evicted.
    proposal_seen: Mutex<BTreeMap<u64, std::time::Instant>>,
}

/// Views retained for latency measurement. A commit older than this window has
/// no local proposal observation and produces no sample rather than a guess.
const MAX_LATENCY_VIEWS: usize = 128;

impl RuntimeHealth {
    fn new(catching_up: Arc<AtomicBool>) -> Self {
        Self {
            ledger_loaded: AtomicBool::new(false),
            pipeline_alive: AtomicBool::new(false),
            catching_up,
            proposal_seen: Mutex::new(BTreeMap::new()),
        }
    }

    fn publish(&self, metrics: &Metrics, halted: bool) {
        metrics.set_health(HealthStatus {
            ledger_loaded: self.ledger_loaded.load(Ordering::SeqCst),
            syncing: self.catching_up.load(Ordering::SeqCst),
            consensus_alive: self.pipeline_alive.load(Ordering::SeqCst) && !halted,
        });
    }

    /// Remember when a proposal was first authenticated locally. First wins: a
    /// retransmission must not restart the latency clock.
    fn note_proposal(&self, view: u64) {
        let Ok(mut seen) = self.proposal_seen.lock() else { return };
        if seen.contains_key(&view) {
            return;
        }
        if seen.len() >= MAX_LATENCY_VIEWS {
            if let Some(oldest) = seen.keys().next().copied() {
                seen.remove(&oldest);
            }
        }
        seen.insert(view, std::time::Instant::now());
    }

    /// Take the latency sample for a committed view, if this node observed the
    /// proposal. Catch-up commits never observed it and yield `None`.
    fn take_qc_latency(&self, view: u64) -> Option<f64> {
        let mut seen = self.proposal_seen.lock().ok()?;
        seen.remove(&view).map(|at| at.elapsed().as_secs_f64())
    }
}

/// Publish readiness from actual runtime state. A halted driver refuses every
/// signature, so it is not a live consensus participant until an operator clears
/// the incident; a pipeline that exited is not alive either.
fn publish_health(metrics: &Metrics, runtime: &RuntimeHealth, halted: bool) {
    runtime.publish(metrics, halted);
}

/// Publish the ledger footprint gauges, keeping the last value on failure.
///
/// ADR-013: a missing or invalid active file is an error, never a false zero, so
/// a failed measurement must not rewrite the gauges.
fn publish_disk_stats(metrics: &Metrics, state: &RpcState) {
    match state.ledger_disk_stats() {
        Ok(stats) => {
            metrics.set_ledger_disk_stats(stats.active_file_bytes, stats.snapshot_modified)
        }
        Err(error) => {
            debug!(%error, "ledger disk measurement unavailable; keeping last value")
        }
    }
}

/// Publish finality measurements after a verified final commit (S8, ADR-013).
///
/// Finalized height comes only from the verified commit boundary, never from a
/// peer announcement. A latency sample exists only when this node authenticated
/// the proposal itself; catch-up commits produce no sample rather than a guess.
fn publish_finality(
    metrics: Option<&Metrics>,
    runtime: Option<&RuntimeHealth>,
    state: &RpcState,
    driver: &ConsensusDriver,
) {
    let Some(metrics) = metrics else { return };
    metrics.set_finalized_height(state.finalized_height());
    if let (Some(runtime), Some(qc)) = (runtime, driver.committed_qc()) {
        if let Some(sample) = runtime.take_qc_latency(qc.view) {
            metrics.record_qc_latency(sample);
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Apply one authenticated `(block, QC)` pair in the live commit order.
///
/// Q-027: a driver clone advances only after execution validation, the archive
/// append and the ledger commit succeed, and the journal is saved per commit.
fn apply_certified_item(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    block: &Block,
    qc: &QuorumCertificate,
    commit: &mut CommitContext<'_>,
) -> Result<(), NodeError> {
    let mut candidate = driver.clone();
    // S8.1 (Q-035): the certified-history commit step gets the same span as
    // the live commit path; the ledger write below emits its own commit_qc.
    let span = tracing::info_span!(
        "consensus_commit",
        view = qc.view,
        height = block.header.height,
        block_hash = %block.hash(),
        outcome = tracing::field::Empty,
        error_kind = tracing::field::Empty,
    );
    {
        let _enter = span.enter();
        let result = candidate.receive_commit(block.clone(), qc.clone());
        match &result {
            Ok(_) => {
                span.record("outcome", "ok");
            }
            Err(error) => {
                span.record("outcome", consensus_outcome(error));
                span.record("error_kind", consensus_error_kind(error));
            }
        }
        result
    }?;
    state.validate_certified_block(block, qc)?;
    if let Some(journal) = commit.journal {
        journal.prepare_commit(driver, block.clone(), qc.clone())?;
    }
    archive_commit(commit.history, block, qc)?;
    state.commit_qc(block.clone(), qc)?;
    *driver = candidate;
    if let Some(journal) = commit.journal {
        journal.save(driver)?;
    }
    publish_finality(commit.metrics, commit.runtime, state, driver);
    // Certified adoption is a commit too; retention applies on the same terms.
    if let Some(trigger) = commit.retention {
        trigger.after_commit(state);
    }
    Ok(())
}

/// What an authenticated `Historical` certificate means against this ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoricalMeaning {
    /// The retained block at that height carries the same hash.
    Duplicate,
    /// The retained block at that height carries a different hash.
    Conflict,
    /// No retained block can be compared: pruned, or the genesis anchor.
    Unverifiable,
}

/// Decide a `Historical` certificate against the retained ledger (R-57-01).
///
/// Pruning drops old blocks but not the finality they proved, so a canonical
/// certificate replayed at a pruned height can be neither confirmed nor refuted.
/// Only a **retained** differing hash is accountable evidence; an unverifiable
/// height must never halt the node, otherwise anyone replaying old canonical
/// history could stop every pruning validator.
fn historical_meaning(
    base_height: BlockHeight,
    height: BlockHeight,
    retained: Option<Hash>,
    incoming: Hash,
) -> HistoricalMeaning {
    if height <= base_height {
        return HistoricalMeaning::Unverifiable;
    }
    match retained {
        Some(local) if local == incoming => HistoricalMeaning::Duplicate,
        Some(_) => HistoricalMeaning::Conflict,
        None => HistoricalMeaning::Unverifiable,
    }
}

/// Route an authenticated final certificate by its local meaning (S3.1/S3.2).
///
/// Junk is a plain error from the classifier and never halts. Duplicate evidence
/// changes no obligation. A contradiction is persisted and halts signing without
/// touching the append-only ledger.
fn route_final_evidence(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    block: Block,
    qc: QuorumCertificate,
    commit: &mut CommitContext<'_>,
) -> Result<Option<Vote>, NodeError> {
    match driver.classify_final_evidence(&block, &qc)? {
        FinalEvidence::Duplicate { .. } => Ok(None),
        FinalEvidence::Next => {
            apply_certified_item(state, driver, &block, &qc, commit)?;
            Ok(None)
        }
        FinalEvidence::Historical { height, hash } => {
            let retained = state.block_at_height(height).map(|block| block.hash());
            let meaning = historical_meaning(state.base_height(), height, retained, hash);
            if meaning == HistoricalMeaning::Conflict {
                record_conflict(state, driver, height, hash, &qc, commit)?;
            } else {
                // Duplicate, or evidence this node cannot verify after a prune:
                // append-only finality means there is nothing to adopt or halt on.
                debug!(
                    height,
                    base_height = state.base_height(),
                    unverifiable = meaning == HistoricalMeaning::Unverifiable,
                    "historical certificate needs no ledger change"
                );
            }
            Ok(None)
        }
        FinalEvidence::Conflicting { height } => {
            let incoming = block.hash();
            record_conflict(state, driver, height, incoming, &qc, commit)?;
            Ok(None)
        }
        FinalEvidence::MissingAncestors { .. } => {
            // The pump turns a buffered candidate into a `GetBlocks` range; the
            // item is applied only after every ancestor is committed.
            commit.candidates.offer(block, qc);
            Ok(None)
        }
    }
}

fn record_conflict(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    height: BlockHeight,
    incoming_hash: Hash,
    qc: &QuorumCertificate,
    commit: &mut CommitContext<'_>,
) -> Result<(), NodeError> {
    // `Hash::ZERO` is a sentinel, not a block hash: it records that no local
    // block is retained at this height. Only a retained, differing hash is a
    // contradiction, so the fallback should be unreachable and must not claim the
    // tip hash proves anything about `height`.
    let local_hash = state.block_at_height(height).map_or(Hash::ZERO, |block| block.hash());
    let incident = ForkIncident {
        height,
        local_hash,
        incoming_hash,
        local_view: driver.committed_qc().map_or(0, |qc| qc.view),
        incoming_view: qc.view,
        detected_unix: unix_now(),
    };
    if let Some(log) = commit.fork_log {
        let mut log =
            log.lock().map_err(|_| NodeError::ServiceStopped("poisoned fork incident lock"))?;
        log.record(incident)?;
    }
    driver.halt(HaltReason::ConflictingFinalEvidence { height });
    if let (Some(metrics), Some(runtime)) = (commit.metrics, commit.runtime) {
        publish_health(metrics, runtime, true);
    }
    warn!(
        height,
        %local_hash,
        %incoming_hash,
        "conflicting final evidence persisted; signing halted"
    );
    Ok(())
}

/// Apply buffered candidates whose ancestors just arrived, ascending.
fn drain_candidates(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    commit: &mut CommitContext<'_>,
) -> Result<u64, NodeError> {
    let mut applied = 0u64;
    while let Some((block, qc)) = commit.candidates.take_ready(driver.height()) {
        apply_certified_item(state, driver, &block, &qc, commit)?;
        applied += 1;
    }
    Ok(applied)
}

/// Serve a bounded ascending slice of certified history this node committed.
///
/// Q-013: never invent history. An item leaves only when the retained ledger
/// holds the same block hash at that height, and the response is truncated to a
/// valid prefix rather than padded to the requested range.
fn serve_certified_range(
    state: &RpcState,
    history: &Mutex<CertifiedHistory>,
    from: BlockHeight,
    to: BlockHeight,
) -> Result<Option<NetworkMessage>, NodeError> {
    let history =
        history.lock().map_err(|_| NodeError::ServiceStopped("poisoned certified history lock"))?;
    let to = to.min(state.block_height());
    let budget = sync_payload_budget();
    let mut items = Vec::new();
    let mut bytes = 0usize;
    for (height, block_bytes, qc_bytes) in history.range(from, to) {
        let Some(committed) = state.block_at_height(height) else { break };
        let Ok(block) = decode_block(block_bytes) else { break };
        if block.hash() != committed.hash() {
            break;
        }
        let item = certified_history::encode_item(block_bytes, qc_bytes);
        if bytes + item.len() > budget {
            break;
        }
        bytes += item.len();
        items.push(item);
    }
    if items.is_empty() {
        return Ok(None);
    }
    let message = NetworkMessage::Blocks { items };
    message.validate()?;
    Ok(Some(message))
}

/// Adopt certified history in ascending parent-linked order.
///
/// Sari's Q-027 boundary: the live `BlockCommit` ordering on a driver clone, a
/// journal save per commit, and no consensus-crate change. A failed suffix stops
/// the run; everything already committed stays, so the tip never skips a
/// missing certificate.
fn apply_certified_range(
    state: &RpcState,
    driver: &mut ConsensusDriver,
    commit: &mut CommitContext<'_>,
    items: &[Vec<u8>],
) -> Result<u64, NodeError> {
    let mut applied = 0u64;
    for item in items {
        let (block_bytes, qc_bytes) = certified_history::decode_item(item).ok_or_else(|| {
            subhost_rpc::RpcError::BlockRejected("malformed certified sync item".into())
        })?;
        let block = decode_block(block_bytes)
            .map_err(|error| subhost_rpc::RpcError::BlockRejected(error.to_string()))?;
        if Some(block.header.height) != driver.height().checked_add(1) {
            // Only a contiguous extension is adoptable; stop at the first gap.
            break;
        }
        let qc: QuorumCertificate = decode_consensus(qc_bytes)?;
        if qc.block_hash != block.hash() {
            return Err(subhost_rpc::RpcError::BlockRejected(
                "certificate does not match the synced block".into(),
            )
            .into());
        }
        apply_certified_item(state, driver, &block, &qc, commit)?;
        applied += 1;
    }
    Ok(applied)
}

/// Retry proposal/vote/commit dissemination as peers join the mesh.
async fn produce_blocks(
    state: RpcState,
    driver: Option<Arc<Mutex<ConsensusDriver>>>,
    outbound: Option<tokio::sync::mpsc::Sender<NetworkMessage>>,
    services: NodeServices,
) {
    let NodeServices { journal, history, catching_up, metrics, runtime, retention, .. } = services;
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
        // A catching-up node must not propose or sign: the range about to be
        // adopted may already contain the height it would vote at (Q-027.4).
        if catching_up.load(Ordering::SeqCst) {
            continue;
        }
        let result = (|| -> Result<Vec<NetworkMessage>, NodeError> {
            let mut driver =
                driver.lock().map_err(|_| NodeError::ServiceStopped("poisoned consensus lock"))?;
            // S3.2: a halted driver must not propose, sign or extend local
            // finality. Readiness already reports it as not participating, and
            // only an operator clears the durable incident.
            if let Some(reason) = driver.halted() {
                warn!(?reason, "consensus is halted; refusing to propose or sign");
                if let Some(metrics) = &metrics {
                    publish_health(metrics, &runtime, true);
                }
                return Ok(Vec::new());
            }
            if let Some(metrics) = &metrics {
                publish_health(metrics, &runtime, false);
            }
            commit_ready(
                &state,
                &mut driver,
                journal.as_deref(),
                history.as_deref(),
                retention.as_deref(),
            )?;
            let mut candidate = driver.clone();
            let pending = state.pending_count() > 0;
            if pending && !had_pending {
                candidate.wake_on_work(clock.elapsed())?;
            }
            had_pending = pending;
            let view_before = candidate.view();
            let ticked = candidate.tick(clock.elapsed())?;
            if let Some(change) = &ticked {
                // S8.1 (Q-035): a no-op tick emits nothing (volume discipline).
                // A signed report is a post-hoc event marker: every field is
                // known after the step, and the span closes immediately.
                let _marker = tracing::info_span!(
                    "consensus_view_change",
                    view = view_before,
                    target_view = change.view,
                    reports_have = candidate.view_change_count() as u64,
                    reports_need = candidate.quorum_threshold() as u64,
                    has_prepared = candidate.prepared_certificate().is_some(),
                    outcome = "ok",
                );
            }
            adopt_pending(&mut driver, candidate, journal.as_deref())?;
            let round = produce_once_inner(
                &state,
                &mut driver,
                journal.as_deref(),
                history.as_deref(),
                retention.as_deref(),
            )?;
            // A locally produced proposal starts its own latency clock; the
            // verified commit below (or in a later cycle) closes the sample.
            if let Some(round) = &round {
                if !round.proposal.signature.is_empty() {
                    runtime.note_proposal(round.proposal.view);
                }
            }
            publish_finality(metrics.as_ref(), Some(runtime.as_ref()), &state, &driver);
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

/// Finish or advance a catch-up window after the tip moved.
///
/// Returns the next `GetBlocks` request, or `None` once the announced peer tip
/// is reached — at which point signing resumes.
fn advance_sync(
    sync: &mut SyncState,
    tip: BlockHeight,
    catching_up: &AtomicBool,
) -> Option<NetworkMessage> {
    match sync.target {
        Some(target) if tip >= target => {
            sync.target = None;
            sync.requested_at = None;
            sync.progressed_at = None;
            catching_up.store(false, Ordering::SeqCst);
            debug!(tip, "certified catch-up reached the announced peer tip");
            None
        }
        Some(_) => sync.request(tip),
        None => None,
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
    certified: CertifiedSync<'_>,
) {
    let CertifiedSync { journal, history, fork_log, catching_up, metrics, runtime, retention } =
        certified;
    let mut sync = SyncState::default();
    // Q-028: one node-owned store for the whole pump. It is never handed to the
    // driver, so an unauthenticated card cannot reach the ordered commit path.
    let mut candidates = CandidateBuffer::default();
    let mut retry = tokio::time::interval(std::time::Duration::from_millis(500));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            message = inbound.recv() => match message {
                Some(NetworkMessage::GetBlocks { from, to }) if driver.is_some() => {
                    match history {
                        Some(history) => match serve_certified_range(state, history, from, to) {
                            Ok(Some(message)) => relay(outbound, message),
                            Ok(None) => debug!(from, to, "no retained certified history for the range"),
                            Err(error) => debug!(%error, "cannot serve certified history"),
                        },
                        None => debug!(from, to, "this node keeps no certified history"),
                    }
                }
                Some(NetworkMessage::Blocks { items }) if driver.is_some() => {
                    let Some(driver) = driver else { continue };
                    let outcome = match driver.lock() {
                        Ok(mut driver) => {
                            let before = driver.height();
                            let mut commit = CommitContext {
                                journal,
                                history,
                                fork_log,
                                candidates: &mut candidates,
                                metrics,
                                runtime: Some(runtime),
                                retention,
                            };
                            let applied =
                                apply_certified_range(state, &mut driver, &mut commit, &items)
                                    .and_then(|applied| {
                                        // The ancestors just landed; buffered descendants
                                        // apply in ascending order without another round trip.
                                        drain_candidates(state, &mut driver, &mut commit)
                                            .map(|drained| applied + drained)
                                    });
                            if applied.is_ok() && driver.height() > before {
                                if let Some(message) = certified_tip_message(state, &driver) {
                                    relay(outbound, message);
                                }
                            }
                            applied.map(|applied| (applied, before, driver.height()))
                        }
                        Err(_) => {
                            warn!("consensus lock poisoned; stopping gossip application");
                            return;
                        }
                    };
                    match outcome {
                        Ok((applied, before, height)) if height > before => {
                            debug!(applied, height, "adopted certified history");
                            sync.progressed();
                            if let Some(request) = advance_sync(&mut sync, height, catching_up) {
                                if let Some(metrics) = metrics {
                                    publish_health(metrics, runtime, false);
                                }
                                relay(outbound, request);
                            } else if let Some(metrics) = metrics {
                                publish_health(metrics, runtime, false);
                            }
                        }
                        Ok((0, _, height)) if sync.target.is_some_and(|target| height >= target) => {
                            sync.target = None;
                            sync.requested_at = None;
                            sync.progressed_at = None;
                            catching_up.store(false, Ordering::SeqCst);
                            if let Some(metrics) = metrics {
                                publish_health(metrics, runtime, false);
                            }
                        }
                        Ok(_) => {}
                        Err(error) => debug!(%error, "certified range stopped at the first bad item"),
                    }
                }
                Some(message) if driver.is_some() => {
                    let Some(driver) = driver else { continue };
                    let (result, fresh) = match driver.lock() {
                        Ok(mut driver) => {
                            let before = current_round_messages(&driver);
                            let height = driver.height();
                            let mut commit = CommitContext {
                                journal,
                                history,
                                fork_log,
                                candidates: &mut candidates,
                                metrics,
                                runtime: Some(runtime),
                                retention,
                            };
                            // S3.1: classification and application happen together,
                            // so a gap beyond the next height buffers rather than drops.
                            let result = handle_consensus_message_inner(
                                state,
                                &mut driver,
                                message,
                                &mut commit,
                            );
                            if result.is_ok() {
                                publish_finality(metrics, Some(runtime), state, &driver);
                            }
                            let mut fresh = Vec::new();
                            if result.is_ok() {
                                if driver.halted().is_some() {
                                    // Contradictory evidence was persisted; emit nothing
                                    // until an operator clears the incident.
                                    catching_up.store(false, Ordering::SeqCst);
                                } else {
                                    // A backfilled ancestor can free a buffered candidate.
                                    if let Err(error) =
                                        drain_candidates(state, &mut driver, &mut commit)
                                    {
                                        debug!(%error, "buffered candidate did not apply");
                                    }
                                    fresh = current_round_messages(&driver)
                                        .into_iter()
                                        .filter(|message| !before.contains(message))
                                        .collect();
                                    if driver.height() > height {
                                        if let Some(message) =
                                            certified_tip_message(state, &driver)
                                        {
                                            fresh.push(message);
                                        }
                                        if let Some(request) = advance_sync(
                                            &mut sync,
                                            driver.height(),
                                            catching_up,
                                        ) {
                                            fresh.push(request);
                                        } else if let Some(metrics) = metrics {
                                            // The reached target cleared `catching_up`.
                                            publish_health(metrics, runtime, false);
                                        }
                                    } else if let Some(announced) = commit.candidates.highest() {
                                        // Missing certified ancestors: ask for the range and
                                        // stay suppress-signing until the buffer drains or the
                                        // stall budget expires. The height came from an
                                        // authenticated certificate, never a bare announcement.
                                        sync.begin(announced);
                                        catching_up.store(true, Ordering::SeqCst);
                                        if let Some(metrics) = metrics {
                                            publish_health(metrics, runtime, false);
                                        }
                                        debug!(
                                            announced,
                                            height,
                                            buffered = commit.candidates.len(),
                                            "certified candidate is ahead; requesting history"
                                        );
                                        if let Some(request) = sync.request(driver.height()) {
                                            fresh.push(request);
                                        }
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
            _ = retry.tick() => {
                if sync.target.is_some() {
                    if sync.expire(catching_up) {
                        warn!(
                            "certified catch-up stalled; resuming signing and dropping the \
                             unserved range"
                        );
                        if let Some(metrics) = metrics {
                            publish_health(metrics, runtime, false);
                        }
                    } else if let Some(driver) = driver {
                        if let Ok(driver) = driver.lock() {
                            if let Some(request) = sync.retry(driver.height()) {
                                relay(outbound, request);
                            }
                        }
                    }
                }
            }
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
///
/// The file is read through the keystore crate's hardened reader (ADR-011):
/// `O_NOFOLLOW`, a regular file, an owner-only mode, an effective-uid ownership
/// check and a bounded read, all on the opened handle rather than a separate
/// `stat` followed by a second open. Symlinks are refused outright, non-Unix
/// platforms fail explicitly, and the legacy path and the keystore path cannot
/// drift apart — which also closes the init consumer's double-read window.
pub fn read_producer_key(path: &Path) -> Result<PrivateKey, NodeError> {
    let bytes = subhost_keystore::read_bounded_owner_only(path, MAX_PRODUCER_KEY_FILE_BYTES)
        .map_err(|error| producer_read_error(path, error))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| NodeError::InvalidProducerKey(path.to_path_buf()))?;
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

/// A legacy key file is 64 hex digits plus an optional `0x` and a newline; the
/// bound leaves headroom for whitespace without reading an attacker-sized file.
const MAX_PRODUCER_KEY_FILE_BYTES: u64 = 256;

/// Map the shared secret-file reader's refusals onto the node's error surface.
///
/// Permission bits keep their existing dedicated variant; the remaining
/// refusals (symlink or special file, foreign owner, unsupported platform,
/// oversized file) name the reason so an operator can act on it.
fn producer_read_error(path: &Path, error: subhost_keystore::ProducerKeystoreError) -> NodeError {
    use subhost_keystore::ProducerKeystoreError as Keystore;
    match error {
        Keystore::InsecurePermissions { mode } => {
            NodeError::InsecureProducerKey { path: path.to_path_buf(), mode }
        }
        Keystore::Io(source) => NodeError::ProducerKey { path: path.to_path_buf(), source },
        Keystore::UnsafeFile => NodeError::UnsafeProducerKey {
            path: path.to_path_buf(),
            reason: "must be a regular file, not a symlink or special file".into(),
        },
        Keystore::InsecureOwner => NodeError::UnsafeProducerKey {
            path: path.to_path_buf(),
            reason: "is not owned by the running user".into(),
        },
        Keystore::UnsupportedPlatform => NodeError::UnsafeProducerKey {
            path: path.to_path_buf(),
            reason: "producer keys require a Unix platform with owner-only file permissions".into(),
        },
        Keystore::FileTooLarge => NodeError::UnsafeProducerKey {
            path: path.to_path_buf(),
            reason: "is larger than a producer key file may be".into(),
        },
        other => {
            NodeError::UnsafeProducerKey { path: path.to_path_buf(), reason: other.to_string() }
        }
    }
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

/// Resolve an already-unlocked producer key to its unique genesis validator.
///
/// The same checks the file path applies: canonical encoding round-trip,
/// derived public key, exactly one genesis match. Proof-of-possession and
/// corrected-suite verification already happened in
/// `ValidatorRegistry::from_genesis` before this point, so unlocking a keystore
/// cannot bypass them.
fn resolve_supplied_producer(
    secret: PrivateKey,
    genesis: Option<&GenesisConfig>,
) -> Result<(Address, PrivateKey), NodeError> {
    let encoded = BLSScheme::encode_secret_key(&secret);
    let decoded =
        BLSScheme::decode_secret_key(&encoded).ok_or(NodeError::InvalidSuppliedProducerKey)?;
    if BLSScheme::encode_secret_key(&decoded) != encoded {
        return Err(NodeError::InvalidSuppliedProducerKey);
    }
    let genesis = genesis.ok_or_else(|| {
        NodeError::InvalidConsensusConfiguration(
            "a supplied producer key requires a genesis validator set".into(),
        )
    })?;
    let public_key = BLSScheme::encode_public_key(&BLSScheme::public_key(&secret));
    let mut matches =
        genesis.initial_validators.iter().filter(|validator| validator.public_key == public_key);
    match (matches.next(), matches.next()) {
        (Some(validator), None) => Ok((validator.address, secret)),
        (Some(_), Some(_)) => Err(NodeError::AmbiguousSuppliedProducer),
        (None, _) => Err(NodeError::UnknownSuppliedProducer),
    }
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

    #[error("certified history {path}: {detail}")]
    CertifiedHistory { path: PathBuf, detail: String },

    #[error("fork incident log {path}: {detail}")]
    ForkIncident { path: PathBuf, detail: String },

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

    #[error("producer key file {path} is unsafe: {reason}")]
    UnsafeProducerKey { path: PathBuf, reason: String },

    #[error("the supplied producer key is not a canonical BLS scalar")]
    InvalidSuppliedProducerKey,

    #[error("the supplied producer key does not belong to any genesis validator")]
    UnknownSuppliedProducer,

    #[error("the supplied producer key matches more than one genesis validator")]
    AmbiguousSuppliedProducer,

    #[error("chain ID cannot be zero")]
    InvalidChainId,

    #[error(transparent)]
    Genesis(#[from] subhost_core::CoreError),

    #[error(transparent)]
    Consensus(#[from] ConsensusError),

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

    #[error("pending snapshot {path}: {detail}")]
    PendingSnapshot { path: PathBuf, detail: String },
}

#[cfg(test)]
#[path = "basic_tests.rs"]
mod tests;
