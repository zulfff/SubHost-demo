//! Node-local voting recovery, separate from the committed block ledger.

use crate::NodeError;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use subhost_consensus::{
    driver::{ConsensusDriver, DriverCheckpoint},
    ConsensusConfig, QuorumCertificate, ValidatorRegistry,
};
use subhost_core::{encode_canonical, Address, Block, ChainId, Hash};
use subhost_crypto::PrivateKey;
use subhost_rpc::RpcState;

pub(crate) const JOURNAL_FILE: &str = "consensus-state.bin";
const MAGIC: &[u8; 8] = b"SUBVOTE1";
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct JournalData {
    version: u32,
    chain_id: ChainId,
    genesis: Hash,
    identity: Option<Address>,
    checkpoint: DriverCheckpoint,
    /// Persisted before appending the certified block to the ledger.
    prepared_commit: Option<(Block, QuorumCertificate)>,
}

pub(crate) struct ConsensusJournal {
    path: PathBuf,
    chain_id: ChainId,
    genesis: Hash,
    identity: Option<Address>,
    // File locking is available at the workspace's Rust 1.89 floor. Keep the
    // descriptor alive so two processes cannot vote from one data directory.
    _lock: File,
}

impl ConsensusJournal {
    pub(crate) fn open(
        data_dir: &Path,
        state: &RpcState,
        genesis: Hash,
        config: ConsensusConfig,
        registry: ValidatorRegistry,
        identity: Option<(Address, PrivateKey)>,
        fresh: bool,
    ) -> Result<(Self, ConsensusDriver), NodeError> {
        let lock_path = data_dir.join("consensus.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path).map_err(|e| journal_error(&lock_path, e))?;
        lock.try_lock().map_err(|e| journal_error(&lock_path, e))?;
        let journal = Self {
            path: data_dir.join(JOURNAL_FILE),
            chain_id: state.chain_id(),
            genesis,
            identity: identity.as_ref().map(|(address, _)| *address),
            _lock: lock,
        };
        let driver = match journal.read()? {
            None => {
                if !fresh && identity.is_some() {
                    return Err(journal_error(
                        &journal.path,
                        "missing journal for an existing validator ledger",
                    ));
                }
                ConsensusDriver::with_tip(
                    config,
                    registry,
                    identity,
                    state.block_height(),
                    state.tip_hash(),
                )?
            }
            Some(data) => {
                if data.version != 1
                    || data.chain_id != journal.chain_id
                    || data.genesis != genesis
                    || data.identity != journal.identity
                {
                    return Err(journal_error(
                        &journal.path,
                        "journal version, genesis or identity mismatch",
                    ));
                }
                let height = state.block_height();
                if data.checkpoint.height > height {
                    return Err(journal_error(
                        &journal.path,
                        "ledger is behind the saved voting state",
                    ));
                }
                if data.checkpoint.height == height && data.checkpoint.tip != state.tip_hash() {
                    return Err(journal_error(
                        &journal.path,
                        "journal tip does not match the ledger",
                    ));
                }
                // An idle old checkpoint has no outstanding voting obligation.
                // This also supports trusted Rust callers that committed directly.
                if data.checkpoint.height < height
                    && data.checkpoint.proposal.is_none()
                    && data.prepared_commit.is_none()
                {
                    if data.checkpoint.height > 0
                        && state.block_at_height(data.checkpoint.height).map(|block| block.hash())
                            != Some(data.checkpoint.tip)
                    {
                        return Err(journal_error(
                            &journal.path,
                            "saved anchor is not in retained ledger history",
                        ));
                    }
                    ConsensusDriver::from_checkpoint(
                        config.clone(),
                        registry.clone(),
                        identity.clone(),
                        data.checkpoint,
                    )?;
                    ConsensusDriver::with_tip(config, registry, identity, height, state.tip_hash())?
                } else {
                    let saved_height = data.checkpoint.height;
                    if saved_height == height {
                        if let Some(proposal) = &data.checkpoint.proposal {
                            state.validate_proposal(&proposal.block)?;
                        }
                    }
                    let mut restored = ConsensusDriver::from_checkpoint(
                        config,
                        registry,
                        identity,
                        data.checkpoint,
                    )?;
                    if let Some((block, qc)) = data.prepared_commit {
                        let mut committed = restored.clone();
                        committed.receive_commit(block.clone(), qc.clone())?;
                        if saved_height == height {
                            state.commit_qc(block, &qc)?;
                        } else if committed.height() != height
                            || committed.tip() != state.tip_hash()
                        {
                            return Err(journal_error(
                                &journal.path,
                                "prepared commit does not match the ledger",
                            ));
                        }
                        restored = committed;
                    } else if saved_height != height {
                        return Err(journal_error(
                            &journal.path,
                            "unreconciled voting state behind the ledger",
                        ));
                    }
                    restored
                }
            }
        };
        journal.save(&driver)?;
        Ok((journal, driver))
    }

    pub(crate) fn save(&self, driver: &ConsensusDriver) -> Result<(), NodeError> {
        self.write(driver.checkpoint(), None)
    }

    pub(crate) fn prepare_commit(
        &self,
        driver: &ConsensusDriver,
        block: Block,
        qc: QuorumCertificate,
    ) -> Result<(), NodeError> {
        self.write(driver.checkpoint(), Some((block, qc)))
    }

    fn read(&self) -> Result<Option<JournalData>, NodeError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(journal_error(&self.path, e)),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| journal_error(&self.path, e))?;
        if bytes.len() < MAGIC.len() + 32 || bytes.len() as u64 > MAX_BYTES || &bytes[..8] != MAGIC
        {
            return Err(journal_error(&self.path, "invalid journal envelope or size"));
        }
        let end = bytes.len() - 32;
        let payload = &bytes[8..end];
        if Hash::from_data(payload).as_bytes() != &bytes[end..] {
            return Err(journal_error(&self.path, "journal checksum mismatch"));
        }
        let data = bincode::options()
            .with_fixint_encoding()
            .with_limit(MAX_BYTES)
            .deserialize(payload)
            .map_err(|e| journal_error(&self.path, e))?;
        Ok(Some(data))
    }

    fn write(
        &self,
        checkpoint: DriverCheckpoint,
        prepared_commit: Option<(Block, QuorumCertificate)>,
    ) -> Result<(), NodeError> {
        let payload = encode_canonical(&JournalData {
            version: 1,
            chain_id: self.chain_id,
            genesis: self.genesis,
            identity: self.identity,
            checkpoint,
            prepared_commit,
        });
        if payload.len() as u64 + 40 > MAX_BYTES {
            return Err(journal_error(&self.path, "journal exceeds its size limit"));
        }
        let temp = self.path.with_extension("bin.tmp");
        // An interrupted temporary write was never published. Remove only the
        // known orphan name, then create exclusively instead of following links.
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(journal_error(&temp, e)),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> std::io::Result<()> {
            let mut file = options.open(&temp)?;
            file.write_all(MAGIC)?;
            file.write_all(&payload)?;
            file.write_all(Hash::from_data(&payload).as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            #[cfg(unix)]
            File::open(self.path.parent().expect("journal has a data directory"))?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(|e| journal_error(&self.path, e))
    }
}

fn journal_error(path: &Path, error: impl std::fmt::Display) -> NodeError {
    NodeError::ConsensusJournal { path: path.to_path_buf(), detail: error.to_string() }
}
