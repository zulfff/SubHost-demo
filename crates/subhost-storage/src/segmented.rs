//! The segmented ledger: a manifest commit point, one state snapshot, and an
//! append-only log of checksummed block records.
//!
//! Layout inside the data directory:
//!
//! ```text
//! ledger/manifest.json              commit point (chain id, head, snapshot, segments)
//! ledger/snapshot-<height>.bin      serialized account state at the head
//! ledger/segments/000000.log        framed `block + receipts` records
//! ```
//!
//! A persist appends only the blocks that are new, fsyncs the segment, writes
//! a state snapshot when due, and finally replaces the manifest atomically.
//! Bytes past the manifest's committed length are an interrupted append: they are
//! ignored on load and truncated before the next append, so a torn tail can never
//! prevent start-up from the last committed head.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use subhost_core::{Block, ChainId, FeePolicy, Hash, Receipt};
use subhost_state::{State, StateSnapshot};
use tracing::debug;

use crate::{
    bind_fee_policy, lock_cache, replay_blocks, HeadCache, LedgerSnapshot, PruneOutcome,
    RestoredLedger, StorageError, VerifiedHead, LEDGER_BACKUP_FILE_NAME, LEDGER_DIR_NAME,
    LEDGER_FILE_NAME, MANIFEST_FILE_NAME,
};

/// Target size for one segment file. A record is never split, so a segment may
/// exceed this by at most one record. Total history is not capped.
pub(crate) const SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Write a fresh state snapshot once this many blocks have accumulated past the
/// last one. Load replays at most this many blocks on top of the snapshot.
pub(crate) const SNAPSHOT_INTERVAL_BLOCKS: u64 = 1024;

const SEGMENT_MAGIC: [u8; 8] = *b"SUBSEG01";
const SEGMENT_VERSION: u32 = 1;
const SEGMENT_HEADER_LEN: u64 = 28;
const RECORD_MAGIC: [u8; 4] = *b"SREC";
const RECORD_VERSION: u32 = 1;
const RECORD_HEADER_LEN: u64 = 44;
const MAX_RECORD_BYTES: u32 = 16 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MANIFEST_VERSION: u32 = 2;
/// Version 1 manifests predate pruning: they always start at height 1 and carry
/// no `base_height`.
const MANIFEST_VERSION_LEGACY: u32 = 1;
const SNAPSHOT_MAGIC: [u8; 8] = *b"SUBHSST1";
const SNAPSHOT_VERSION: u32 = 1;

/// Directory that holds the segmented ledger.
pub(crate) fn ledger_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LEDGER_DIR_NAME)
}

/// Manifest inside [`ledger_dir`]; its presence makes the ledger authoritative
/// over a leftover legacy `node-state.bin`.
pub(crate) fn manifest_path(data_dir: &Path) -> PathBuf {
    ledger_dir(data_dir).join(MANIFEST_FILE_NAME)
}

fn segments_dir(data_dir: &Path) -> PathBuf {
    ledger_dir(data_dir).join("segments")
}

fn snapshot_file_name(height: u64) -> String {
    format!("snapshot-{height:010}.bin")
}

fn segment_file_name(index: u64) -> String {
    format!("{index:06}.log")
}

/// `manifest.json` as written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    version: u32,
    chain_id: ChainId,
    head_height: u64,
    head_hash: String,
    /// Number of blocks that were pruned before the retained segments. Absent in
    /// version 1, where it is always zero.
    #[serde(default)]
    base_height: u64,
    /// Genesis-bound fee policy of a fee-enabled chain (Q-030). Absent on every
    /// legacy manifest, so policy-less files stay byte-identical; an older build
    /// refuses the field outright because this struct denies unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fee_policy: Option<FeePolicy>,
    snapshot: SnapshotFile,
    segments: Vec<SegmentFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotFile {
    file: String,
    height: u64,
    state_root: String,
    checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentFile {
    file: String,
    records: u64,
    bytes: u64,
}

/// A validated snapshot reference from the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotRef {
    pub file: String,
    pub height: u64,
    pub state_root: Hash,
    pub checksum: Hash,
}

/// A validated segment reference from the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentRef {
    pub file: String,
    pub records: u64,
    pub bytes: u64,
}

/// What the manifest commits to, with hashes already parsed.
#[derive(Debug, Clone)]
pub(crate) struct CommittedLedger {
    pub head_height: u64,
    pub head_hash: Hash,
    /// Height of the last pruned block; the retained segments start after it.
    pub base_height: u64,
    /// The fee policy this ledger commits to, if it is fee-enabled.
    pub fee_policy: Option<FeePolicy>,
    pub snapshot: Option<SnapshotRef>,
    pub segments: Vec<SegmentRef>,
}

impl CommittedLedger {
    fn records(&self) -> Result<u64, StorageError> {
        self.segments.iter().try_fold(0u64, |total, segment| {
            total
                .checked_add(segment.records)
                .ok_or_else(|| StorageError::Manifest("segment record count overflow".into()))
        })
    }

    /// Height of the oldest retained block.
    fn first_retained_height(&self) -> Result<u64, StorageError> {
        self.base_height
            .checked_add(1)
            .ok_or_else(|| StorageError::Manifest("base height overflow".into()))
    }
}

/// One log record: the block plus the receipts it produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentRecord {
    block: Block,
    receipts: Vec<Receipt>,
}

