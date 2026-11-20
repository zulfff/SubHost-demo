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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;
use subhost_core::{
    encode_canonical, settle_block_fees, tx_root_of, Address, Block, BlockHeader, CalldataPolicy,
    ChainId, FeePolicy, Gas, Hash, Receipt, ReceiptStatus, CALLDATA_HEADER_VERSION,
    FEE_HEADER_VERSION,
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
    /// The newest snapshot anchor is at or below the requested height, so
    /// pruning would drop the block the replay anchor is rooted in. Nothing was
    /// written. This is a normal outcome of automatic retention, not a fault:
    /// the caller logs and counts it and never fails a block commit (Q-036).
    RefusedBySnapshot { snapshot_height: u64, requested_height: u64 },
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

/// The newest head this process fully verified, kept so an append validates
/// only the blocks it adds instead of replaying the whole retained history.
///
/// Two trust sources, one per store kind:
/// - **durable**: trusted only while it matches the committed manifest (base
///   height, head height, head hash). Any mismatch — a pruned store, a
///   rolled-back manifest, a fresh process — falls back to full validation;
/// - **ephemeral**: there is no manifest, so the cache *is* the committed
///   record. It is trusted as the last successful in-process validation, and
///   only an append that strictly extends it takes the suffix path.
///
/// Either way the cache is written only after a validation succeeded, and an
/// unchanged or shortened history always keeps the full validation.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedHead {
    pub base_height: u64,
    pub height: u64,
    pub hash: Hash,
    pub state: State,
}

pub(crate) type HeadCache = Mutex<Option<VerifiedHead>>;

/// Lock the head cache, recovering from a poisoned mutex: the cache is a plain
/// verified snapshot with no torn-state invariant, and a panic elsewhere must
/// not turn every later persist into a crash.
pub(crate) fn lock_cache(cache: &HeadCache) -> MutexGuard<'_, Option<VerifiedHead>> {
    cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A ledger store bound to one chain ID and one data directory.
