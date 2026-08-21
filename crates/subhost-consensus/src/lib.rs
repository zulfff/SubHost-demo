use subhost_core::{Block, BlockHeader, Hash, Address};
use std::collections::{HashMap, BTreeMap, HashSet};
use tokio::sync::{RwLock, mpsc};

#[derive(Clone, Debug)]
pub struct ConsensusConfig {
    pub validator_count: usize,
    pub quorum_threshold: usize,
    pub max_faulty: usize,
    pub block_time_ms: u64,
    pub dag_round_timeout_ms: u64,
}

impl ConsensusConfig {
    pub fn new(validator_count: usize) -> Self {
        // guard: a network with zero validators cannot form a quorum, and
        // `(validator_count - 1)/3` would underflow below 1 anyway.
        assert!(validator_count > 0, "validator_count must be > 0");
        let max_faulty = (validator_count - 1) / 3;
        let quorum_threshold = 2 * max_faulty + 1;
        
        Self {
            validator_count,
            quorum_threshold,
            max_faulty,
            block_time_ms: 1000,
            dag_round_timeout_ms: 500,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DAGVertex {
    pub author: Address,
    pub round: u64,
    pub block: Block,
    pub parents: Vec<Hash>,
    pub signature: Vec<u8>,
}

impl DAGVertex {
    pub fn hash(&self) -> Hash {
        let data = format!("{:?}{:?}{:?}", self.author, self.round, self.block.hash());
        Hash::from_data(data.as_bytes())
    }
}

pub struct DAG {
    vertices: BTreeMap<u64, Vec<DAGVertex>>,
    edges: HashMap<Hash, Vec<Hash>>,
    #[allow(dead_code)]
    committed: HashSet<Hash>,
    config: ConsensusConfig,
}

impl DAG {
    pub fn new(config: ConsensusConfig) -> Self {
        Self {
            vertices: BTreeMap::new(),
            edges: HashMap::new(),
            committed: HashSet::new(),
            config,
        }
    }

    pub fn add_vertex(&mut self, vertex: DAGVertex) -> Result<(), ConsensusError> {
        let hash = vertex.hash();
        
        if self.edges.contains_key(&hash) {
            return Ok(());
        }

        for parent in &vertex.parents {
            if !self.edges.contains_key(parent) && vertex.round > 1 {
                return Err(ConsensusError::MissingParent(*parent));
            }
        }

        self.vertices
            .entry(vertex.round)
            .or_default()
            .push(vertex.clone());
        
        self.edges.insert(hash, vertex.parents.clone());
        
        Ok(())
    }

    pub fn get_round(&self, round: u64) -> Vec<&DAGVertex> {
        self.vertices.get(&round)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub fn has_quorum_support(&self, vertex_hash: &Hash) -> bool {
        // Narwhal semantics: a vertex has quorum support when >= the quorum
        // threshold of *other* validators produced a vertex (in the next round)
        // that references it as a parent. The previous code counted the distinct
        // validators among the vertex's OWN parents (its fan-in / ancestry), which
        // is not "support for" this vertex and could trivially satisfy the quorum
        // with unrelated history.
        let target_round = match self.vertex_round(vertex_hash) {
            Some(r) => r,
            None => return false,
        };

        let supporters: HashSet<Address> = self
            .vertices
            .get(&(target_round + 1))
            .map(|round_vertices| {
                round_vertices
                    .iter()
                    .filter(|v| v.parents.contains(vertex_hash))
                    .map(|v| v.author)
                    .collect()
            })
            .unwrap_or_default();

        supporters.len() >= self.config.quorum_threshold
    }

    fn vertex_round(&self, hash: &Hash) -> Option<u64> {
        self.vertices
            .iter()
            .find(|(_, vs)| vs.iter().any(|v| &v.hash() == hash))
            .map(|(round, _)| *round)
    }

    pub fn gc(&mut self, keep_rounds: u64) {
        let current_round = self.vertices.keys().next_back().copied().unwrap_or(0);
        let cutoff = current_round.saturating_sub(keep_rounds);
        
        self.vertices.retain(|&round, _| round >= cutoff);
    }
}

#[derive(Clone, Debug)]
pub struct QuorumCertificate {
    pub view: u64,
    pub block_hash: Hash,
    pub signatures: Vec<(Address, Vec<u8>)>,
}

pub struct HotStuff {
    view: u64,
    high_qc: Option<QuorumCertificate>,
    #[allow(dead_code)]
    locked_view: u64,
    config: ConsensusConfig,
    validators: Vec<Address>,
}

impl HotStuff {
    pub fn new(config: ConsensusConfig, validators: Vec<Address>) -> Self {
        Self {
            view: 0,
            high_qc: None,
            locked_view: 0,
            config,
            validators,
        }
    }

    pub fn create_proposal(&self, parent_hash: Hash, author: Address) -> BlockHeader {
        BlockHeader {
            version: 1,
            chain_id: 1,
            height: self.view,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
            parent_hash,
            state_root: Hash::ZERO,
            tx_root: Hash::ZERO,
            receipt_root: Hash::ZERO,
            validator: author,
            gas_used: 0,
            gas_limit: 30_000_000,
            extra_data: vec![],
        }
    }

    pub fn validate_qc(&self, qc: &QuorumCertificate) -> bool {
        let signers: HashSet<Address> = qc.signatures.iter().map(|(addr, _)| *addr).collect();
        if signers.len() < self.config.quorum_threshold {
            return false;
        }

        if qc.signatures.iter().any(|(_, sig)| sig.len() != 96) {
            return false;
        }

        for addr in signers {
            if !self.validators.contains(&addr) {
                return false;
            }
            // This scaffold stores validator public keys separately in the
            // production validator registry. Until that registry is wired, QC
            // validation must fail closed instead of accepting unsigned bytes.
            let _ = addr;
            return false;
        }

        false
    }

    pub fn update_high_qc(&mut self, qc: QuorumCertificate) {
        if qc.view > self.high_qc.as_ref().map(|q| q.view).unwrap_or(0) {
            self.high_qc = Some(qc);
        }
    }
}

pub struct ConsensusEngine {
    #[allow(dead_code)]
    dag: std::sync::Arc<RwLock<DAG>>,
    #[allow(dead_code)]
    hotstuff: std::sync::Arc<RwLock<HotStuff>>,
    #[allow(dead_code)]
    config: ConsensusConfig,
    #[allow(dead_code)]
    finalized_tx: mpsc::Sender<Block>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConsensusError {
    #[error("Missing parent: {0}")]
    MissingParent(Hash),
    
    #[error("Invalid signature")]
    InvalidSignature,
    
    #[error("Serialization failed: {0}")]
    Serialization(String),
    
    #[error("Channel closed")]
    ChannelClosed,
    
    #[error("Invalid proposal")]
    InvalidProposal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consensus_config() {
        let config = ConsensusConfig::new(4);
        assert_eq!(config.max_faulty, 1);
        assert_eq!(config.quorum_threshold, 3);
    }

    #[test]
    fn qc_requires_distinct_validators() {
        let config = ConsensusConfig::new(4);
        let validator = Address::new([1u8; 20]);
        let hotstuff = HotStuff::new(config, vec![validator]);
        let qc = QuorumCertificate {
            view: 1,
            block_hash: Hash::ZERO,
            signatures: vec![(validator, vec![]), (validator, vec![]), (validator, vec![])],
        };
        assert!(!hotstuff.validate_qc(&qc));
    }
}

pub mod staking;
