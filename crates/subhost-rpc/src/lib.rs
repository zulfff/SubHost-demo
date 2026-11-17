//! JSON-RPC server and the single-node block producer behind it.
//!
//! Scope: an honest, deliberately small Ethereum-compatible subset for a single
//! local node. A submission is admitted to the pool and the next block is sealed
//! with every pending transaction that is currently executable (per-sender
//! nonce-contiguous, bounded by the block gas limit), so `eth_sendTransaction`
//! either returns a hash for a committed block or an error — there is no pending
//! limbo.
//!
//! Security notes:
//! - `eth_sendTransaction` requires an ed25519 public key and signature; the
//!   signature is verified over the unsigned encoding and the public key must
//!   hash to the `from` address, so a caller cannot spend another account.
//! - Blocks arriving from a peer go through [`RpcState::accept_block`], which
//!   re-verifies parent, height, every signature, both roots, and the resulting
//!   state root before any state changes.
//! - The whole submit path holds one commit lock, and state is only swapped in
//!   after the ledger write succeeds, so a failed persist cannot leave the
//!   in-memory ledger ahead of disk.
//! - The server binds without authentication; it is intended for a loopback or
//!   otherwise access-controlled interface, never a public one.

use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};
use jsonrpsee::server::{BatchRequestConfig, RpcModule, Server, ServerHandle};
use jsonrpsee::types::error::ErrorObject;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use subhost_core::{
    tx_root_of, Address, Amount, Block, BlockHeader, ChainId, Gas, Hash, Nonce, Receipt,
    ReceiptStatus, Transaction, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT,
};
use subhost_mempool::Mempool;
use subhost_state::{AccountProof, State};
use subhost_storage::{
    receipt_commitment, receipts_root, LedgerSnapshot, LedgerStore, StorageError,
};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Largest JSON-RPC request body accepted.
const MAX_REQUEST_BODY_BYTES: u32 = 256 * 1024;
/// Largest JSON-RPC response body produced.
const MAX_RESPONSE_BODY_BYTES: u32 = 1024 * 1024;
/// Largest batch accepted, so one request cannot fan out without bound.
const MAX_BATCH_REQUESTS: u32 = 10;
/// Default gas limit applied when a caller omits one.
const DEFAULT_TX_GAS_LIMIT: Gas = 21_000;

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
            RpcError::BlockRejected(_) | RpcError::UnknownAccount => {
                ErrorObject::owned(-32000, message, None::<()>)
            }
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
    chain_id: ChainId,
    /// Height of the newest committed block, backing `eth_blockNumber`.
    block_height: Arc<AtomicU64>,
    /// Pending transaction pool.
    mempool: Arc<RwLock<Mempool>>,
    /// Account world state backing `eth_getBalance`.
    state: Arc<RwLock<State>>,
    /// Blocks produced by this node, index `n` holding height `n + 1`.
    blocks: Arc<RwLock<Vec<Block>>>,
    /// Confirmed receipts keyed by transaction hash.
    receipts: Arc<RwLock<HashMap<Hash, Receipt>>>,
    store: LedgerStore,
    /// Serializes the read-execute-persist-swap sequence.
    commit_lock: Arc<Mutex<()>>,
    /// Block commits, for relay and tests. A lagging receiver only loses events;
    /// it never blocks the producer.
    block_events: broadcast::Sender<BlockEvent>,
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
            block_height: Arc::new(AtomicU64::new(height)),
            mempool: Arc::new(RwLock::new(Mempool::default())),
            state: Arc::new(RwLock::new(restored.state)),
            blocks: Arc::new(RwLock::new(restored.blocks)),
            receipts: Arc::new(RwLock::new(receipts)),
            store,
            commit_lock: Arc::new(Mutex::new(())),
            block_events: broadcast::channel(BLOCK_EVENT_CAPACITY).0,
        })
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
        let index = usize::try_from(height.checked_sub(1)?).ok()?;
        read(&self.blocks).get(index).cloned()
    }

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

    /// Submit a transaction: admit it to a copy of the pool, then seal the next
    /// block with every pending transaction that is currently executable.
    ///
    /// The live pool is only replaced after the ledger write succeeds, so a
    /// failed commit leaves admission, replacement, and eviction entirely undone.
    pub fn submit_transaction(&self, tx: Transaction) -> Result<Hash, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let requested_hash = tx.hash();

        let mut candidate_pool = read(&self.mempool).clone();
        let admission = candidate_pool
            .admit(tx)
            .map_err(|error| RpcError::TransactionRejected(error.to_string()))?;
        if !admission.accepted {
            return Err(RpcError::TransactionRejected(
                "a transaction with this sender and nonce already has an equal or higher gas price"
                    .into(),
            ));
        }
        debug_assert_eq!(admission.hash, requested_hash);

        let receipts = self.seal_block(&mut candidate_pool)?;
        if !receipts.contains_key(&requested_hash) {
            return Err(RpcError::TransactionRejected(
                "transaction is not executable against the current state".into(),
            ));
        }

        *write(&self.mempool) = candidate_pool;
        Ok(requested_hash)
    }

    /// Execute one pending transaction and commit it with the rest of the next
    /// block's executable transactions.
    pub fn include_transaction(&self, tx_hash: &Hash) -> Result<Receipt, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let mut candidate_pool = read(&self.mempool).clone();
        if !candidate_pool.contains(tx_hash) {
            return Err(RpcError::InvalidTxHash);
        }
        let receipts = self.seal_block(&mut candidate_pool)?;
        let receipt = receipts.get(tx_hash).cloned().ok_or(RpcError::InvalidTxHash)?;
        *write(&self.mempool) = candidate_pool;
        Ok(receipt)
    }

    /// Transactions the producer may include, in deterministic order.
    ///
    /// Senders are visited in address order and each contributes the nonce
    /// continuous prefix that fits in the remaining block gas. Execution stops at
    /// the first rejected transaction for a sender, so a block can never skip a
    /// nonce. [`State::apply_transaction`] validates before mutating, so a reject
    /// leaves `state` untouched.
    fn select_transactions(&self, pool: &Mempool, state: &mut State) -> Vec<Transaction> {
        let mut senders: Vec<Address> = pool.pending().iter().map(|tx| tx.from).collect();
        senders.sort_unstable();
        senders.dedup();

        let mut selected = Vec::new();
        let mut gas_used: Gas = 0;
        for sender in senders {
            for tx in pool.ready_for(&sender, state.nonce(&sender)) {
                let Some(next_gas) = gas_used.checked_add(tx.gas_limit) else { break };
                if next_gas > BLOCK_GAS_LIMIT {
                    break;
                }
                if state.apply_transaction(&tx).is_err() {
                    break;
                }
                gas_used = next_gas;
                selected.push(tx);
            }
        }
        selected
    }

    /// Seal every executable pending transaction into one block.
    ///
    /// On success the pool has the mined transactions removed and every other
    /// in-memory view has been swapped; on error nothing is published. The caller
    /// installs the mutated pool only after this returns `Ok`.
    fn seal_block(&self, pool: &mut Mempool) -> Result<HashMap<Hash, Receipt>, RpcError> {
        let mut candidate_state = read(&self.state).clone();
        let transactions = self.select_transactions(pool, &mut candidate_state);
        if transactions.is_empty() {
            return Ok(HashMap::new());
        }

        let previous_block = read(&self.blocks).last().cloned();
        let height = match previous_block.as_ref() {
            Some(block) => block.header.height.checked_add(1).ok_or(RpcError::Internal)?,
            None => 1,
        };
        let parent_hash = previous_block.as_ref().map_or(Hash::ZERO, Block::hash);

        let mut hashes = Vec::with_capacity(transactions.len());
        let mut commitments = Vec::with_capacity(transactions.len());
        let mut gas_used: Gas = 0;
        for (index, tx) in transactions.iter().enumerate() {
            gas_used = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            hashes.push(tx.hash());
            commitments.push(receipt_commitment(
                tx.hash(),
                height,
                u32::try_from(index).map_err(|_| RpcError::Internal)?,
                tx.gas_limit,
                gas_used,
            ));
        }

        let block = Block {
            header: BlockHeader {
                version: 1,
                chain_id: self.chain_id,
                height,
                timestamp: subhost_core::unix_timestamp(),
                parent_hash,
                state_root: candidate_state.root(),
                tx_root: tx_root_of(&hashes),
                receipt_root: receipts_root(&commitments),
                // A single-node producer has no validator identity to attest with.
                validator: Address::ZERO,
                gas_used,
                gas_limit: BLOCK_GAS_LIMIT,
                extra_data: Vec::new(),
            },
            transactions,
            signatures: Vec::new(),
        };

        let block_hash = block.hash();
        let mut receipts = HashMap::with_capacity(block.transactions.len());
        let mut cumulative_gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used =
                cumulative_gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            receipts.insert(
                tx.hash(),
                Receipt {
                    tx_hash: tx.hash(),
                    block_hash,
                    block_height: height,
                    transaction_index: u32::try_from(index).map_err(|_| RpcError::Internal)?,
                    gas_used: tx.gas_limit,
                    cumulative_gas_used,
                    status: ReceiptStatus::Success,
                    logs: Vec::new(),
                    contract_address: None,
                },
            );
        }

        let mut next_blocks = read(&self.blocks).clone();
        next_blocks.push(block.clone());
        let mut next_receipts = read(&self.receipts).clone();
        for (hash, receipt) in &receipts {
            next_receipts.insert(*hash, receipt.clone());
        }

        // Durability before visibility: if this fails nothing is swapped in.
        self.persist(&candidate_state, &next_blocks, &next_receipts)?;

        for tx in &block.transactions {
            pool.remove(&tx.hash());
        }
        *write(&self.state) = candidate_state;
        *write(&self.blocks) = next_blocks;
        *write(&self.receipts) = next_receipts;
        self.block_height.store(height, Ordering::SeqCst);
        // A send fails only when no receiver is attached, which is normal.
        let _ = self.block_events.send(BlockEvent::Sealed(block));
        Ok(receipts)
    }

    /// Accept a block produced elsewhere (a peer or the consensus driver).
    ///
    /// The block is fully re-validated before any state change: parent, height,
    /// chain, gas accounting, every signature, both commitments, the state
    /// transition, and the resulting state root. Receipts are derived locally
    /// and never taken from the wire.
    pub fn accept_block(&self, block: Block) -> Result<Vec<Receipt>, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let previous = read(&self.blocks).last().cloned();
        let expected_height = match previous.as_ref() {
            Some(previous) => previous.header.height.checked_add(1).ok_or(RpcError::Internal)?,
            None => 1,
        };
        let expected_parent = previous.as_ref().map_or(Hash::ZERO, Block::hash);

        if block.header.height != expected_height
            || block.header.parent_hash != expected_parent
            || block.header.chain_id != self.chain_id
            || block.header.gas_limit != BLOCK_GAS_LIMIT
            || block.transactions.is_empty()
        {
            return Err(RpcError::BlockRejected(format!(
                "block does not extend this chain at height {expected_height}"
            )));
        }

        let mut candidate_state = read(&self.state).clone();
        let mut seen = HashSet::with_capacity(block.transactions.len());
        let mut hashes = Vec::with_capacity(block.transactions.len());
        let mut commitments = Vec::with_capacity(block.transactions.len());
        let mut gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            if tx.chain_id != self.chain_id || !tx.verify_signature() || !seen.insert(tx.hash()) {
                return Err(RpcError::BlockRejected(format!(
                    "invalid transaction {index} in block {}",
                    block.header.height
                )));
            }
            let next_gas = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            if next_gas > block.header.gas_limit {
                return Err(RpcError::BlockRejected(format!(
                    "block {} uses more than its {} gas limit",
                    block.header.height, block.header.gas_limit
                )));
            }
            candidate_state
                .apply_transaction(tx)
                .map_err(|error| RpcError::BlockRejected(error.to_string()))?;
            gas_used = next_gas;
            hashes.push(tx.hash());
            commitments.push(receipt_commitment(
                tx.hash(),
                block.header.height,
                u32::try_from(index).map_err(|_| RpcError::Internal)?,
                tx.gas_limit,
                gas_used,
            ));
        }

        if gas_used != block.header.gas_used
            || block.header.tx_root != tx_root_of(&hashes)
            || block.header.receipt_root != receipts_root(&commitments)
            || candidate_state.root() != block.header.state_root
        {
            return Err(RpcError::BlockRejected(format!(
                "block {} commitments or state transition do not verify",
                block.header.height
            )));
        }

        let block_hash = block.hash();
        let mut receipts = Vec::with_capacity(block.transactions.len());
        let mut cumulative_gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used =
                cumulative_gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            receipts.push(Receipt {
                tx_hash: tx.hash(),
                block_hash,
                block_height: block.header.height,
                transaction_index: u32::try_from(index).map_err(|_| RpcError::Internal)?,
                gas_used: tx.gas_limit,
                cumulative_gas_used,
                status: ReceiptStatus::Success,
                logs: Vec::new(),
                contract_address: None,
            });
        }

        let mut next_blocks = read(&self.blocks).clone();
        next_blocks.push(block.clone());
        let mut next_receipts = read(&self.receipts).clone();
        for receipt in &receipts {
            next_receipts.insert(receipt.tx_hash, receipt.clone());
        }

        self.persist(&candidate_state, &next_blocks, &next_receipts)?;

        {
            let mut pool = write(&self.mempool);
            for tx in &block.transactions {
                pool.remove(&tx.hash());
                pool.prune_below_nonce(&tx.from, candidate_state.nonce(&tx.from));
            }
        }
        *write(&self.state) = candidate_state;
        *write(&self.blocks) = next_blocks;
        *write(&self.receipts) = next_receipts;
        self.block_height.store(block.header.height, Ordering::SeqCst);
        let _ = self.block_events.send(BlockEvent::Accepted(block));
        Ok(receipts)
    }

    fn persist(
        &self,
        state: &State,
        blocks: &[Block],
        receipts: &HashMap<Hash, Receipt>,
    ) -> Result<(), RpcError> {
        // Sort receipts by height so the persisted file is byte-stable.
        let mut ordered: Vec<Receipt> = receipts.values().cloned().collect();
        ordered.sort_by_key(|receipt| receipt.block_height);
        self.store.persist(&LedgerSnapshot {
            chain_id: self.chain_id,
            state: state.snapshot(),
            blocks: blocks.to_vec(),
            receipts: ordered,
        })?;
        Ok(())
    }
}

