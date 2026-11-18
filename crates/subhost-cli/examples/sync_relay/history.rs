use anyhow::{bail, Context, Result};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use subhost_consensus::{ConsensusConfig, HotStuff, QuorumCertificate, ValidatorRegistry};
use subhost_core::{encode_canonical, Address, Block, GenesisConfig, Hash};
use subhost_network::{NetworkMessage, MAX_TRANSMIT_BYTES};
use subhost_rpc::RpcState;

const VERSION: u32 = 1;
const MAX_ENTRY_BYTES: u64 = (MAX_TRANSMIT_BYTES * 4) as u64;
const MAX_AHEAD: u64 = 64;

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    genesis: Hash,
    message: NetworkMessage,
    checksum: Hash,
}

#[derive(Debug)]
pub(super) enum IngestError {
    Invalid(anyhow::Error),
    Storage(anyhow::Error),
}

struct Certified {
    block: Block,
    qc: QuorumCertificate,
    message: NetworkMessage,
}

pub(super) struct History {
    directory: std::path::PathBuf,
    genesis: Hash,
    state: RpcState,
    verifier: HotStuff,
    messages: Vec<NetworkMessage>,
    pending: BTreeMap<u64, Certified>,
    bytes: u64,
    limit: u64,
    _lock: File,
}

impl History {
    pub(super) fn open(directory: &Path, genesis: &GenesisConfig, limit: u64) -> Result<Self> {
        genesis.validate()?;
        genesis.requires_validators()?;
        let registry = ValidatorRegistry::from_genesis(&genesis.initial_validators)?;
        let verifier = HotStuff::new(ConsensusConfig::new(registry.len())?, registry.clone());
        let state = RpcState::in_memory(genesis.chain_id)?.with_validators(registry);
        state
            .seed_accounts(genesis.allocations.iter().map(|(address, value)| (*address, *value)))?;
        std::fs::create_dir_all(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("archive.lock"))?;
        lock.try_lock().context("certified-history archive is already in use")?;
        let genesis_hash = Hash::from_data(&encode_canonical(genesis));
        let binding = directory.join("genesis.bin");
        if binding.exists() {
            if std::fs::read(&binding)? != genesis_hash.as_bytes() {
                bail!("certified-history archive belongs to a different genesis");
            }
        } else {
            atomic_write(&binding, genesis_hash.as_bytes())?;
        }
        let mut history = Self {
            directory: directory.to_owned(),
            genesis: genesis_hash,
            state,
            verifier,
            messages: Vec::new(),
            pending: BTreeMap::new(),
            bytes: 0,
            limit,
            _lock: lock,
        };
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "cert") {
                files.push(path);
            }
        }
        files.sort();
        for path in files {
            let size = path.metadata()?.len();
            if size > MAX_ENTRY_BYTES || history.bytes.saturating_add(size) > limit {
                bail!("certified-history archive exceeds its configured byte limit");
            }
            let encoded = std::fs::read(&path)?;
            let envelope: Envelope = codec().deserialize(&encoded)?;
            if envelope.version != VERSION
                || envelope.genesis != genesis_hash
                || envelope.checksum != checksum(genesis_hash, &envelope.message)
            {
                bail!("invalid certified-history checksum, version or genesis binding");
            }
            let certified = history.decode(envelope.message)?;
            let expected = history.height() + 1;
            if path.file_name().and_then(|name| name.to_str())
                != Some(&format!("{expected:020}.cert"))
            {
                bail!("certified-history archive has a gap or an unexpected filename");
            }
            history.state.commit_qc(certified.block, &certified.qc)?;
            history.messages.push(certified.message);
            history.bytes += size;
        }
        Ok(history)
    }

    pub(super) fn height(&self) -> u64 {
        self.state.block_height()
    }
    pub(super) fn state_root(&self) -> Hash {
        self.state.state_root()
    }
    pub(super) fn len(&self) -> usize {
        self.messages.len()
    }
    pub(super) fn message(&self, index: usize) -> Option<&NetworkMessage> {
        self.messages.get(index)
    }

    fn decode(&self, message: NetworkMessage) -> Result<Certified> {
        message.validate()?;
        let NetworkMessage::BlockCommit { view, data, signatures } = &message else {
            bail!("certified-history archive only accepts quorum-certified blocks");
        };
        let block: Block = bincode::options()
            .with_fixint_encoding()
            .with_limit(MAX_TRANSMIT_BYTES as u64)
            .reject_trailing_bytes()
            .deserialize(data)?;
        if block.header.chain_id != self.state.chain_id() || *view != block.header.height {
            bail!("certified block belongs to a different chain or view");
        }
        let qc = QuorumCertificate {
            view: *view,
            block_hash: block.hash(),
            signatures: signatures
                .iter()
                .map(|(address, signature)| (Address::new(*address), signature.clone()))
                .collect(),
        };
        self.verifier.validate_qc(&qc)?;
        Ok(Certified { block, qc, message })
    }

    pub(super) fn ingest(
        &mut self,
        message: NetworkMessage,
    ) -> std::result::Result<usize, IngestError> {
        let NetworkMessage::BlockCommit { view, .. } = &message else {
            return Ok(0);
        };
        // Ignoring stale/far-future envelopes does not trust or apply them. This
        // avoids repeated pairing work while archives replay the same prefix.
        if *view <= self.height() || *view > self.height().saturating_add(MAX_AHEAD) {
            return Ok(0);
        }
        let certified = self.decode(message).map_err(IngestError::Invalid)?;
        let height = certified.block.header.height;
        if height <= self.height() {
            return Ok(0);
        }
        if height > self.height().saturating_add(MAX_AHEAD) {
            return Ok(0);
        }
        if height != self.height() + 1 {
            self.pending.entry(height).or_insert(certified);
            return Ok(0);
        }
        self.append(certified)?;
        let mut appended = 1;
        while let Some(next) = self.pending.remove(&(self.height() + 1)) {
            match self.append(next) {
                Ok(()) => appended += 1,
                Err(IngestError::Invalid(error)) => {
                    tracing::warn!(%error, "discarding an invalid buffered certified block");
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(appended)
    }

    fn append(&mut self, certified: Certified) -> std::result::Result<(), IngestError> {
        self.state
            .validate_proposal(&certified.block)
            .map_err(|e| IngestError::Invalid(e.into()))?;
        let envelope = Envelope {
            version: VERSION,
            genesis: self.genesis,
            checksum: checksum(self.genesis, &certified.message),
            message: certified.message.clone(),
        };
        let bytes = codec().serialize(&envelope).map_err(|e| IngestError::Storage(e.into()))?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES
            || self.bytes.saturating_add(bytes.len() as u64) > self.limit
        {
            return Err(IngestError::Storage(anyhow::anyhow!(
                "certified-history byte limit reached"
            )));
        }
        let path = self.directory.join(format!("{:020}.cert", certified.block.header.height));
        atomic_write(&path, &bytes).map_err(IngestError::Storage)?;
        self.state
            .commit_qc(certified.block, &certified.qc)
            .map_err(|e| IngestError::Storage(e.into()))?;
        self.bytes += bytes.len() as u64;
        self.messages.push(certified.message);
        Ok(())
    }
}

fn checksum(genesis: Hash, message: &NetworkMessage) -> Hash {
    Hash::from_data(&encode_canonical(&(VERSION, genesis, message)))
}

fn codec() -> impl Options {
    bincode::options().with_fixint_encoding().with_limit(MAX_ENTRY_BYTES).reject_trailing_bytes()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("archive record needs a parent directory")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
