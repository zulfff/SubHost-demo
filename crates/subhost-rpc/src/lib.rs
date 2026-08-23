use jsonrpsee::types::error::ErrorObject;
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};
use jsonrpsee::server::{BatchRequestConfig, Server, ServerHandle, RpcModule};
use serde::{Deserialize, Serialize};
use subhost_core::{
    Address, Block, BlockHeader, Hash, Receipt, ReceiptStatus, Transaction,
    TransactionSignature, TransactionType, ValidatorSignature,
};
use subhost_mempool::{Mempool, MempoolConfig};
use subhost_state::{State, StateSnapshot};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io::Write};
use tempfile::NamedTempFile;
use tracing::{info, debug};

const MAX_STATE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const STATE_MAGIC: [u8; 8] = *b"SUBHOST1";
const STATE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Invalid address")]
    InvalidAddress,
    
    #[error("Invalid transaction hash")]
    InvalidTxHash,
    
    #[error("Block not found")]
    BlockNotFound,
    
    #[error("Invalid params")]
    InvalidParams,

    #[error("Invalid chain ID")]
    InvalidChainId,

    #[error("Invalid nonce")]
    InvalidNonce,

    #[error("Transaction signature is required and must be valid")]
    InvalidSignature,

    #[error("Internal error")]
    InternalError,

    #[error("transaction rejected: {0}")]
    TransactionRejected(String),

    #[error("state persistence failed")]
    Persistence,
}

