//! Node-owned archive of the certified history this node actually committed.
//!
//! Q-013 / ADR-007: the archive keeps the canonical `(block, QC)` pair for every
//! block whose quorum certificate verified, bounded to the latest [`RETENTION`]
//! items. It never infers a certificate from the ledger and never invents
//! history: a frame that does not checksum is dropped, and a caller must still
//! match every block it serves against the retained ledger before it goes on the
//! wire.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use subhost_core::{BlockHeight, Hash};

use crate::NodeError;

/// Archive file name, beside the voting journal in the node data directory.
pub(crate) const HISTORY_FILE: &str = "certified-history.bin";

const MAGIC: &[u8; 8] = b"SUBHIST1";
const VERSION: u8 = 1;
/// Latest certified items kept on disk and offered to peers (Q-013).
pub(crate) const RETENTION: usize = 4096;
/// Rewrite down to this many items once the archive passes [`RETENTION`], so a
/// prune is amortised instead of rewriting the file on every commit.
const PRUNE_TO: usize = RETENTION - 512;
const GENESIS_LEN: usize = 32;
const HEADER_LEN: usize = MAGIC.len() + 1 + GENESIS_LEN;
const FRAME_HEADER: usize = 8 + 4 + 4;
const CHECKSUM_LEN: usize = 8;
/// One archived item may not exceed a single gossip payload.
const MAX_ITEM_BYTES: usize = subhost_network::MAX_WIRE_BYTES;

/// One committed block and the certificate that finalized it, as canonical bytes.
struct Item {
    height: BlockHeight,
    block: Vec<u8>,
    qc: Vec<u8>,
}

impl Item {
    fn frame_len(&self) -> usize {
        FRAME_HEADER + self.block.len() + self.qc.len() + CHECKSUM_LEN
    }
}

/// Bounded, checksummed archive of certified history, opened per data directory.
pub(crate) struct CertifiedHistory {
    path: PathBuf,
    genesis: Option<Hash>,
    items: VecDeque<Item>,
}

impl CertifiedHistory {
    /// Open (or create) the archive next to the node's voting journal.
    pub(crate) fn open(path: &Path, genesis: Option<Hash>) -> Result<Self, NodeError> {
        let mut history = Self { path: path.to_path_buf(), genesis, items: VecDeque::new() };
        if path.is_file() {
            history.load()?;
        } else {
            history.rewrite()?;
        }
        Ok(history)
    }

    /// Height of the newest archived certificate, if any.
    pub(crate) fn tip(&self) -> Option<BlockHeight> {
        self.items.back().map(|item| item.height)
    }

    /// Oldest height still retained, if any.
    pub(crate) fn retained_from(&self) -> Option<BlockHeight> {
        self.items.front().map(|item| item.height)
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Record one verified `(block, QC)` pair.
    ///
    /// The caller verifies the certificate first: this layer only stores bytes.
    /// Recording is durable before it returns, so a crash cannot leave a
    /// committed ledger block without its archived certificate.
    pub(crate) fn append(
        &mut self,
        height: BlockHeight,
        block: &[u8],
        qc: &[u8],
    ) -> Result<(), NodeError> {
        if block.is_empty() || qc.is_empty() {
            return Err(self.error("refusing to archive an empty block or certificate"));
        }
        if block.len() + qc.len() > MAX_ITEM_BYTES {
            return Err(self.error("certified item exceeds the wire budget"));
        }
        if let Some(last) = self.items.back() {
            // A commit that failed after archiving retries the same tip item. It
            // is already durable, so accept the retry instead of wedging it.
            if height == last.height {
                if last.block == block && last.qc == qc {
                    return Ok(());
                }
                return Err(self.error("conflicting certificate for the archived tip"));
            }
            if height < last.height {
                return Err(self.error("certified history must ascend by height"));
            }
        }
        if self.items.len() >= RETENTION {
            self.prune()?;
        }
        let item = Item { height, block: block.to_vec(), qc: qc.to_vec() };
        self.append_frame(&item)?;
        self.items.push_back(item);
        Ok(())
    }

    /// Ascending items inside `[from, to]`, capped by [`super::MAX_SYNC_ITEMS`].
    ///
    /// Heights above the archived tip are simply absent, so a peer receives a
    /// valid prefix rather than a promise this node cannot keep.
    pub(crate) fn range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Vec<(BlockHeight, &[u8], &[u8])> {
        if from > to {
            return Vec::new();
        }
        self.items
            .iter()
            .filter(|item| item.height >= from && item.height <= to)
            .take(subhost_network::MAX_SYNC_ITEMS)
            .map(|item| (item.height, item.block.as_slice(), item.qc.as_slice()))
            .collect()
    }

    fn load(&mut self) -> Result<(), NodeError> {
        let mut bytes = Vec::new();
        File::open(&self.path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| self.error(&format!("cannot read archive: {error}")))?;
        if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
            return Err(self.error("archive header is not a certified history file"));
        }
        if bytes[MAGIC.len()] != VERSION {
            return Err(self.error("unsupported archive version"));
        }
        let on_disk = Hash::new(
            bytes[MAGIC.len() + 1..HEADER_LEN]
                .try_into()
                .map_err(|_| self.error("truncated genesis binding"))?,
        );
        if let Some(genesis) = self.genesis {
            if on_disk != genesis {
                return Err(self.error("archive belongs to another genesis"));
            }
        }

        let mut at = HEADER_LEN;
        let mut good_end = at;
        while at + FRAME_HEADER + CHECKSUM_LEN <= bytes.len() {
            let height = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            let block_len = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
            let qc_len = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap()) as usize;
            if block_len == 0 || qc_len == 0 || block_len + qc_len > MAX_ITEM_BYTES {
                break;
            }
            let end = at + FRAME_HEADER + block_len + qc_len;
            if end + CHECKSUM_LEN > bytes.len() {
                break;
            }
            let checksum = u64::from_le_bytes(bytes[end..end + CHECKSUM_LEN].try_into().unwrap());
            if checksum != checksum_of(&bytes[at..end]) {
                break;
            }
            self.items.push_back(Item {
                height,
                block: bytes[at + FRAME_HEADER..at + FRAME_HEADER + block_len].to_vec(),
                qc: bytes[at + FRAME_HEADER + block_len..end].to_vec(),
            });
            at = end + CHECKSUM_LEN;
            good_end = at;
        }

