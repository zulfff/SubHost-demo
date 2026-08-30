//! Crash-safe ledger persistence.
//!
//! The on-disk format is a versioned envelope: `magic || version || checksum ||
//! payload`. Every write goes to a temporary file in the same directory, is
//! fsynced, atomically renamed over the target, and the directory itself is
//! fsynced, so an interrupted write can never leave a half-written ledger.
//!
//! Every read is validated before it is trusted: size bound, magic, version,
//! BLAKE3 checksum, chain binding, then a full replay of the block/receipt
//! commitments against the restored state. A tampered or truncated file is
//! rejected rather than silently loaded.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use subhost_core::{tx_root_of, Block, ChainId, Hash, Receipt, ReceiptStatus};
use subhost_state::{State, StateSnapshot};
use tracing::debug;

/// Refuse to read or write a ledger larger than this, so a corrupt length field
/// cannot make the node allocate unbounded memory.
pub const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;
/// File name used inside the configured data directory.
pub const LEDGER_FILE_NAME: &str = "node-state.bin";

const LEDGER_MAGIC: [u8; 8] = *b"SUBHOST1";
const LEDGER_FORMAT_VERSION: u32 = 1;

/// The full persisted ledger: account state plus the local block history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub chain_id: ChainId,
    pub state: StateSnapshot,
    pub blocks: Vec<Block>,
    pub receipts: Vec<Receipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerEnvelope {
    magic: [u8; 8],
    version: u32,
    checksum: [u8; 32],
    payload: Vec<u8>,
}

/// What a node recovered from disk.
#[derive(Debug)]
pub struct RestoredLedger {
    pub state: State,
    pub blocks: Vec<Block>,
    pub receipts: Vec<Receipt>,
}

impl RestoredLedger {
    /// Height of the newest block, or 0 for a ledger with no blocks.
    pub fn height(&self) -> u64 {
        self.blocks.last().map_or(0, |block| block.header.height)
    }
}

/// A single-file ledger store bound to one chain ID.
///
/// [`LedgerStore::ephemeral`] yields a store that validates exactly like the
/// durable one but discards writes, which keeps in-memory and on-disk nodes on
/// the same code path.
#[derive(Debug, Clone)]
pub struct LedgerStore {
    path: Option<PathBuf>,
    chain_id: ChainId,
}

impl LedgerStore {
    /// Open (or prepare to create) `<data_dir>/node-state.bin`.
    pub fn open(chain_id: ChainId, data_dir: impl AsRef<Path>) -> Result<Self, StorageError> {
        if chain_id == 0 {
            return Err(StorageError::InvalidChainId);
        }
        let data_dir = data_dir.as_ref();
        fs::create_dir_all(data_dir)
            .map_err(|source| StorageError::Io { path: data_dir.to_path_buf(), source })?;
        Ok(Self { path: Some(data_dir.join(LEDGER_FILE_NAME)), chain_id })
    }

