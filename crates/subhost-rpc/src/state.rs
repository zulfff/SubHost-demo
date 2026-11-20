//! Shared node state, its errors, and the lock helpers that guard it.

use crate::block::BlockAuthority;
use jsonrpsee::types::error::ErrorObject;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use subhost_consensus::ValidatorRegistry;
use subhost_core::{
    encode_canonical, Address, Amount, Block, CalldataPolicy, ChainId, FeePolicy, Gas, Hash, Nonce,
    Receipt, Transaction, TransactionType, BLOCK_GAS_LIMIT, CALLDATA_HEADER_VERSION,
    FEE_HEADER_VERSION,
};
use subhost_mempool::{Mempool, MempoolError, MempoolStore};
use subhost_metrics::{LedgerPruneOutcome, Metrics};
use subhost_state::{AccountProof, State};
use subhost_storage::{LedgerDiskStats, LedgerStore, PruneOutcome, StorageError};
use tokio::sync::broadcast;
use tracing::{debug, warn};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("invalid address")]
    InvalidAddress,

    #[error("invalid transaction hash")]
    InvalidTxHash,

    #[error("invalid params")]
    InvalidParams,

    #[error("invalid chain ID")]
    InvalidChainId,

    #[error("transaction signature is required and must be valid")]
    InvalidSignature,

    #[error("unsupported transaction type: {0}")]
    UnsupportedTransactionType(TransactionType),

    #[error("transaction rejected: {0}")]
    TransactionRejected(String),

    #[error("block rejected: {0}")]
    BlockRejected(String),

    #[error("block producer is not an authorized validator")]
    UnauthorizedProducer,

    #[error("account has no state proof at the latest height")]
    UnknownAccount,

    #[error("state persistence failed: {0}")]
    Persistence(String),

    #[error("internal error")]
    Internal,
}

impl From<RpcError> for ErrorObject<'static> {
    fn from(error: RpcError) -> Self {
        // Client mistakes get -32602/-32000 with detail; internal failures get
        // -32603 with no detail so the node does not leak local paths or state.
        let message = error.to_string();
        match error {
            RpcError::InvalidAddress
            | RpcError::InvalidTxHash
            | RpcError::InvalidParams
            | RpcError::InvalidChainId
            | RpcError::InvalidSignature => ErrorObject::owned(-32602, message, None::<()>),
            RpcError::UnsupportedTransactionType(_) | RpcError::TransactionRejected(_) => {
                ErrorObject::owned(-32000, message, None::<()>)
            }
            RpcError::BlockRejected(_)
            | RpcError::UnknownAccount
            | RpcError::UnauthorizedProducer => ErrorObject::owned(-32000, message, None::<()>),
            RpcError::Persistence(_) => {
                ErrorObject::owned(-32603, "state persistence failed", None::<()>)
            }
            RpcError::Internal => ErrorObject::owned(-32603, "internal error", None::<()>),
        }
    }
}

impl From<StorageError> for RpcError {
    fn from(error: StorageError) -> Self {
        Self::Persistence(error.to_string())
    }
}

/// Shared node state behind the RPC surface.
///
/// Cloning is cheap and shares the same ledger: every field is behind an `Arc`.
/// A block commit this node observed, published so the network layer can relay
/// only blocks this node actually produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockEvent {
    /// A block sealed by this node's own producer.
    Sealed(Block),
    /// A block accepted from a peer and re-validated locally.
    Accepted(Block),
}

#[derive(Clone)]
pub struct RpcState {
    pub(crate) chain_id: ChainId,
    /// Genesis namespace used by every signed consensus object.
    pub(crate) consensus_context: Hash,
    /// Latest verified final-QC view, not inferred from restored block height.
    pub(crate) committed_view: Arc<AtomicU64>,
    pub(crate) verified_view: Arc<AtomicBool>,
    /// Height of the newest block committed with a verified final Commit QC
    /// (S8, ADR-013). Demo single-writer commits never advance it.
    pub(crate) finalized_height: Arc<AtomicU64>,
    /// Optional canonical proposal bound configured before serving.
    pub(crate) proposal_bytes_limit: Option<usize>,
    /// Per-block calldata byte budget (Q-037 L0): the sum of `tx.data.len()`
    /// over selected transactions. Independent of the total-byte budget; at
    /// the floor rate it binds before the block gas limit does.
    pub(crate) proposal_calldata_bytes_limit: Option<usize>,
    /// Height of the newest committed block, backing `eth_blockNumber`.
    pub(crate) block_height: Arc<AtomicU64>,
    /// Pending transaction pool.
    pub(crate) mempool: Arc<RwLock<Mempool>>,
    /// Account world state backing `eth_getBalance`.
    pub(crate) state: Arc<RwLock<State>>,
    /// Blocks produced by this node, index `n` holding height `base_height + n + 1`.
    pub(crate) blocks: Arc<RwLock<Vec<Block>>>,
    /// Height of the last block that pruning removed, captured when the state is
    /// built. `0` when history still starts at genesis.
    /// Atomic so an online retention trim can advance it across clones under
    /// the commit lock, exactly like `block_height` (Q-036).
    pub(crate) base_height: Arc<AtomicU64>,
    /// Confirmed receipts keyed by transaction hash.
    pub(crate) receipts: Arc<RwLock<HashMap<Hash, Receipt>>>,
    pub(crate) store: LedgerStore,
    /// Serializes the read-execute-persist-swap sequence.
    pub(crate) commit_lock: Arc<Mutex<()>>,
    /// Block commits, for relay and tests. A lagging receiver only loses events;
    /// it never blocks the producer.
    pub(crate) block_events: broadcast::Sender<BlockEvent>,
    /// Signing identity for blocks this node produces, when it is a validator.
    pub(crate) authority: Option<Arc<BlockAuthority>>,
    /// Who may produce a block, when this node has a genesis validator set.
    ///
    /// Separate from `authority`: a follower verifies peer blocks without holding
    /// a signing key, and a producer needs both.
    pub(crate) validators: Option<Arc<ValidatorRegistry>>,
    /// Metrics registry for committed mempool lifecycle events, attached by
    /// `RpcServer::with_metrics`. `None` drains and discards buffered events
    /// on flush instead of recording them.
    pub(crate) metrics: Option<Metrics>,
    /// Fee-market policy inherited from a fee-enabled genesis (S6.2, Q-017).
    ///
    /// `None` keeps the legacy fixed-price rules: version 1 headers, every fee
    /// burned, no empty blocks. A policy makes the node produce and accept
    /// version 2 headers with a derived base fee and end-of-block priority
    /// settlement.
    pub(crate) fee_policy: Option<FeePolicy>,
    /// Calldata schedule inherited from a calldata-priced genesis (Q-037 L3).
    ///
    /// `None` keeps calldata unpriced. A schedule makes the node enforce the
    /// intrinsic gas floor on version 3 blocks, and the store replays that
    /// floor from the recorded schedule on every load. Independent of the fee
    /// policy: a calldata-only chain binds this with no fee policy at all.
    pub(crate) calldata_policy: Option<CalldataPolicy>,
    /// Block gas limit this chain produces and accepts, from the genesis
    /// document. Defaults to [`BLOCK_GAS_LIMIT`]; a genesis that names a
    /// different limit is honoured end to end (S6.2 step 3).
    pub(crate) block_gas_limit: Gas,
    /// Optional durable pending-transaction store (S6.1b). `None` keeps the
    /// pre-S6.1b volatile behavior: the hooks below still run, but no
    /// `mempool.bin` is written until a consumer configures a store.
    pub(crate) pending_store: Option<MempoolStore>,
}

