//! Atomic pending snapshots, independent of committed ledger and voting state.

use crate::Mempool;
use bincode::Options;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use subhost_core::{ChainId, Hash, Transaction, TransactionType};

pub const MEMPOOL_FILE_NAME: &str = "mempool.bin";
pub const MAX_MEMPOOL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MEMPOOL_TRANSACTIONS: usize = 10_000;
pub const MAX_MEMPOOL_TRANSACTION_BYTES: u64 = 1024 * 1024;
const MAGIC: &[u8; 8] = b"SUBPOOL1";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 44;

/// Pending snapshot storage. The caller must serialize writes for one directory.
///
/// Load returns authenticated transactions, not a live pool. Reconcile the
/// committed ledger/journal, drop mined nonces and apply current admission
/// policy before installing them. Persistence is a candidate durability barrier;
/// post-commit cleanup failure must not invalidate an already committed block.
#[derive(Debug, Clone)]
pub struct MempoolStore {
    path: PathBuf,
    chain_id: ChainId,
    genesis_hash: Hash,
}

impl MempoolStore {
    /// Configure a store without creating or changing files.
    pub fn open(
        chain_id: ChainId,
        genesis_hash: Hash,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self, MempoolPersistenceError> {
        if chain_id == 0 {
            return Err(MempoolPersistenceError::InvalidChainId);
        }
        Ok(Self { path: data_dir.as_ref().join(MEMPOOL_FILE_NAME), chain_id, genesis_hash })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the complete validated snapshot, or an empty list if absent.
    pub fn load(&self) -> Result<Vec<Transaction>, MempoolPersistenceError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io(&self.path, source)),
        };
        let size = file.metadata().map_err(|source| self.io(&self.path, source))?.len();
        check_size(size, MAX_MEMPOOL_BYTES)?;
        let mut bytes = Vec::new();
        file.take(MAX_MEMPOOL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| self.io(&self.path, source))?;
        check_size(bytes.len() as u64, MAX_MEMPOOL_BYTES)?;
        if bytes.len() < HEADER_BYTES || &bytes[..8] != MAGIC {
            return Err(MempoolPersistenceError::Format("invalid snapshot header"));
        }
        let mut cursor = 8;
        let version = read_u32(&bytes, &mut cursor)?;
        if version != VERSION {
            return Err(MempoolPersistenceError::UnsupportedVersion(version));
        }
        let payload = &bytes[HEADER_BYTES..];
        if blake3::hash(payload).as_bytes() != &bytes[12..HEADER_BYTES] {
            return Err(MempoolPersistenceError::ChecksumMismatch);
        }
        cursor = 0;
        let chain_id = u64::from_le_bytes(
            take(payload, &mut cursor, 8)?
                .try_into()
                .map_err(|_| MempoolPersistenceError::Format("invalid chain ID encoding"))?,
        );
        if chain_id != self.chain_id {
            return Err(MempoolPersistenceError::ChainMismatch {
                expected: self.chain_id,
                got: chain_id,
            });
        }
        if take(payload, &mut cursor, 32)? != self.genesis_hash.as_bytes() {
            return Err(MempoolPersistenceError::GenesisMismatch);
        }
        let count = read_u32(payload, &mut cursor)? as usize;
        check_count(count)?;
        let mut transactions = Vec::with_capacity(count);
        let mut slots = HashSet::with_capacity(count);
        for index in 0..count {
            let length = read_u32(payload, &mut cursor)? as usize;
            check_size(length as u64, MAX_MEMPOOL_TRANSACTION_BYTES)?;
            let record = take(payload, &mut cursor, length)?;
            let tx: Transaction = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(length as u64)
                .reject_trailing_bytes()
                .deserialize(record)
                .map_err(MempoolPersistenceError::Encoding)?;
            validate(&tx, self.chain_id, index)?;
            if !slots.insert((tx.from, tx.nonce)) {
                return Err(MempoolPersistenceError::DuplicateSlot(index));
            }
            transactions.push(tx);
        }
        if cursor != payload.len() {
            return Err(MempoolPersistenceError::Format("trailing snapshot bytes"));
        }
        Ok(transactions)
    }

    /// Validate and atomically replace a candidate snapshot before acknowledging it.
    pub fn persist(&self, pool: &Mempool) -> Result<(), MempoolPersistenceError> {
        check_count(pool.len())?;
        // Borrow transactions: cloning a large pool just to reject the byte
        // limit would defeat bounded snapshot memory use.
        let mut transactions: Vec<_> = pool.txs.values().collect();
        transactions.sort_by_key(|tx| (tx.from, tx.nonce));
        let mut total = HEADER_BYTES as u64 + 8 + 32 + 4;
        let mut lengths = Vec::with_capacity(transactions.len());
        for tx in &transactions {
            check_size(tx.data.len() as u64, MAX_MEMPOOL_TRANSACTION_BYTES)?;
            let length = bincode::serialized_size(tx).map_err(MempoolPersistenceError::Encoding)?;
            check_size(length, MAX_MEMPOOL_TRANSACTION_BYTES)?;
            total += 4 + length;
            check_size(total, MAX_MEMPOOL_BYTES)?;
            lengths.push(length as u32);
        }
        for (index, tx) in transactions.iter().enumerate() {
            validate(tx, self.chain_id, index)?;
        }
        let mut payload = Vec::with_capacity(total as usize - HEADER_BYTES);
        payload.extend_from_slice(&self.chain_id.to_le_bytes());
        payload.extend_from_slice(self.genesis_hash.as_bytes());
        payload.extend_from_slice(&(transactions.len() as u32).to_le_bytes());
        for (tx, length) in transactions.into_iter().zip(lengths) {
            payload.extend_from_slice(&length.to_le_bytes());
            payload.extend_from_slice(
                &bincode::serialize(tx).map_err(MempoolPersistenceError::Encoding)?,
            );
        }
        let parent = self.path.parent().ok_or(MempoolPersistenceError::Format("missing parent"))?;
        fs::create_dir_all(parent).map_err(|source| self.io(parent, source))?;
        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| self.io(parent, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| self.io(temp.path(), source))?;
        }
        temp.write_all(&envelope(&payload)).map_err(|source| self.io(&self.path, source))?;
        temp.as_file().sync_all().map_err(|source| self.io(&self.path, source))?;
        temp.persist(&self.path).map_err(|error| self.io(&self.path, error.error))?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| self.io(parent, source))?;
        Ok(())
    }

    fn io(&self, path: &Path, source: std::io::Error) -> MempoolPersistenceError {
        MempoolPersistenceError::Io { path: path.to_path_buf(), source }
    }
}