/// Lock helpers that recover from a poisoned lock instead of panicking.
///
/// A panic elsewhere must not turn every later RPC call into a crash: the
/// protected data is a plain snapshot with no torn-state invariant.
fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Verify that `signature` was produced by `public_key` over `tx`, and that the
/// key really controls `tx.from`.
fn verify_transaction_signature(
    tx: &Transaction,
    public_key: &[u8; 32],
    signature: &[u8; 64],
) -> Result<(), RpcError> {
    let verifying_key =
        VerifyingKey::from_bytes(public_key).map_err(|_| RpcError::InvalidSignature)?;
    // Bind the key to the claimed sender before spending anything.
    if Address::from_public_key(public_key) != tx.from {
        return Err(RpcError::InvalidSignature);
    }
    verifying_key
        .verify(&tx.signing_payload(), &Ed25519Signature::from_bytes(signature))
        .map_err(|_| RpcError::InvalidSignature)
}

fn parse_address(value: &str) -> Result<Address, RpcError> {
    if value.len() != 42 {
        return Err(RpcError::InvalidAddress);
    }
    Address::from_hex(value).map_err(|_| RpcError::InvalidAddress)
}

fn parse_tx_hash(value: &str) -> Result<Hash, RpcError> {
    if value.len() != 66 {
        return Err(RpcError::InvalidTxHash);
    }
    Hash::from_hex(value).map_err(|_| RpcError::InvalidTxHash)
}