/// Read and validate `manifest.json`.
pub(crate) fn read_manifest(
    data_dir: &Path,
    chain_id: ChainId,
) -> Result<CommittedLedger, StorageError> {
    let path = manifest_path(data_dir);
    let metadata =
        fs::metadata(&path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(StorageError::TooLarge { size: metadata.len(), max: MAX_MANIFEST_BYTES });
    }
    let bytes =
        fs::read(&path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
    let manifest: ManifestFile = serde_json::from_slice(&bytes)
        .map_err(|error| StorageError::Manifest(error.to_string()))?;

    if manifest.version != MANIFEST_VERSION && manifest.version != MANIFEST_VERSION_LEGACY {
        return Err(StorageError::UnsupportedManifestVersion(manifest.version));
    }
    if manifest.version == MANIFEST_VERSION_LEGACY && manifest.base_height != 0 {
        return Err(StorageError::Manifest(
            "a version 1 manifest cannot have pruned history".into(),
        ));
    }
    if manifest.chain_id != chain_id {
        return Err(StorageError::ChainMismatch { expected: chain_id, got: manifest.chain_id });
    }

    let snapshot = SnapshotRef {
        file: manifest.snapshot.file.clone(),
        height: manifest.snapshot.height,
        state_root: parse_hash(&manifest.snapshot.state_root, "snapshot.state_root")?,
        checksum: parse_hash(&manifest.snapshot.checksum, "snapshot.checksum")?,
    };
    if snapshot.file != snapshot_file_name(snapshot.height) {
        return Err(StorageError::Manifest(format!(
            "snapshot file name {:?} does not match its height",
            snapshot.file
        )));
    }

    let mut segments = Vec::with_capacity(manifest.segments.len());
    let mut previous_index = None;
    for entry in &manifest.segments {
        let index = segment_index(&entry.file).ok_or_else(|| {
            StorageError::Manifest(format!("segment file name {:?} is invalid", entry.file))
        })?;
        // Names stay monotonically increasing after pruning, so a gap is fine but
        // a repeat or a step backwards is not.
        if previous_index.is_some_and(|previous| index <= previous) {
            return Err(StorageError::Manifest(format!(
                "segment file name {:?} is not ordered after its predecessor",
                entry.file
            )));
        }
        previous_index = Some(index);
        if entry.records == 0 {
            return Err(StorageError::Manifest(format!(
                "segment {} is committed with no records",
                entry.file
            )));
        }
        if entry.bytes < SEGMENT_HEADER_LEN {
            return Err(StorageError::Manifest(format!(
                "segment {} is smaller than its header",
                entry.file
            )));
        }
        segments.push(SegmentRef {
            file: entry.file.clone(),
            records: entry.records,
            bytes: entry.bytes,
        });
    }

    let committed = CommittedLedger {
        head_height: manifest.head_height,
        head_hash: parse_hash(&manifest.head_hash, "head_hash")?,
        base_height: manifest.base_height,
        fee_policy: manifest.fee_policy.clone(),
        snapshot: Some(snapshot),
        segments,
    };
    let records = committed.records()?;
    let covered = committed
        .base_height
        .checked_add(records)
        .ok_or_else(|| StorageError::Manifest("retained height overflow".into()))?;
    if covered != committed.head_height {
        return Err(StorageError::Manifest(format!(
            "segments hold {records} records after base height {} but the head height is {}",
            committed.base_height, committed.head_height
        )));
    }
    if committed.head_height == 0
        && (committed.head_hash != Hash::ZERO
            || committed.base_height != 0
            || !committed.segments.is_empty())
    {
        return Err(StorageError::Manifest(
            "an empty ledger must have a zero head, no base, and no segments".into(),
        ));
    }
    if committed.base_height >= committed.head_height && committed.head_height > 0 {
        return Err(StorageError::Manifest(format!(
            "base height {} is not below the head height {}",
            committed.base_height, committed.head_height
        )));
    }
    if let Some(snapshot) = &committed.snapshot {
        if snapshot.height > committed.head_height {
            return Err(StorageError::Manifest(format!(
                "snapshot height {} is ahead of the head height {}",
                snapshot.height, committed.head_height
            )));
        }
        if snapshot.height < committed.base_height
            || (snapshot.height == committed.base_height && snapshot.height > 0)
        {
            return Err(StorageError::Manifest(format!(
                "snapshot height {} is not inside the retained history that starts at {}",
                snapshot.height,
                committed.base_height + 1
            )));
        }
    }
    Ok(committed)
}

/// Measure the files the validated manifest names, without replaying records.
///
/// Every named file must exist: a missing segment or snapshot is reported as an
/// I/O error rather than silently shrinking the measurement. Only the manifest,
/// its snapshot and its segments count; backups and orphans are excluded.
pub(crate) fn disk_stats(
    chain_id: ChainId,
    data_dir: &Path,
) -> Result<crate::LedgerDiskStats, StorageError> {
    let committed = read_manifest(data_dir, chain_id)?;
    let mut total = 0u64;
    let add = |path: PathBuf| -> Result<u64, StorageError> {
        let metadata = fs::metadata(&path)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        Ok(metadata.len())
    };
    total = total
        .checked_add(add(manifest_path(data_dir))?)
        .ok_or_else(|| StorageError::Invalid("ledger size overflow".into()))?;
    for segment in &committed.segments {
        let bytes = add(segments_dir(data_dir).join(&segment.file))?;
        total = total
            .checked_add(bytes)
            .ok_or_else(|| StorageError::Invalid("ledger size overflow".into()))?;
    }
    let snapshot_modified = if let Some(snapshot) = &committed.snapshot {
        let path = ledger_dir(data_dir).join(&snapshot.file);
        let metadata = fs::metadata(&path)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| StorageError::Invalid("ledger size overflow".into()))?;
        metadata.modified().ok()
    } else {
        None
    };
    Ok(crate::LedgerDiskStats { active_file_bytes: total, snapshot_modified })
}

/// Parse the six-digit index out of `NNNNNN.log`.
fn segment_index(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".log")?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn parse_hash(value: &str, field: &str) -> Result<Hash, StorageError> {
    Hash::from_hex(value)
        .map_err(|error| StorageError::Manifest(format!("invalid {field}: {error}")))
}

/// Load and fully validate the segmented ledger.
///
/// `supplied` is the policy the caller opened the store with; `None` means "use
/// the recorded one", which is how offline tools validate a fee-enabled ledger.
pub(crate) fn load(
    chain_id: ChainId,
    data_dir: &Path,
    supplied: Option<&FeePolicy>,
) -> Result<RestoredLedger, StorageError> {
    let committed = read_manifest(data_dir, chain_id)?;
    let base_height = committed.base_height;
    let snapshot = committed
        .snapshot
        .as_ref()
        .ok_or_else(|| StorageError::Manifest("manifest has no snapshot".into()))?;
    let mut state = read_snapshot(data_dir, chain_id, snapshot)?;
    let (blocks, receipts) = read_segments(data_dir, chain_id, &committed)?;

    let retained = u64::try_from(blocks.len()).ok();
    let height = retained
        .and_then(|retained| base_height.checked_add(retained))
        .filter(|height| *height == committed.head_height);
    let head_hash = blocks.last().map_or(Hash::ZERO, Block::hash);
    if height.is_none() || head_hash != committed.head_hash {
        return Err(StorageError::Manifest(
            "segment history does not match the committed head".into(),
        ));
    }

    // The snapshot must be rooted in the block at its own height, not merely be
    // internally consistent: otherwise a stale or mismatched snapshot could pass
    // the manifest checksum and still replay onto the wrong history. A snapshot
    // at height 0 is the genesis state and has no block to anchor to.
    if snapshot.height > 0 {
        let index = snapshot
            .height
            .checked_sub(base_height)
            .and_then(|offset| offset.checked_sub(1))
            .ok_or_else(|| {
                StorageError::Manifest(format!(
                    "snapshot height {} is not inside the retained history",
                    snapshot.height
                ))
            })?;
        let index = usize::try_from(index)
            .map_err(|_| StorageError::Manifest("snapshot height does not fit in memory".into()))?;
        let anchored = blocks.get(index).ok_or_else(|| {
            StorageError::Manifest(format!(
                "snapshot height {} is beyond the committed history",
                snapshot.height
            ))
        })?;
        if anchored.header.state_root != snapshot.state_root {
            return Err(StorageError::Snapshot(format!(
                "snapshot state root does not match block {}",
                snapshot.height
            )));
        }
    }

    // Replay only the blocks after the snapshot. `validate_chain_from` still
    // checks every retained commitment, so a truncated suffix cannot hide a
    // rewrite of the blocks this node still holds.
    let replay_from = snapshot
        .height
        .checked_sub(base_height)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(|| StorageError::Manifest("snapshot height does not fit in memory".into()))?;
    let policy = bind_fee_policy(committed.fee_policy.as_ref(), supplied, &blocks, true, false)?;
    let parent =
        replay_from.checked_sub(1).and_then(|index| blocks.get(index)).map(|block| &block.header);
    replay_blocks(&blocks[replay_from..], parent, policy.as_ref(), &mut state, "on the snapshot")
        .map_err(|error| match error {
        StorageError::Invalid(message) => StorageError::Snapshot(message),
        other => other,
    })?;

    crate::validate_chain_from(base_height, &blocks, &receipts, chain_id, &state)?;
    debug!(
        base_height,
        blocks = blocks.len(),
        receipts = receipts.len(),
        replayed = blocks.len().saturating_sub(replay_from),
        "restored segmented ledger"
    );
    Ok(RestoredLedger { base_height, state, blocks, receipts })
}