impl From<RpcError> for ErrorObject<'static> {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::InvalidAddress => ErrorObject::owned(-32602, "Invalid address", None::<()>),
            RpcError::InvalidTxHash => ErrorObject::owned(-32602, "Invalid transaction hash", None::<()>),
            RpcError::BlockNotFound => ErrorObject::owned(-32000, "Block not found", None::<()>),
            RpcError::InvalidParams => ErrorObject::owned(-32602, "Invalid params", None::<()>),
            RpcError::InvalidChainId => ErrorObject::owned(-32602, "Invalid chain ID", None::<()>),
            RpcError::InvalidNonce => ErrorObject::owned(-32602, "Invalid nonce", None::<()>),
            RpcError::InvalidSignature => ErrorObject::owned(-32602, "Invalid transaction signature", None::<()>),
            RpcError::InternalError => ErrorObject::owned(-32603, "Internal error", None::<()>),
            RpcError::TransactionRejected(_) => ErrorObject::owned(-32000, "Transaction rejected", None::<()>),
            RpcError::Persistence => ErrorObject::owned(-32603, "State persistence failed", None::<()>),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedNode {
    chain_id: u64,
    state: StateSnapshot,
    blocks: Vec<Block>,
    receipts: Vec<Receipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedEnvelope {
    magic: [u8; 8],
    version: u32,
    checksum: [u8; 32],
    payload: Vec<u8>,
}

#[derive(Clone)]
pub struct RpcState {
    pub chain_id: u64,
    /// Current (mutable) block height backing `eth_blockNumber`.
    block_height: Arc<AtomicU64>,
    /// Pending transaction pool - `eth_sendTransaction` inserts here.
    pub mempool: Arc<RwLock<Mempool>>,
    /// Account world state backing `eth_getBalance`.
    pub state: Arc<RwLock<State>>,
    /// Canonical blocks produced by this local node.
    pub blocks: Arc<RwLock<Vec<Block>>>,
    /// Confirmed receipts keyed by transaction hash.
    pub receipts: Arc<RwLock<std::collections::HashMap<Hash, Receipt>>>,
    data_file: Option<PathBuf>,
    commit_lock: Arc<Mutex<()>>,
}

impl RpcState {
    pub fn new(chain_id: u64) -> Self {
        Self::with_data_file(chain_id, None).expect("in-memory RPC state must initialize")
    }

    pub fn with_data_dir(chain_id: u64, data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let data_dir = data_dir.as_ref();
        fs::create_dir_all(data_dir)?;
        Self::with_data_file(chain_id, Some(data_dir.join("node-state.bin")))
    }

    fn with_data_file(chain_id: u64, data_file: Option<PathBuf>) -> anyhow::Result<Self> {
        if chain_id == 0 {
            anyhow::bail!("chain ID cannot be zero");
        }

        let mut state = State::with_chain_id(chain_id);
        let mut blocks = Vec::new();
        let mut receipts = std::collections::HashMap::new();

        if let Some(path) = &data_file {
            if path.exists() {
                let metadata = fs::metadata(path)?;
                if metadata.len() > MAX_STATE_FILE_BYTES {
                    anyhow::bail!("state file exceeds {} bytes", MAX_STATE_FILE_BYTES);
                }
                let encoded = fs::read(path)?;
                let envelope: PersistedEnvelope = bincode::deserialize(&encoded)?;
                if envelope.magic != STATE_MAGIC || envelope.version != STATE_FORMAT_VERSION {
                    anyhow::bail!("unsupported persisted state format");
                }
                if *blake3::hash(&envelope.payload).as_bytes() != envelope.checksum {
                    anyhow::bail!("persisted state checksum mismatch");
                }
                let persisted: PersistedNode = bincode::deserialize(&envelope.payload)?;
                if persisted.chain_id != chain_id || persisted.state.chain_id != chain_id {
                    anyhow::bail!("persisted state chain ID does not match configured chain ID");
                }
                state = State::from_snapshot(persisted.state)?;
                validate_chain(
                    &persisted.blocks,
                    &persisted.receipts,
                    chain_id,
                    &state,
                )?;
                blocks = persisted.blocks;
                receipts = persisted
                    .receipts
                    .into_iter()
                    .map(|receipt| (receipt.tx_hash, receipt))
                    .collect();
            }
        }

        let height = blocks.last().map(|block| block.header.height).unwrap_or(0);
        Ok(Self {
            chain_id,
            block_height: Arc::new(AtomicU64::new(height)),
            mempool: Arc::new(RwLock::new(Mempool::new(MempoolConfig::default()))),
            state: Arc::new(RwLock::new(state)),
            blocks: Arc::new(RwLock::new(blocks)),
            receipts: Arc::new(RwLock::new(receipts)),
            data_file,
            commit_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn set_block_height(&self, height: u64) {
        self.block_height.store(height, Ordering::SeqCst);
    }

    pub fn block_height(&self) -> u64 {
        self.block_height.load(Ordering::SeqCst)
    }

    pub fn has_persisted_state(&self) -> bool {
        self.data_file.as_ref().is_some_and(|path| path.is_file())
    }

    pub fn receipt(&self, hash: &Hash) -> Option<Receipt> {
        self.receipts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(hash)
            .cloned()
    }

    pub fn seed_account(&self, address: Address, balance: u128) -> anyhow::Result<()> {
        self.seed_accounts([(address, balance)])
    }

    pub fn seed_accounts<I>(&self, allocations: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = (Address, u128)>,
    {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current_state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut candidate_state = current_state.clone();
        drop(current_state);
        for (address, balance) in allocations {
            candidate_state.set_balance(address, balance);
        }
        let blocks = self
            .blocks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let receipts = self
            .receipts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut persisted_receipts: Vec<_> = receipts.values().cloned().collect();
        persisted_receipts.sort_by_key(|receipt| receipt.block_height);
        self.persist(PersistedNode {
            chain_id: self.chain_id,
            state: candidate_state.snapshot(),
            blocks,
            receipts: persisted_receipts,
        })
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        *self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = candidate_state;
        Ok(())
    }

    pub fn submit_transaction(&self, tx: Transaction) -> Result<Hash, RpcError> {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let requested_hash = Mempool::transaction_hash(&tx);
        let previous_pending = {
            let mempool = self
                .mempool
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            mempool
                .get_by_sender_nonce(&tx.from, tx.nonce)
                .map(|(_, existing_tx)| existing_tx)
        };
        let (hash, displaced) = {
            let mut mempool = self
                .mempool
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (accepted_hash, displaced) = mempool
                .add_with_eviction(tx)
                .map_err(|error| RpcError::TransactionRejected(error.to_string()))?;
            if accepted_hash != requested_hash {
                return Err(RpcError::TransactionRejected(
                    "a transaction with this sender and nonce already has equal or higher gas price"
                        .to_string(),
                ));
            }
            (accepted_hash, displaced)
        };
        if let Err(error) = self.include_transaction_locked(&hash) {
            let mut mempool = self
                .mempool
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            mempool.remove(&hash);
            if let Some(previous_tx) = previous_pending {
                mempool
                    .add(previous_tx)
                    .expect("restoring replaced transaction to the mempool must succeed");
            }
            if let Some(displaced_tx) = displaced {
                mempool
                    .add(displaced_tx)
                    .expect("restoring an evicted transaction to the mempool must succeed");
            }
            return Err(error);
        }
        Ok(hash)
    }

    /// Execute and persist one pending transaction as the next local block.
    pub fn include_transaction(&self, tx_hash: &Hash) -> Result<Receipt, RpcError> {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.include_transaction_locked(tx_hash)
    }

    fn include_transaction_locked(&self, tx_hash: &Hash) -> Result<Receipt, RpcError> {
        let tx = {
            let mempool = self
                .mempool
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            mempool.get(tx_hash).cloned().ok_or(RpcError::InvalidTxHash)?
        };

        let candidate_state = {
            let current = self
                .state
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut candidate = current.clone();
            candidate
                .apply_transaction(&tx)
                .map_err(|error| RpcError::TransactionRejected(error.to_string()))?;
            candidate
        };

        let previous_block = self
            .blocks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .cloned();
        let height = match previous_block.as_ref() {
            Some(block) => block
                .header
                .height
                .checked_add(1)
                .ok_or(RpcError::InternalError)?,
            None => 1,
        };
        let parent_hash = previous_block
            .as_ref()
            .map(Block::hash)
            .unwrap_or(Hash::ZERO);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RpcError::InternalError)?
            .as_secs();
        let tx_root = Hash::from_data(
            &bincode::serialize(&vec![*tx_hash.as_bytes()]).map_err(|_| RpcError::InternalError)?,
        );
        let gas_used = tx.gas_limit;
        if gas_used > 30_000_000 {
            return Err(RpcError::TransactionRejected(
                "transaction gas exceeds block gas limit".to_string(),
            ));
        }
        let receipt_root = receipt_root(*tx_hash, height, gas_used);
        let header = BlockHeader {
            version: 1,
            chain_id: self.chain_id,
            height,
            timestamp,
            parent_hash,
            state_root: Hash::from_data(
                &bincode::serialize(&candidate_state.snapshot())
                    .map_err(|_| RpcError::InternalError)?,
            ),
            tx_root,
            receipt_root,
            validator: Address::new([0u8; 20]),
            gas_used,
            gas_limit: 30_000_000,
            extra_data: Vec::new(),
        };
        let block = Block {
            header,
            transactions: vec![tx],
            signatures: Vec::<ValidatorSignature>::new(),
        };
        let receipt = Receipt {
            tx_hash: *tx_hash,
            block_hash: block.hash(),
            block_height: height,
            gas_used,
            status: ReceiptStatus::Success,
            logs: Vec::new(),
            contract_address: None,
        };

        let mut next_blocks = self
            .blocks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        next_blocks.push(block);
        let mut next_receipts = self
            .receipts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        next_receipts.insert(*tx_hash, receipt.clone());
        let mut persisted_receipts: Vec<_> = next_receipts.values().cloned().collect();
        persisted_receipts.sort_by_key(|receipt| receipt.block_height);
        self.persist(PersistedNode {
            chain_id: self.chain_id,
            state: candidate_state.snapshot(),
            blocks: next_blocks.clone(),
            receipts: persisted_receipts,
        })?;

        *self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = candidate_state;
        self.mempool
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(tx_hash);
        *self
            .blocks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next_blocks;
        *self
            .receipts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next_receipts;
        self.set_block_height(height);
        Ok(receipt)
    }

    fn persist(&self, snapshot: PersistedNode) -> Result<(), RpcError> {
        let Some(path) = &self.data_file else {
            return Ok(());
        };
        let parent = path.parent().ok_or(RpcError::Persistence)?;
        fs::create_dir_all(parent).map_err(|_| RpcError::Persistence)?;
        let payload = bincode::serialize(&snapshot).map_err(|_| RpcError::Persistence)?;
        let envelope = PersistedEnvelope {
            magic: STATE_MAGIC,
            version: STATE_FORMAT_VERSION,
            checksum: *blake3::hash(&payload).as_bytes(),
            payload,
        };
        let encoded = bincode::serialize(&envelope).map_err(|_| RpcError::Persistence)?;
        if encoded.len() as u64 > MAX_STATE_FILE_BYTES {
            return Err(RpcError::Persistence);
        }
        let mut temp = NamedTempFile::new_in(parent).map_err(|_| RpcError::Persistence)?;
        temp.write_all(&encoded).map_err(|_| RpcError::Persistence)?;
        temp.as_file().sync_all().map_err(|_| RpcError::Persistence)?;
        temp.persist(path).map_err(|_| RpcError::Persistence)?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| RpcError::Persistence)?;
        Ok(())
    }

}

fn receipt_root(tx_hash: Hash, height: u64, gas_used: u64) -> Hash {
    Hash::from_data(
        &bincode::serialize(&(tx_hash, height, gas_used, ReceiptStatus::Success as u8))
            .expect("receipt root serialization cannot fail"),
    )
}

fn validate_chain(
    blocks: &[Block],
    receipts: &[Receipt],
    chain_id: u64,
    state: &State,
) -> anyhow::Result<()> {
    let mut parent = Hash::ZERO;
    let mut receipt_blocks = HashSet::with_capacity(receipts.len());
    for (index, block) in blocks.iter().enumerate() {
        let expected_height = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("block height overflow"))?;
        if block.header.height != expected_height
            || block.header.parent_hash != parent
            || block.header.chain_id != chain_id
            || block.transactions.len() != 1
            || block.header.gas_used > block.header.gas_limit
        {
            anyhow::bail!("invalid persisted block sequence");
        }
        let tx_hash = Mempool::transaction_hash(&block.transactions[0]);
        let tx_root = Hash::from_data(
            &bincode::serialize(&vec![*tx_hash.as_bytes()])
                .map_err(|_| anyhow::anyhow!("cannot serialize transaction root"))?,
        );
        if block.header.tx_root != tx_root
            || block.transactions[0].chain_id != chain_id
            || block.header.gas_used != block.transactions[0].gas_limit
            || block.header.receipt_root
                != receipt_root(tx_hash, block.header.height, block.header.gas_used)
        {
            anyhow::bail!("invalid persisted block commitment");
        }
        parent = block.hash();
    }
    if blocks.len() != receipts.len() {
        anyhow::bail!("every persisted block must have one receipt");
    }
    for receipt in receipts {
        if !receipt_blocks.insert(receipt.block_hash) {
            anyhow::bail!("multiple receipts reference the same block");
        }
        let Some(block) = blocks.iter().find(|block| block.hash() == receipt.block_hash) else {
            anyhow::bail!("receipt references an unknown block");
        };
        if receipt.block_height != block.header.height
            || !block
                .transactions
                .iter()
                .any(|tx| Mempool::transaction_hash(tx) == receipt.tx_hash)
            || receipt.status != ReceiptStatus::Success
            || receipt.gas_used != block.header.gas_used
            || receipt_root(receipt.tx_hash, receipt.block_height, receipt.gas_used)
                != block.header.receipt_root
        {
            anyhow::bail!("invalid persisted receipt");
        }
    }
    if receipt_blocks.len() != blocks.len() {
        anyhow::bail!("a persisted block is missing its receipt");
    }
    if let Some(last_block) = blocks.last() {
        let state_root = Hash::from_data(
            &bincode::serialize(&state.snapshot())
                .map_err(|_| anyhow::anyhow!("cannot serialize state root"))?,
        );
        if state_root != last_block.header.state_root {
            anyhow::bail!("persisted state does not match the latest block");
        }
    }
    Ok(())
}

fn is_hex_address(s: &str) -> bool {
    s.starts_with("0x") && s.len() == 42 && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn is_hex_tx_hash(s: &str) -> bool {
    s.starts_with("0x") && s.len() == 66 && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_hash(s: &str) -> Result<Hash, RpcError> {
    if !is_hex_tx_hash(s) {
        return Err(RpcError::InvalidTxHash);
    }
    let bytes = subhost_core::hex::decode(&s[2..]).map_err(|_| RpcError::InvalidTxHash)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| RpcError::InvalidTxHash)?;
    Ok(Hash::new(bytes))
}

fn block_index(tag: &str, block_count: usize) -> Result<Option<usize>, RpcError> {
    if block_count == 0 {
        return Ok(None);
    }
    match tag {
        "latest" | "pending" => Ok(Some(block_count - 1)),
        "earliest" => Ok(Some(0)),
        value => {
            let height = value
                .strip_prefix("0x")
                .filter(|digits| !digits.is_empty())
                .and_then(|digits| u64::from_str_radix(digits, 16).ok())
                .ok_or(RpcError::InvalidParams)?;
            if height == 0 {
                return Ok(None);
            }
            let index = usize::try_from(height - 1).map_err(|_| RpcError::BlockNotFound)?;
            Ok((index < block_count).then_some(index))
        }
    }
}

fn receipt_json(receipt: &Receipt) -> serde_json::Value {
    serde_json::json!({
        "transactionHash": format!("0x{}", subhost_core::hex::encode(receipt.tx_hash.as_bytes())),
        "blockHash": format!("0x{}", subhost_core::hex::encode(receipt.block_hash.as_bytes())),
        "blockNumber": format!("0x{:x}", receipt.block_height),
        "transactionIndex": "0x0",
        "cumulativeGasUsed": format!("0x{:x}", receipt.gas_used),
        "gasUsed": format!("0x{:x}", receipt.gas_used),
        "status": format!("0x{:x}", receipt.status as u8),
        "contractAddress": receipt.contract_address.map(|address| address.to_string()),
        "logs": receipt.logs.iter().map(|log| serde_json::json!({
            "address": log.address.to_string(),
            "topics": log.topics.iter().map(|topic| topic.to_string()).collect::<Vec<_>>(),
            "data": format!("0x{}", subhost_core::hex::encode(&log.data)),
        })).collect::<Vec<_>>(),
    })
}

fn block_json(block: &Block, full_transactions: bool) -> serde_json::Value {
    let transactions = if full_transactions {
        block
            .transactions
            .iter()
            .map(|tx| serde_json::to_value(tx).unwrap_or(serde_json::Value::Null))
            .collect::<Vec<_>>()
    } else {
        block
            .transactions
            .iter()
            .map(|tx| {
                format!(
                    "0x{}",
                    subhost_core::hex::encode(Mempool::transaction_hash(tx).as_bytes())
                )
            })
            .map(serde_json::Value::String)
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "number": format!("0x{:x}", block.header.height),
        "hash": format!("0x{}", subhost_core::hex::encode(block.hash().as_bytes())),
        "parentHash": block.header.parent_hash.to_string(),
        "timestamp": format!("0x{:x}", block.header.timestamp),
        "gasLimit": format!("0x{:x}", block.header.gas_limit),
        "gasUsed": format!("0x{:x}", block.header.gas_used),
        "transactions": transactions,
    })
}

fn parse_address(s: &str) -> Result<Address, RpcError> {
    if !is_hex_address(s) {
        return Err(RpcError::InvalidAddress);
    }
    let bytes = subhost_core::hex::decode(&s[2..]).map_err(|_| RpcError::InvalidAddress)?;
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(Address::new(arr))
}

/// Parse an Ethereum QUANTITY (hex string or number) into a u64.
fn hex_quantity(v: Option<&serde_json::Value>, default: u64) -> Result<u64, RpcError> {
    let Some(value) = v else {
        return Ok(default);
    };
    let serde_json::Value::String(s) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = s.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    u64::from_str_radix(digits, 16).map_err(|_| RpcError::InvalidParams)
}

fn hex_amount(v: Option<&serde_json::Value>, default: u128) -> Result<u128, RpcError> {
    let Some(value) = v else {
        return Ok(default);
    };
    let serde_json::Value::String(s) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = s.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    u128::from_str_radix(digits, 16).map_err(|_| RpcError::InvalidParams)
}

fn fixed_hex<const N: usize>(value: Option<&serde_json::Value>) -> Result<[u8; N], RpcError> {
    let value = value
        .and_then(serde_json::Value::as_str)
        .ok_or(RpcError::InvalidSignature)?;
    if !value.starts_with("0x") {
        return Err(RpcError::InvalidSignature);
    }
    let bytes = hex_bytes(value).map_err(|_| RpcError::InvalidSignature)?;
    bytes.try_into().map_err(|_| RpcError::InvalidSignature)
}

fn verify_transaction_signature(
    tx: &Transaction,
    public_key: &[u8; 32],
    signature: &[u8; 64],
) -> Result<(), RpcError> {
    let verifying_key = VerifyingKey::from_bytes(public_key).map_err(|_| RpcError::InvalidSignature)?;
    if Address::from_public_key(public_key) != tx.from {
        return Err(RpcError::InvalidSignature);
    }
    let unsigned_tx = Transaction {
        signature: TransactionSignature {
            r: [0; 32],
            s: [0; 32],
            v: 0,
        },
        ..tx.clone()
    };
    let encoded = bincode::serialize(&unsigned_tx).map_err(|_| RpcError::InternalError)?;
    verifying_key
        .verify(&encoded, &Ed25519Signature::from_bytes(signature))
        .map_err(|_| RpcError::InvalidSignature)
}

fn hex_bytes(s: &str) -> Result<Vec<u8>, RpcError> {
    subhost_core::hex::decode(s).map_err(|_| RpcError::InvalidParams)
}

#[derive(Clone)]
pub struct RpcServer {
    state: Arc<RwLock<RpcState>>,
}

impl RpcServer {
    pub fn new(state: RpcState) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }
    
    pub async fn run(self, addr: SocketAddr) -> anyhow::Result<()> {
        self.run_with_limits(addr, 1000).await
    }

    pub async fn run_with_limits(self, addr: SocketAddr, max_connections: usize) -> anyhow::Result<()> {
        let (_, handle) = self.start_with_limits(addr, max_connections).await?;
        handle.stopped().await;
        Ok(())
    }

    pub async fn start_with_limits(
        self,
        addr: SocketAddr,
        max_connections: usize,
    ) -> anyhow::Result<(SocketAddr, ServerHandle)> {
        let max_connections = u32::try_from(max_connections).unwrap_or(u32::MAX).max(1);
        let server = Server::builder()
            .max_connections(max_connections)
            .max_request_body_size(256 * 1024)
            .max_response_body_size(1024 * 1024)
            .set_batch_request_config(BatchRequestConfig::Limit(10))
            .build(addr)
            .await?;
        let mut module = RpcModule::new(self.state.clone());
        
        module.register_method("eth_chainId", |_params, ctx| {
            // std::sync::RwLock: safe to take briefly inside a sync handler on the
            // async runtime (no tokio Blocking* API that panics on a worker thread).
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(format!("0x{:x}", state.chain_id))
        })?;
        
        module.register_method("eth_blockNumber", |_params, ctx| {
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(format!("0x{:x}", state.block_height()))
        })?;
        
        module.register_method("eth_getBalance", |params, ctx| {
            // Accept both `[address]` and the spec-compliant `[address, block]`.
            let mut seq = params.sequence();
            let address: String = seq.next().map_err(|_| RpcError::InvalidParams)?;
            let addr = parse_address(&address)?;
            // Read the real world-state balance (no more fabricated 0x0).
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            let state_guard = state.state.read().unwrap_or_else(|e| e.into_inner());
            let balance = state_guard.balance(&addr);
            Ok::<_, RpcError>(format!("0x{:x}", balance))
        })?;
        
        module.register_method("eth_sendTransaction", |params, ctx| {
            let tx: serde_json::Value = params.one().map_err(|_| RpcError::InvalidParams)?;

            let from = tx
                .get("from").and_then(|v| v.as_str())
                .ok_or(RpcError::InvalidAddress)
                .and_then(parse_address)?;
            let to = match tx.get("to").and_then(|v| v.as_str()) {
                Some(t) => Some(parse_address(t)?),
                None => None,
            };
            let value = hex_amount(tx.get("value"), 0)?;
            let nonce = hex_quantity(tx.get("nonce"), 0)?;
            let gas_price = hex_amount(tx.get("gasPrice"), 1)?;
            let gas_limit = hex_quantity(tx.get("gasLimit"), 21_000)?;
            let configured_chain_id = ctx
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .chain_id;
            let chain_id = match tx.get("chainId") {
                Some(value) => hex_quantity(Some(value), configured_chain_id)?,
                None => configured_chain_id,
            };
            if chain_id != configured_chain_id {
                return Err(RpcError::InvalidChainId);
            }
            let data = match tx.get("data").and_then(|v| v.as_str()) {
                Some(d) => hex_bytes(d)?,
                None => Vec::new(),
            };

            let tx_type = if to.is_none() {
                TransactionType::ContractCreation
            } else {
                TransactionType::Transfer
            };

            let public_key = fixed_hex::<32>(tx.get("publicKey"))?;
            let signature = fixed_hex::<64>(tx.get("signature"))?;
            let unsigned_tx = Transaction {
                tx_type,
                nonce,
                from,
                to,
                value,
                gas_price,
                gas_limit,
                data,
                chain_id,
                signature: TransactionSignature { r: [0u8; 32], s: [0u8; 32], v: 0 },
            };

            verify_transaction_signature(&unsigned_tx, &public_key, &signature)?;
            let signature = Ed25519Signature::from_bytes(&signature);

            let tx = Transaction {
                signature: TransactionSignature {
                    r: signature.to_bytes()[..32].try_into().expect("fixed signature half"),
                    s: signature.to_bytes()[32..].try_into().expect("fixed signature half"),
                    v: 0,
                },
                ..unsigned_tx
            };

            // Insert into the pending pool, then execute and persist it as the
            // next local block. A failed execution is removed again so invalid
            // transactions cannot remain stuck in the pool.
            let rpc_state = ctx.read().unwrap_or_else(|e| e.into_inner()).clone();
            let hash = rpc_state.submit_transaction(tx)?;

            Ok::<_, RpcError>(format!("0x{}", hex::encode(hash.as_bytes())))
        })?;
        
        module.register_method("eth_getTransactionReceipt", |params, ctx| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            if !is_hex_tx_hash(&tx_hash) {
                return Err(RpcError::InvalidTxHash);
            }
            let tx_hash = parse_hash(&tx_hash)?;
            let rpc_state = ctx.read().unwrap_or_else(|e| e.into_inner());
            let Some(receipt) = rpc_state.receipt(&tx_hash) else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            Ok::<_, RpcError>(receipt_json(&receipt))
        })?;

        module.register_method("eth_getBlockByNumber", |params, ctx| {
            let (tag, full_transactions): (String, bool) =
                params.parse().map_err(|_| RpcError::InvalidParams)?;
            let rpc_state = ctx.read().unwrap_or_else(|e| e.into_inner());
            let blocks = rpc_state
                .blocks
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let Some(index) = block_index(&tag, blocks.len())? else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            Ok::<_, RpcError>(block_json(&blocks[index], full_transactions))
        })?;
        
        module.register_method("eth_gasPrice", |_params, _ctx| {
            Ok::<_, RpcError>("0x1".to_string())
        })?;
        
        module.register_method("net_version", |_params, ctx| {
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(state.chain_id.to_string())
        })?;
        
        let addr = server.local_addr()?;
        let handle = server.start(module);
        
        info!("RPC server started at {}", addr);
        Ok((addr, handle))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostrpcConfig {
    pub enabled: bool,
    pub max_connections: usize,
    pub listen_addr: String,
}

impl Default for SubhostrpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
            listen_addr: "127.0.0.1:8545".to_string(),
        }
    }
}