///
/// [`LedgerStore::ephemeral`] yields a store that validates exactly like the
/// durable one but discards writes, which keeps in-memory and on-disk nodes on
/// the same code path.
///
/// Clones share the verified-head cache: they describe the same directory, and
/// the node serializes persist/prune under its commit lock (single writer).
#[derive(Debug, Clone)]
pub struct LedgerStore {
    path: Option<PathBuf>,
    chain_id: ChainId,
    head: Arc<HeadCache>,
    fee_policy: Option<FeePolicy>,
    calldata_policy: Option<CalldataPolicy>,
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
        Ok(Self {
            path: Some(data_dir.join(LEDGER_FILE_NAME)),
            chain_id,
            head: Arc::new(Mutex::new(None)),
            fee_policy: None,
            calldata_policy: None,
        })
    }

    /// A store that performs every validation but never touches the filesystem.
    pub fn ephemeral(chain_id: ChainId) -> Result<Self, StorageError> {
        if chain_id == 0 {
            return Err(StorageError::InvalidChainId);
        }
        Ok(Self {
            path: None,
            chain_id,
            head: Arc::new(Mutex::new(None)),
            fee_policy: None,
            calldata_policy: None,
        })
    }

    /// Bind the genesis fee policy this store replays version 2 blocks with.
    ///
    /// Attach it after `open`/`ephemeral` and before the first `load`/`persist`:
    /// the node knows the policy from genesis, storage never reads a genesis
    /// file. A durable store records it in the manifest at the first persist and
    /// refuses any later change; offline tools pass nothing and use the recorded
    /// policy. Clones carry it by value, like the chain id.
    ///
    /// Structural validation needs the chain's block gas limit, which only the
    /// anchor or first header supplies, so it happens during replay
    /// (`FeePolicy::initial_context`/`next_context`), not here.
    pub fn with_fee_policy(mut self, policy: FeePolicy) -> Result<Self, StorageError> {
        self.fee_policy = Some(policy);
        Ok(self)
    }

    /// The policy this store was opened with, if any.
    pub fn fee_policy(&self) -> Option<&FeePolicy> {
        self.fee_policy.as_ref()
    }

    /// Bind the genesis calldata schedule this store replays version 3 blocks
    /// with (Q-037 L3). Mirrors [`Self::with_fee_policy`]: attach after
    /// `open`/`ephemeral` and before the first `load`/`persist`; a durable store
    /// records it in the manifest at the first persist and refuses any later
    /// change; offline tools pass nothing and use the recorded schedule. A
    /// calldata-bound chain is v3 from block 1, with or without a fee policy.
    ///
    /// Structural validation needs the chain's block gas limit, which only the
    /// anchor or first header supplies, so it happens during replay
    /// (`CalldataPolicy::validate`), not here.
    pub fn with_calldata_policy(mut self, policy: CalldataPolicy) -> Result<Self, StorageError> {
        self.calldata_policy = Some(policy);
        Ok(self)
    }

    /// The calldata schedule this store was opened with, if any.
    pub fn calldata_policy(&self) -> Option<&CalldataPolicy> {
        self.calldata_policy.as_ref()
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
    ///
    /// A successful load fully verifies the retained history, so its head is
    /// cached for suffix-only validation of later appends.
    pub fn load(&self) -> Result<RestoredLedger, StorageError> {
        let restored = match &self.path {
            None => self.empty(),
            Some(path) => {
                let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
                if segmented::manifest_path(data_dir).is_file() {
                    segmented::load(
                        self.chain_id,
                        data_dir,
                        self.fee_policy.as_ref(),
                        self.calldata_policy.as_ref(),
                    )?
                } else if !path.is_file() {
                    self.empty()
                } else {
                    self.restore(legacy::read(path)?)?
                }
            }
        };
        *lock_cache(&self.head) = Some(VerifiedHead {
            base_height: restored.base_height,
            height: restored.height(),
            hash: restored.blocks.last().map_or(Hash::ZERO, Block::hash),
            state: restored.state.clone(),
        });
        Ok(restored)
    }

    /// Validate a decoded snapshot and rebuild the in-memory ledger from it.
    pub fn restore(&self, snapshot: LedgerSnapshot) -> Result<RestoredLedger, StorageError> {
        let state = validate_snapshot(
            &snapshot,
            self.chain_id,
            self.fee_policy.as_ref(),
            self.calldata_policy.as_ref(),
        )?;
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
            // No filesystem, but the same validation sequence as a durable
            // store: suffix on the verified head when it anchors the append,
            // full validation otherwise, and the head recorded only on success.
            return segmented::validate_ephemeral(
                self.chain_id,
                snapshot,
                &self.head,
                self.fee_policy.as_ref(),
                self.calldata_policy.as_ref(),
            );
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        fs::create_dir_all(data_dir)
            .map_err(|source| StorageError::Io { path: data_dir.to_path_buf(), source })?;
        // `segmented::persist` reads the committed base from the manifest and
        // validates the retained suffix against it before writing anything.
        segmented::persist(
            self.chain_id,
            data_dir,
            snapshot,
            segment_max_bytes,
            snapshot_interval,
            &self.head,
            self.fee_policy.as_ref(),
            self.calldata_policy.as_ref(),
        )
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

    /// Prune whole segments so that only blocks above `height` are retained.
    ///
    /// The height-based, online-safe counterpart of [`Self::prune`]: the node
    /// knows a height, not a file count. Segments are dropped whole, so the new
    /// `base_height` is the last segment boundary at or below `height` — a
    /// segment straddling `height` is retained because it holds blocks above it.
    ///
    /// **The newest snapshot is the hard floor.** A request at or above
    /// `newest_snapshot_height` returns
    /// [`PruneOutcome::RefusedBySnapshot`] without writing anything, because the
    /// replay anchor's own block must stay retained. With snapshots every 1024
    /// blocks the honest retention bound is `n + up to 1023 blocks`, not `n`.
    ///
    /// The caller serializes this with [`Self::persist`]/[`Self::load`] under the
    /// node's commit lock (single writer). On success the verified-head cache's
    /// base is refreshed in place, so a warm append still takes the suffix path;
    /// the head height, hash and state are unchanged by retention.
    pub fn prune_to_height(&self, height: u64) -> Result<PruneOutcome, StorageError> {
        let Some(path) = &self.path else {
            return Err(StorageError::Invalid("an ephemeral store has no ledger to prune".into()));
        };
        let data_dir = path.parent().ok_or(StorageError::MissingParentDirectory)?;
        if !segmented::manifest_path(data_dir).is_file() {
            return Err(StorageError::Invalid("there is no segmented ledger to prune".into()));
        }
        segmented::prune_to_height(self.chain_id, data_dir, height, &self.head)
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

/// Where a full replay starts: an already-verified state, the height it belongs
/// to, and that block's header when one exists (height 0 is the genesis state).
pub(crate) struct ReplayAnchor {
    pub state: State,
    pub height: u64,
    pub header: Option<BlockHeader>,
}

/// Validate a snapshot against the configured chain and rebuild its state.
///
/// No anchor is available here (ephemeral store, legacy restore), so a version 2
/// history is refused: replay cannot reconstruct the genesis state on its own.
pub(crate) fn validate_snapshot(
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    policy: Option<&FeePolicy>,
    schedule: Option<&CalldataPolicy>,
) -> Result<State, StorageError> {
    validate_snapshot_from(snapshot, chain_id, 0, policy, schedule, None)
}

/// Validate a snapshot whose history starts after `base_height`.
///
/// `anchor` is the already-verified state at a retained height. A version 2
/// history is re-executed from it and settled per block, so the offered root is
/// checked against a replayed one rather than trusted. A legacy history keeps
/// the commitment-only check: no execution, no settlement.
fn validate_snapshot_from(
    snapshot: &LedgerSnapshot,
    chain_id: ChainId,
    base_height: u64,
    policy: Option<&FeePolicy>,
    schedule: Option<&CalldataPolicy>,
    anchor: Option<&ReplayAnchor>,
) -> Result<State, StorageError> {
    if snapshot.chain_id != chain_id || snapshot.state.chain_id != chain_id {
        return Err(StorageError::ChainMismatch { expected: chain_id, got: snapshot.chain_id });
    }
    let offered = State::from_snapshot(snapshot.state.clone())
        .map_err(|error| StorageError::Invalid(error.to_string()))?;
    let fee_enabled =
        snapshot.blocks.iter().any(|block| block.header.version >= FEE_HEADER_VERSION);
    let state = if fee_enabled {
        let anchor = anchor.ok_or_else(|| {
            StorageError::Invalid(
                "a version 2+ ledger needs a committed state anchor: persist the genesis state first"
                    .into(),
            )
        })?;
        let skip = usize::try_from(anchor.height.saturating_sub(base_height))
            .map_err(|_| StorageError::Invalid("anchor height does not fit in memory".into()))?;
        let blocks = snapshot.blocks.get(skip..).ok_or_else(|| {
            StorageError::Invalid("anchor height is beyond the offered history".into())
        })?;
        let mut state = anchor.state.clone();
        replay_blocks(
            blocks,
            anchor.header.as_ref(),
            policy,
            schedule,
            &mut state,
            "on the committed anchor",
        )?;
        if state.root() != offered.root() {
            return Err(StorageError::Invalid(
                "offered state does not match the replayed history".into(),
            ));
        }
        state
    } else {
        offered
    };
    validate_chain_from(base_height, &snapshot.blocks, &snapshot.receipts, chain_id, &state)?;
    Ok(state)
}

/// Bind the recorded fee policy to the one this call supplies (Q-030 rules 1-4).
///
/// `committed` says whether a manifest or legacy record already exists: a fresh
/// store has nothing recorded yet and may take a supplied policy, while a
/// committed ledger that records none cannot hold version 2 blocks at all.
/// `persisting` separates the two asymmetries: `load` with no supplied policy
/// uses the recorded one (offline tools), `persist` refuses to drop it.
///
/// Q-037 L3: the "needs a fee policy" rules key on strictly-v2 blocks. A v3
/// block settles fees only when a fee policy is bound; a calldata-only chain
/// (v3 + schedule, no fee policy) is valid (ruling 1), and v3 without any
/// policy is refused by [`bind_calldata_policy`] instead. The drop rule keeps
/// the full v2+ range: a recorded fee policy covers v3 blocks too.
pub(crate) fn bind_fee_policy(
    recorded: Option<&FeePolicy>,
    supplied: Option<&FeePolicy>,
    blocks: &[Block],
    committed: bool,
    persisting: bool,
) -> Result<Option<FeePolicy>, StorageError> {
    let has_v2 = blocks.iter().any(|block| block.header.version >= FEE_HEADER_VERSION);
    let has_fee_only = blocks
        .iter()
        .any(|block| (FEE_HEADER_VERSION..CALLDATA_HEADER_VERSION).contains(&block.header.version));
    let has_legacy = blocks.iter().any(|block| block.header.version < FEE_HEADER_VERSION);
    if let (Some(recorded), Some(supplied)) = (recorded, supplied) {
        if recorded != supplied {
            return Err(StorageError::Invalid(
                "supplied fee policy differs from the recorded one".into(),
            ));
        }
    }
    if recorded.is_some() && has_legacy {
        return Err(StorageError::Invalid(
            "a fee-enabled ledger cannot contain legacy version 1 blocks".into(),
        ));
    }
    if committed && recorded.is_none() && has_fee_only {
        return Err(StorageError::Invalid(
            "version 2 blocks in a ledger that records no fee policy".into(),
        ));
    }
    if supplied.is_some() && recorded.is_none() && has_legacy {
        return Err(StorageError::Invalid(
            "a fee policy cannot be bound to a legacy version 1 ledger".into(),
        ));
    }
    if persisting && committed && recorded.is_some() && supplied.is_none() && has_v2 {
        return Err(StorageError::Invalid(
            "persist cannot drop the recorded fee policy of a fee-enabled ledger".into(),
        ));
    }
    if has_fee_only && recorded.is_none() && supplied.is_none() {
        return Err(StorageError::Invalid(
            "a fee-enabled ledger needs a fee policy: persist the genesis state first".into(),
        ));
    }
    Ok(recorded.or(supplied).cloned())
}

/// Bind the recorded calldata schedule to the one this call supplies (Q-037
/// L3, mirror of [`bind_fee_policy`]).
///
/// A priced ledger is ALL-v3 (ruling 2): a recorded or supplied schedule
/// refuses version 1 AND version 2 blocks alike: v2 blocks carry unpriced
/// calldata, so admitting a pre-v3 prefix under a schedule would commit
/// unpriced history on a priced chain and reopen SUBHOST-2026-003. Binding on
/// an empty ledger stays allowed: genesis bootstrap binds before the first
/// block, exactly as fee does.
pub(crate) fn bind_calldata_policy(
    recorded: Option<&CalldataPolicy>,
    supplied: Option<&CalldataPolicy>,
    blocks: &[Block],
    committed: bool,
    persisting: bool,
) -> Result<Option<CalldataPolicy>, StorageError> {
    let has_v3 = blocks.iter().any(|block| block.header.version >= CALLDATA_HEADER_VERSION);
    let has_pre_v3 = blocks.iter().any(|block| block.header.version < CALLDATA_HEADER_VERSION);
    if let (Some(recorded), Some(supplied)) = (recorded, supplied) {
        if recorded != supplied {
            return Err(StorageError::Invalid(
                "supplied calldata policy differs from the recorded one".into(),
            ));
        }
    }
    if recorded.is_some() && has_pre_v3 {
        return Err(StorageError::Invalid(
            "a calldata-priced ledger cannot contain pre-version-3 blocks".into(),
        ));
    }
    if committed && recorded.is_none() && has_v3 {
        return Err(StorageError::Invalid(
            "version 3 blocks in a ledger that records no calldata policy".into(),
        ));
    }
    if supplied.is_some() && recorded.is_none() && has_pre_v3 {
        return Err(StorageError::Invalid(
            "a calldata policy cannot be bound to a pre-version-3 ledger".into(),
        ));
    }
    if persisting && committed && recorded.is_some() && supplied.is_none() && has_v3 {
        return Err(StorageError::Invalid(
            "persist cannot drop the recorded calldata policy of a calldata-priced ledger".into(),
        ));
    }
    if has_v3 && recorded.is_none() && supplied.is_none() {
        return Err(StorageError::Invalid(
            "a calldata-priced ledger needs a calldata policy: persist the genesis state first"
                .into(),
        ));
    }
    Ok(recorded.or(supplied).cloned())
}

/// Execute `blocks` on `state`, settling version 2 fees once per block and
/// enforcing the version 3 calldata floor once per transaction (Q-037 L3).
///
/// `parent` is the header immediately before `blocks[0]`, taken from the offered
/// snapshot or the retained history — never from the cache. With no parent the
/// first block uses the genesis initial fee. The producer credit happens after
/// every transaction of the block, so a tip is never spendable inside its own
/// block, and the recorded base fee must equal the derived one.
///
/// `schedule` is the bound calldata policy. Any version 3 block requires it,
/// and `check_floor` runs per transaction BEFORE `apply_transaction`: the
/// same pure rule admission and selection enforce, applied here from the
/// recorded schedule independently of them (dewi's floor condition). The
/// length refusal inside `check_floor` makes the protocol cap consensus
/// validity on replay too (R-L2-02). A calldata-only chain (v3, schedule, no
/// fee policy) executes without settlement at base fee 0 (ruling 1); fee
/// settlement stays keyed on the fee policy alone.
///
/// Legacy blocks are executed without settlement, which is what restore has
/// always done; a legacy block inside a fee-enabled history is refused, and a
/// pre-v3 block inside a calldata-priced history is refused (no downgrade,
/// defense in depth on top of the bind rules).
pub(crate) fn replay_blocks(
    blocks: &[Block],
    parent: Option<&BlockHeader>,
    policy: Option<&FeePolicy>,
    schedule: Option<&CalldataPolicy>,
    state: &mut State,
    anchor: &str,
) -> Result<(), StorageError> {
    let calldata_enabled =
        blocks.iter().any(|block| block.header.version >= CALLDATA_HEADER_VERSION);
    let schedule = if calldata_enabled {
        Some(schedule.ok_or_else(|| {
            StorageError::Invalid("a version 3 block requires a calldata policy".into())
        })?)
    } else {
        None
    };
    // Strictly-v2 blocks always need a fee policy; v3 blocks settle only when
    // one is bound (a calldata-only chain has none). Mirrors bind_fee_policy.
    let has_fee_only = blocks
        .iter()
        .any(|block| (FEE_HEADER_VERSION..CALLDATA_HEADER_VERSION).contains(&block.header.version));
    let fee_enabled = policy.is_some() || has_fee_only;
    let policy = if fee_enabled {
        Some(policy.ok_or_else(|| {
            StorageError::Invalid("a version 2 block requires a fee policy".into())
        })?)
    } else {
        None
    };
    let limit = parent.map_or_else(
        || blocks.first().map_or(0, |block| block.header.gas_limit),
        |header| header.gas_limit,
    );
    if let Some(schedule) = schedule {
        schedule.validate(limit).map_err(|error| {
            StorageError::Invalid(format!("cannot bind the calldata policy {anchor}: {error}"))
        })?;
    }
    let mut previous = parent;
    for block in blocks {
        let height = block.header.height;
        if calldata_enabled && block.header.version < CALLDATA_HEADER_VERSION {
            return Err(StorageError::Invalid(format!(
                "block {height} mixes a pre-version-3 header into a calldata-priced history"
            )));
        }
        if let Some(schedule) = schedule {
            for transaction in &block.transactions {
                schedule.check_floor(transaction).map_err(|error| {
                    StorageError::Invalid(format!(
                        "block {height} breaks the calldata floor {anchor}: {error}"
                    ))
                })?;
            }
        }
        if let Some(policy) = policy {
            if block.header.version < FEE_HEADER_VERSION {
                return Err(StorageError::Invalid(format!(
                    "block {height} mixes a legacy header into a fee-enabled history"
                )));
            }
            let context = match previous {
                Some(parent) => policy.next_context(parent, limit),
                None => policy.initial_context(limit),
            }
            .map_err(|error| {
                StorageError::Invalid(format!(
                    "cannot derive the fee context for block {height}: {error}"
                ))
            })?;
            if block.header.base_fee_per_gas != context.base_fee_per_gas {
                return Err(StorageError::Invalid(format!(
                    "block {height} records base fee {} but the policy derives {}",
                    block.header.base_fee_per_gas, context.base_fee_per_gas
                )));
            }
            let mut debited = 0u128;
            for transaction in &block.transactions {
                let fee = state.apply_transaction(transaction).map_err(|error| {
                    StorageError::Invalid(format!("cannot replay block {height} {anchor}: {error}"))
                })?;
                debited = debited.checked_add(fee).ok_or_else(|| {
                    StorageError::Invalid(format!("block {height} debited fees overflow"))
                })?;
            }
            let settlement = settle_block_fees(
                &context,
                block.transactions.iter().map(|transaction| (transaction, transaction.gas_limit)),
            )
            .map_err(|error| {
                StorageError::Invalid(format!("cannot settle block {height} fees: {error}"))
            })?;
            if settlement.total != debited {
                return Err(StorageError::Invalid(format!(
                    "block {height} breaks fee conservation: settled {} but debited {debited}",
                    settlement.total
                )));
            }
            let producer =
                (block.header.validator != Address::ZERO).then_some(block.header.validator);
            state.credit_block_priority(producer, settlement.priority).map_err(|error| {
                StorageError::Invalid(format!(
                    "cannot credit block {height} priority fees: {error}"
                ))
            })?;
        } else {
            // Calldata-only: v3 validity rule, legacy settlement, base fee 0
            // (ruling 1). A nonzero recorded base fee without a fee policy
            // would commit to fees nothing settles.
            if block.header.version >= FEE_HEADER_VERSION && block.header.base_fee_per_gas != 0 {
                return Err(StorageError::Invalid(format!(
                    "block {height} records base fee {} without a fee policy; a calldata-only chain settles at base fee 0",
                    block.header.base_fee_per_gas
                )));
            }
            for transaction in &block.transactions {
                state.apply_transaction(transaction).map_err(|error| {
                    StorageError::Invalid(format!("cannot replay block {height} {anchor}: {error}"))
                })?;
            }
        }
        previous = Some(&block.header);
    }
    Ok(())
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
