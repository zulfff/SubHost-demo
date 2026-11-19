//! Node-owned durable record of contradictory final evidence (S3.2, ADR-007).
//!
//! `subhost-consensus` classifies authenticated final certificates and refuses to
//! sign once it is halted, but it keeps no disk state: the incident that caused
//! the halt has to survive a restart somewhere node-owned. This file is that
//! place. It stores a small bounded ring of `(height, local hash, incoming hash,
//! views, detection time)` records next to the voting journal, bound to the
//! consensus context so a record from another genesis can never halt this node.
//!
//! On boot the node reads [`ForkLog::latest`] and re-halts before it signs its
//! first view; clearing the halt is an operator action, not a code path.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use subhost_core::{BlockHeight, Hash};

use crate::NodeError;

/// Incident file name, beside the voting journal in the node data directory.
pub(crate) const INCIDENT_FILE: &str = "fork-incidents.bin";

const MAGIC: &[u8; 8] = b"SUBFORK1";
const VERSION: u8 = 1;
const GENESIS_LEN: usize = 32;
const HEADER_LEN: usize = MAGIC.len() + 1 + GENESIS_LEN;
const RECORD_LEN: usize = 8 + 32 + 32 + 8 + 8 + 8 + 8;
/// A bounded incident ring: an operator review needs the newest evidence, not an
/// unbounded log a peer can grow.
const LIMIT: usize = 8;

/// One authenticated contradiction of local finality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForkIncident {
    pub(crate) height: BlockHeight,
    /// The local block hash at [`ForkIncident::height`].
    ///
    /// [`Hash::ZERO`] is a sentinel meaning "this node retains no block at that
    /// height"; it is never a real block hash and never a claim about the tip.
    pub(crate) local_hash: Hash,
    pub(crate) incoming_hash: Hash,
    pub(crate) local_view: u64,
    pub(crate) incoming_view: u64,
    pub(crate) detected_unix: u64,
}

/// Bounded, checksummed incident ring, opened per data directory.
pub(crate) struct ForkLog {
    path: PathBuf,
    genesis: Hash,
    records: VecDeque<ForkIncident>,
}

impl ForkLog {
    /// Open (or create) the incident log next to the node's voting journal.
    pub(crate) fn open(path: &Path, genesis: Hash) -> Result<Self, NodeError> {
        let mut log = Self { path: path.to_path_buf(), genesis, records: VecDeque::new() };
        if path.is_file() {
            log.load()?;
        } else {
            log.rewrite()?;
        }
        Ok(log)
    }

    /// Newest incident, the reason a restarted node must stay halted.
    pub(crate) fn latest(&self) -> Option<&ForkIncident> {
        self.records.back()
    }

    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Persist one incident. The caller authenticated both certificates already.
    ///
    /// A repeated announcement of the same contradiction is a no-op, so trailing
    /// gossiped evidence cannot fill the ring with copies.
    pub(crate) fn record(&mut self, incident: ForkIncident) -> Result<(), NodeError> {
        if self.records.iter().any(|existing| {
            existing.height == incident.height
                && existing.local_hash == incident.local_hash
                && existing.incoming_hash == incident.incoming_hash
        }) {
            return Ok(());
        }
        if self.records.len() >= LIMIT {
            self.records.pop_front();
            self.rewrite()?;
        }
        self.append_frame(&incident)?;
        self.records.push_back(incident);
        Ok(())
    }

    fn load(&mut self) -> Result<(), NodeError> {
        let mut bytes = Vec::new();
        File::open(&self.path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| self.error(&format!("cannot read incident log: {error}")))?;
        if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
            return Err(self.error("incident log header is not a fork incident file"));
        }
        if bytes[MAGIC.len()] != VERSION {
            return Err(self.error("unsupported incident log version"));
        }
        let on_disk = Hash::new(
            bytes[MAGIC.len() + 1..HEADER_LEN]
                .try_into()
                .map_err(|_| self.error("truncated genesis binding"))?,
        );
        if on_disk != self.genesis {
            return Err(self.error("incident log belongs to another consensus context"));
        }

        let mut at = HEADER_LEN;
        let mut good_end = at;
        while at + RECORD_LEN <= bytes.len() {
            let frame = &bytes[at..at + RECORD_LEN];
            let checksum = u64::from_le_bytes(frame[RECORD_LEN - 8..].try_into().unwrap());
            if checksum != checksum_of(&frame[..RECORD_LEN - 8]) {
                break;
            }
            self.records.push_back(ForkIncident {
                height: u64::from_le_bytes(frame[0..8].try_into().unwrap()),
                local_hash: Hash::new(frame[8..40].try_into().unwrap()),
                incoming_hash: Hash::new(frame[40..72].try_into().unwrap()),
                local_view: u64::from_le_bytes(frame[72..80].try_into().unwrap()),
                incoming_view: u64::from_le_bytes(frame[80..88].try_into().unwrap()),
                detected_unix: u64::from_le_bytes(frame[88..96].try_into().unwrap()),
            });
            at += RECORD_LEN;
            good_end = at;
        }

