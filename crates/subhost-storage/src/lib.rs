//! Crash-safe ledger persistence.
//!
//! The current on-disk format is a segmented log (`ledger/manifest.json`,
//! `ledger/snapshot-<height>.bin`, `ledger/segments/NNNNNN.log`). The manifest is
//! the commit point: a segment record or snapshot that the manifest does not
//! reference is never trusted, and is dropped before the next append.
//!
//! The previous version 2 single-file format (`node-state.bin`) is still read, so
//! an existing node upgrades in place: the first [`LedgerStore::persist`] after
//! the upgrade rewrites the history as segments and keeps the old file as
//! `node-state.bin.v2-backup`.
//!
//! Every read is validated before it is trusted: size bounds, magic, version,
//! BLAKE3 checksums, manifest/segment consistency, chain binding, then a full
//! replay of the block/receipt commitments against the restored state. A
//! tampered, truncated, or half-written ledger is rejected rather than silently
//! loaded.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use subhost_core::{
    encode_canonical, tx_root_of, Block, ChainId, Gas, Hash, Receipt, ReceiptStatus,
};
use subhost_state::{State, StateSnapshot};
use tracing::debug;

mod legacy;
mod segmented;
#[cfg(test)]
mod tests;

/// Largest legacy (`node-state.bin`) envelope this build will read. The
/// segmented format replaces the single-file limit with a per-segment target.
pub const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;
/// Legacy single-file ledger name, kept for reading and migrating version 2 data.
pub const LEDGER_FILE_NAME: &str = "node-state.bin";
/// Where the migrated version 2 file is kept after a successful upgrade.
pub const LEDGER_BACKUP_FILE_NAME: &str = "node-state.bin.v2-backup";
/// Directory inside the data directory that holds the segmented ledger.
pub const LEDGER_DIR_NAME: &str = "ledger";
/// Manifest file inside [`LEDGER_DIR_NAME`]. Its presence makes the ledger
/// authoritative over any leftover legacy file.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

pub(crate) const LEDGER_MAGIC: [u8; 8] = *b"SUBHOST1";
/// Version 2 introduced Merkle state roots, multi-transaction blocks, and
/// transaction-indexed receipts. A version 1 file cannot be validated against
/// the new commitments, so it is refused rather than silently reinterpreted.
pub(crate) const LEDGER_FORMAT_VERSION: u32 = 2;

/// The persisted ledger: account state plus the block history it still holds.
///
/// `blocks` is the *retained* history. On a node that has never pruned it starts
/// at height 1; after [`LedgerStore::prune`] it starts at `base_height + 1`, and
/// [`LedgerStore::persist`] expects that shape rather than a full replay from
/// genesis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub chain_id: ChainId,
    pub state: StateSnapshot,
    pub blocks: Vec<Block>,
    pub receipts: Vec<Receipt>,
}

/// What a node recovered from disk.
#[derive(Debug)]
pub struct RestoredLedger {
    /// Number of blocks that existed before the retained history started.
    ///
    /// `0` for a ledger that still holds every block. `blocks[0]`, when present,
    /// is at height `base_height + 1`, and the newest block is at
    /// `base_height + blocks.len()`.
    pub base_height: u64,
    pub state: State,
    pub blocks: Vec<Block>,
    pub receipts: Vec<Receipt>,
}

impl RestoredLedger {
    /// Height of the newest block, or 0 for a ledger with no blocks.
    pub fn height(&self) -> u64 {
        self.blocks.last().map_or(0, |block| block.header.height)
    }

    /// Number of retained blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

/// Result of [`LedgerStore::prune`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneOutcome {
    /// Oldest segments were dropped and the manifest now starts at `base_height`.
    Pruned { dropped_segments: usize, retained_segments: usize, base_height: u64 },
    /// The ledger already keeps at most the requested number of segments.
    NothingToPrune { segments: usize },
}

/// Result of [`LedgerStore::migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The ledger is already segmented; nothing was written. Migration is
    /// idempotent, so a second run is not an error.
    AlreadySegmented,
    /// There is no version 2 `node-state.bin` to migrate.
    NothingToMigrate,
    /// The legacy file was migrated and verified by a full replay.
    Migrated { height: u64 },
}

/// On-disk footprint of the committed ledger, for the metrics exporter.
///
/// Measured from the validated manifest's own file references plus filesystem
/// metadata — no block replay, no checksum verification. This is an operational
/// measurement, not block/QC authentication. Backups and orphan files the
/// manifest does not name are excluded. Missing or invalid active data is an
/// error, never a false zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerDiskStats {
    /// Actual on-disk bytes of the manifest, its snapshot and its segments.
    pub active_file_bytes: u64,
    /// Modification time of the committed snapshot file, when one exists.
    /// `None` for an ephemeral/fresh store or a legacy single-file ledger.
    pub snapshot_modified: Option<SystemTime>,
}