fn read_snapshot(
    data_dir: &Path,
    chain_id: ChainId,
    reference: &SnapshotRef,
) -> Result<State, StorageError> {
    let path = ledger_dir(data_dir).join(&reference.file);
    let metadata =
        fs::metadata(&path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
    if metadata.len() > MAX_SNAPSHOT_BYTES {
        return Err(StorageError::TooLarge { size: metadata.len(), max: MAX_SNAPSHOT_BYTES });
    }
    let bytes =
        fs::read(&path).map_err(|source| StorageError::Io { path: path.clone(), source })?;
    let (payload, checksum) = snapshot_payload(&bytes)?;
    if Hash::new(checksum) != reference.checksum {
        return Err(StorageError::ChecksumMismatch);
    }
    let snapshot: StateSnapshot =
        bincode::deserialize(payload).map_err(|error| StorageError::Snapshot(error.to_string()))?;
    if snapshot.chain_id != chain_id {
        return Err(StorageError::ChainMismatch { expected: chain_id, got: snapshot.chain_id });
    }
    let state = State::from_snapshot(snapshot)
        .map_err(|error| StorageError::Snapshot(error.to_string()))?;
    if state.root() != reference.state_root {
        return Err(StorageError::Snapshot(
            "restored state root does not match the manifest".into(),
        ));
    }
    Ok(state)
}

/// Validate the snapshot envelope and return its payload and checksum.
fn snapshot_payload(bytes: &[u8]) -> Result<(&[u8], [u8; 32]), StorageError> {
    let header_len = 8 + 4 + 32;
    if bytes.len() < header_len {
        return Err(StorageError::Snapshot("snapshot is shorter than its header".into()));
    }
    if bytes[..8] != SNAPSHOT_MAGIC {
        return Err(StorageError::Snapshot("bad snapshot magic".into()));
    }
    let version = u32::from_le_bytes(
        bytes[8..12].try_into().map_err(|_| StorageError::Snapshot("bad header".into()))?,
    );
    if version != SNAPSHOT_VERSION {
        return Err(StorageError::Snapshot(format!("unsupported snapshot version {version}")));
    }
    let checksum: [u8; 32] =
        bytes[12..44].try_into().map_err(|_| StorageError::Snapshot("bad header".into()))?;
    let payload = &bytes[header_len..];
    if *blake3::hash(payload).as_bytes() != checksum {
        return Err(StorageError::ChecksumMismatch);
    }
    Ok((payload, checksum))
}

fn read_segments(
    data_dir: &Path,
    chain_id: ChainId,
    committed: &CommittedLedger,
) -> Result<(Vec<Block>, Vec<Receipt>), StorageError> {
    let dir = segments_dir(data_dir);
    let mut blocks = Vec::new();
    let mut receipts = Vec::new();
    let mut expected_height = committed.first_retained_height()?;

    for entry in &committed.segments {
        let path = dir.join(&entry.file);
        let metadata = fs::metadata(&path)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        if metadata.len() < entry.bytes {
            return Err(StorageError::Segment(format!(
                "{} is {} bytes but the manifest commits {}",
                entry.file,
                metadata.len(),
                entry.bytes
            )));
        }
        let bytes = read_committed(&path, entry.bytes)?;
        let mut cursor = 0usize;
        let (segment_chain_id, first_height) = read_segment_header(&bytes, &mut cursor)?;
        if segment_chain_id != chain_id {
            return Err(StorageError::ChainMismatch { expected: chain_id, got: segment_chain_id });
        }
        if first_height != expected_height {
            return Err(StorageError::Segment(format!(
                "{} starts at height {first_height}, expected {expected_height}",
                entry.file
            )));
        }

        for _ in 0..entry.records {
            let record = read_record(&bytes, &mut cursor)?;
            if record.block.header.height != expected_height {
                return Err(StorageError::Segment(format!(
                    "record at height {} breaks the sequence, expected {expected_height}",
                    record.block.header.height
                )));
            }
            let block_hash = record.block.hash();
            for receipt in &record.receipts {
                if receipt.block_height != expected_height || receipt.block_hash != block_hash {
                    return Err(StorageError::Segment(format!(
                        "receipt does not belong to block {expected_height}"
                    )));
                }
            }
            expected_height = expected_height
                .checked_add(1)
                .ok_or_else(|| StorageError::Segment("height overflow".into()))?;
            blocks.push(record.block);
            receipts.extend(record.receipts);
        }

        if u64::try_from(cursor).ok() != Some(entry.bytes) {
            return Err(StorageError::Segment(format!(
                "{} holds a different byte length than the manifest commits",
                entry.file
            )));
        }
    }
    Ok((blocks, receipts))
}

/// Read exactly the committed prefix of a segment. An uncommitted torn tail is
/// never read, so it cannot influence validation.
fn read_committed(path: &Path, bytes: u64) -> Result<Vec<u8>, StorageError> {
    let max = SEGMENT_MAX_BYTES.saturating_add(u64::from(MAX_RECORD_BYTES));
    if bytes > max {
        return Err(StorageError::Segment(format!(
            "committed segment size {bytes} exceeds the {max} byte bound"
        )));
    }
    let length = usize::try_from(bytes)
        .map_err(|_| StorageError::Segment("segment size does not fit in memory".into()))?;
    let mut file =
        File::open(path).map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    let mut buffer = vec![0u8; length];
    file.read_exact(&mut buffer).map_err(|source| {
        if source.kind() == std::io::ErrorKind::UnexpectedEof {
            StorageError::Segment(format!(
                "{} ends before the committed {bytes} bytes",
                path.display()
            ))
        } else {
            StorageError::Io { path: path.to_path_buf(), source }
        }
    })?;
    Ok(buffer)
}

fn read_segment_header(bytes: &[u8], cursor: &mut usize) -> Result<(ChainId, u64), StorageError> {
    let header = take(bytes, cursor, SEGMENT_HEADER_LEN)?;
    if header[..8] != SEGMENT_MAGIC {
        return Err(StorageError::Segment("bad segment magic".into()));
    }
    let version = u32::from_le_bytes(
        header[8..12].try_into().map_err(|_| StorageError::Segment("bad header".into()))?,
    );
    if version != SEGMENT_VERSION {
        return Err(StorageError::Segment(format!("unsupported segment version {version}")));
    }
    let chain_id = u64::from_le_bytes(
        header[12..20].try_into().map_err(|_| StorageError::Segment("bad header".into()))?,
    );
    let first_height = u64::from_le_bytes(
        header[20..28].try_into().map_err(|_| StorageError::Segment("bad header".into()))?,
    );
    Ok((chain_id, first_height))
}

fn read_record(bytes: &[u8], cursor: &mut usize) -> Result<SegmentRecord, StorageError> {
    let header = take(bytes, cursor, RECORD_HEADER_LEN)?;
    if header[..4] != RECORD_MAGIC {
        return Err(StorageError::Segment("bad record magic".into()));
    }
    let version = u32::from_le_bytes(
        header[4..8].try_into().map_err(|_| StorageError::Segment("bad header".into()))?,
    );
    if version != RECORD_VERSION {
        return Err(StorageError::Segment(format!("unsupported record version {version}")));
    }
    let length = u32::from_le_bytes(
        header[8..12].try_into().map_err(|_| StorageError::Segment("bad header".into()))?,
    );
    if length > MAX_RECORD_BYTES {
        return Err(StorageError::Segment(format!(
            "record is {length} bytes, above the {MAX_RECORD_BYTES} byte limit"
        )));
    }
    let checksum: [u8; 32] =
        header[12..44].try_into().map_err(|_| StorageError::Segment("bad header".into()))?;
    let payload = take(bytes, cursor, u64::from(length))?;
    if *blake3::hash(payload).as_bytes() != checksum {
        return Err(StorageError::ChecksumMismatch);
    }
    bincode::deserialize(payload)
        .map_err(|error| StorageError::Segment(format!("cannot decode record: {error}")))
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: u64) -> Result<&'a [u8], StorageError> {
    let len = usize::try_from(len)
        .map_err(|_| StorageError::Segment("record length does not fit in memory".into()))?;
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| StorageError::Segment("record offset overflow".into()))?;
    let slice =
        bytes.get(*cursor..end).ok_or_else(|| StorageError::Segment("truncated record".into()))?;
    *cursor = end;
    Ok(slice)
}