        let mut dropped = good_end != bytes.len();
        while self.records.len() > LIMIT {
            self.records.pop_front();
            dropped = true;
        }
        if dropped {
            // A torn suffix (crash mid-append) or an oversized ring is reconciled
            // by rewriting the retained records; nothing is repaired in place.
            self.rewrite()?;
        }
        Ok(())
    }

    fn error(&self, detail: &str) -> NodeError {
        NodeError::ForkIncident { path: self.path.clone(), detail: detail.to_string() }
    }

    fn append_frame(&self, incident: &ForkIncident) -> Result<(), NodeError> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| self.error(&format!("cannot open incident log: {error}")))?;
        file.write_all(&encode_record(incident))
            .and_then(|()| file.sync_data())
            .map_err(|error| self.error(&format!("cannot append to incident log: {error}")))
    }

    fn rewrite(&self) -> Result<(), NodeError> {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.records.len() * RECORD_LEN);
        bytes.extend_from_slice(MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(self.genesis.as_bytes());
        for incident in &self.records {
            bytes.extend_from_slice(&encode_record(incident));
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
            self.error(&format!("cannot write incident log: {error}"))
        })
    }
}

fn encode_record(incident: &ForkIncident) -> Vec<u8> {
    let mut record = Vec::with_capacity(RECORD_LEN);
    record.extend_from_slice(&incident.height.to_le_bytes());
    record.extend_from_slice(incident.local_hash.as_bytes());
    record.extend_from_slice(incident.incoming_hash.as_bytes());
    record.extend_from_slice(&incident.local_view.to_le_bytes());
    record.extend_from_slice(&incident.incoming_view.to_le_bytes());
    record.extend_from_slice(&incident.detected_unix.to_le_bytes());
    let checksum = checksum_of(&record);
    record.extend_from_slice(&checksum.to_le_bytes());
    record
}

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

    fn incident(height: BlockHeight, incoming: u8) -> ForkIncident {
        ForkIncident {
            height,
            local_hash: Hash::new([height as u8; 32]),
            incoming_hash: Hash::new([incoming; 32]),
            local_view: height + 1,
            incoming_view: height + 2,
            detected_unix: 1_700_000_000 + height,
        }
    }

    fn log() -> (tempfile::TempDir, ForkLog) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INCIDENT_FILE);
        let log = ForkLog::open(&path, Hash::new([9; 32])).unwrap();
        (dir, log)
    }

    #[test]
    fn incidents_survive_a_reopen_and_the_newest_is_the_halt_reason() {
        let (dir, mut log) = log();
        log.record(incident(3, 7)).unwrap();
        log.record(incident(4, 8)).unwrap();
        let path = dir.path().join(INCIDENT_FILE);
        drop(log);

        let reopened = ForkLog::open(&path, Hash::new([9; 32])).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.latest().unwrap().height, 4);
        assert_eq!(reopened.latest().unwrap().incoming_hash, Hash::new([8; 32]));
    }

    #[test]
    fn a_repeated_contradiction_does_not_grow_the_log() {
        let (_dir, mut log) = log();
        let first = incident(3, 7);
        log.record(first).unwrap();
        log.record(first).unwrap();
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest_incidents() {
        let (_dir, mut log) = log();
        for height in 1..=(LIMIT as u64 + 3) {
            log.record(incident(height, height as u8)).unwrap();
        }
        assert_eq!(log.len(), LIMIT);
        assert_eq!(log.latest().unwrap().height, LIMIT as u64 + 3);
    }

    #[test]
    fn a_torn_tail_is_dropped_and_the_valid_prefix_kept() {
        let (dir, mut log) = log();
        log.record(incident(3, 7)).unwrap();
        let path = dir.path().join(INCIDENT_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0xab; RECORD_LEN / 2]);
        std::fs::write(&path, bytes).unwrap();
        drop(log);

        let reopened = ForkLog::open(&path, Hash::new([9; 32])).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.latest().unwrap().height, 3);
    }

    #[test]
    fn an_incident_log_from_another_context_is_refused() {
        let (dir, mut log) = log();
        log.record(incident(3, 7)).unwrap();
        let path = dir.path().join(INCIDENT_FILE);
        drop(log);

        assert!(matches!(
            ForkLog::open(&path, Hash::new([1; 32])),
            Err(NodeError::ForkIncident { .. })
        ));
    }
}