/// Buffered block events per subscriber. A node only needs the newest few.
const BLOCK_EVENT_CAPACITY: usize = 64;

impl RpcState {
    /// In-memory node, used by tests and ephemeral deployments.
    pub fn in_memory(chain_id: ChainId) -> Result<Self, StorageError> {
        Self::with_store(LedgerStore::ephemeral(chain_id)?)
    }

    /// Node persisted under `data_dir`, restoring any existing ledger.
    pub fn with_data_dir(
        chain_id: ChainId,
        data_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, StorageError> {
        Self::with_store(LedgerStore::open(chain_id, data_dir)?)
    }

    /// Build node state from an already-configured store.
    pub fn with_store(store: LedgerStore) -> Result<Self, StorageError> {
        let restored = store.load()?;
        let height = restored.height();
        let receipts =
            restored.receipts.into_iter().map(|receipt| (receipt.tx_hash, receipt)).collect();
        Ok(Self {
            chain_id: store.chain_id(),
            consensus_context: Hash::ZERO,
            committed_view: Arc::new(AtomicU64::new(0)),
            verified_view: Arc::new(AtomicBool::new(height == 0)),
            finalized_height: Arc::new(AtomicU64::new(0)),
            proposal_bytes_limit: None,
            proposal_calldata_bytes_limit: Some(256 * 1024),
            block_height: Arc::new(AtomicU64::new(height)),
            mempool: Arc::new(RwLock::new(Mempool::default())),
            state: Arc::new(RwLock::new(restored.state)),
            blocks: Arc::new(RwLock::new(restored.blocks)),
            base_height: Arc::new(AtomicU64::new(restored.base_height)),
            receipts: Arc::new(RwLock::new(receipts)),
            store,
            commit_lock: Arc::new(Mutex::new(())),
            block_events: broadcast::channel(BLOCK_EVENT_CAPACITY).0,
            authority: None,
            validators: None,
            metrics: None,
            fee_policy: None,
            calldata_policy: None,
            block_gas_limit: BLOCK_GAS_LIMIT,
            pending_store: None,
        })
    }

    /// Bind the block gas limit named by the genesis document.
    ///
    /// The block path used to hardcode [`BLOCK_GAS_LIMIT`] while
    /// `GenesisConfig::validate` checked the fee policy against
    /// `genesis.gas_limit`, so a genesis naming another limit produced headers
    /// the policy could not extend. The limit is now honoured end to end. A
    /// restored ledger must keep the limit its headers record; changing it
    /// would make the committed tip unverifiable, so that is refused rather
    /// than silently re-framing history.
    pub fn with_block_gas_limit(mut self, limit: Gas) -> Result<Self, RpcError> {
        if limit == 0 {
            return Err(RpcError::BlockRejected("block gas limit must be > 0".into()));
        }
        if let Some(tip) = read(&self.blocks).last() {
            if tip.header.gas_limit != limit {
                return Err(RpcError::BlockRejected(format!(
                    "genesis gas limit {limit} does not match the restored ledger's {}",
                    tip.header.gas_limit
                )));
            }
        }
        self.block_gas_limit = limit;
        Ok(self)
    }

    /// The block gas limit this chain produces and accepts.
    pub fn block_gas_limit(&self) -> Gas {
        self.block_gas_limit
    }

    /// Adopt the fee-market policy of a fee-enabled genesis (S6.2, Q-017).
    ///
    /// Refuses to run a fee-enabled chain on top of a restored legacy ledger:
    /// legacy blocks burned their fees and have no base fee to derive from, so
    /// continuing them would silently reinterpret history. The caller must use
    /// a fresh data directory, matching the genesis decision; nothing here
    /// rewrites or discards existing data.
    pub fn with_fee_policy(mut self, policy: FeePolicy) -> Result<Self, RpcError> {
        policy
            .validate(self.block_gas_limit)
            .map_err(|error| RpcError::BlockRejected(format!("fee policy: {error}")))?;
        let legacy_history =
            read(&self.blocks).iter().any(|block| block.header.version < FEE_HEADER_VERSION);
        if legacy_history {
            return Err(RpcError::BlockRejected(
                "fee-enabled genesis cannot extend a legacy ledger; use a fresh data directory"
                    .into(),
            ));
        }
        // A restored fee-enabled tip must still be extendable under this policy:
        // same gas-limit frame and an in-bounds recorded base fee.
        if let Some(tip) = read(&self.blocks).last() {
            policy.next_context(&tip.header, self.block_gas_limit).map_err(|error| {
                RpcError::BlockRejected(format!(
                    "restored ledger is incompatible with this fee policy: {error}"
                ))
            })?;
        }
        self.fee_policy = Some(policy.clone());
        // The store validates every persist, so it needs the same policy. The
        // node binds it before `with_store` for a restored ledger; this keeps
        // in-memory stores and any later attachment on the same footing.
        // Clones share the verified-head cache, so replacing the handle is safe.
        self.store = self.store.clone().with_fee_policy(policy)?;
        Ok(self)
    }

    /// The fee policy in force, when this chain is fee-enabled.
    pub fn fee_policy(&self) -> Option<&FeePolicy> {
        self.fee_policy.as_ref()
    }

    /// Adopt the calldata schedule of a calldata-priced genesis (Q-037 L3).
    ///
    /// Mirrors [`Self::with_fee_policy`]: refuses a schedule that cannot frame
    /// this chain's gas limit, and refuses to price a restored pre-version-3
    /// ledger, whose calldata was committed unpriced; continuing it under a
    /// floor would reinterpret history, so the caller must use a fresh data
    /// directory. A restored tip must also sit in the same gas-limit frame.
    /// Nothing here rewrites or discards existing data.
    pub fn with_calldata_policy(mut self, policy: CalldataPolicy) -> Result<Self, RpcError> {
        policy
            .validate(self.block_gas_limit)
            .map_err(|error| RpcError::BlockRejected(format!("calldata policy: {error}")))?;
        let pre_v3_history =
            read(&self.blocks).iter().any(|block| block.header.version < CALLDATA_HEADER_VERSION);
        if pre_v3_history {
            return Err(RpcError::BlockRejected(
                "calldata-priced genesis cannot extend a pre-version-3 ledger; use a fresh data directory"
                    .into(),
            ));
        }
        if let Some(tip) = read(&self.blocks).last() {
            if tip.header.gas_limit != self.block_gas_limit {
                return Err(RpcError::BlockRejected(format!(
                    "genesis gas limit {} does not match the restored ledger's {}",
                    self.block_gas_limit, tip.header.gas_limit
                )));
            }
        }
        self.calldata_policy = Some(policy);
        // The store replays the floor on every load and persist, so it needs
        // the same schedule. Clones share the verified-head cache, so replacing
        // the handle is safe, exactly as for the fee policy.
        self.store = self.store.clone().with_calldata_policy(policy)?;
        Ok(self)
    }

    /// The calldata schedule in force, when this chain is calldata-priced.
    pub fn calldata_policy(&self) -> Option<&CalldataPolicy> {
        self.calldata_policy.as_ref()
    }

    /// Prune storage to `height` and trim the in-memory views to match (Q-036).
    ///
    /// Storage first, memory second, both under the commit lock: a refusal or a
    /// failure leaves every in-memory view untouched, and a crash between the
    /// two is safe because `load()` rebuilds the views from the retained suffix.
    /// Returns the storage outcome so the node can log and count it; a prune
    /// never fails an already durable block commit.
    ///
    /// The newest snapshot is the hard floor, so a request at or above it is a
    /// typed `RefusedBySnapshot` no-op. State is unchanged by retention.
    pub fn retain_from(&self, height: u64) -> Result<PruneOutcome, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let outcome = self.store.prune_to_height(height)?;
        self.trim_to_pruned_base(outcome);
        self.record_prune_outcome(outcome);
        Ok(outcome)
    }