fn envelope(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(blake3::hash(payload).as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn validate(
    tx: &Transaction,
    chain_id: ChainId,
    index: usize,
) -> Result<(), MempoolPersistenceError> {
    if tx.chain_id != chain_id {
        return Err(MempoolPersistenceError::ChainMismatch {
            expected: chain_id,
            got: tx.chain_id,
        });
    }
    if tx.tx_type != TransactionType::Transfer
        || tx.to.is_none()
        || tx.gas_limit == 0
        || tx.total_cost().is_none()
        || !tx.verify_signature()
    {
        return Err(MempoolPersistenceError::InvalidTransaction(index));
    }
    Ok(())
}

fn check_count(count: usize) -> Result<(), MempoolPersistenceError> {
    if count > MAX_MEMPOOL_TRANSACTIONS {
        return Err(MempoolPersistenceError::TooMany { count, max: MAX_MEMPOOL_TRANSACTIONS });
    }
    Ok(())
}

fn check_size(size: u64, max: u64) -> Result<(), MempoolPersistenceError> {
    if size > max {
        return Err(MempoolPersistenceError::TooLarge { size, max });
    }
    Ok(())
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, MempoolPersistenceError> {
    Ok(u32::from_le_bytes(
        take(bytes, cursor, 4)?
            .try_into()
            .map_err(|_| MempoolPersistenceError::Format("invalid length encoding"))?,
    ))
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], MempoolPersistenceError> {
    let end = cursor
        .checked_add(length)
        .ok_or(MempoolPersistenceError::Format("record offset overflow"))?;
    let record =
        bytes.get(*cursor..end).ok_or(MempoolPersistenceError::Format("truncated snapshot"))?;
    *cursor = end;
    Ok(record)
}

#[derive(Debug, thiserror::Error)]
pub enum MempoolPersistenceError {
    #[error("mempool chain ID must be nonzero")]
    InvalidChainId,
    #[error("pending snapshot chain mismatch: expected {expected}, got {got}")]
    ChainMismatch { expected: ChainId, got: ChainId },
    #[error("pending snapshot genesis mismatch")]
    GenesisMismatch,
    #[error("invalid pending snapshot: {0}")]
    Format(&'static str),
    #[error("unsupported pending snapshot version: {0}")]
    UnsupportedVersion(u32),
    #[error("pending snapshot checksum mismatch")]
    ChecksumMismatch,
    #[error("invalid or unauthenticated pending transaction at index {0}")]
    InvalidTransaction(usize),
    #[error("duplicate pending sender/nonce slot at index {0}")]
    DuplicateSlot(usize),
    #[error("pending snapshot has {count} transactions, maximum {max}")]
    TooMany { count: usize, max: usize },
    #[error("pending snapshot/record is {size} bytes, maximum {max}")]
    TooLarge { size: u64, max: u64 },
    #[error("cannot encode/decode pending transaction: {0}")]
    Encoding(#[source] bincode::Error),
    #[error("pending snapshot I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests;