/// A ledger store bound to one chain ID and one data directory.
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
    /// Open (or prepare to create) the ledger under `data_dir`.
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

    /// Legacy single-file path, or `None` for an ephemeral store.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether a ledger already exists in any supported format. Used to tell a
    /// fresh node from one that has already applied genesis allocations.
    pub fn exists(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let Some(data_dir) = path.parent() else {
            return path.is_file();
        };
        segmented::manifest_path(data_dir).is_file() || path.is_file()
    }

    /// Load and fully validate the ledger, or return an empty one.
    pub fn load(&self) -> Result<RestoredLedger, StorageError> {
        let Some(path) = &self.path else {
            return Ok(self.empty());
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        if segmented::manifest_path(data_dir).is_file() {
            return segmented::load(self.chain_id, data_dir);
        }
        if !path.is_file() {
            return Ok(self.empty());
        }
        self.restore(legacy::read(path)?)
    }

    /// Validate a decoded snapshot and rebuild the in-memory ledger from it.
    pub fn restore(&self, snapshot: LedgerSnapshot) -> Result<RestoredLedger, StorageError> {
        let state = validate_snapshot(&snapshot, self.chain_id)?;
        debug!(
            blocks = snapshot.blocks.len(),
            receipts = snapshot.receipts.len(),
            "restored ledger"
        );
        Ok(RestoredLedger {
            base_height: 0,
            state,
            blocks: snapshot.blocks,
            receipts: snapshot.receipts,
        })
    }

    /// Persist a snapshot, validating it first so a rejected ledger can never be
    /// written and then fail to load.
    ///
    /// A snapshot that does not extend the committed history is refused until
    /// fork choice lands (S3): writing it would silently rewrite the chain.
    pub fn persist(&self, snapshot: &LedgerSnapshot) -> Result<(), StorageError> {
        self.persist_snapshot(
            snapshot,
            segmented::SEGMENT_MAX_BYTES,
            segmented::SNAPSHOT_INTERVAL_BLOCKS,
        )
    }

    fn persist_snapshot(
        &self,
        snapshot: &LedgerSnapshot,
        segment_max_bytes: u64,
        snapshot_interval: u64,
    ) -> Result<(), StorageError> {
        let Some(path) = &self.path else {
            return validate_snapshot(snapshot, self.chain_id).map(|_| ());
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        fs::create_dir_all(data_dir)
            .map_err(|source| StorageError::Io { path: data_dir.to_path_buf(), source })?;
        // `segmented::persist` reads the committed base from the manifest and
        // validates the retained suffix against it before writing anything.
        segmented::persist(self.chain_id, data_dir, snapshot, segment_max_bytes, snapshot_interval)
    }

    /// Test-only entry point that shrinks the segment roll-over threshold and the
    /// snapshot interval so both can be exercised without huge fixtures.
    #[cfg(test)]
    pub(crate) fn persist_with_limits(
        &self,
        snapshot: &LedgerSnapshot,
        segment_max_bytes: u64,
        snapshot_interval: u64,
    ) -> Result<(), StorageError> {
        self.persist_snapshot(snapshot, segment_max_bytes, snapshot_interval)
    }

    /// Test-only: keep the default interval but shrink only the segment target.
    #[cfg(test)]
    pub(crate) fn persist_with_segment_limit(
        &self,
        snapshot: &LedgerSnapshot,
        segment_max_bytes: u64,
    ) -> Result<(), StorageError> {
        self.persist_with_limits(snapshot, segment_max_bytes, segmented::SNAPSHOT_INTERVAL_BLOCKS)
    }

    /// Migrate a version 2 `node-state.bin` to the segmented format.
    ///
    /// The legacy file is fully validated before anything is written, the
    /// migrated ledger is loaded back and compared with the legacy head, and the
    /// original file is kept as `node-state.bin.v2-backup`. A second call
    /// validates the segmented ledger without rewriting it, so an interrupted
    /// run can simply be repeated and committed damage is still reported.
    pub fn migrate(&self) -> Result<MigrationOutcome, StorageError> {
        let Some(path) = &self.path else {
            return Err(StorageError::Invalid(
                "an ephemeral store has no ledger to migrate".into(),
            ));
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        if segmented::manifest_path(data_dir).is_file() {
            self.load()?;
            return Ok(MigrationOutcome::AlreadySegmented);
        }
        if !path.is_file() {
            return Ok(MigrationOutcome::NothingToMigrate);
        }

        let legacy = self.restore(legacy::read(path)?)?;
        let height = legacy.height();
        let state_root = legacy.state.root();
        let snapshot = LedgerSnapshot {
            chain_id: self.chain_id,
            state: legacy.state.snapshot(),
            blocks: legacy.blocks,
            receipts: legacy.receipts,
        };

        self.persist(&snapshot)?;

        // Re-read through the normal loader: the migration only counts if the
        // segmented history replays to the same head.
        let migrated = self.load()?;
        if migrated.height() != height || migrated.state.root() != state_root {
            return Err(StorageError::Invalid(
                "migrated ledger does not replay to the same head".into(),
            ));
        }
        debug!(height, "migrated the legacy ledger to segments");
        Ok(MigrationOutcome::Migrated { height })
    }

    /// Drop the oldest segments, keeping only the newest `keep_segments` files.
    ///
    /// The committed snapshot stays the replay anchor, so pruning is refused when
    /// the retained history would no longer contain the block at the snapshot's
    /// own height. On refusal nothing on disk changes. Blocks and receipts that
    /// are pruned are gone: this node stops answering for those heights.
    pub fn prune(&self, keep_segments: usize) -> Result<PruneOutcome, StorageError> {
        if keep_segments == 0 {
            return Err(StorageError::Invalid(
                "keep_segments must be at least 1, else the ledger has no history".into(),
            ));
        }
        let Some(path) = &self.path else {
            return Err(StorageError::Invalid("an ephemeral store has no ledger to prune".into()));
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        if !segmented::manifest_path(data_dir).is_file() {
            return Err(StorageError::Invalid("there is no segmented ledger to prune".into()));
        }
        segmented::prune(self.chain_id, data_dir, keep_segments)
    }

    /// Measure the committed ledger's on-disk footprint without replaying it.
    ///
    /// The caller serializes this with [`Self::persist`]/[`Self::prune`] under
    /// the node's commit lock so the manifest and the files it names are read
    /// coherently. An ephemeral or fresh store reports empty metadata; a legacy
    /// single-file ledger reports that file's size and no snapshot timestamp.
    /// Legacy edge: a ledger file deleted after open is indistinguishable from
    /// a fresh store without extra state, so it also reports empty metadata
    /// (segmented stores error on a missing named file instead).
    pub fn disk_stats(&self) -> Result<LedgerDiskStats, StorageError> {
        let Some(path) = &self.path else {
            return Ok(LedgerDiskStats { active_file_bytes: 0, snapshot_modified: None });
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        if segmented::manifest_path(data_dir).is_file() {
            return segmented::disk_stats(self.chain_id, data_dir);
        }
        if !path.is_file() {
            return Ok(LedgerDiskStats { active_file_bytes: 0, snapshot_modified: None });
        }
        let metadata =
            fs::metadata(path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
        Ok(LedgerDiskStats { active_file_bytes: metadata.len(), snapshot_modified: None })
    }

    fn empty(&self) -> RestoredLedger {
        RestoredLedger {
            base_height: 0,
            state: State::with_chain_id(self.chain_id),
            blocks: Vec::new(),
            receipts: Vec::new(),
        }
    }
}

/// Validate a snapshot against the configured chain and rebuild its state.
pub(crate) fn validate_snapshot(
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
) -> Result<State, StorageError> {
    validate_snapshot_from(snapshot, chain_id, 0)
}

/// Validate a snapshot whose history starts after `base_height`.
fn validate_snapshot_from(
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    base_height: u64,
) -> Result<State, StorageError> {
    if snapshot.chain_id != chain_id || snapshot.state.chain_id != chain_id {
        return Err(StorageError::ChainMismatch { expected: chain_id, got: snapshot.chain_id });
    }
    let state = State::from_snapshot(snapshot.state.clone())
        .map_err(|error| StorageError::Invalid(error.to_string()))?;
    validate_chain_from(base_height, &snapshot.blocks, &snapshot.receipts, chain_id, &state)?;
    Ok(state)
}

/// Commitment for one successful receipt.
///
/// Every field that the receipt reports is committed, including its position in
/// the block and the cumulative gas figure, so a relayer cannot reorder receipts
/// without changing the block's `receipt_root`. The block hash is deliberately
/// excluded because the commitment is computed before the header is hashed.
pub fn receipt_commitment(
    tx_hash: Hash,
    block_height: u64,
    transaction_index: u32,
    gas_used: Gas,
    cumulative_gas_used: Gas,
) -> Hash {
    Hash::from_data(&encode_canonical(&(
        tx_hash,
        block_height,
        transaction_index,
        gas_used,
        cumulative_gas_used,
        ReceiptStatus::Success as u8,
    )))
}

/// Ordered commitment over every receipt in one block.
pub fn receipts_root(commitments: &[Hash]) -> Hash {
    Hash::from_data(&encode_canonical(&commitments.to_vec()))
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
    validate_chain_from(0, blocks, receipts, chain_id, state)
}

/// [`validate_chain`] for a history whose first retained block is
/// `base_height + 1`.
///
/// The parent link of that first block cannot be checked, because the block it
/// points at has been pruned; every later parent link is still verified. The
/// retained state snapshot is what anchors the suffix to the pruned prefix.
fn validate_chain_from(
    base_height: u64,
    blocks: &[Block],
    receipts: &[Receipt],
    chain_id: ChainId,
    state: &State,
) -> Result<(), StorageError> {
    let mut parent = Hash::ZERO;
    let mut seen_blocks = HashSet::with_capacity(blocks.len());
    let mut seen_transactions = HashSet::new();
    let mut transaction_count = 0usize;

    for (index, block) in blocks.iter().enumerate() {
        let expected_height = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(base_height))
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| StorageError::Invalid("block height overflow".into()))?;
        let parent_checked = index > 0 || base_height == 0;
        if block.header.height != expected_height
            || (parent_checked && block.header.parent_hash != parent)
            || block.header.chain_id != chain_id
            || block.header.gas_used > block.header.gas_limit
            || !seen_blocks.insert(block.hash())
        {
            return Err(StorageError::Invalid(format!(
                "invalid block sequence at height {expected_height}"
            )));
        }

        let mut gas_used = 0u64;
        let mut hashes = Vec::with_capacity(block.transactions.len());
        let mut commitments = Vec::with_capacity(block.transactions.len());
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            let tx_hash = tx.hash();
            if tx.chain_id != chain_id
                || tx.gas_limit == 0
                || !tx.verify_signature()
                || !seen_transactions.insert(tx_hash)
            {
                return Err(StorageError::Invalid(format!(
                    "invalid transaction at height {expected_height}"
                )));
            }
            gas_used = gas_used
                .checked_add(tx.gas_limit)
                .ok_or_else(|| StorageError::Invalid("block gas overflow".into()))?;
            hashes.push(tx_hash);
            let transaction_index = u32::try_from(tx_index)
                .map_err(|_| StorageError::Invalid("too many transactions in a block".into()))?;
            commitments.push(receipt_commitment(
                tx_hash,
                expected_height,
                transaction_index,
                tx.gas_limit,
                gas_used,
            ));
        }

        if gas_used != block.header.gas_used
            || block.header.tx_root != tx_root_of(&hashes)
            || block.header.receipt_root != receipts_root(&commitments)
        {
            return Err(StorageError::Invalid(format!(
                "invalid block commitment at height {expected_height}"
            )));
        }
        transaction_count = transaction_count
            .checked_add(block.transactions.len())
            .ok_or_else(|| StorageError::Invalid("transaction count overflow".into()))?;
        parent = block.hash();
    }

    if transaction_count != receipts.len() {
        return Err(StorageError::Invalid(
            "every transaction must have exactly one receipt".into(),
        ));
    }

    let mut receipt_by_tx = HashMap::with_capacity(receipts.len());
    for receipt in receipts {
        if receipt.status != ReceiptStatus::Success {
            return Err(StorageError::Invalid("a failed receipt cannot be committed".into()));
        }
        if receipt_by_tx.insert(receipt.tx_hash, receipt).is_some() {
            return Err(StorageError::Invalid("duplicate receipt for one transaction".into()));
        }
    }

    for block in blocks {
        let mut cumulative_gas_used = 0u64;
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used = cumulative_gas_used
                .checked_add(tx.gas_limit)
                .ok_or_else(|| StorageError::Invalid("block gas overflow".into()))?;
            let transaction_index = u32::try_from(tx_index)
                .map_err(|_| StorageError::Invalid("too many transactions in a block".into()))?;
            let receipt = receipt_by_tx
                .get(&tx.hash())
                .ok_or_else(|| StorageError::Invalid("missing transaction receipt".into()))?;
            if receipt.block_hash != block.hash()
                || receipt.block_height != block.header.height
                || receipt.transaction_index != transaction_index
                || receipt.gas_used != tx.gas_limit
                || receipt.cumulative_gas_used != cumulative_gas_used
            {
                return Err(StorageError::Invalid(format!(
                    "invalid receipt for block {}",
                    block.header.height
                )));
            }
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

    #[error("unsupported ledger manifest version: {0}")]
    UnsupportedManifestVersion(u32),

    #[error("ledger checksum mismatch")]
    ChecksumMismatch,

    #[error("ledger manifest is invalid: {0}")]
    Manifest(String),

    #[error("ledger segment is invalid: {0}")]
    Segment(String),

    #[error("ledger snapshot is invalid: {0}")]
    Snapshot(String),

    #[error("refusing to persist a history that does not extend the committed chain")]
    NonAppendPersist,

    #[error(
        "refusing to prune: the retained history would start at height {first_retained_height}, after the snapshot at height {snapshot_height} that anchors replay"
    )]
    PruneWouldDropSnapshot { first_retained_height: u64, snapshot_height: u64 },

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