    /// A store that performs every validation but never touches the filesystem.
    pub fn ephemeral(chain_id: ChainId) -> Result<Self, StorageError> {
        if chain_id == 0 {
            return Err(StorageError::InvalidChainId);
        }
        Ok(Self { path: None, chain_id })
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether a ledger file already exists. Used to tell a fresh node from one
    /// that has already applied genesis allocations.
    pub fn exists(&self) -> bool {
        self.path.as_ref().is_some_and(|path| path.is_file())
    }

    /// Load and fully validate the ledger, or return an empty one.
    pub fn load(&self) -> Result<RestoredLedger, StorageError> {
        let Some(path) = &self.path else {
            return Ok(self.empty());
        };
        if !path.is_file() {
            return Ok(self.empty());
        }

        let metadata =
            fs::metadata(path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
        if metadata.len() > MAX_LEDGER_BYTES {
            return Err(StorageError::TooLarge { size: metadata.len(), max: MAX_LEDGER_BYTES });
        }

        let encoded =
            fs::read(path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
        let envelope: LedgerEnvelope = bincode::deserialize(&encoded)
            .map_err(|error| StorageError::Decode(error.to_string()))?;
        if envelope.magic != LEDGER_MAGIC {
            return Err(StorageError::UnsupportedFormat);
        }
        if envelope.version != LEDGER_FORMAT_VERSION {
            return Err(StorageError::UnsupportedVersion(envelope.version));
        }
        if *blake3::hash(&envelope.payload).as_bytes() != envelope.checksum {
            return Err(StorageError::ChecksumMismatch);
        }

        let snapshot: LedgerSnapshot = bincode::deserialize(&envelope.payload)
            .map_err(|error| StorageError::Decode(error.to_string()))?;
        self.restore(snapshot)
    }

    /// Validate a decoded snapshot and rebuild the in-memory ledger from it.
    pub fn restore(&self, snapshot: LedgerSnapshot) -> Result<RestoredLedger, StorageError> {
        if snapshot.chain_id != self.chain_id || snapshot.state.chain_id != self.chain_id {
            return Err(StorageError::ChainMismatch {
                expected: self.chain_id,
                got: snapshot.chain_id,
            });
        }
        let state = State::from_snapshot(snapshot.state)
            .map_err(|error| StorageError::Invalid(error.to_string()))?;
        validate_chain(&snapshot.blocks, &snapshot.receipts, self.chain_id, &state)?;
        debug!(
            blocks = snapshot.blocks.len(),
            receipts = snapshot.receipts.len(),
            "restored ledger"
        );
        Ok(RestoredLedger { state, blocks: snapshot.blocks, receipts: snapshot.receipts })
    }

    /// Atomically persist a snapshot, validating it first so a rejected ledger
    /// can never be written and then fail to load.
    pub fn persist(&self, snapshot: &LedgerSnapshot) -> Result<(), StorageError> {
        if snapshot.chain_id != self.chain_id || snapshot.state.chain_id != self.chain_id {
            return Err(StorageError::ChainMismatch {
                expected: self.chain_id,
                got: snapshot.chain_id,
            });
        }

        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        fs::create_dir_all(parent)
            .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;

        let payload = bincode::serialize(snapshot)
            .map_err(|error| StorageError::Encode(error.to_string()))?;
        let envelope = LedgerEnvelope {
            magic: LEDGER_MAGIC,
            version: LEDGER_FORMAT_VERSION,
            checksum: *blake3::hash(&payload).as_bytes(),
            payload,
        };
        let encoded = bincode::serialize(&envelope)
            .map_err(|error| StorageError::Encode(error.to_string()))?;
        if encoded.len() as u64 > MAX_LEDGER_BYTES {
            return Err(StorageError::TooLarge {
                size: encoded.len() as u64,
                max: MAX_LEDGER_BYTES,
            });
        }

        // Write to a sibling temporary file, fsync it, rename over the target,
        // then fsync the directory so the rename itself is durable.
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
        temp.write_all(&encoded)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        temp.as_file()
            .sync_all()
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        }
        temp.persist(path)
            .map_err(|error| StorageError::Io { path: path.clone(), source: error.error })?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
        Ok(())
    }

    fn empty(&self) -> RestoredLedger {
        RestoredLedger {
            state: State::with_chain_id(self.chain_id),
            blocks: Vec::new(),
            receipts: Vec::new(),
        }
    }
}

/// Recompute the commitment for a single-transaction block receipt.
///
/// The producer and the validator must agree byte-for-byte, so this lives beside
/// the validator rather than in the RPC crate.
pub fn receipt_root(tx_hash: Hash, height: u64, gas_used: u64) -> Hash {
    Hash::from_data(&subhost_core::encode_canonical(&(
        tx_hash,
        height,
        gas_used,
        ReceiptStatus::Success as u8,
    )))
}

/// Replay every persisted block and receipt against the restored state.
///
/// This is the integrity gate for the whole ledger: heights are contiguous from
/// 1, parents chain, every commitment recomputes, receipts map one-to-one onto
/// blocks, and the final state root matches the newest header.
pub fn validate_chain(
    blocks: &[Block],
    receipts: &[Receipt],
    chain_id: ChainId,
    state: &State,
) -> Result<(), StorageError> {
    let mut parent = Hash::ZERO;
    for (index, block) in blocks.iter().enumerate() {
        let expected_height = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| StorageError::Invalid("block height overflow".into()))?;
        if block.header.height != expected_height
            || block.header.parent_hash != parent
            || block.header.chain_id != chain_id
            || block.transactions.len() != 1
            || block.header.gas_used > block.header.gas_limit
        {
            return Err(StorageError::Invalid(format!(
                "invalid block sequence at height {expected_height}"
            )));
        }

        let tx = &block.transactions[0];
        let tx_hash = tx.hash();
        if block.header.tx_root != tx_root_of(&[tx_hash])
            || tx.chain_id != chain_id
            || block.header.gas_used != tx.gas_limit
            || block.header.receipt_root
                != receipt_root(tx_hash, block.header.height, block.header.gas_used)
        {
            return Err(StorageError::Invalid(format!(
                "invalid block commitment at height {expected_height}"
            )));
        }
        parent = block.hash();
    }

    if blocks.len() != receipts.len() {
        return Err(StorageError::Invalid("every block must have exactly one receipt".into()));
    }

    let mut receipt_blocks = HashSet::with_capacity(receipts.len());
    for receipt in receipts {
        if !receipt_blocks.insert(receipt.block_hash) {
            return Err(StorageError::Invalid("multiple receipts reference the same block".into()));
        }
        let Some(block) = blocks.iter().find(|block| block.hash() == receipt.block_hash) else {
            return Err(StorageError::Invalid("receipt references an unknown block".into()));
        };
        if receipt.block_height != block.header.height
            || !block.transactions.iter().any(|tx| tx.hash() == receipt.tx_hash)
            || receipt.status != ReceiptStatus::Success
            || receipt.gas_used != block.header.gas_used
            || receipt_root(receipt.tx_hash, receipt.block_height, receipt.gas_used)
                != block.header.receipt_root
        {
            return Err(StorageError::Invalid(format!(
                "invalid receipt for block {}",
                receipt.block_height
            )));
        }
    }

    if let Some(last_block) = blocks.last() {
        if state.root() != last_block.header.state_root {
            return Err(StorageError::Invalid(
                "state does not match the newest block state root".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("chain ID cannot be zero")]
    InvalidChainId,

    #[error("ledger chain ID {got} does not match configured chain ID {expected}")]
    ChainMismatch { expected: ChainId, got: ChainId },

    #[error("ledger is {size} bytes, above the {max} byte limit")]
    TooLarge { size: u64, max: u64 },

    #[error("unsupported ledger format")]
    UnsupportedFormat,

    #[error("unsupported ledger format version: {0}")]
    UnsupportedVersion(u32),

    #[error("ledger checksum mismatch")]
    ChecksumMismatch,

    #[error("ledger is invalid: {0}")]
    Invalid(String),

    #[error("cannot decode ledger: {0}")]
    Decode(String),

    #[error("cannot encode ledger: {0}")]
    Encode(String),

    #[error("ledger path has no parent directory")]
    MissingParentDirectory,

    #[error("ledger I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_core::{
        Address, BlockHeader, Transaction, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT,
    };

    fn transfer(nonce: u64) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::new([1; 20]),
            to: Some(Address::new([2; 20])),
            value: 10,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature::EMPTY,
        }
    }

    /// Build a one-transaction-per-block ledger the validator must accept.
    fn ledger(block_count: u64) -> (LedgerSnapshot, State) {
        let mut state = State::with_chain_id(1);
        state.credit(Address::new([1; 20]), 10_000_000).unwrap();
        let mut blocks = Vec::new();
        let mut receipts = Vec::new();
        let mut parent = Hash::ZERO;

        for height in 1..=block_count {
            let tx = transfer(height - 1);
            state.apply_transaction(&tx).unwrap();
            let tx_hash = tx.hash();
            let gas_used = tx.gas_limit;
            let header = BlockHeader {
                version: 1,
                chain_id: 1,
                height,
                timestamp: 1_700_000_000 + height,
                parent_hash: parent,
                state_root: state.root(),
                tx_root: tx_root_of(&[tx_hash]),
                receipt_root: receipt_root(tx_hash, height, gas_used),
                validator: Address::ZERO,
                gas_used,
                gas_limit: BLOCK_GAS_LIMIT,
                extra_data: Vec::new(),
            };
            let block = Block { header, transactions: vec![tx], signatures: Vec::new() };
            receipts.push(Receipt {
                tx_hash,
                block_hash: block.hash(),
                block_height: height,
                gas_used,
                status: ReceiptStatus::Success,
                logs: Vec::new(),
                contract_address: None,
            });
            parent = block.hash();
            blocks.push(block);
        }

        (LedgerSnapshot { chain_id: 1, state: state.snapshot(), blocks, receipts }, state)
    }

    #[test]
    fn round_trip_restores_state_blocks_and_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::open(1, dir.path()).unwrap();
        assert!(!store.exists());
        assert_eq!(store.load().unwrap().height(), 0);

        let (snapshot, state) = ledger(3);
        store.persist(&snapshot).unwrap();
        assert!(store.exists());

        let restored = store.load().unwrap();
        assert_eq!(restored.height(), 3);
        assert_eq!(restored.blocks.len(), 3);
        assert_eq!(restored.receipts.len(), 3);
        assert_eq!(restored.state.root(), state.root());
        assert_eq!(restored.state.balance(&Address::new([2; 20])), 30);
    }