/// Whether a fresh state snapshot should be written at `head`.
///
/// A snapshot is written when the manifest has none (first persist or a legacy
/// migration) and once `interval` blocks have accumulated past the current one.
/// An interval of zero snapshots on every persist, which the tests use to get a
/// snapshot at an exact height.
fn snapshot_due(head: u64, current: Option<&SnapshotRef>, interval: u64) -> bool {
    match current {
        None => true,
        Some(snapshot) => head >= snapshot.height.saturating_add(interval),
    }
}

/// Append new block records, write the state snapshot when one is due, then
/// commit the manifest.
///
/// Validation replays only the appended suffix when `head_cache` still matches
/// the committed manifest; otherwise the whole offered history is validated.
/// The cache is updated only after the manifest commit succeeds, so a failed
/// persist leaves it describing the head that is actually on disk.
pub(crate) fn persist(
    chain_id: ChainId,
    data_dir: &Path,
    snapshot: &LedgerSnapshot,
    segment_max_bytes: u64,
    snapshot_interval: u64,
    head_cache: &HeadCache,
    supplied: Option<&FeePolicy>,
) -> Result<(), StorageError> {
    let previous = read_previous(chain_id, data_dir)?;
    let base_height = previous.as_ref().map_or(0, |previous| previous.base_height);
    let new_height = snapshot.blocks.last().map_or(0, |block| block.header.height);
    let committed_records = previous.as_ref().map_or(Ok(0), CommittedLedger::records)?;
    let policy = bind_fee_policy(
        previous.as_ref().and_then(|previous| previous.fee_policy.as_ref()),
        supplied,
        &snapshot.blocks,
        previous.is_some(),
        true,
    )?;
    // Validate before any write, against the base the manifest commits to. A
    // full history offered to a pruned store is rejected here rather than being
    // silently re-anchored.
    let verified = match validate_appended_suffix(
        previous.as_ref(),
        head_cache,
        snapshot,
        chain_id,
        committed_records,
        policy.as_ref(),
    )? {
        Some(state) => state,
        None => {
            // A fee-enabled full validation replays from the committed snapshot;
            // a legacy one keeps the commitment-only check and reads nothing.
            let anchor = if snapshot.blocks.iter().any(|block| block.header.version >= 2) {
                committed_anchor(previous.as_ref(), data_dir, chain_id, snapshot, base_height)?
            } else {
                None
            };
            crate::validate_snapshot_from(
                snapshot,
                chain_id,
                base_height,
                policy.as_ref(),
                anchor.as_ref(),
            )?
        }
    };
    check_append_only(previous.as_ref(), snapshot, new_height)?;

    let dir = segments_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|source| StorageError::Io { path: dir.clone(), source })?;

    let mut segments = previous.as_ref().map_or_else(Vec::new, |best| best.segments.clone());
    if committed_records < new_height.saturating_sub(base_height) {
        // Appended blocks start above the last committed *record*; a legacy
        // previous has a head but no records, so every receipt is still new.
        let committed_head = base_height.saturating_add(committed_records);
        let receipts = group_receipts(snapshot, committed_head);
        let mut appender = Appender::open(&dir, &mut segments, segment_max_bytes)?;
        let start = usize::try_from(committed_records)
            .map_err(|_| StorageError::Segment("record count does not fit in memory".into()))?;
        for block in snapshot.blocks.iter().skip(start) {
            let height = block.header.height;
            let record = SegmentRecord {
                block: block.clone(),
                receipts: receipts.get(&height).cloned().unwrap_or_default(),
            };
            appender.append(chain_id, height, &record)?;
        }
        appender.finish()?;
        File::open(&dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StorageError::Io { path: dir.clone(), source })?;
    }

    let previous_snapshot = previous.as_ref().and_then(|best| best.snapshot.clone());
    let snapshot_ref = if snapshot_due(new_height, previous_snapshot.as_ref(), snapshot_interval) {
        let state = State::from_snapshot(snapshot.state.clone())
            .map_err(|error| StorageError::Snapshot(error.to_string()))?;
        write_snapshot(data_dir, new_height, &snapshot.state, state.root())?
    } else {
        // Reuse the committed snapshot and replay the blocks after it on load.
        previous_snapshot
            .clone()
            .ok_or_else(|| StorageError::Manifest("manifest has no snapshot".into()))?
    };
    let committed = CommittedLedger {
        head_height: new_height,
        head_hash: snapshot.blocks.last().map_or(Hash::ZERO, Block::hash),
        base_height,
        fee_policy: policy,
        snapshot: Some(snapshot_ref),
        segments,
    };
    write_manifest(data_dir, chain_id, &committed)?;
    cleanup(data_dir, &committed, previous_snapshot.as_ref());
    // The manifest commit succeeded: the offered head is now the verified head.
    *lock_cache(head_cache) = Some(VerifiedHead {
        base_height,
        height: new_height,
        hash: committed.head_hash,
        state: verified,
    });
    Ok(())
}