    /// Publish a prune outcome and, on a real prune, the new base height
    /// (Q-036). Without a registry the outcome is simply not counted; metrics
    /// never fail a prune. Errors are not an outcome label — they propagate to
    /// the caller before this is reached.
    fn record_prune_outcome(&self, outcome: PruneOutcome) {
        let Some(metrics) = &self.metrics else { return };
        let label = match outcome {
            PruneOutcome::Pruned { base_height, .. } => {
                metrics.set_ledger_base_height(base_height);
                LedgerPruneOutcome::Pruned
            }
            PruneOutcome::NothingToPrune { .. } => LedgerPruneOutcome::NothingToPrune,
            PruneOutcome::RefusedBySnapshot { .. } => LedgerPruneOutcome::RefusedBySnapshot,
        };
        metrics.record_ledger_prune(label);
    }

    /// Apply a storage prune to the in-memory views. Only a `Pruned` outcome
    /// trims; a refusal, a no-op or an error leaves every view untouched.
    fn trim_to_pruned_base(&self, outcome: PruneOutcome) {
        if let PruneOutcome::Pruned { base_height, .. } = outcome {
            let mut blocks = write(&self.blocks);
            blocks.retain(|block| block.header.height > base_height);
            let mut receipts = write(&self.receipts);
            receipts.retain(|_, receipt| receipt.block_height > base_height);
            self.base_height.store(base_height, Ordering::SeqCst);
        }
    }

    /// Attach the durable pending-transaction store (S6.1b, Q-016/Q-034).
    ///
    /// Additive and opt-in: without it the pool stays volatile exactly as before,
    /// while the runtime byte budget still binds. Attach it before serving, and
    /// call [`Self::restore_pending`] only after the ledger and voting journal
    /// have reconciled.
    pub fn with_pending_store(mut self, store: MempoolStore) -> Self {
        self.pending_store = Some(store);
        self
    }

    /// Install a candidate pool: persist it first, publish only on success.
    ///
    /// This is the acknowledgement boundary. A persistence failure leaves the
    /// live pool untouched and propagates, so a caller can never return a
    /// transaction hash for an admission that is not durable. With no store
    /// configured it is a plain swap.
    pub fn publish_pool(&self, candidate: Mempool) -> Result<(), RpcError> {
        if let Some(store) = &self.pending_store {
            store.persist(&candidate).map_err(|error| RpcError::Persistence(error.to_string()))?;
        }
        *write(&self.mempool) = candidate;
        Ok(())
    }

    /// Retire mined transactions from the **live** pool after a durable commit.
    ///
    /// Best-effort by contract: the block is already committed, so a pending-file
    /// cleanup failure is logged and counted, never propagated. Use this only at
    /// a site that owns the live pool; a candidate that may never be installed
    /// must use [`Self::retire_mined_candidate`] instead.
    pub fn retire_mined(&self, mined: &[Transaction], committed: &State) {
        {
            let mut pool = write(&self.mempool);
            Self::retire_mined_candidate(&mut pool, mined, committed);
        }
        self.persist_pending_best_effort();
    }

    /// Retire mined transactions from a **candidate** pool. Pure mutation, no I/O.
    ///
    /// The seal path builds a candidate that its caller installs only on success,
    /// so writing a snapshot here would publish a pool that may never become live.
    /// The pending file is written by [`Self::publish_pool`] when the candidate is
    /// actually installed.
    pub fn retire_mined_candidate(pool: &mut Mempool, mined: &[Transaction], committed: &State) {
        for tx in mined {
            pool.remove(&tx.hash());
            pool.prune_below_nonce(&tx.from, committed.nonce(&tx.from));
        }
    }

    /// Write the live pool to the pending store, tolerating failure.
    ///
    /// A failure means the pending file is stale, not that the chain is broken:
    /// the next restore filters mined nonces again. Counted separately from the
    /// pool lifecycle events, and any increase deserves a ticket.
    pub fn persist_pending_best_effort(&self) {
        let Some(store) = &self.pending_store else {
            return;
        };
        let pool = read(&self.mempool).clone();
        if let Err(error) = store.persist(&pool) {
            if let Some(metrics) = &self.metrics {
                metrics.record_pending_cleanup_failure();
            }
            warn!(%error, "pending snapshot write failed; the committed block stands");
        }
    }

    /// Restore the pending pool after ledger and journal reconciliation.
    ///
    /// Loads and validates the snapshot, drops entries below each sender's
    /// committed nonce, and rebuilds a candidate under the current count,
    /// per-sender, calldata, proposal-byte and runtime-byte limits in
    /// deterministic sender/nonce order. Only then does it become the live pool.
    ///
    /// An entry that is merely underpriced is **preserved**, not dropped: Q-017
    /// requires parked entries to survive a restart and a fee-floor change, so
    /// the restore path admits without the local gas-price floor. Cap-driven
    /// drops are logged with a count. With no store configured this is a no-op.
    pub fn restore_pending(&self) -> Result<(), RpcError> {
        let Some(store) = &self.pending_store else {
            return Ok(());
        };
        let stored = store.load().map_err(|error| RpcError::Persistence(error.to_string()))?;
        if stored.is_empty() {
            return Ok(());
        }
        let committed = read(&self.state).clone();
        let config = read(&self.mempool).config().clone();
        let proposal_budget = self.proposal_bytes_limit.map(|limit| limit.saturating_sub(512));
        let mut candidate = Mempool::new(config);
        let mut ordered = stored;
        ordered.sort_by(|left, right| {
            left.from.cmp(&right.from).then_with(|| left.nonce.cmp(&right.nonce))
        });

        let (mut mined, mut floor_dropped, mut oversized, mut cap_dropped, mut refused) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        for tx in ordered {
            if tx.nonce < committed.nonce(&tx.from) {
                mined += 1;
                continue;
            }
            // Calldata validity floor (Q-037 L4): under-intrinsic gas or data
            // over the protocol maximum can never become valid, both fields
            // are signed, so drop, never park. Runs before admit_restored so a
            // floor-invalid entry can never reach the parked set. First-match
            // order: mined, floor, budget, then admission (cap, economics).
            if self.calldata_policy.as_ref().is_some_and(|policy| policy.check_floor(&tx).is_err())
            {
                floor_dropped += 1;
                continue;
            }
            if proposal_budget.is_some_and(|budget| encode_canonical(&tx).len() > budget) {
                oversized += 1;
                continue;
            }
            match candidate.admit_restored(tx) {
                Ok(admission) if admission.accepted => {}
                // Observation-only split (Q-037): the L0 cap drop gets its own
                // reason instead of folding into refused. Same drop set and
                // pool outcome as before, only the counter becomes visible.
                Err(MempoolError::DataTooLarge { .. }) => cap_dropped += 1,
                Ok(_) | Err(_) => refused += 1,
            }
        }
        debug!(
            mined,
            floor_dropped,
            oversized,
            cap_dropped,
            refused,
            restored = candidate.len(),
            "pending pool restored"
        );
        *write(&self.mempool) = candidate;
        // The filtered pool is now live; rewriting the file is best-effort so a
        // pending-file problem cannot stop a node whose ledger is durable.
        self.persist_pending_best_effort();
        Ok(())
    }