/// Strict Ethereum QUANTITY parsing: `0x`-prefixed, non-empty, no leading zeros.
fn hex_digits(value: &serde_json::Value) -> Result<&str, RpcError> {
    let serde_json::Value::String(text) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    Ok(digits)
}

fn hex_quantity(value: Option<&serde_json::Value>, default: u64) -> Result<u64, RpcError> {
    let Some(value) = value else { return Ok(default) };
    u64::from_str_radix(hex_digits(value)?, 16).map_err(|_| RpcError::InvalidParams)
}

fn hex_amount(value: Option<&serde_json::Value>, default: Amount) -> Result<Amount, RpcError> {
    let Some(value) = value else { return Ok(default) };
    Amount::from_str_radix(hex_digits(value)?, 16).map_err(|_| RpcError::InvalidParams)
}

/// Parse a `0x`-prefixed fixed-width byte string (public keys, signatures).
fn fixed_hex<const N: usize>(value: Option<&serde_json::Value>) -> Result<[u8; N], RpcError> {
    let text = value.and_then(serde_json::Value::as_str).ok_or(RpcError::InvalidSignature)?;
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidSignature)?;
    let bytes = hex::decode(digits).map_err(|_| RpcError::InvalidSignature)?;
    bytes.as_slice().try_into().map_err(|_| RpcError::InvalidSignature)
}

/// Optional `0x`-prefixed calldata.
fn hex_data(value: Option<&serde_json::Value>) -> Result<Vec<u8>, RpcError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let text = value.as_str().ok_or(RpcError::InvalidParams)?;
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    hex::decode(digits).map_err(|_| RpcError::InvalidParams)
}

/// Resolve a block tag (`latest`, `earliest`, `pending`, or a hex height) to a
/// concrete height. `Ok(None)` means "no such block", which maps to JSON `null`.
fn resolve_block_height(tag: &str, block_count: usize) -> Result<Option<u64>, RpcError> {
    if block_count == 0 {
        return Ok(None);
    }
    let newest = block_count as u64;
    match tag {
        "latest" | "pending" | "safe" | "finalized" => Ok(Some(newest)),
        "earliest" => Ok(Some(1)),
        value => {
            let height =
                u64::from_str_radix(value.strip_prefix("0x").ok_or(RpcError::InvalidParams)?, 16)
                    .map_err(|_| RpcError::InvalidParams)?;
            Ok((height >= 1 && height <= newest).then_some(height))
        }
    }
}