/// Validate only the blocks appended on top of the cached verified head.
///
/// Returns `None` when the cache cannot be trusted for this append — no cache,
/// a legacy previous, a different committed head or base, an unchanged or
/// shortened history — and the caller falls back to the full snapshot
/// validation. Once the cache matches, its verdict is final: a suffix the
/// cached state cannot replay is refused, not retried with the weaker full
/// check, which never replays transitions.
///
/// The committed prefix is not re-read or re-validated here: it was fully
/// verified when the cache was populated (load or a previous persist) and is
/// append-only on disk. `check_append_only` still pins the carried head block.
fn validate_appended_suffix(
    previous: Option<&CommittedLedger>,
    head_cache: &HeadCache,
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    committed_records: u64,
    policy: Option<&FeePolicy>,
) -> Result<Option<State>, StorageError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    // A legacy single file has no committed snapshot; this persist rewrites it
    // as segments, so the cache cannot describe what is on disk.
    if previous.snapshot.is_none() {
        return Ok(None);
    }
    let guard = lock_cache(head_cache);
    let Some(cached) = guard.as_ref() else {
        return Ok(None);
    };
    if cached.base_height != previous.base_height
        || cached.height != previous.head_height
        || cached.hash != previous.head_hash
    {
        return Ok(None);
    }
    let start = usize::try_from(committed_records).map_err(|_| {
        StorageError::Invalid("retained record count does not fit in memory".into())
    })?;
    validate_suffix_on(cached, start, snapshot, chain_id, policy)
}

/// Validate the blocks an ephemeral store appends on top of its last successful
/// validation.
///
/// There is no manifest to match against: for an ephemeral store the cache *is*
/// the committed record, so it anchors the suffix on its own. Every other trust
/// condition is the durable one — a missing cache, an unchanged or shortened
/// history, or a rewritten prefix falls back to the full validation.
fn validate_ephemeral_suffix(
    head_cache: &HeadCache,
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    policy: Option<&FeePolicy>,
) -> Result<Option<State>, StorageError> {
    let guard = lock_cache(head_cache);
    let Some(cached) = guard.as_ref() else {
        return Ok(None);
    };
    let Some(retained) = cached.height.checked_sub(cached.base_height) else {
        return Ok(None);
    };
    let start = usize::try_from(retained).map_err(|_| {
        StorageError::Invalid("retained record count does not fit in memory".into())
    })?;
    validate_suffix_on(cached, start, snapshot, chain_id, policy)
}

/// Validate only the append: `start` counts the already-verified blocks the
/// offered snapshot carries, so `snapshot.blocks[start..]` is what is new.
///
/// The parent header comes from the offered snapshot, never from the cache: the
/// block at `start - 1` is the verified head, whose hash the cache already pins.
fn validate_suffix_on(
    cached: &VerifiedHead,
    start: usize,
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    policy: Option<&FeePolicy>,
) -> Result<Option<State>, StorageError> {
    // Only growth is covered here. An unchanged or shortened history keeps the
    // full validation, which must also re-check the offered prefix: genesis
    // seeding legitimately persists a changed state at height 0.
    let Some(suffix) = snapshot.blocks.get(start..) else {
        return Ok(None);
    };
    if suffix.is_empty() || snapshot.chain_id != chain_id || snapshot.state.chain_id != chain_id {
        return Ok(None);
    }

    // `validate_chain_from` skips the first block's parent link when the base
    // height is non-zero, so the link to the verified head is checked here.
    let first = &suffix[0];
    let expected_height = cached
        .height
        .checked_add(1)
        .ok_or_else(|| StorageError::Invalid("block height overflow".into()))?;
    if first.header.height != expected_height || first.header.parent_hash != cached.hash {
        return Err(StorageError::Invalid(format!(
            "block {expected_height} does not extend the verified head"
        )));
    }

    // Replaying the suffix enforces nonce ordering across the whole history:
    // a transaction already committed below the head cannot be applied again.
    // Version 2 blocks also settle their fees here, exactly as the block path
    // does, so the replayed root carries the producer credit.
    let parent = start
        .checked_sub(1)
        .and_then(|index| snapshot.blocks.get(index))
        .map(|block| &block.header);
    let mut state = cached.state.clone();
    replay_blocks(suffix, parent, policy, &mut state, "on the verified head")?;

    let receipts: Vec<Receipt> = snapshot
        .receipts
        .iter()
        .filter(|receipt| receipt.block_height > cached.height)
        .cloned()
        .collect();
    crate::validate_chain_from(cached.height, suffix, &receipts, chain_id, &state)?;

    let offered = State::from_snapshot(snapshot.state.clone())
        .map_err(|error| StorageError::Invalid(error.to_string()))?;
    if offered.root() != state.root() {
        return Err(StorageError::Invalid(
            "offered state does not match the replayed suffix".into(),
        ));
    }
    Ok(Some(state))
}

/// Validate an ephemeral persist and record the head it verified.
///
/// Same sequence as the durable path: suffix validation when the cache anchors
/// the append, full validation otherwise, append-only against the verified head,
/// and the cache updated only after everything succeeded. Nothing is written to
/// disk; the cache is the only record an ephemeral store keeps.
pub(crate) fn validate_ephemeral(
    chain_id: ChainId,
    snapshot: &LedgerSnapshot,
    head_cache: &HeadCache,
    supplied: Option<&FeePolicy>,
) -> Result<(), StorageError> {
    // Nothing is recorded on disk, so the supplied policy is the only one; the
    // binding still refuses a version 2 history offered without any policy.
    let policy = bind_fee_policy(None, supplied, &snapshot.blocks, false, true)?;
    let new_height = snapshot.blocks.last().map_or(0, |block| block.header.height);
    let verified = match validate_ephemeral_suffix(head_cache, snapshot, chain_id, policy.as_ref())?
    {
        Some(state) => state,
        None => {
            // An unchanged or shortened history keeps the full validation. The
            // cache is the only anchor an ephemeral store has, so it serves here
            // exactly as the committed snapshot does for a durable store.
            let anchor = ephemeral_anchor(head_cache, snapshot);
            crate::validate_snapshot_from(snapshot, chain_id, 0, policy.as_ref(), anchor.as_ref())?
        }
    };
    check_ephemeral_append_only(head_cache, snapshot, new_height)?;
    *lock_cache(head_cache) = Some(VerifiedHead {
        base_height: 0,
        height: new_height,
        hash: snapshot.blocks.last().map_or(Hash::ZERO, Block::hash),
        state: verified,
    });
    Ok(())
}