pub struct SubhostrpcModule {
    config: SubhostrpcConfig,
}

impl SubhostrpcModule {
    pub fn new(config: SubhostrpcConfig) -> Self {
        info!("Initializing SubhostrpcModule");
        Self { config }
    }
    
    pub async fn start(&self) -> anyhow::Result<()> {
        if self.config.enabled {
            let server = RpcServer::new(RpcState::new(1));
            let addr: SocketAddr = self.config.listen_addr.parse()?;
            server.run_with_limits(addr, self.config.max_connections).await?;
        }
        Ok(())
    }
    
    pub fn process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.config.enabled {
            return Ok(());
        }
        debug!("Processing request in subhost-rpc");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostrpcError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Processing error: {0}")]
    Processing(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use subhost_core::TransactionType;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tempfile::tempdir;

    fn transaction(sender: u8, nonce: u64, value: u128) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::new([sender; 20]),
            to: Some(Address::new([2; 20])),
            value,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature {
                r: [0; 32],
                s: [0; 32],
                v: 0,
            },
        }
    }

    fn submit(state: &RpcState, tx: Transaction) -> (Hash, Receipt) {
        let hash = Mempool::transaction_hash(&tx);
        state
            .mempool
            .write()
            .unwrap()
            .add(tx)
            .expect("test transaction should enter pool");
        let receipt = state
            .include_transaction(&hash)
            .expect("test transaction should execute");
        (hash, receipt)
    }
    
    #[test]
    fn test_default_config() {
        let config = SubhostrpcConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
        assert_eq!(config.listen_addr, "127.0.0.1:8545");
    }

    #[test]
    fn quantity_and_signature_parsers_are_strict() {
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x1f")), 0).unwrap(), 31);
        assert!(hex_quantity(Some(&serde_json::json!("1f")), 0).is_err());
        assert!(hex_quantity(Some(&serde_json::json!("0x01")), 0).is_err());
        assert!(hex_amount(Some(&serde_json::json!("0x100000000000000000000")), 0).is_ok());
        assert!(fixed_hex::<32>(Some(&serde_json::json!("00".repeat(32)))).is_err());
        assert!(fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(32))))).is_ok());
    }

    #[test]
    fn producer_executes_transfer_and_records_receipt() {
        let state = RpcState::new(1);
        state
            .state
            .write()
            .unwrap()
            .add_account(Address::new([1; 20]), 100_000)
            .unwrap();
        let (hash, receipt) = submit(&state, transaction(1, 0, 10));

        assert_eq!(receipt.tx_hash, hash);
        assert_eq!(receipt.block_height, 1);
        assert_eq!(state.block_height(), 1);
        assert_eq!(state.state.read().unwrap().balance(&Address::new([2; 20])), 10);
        assert_eq!(state.state.read().unwrap().nonce(&Address::new([1; 20])), 1);
        assert!(state.mempool.read().unwrap().is_empty());
        assert!(state.receipt(&hash).is_some());
    }

    #[test]
    fn producer_rejects_insufficient_balance_without_mutating_state() {
        let state = RpcState::new(1);
        let tx = transaction(1, 0, 10);
        let hash = Mempool::transaction_hash(&tx);
        state.mempool.write().unwrap().add(tx).unwrap();

        assert!(matches!(
            state.include_transaction(&hash),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.state.read().unwrap().account_count(), 0);
        assert!(state.receipt(&hash).is_none());
    }

    #[test]
    fn persisted_ledger_restores_blocks_receipts_and_state() {
        let dir = tempdir().unwrap();
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state
            .state
            .write()
            .unwrap()
            .add_account(Address::new([1; 20]), 100_000)
            .unwrap();
        let (hash, _) = submit(&state, transaction(1, 0, 10));
        drop(state);

        let restored = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert_eq!(restored.block_height(), 1);
        assert!(restored.receipt(&hash).is_some());
        assert_eq!(restored.state.read().unwrap().balance(&Address::new([2; 20])), 10);
        assert_eq!(restored.blocks.read().unwrap().len(), 1);
    }

    #[test]
    fn persisted_genesis_state_is_not_reported_as_fresh() {
        let dir = tempdir().unwrap();
        let address = Address::new([1; 20]);
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert!(!state.has_persisted_state());
        state.seed_accounts([(address, 42)]).unwrap();
        assert!(state.has_persisted_state());
        drop(state);

        let restored = RpcState::with_data_dir(1, dir.path()).unwrap();
        assert_eq!(restored.block_height(), 0);
        assert!(restored.has_persisted_state());
        assert_eq!(restored.state.read().unwrap().balance(&address), 42);
    }

    #[test]
    fn corrupted_ledger_is_rejected_on_startup() {
        let dir = tempdir().unwrap();
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        state.seed_account(Address::new([1; 20]), 100_000).unwrap();
        submit(&state, transaction(1, 0, 10));
        drop(state);

        let path = dir.path().join("node-state.bin");
        let mut bytes = fs::read(&path).unwrap();
        let index = bytes.len() - 1;
        bytes[index] ^= 0x01;
        fs::write(path, bytes).unwrap();
        assert!(RpcState::with_data_dir(1, dir.path()).is_err());
    }

    #[test]
    fn gas_above_block_limit_is_rejected_without_commit() {
        let state = RpcState::new(1);
        state.seed_account(Address::new([1; 20]), 100_000_000).unwrap();
        let mut tx = transaction(1, 0, 10);
        tx.gas_limit = 30_000_001;
        assert!(matches!(
            state.submit_transaction(tx),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert!(state.mempool.read().unwrap().is_empty());
    }

    #[test]
    fn failed_replacement_restores_previous_pending_transaction() {
        let state = RpcState::new(1);
        let sender = Address::new([1; 20]);
        let mut original = transaction(1, 0, 10);
        original.gas_price = 1;
        state.mempool.write().unwrap().add(original.clone()).unwrap();

        let mut replacement = original.clone();
        replacement.gas_price = 2;
        let error = state.submit_transaction(replacement).unwrap_err();
        assert!(matches!(error, RpcError::TransactionRejected(_)));

        let pool = state.mempool.read().unwrap();
        let (_, restored) = pool.get_by_sender_nonce(&sender, 0).unwrap();
        assert_eq!(restored.gas_price, 1);
    }

    #[test]
    fn signature_verification_binds_payload_and_sender() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        let mut tx = transaction(1, 0, 10);
        tx.from = Address::from_public_key(&public_key);
        let encoded = bincode::serialize(&tx).unwrap();
        let signature = signing_key.sign(&encoded).to_bytes();
        assert!(verify_transaction_signature(&tx, &public_key, &signature).is_ok());

        tx.value = 11;
        assert!(matches!(
            verify_transaction_signature(&tx, &public_key, &signature),
            Err(RpcError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn rpc_http_round_trip_returns_receipt_and_block() {
        let state = RpcState::new(1);
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        let from = Address::from_public_key(&public_key);
        state.seed_account(from, 100_000).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (_, handle) = RpcServer::new(state.clone())
            .start_with_limits(address, 16)
            .await
            .unwrap();

        let transaction = Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from,
            to: Some(Address::new([2; 20])),
            value: 10,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature {
                r: [0; 32],
                s: [0; 32],
                v: 0,
            },
        };
        let signature = signing_key
            .sign(&bincode::serialize(&transaction).unwrap())
            .to_bytes();
        let payload = serde_json::json!({
            "from": from.to_string(),
            "to": "0x0202020202020202020202020202020202020202",
            "value": "0xa",
            "nonce": "0x0",
            "gasPrice": "0x1",
            "gasLimit": "0x5208",
            "chainId": "0x1",
            "data": "0x",
            "publicKey": format!("0x{}", subhost_core::hex::encode(public_key)),
            "signature": format!("0x{}", subhost_core::hex::encode(signature)),
        });
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_sendTransaction",
            "params": [payload],
            "id": 1,
        })
        .to_string();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request_bytes = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            address,
            request.len(),
            request
        );
        stream.write_all(request_bytes.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("\"result\":\"0x"));
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let hash: String = serde_json::from_str::<serde_json::Value>(body)
            .unwrap()["result"]
            .as_str()
            .unwrap()
            .to_string();

        let receipt_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionReceipt",
            "params": [hash],
            "id": 2,
        })
        .to_string();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request_bytes = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            address,
            receipt_request.len(),
            receipt_request
        );
        stream.write_all(request_bytes.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("\"blockNumber\":\"0x1\""));

        let block_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": ["latest", false],
            "id": 3,
        })
        .to_string();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request_bytes = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            address,
            block_request.len(),
            block_request
        );
        stream.write_all(request_bytes.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("\"number\":\"0x1\""));
        assert!(response.contains("\"transactions\":[\"0x"));
        handle.stop().unwrap();
        handle.stopped().await;
    }
}