        let truncated = good_end != bytes.len();
        let mut dropped = false;
        while self.items.len() > RETENTION {
            self.items.pop_front();
            dropped = true;
        }
        if truncated || dropped || !self.ascends() {
            // A torn suffix or a dropped prefix is reconciled by rewriting the
            // retained window; nothing is repaired in place.
            self.rewrite()?;
        }
        Ok(())
    }

    fn ascends(&self) -> bool {
        self.items.iter().zip(self.items.iter().skip(1)).all(|(a, b)| a.height < b.height)
    }

    fn error(&self, detail: &str) -> NodeError {
        NodeError::CertifiedHistory { path: self.path.clone(), detail: detail.to_string() }
    }

    fn append_frame(&self, item: &Item) -> Result<(), NodeError> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| self.error(&format!("cannot open archive: {error}")))?;
        let frame = encode_frame(item);
        file.write_all(&frame)
            .and_then(|()| file.sync_data())
            .map_err(|error| self.error(&format!("cannot append to archive: {error}")))
    }

    fn prune(&mut self) -> Result<(), NodeError> {
        while self.items.len() > PRUNE_TO {
            self.items.pop_front();
        }
        self.rewrite()
    }

    fn rewrite(&self) -> Result<(), NodeError> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + self.items.iter().map(Item::frame_len).sum::<usize>());
        bytes.extend_from_slice(MAGIC);
        bytes.push(VERSION);
        match self.genesis {
            Some(genesis) => bytes.extend_from_slice(genesis.as_bytes()),
            None => bytes.extend_from_slice(&[0u8; GENESIS_LEN]),
        }
        for item in &self.items {
            bytes.extend_from_slice(&encode_frame(item));
        }

        let temp = self.path.with_extension("tmp");
        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)
        })();
        result.map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            self.error(&format!("cannot write archive: {error}"))
        })
    }
}

/// Wire encoding of one certified item: block length, block bytes, certificate.
///
/// The `Blocks` payload carries these items verbatim, so serving an archived
/// item never re-encodes a block it already holds.
pub(crate) fn encode_item(block: &[u8], qc: &[u8]) -> Vec<u8> {
    let mut item = Vec::with_capacity(4 + block.len() + qc.len());
    item.extend_from_slice(&(block.len() as u32).to_le_bytes());
    item.extend_from_slice(block);
    item.extend_from_slice(qc);
    item
}

/// Split one `Blocks` item back into its block and certificate bytes.
pub(crate) fn decode_item(item: &[u8]) -> Option<(&[u8], &[u8])> {
    if item.len() < 5 {
        return None;
    }
    let block_len = u32::from_le_bytes(item[..4].try_into().ok()?) as usize;
    let end = 4usize.checked_add(block_len)?;
    if block_len == 0 || end >= item.len() {
        return None;
    }
    Some((&item[4..end], &item[end..]))
}

