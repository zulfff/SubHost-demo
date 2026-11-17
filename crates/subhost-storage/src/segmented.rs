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
//! A persist appends only the blocks that are new, fsyncs the segment, rewrites
//! the (bounded) state snapshot, and finally replaces the manifest atomically.
//! Bytes past the manifest's committed length are an interrupted append: they are
//! ignored on load and truncated before the next append, so a torn tail can never
//! prevent start-up from the last committed head.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use subhost_core::{Block, ChainId, Hash, Receipt};
use subhost_state::{State, StateSnapshot};
use tracing::debug;

use crate::{
    LedgerSnapshot, RestoredLedger, StorageError, LEDGER_BACKUP_FILE_NAME, LEDGER_DIR_NAME,
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
const MANIFEST_VERSION: u32 = 1;
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

    if manifest.version != MANIFEST_VERSION {
        return Err(StorageError::UnsupportedManifestVersion(manifest.version));
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
    for (index, entry) in manifest.segments.iter().enumerate() {
        let index =
            u64::try_from(index).map_err(|_| StorageError::Manifest("too many segments".into()))?;
        if entry.file != segment_file_name(index) {
            return Err(StorageError::Manifest(format!(
                "segment file name {:?} does not match index {index}",
                entry.file
            )));
        }
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
        snapshot: Some(snapshot),
        segments,
    };
    let records = committed.records()?;
    if records != committed.head_height {
        return Err(StorageError::Manifest(format!(
            "segments hold {records} records but the head height is {}",
            committed.head_height
        )));
    }
    if committed.head_height == 0
        && (committed.head_hash != Hash::ZERO || !committed.segments.is_empty())
    {
        return Err(StorageError::Manifest(
            "an empty ledger must have a zero head and no segments".into(),
        ));
    }
    if let Some(snapshot) = &committed.snapshot {
        if snapshot.height > committed.head_height {
            return Err(StorageError::Manifest(format!(
                "snapshot height {} is ahead of the head height {}",
                snapshot.height, committed.head_height
            )));
        }
    }
    Ok(committed)
}

fn parse_hash(value: &str, field: &str) -> Result<Hash, StorageError> {
    Hash::from_hex(value)
        .map_err(|error| StorageError::Manifest(format!("invalid {field}: {error}")))
}

/// Load and fully validate the segmented ledger.
pub(crate) fn load(chain_id: ChainId, data_dir: &Path) -> Result<RestoredLedger, StorageError> {
    let committed = read_manifest(data_dir, chain_id)?;
    let snapshot = committed
        .snapshot
        .as_ref()
        .ok_or_else(|| StorageError::Manifest("manifest has no snapshot".into()))?;
    let mut state = read_snapshot(data_dir, chain_id, snapshot)?;
    let (blocks, receipts) = read_segments(data_dir, chain_id, &committed)?;

    let height = u64::try_from(blocks.len()).ok().filter(|height| *height == committed.head_height);
    let head_hash = blocks.last().map_or(Hash::ZERO, Block::hash);
    if height.is_none() || head_hash != committed.head_hash {
        return Err(StorageError::Manifest(
            "segment history does not match the committed head".into(),
        ));
    }

    // The snapshot must be rooted in the block at its own height, not merely be
    // internally consistent: otherwise a stale or mismatched snapshot could pass
    // the manifest checksum and still replay onto the wrong history.
    if snapshot.height > 0 {
        let index = usize::try_from(snapshot.height - 1)
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

    // Replay only the blocks after the snapshot. `validate_chain` still checks the
    // full history's commitments, so a truncated prefix cannot hide a rewrite.
    let replay_from = usize::try_from(snapshot.height)
        .map_err(|_| StorageError::Manifest("snapshot height does not fit in memory".into()))?;
    for block in blocks.iter().skip(replay_from) {
        for transaction in &block.transactions {
            state.apply_transaction(transaction).map_err(|error| {
                StorageError::Snapshot(format!(
                    "cannot replay block {} on the snapshot: {error}",
                    block.header.height
                ))
            })?;
        }
    }

    crate::validate_chain(&blocks, &receipts, chain_id, &state)?;
    debug!(
        blocks = blocks.len(),
        receipts = receipts.len(),
        replayed = blocks.len().saturating_sub(replay_from),
        "restored segmented ledger"
    );
    Ok(RestoredLedger { state, blocks, receipts })
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
    let mut expected_height = 1u64;

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
pub(crate) fn persist(
    chain_id: ChainId,
    data_dir: &Path,
    snapshot: &LedgerSnapshot,
    segment_max_bytes: u64,
    snapshot_interval: u64,
) -> Result<(), StorageError> {
    let previous = read_previous(chain_id, data_dir)?;
    let new_height = snapshot.blocks.last().map_or(0, |block| block.header.height);
    check_append_only(previous.as_ref(), snapshot, new_height)?;

    let committed_records = previous.as_ref().map_or(Ok(0), CommittedLedger::records)?;
    let dir = segments_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|source| StorageError::Io { path: dir.clone(), source })?;

    let mut segments = previous.as_ref().map_or_else(Vec::new, |best| best.segments.clone());
    if committed_records < new_height {
        let receipts = group_receipts(snapshot);
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
        snapshot: Some(snapshot_ref),
        segments,
    };
    write_manifest(data_dir, chain_id, &committed)?;
    cleanup(data_dir, &committed, previous_snapshot.as_ref());
    Ok(())
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
    if new_height < previous.head_height {
        return Err(StorageError::NonAppendPersist);
    }
    if previous.head_height > 0 {
        let index = usize::try_from(previous.head_height - 1)
            .map_err(|_| StorageError::Invalid("block height overflow".into()))?;
        let carried = snapshot.blocks.get(index).ok_or(StorageError::NonAppendPersist)?;
        if carried.hash() != previous.head_hash {
            return Err(StorageError::NonAppendPersist);
        }
    }
    Ok(())
}

fn group_receipts(snapshot: &LedgerSnapshot) -> HashMap<u64, Vec<Receipt>> {
    let mut by_height: HashMap<u64, Vec<Receipt>> = HashMap::new();
    for receipt in &snapshot.receipts {
        by_height.entry(receipt.block_height).or_default().push(receipt.clone());
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
                let index = u64::try_from(entries.len() - 1)
                    .map_err(|_| StorageError::Segment("too many segments".into()))?;
                let path = dir.join(&last.file);
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| StorageError::Io { path: path.clone(), source })?;
                // Drop a torn append that the manifest never committed.
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
            let index = u64::try_from(self.entries.len())
                .map_err(|_| StorageError::Segment("too many segments".into()))?;
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
        let index = usize::try_from(current.index)
            .map_err(|_| StorageError::Segment("segment index does not fit in memory".into()))?;
        if index < self.entries.len() {
            self.entries[index] = entry;
        } else if index == self.entries.len() {
            self.entries.push(entry);
        } else {
            return Err(StorageError::Segment("segment index is not contiguous".into()));
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