/// The cached head as a replay anchor, when the offered history still carries
/// the block it names. Used only by the ephemeral full validation.
fn ephemeral_anchor(
    head_cache: &HeadCache,
    snapshot: &LedgerSnapshot,
) -> Option<crate::ReplayAnchor> {
    let guard = lock_cache(head_cache);
    let cached = guard.as_ref()?;
    let header = if cached.height == 0 {
        None
    } else {
        let index = cached
            .height
            .checked_sub(cached.base_height)
            .and_then(|retained| retained.checked_sub(1))
            .and_then(|offset| usize::try_from(offset).ok())?;
        let block = snapshot.blocks.get(index)?;
        if block.header.height != cached.height || block.hash() != cached.hash {
            return None;
        }
        Some(block.header.clone())
    };
    Some(crate::ReplayAnchor { state: cached.state.clone(), height: cached.height, header })
}

/// The committed snapshot's state, plus the header of the block at its height
/// when the offered history still carries it.
///
/// This is the replay anchor for a fee-enabled full validation whose head cache
/// cannot serve: the snapshot is the only intermediate state storage commits to,
/// and `prune` guarantees the block at its height is retained.
fn committed_anchor(
    previous: Option<&CommittedLedger>,
    data_dir: &Path,
    chain_id: ChainId,
    snapshot: &LedgerSnapshot,
    base_height: u64,
) -> Result<Option<crate::ReplayAnchor>, StorageError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    let Some(committed_snapshot) = previous.snapshot.as_ref() else {
        return Ok(None);
    };
    let state = read_snapshot(data_dir, chain_id, committed_snapshot)?;
    let header = if committed_snapshot.height == 0 {
        None
    } else {
        let index = committed_snapshot
            .height
            .checked_sub(base_height)
            .and_then(|offset| offset.checked_sub(1))
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| {
                StorageError::Invalid("committed snapshot height does not fit in memory".into())
            })?;
        let anchor = snapshot.blocks.get(index).ok_or_else(|| {
            StorageError::Invalid(
                "the offered history does not carry the committed snapshot anchor".into(),
            )
        })?;
        if anchor.header.state_root != committed_snapshot.state_root {
            return Err(StorageError::Invalid(
                "offered anchor block does not match the committed snapshot root".into(),
            ));
        }
        Some(anchor.header.clone())
    };
    Ok(Some(crate::ReplayAnchor { state, height: committed_snapshot.height, header }))
}

/// Read whatever history the directory currently commits to: the manifest if it
/// exists, otherwise a legacy single file.
fn read_previous(
    chain_id: ChainId,
    data_dir: &Path,
) -> Result<Option<CommittedLedger>, StorageError> {
    if manifest_path(data_dir).is_file() {
        return read_manifest(data_dir, chain_id).map(Some);
    }
    let legacy_path = data_dir.join(LEDGER_FILE_NAME);
    if !legacy_path.is_file() {
        return Ok(None);
    }
    let legacy = crate::legacy::read(&legacy_path)?;
    if legacy.chain_id != chain_id {
        return Err(StorageError::ChainMismatch { expected: chain_id, got: legacy.chain_id });
    }
    Ok(Some(CommittedLedger {
        head_height: legacy.blocks.last().map_or(0, |block| block.header.height),
        head_hash: legacy.blocks.last().map_or(Hash::ZERO, Block::hash),
        base_height: 0,
        fee_policy: None,
        snapshot: None,
        segments: Vec::new(),
    }))
}

/// Refuse to rewrite committed history. Hash linkage makes the head comparison
/// sufficient: a block that hashes to the committed head carries the same
/// ancestors.
fn check_append_only(
    previous: Option<&CommittedLedger>,
    snapshot: &LedgerSnapshot,
    new_height: u64,
) -> Result<(), StorageError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    check_head_unchanged(
        previous.head_height,
        previous.base_height,
        previous.head_hash,
        snapshot,
        new_height,
    )
}

/// The ephemeral counterpart: the verified head is the only committed record, so
/// it is what a rewrite would have to contradict.
fn check_ephemeral_append_only(
    head_cache: &HeadCache,
    snapshot: &LedgerSnapshot,
    new_height: u64,
) -> Result<(), StorageError> {
    let guard = lock_cache(head_cache);
    let Some(cached) = guard.as_ref() else {
        return Ok(());
    };
    check_head_unchanged(cached.height, cached.base_height, cached.hash, snapshot, new_height)
}

fn check_head_unchanged(
    head_height: u64,
    base_height: u64,
    head_hash: Hash,
    snapshot: &LedgerSnapshot,
    new_height: u64,
) -> Result<(), StorageError> {
    if new_height < head_height {
        return Err(StorageError::NonAppendPersist);
    }
    if head_height > 0 {
        let index = head_height
            .checked_sub(base_height)
            .and_then(|offset| offset.checked_sub(1))
            .ok_or(StorageError::NonAppendPersist)?;
        let index = usize::try_from(index)
            .map_err(|_| StorageError::Invalid("block height overflow".into()))?;
        let carried = snapshot.blocks.get(index).ok_or(StorageError::NonAppendPersist)?;
        if carried.header.height != head_height || carried.hash() != head_hash {
            return Err(StorageError::NonAppendPersist);
        }
    }
    Ok(())
}