fn receipt_json(receipt: &Receipt) -> serde_json::Value {
    serde_json::json!({
        "transactionHash": receipt.tx_hash.to_string(),
        "blockHash": receipt.block_hash.to_string(),
        "blockNumber": format!("0x{:x}", receipt.block_height),
        "transactionIndex": format!("0x{:x}", receipt.transaction_index),
        "cumulativeGasUsed": format!("0x{:x}", receipt.cumulative_gas_used),
        "gasUsed": format!("0x{:x}", receipt.gas_used),
        "status": format!("0x{:x}", receipt.status as u8),
        "contractAddress": receipt.contract_address.map(|address| address.to_string()),
        "logs": receipt.logs.iter().map(|log| serde_json::json!({
            "address": log.address.to_string(),
            "topics": log.topics.iter().map(Hash::to_string).collect::<Vec<_>>(),
            "data": format!("0x{}", hex::encode(&log.data)),
        })).collect::<Vec<_>>(),
    })
}

fn transaction_json(tx: &Transaction, block: &Block, index: usize) -> serde_json::Value {
    serde_json::json!({
        "hash": tx.hash().to_string(),
        "blockHash": block.hash().to_string(),
        "blockNumber": format!("0x{:x}", block.header.height),
        "transactionIndex": format!("0x{index:x}"),
        "from": tx.from.to_string(),
        "to": tx.to.map(|address| address.to_string()),
        "value": format!("0x{:x}", tx.value),
        "nonce": format!("0x{:x}", tx.nonce),
        "gas": format!("0x{:x}", tx.gas_limit),
        "gasPrice": format!("0x{:x}", tx.gas_price),
        "chainId": format!("0x{:x}", tx.chain_id),
        "input": format!("0x{}", hex::encode(&tx.data)),
        "type": tx.tx_type.to_string(),
    })
}

fn block_json(block: &Block, full_transactions: bool) -> serde_json::Value {
    let transactions = if full_transactions {
        block
            .transactions
            .iter()
            .enumerate()
            .map(|(index, tx)| transaction_json(tx, block, index))
            .collect::<Vec<_>>()
    } else {
        block
            .transactions
            .iter()
            .map(|tx| serde_json::Value::String(tx.hash().to_string()))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "number": format!("0x{:x}", block.header.height),
        "hash": block.hash().to_string(),
        "parentHash": block.header.parent_hash.to_string(),
        "stateRoot": block.header.state_root.to_string(),
        "transactionsRoot": block.header.tx_root.to_string(),
        "receiptsRoot": block.header.receipt_root.to_string(),
        "miner": block.header.validator.to_string(),
        "timestamp": format!("0x{:x}", block.header.timestamp),
        "gasLimit": format!("0x{:x}", block.header.gas_limit),
        "gasUsed": format!("0x{:x}", block.header.gas_used),
        "extraData": format!("0x{}", hex::encode(&block.header.extra_data)),
        "transactions": transactions,
    })
}

/// Build the transaction described by an `eth_sendTransaction` parameter object.
fn transaction_from_params(
    params: &serde_json::Value,
    configured_chain_id: ChainId,
    public_key: [u8; subhost_core::PUBLIC_KEY_SIZE],
) -> Result<Transaction, RpcError> {
    let from = params
        .get("from")
        .and_then(serde_json::Value::as_str)
        .ok_or(RpcError::InvalidAddress)
        .and_then(parse_address)?;
    let to = match params.get("to").and_then(serde_json::Value::as_str) {
        Some(value) => Some(parse_address(value)?),
        None => None,
    };
    let chain_id = hex_quantity(params.get("chainId"), configured_chain_id)?;
    if chain_id != configured_chain_id {
        return Err(RpcError::InvalidChainId);
    }

    // Accept `gas` (spec) and `gasLimit` (widely used) for the same field.
    let gas_limit = match params.get("gas").or_else(|| params.get("gasLimit")) {
        Some(value) => hex_quantity(Some(value), DEFAULT_TX_GAS_LIMIT)?,
        None => DEFAULT_TX_GAS_LIMIT,
    };

    // Only transfers execute today; refuse anything else up front instead of
    // admitting it and failing during execution.
    if to.is_none() {
        return Err(RpcError::UnsupportedTransactionType(TransactionType::ContractCreation));
    }

    Ok(Transaction {
        tx_type: TransactionType::Transfer,
        nonce: hex_quantity(params.get("nonce"), 0)?,
        from,
        to,
        value: hex_amount(params.get("value"), 0)?,
        gas_price: hex_amount(params.get("gasPrice"), 1)?,
        gas_limit,
        data: hex_data(params.get("data").or_else(|| params.get("input")))?,
        chain_id,
        public_key,
        signature: TransactionSignature::EMPTY,
    })
}

/// Tunables for the JSON-RPC listener.
#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    pub listen_addr: SocketAddr,
    pub max_connections: u32,
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self { listen_addr: SocketAddr::from(([127, 0, 0, 1], 8545)), max_connections: 1_000 }
    }
}

/// The JSON-RPC server.
///
/// The server has no authentication or TLS of its own: expose it on loopback, or
/// behind a reverse proxy that terminates both.
#[derive(Clone)]
pub struct RpcServer {
    state: RpcState,
}

impl RpcServer {
    pub fn new(state: RpcState) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &RpcState {
        &self.state
    }

    /// Serve until the handle is stopped.
    pub async fn run(self, config: RpcServerConfig) -> Result<(), RpcServeError> {
        let (_, handle) = self.start(config).await?;
        handle.stopped().await;
        Ok(())
    }

    /// Bind and start serving, returning the bound address and stop handle.
    pub async fn start(
        self,
        config: RpcServerConfig,
    ) -> Result<(SocketAddr, ServerHandle), RpcServeError> {
        let server = Server::builder()
            .max_connections(config.max_connections.max(1))
            .max_request_body_size(MAX_REQUEST_BODY_BYTES)
            .max_response_body_size(MAX_RESPONSE_BODY_BYTES)
            .set_batch_request_config(BatchRequestConfig::Limit(MAX_BATCH_REQUESTS))
            .build(config.listen_addr)
            .await
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;

        let addr = server
            .local_addr()
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;
        let handle = server.start(self.build_module()?);

        if !addr.ip().is_loopback() {
            warn!(
                %addr,
                "JSON-RPC is bound to a non-loopback address without authentication; \
                 restrict access at the network layer"
            );
        }
        info!(%addr, chain_id = self.state.chain_id(), "JSON-RPC server started");
        Ok((addr, handle))
    }