fn encode_frame(item: &Item) -> Vec<u8> {
    let mut frame =
        Vec::with_capacity(FRAME_HEADER + item.block.len() + item.qc.len() + CHECKSUM_LEN);
    frame.extend_from_slice(&item.height.to_le_bytes());
    frame.extend_from_slice(&(item.block.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(item.qc.len() as u32).to_le_bytes());
    frame.extend_from_slice(&item.block);
    frame.extend_from_slice(&item.qc);
    let checksum = checksum_of(&frame);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame
}

/// FNV-1a over the frame; stable across releases and cheap on the hot path.
fn checksum_of(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(dir: &Path) -> CertifiedHistory {
        CertifiedHistory::open(&dir.join(HISTORY_FILE), Some(Hash::new([9; 32]))).unwrap()
    }

    #[test]
    fn archived_items_survive_a_reopen_in_order() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut archive = history(dir.path());
            archive.append(1, b"block-1", b"qc-1").unwrap();
            archive.append(2, b"block-2", b"qc-2").unwrap();
            assert_eq!(archive.tip(), Some(2));
        }
        let reopened = history(dir.path());
        assert_eq!(reopened.tip(), Some(2));
        assert_eq!(reopened.retained_from(), Some(1));
        assert_eq!(reopened.len(), 2);
        assert_eq!(
            reopened.range(1, 2),
            vec![
                (1, b"block-1".as_slice(), b"qc-1".as_slice()),
                (2, b"block-2".as_slice(), b"qc-2".as_slice())
            ]
        );
    }

    #[test]
    fn history_must_ascend_and_stay_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = history(dir.path());
        assert!(archive.append(2, b"", b"qc").is_err());
        assert!(archive.append(2, b"block", b"").is_err());
        archive.append(2, b"block-2", b"qc-2").unwrap();
        assert!(
            archive.append(2, b"block-2", b"qc-2").is_ok(),
            "retrying a commit that already archived its tip must not wedge"
        );
        assert!(
            archive.append(2, b"other-block", b"qc-2").is_err(),
            "a conflicting certificate for the same tip is refused"
        );
        assert!(archive.append(1, b"block-1", b"qc-1").is_err());
        assert_eq!(archive.len(), 1);
    }

    #[test]
    fn ranges_clamp_to_retained_items_and_ignore_inverted_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = history(dir.path());
        for height in 1..=3 {
            archive.append(height, &[height as u8], &[height as u8]).unwrap();
        }
        assert_eq!(archive.range(2, 9).len(), 2);
        assert_eq!(archive.range(0, 1).len(), 1);
        assert!(archive.range(3, 2).is_empty());
        assert!(archive.range(4, 9).is_empty());
    }

    #[test]
    fn a_torn_tail_is_dropped_and_the_valid_prefix_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE);
        {
            let mut archive = CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).unwrap();
            archive.append(1, b"block-1", b"qc-1").unwrap();
            archive.append(2, b"block-2", b"qc-2").unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&path, &bytes).unwrap();

        let recovered = CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).unwrap();
        assert_eq!(recovered.tip(), Some(1));
        assert_eq!(recovered.len(), 1);
        // The repaired file is rewritten, so a second open sees the same prefix.
        let again = CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).unwrap();
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn an_archive_bound_to_another_genesis_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE);
        {
            let mut archive = CertifiedHistory::open(&path, Some(Hash::new([7; 32]))).unwrap();
            archive.append(1, b"block-1", b"qc-1").unwrap();
        }
        assert!(CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).is_err());
    }

    #[test]
    fn retention_keeps_the_newest_window_and_the_tip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE);
        let appended = RETENTION as BlockHeight + 8;
        {
            let mut archive = CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).unwrap();
            for height in 1..=appended {
                archive.append(height, b"block", b"qc").unwrap();
            }
            assert_eq!(archive.tip(), Some(appended));
            assert!(archive.len() <= RETENTION);
            assert!(archive.len() >= PRUNE_TO);
            let oldest = appended - archive.len() as BlockHeight + 1;
            assert_eq!(archive.retained_from(), Some(oldest));
        }
        let reopened = CertifiedHistory::open(&path, Some(Hash::new([9; 32]))).unwrap();
        assert!(reopened.len() <= RETENTION);
        assert_eq!(reopened.tip(), Some(appended));
        assert_eq!(reopened.retained_from(), Some((RETENTION - PRUNE_TO) as BlockHeight + 1));
    }

    #[test]
    fn wire_items_round_trip_and_reject_truncation() {
        let item = encode_item(b"block-bytes", b"certificate-bytes");
        assert_eq!(
            decode_item(&item),
            Some((b"block-bytes".as_slice(), b"certificate-bytes".as_slice()))
        );
        assert!(decode_item(&[]).is_none());
        assert!(decode_item(&item[..4]).is_none());
        assert!(
            decode_item(&item[..item.len() - 1]).is_some(),
            "a shorter certificate is the receiver's check"
        );
        assert!(decode_item(&[0, 0, 0, 0, 1]).is_none(), "an empty block is never a valid item");
        assert!(decode_item(&[9, 0, 0, 0, 1]).is_none(), "a block length past the item is refused");
    }
}