/// Drop the oldest segments, keeping the newest `keep_segments`.
///
/// The snapshot is the replay anchor, so a prune that would drop the block at the
/// snapshot's own height is refused before anything is written. The manifest is
/// the commit point: segment files are deleted only after it no longer names
/// them, so an interrupted prune leaves a loadable ledger.
pub(crate) fn prune(
    chain_id: ChainId,
    data_dir: &Path,
    keep_segments: usize,
) -> Result<PruneOutcome, StorageError> {
    let committed = read_manifest(data_dir, chain_id)?;
    let snapshot = committed
        .snapshot
        .clone()
        .ok_or_else(|| StorageError::Manifest("manifest has no snapshot".into()))?;
    if committed.segments.len() <= keep_segments {
        return Ok(PruneOutcome::NothingToPrune { segments: committed.segments.len() });
    }

    let dropped_segments = committed.segments.len() - keep_segments;
    let dropped_records =
        committed.segments[..dropped_segments].iter().try_fold(0u64, |total, segment| {
            total
                .checked_add(segment.records)
                .ok_or_else(|| StorageError::Manifest("segment record count overflow".into()))
        })?;
    let base_height = committed
        .base_height
        .checked_add(dropped_records)
        .ok_or_else(|| StorageError::Manifest("base height overflow".into()))?;
    let first_retained_height = base_height
        .checked_add(1)
        .ok_or_else(|| StorageError::Manifest("base height overflow".into()))?;
    if first_retained_height > snapshot.height {
        return Err(StorageError::PruneWouldDropSnapshot {
            first_retained_height,
            snapshot_height: snapshot.height,
        });
    }

    let pruned = CommittedLedger {
        head_height: committed.head_height,
        head_hash: committed.head_hash,
        base_height,
        fee_policy: committed.fee_policy.clone(),
        snapshot: Some(snapshot.clone()),
        segments: committed.segments[dropped_segments..].to_vec(),
    };
    let covered = pruned.base_height.checked_add(pruned.records()?);
    if covered != Some(pruned.head_height) {
        return Err(StorageError::Manifest(
            "pruned segments do not cover the committed head".into(),
        ));
    }

    write_manifest(data_dir, chain_id, &pruned)?;
    cleanup(data_dir, &pruned, Some(&snapshot));
    debug!(
        base_height,
        dropped_segments,
        retained_segments = keep_segments,
        "pruned ledger segments"
    );
    Ok(PruneOutcome::Pruned { dropped_segments, retained_segments: keep_segments, base_height })
}

/// Group the receipts of the blocks this persist appends (heights above
/// `previous_head`); receipts of the committed prefix are never rewritten.
fn group_receipts(snapshot: &LedgerSnapshot, previous_head: u64) -> HashMap<u64, Vec<Receipt>> {
    let mut by_height: HashMap<u64, Vec<Receipt>> = HashMap::new();
    for receipt in &snapshot.receipts {
        if receipt.block_height > previous_head {
            by_height.entry(receipt.block_height).or_default().push(receipt.clone());
        }
    }
    for receipts in by_height.values_mut() {
        receipts.sort_by_key(|receipt| receipt.transaction_index);
    }
    by_height
}

/// Append-only writer for one persist. It truncates any uncommitted tail of the
/// last committed segment before appending, and rolls to a new file when the
/// current one would exceed the target size.
struct Appender<'a> {
    dir: &'a Path,
    entries: &'a mut Vec<SegmentRef>,
    max_bytes: u64,
    current: Option<CurrentSegment>,
}

struct CurrentSegment {
    index: u64,
    records: u64,
    bytes: u64,
    file: File,
}

impl<'a> Appender<'a> {
    fn open(
        dir: &'a Path,
        entries: &'a mut Vec<SegmentRef>,
        max_bytes: u64,
    ) -> Result<Self, StorageError> {
        let current = match entries.last() {
            Some(last) => {
                let index = segment_index(&last.file).ok_or_else(|| {
                    StorageError::Segment(format!("segment name {:?} is invalid", last.file))
                })?;
                let path = dir.join(&last.file);
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| StorageError::Io { path: path.clone(), source })?;
                let actual_bytes = file
                    .metadata()
                    .map_err(|source| StorageError::Io { path: path.clone(), source })?
                    .len();
                if actual_bytes < last.bytes {
                    return Err(StorageError::Segment(format!(
                        "{} is {actual_bytes} bytes but the manifest commits {}",
                        last.file, last.bytes
                    )));
                }
                // Drop a torn append that the manifest never committed.
                // Never extend a shortened committed file: set_len would pad
                // missing data with zeroes and let an unreadable head commit.
                file.set_len(last.bytes)
                    .map_err(|source| StorageError::Io { path: path.clone(), source })?;
                file.seek(SeekFrom::End(0))
                    .map_err(|source| StorageError::Io { path: path.clone(), source })?;
                Some(CurrentSegment { index, records: last.records, bytes: last.bytes, file })
            }
            None => None,
        };
        Ok(Self { dir, entries, max_bytes, current })
    }

    fn append(
        &mut self,
        chain_id: ChainId,
        first_height: u64,
        record: &SegmentRecord,
    ) -> Result<(), StorageError> {
        let payload =
            bincode::serialize(record).map_err(|error| StorageError::Encode(error.to_string()))?;
        let length = u32::try_from(payload.len())
            .map_err(|_| StorageError::Segment("record does not fit in a segment".into()))?;
        if length > MAX_RECORD_BYTES {
            return Err(StorageError::Segment(format!(
                "record is {length} bytes, above the {MAX_RECORD_BYTES} byte limit"
            )));
        }
        let total = RECORD_HEADER_LEN
            .checked_add(u64::from(length))
            .ok_or_else(|| StorageError::Segment("record length overflow".into()))?;

        let roll = match &self.current {
            None => true,
            Some(current) => {
                current.records > 0 && current.bytes.saturating_add(total) > self.max_bytes
            }
        };
        if roll {
            self.commit_current()?;
            // Segment names stay monotonically increasing after a prune, so the
            // new file continues from the last committed one, not from `len()`.
            let index = match self.entries.last() {
                Some(last) => segment_index(&last.file)
                    .and_then(|index| index.checked_add(1))
                    .ok_or_else(|| StorageError::Segment("segment index overflow".into()))?,
                None => 0,
            };
            self.current = Some(create_segment(self.dir, index, chain_id, first_height)?);
        }

        let index = self
            .current
            .as_ref()
            .map(|current| current.index)
            .ok_or_else(|| StorageError::Segment("segment writer was not opened".into()))?;
        let path = self.segment_path(index);
        let current = self
            .current
            .as_mut()
            .ok_or_else(|| StorageError::Segment("segment writer was not opened".into()))?;
        let mut framed = Vec::with_capacity(payload.len().saturating_add(44));
        framed.extend_from_slice(&RECORD_MAGIC);
        framed.extend_from_slice(&RECORD_VERSION.to_le_bytes());
        framed.extend_from_slice(&length.to_le_bytes());
        framed.extend_from_slice(blake3::hash(&payload).as_bytes());
        framed.extend_from_slice(&payload);
        current
            .file
            .write_all(&framed)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        current.records = current
            .records
            .checked_add(1)
            .ok_or_else(|| StorageError::Segment("record count overflow".into()))?;
        current.bytes = current
            .bytes
            .checked_add(total)
            .ok_or_else(|| StorageError::Segment("segment size overflow".into()))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), StorageError> {
        self.commit_current()
    }

    fn commit_current(&mut self) -> Result<(), StorageError> {
        let Some(current) = self.current.as_ref() else {
            return Ok(());
        };
        let path = self.segment_path(current.index);
        current
            .file
            .sync_all()
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        let entry = SegmentRef {
            file: segment_file_name(current.index),
            records: current.records,
            bytes: current.bytes,
        };
        match self.entries.last_mut() {
            Some(last) if last.file == entry.file => *last = entry,
            Some(last) => {
                let last_index = segment_index(&last.file)
                    .ok_or_else(|| StorageError::Segment("segment name is invalid".into()))?;
                if current.index <= last_index {
                    return Err(StorageError::Segment("segment index is not increasing".into()));
                }
                self.entries.push(entry);
            }
            None => self.entries.push(entry),
        }
        Ok(())
    }

    fn segment_path(&self, index: u64) -> PathBuf {
        self.dir.join(segment_file_name(index))
    }
}