    fn build_module(&self) -> Result<RpcModule<RpcState>, RpcServeError> {
        let mut module = RpcModule::new(self.state.clone());

        module.register_method("eth_chainId", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.chain_id()))
        })?;

        module.register_method("net_version", |_params, state, _extensions| {
            Ok::<_, RpcError>(state.chain_id().to_string())
        })?;

        module.register_method("eth_blockNumber", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.block_height()))
        })?;

        module.register_method("eth_gasPrice", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.min_gas_price()))
        })?;

        module.register_method("eth_getBalance", |params, state, _extensions| {
            // Accept `[address]` as well as the spec's `[address, block]`.
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            Ok::<_, RpcError>(format!("0x{:x}", state.balance(&parse_address(&address)?)))
        })?;

        module.register_method("eth_getTransactionCount", |params, state, _extensions| {
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            Ok::<_, RpcError>(format!("0x{:x}", state.nonce(&parse_address(&address)?)))
        })?;

        // Subhost extension: a Merkle membership proof for an account at the
        // latest state root. `storageKeys` and a block tag are accepted for shape
        // compatibility, but this chain has no contract storage yet, so the
        // response contains no storage proof. Absence cannot be proven.
        module.register_method("eth_getProof", |params, state, _extensions| {
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            let address = parse_address(&address)?;
            let proof = state.account_proof(&address).ok_or(RpcError::UnknownAccount)?;

            let mut account_proof = Vec::with_capacity(proof.proof.siblings.len() + 1);
            account_proof.push(proof.proof.leaf.to_string());
            account_proof.extend(proof.proof.siblings.iter().map(Hash::to_string));

            Ok::<_, RpcError>(serde_json::json!({
                "address": address.to_string(),
                "accountProof": account_proof,
                "balance": format!("0x{:x}", proof.account.balance),
                "nonce": format!("0x{:x}", proof.account.nonce),
                "stateRoot": proof.root.to_string(),
                "leafIndex": format!("0x{:x}", proof.proof.index),
                "storageProof": [],
            }))
        })?;

        module.register_method("eth_sendTransaction", |params, state, _extensions| {
            let request: serde_json::Value = params.one().map_err(|_| RpcError::InvalidParams)?;

            // Signature is mandatory: the node never signs on a caller's behalf.
            let public_key = fixed_hex::<32>(request.get("publicKey"))?;
            let signature = fixed_hex::<64>(request.get("signature"))?;
            let unsigned = transaction_from_params(&request, state.chain_id(), public_key)?;
            verify_transaction_signature(&unsigned, &public_key, &signature)?;

            let tx = Transaction {
                signature: TransactionSignature::from_ed25519(&signature),
                ..unsigned
            };
            Ok::<_, RpcError>(state.submit_transaction(tx)?.to_string())
        })?;

        module.register_method("eth_getTransactionReceipt", |params, state, _extensions| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            Ok::<_, RpcError>(
                state
                    .receipt(&parse_tx_hash(&tx_hash)?)
                    .as_ref()
                    .map_or(serde_json::Value::Null, receipt_json),
            )
        })?;

        module.register_method("eth_getBlockByNumber", |params, state, _extensions| {
            let mut sequence = params.sequence();
            let tag: String = sequence.next().map_err(|_| RpcError::InvalidParams)?;
            let full_transactions: bool =
                sequence.optional_next().unwrap_or(Some(false)).unwrap_or(false);
            let Some(height) = resolve_block_height(&tag, state.block_count())? else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            Ok::<_, RpcError>(
                state
                    .block_at_height(height)
                    .map_or(serde_json::Value::Null, |block| block_json(&block, full_transactions)),
            )
        })?;

        module.register_method("eth_getTransactionByHash", |params, state, _extensions| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            let tx_hash = parse_tx_hash(&tx_hash)?;
            let Some(receipt) = state.receipt(&tx_hash) else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            let Some(block) = state.block_at_height(receipt.block_height) else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            let found = block
                .transactions
                .iter()
                .position(|tx| tx.hash() == tx_hash)
                .map(|index| transaction_json(&block.transactions[index], &block, index));
            Ok::<_, RpcError>(found.unwrap_or(serde_json::Value::Null))
        })?;

        Ok(module)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcServeError {
    #[error("cannot bind JSON-RPC server to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot register JSON-RPC method: {0}")]
    Registration(#[from] jsonrpsee::core::RegisterMethodError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn signed_transfer(key: &SigningKey, nonce: Nonce, value: Amount) -> Transaction {
        let public_key = key.verifying_key().to_bytes();
        let unsigned = Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::from_public_key(&public_key),
            to: Some(Address::new([2; 20])),
            value,
            gas_price: 1,
            gas_limit: DEFAULT_TX_GAS_LIMIT,
            data: Vec::new(),
            chain_id: 1,
            public_key,
            signature: TransactionSignature::EMPTY,
        };
        let signature = key.sign(&unsigned.signing_payload()).to_bytes();
        Transaction { signature: TransactionSignature::from_ed25519(&signature), ..unsigned }
    }

    fn funded_state(key: &SigningKey, balance: Amount) -> RpcState {
        let state = RpcState::in_memory(1).unwrap();
        let address = Address::from_public_key(&key.verifying_key().to_bytes());
        state.seed_accounts([(address, balance)]).unwrap();
        state
    }

    async fn rpc_request(addr: SocketAddr, body: serde_json::Value) -> serde_json::Value {
        let payload = body.to_string();
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
    }

    async fn start_test_server(state: RpcState) -> (SocketAddr, ServerHandle) {
        RpcServer::new(state)
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
            })
            .await
            .unwrap()
    }

    #[test]
    fn quantity_parsing_is_strict() {
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x1f")), 0).unwrap(), 31);
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x0")), 0).unwrap(), 0);
        assert_eq!(hex_quantity(None, 7).unwrap(), 7);
        assert!(hex_quantity(Some(&serde_json::json!("1f")), 0).is_err(), "missing 0x");
        assert!(hex_quantity(Some(&serde_json::json!("0x01")), 0).is_err(), "leading zero");
        assert!(hex_quantity(Some(&serde_json::json!("0x")), 0).is_err(), "empty");
        assert!(hex_quantity(Some(&serde_json::json!("0xzz")), 0).is_err(), "non-hex");
        assert!(hex_quantity(Some(&serde_json::json!(31)), 0).is_err(), "number");
        assert!(hex_amount(Some(&serde_json::json!("0x100000000000000000000")), 0).is_ok());
    }

    #[test]
    fn fixed_and_variable_hex_parsing_is_strict() {
        assert!(fixed_hex::<32>(Some(&serde_json::json!("00".repeat(32)))).is_err());
        assert!(
            fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(31))))).is_err()
        );
        assert!(fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(32))))).is_ok());
        assert!(fixed_hex::<32>(None).is_err());

        assert_eq!(hex_data(None).unwrap(), Vec::<u8>::new());
        assert_eq!(hex_data(Some(&serde_json::json!("0x"))).unwrap(), Vec::<u8>::new());
        assert_eq!(hex_data(Some(&serde_json::json!("0xdead"))).unwrap(), vec![0xde, 0xad]);
        assert!(hex_data(Some(&serde_json::json!("dead"))).is_err());
        assert!(hex_data(Some(&serde_json::json!("0xodd"))).is_err());
    }

    #[test]
    fn address_and_hash_parsing_reject_wrong_widths() {
        assert!(parse_address("0x1234").is_err());
        assert!(parse_address(&format!("0x{}", "11".repeat(20))).is_ok());
        assert!(parse_tx_hash(&format!("0x{}", "11".repeat(20))).is_err());
        assert!(parse_tx_hash(&format!("0x{}", "11".repeat(32))).is_ok());
    }

    #[test]
    fn block_tag_resolution_covers_tags_heights_and_gaps() {
        assert_eq!(resolve_block_height("latest", 0).unwrap(), None);
        assert_eq!(resolve_block_height("latest", 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("pending", 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("finalized", 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("earliest", 3).unwrap(), Some(1));
        assert_eq!(resolve_block_height("0x2", 3).unwrap(), Some(2));
        assert_eq!(resolve_block_height("0x0", 3).unwrap(), None, "genesis has no block");
        assert_eq!(resolve_block_height("0x9", 3).unwrap(), None, "beyond the tip");
        assert!(resolve_block_height("2", 3).is_err());
    }

    #[test]
    fn signature_verification_binds_payload_and_sender() {
        let key = signing_key(7);
        let public_key = key.verifying_key().to_bytes();
        let tx = signed_transfer(&key, 0, 10);
        let signature = tx.signature.to_ed25519();
        assert!(verify_transaction_signature(&tx, &public_key, &signature).is_ok());

        // Any mutated field invalidates the signature.
        let mut tampered = tx.clone();
        tampered.value = 11;
        assert!(matches!(
            verify_transaction_signature(&tampered, &public_key, &signature),
            Err(RpcError::InvalidSignature)
        ));

        // A valid signature from another key cannot spend this account.
        let attacker = signing_key(9);
        let attacker_key = attacker.verifying_key().to_bytes();
        let attacker_signature = attacker.sign(&tx.signing_payload()).to_bytes();
        assert!(matches!(
            verify_transaction_signature(&tx, &attacker_key, &attacker_signature),
            Err(RpcError::InvalidSignature)
        ));
    }

    #[test]
    fn producer_executes_transfer_and_records_receipt() {
        let key = signing_key(7);
        let state = funded_state(&key, 100_000);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());

        let hash = state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        assert_eq!(state.balance(&Address::new([2; 20])), 10);
        assert_eq!(state.nonce(&sender), 1);
        assert_eq!(state.pending_count(), 0, "mined transaction must leave the pool");

        let receipt = state.receipt(&hash).unwrap();
        assert_eq!(receipt.block_height, 1);
        assert_eq!(receipt.status, ReceiptStatus::Success);
        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.header.parent_hash, Hash::ZERO);
        assert_eq!(block.header.tx_root, tx_root_of(&[hash]));
    }

    #[test]
    fn consecutive_transfers_chain_blocks() {
        let key = signing_key(7);
        let state = funded_state(&key, 1_000_000);
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        state.submit_transaction(signed_transfer(&key, 1, 20)).unwrap();

        assert_eq!(state.block_height(), 2);
        let first = state.block_at_height(1).unwrap();
        let second = state.block_at_height(2).unwrap();
        assert_eq!(second.header.parent_hash, first.hash());
        assert_eq!(state.balance(&Address::new([2; 20])), 30);
    }

    #[tokio::test]
    async fn block_events_distinguish_sealed_from_accepted_blocks() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let producer = funded_state(&key, 1_000_000);
        let mut events = producer.subscribe_blocks();

        producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        match events.recv().await.unwrap() {
            BlockEvent::Sealed(block) => assert_eq!(block.header.height, 1),
            other => panic!("expected a sealed block, got {other:?}"),
        }

        let follower = RpcState::in_memory(1).unwrap();
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        let mut follower_events = follower.subscribe_blocks();
        follower.accept_block(producer.block_at_height(1).unwrap()).unwrap();
        match follower_events.recv().await.unwrap() {
            BlockEvent::Accepted(block) => assert_eq!(block.header.height, 1),
            other => panic!("expected an accepted block, got {other:?}"),
        }
    }

    #[test]
    fn one_block_carries_every_executable_transaction() {
        let alice = signing_key(7);
        let bob = signing_key(8);
        let state = RpcState::in_memory(1).unwrap();
        state
            .seed_accounts([
                (Address::from_public_key(&alice.verifying_key().to_bytes()), 100_000),
                (Address::from_public_key(&bob.verifying_key().to_bytes()), 100_000),
            ])
            .unwrap();

        // Park Alice's transaction without asking the producer for a block.
        let alice_tx = signed_transfer(&alice, 0, 10);
        state.mempool.write().unwrap().add(alice_tx.clone()).unwrap();

        let bob_tx = signed_transfer(&bob, 0, 20);
        let hash = state.submit_transaction(bob_tx.clone()).unwrap();
        assert_eq!(hash, bob_tx.hash());

        assert_eq!(state.block_height(), 1);
        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(state.pending_count(), 0);
        assert_eq!(block.header.gas_used, 2 * DEFAULT_TX_GAS_LIMIT);

        let receipts: Vec<Receipt> = block
            .transactions
            .iter()
            .map(|tx| state.receipt(&tx.hash()).expect("every included transaction has a receipt"))
            .collect();
        let indexes: Vec<u32> = receipts.iter().map(|receipt| receipt.transaction_index).collect();
        assert_eq!(indexes, vec![0, 1]);
        assert_eq!(receipts[0].cumulative_gas_used, DEFAULT_TX_GAS_LIMIT);
        assert_eq!(receipts[1].cumulative_gas_used, 2 * DEFAULT_TX_GAS_LIMIT);
    }

    #[test]
    fn a_nonce_gap_blocks_later_transactions_of_the_same_sender() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = funded_state(&key, 1_000_000);

        // Nonce 1 cannot execute before nonce 0, so the submission is rejected
        // and leaves no trace in the pool.
        let gap = signed_transfer(&key, 1, 10);
        assert!(matches!(
            state.submit_transaction(gap.clone()),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.pending_count(), 0);

        // Once nonce 0 arrives, the contiguous prefix of both mines in order.
        state.mempool.write().unwrap().add(gap).unwrap();
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        assert_eq!(state.block_at_height(1).unwrap().transactions.len(), 2);
        assert_eq!(state.nonce(&sender), 2);
        assert_eq!(state.balance(&Address::new([2; 20])), 20);
    }

    #[test]
    fn accept_block_replays_a_peer_block_and_rejects_tampering() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let producer = funded_state(&key, 1_000_000);
        let hash = producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        let block = producer.block_at_height(1).unwrap();
        assert_eq!(block.transactions[0].hash(), hash);

        let follower = RpcState::in_memory(1).unwrap();
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        // The same transaction reached the follower through gossip; accepting the
        // block must prune it from the pool.
        follower.mempool.write().unwrap().add(block.transactions[0].clone()).unwrap();

        let receipts = follower.accept_block(block.clone()).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(follower.block_height(), 1);
        assert_eq!(follower.block_count(), 1);
        assert_eq!(follower.state_root(), block.header.state_root);
        assert_eq!(follower.balance(&Address::new([2; 20])), 10);
        assert_eq!(follower.nonce(&sender), 1);
        assert_eq!(follower.pending_count(), 0);

        // The same height cannot be replayed on top of itself.
        assert!(matches!(follower.accept_block(block.clone()), Err(RpcError::BlockRejected(_))));

        // A mutated transaction invalidates its signature (and every root).
        let mut tampered = block.clone();
        tampered.transactions[0].value += 1;
        let other = RpcState::in_memory(1).unwrap();
        other.seed_accounts([(sender, 1_000_000)]).unwrap();
        assert!(matches!(other.accept_block(tampered), Err(RpcError::BlockRejected(_))));
        assert_eq!(other.block_height(), 0);

        // Even a well-formed block fails when its declared state root is wrong.
        let mut bad_state_root = block;
        bad_state_root.header.state_root = Hash::ZERO;
        let third = RpcState::in_memory(1).unwrap();
        third.seed_accounts([(sender, 1_000_000)]).unwrap();
        assert!(matches!(third.accept_block(bad_state_root), Err(RpcError::BlockRejected(_))));
        assert_eq!(third.block_height(), 0);
    }

    #[test]
    fn rejects_insufficient_balance_without_mutating_state() {
        let key = signing_key(7);
        let state = RpcState::in_memory(1).unwrap();
        assert!(matches!(
            state.submit_transaction(signed_transfer(&key, 0, 10)),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.block_count(), 0);
        assert_eq!(state.pending_count(), 0, "rejected transaction must not linger");
    }

    #[test]
    fn rejects_gas_above_the_block_limit_without_commit() {
        let key = signing_key(7);
        let state = funded_state(&key, Amount::MAX / 2);
        let mut tx = signed_transfer(&key, 0, 10);
        tx.gas_limit = BLOCK_GAS_LIMIT + 1;
        assert!(matches!(state.submit_transaction(tx), Err(RpcError::TransactionRejected(_))));
        assert_eq!(state.block_height(), 0);
        assert!(state.pending_count() == 0);
    }

    #[test]
    fn failed_replacement_restores_the_previous_pending_transaction() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        // Unfunded: the replacement will fail during execution.
        let state = RpcState::in_memory(1).unwrap();
        let original = signed_transfer(&key, 0, 10);
        state.mempool.write().unwrap().add(original.clone()).unwrap();

        let mut replacement = original.clone();
        replacement.gas_price = 2;
        assert!(state.submit_transaction(replacement).is_err());

        let (_, restored) = read(&state.mempool)
            .get_by_sender_nonce(&sender, 0)
            .expect("the original transaction must be back in the pool");
        assert_eq!(restored.gas_price, original.gas_price);
    }

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

        let path = dir.path().join(subhost_storage::LEDGER_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();
        assert!(RpcState::with_data_dir(1, dir.path()).is_err());
    }

    #[test]
    fn contract_creation_is_refused_before_it_reaches_the_pool() {
        let request = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "value": "0x1",
        });
        assert!(matches!(
            transaction_from_params(&request, 1, [0u8; 32]),
            Err(RpcError::UnsupportedTransactionType(TransactionType::ContractCreation))
        ));
    }

    #[test]
    fn transaction_params_reject_a_foreign_chain_id() {
        let request = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "to": Address::new([2; 20]).to_string(),
            "chainId": "0x9",
        });
        assert!(matches!(
            transaction_from_params(&request, 1, [0u8; 32]),
            Err(RpcError::InvalidChainId)
        ));
    }

    #[test]
    fn transaction_params_accept_gas_and_gas_limit_aliases() {
        let base = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "to": Address::new([2; 20]).to_string(),
        });
        assert_eq!(
            transaction_from_params(&base, 1, [0u8; 32]).unwrap().gas_limit,
            DEFAULT_TX_GAS_LIMIT
        );

        let mut with_gas = base.clone();
        with_gas["gas"] = serde_json::json!("0x7530");
        assert_eq!(transaction_from_params(&with_gas, 1, [0u8; 32]).unwrap().gas_limit, 30_000);

        let mut with_gas_limit = base;
        with_gas_limit["gasLimit"] = serde_json::json!("0x7530");
        assert_eq!(
            transaction_from_params(&with_gas_limit, 1, [0u8; 32]).unwrap().gas_limit,
            30_000
        );
    }

    #[tokio::test]
    async fn http_round_trip_covers_send_receipt_block_and_transaction() {
        let key = signing_key(7);
        let public_key = key.verifying_key().to_bytes();
        let sender = Address::from_public_key(&public_key);
        let state = funded_state(&key, 100_000);
        let (addr, handle) = start_test_server(state).await;

        let tx = signed_transfer(&key, 0, 10);
        let send = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                    "nonce": "0x0",
                    "gasPrice": "0x1",
                    "gas": "0x5208",
                    "chainId": "0x1",
                    "data": "0x",
                    "publicKey": format!("0x{}", hex::encode(public_key)),
                    "signature": format!("0x{}", hex::encode(tx.signature.to_ed25519())),
                }],
            }),
        )
        .await;
        let hash = send["result"].as_str().expect("a transaction hash").to_string();
        assert_eq!(hash, tx.hash().to_string());

        let receipt = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "eth_getTransactionReceipt", "params": [hash.clone()],
            }),
        )
        .await;
        assert_eq!(receipt["result"]["blockNumber"], "0x1");
        assert_eq!(receipt["result"]["status"], "0x1");

        let block = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3,
                "method": "eth_getBlockByNumber", "params": ["latest", true],
            }),
        )
        .await;
        assert_eq!(block["result"]["number"], "0x1");
        assert_eq!(block["result"]["transactions"][0]["hash"], hash.as_str());
        assert_eq!(block["result"]["transactions"][0]["from"], sender.to_string());

        let transaction = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 4,
                "method": "eth_getTransactionByHash", "params": [hash],
            }),
        )
        .await;
        assert_eq!(transaction["result"]["value"], "0xa");
        assert_eq!(transaction["result"]["type"], "transfer");

        let balance = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 5,
                "method": "eth_getBalance",
                "params": [Address::new([2; 20]).to_string(), "latest"],
            }),
        )
        .await;
        assert_eq!(balance["result"], "0xa");

        let nonce = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 6,
                "method": "eth_getTransactionCount",
                "params": [sender.to_string(), "latest"],
            }),
        )
        .await;
        assert_eq!(nonce["result"], "0x1");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_get_proof_returns_a_verifiable_merkle_path() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = funded_state(&key, 100_000);
        let (addr, handle) = start_test_server(state.clone()).await;

        let response = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "eth_getProof",
                "params": [sender.to_string(), [], "latest"],
            }),
        )
        .await;
        let result = &response["result"];

        assert_eq!(result["address"], sender.to_string());
        assert_eq!(result["balance"], "0x186a0");
        assert_eq!(result["nonce"], "0x0");
        assert_eq!(result["stateRoot"], state.state_root().to_string());

        // The first element is the leaf, the rest are the path from the bottom.
        let path = result["accountProof"].as_array().expect("an account proof");
        assert_eq!(path[0], state.account_proof(&sender).unwrap().proof.leaf.to_string());
        assert_eq!(path.len(), 1 + state.account_proof(&sender).unwrap().proof.siblings.len());

        let proof = state.account_proof(&sender).unwrap();
        assert!(proof.verify(&state.state_root()));

        // An account that has never been touched cannot be proven absent.
        let unknown = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "eth_getProof",
                "params": [Address::new([9; 20]).to_string(), [], "latest"],
            }),
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32000);

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_surface_reports_chain_metadata_and_missing_data_as_null() {
        let state = RpcState::in_memory(1337).unwrap();
        let (addr, handle) = start_test_server(state).await;

        let chain_id = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
        )
        .await;
        assert_eq!(chain_id["result"], "0x539");

        let net_version = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"net_version","params":[]}),
        )
        .await;
        assert_eq!(net_version["result"], "1337");

        let gas_price = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"eth_gasPrice","params":[]}),
        )
        .await;
        assert_eq!(gas_price["result"], "0x1");

        let block = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"eth_getBlockByNumber","params":["latest", false],
            }),
        )
        .await;
        assert!(block["result"].is_null(), "an empty chain has no latest block");

        let receipt = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"eth_getTransactionReceipt",
                "params":[format!("0x{}", "11".repeat(32))],
            }),
        )
        .await;
        assert!(receipt["result"].is_null());

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_surface_rejects_unsigned_and_malformed_requests() {
        let key = signing_key(7);
        let state = funded_state(&key, 100_000);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let (addr, handle) = start_test_server(state).await;

        // No signature at all.
        let unsigned = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                }],
            }),
        )
        .await;
        assert_eq!(unsigned["error"]["code"], -32602);

        // Well-formed signature from the wrong key.
        let attacker = signing_key(9);
        let victim_tx = signed_transfer(&key, 0, 10);
        let forged = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                    "nonce": "0x0",
                    "gasPrice": "0x1",
                    "gas": "0x5208",
                    "chainId": "0x1",
                    "publicKey": format!("0x{}", hex::encode(attacker.verifying_key().to_bytes())),
                    "signature": format!(
                        "0x{}",
                        hex::encode(attacker.sign(&victim_tx.signing_payload()).to_bytes())
                    ),
                }],
            }),
        )
        .await;
        assert_eq!(forged["error"]["code"], -32602);

        let bad_address = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":3,"method":"eth_getBalance","params":["0x1234","latest"],
            }),
        )
        .await;
        assert_eq!(bad_address["error"]["code"], -32602);

        let bad_hash = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"eth_getTransactionReceipt","params":["0xzz"],
            }),
        )
        .await;
        assert_eq!(bad_hash["error"]["code"], -32602);

        // Nothing above may have moved funds.
        let balance = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"eth_getBalance",
                "params":[Address::new([2; 20]).to_string(),"latest"],
            }),
        )
        .await;
        assert_eq!(balance["result"], "0x0");

        handle.stop().unwrap();
        handle.stopped().await;
    }
}
