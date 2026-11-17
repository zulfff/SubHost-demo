//! The version 2 single-file ledger envelope, kept so an existing node can be
//! read and migrated in place.
//!
//! Nothing writes this format any more. [`read`] is the only production entry
//! point; [`encode`] exists for the tests that build a pre-S4 ledger.

use crate::{LedgerSnapshot, StorageError, LEDGER_FORMAT_VERSION, LEDGER_MAGIC, MAX_LEDGER_BYTES};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerEnvelope {
    magic: [u8; 8],
    version: u32,
    checksum: [u8; 32],
    payload: Vec<u8>,
}

/// Read, validate, and decode a version 2 `node-state.bin`.
pub(crate) fn read(path: &Path) -> Result<LedgerSnapshot, StorageError> {
    let metadata = fs::metadata(path)
        .map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    if metadata.len() > MAX_LEDGER_BYTES {
        return Err(StorageError::TooLarge { size: metadata.len(), max: MAX_LEDGER_BYTES });
    }

    let encoded =
        fs::read(path).map_err(|source| StorageError::Io { path: path.to_path_buf(), source })?;
    let envelope: LedgerEnvelope =
        bincode::deserialize(&encoded).map_err(|error| StorageError::Decode(error.to_string()))?;
    if envelope.magic != LEDGER_MAGIC {
        return Err(StorageError::UnsupportedFormat);
    }
    if envelope.version != LEDGER_FORMAT_VERSION {
        return Err(StorageError::UnsupportedVersion(envelope.version));
    }
    if *blake3::hash(&envelope.payload).as_bytes() != envelope.checksum {
        return Err(StorageError::ChecksumMismatch);
    }

    bincode::deserialize(&envelope.payload).map_err(|error| StorageError::Decode(error.to_string()))
}

/// Encode a snapshot in the version 2 envelope. Test-only.
#[cfg(test)]
pub(crate) fn encode(snapshot: &LedgerSnapshot) -> Vec<u8> {
    let payload = bincode::serialize(snapshot).expect("test snapshots must encode");
    let envelope = LedgerEnvelope {
        magic: LEDGER_MAGIC,
        version: LEDGER_FORMAT_VERSION,
        checksum: *blake3::hash(&payload).as_bytes(),
        payload,
    };
    bincode::serialize(&envelope).expect("test envelopes must encode")
}