fn create_segment(
    dir: &Path,
    index: u64,
    chain_id: ChainId,
    first_height: u64,
) -> Result<CurrentSegment, StorageError> {
    let path = dir.join(segment_file_name(index));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|source| StorageError::Io { path: path.clone(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
    }
    let mut header = Vec::with_capacity(usize::try_from(SEGMENT_HEADER_LEN).unwrap_or(0));
    header.extend_from_slice(&SEGMENT_MAGIC);
    header.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
    header.extend_from_slice(&chain_id.to_le_bytes());
    header.extend_from_slice(&first_height.to_le_bytes());
    file.write_all(&header).map_err(|source| StorageError::Io { path: path.clone(), source })?;
    Ok(CurrentSegment { index, records: 0, bytes: SEGMENT_HEADER_LEN, file })
}

/// Serialize a state snapshot into the envelope: magic, version, BLAKE3, payload.
fn encode_snapshot(state: &StateSnapshot) -> Result<(Vec<u8>, Hash), StorageError> {
    let payload =
        bincode::serialize(state).map_err(|error| StorageError::Encode(error.to_string()))?;
    let checksum = *blake3::hash(&payload).as_bytes();
    let mut bytes = Vec::with_capacity(44 + payload.len());
    bytes.extend_from_slice(&SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&checksum);
    bytes.extend_from_slice(&payload);
    let size = u64::try_from(bytes.len())
        .map_err(|_| StorageError::Snapshot("snapshot size overflow".into()))?;
    if size > MAX_SNAPSHOT_BYTES {
        return Err(StorageError::TooLarge { size, max: MAX_SNAPSHOT_BYTES });
    }
    Ok((bytes, Hash::new(checksum)))
}

fn write_snapshot(
    data_dir: &Path,
    height: u64,
    state: &StateSnapshot,
    state_root: Hash,
) -> Result<SnapshotRef, StorageError> {
    let (bytes, checksum) = encode_snapshot(state)?;
    let file = snapshot_file_name(height);
    write_atomic(&ledger_dir(data_dir).join(&file), &bytes)?;
    Ok(SnapshotRef { file, height, state_root, checksum })
}

/// Test-only: replace the snapshot payload at `height` and return its new
/// checksum, so a test can keep a manifest internally consistent while making
/// the snapshot disagree with its block.
#[cfg(test)]
pub(crate) fn overwrite_snapshot(
    data_dir: &Path,
    height: u64,
    state: &StateSnapshot,
) -> Result<Hash, StorageError> {
    let (bytes, checksum) = encode_snapshot(state)?;
    write_atomic(&ledger_dir(data_dir).join(snapshot_file_name(height)), &bytes)?;
    Ok(checksum)
}

fn write_manifest(
    data_dir: &Path,
    chain_id: ChainId,
    committed: &CommittedLedger,
) -> Result<(), StorageError> {
    let snapshot = committed.snapshot.as_ref().ok_or_else(|| {
        StorageError::Manifest("cannot write a manifest without a snapshot".into())
    })?;
    let manifest = ManifestFile {
        version: MANIFEST_VERSION,
        chain_id,
        head_height: committed.head_height,
        head_hash: committed.head_hash.to_string(),
        base_height: committed.base_height,
        fee_policy: committed.fee_policy.clone(),
        snapshot: SnapshotFile {
            file: snapshot.file.clone(),
            height: snapshot.height,
            state_root: snapshot.state_root.to_string(),
            checksum: snapshot.checksum.to_string(),
        },
        segments: committed
            .segments
            .iter()
            .map(|segment| SegmentFile {
                file: segment.file.clone(),
                records: segment.records,
                bytes: segment.bytes,
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| StorageError::Encode(error.to_string()))?;
    bytes.push(b'\n');
    let size = u64::try_from(bytes.len())
        .map_err(|_| StorageError::Manifest("manifest too large".into()))?;
    if size > MAX_MANIFEST_BYTES {
        return Err(StorageError::TooLarge { size, max: MAX_MANIFEST_BYTES });
    }
    write_atomic(&manifest_path(data_dir), &bytes)
}

/// Write `bytes` to a sibling temporary file, fsync it, rename it over `path`,
/// then fsync the directory so the rename itself is durable.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().ok_or(StorageError::MissingParentDirectory)?;
    fs::create_dir_all(parent)
        .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
    temp.write_all(bytes)
        .map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    temp.as_file()
        .sync_all()
        .map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    }
    temp.persist(path)
        .map_err(|error| StorageError::Io { path: path.to_path_buf(), source: error.error })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
    Ok(())
}

/// Drop files the new manifest does not reference. The previous snapshot is kept
/// as well, so a manifest that was never replaced still points at live data.
/// Cleanup failures never fail a persist that is already committed.
fn cleanup(data_dir: &Path, committed: &CommittedLedger, previous: Option<&SnapshotRef>) {
    let keep_snapshot = committed.snapshot.as_ref().map(|snapshot| snapshot.file.as_str());
    let keep_previous = previous.map(|snapshot| snapshot.file.as_str());
    let ledger = ledger_dir(data_dir);
    if let Ok(entries) = fs::read_dir(&ledger) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stale_snapshot = name.starts_with("snapshot-")
                && name.ends_with(".bin")
                && Some(name) != keep_snapshot
                && Some(name) != keep_previous;
            if stale_snapshot || name.starts_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    let keep_segments: HashSet<&str> =
        committed.segments.iter().map(|segment| segment.file.as_str()).collect();
    if let Ok(entries) = fs::read_dir(segments_dir(data_dir)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let is_segment = name.len() == 10
                && name.ends_with(".log")
                && name.as_bytes()[..6].iter().all(u8::is_ascii_digit);
            if (is_segment && !keep_segments.contains(name)) || name.starts_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    // Keep the migrated version 2 file as a backup; never load it again once a
    // manifest exists.
    let legacy = data_dir.join(LEDGER_FILE_NAME);
    if legacy.is_file() {
        let backup = data_dir.join(LEDGER_BACKUP_FILE_NAME);
        if let Err(error) = fs::rename(&legacy, &backup) {
            debug!(path = %legacy.display(), %error, "could not move the legacy ledger aside");
        }
    }
}