    /// Require blocks from peers to be signed by a registered validator.
    ///
    /// This is the follower half: it enables verification without giving the node
    /// a signing key. Without it the node keeps the pre-S1 behaviour and accepts
    /// unsigned blocks, which is documented as a limitation rather than a
    /// guarantee.
    pub fn with_validators(mut self, validators: ValidatorRegistry) -> Self {
        if self.block_height() > 0 {
            self.verified_view.store(false, Ordering::SeqCst);
        }
        self.validators = Some(Arc::new(validators));
        self
    }

    /// Attach the validator identity that signs blocks this node produces.
    ///
    /// Without one, the node keeps the single-node behaviour: `validator` is
    /// [`Address::ZERO`] and `signatures` is empty. An authority also enables
    /// verification, because producing without checking peer blocks is not a
    /// coherent stance.
    pub fn with_authority(mut self, authority: BlockAuthority) -> Self {
        if self.block_height() > 0 {
            self.verified_view.store(false, Ordering::SeqCst);
        }
        self.validators = Some(Arc::new(authority.validators().clone()));
        self.authority = Some(Arc::new(authority));
        self
    }

    /// Bind signature verification to a deterministic genesis commitment.
    pub fn with_consensus_context(mut self, context: Hash) -> Self {
        if self.consensus_context != context && self.block_height() > 0 {
            self.verified_view.store(false, Ordering::SeqCst);
        }
        self.consensus_context = context;
        self
    }

    /// Bound canonical proposal construction/admission before serving.
    pub fn with_proposal_bytes_limit(mut self, max_bytes: usize) -> Self {
        self.proposal_bytes_limit = Some(max_bytes);
        self
    }

    /// Override the per-block calldata byte budget (Q-037 L0). The default is
    /// 256 KiB; operators may tighten it. Keep it at or above the mempool's
    /// `max_tx_data_bytes`, or a transaction that passes admission could never
    /// be selected.
    pub fn with_proposal_calldata_bytes_limit(mut self, max_bytes: usize) -> Self {
        self.proposal_calldata_bytes_limit = Some(max_bytes);
        self
    }

    /// The per-block calldata byte budget, if set.
    pub fn proposal_calldata_bytes_limit(&self) -> Option<usize> {
        self.proposal_calldata_bytes_limit
    }

    /// Attach the metrics registry used for committed mempool lifecycle events
    /// and ledger retention outcomes (Q-036). Seeds the base-height gauge from
    /// the current base so a restored, already-trimmed ledger publishes its
    /// real base before the first prune.
    pub(crate) fn set_metrics(&mut self, metrics: Metrics) {
        metrics.set_ledger_base_height(self.base_height.load(Ordering::SeqCst));
        self.metrics = Some(metrics);
    }

    /// Publish lifecycle events buffered by committed admissions (S8.2).
    ///
    /// Called after a successful submit: the candidate pool's event buffer was
    /// swapped into the live pool, and rejected candidates dropped theirs, so
    /// this counts exactly the committed admissions. The buffer is drained
    /// unconditionally; without a registry the events are discarded, so they
    /// neither accumulate nor publish when a registry is attached later.
    pub(crate) fn flush_mempool_events(&self) {
        self.publish_pool_events(&mut write(&self.mempool));
    }

    /// Publish (or, without a registry, discard) the pool's buffered lifecycle
    /// events. Shared by the submit flush and the expiry sweep so both use the
    /// same drain policy under the caller's pool lock.
    fn publish_pool_events(&self, pool: &mut Mempool) {
        match &self.metrics {
            Some(metrics) => pool.flush_events(metrics),
            None => pool.drain_events(),
        }
    }

    /// Drop pending transactions past their TTL and publish their events in the
    /// same call, under one pool write lock (Q-044). Returns the dropped count.
    ///
    /// An expiry is a committed pool removal, so it buffers an `expired`
    /// lifecycle event and republishes the sender-depth gauge here, exactly like
    /// an admission flush. Without an attached registry the events are drained.
    /// `now` is a clock argument so a node timer (or a test) controls the sweep;
    /// this never errors and never persists: removals become durable at the next
    /// pending persist, consistent with restore-resets-clock.
    pub fn expire_pending(&self, now: Instant) -> usize {
        let mut pool = write(&self.mempool);
        let dropped = pool.expire_before(now);
        self.publish_pool_events(&mut pool);
        dropped
    }

    pub fn consensus_context(&self) -> Hash {
        self.consensus_context
    }

    /// Latest verified final-QC view. None means restored certificate evidence
    /// is still missing; block height is never treated as a verified view.
    pub fn committed_view(&self) -> Option<u64> {
        self.verified_view
            .load(Ordering::SeqCst)
            .then(|| self.committed_view.load(Ordering::SeqCst))
    }

