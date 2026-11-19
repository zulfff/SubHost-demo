//! Shared node state, its errors, and the lock helpers that guard it.

use crate::block::BlockAuthority;
use jsonrpsee::types::error::ErrorObject;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use subhost_consensus::ValidatorRegistry;
use subhost_core::{Address, Amount, Block, ChainId, Hash, Nonce, Receipt, TransactionType};
use subhost_mempool::Mempool;
use subhost_metrics::Metrics;
use subhost_state::{AccountProof, State};
use subhost_storage::{LedgerStore, StorageError};
use tokio::sync::broadcast;

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
    /// Optional canonical proposal bound configured before serving.
    pub(crate) proposal_bytes_limit: Option<usize>,
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
    pub(crate) base_height: u64,
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
    /// `RpcServer::with_metrics`. `None` keeps the pool's event buffer inert.
    pub(crate) metrics: Option<Metrics>,
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
            proposal_bytes_limit: None,
            block_height: Arc::new(AtomicU64::new(height)),
            mempool: Arc::new(RwLock::new(Mempool::default())),
            state: Arc::new(RwLock::new(restored.state)),
            blocks: Arc::new(RwLock::new(restored.blocks)),
            base_height: restored.base_height,
            receipts: Arc::new(RwLock::new(receipts)),
            store,
            commit_lock: Arc::new(Mutex::new(())),
            block_events: broadcast::channel(BLOCK_EVENT_CAPACITY).0,
            authority: None,
            validators: None,
            metrics: None,
        })
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

    /// Attach the metrics registry used for committed mempool lifecycle events.
    pub(crate) fn set_metrics(&mut self, metrics: Metrics) {
        self.metrics = Some(metrics);
    }

    /// Publish lifecycle events buffered by committed admissions (S8.2).
    ///
    /// Called after a successful submit: the candidate pool's event buffer was
    /// swapped into the live pool, and rejected candidates dropped theirs, so
    /// this counts exactly the committed admissions. Inert without metrics.
    pub(crate) fn flush_mempool_events(&self) {
        if let Some(metrics) = &self.metrics {
            write(&self.mempool).flush_events(metrics);
        }
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
        // offset because the retained history is contiguous.
        let index = height.checked_sub(self.base_height)?.checked_sub(1)?;
        let index = usize::try_from(index).ok()?;
        read(&self.blocks).get(index).cloned()
    }

    /// Height of the newest block this node can serve.
    pub fn head_height(&self) -> u64 {
        self.base_height.saturating_add(self.block_count() as u64)
    }

    /// Height of the last block pruning removed; `0` without pruning.
    pub fn base_height(&self) -> u64 {
        self.base_height
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
mod tests {
    use super::*;
    use crate::test_support::{signed_transfer, signing_key};
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
}