    #[test]
    fn ephemeral_store_validates_but_never_writes() {
        let store = LedgerStore::ephemeral(1).unwrap();
        let (snapshot, _) = ledger(1);
        store.persist(&snapshot).unwrap();
        assert!(!store.exists());
        assert!(store.path().is_none());
        assert_eq!(store.load().unwrap().height(), 0);

        // Validation still applies to an ephemeral store.
        let mut foreign = snapshot;
        foreign.chain_id = 9;
        assert!(matches!(
            store.persist(&foreign),
            Err(StorageError::ChainMismatch { expected: 1, got: 9 })
        ));
    }

    #[test]
    fn zero_chain_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(LedgerStore::open(0, dir.path()), Err(StorageError::InvalidChainId)));
        assert!(matches!(LedgerStore::ephemeral(0), Err(StorageError::InvalidChainId)));
    }

    #[test]
    fn bit_flip_anywhere_in_the_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::open(1, dir.path()).unwrap();
        let (snapshot, _) = ledger(2);
        store.persist(&snapshot).unwrap();
        let path = dir.path().join(LEDGER_FILE_NAME);
        let original = fs::read(&path).unwrap();

        // Flip the last payload byte and the checksum must catch it.
        let mut corrupted = original.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0x01;
        fs::write(&path, &corrupted).unwrap();
        assert!(store.load().is_err());

        // Corrupt the magic prefix.
        let mut bad_magic = original.clone();
        bad_magic[0] ^= 0xff;
        fs::write(&path, &bad_magic).unwrap();
        assert!(matches!(store.load(), Err(StorageError::UnsupportedFormat)));

        // Truncation must fail to decode rather than load a partial ledger.
        fs::write(&path, &original[..original.len() / 2]).unwrap();
        assert!(store.load().is_err());

        fs::write(&path, &original).unwrap();
        assert!(store.load().is_ok(), "the untouched file must still load");
    }

    #[test]
    fn ledger_from_another_chain_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        LedgerStore::open(1, dir.path()).unwrap().persist(&ledger(1).0).unwrap();
        let other_chain = LedgerStore::open(2, dir.path()).unwrap();
        assert!(matches!(
            other_chain.load(),
            Err(StorageError::ChainMismatch { expected: 2, got: 1 })
        ));
    }

    #[test]
    fn validator_rejects_tampered_history() {
        let (snapshot, state) = ledger(2);

        // Renumbered height breaks the contiguous sequence.
        let mut renumbered = snapshot.clone();
        renumbered.blocks[1].header.height = 5;
        assert!(validate_chain(&renumbered.blocks, &renumbered.receipts, 1, &state).is_err());

        // Rewritten parent breaks the chain link.
        let mut reparented = snapshot.clone();
        reparented.blocks[1].header.parent_hash = Hash::ZERO;
        assert!(validate_chain(&reparented.blocks, &reparented.receipts, 1, &state).is_err());

        // Swapped transaction no longer matches the committed tx_root.
        let mut retx = snapshot.clone();
        retx.blocks[1].transactions = vec![transfer(99)];
        assert!(validate_chain(&retx.blocks, &retx.receipts, 1, &state).is_err());

        // Gas above the block limit.
        let mut overspent = snapshot.clone();
        overspent.blocks[1].header.gas_used = BLOCK_GAS_LIMIT + 1;
        assert!(validate_chain(&overspent.blocks, &overspent.receipts, 1, &state).is_err());

        // Missing receipt.
        let mut short = snapshot.clone();
        short.receipts.pop();
        assert!(validate_chain(&short.blocks, &short.receipts, 1, &state).is_err());

        // Duplicate receipt for one block.
        let mut duplicated = snapshot.clone();
        duplicated.receipts[1] = duplicated.receipts[0].clone();
        assert!(validate_chain(&duplicated.blocks, &duplicated.receipts, 1, &state).is_err());

        // Receipt claiming a different gas figure.
        let mut regassed = snapshot.clone();
        regassed.receipts[1].gas_used += 1;
        assert!(validate_chain(&regassed.blocks, &regassed.receipts, 1, &state).is_err());

        // A failed receipt cannot appear in a committed block.
        let mut failed = snapshot.clone();
        failed.receipts[1].status = ReceiptStatus::Failure;
        assert!(validate_chain(&failed.blocks, &failed.receipts, 1, &state).is_err());

        // State that does not match the newest state root.
        let mut divergent = State::with_chain_id(1);
        divergent.credit(Address::new([7; 20]), 1).unwrap();
        assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &divergent).is_err());

        // The untouched ledger still validates.
        assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &state).is_ok());
    }

    #[test]
    fn empty_history_is_valid() {
        let state = State::with_chain_id(1);
        assert!(validate_chain(&[], &[], 1, &state).is_ok());
    }

    #[test]
    fn persist_overwrites_atomically_and_keeps_the_latest_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::open(1, dir.path()).unwrap();
        store.persist(&ledger(1).0).unwrap();
        store.persist(&ledger(3).0).unwrap();

        assert_eq!(store.load().unwrap().height(), 3);
        // No temporary files may be left behind.
        let files: Vec<_> =
            fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(files, vec![std::ffi::OsString::from(LEDGER_FILE_NAME)]);
    }

    #[test]
    fn oversized_ledger_is_refused_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::open(1, dir.path()).unwrap();
        let path = dir.path().join(LEDGER_FILE_NAME);
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_LEDGER_BYTES + 1).unwrap();
        assert!(matches!(store.load(), Err(StorageError::TooLarge { .. })));
    }
}