    pub fn proposal_bytes_limit(&self) -> Option<usize> {
        self.proposal_bytes_limit
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    /// Observe blocks this node seals or accepts.
    pub fn subscribe_blocks(&self) -> broadcast::Receiver<BlockEvent> {
        self.block_events.subscribe()
    }

    pub fn block_height(&self) -> u64 {
        self.block_height.load(Ordering::SeqCst)
    }

    /// Newest height committed with a verified final Commit QC (S8, ADR-013).
    /// Zero until such a commit happens: restored block height and demo
    /// single-writer commits are not finality evidence.
    pub fn finalized_height(&self) -> u64 {
        self.finalized_height.load(Ordering::SeqCst)
    }

    /// Measure the active ledger footprint under the commit lock (S8, ADR-013).
    ///
    /// Serialized with persist/prune so the measurement names a consistent
    /// manifest. Measurement only: no replay and no authentication. An error
    /// means the active data is missing or invalid, never a false zero.
    pub fn ledger_disk_stats(&self) -> Result<LedgerDiskStats, StorageError> {
        let _commit_guard = lock(&self.commit_lock);
        self.store.disk_stats()
    }

    /// Hash of the newest committed block, or [`Hash::ZERO`] at genesis.
    ///
    /// A consensus driver needs this to resume after a restart: the next
    /// proposal must name this hash as its parent.
    pub fn tip_hash(&self) -> Hash {
        read(&self.blocks).last().map_or(Hash::ZERO, Block::hash)
    }

    /// Whether this node checks blocks against a genesis validator set.
    ///
    /// The consensus orchestrator uses this to distinguish validator chains
    /// from development chains without registered block producers.
    pub fn has_validator_set(&self) -> bool {
        self.validators.is_some()
    }

    /// Whether a ledger file already exists on disk.
    pub fn has_persisted_state(&self) -> bool {
        self.store.exists()
    }

    /// Minimum gas price the pool accepts, reported by `eth_gasPrice`.
    pub fn min_gas_price(&self) -> Amount {
        read(&self.mempool).config().min_gas_price
    }

    pub fn balance(&self, address: &Address) -> Amount {
        read(&self.state).balance(address)
    }

    pub fn nonce(&self, address: &Address) -> Nonce {
        read(&self.state).nonce(address)
    }

    /// Merkle root over the current account set, bound to the chain ID.
    pub fn state_root(&self) -> Hash {
        read(&self.state).root()
    }

    /// Merkle inclusion proof for one account at the current state root.
    pub fn account_proof(&self, address: &Address) -> Option<AccountProof> {
        read(&self.state).account_proof(address)
    }

    pub fn receipt(&self, tx_hash: &Hash) -> Option<Receipt> {
        read(&self.receipts).get(tx_hash).cloned()
    }

    pub fn block_at_height(&self, height: u64) -> Option<Block> {
        // A pruned height has no block to serve; the mapping stays a single
        // offset because the retained history is contiguous. The base is read
        // outside the blocks lock, so a concurrent retention trim can advance
        // it between the two reads; the height self-check makes every such
        // interleaving return the correct block or null, never a wrong block
        // (R-RET-01).
        let index = height.checked_sub(self.base_height())?.checked_sub(1)?;
        let index = usize::try_from(index).ok()?;
        read(&self.blocks).get(index).cloned().filter(|block| block.header.height == height)
    }

    /// Height of the newest block this node can serve.
    pub fn head_height(&self) -> u64 {
        self.base_height().saturating_add(self.block_count() as u64)
    }

    /// Height of the last block pruning removed; `0` without pruning.
    pub fn base_height(&self) -> u64 {
        self.base_height.load(Ordering::SeqCst)
    }

    /// Number of retained blocks, which is not the head height after a prune.
    pub fn block_count(&self) -> usize {
        read(&self.blocks).len()
    }

    pub fn pending_count(&self) -> usize {
        read(&self.mempool).len()
    }

    /// Apply genesis (or faucet) allocations and persist them.
    ///
    /// Balances are *set*, not credited, so re-running genesis is idempotent.
    pub fn seed_accounts<I>(&self, allocations: I) -> Result<(), RpcError>
    where
        I: IntoIterator<Item = (Address, Amount)>,
    {
        let _commit_guard = lock(&self.commit_lock);
        let mut candidate = read(&self.state).clone();
        for (address, balance) in allocations {
            candidate.set_balance(address, balance);
        }

        self.persist(&candidate, &read(&self.blocks), &read(&self.receipts))?;
        *write(&self.state) = candidate;
        Ok(())
    }
}

/// Lock helpers that recover from a poisoned lock instead of panicking.
///
/// A panic elsewhere must not turn every later RPC call into a crash: the
/// protected data is a plain snapshot with no torn-state invariant.
pub(crate) fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[path = "finality_disk_tests.rs"]
mod finality_disk_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{signed_transfer, signing_key};
    use ed25519_dalek::Signer;
    use tempfile::tempdir;

    #[test]
    fn seeding_is_idempotent_and_survives_a_restart() {
        let dir = tempdir().unwrap();
        let address = Address::new([1; 20]);
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert!(!state.has_persisted_state());

        state.seed_accounts([(address, 42)]).unwrap();
        state.seed_accounts([(address, 42)]).unwrap();
        assert!(state.has_persisted_state());
        assert_eq!(state.balance(&address), 42);
        drop(state);

        let restored = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert_eq!(restored.block_height(), 0);
        assert!(restored.has_persisted_state());
        assert_eq!(restored.balance(&address), 42);
    }

    #[test]
    fn persisted_ledger_restores_blocks_receipts_and_state() {
        let dir = tempdir().unwrap();
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state.seed_accounts([(sender, 100_000)]).unwrap();
        let hash = state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        drop(state);

        let restored = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert_eq!(restored.block_height(), 1);
        assert_eq!(restored.block_count(), 1);
        assert!(restored.receipt(&hash).is_some());
        assert_eq!(restored.balance(&Address::new([2; 20])), 10);
        assert_eq!(restored.nonce(&sender), 1);
    }

    #[test]
    fn corrupted_ledger_is_rejected_on_startup() {
        let dir = tempdir().unwrap();
        let key = signing_key(7);
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state
            .seed_accounts([(Address::from_public_key(&key.verifying_key().to_bytes()), 100_000)])
            .unwrap();
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        drop(state);

        let path = dir
            .path()
            .join(subhost_storage::LEDGER_DIR_NAME)
            .join(subhost_storage::MANIFEST_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();
        assert!(RpcState::with_data_dir(1, dir.path()).is_err());
    }

    // --- S6.1b pending-store hooks (Q-016/Q-034) ---

    fn pending_store(dir: &std::path::Path) -> MempoolStore {
        MempoolStore::open(1, Hash::ZERO, dir).unwrap()
    }

    fn stored_pool(txs: Vec<Transaction>) -> Mempool {
        let mut pool = Mempool::default();
        for tx in txs {
            pool.add(tx).unwrap();
        }
        pool
    }

    #[test]
    fn publish_pool_persists_before_the_candidate_is_live() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        let key = signing_key(11);
        let tx = signed_transfer(&key, 0, 1);
        let mut candidate = read(&state.mempool).clone();
        candidate.add(tx.clone()).unwrap();

        state.publish_pool(candidate).unwrap();
        assert!(state.mempool.read().unwrap().contains(&tx.hash()));
        let on_disk = store.load().unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].hash(), tx.hash());
    }

    #[test]
    fn publish_pool_failure_keeps_the_live_pool_and_refuses_the_ack() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        // A directory where the snapshot file belongs makes every write fail.
        std::fs::create_dir(store.path()).unwrap();
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store);
        let key = signing_key(12);
        let tx = signed_transfer(&key, 0, 1);
        let mut candidate = read(&state.mempool).clone();
        candidate.add(tx.clone()).unwrap();

        let error = state.publish_pool(candidate).unwrap_err();
        assert!(matches!(error, RpcError::Persistence(_)), "{error:?}");
        assert!(!state.mempool.read().unwrap().contains(&tx.hash()));
        assert_eq!(state.pending_count(), 0);
    }

    #[test]
    fn retire_mined_survives_a_pending_file_failure_and_counts_it() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        std::fs::create_dir(store.path()).unwrap();
        let metrics = Metrics::new().unwrap();
        let key = signing_key(13);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let mut state = RpcState::in_memory(1).unwrap().with_pending_store(store);
        state.set_metrics(metrics.clone());
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        let tx = signed_transfer(&key, 0, 1);
        state.mempool.write().unwrap().add(tx.clone()).unwrap();

        // The block is already durable: cleanup must not propagate a failure.
        let committed = read(&state.state).clone();
        state.retire_mined(std::slice::from_ref(&tx), &committed);
        assert!(!state.mempool.read().unwrap().contains(&tx.hash()));
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(
            rendered.contains("subhost_mempool_pending_cleanup_failures_total 1"),
            "{rendered}"
        );
    }

    #[test]
    fn retire_mined_candidate_writes_nothing() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        let key = signing_key(14);
        let tx = signed_transfer(&key, 0, 1);
        let mut candidate = stored_pool(vec![tx.clone()]);
        let committed = read(&state.state).clone();

        RpcState::retire_mined_candidate(&mut candidate, std::slice::from_ref(&tx), &committed);
        assert!(!candidate.contains(&tx.hash()));
        // A candidate that may never be installed must not publish a snapshot.
        assert!(!store.path().exists());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn restore_pending_drops_mined_nonces_and_preserves_parked_entries() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let key = signing_key(15);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        state.seed_accounts([(sender, 1_000_000)]).unwrap();

        // Nonce 0 is mined by the demo commit path; 1 and 2 stay pending. The
        // stored pool also holds an entry below the *current* local floor.
        let mined = signed_transfer(&key, 0, 1);
        state.submit_transaction(mined.clone()).unwrap();
        let parked = signed_transfer(&key, 1, 1);
        let later = signed_transfer(&key, 2, 1);
        store.persist(&stored_pool(vec![mined.clone(), parked.clone(), later.clone()])).unwrap();

        // Raise the local admission floor after the snapshot was written: a
        // merely underpriced entry must survive the restart (Q-017).
        *write(&state.mempool) =
            Mempool::new(subhost_mempool::MempoolConfig { min_gas_price: 5, ..Default::default() });
        state.restore_pending().unwrap();

        let pool = read(&state.mempool);
        assert!(!pool.contains(&mined.hash()), "a mined nonce must be dropped");
        assert!(pool.contains(&parked.hash()), "a parked underpriced entry must survive");
        assert!(pool.contains(&later.hash()));
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn restore_pending_without_a_store_is_a_noop() {
        let state = RpcState::in_memory(1).unwrap();
        let key = signing_key(16);
        let tx = signed_transfer(&key, 0, 1);
        state.mempool.write().unwrap().add(tx.clone()).unwrap();
        state.restore_pending().unwrap();
        assert!(state.mempool.read().unwrap().contains(&tx.hash()));
    }

    #[test]
    fn restore_pending_refuses_a_corrupt_snapshot_without_touching_the_pool() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        let key = signing_key(17);
        let live = signed_transfer(&key, 0, 1);
        state.mempool.write().unwrap().add(live.clone()).unwrap();
        std::fs::write(store.path(), b"not a snapshot").unwrap();

        let error = state.restore_pending().unwrap_err();
        assert!(matches!(error, RpcError::Persistence(_)), "{error:?}");
        assert!(state.mempool.read().unwrap().contains(&live.hash()));
    }

    #[test]
    fn restore_pending_drops_over_budget_entries_deterministically() {
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let key = signing_key(18);
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        let mut txs = Vec::new();
        for nonce in 0..6 {
            let mut tx = signed_transfer(&key, nonce, 1);
            tx.data = vec![0; 4096];
            let signature = key.sign(&tx.signing_payload()).to_bytes();
            tx.signature = subhost_core::TransactionSignature::from_ed25519(&signature);
            txs.push(tx);
        }
        store.persist(&stored_pool(txs.clone())).unwrap();

        // A budget that fits roughly two entries: the drop order must be the
        // deterministic sender/nonce order, not iteration order.
        let one = encode_canonical(&txs[0]).len();
        *write(&state.mempool) = Mempool::new(subhost_mempool::MempoolConfig {
            max_pending_bytes: one * 2 + 64,
            ..Default::default()
        });
        state.restore_pending().unwrap();
        let pool = read(&state.mempool);
        assert!(pool.len() < txs.len());
        assert!(pool.pending_bytes() <= pool.config().max_pending_bytes);
        assert!(pool.contains(&txs[0].hash()), "the lowest nonces are restored first");
    }

    // --- Q-036 online retention ---

    #[test]
    fn trim_to_pruned_base_drops_blocks_and_receipts_below_the_new_base() {
        let key = signing_key(41);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::in_memory(1).unwrap();
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        for nonce in 0..6 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        assert_eq!(state.block_height(), 6);
        assert_eq!(state.base_height(), 0);

        state.trim_to_pruned_base(PruneOutcome::Pruned {
            dropped_segments: 3,
            retained_segments: 3,
            base_height: 3,
        });
        assert_eq!(state.base_height(), 3);
        assert_eq!(state.block_count(), 3);
        assert_eq!(state.head_height(), 6);
        assert!(state.block_at_height(3).is_none(), "a pruned height is not servable");
        assert_eq!(state.block_at_height(4).unwrap().header.height, 4);
        assert_eq!(read(&state.receipts).len(), 3);
        assert!(read(&state.receipts).values().all(|r| r.block_height > 3));
        // Retention does not touch state: the newest nonce is still committed.
        assert_eq!(state.nonce(&sender), 6);
    }

    #[test]
    fn a_refused_or_no_op_prune_leaves_every_view_untouched() {
        let key = signing_key(42);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::in_memory(1).unwrap();
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        for nonce in 0..3 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        for outcome in [
            PruneOutcome::RefusedBySnapshot { snapshot_height: 1, requested_height: 2 },
            PruneOutcome::NothingToPrune { segments: 1 },
        ] {
            state.trim_to_pruned_base(outcome);
            assert_eq!(state.base_height(), 0);
            assert_eq!(state.block_count(), 3);
            assert_eq!(read(&state.receipts).len(), 3);
        }
    }

    #[test]
    fn retain_from_refuses_at_the_snapshot_floor_and_keeps_memory() {
        let dir = tempdir().unwrap();
        let key = signing_key(43);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        for nonce in 0..4 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        // Seeding persists the genesis anchor at height 0 and the default
        // cadence writes no new snapshot before 1024 blocks, so the floor is 0
        // and any request is the typed refusal, not a fault.
        let outcome = state.retain_from(2).unwrap();
        assert!(
            matches!(
                outcome,
                PruneOutcome::RefusedBySnapshot { snapshot_height: 0, requested_height: 2 }
            ),
            "{outcome:?}"
        );
        assert_eq!(state.base_height(), 0);
        assert_eq!(state.block_count(), 4);
        assert_eq!(read(&state.receipts).len(), 4);
    }

    #[test]
    fn retain_from_on_a_store_without_a_ledger_errors_without_trimming() {
        let state = RpcState::in_memory(1).unwrap();
        let key = signing_key(44);
        let tx = signed_transfer(&key, 0, 1);
        state.mempool.write().unwrap().add(tx).unwrap();
        assert!(state.retain_from(1).is_err());
        assert_eq!(state.base_height(), 0);
        assert_eq!(state.pending_count(), 1);
    }
    #[test]
    fn a_stale_base_read_never_serves_a_wrong_block() {
        // R-RET-01: simulate the trim window directly — the blocks vector is
        // already trimmed to [4,5,6] while a reader still sees the old base 0.
        let key = signing_key(45);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = RpcState::in_memory(1).unwrap();
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        for nonce in 0..6 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        write(&state.blocks).retain(|block| block.header.height > 3);
        assert_eq!(state.base_height(), 0);

        // Without the height self-check these would serve blocks 4, 5 and 6.
        for height in 1..=3 {
            assert!(state.block_at_height(height).is_none(), "height {height}");
        }
        // The stale base can also map a retained height past the trimmed
        // vector; that is a transient null, never a wrong block.
        assert!(state.block_at_height(4).is_none());

        // Once the trim stores the new base, the mapping is correct again.
        state.base_height.store(3, Ordering::SeqCst);
        assert_eq!(state.block_at_height(4).unwrap().header.height, 4);
        assert_eq!(state.block_at_height(6).unwrap().header.height, 6);
        assert!(state.block_at_height(3).is_none());
    }

    #[test]
    fn retain_from_counts_a_refusal_when_metrics_are_attached() {
        let dir = tempdir().unwrap();
        let key = signing_key(45);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let mut state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state.set_metrics(Metrics::new().unwrap());
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        for nonce in 0..4 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        // Floor is 0 (genesis anchor only): the typed refusal must count as
        // refused_by_snapshot, leave the base gauge at 0 and touch no view.
        let outcome = state.retain_from(2).unwrap();
        assert!(matches!(outcome, PruneOutcome::RefusedBySnapshot { .. }));
        let metrics = state.metrics.as_ref().unwrap();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_ledger_prune_total{outcome=\"refused_by_snapshot\"} 1"));
        assert!(rendered.contains("subhost_ledger_prune_total{outcome=\"pruned\"} 0"));
        assert!(rendered.contains("subhost_ledger_base_height 0"));
        assert_eq!(state.base_height(), 0);
        assert_eq!(state.block_count(), 4);
    }

    #[test]
    fn a_prune_error_counts_no_outcome() {
        let mut state = RpcState::in_memory(1).unwrap();
        state.set_metrics(Metrics::new().unwrap());
        assert!(state.retain_from(1).is_err());
        let metrics = state.metrics.as_ref().unwrap();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        for label in ["pruned", "nothing_to_prune", "refused_by_snapshot"] {
            assert!(
                rendered.contains(&format!("subhost_ledger_prune_total{{outcome=\"{label}\"}} 0"))
            );
        }
    }

    #[test]
    fn record_prune_outcome_maps_every_variant_and_moves_the_gauge_only_on_pruned() {
        let mut state = RpcState::in_memory(1).unwrap();
        // Without a registry the recording is a silent no-op, never a panic.
        state.record_prune_outcome(PruneOutcome::Pruned {
            dropped_segments: 1,
            retained_segments: 1,
            base_height: 9,
        });
        state.set_metrics(Metrics::new().unwrap());
        state.record_prune_outcome(PruneOutcome::Pruned {
            dropped_segments: 3,
            retained_segments: 3,
            base_height: 3,
        });
        state.record_prune_outcome(PruneOutcome::NothingToPrune { segments: 3 });
        state.record_prune_outcome(PruneOutcome::RefusedBySnapshot {
            snapshot_height: 5,
            requested_height: 6,
        });
        let metrics = state.metrics.as_ref().unwrap();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_ledger_prune_total{outcome=\"pruned\"} 1"));
        assert!(rendered.contains("subhost_ledger_prune_total{outcome=\"nothing_to_prune\"} 1"));
        assert!(rendered.contains("subhost_ledger_prune_total{outcome=\"refused_by_snapshot\"} 1"));
        // Only the Pruned outcome moves the gauge; the no-op and the refusal
        // leave it at the last real base.
        assert!(rendered.contains("subhost_ledger_base_height 3"));
    }

    #[test]
    fn attaching_metrics_seeds_the_base_gauge_from_the_current_base() {
        let mut state = RpcState::in_memory(1).unwrap();
        // Simulate a restored, already-trimmed ledger base.
        state.base_height.store(1024, Ordering::SeqCst);
        state.set_metrics(Metrics::new().unwrap());
        let metrics = state.metrics.as_ref().unwrap();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_ledger_base_height 1024"));
    }

    #[test]
    fn restore_pending_drops_entries_over_the_tighter_calldata_cap() {
        // nara's L0 ask, end to end: a pending file written under the old
        // 128 KiB cap restores under the new 16 KiB default with the oversized
        // entry cap-dropped and the rest intact (Q-034 filter semantics).
        let dir = tempdir().unwrap();
        let store = pending_store(dir.path());
        let key = signing_key(46);
        let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
        let mut big = signed_transfer(&key, 0, 1);
        big.data = vec![0; 32 * 1024];
        let signature = key.sign(&big.signing_payload()).to_bytes();
        big.signature = subhost_core::TransactionSignature::from_ed25519(&signature);
        let small = signed_transfer(&key, 1, 1);

        let mut old = Mempool::new(subhost_mempool::MempoolConfig {
            max_tx_data_bytes: 128 * 1024,
            ..Default::default()
        });
        old.add(big.clone()).unwrap();
        old.add(small.clone()).unwrap();
        store.persist(&old).unwrap();

        state.restore_pending().unwrap();
        let pool = read(&state.mempool);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&small.hash()));
        assert!(!pool.contains(&big.hash()));
    }

    // --- Q-037 L3 calldata policy binding ---

    use subhost_core::{
        CALLDATA_POLICY_VERSION, FEE_CHANGE_DENOMINATOR, FEE_ELASTICITY_MULTIPLIER,
    };

    fn calldata_schedule() -> CalldataPolicy {
        CalldataPolicy { version: CALLDATA_POLICY_VERSION, gas_per_byte: 16 }
    }

    fn fee_policy() -> FeePolicy {
        FeePolicy {
            initial_base_fee_per_gas: 8,
            min_base_fee_per_gas: 1,
            max_base_fee_per_gas: Amount::MAX / u128::from(BLOCK_GAS_LIMIT),
            elasticity_multiplier: FEE_ELASTICITY_MULTIPLIER,
            change_denominator: FEE_CHANGE_DENOMINATOR,
        }
    }

    #[test]
    fn calldata_policy_binding_validates_the_schedule_and_reports_it() {
        let state = RpcState::in_memory(1).unwrap();
        assert!(state.calldata_policy().is_none());
        for bad in [
            CalldataPolicy { gas_per_byte: 0, ..calldata_schedule() },
            CalldataPolicy { version: 2, ..calldata_schedule() },
            // 256 * 128 KiB exceeds the default 30M block limit.
            CalldataPolicy { gas_per_byte: 256, ..calldata_schedule() },
        ] {
            let error = RpcState::in_memory(1)
                .unwrap()
                .with_calldata_policy(bad)
                .err()
                .expect("an invalid schedule must be refused");
            assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
            assert!(error.to_string().contains("calldata policy"), "{error}");
        }

        let state =
            RpcState::in_memory(1).unwrap().with_calldata_policy(calldata_schedule()).unwrap();
        assert_eq!(state.calldata_policy(), Some(&calldata_schedule()));
        // The store handle carries the same schedule for replay.
        assert_eq!(state.store.calldata_policy(), Some(&calldata_schedule()));
        // Independent of the fee policy: a calldata-only chain binds no fee.
        assert!(state.fee_policy().is_none());

        let both = RpcState::in_memory(1)
            .unwrap()
            .with_fee_policy(fee_policy())
            .unwrap()
            .with_calldata_policy(calldata_schedule())
            .unwrap();
        assert!(both.fee_policy().is_some());
        assert!(both.calldata_policy().is_some());
    }

    #[test]
    fn calldata_policy_refuses_a_pre_v3_ledger() {
        // A legacy v1 ledger must not be repriced under a calldata schedule.
        let legacy_dir = tempdir().unwrap();
        {
            let key = signing_key(50);
            let sender = Address::from_public_key(&key.verifying_key().to_bytes());
            let state = RpcState::with_data_dir(1, legacy_dir.path()).unwrap();
            state.seed_accounts([(sender, 1_000_000)]).unwrap();
            state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
            assert_eq!(state.block_height(), 1);
        }
        let reopened = RpcState::with_data_dir(1, legacy_dir.path()).unwrap();
        let error = reopened
            .with_calldata_policy(calldata_schedule())
            .err()
            .expect("a legacy ledger must not be repriced");
        assert!(error.to_string().contains("cannot extend a pre-version-3 ledger"), "{error}");

        // A fee-only v2 ledger is pre-v3 as well: its calldata was unpriced.
        let fee_dir = tempdir().unwrap();
        {
            let state = RpcState::with_data_dir(1, fee_dir.path())
                .unwrap()
                .with_fee_policy(fee_policy())
                .unwrap();
            state.seed_accounts([]).unwrap();
            let block = state.build_block().unwrap().unwrap();
            state.accept_block(block).unwrap();
        }
        let reopened = RpcState::with_data_dir(1, fee_dir.path()).unwrap();
        let error = reopened
            .with_calldata_policy(calldata_schedule())
            .err()
            .expect("a fee-only v2 ledger must not be repriced");
        assert!(error.to_string().contains("cannot extend a pre-version-3 ledger"), "{error}");
    }

    #[test]
    fn calldata_policy_respects_a_custom_block_gas_limit() {
        // The schedule is validated against the genesis limit, not the default:
        // rate 16 fits exactly in a 2_097_152-gas block, rate 17 does not.
        let ok = RpcState::in_memory(1).unwrap().with_block_gas_limit(2_097_152).unwrap();
        assert!(ok.with_calldata_policy(calldata_schedule()).is_ok());
        let tight = RpcState::in_memory(1).unwrap().with_block_gas_limit(2_097_152).unwrap();
        let error = tight
            .with_calldata_policy(CalldataPolicy { gas_per_byte: 17, ..calldata_schedule() })
            .err()
            .expect("rate 17 does not fit the custom limit");
        assert!(error.to_string().contains("calldata policy"), "{error}");
    }

    // --- Q-044 node sweep accessor: expire_pending ---

    use std::time::Duration;

    fn expiry_state(ttl: Duration, txs: Vec<Transaction>) -> RpcState {
        let state = RpcState::in_memory(1).unwrap();
        let mut pool =
            Mempool::new(subhost_mempool::MempoolConfig { tx_ttl: ttl, ..Default::default() });
        for tx in txs {
            pool.add(tx).unwrap();
        }
        state.publish_pool(pool).unwrap();
        state
    }

    fn rendered(metrics: &Metrics) -> String {
        String::from_utf8(metrics.encode().unwrap()).unwrap()
    }

    #[test]
    fn expire_pending_drops_past_ttl_and_publishes_the_expiry() {
        let metrics = Metrics::new().unwrap();
        let ttl = Duration::from_secs(30 * 60);
        let a = signed_transfer(&signing_key(21), 0, 1);
        let b = signed_transfer(&signing_key(22), 0, 1);
        let mut state = expiry_state(ttl, vec![a.clone(), b.clone()]);
        state.set_metrics(metrics.clone());

        // A clock one TTL plus a second ahead of admission expires both.
        let dropped = state.expire_pending(Instant::now() + ttl + Duration::from_secs(1));
        assert_eq!(dropped, 2);
        assert_eq!(state.pending_count(), 0);
        assert!(!state.mempool.read().unwrap().contains(&a.hash()));
        let out = rendered(&metrics);
        assert!(out.contains("subhost_mempool_transactions_total{event=\"expired\"} 2"), "{out}");
        assert!(out.contains("subhost_mempool_sender_depth{bucket=\"1\"} 0"), "{out}");
    }

    #[test]
    fn expire_pending_keeps_entries_within_the_ttl() {
        let metrics = Metrics::new().unwrap();
        let ttl = Duration::from_secs(30 * 60);
        let tx = signed_transfer(&signing_key(23), 0, 1);
        let mut state = expiry_state(ttl, vec![tx.clone()]);
        state.set_metrics(metrics.clone());

        // The current clock is not yet a full TTL past admission: nothing drops.
        assert_eq!(state.expire_pending(Instant::now()), 0);
        assert!(state.mempool.read().unwrap().contains(&tx.hash()));
        let out = rendered(&metrics);
        assert!(out.contains("subhost_mempool_transactions_total{event=\"expired\"} 0"), "{out}");
        assert!(out.contains("subhost_mempool_sender_depth{bucket=\"1\"} 1"), "{out}");
    }

    #[test]
    fn expire_pending_without_a_registry_drains_and_still_counts() {
        let ttl = Duration::from_secs(30 * 60);
        let tx = signed_transfer(&signing_key(24), 0, 1);
        let mut state = expiry_state(ttl, vec![tx]);
        // No registry attached: the sweep drains the buffered events.
        assert_eq!(state.expire_pending(Instant::now() + ttl + Duration::from_secs(1)), 1);
        assert_eq!(state.pending_count(), 0);

        // A registry attached later publishes nothing from before it.
        let metrics = Metrics::new().unwrap();
        state.set_metrics(metrics.clone());
        state.flush_mempool_events();
        let out = rendered(&metrics);
        assert!(out.contains("subhost_mempool_transactions_total{event=\"expired\"} 0"), "{out}");
    }

    #[test]
    fn expire_pending_on_an_empty_pool_is_a_no_op() {
        let metrics = Metrics::new().unwrap();
        let mut state = expiry_state(Duration::from_secs(60), vec![]);
        state.set_metrics(metrics.clone());
        assert_eq!(state.expire_pending(Instant::now() + Duration::from_secs(3600)), 0);
        let out = rendered(&metrics);
        assert!(out.contains("subhost_mempool_transactions_total{event=\"expired\"} 0"), "{out}");
        assert!(out.contains("subhost_mempool_sender_depth{bucket=\"1\"} 0"), "{out}");
    }

    #[test]
    fn expire_pending_is_idempotent() {
        let ttl = Duration::from_secs(30 * 60);
        let tx = signed_transfer(&signing_key(25), 0, 1);
        let state = expiry_state(ttl, vec![tx]);
        let future = Instant::now() + ttl + Duration::from_secs(1);
        assert_eq!(state.expire_pending(future), 1);
        assert_eq!(state.expire_pending(future), 0);
        assert_eq!(state.pending_count(), 0);
    }
}
