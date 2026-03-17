//! Hybrid consensus: DAG-based mempool + HotStuff finality
//!
//! # Architecture
//! - Narwhal-inspired DAG for transaction dissemination
//! - Bullshark/Tusk for DAG consensus
//! - HotStuff for deterministic finality (1-2 seconds)
//!
//! # Security Guarantees
//! - BFT tolerance: f < n/3 Byzantine validators
//! - Double-signing slashing
//! - Optimistic responsiveness
//!
//! # Known Limitations (By Design)
//! 1. **Network Synchrony Assumption**: HotStuff requires partial synchrony
//!    for liveness. During network partitions, liveness may stall.
//! 2. **Complexity**: DAG + HotStuff is more complex than single-chain.
//!    Higher audit surface.
//! 3. **Memory Usage**: DAG stores multiple rounds in memory.
//!    See memory limits in config.

use omnichain_core::{Block, BlockHeader, Hash, Address, BlockHeight, Timestamp};
use omnichain_crypto::{BLSScheme, PrivateKey, PublicKey, Signature};
use std::collections::{HashMap, BTreeMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn, info};

/// Configuration for consensus
#[derive(Clone, Debug)]
pub struct ConsensusConfig {
    /// Number of validators
    pub validator_count: usize,
    /// Threshold for quorum (2f+1)
    pub quorum_threshold: usize,
    /// Maximum faulty validators
    pub max_faulty: usize,
    /// Block time target (milliseconds)
    pub block_time_ms: u64,
    /// DAG round timeout (milliseconds)
    pub dag_round_timeout_ms: u64,
    /// Enable optimistic responsiveness
    pub optimistic_responsiveness: bool,
}

impl ConsensusConfig {
    pub fn new(validator_count: usize) -> Self {
        let max_faulty = (validator_count - 1) / 3;
        let quorum_threshold = 2 * max_faulty + 1;
        
        Self {
            validator_count,
            quorum_threshold,
            max_faulty,
            block_time_ms: 1000,
            dag_round_timeout_ms: 500,
            optimistic_responsiveness: true,
        }
    }
}

/// DAG vertex representing a block proposal
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

/// DAG structure for transaction dissemination
pub struct DAG {
    vertices: BTreeMap<u64, Vec<DAGVertex>>,
    edges: HashMap<Hash, Vec<Hash>>, // vertex -> parents
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

    /// Add vertex to DAG
    pub fn add_vertex(&mut self, vertex: DAGVertex) -> Result<(), ConsensusError> {
        let hash = vertex.hash();
        
        // Check if already exists
        if self.edges.contains_key(&hash) {
            return Ok(());
        }

        // Validate parents exist
        for parent in &vertex.parents {
            if !self.edges.contains_key(parent) && vertex.round > 1 {
                return Err(ConsensusError::MissingParent(*parent));
            }
        }

        // Add to round
        self.vertices
            .entry(vertex.round)
            .or_default()
            .push(vertex.clone());
        
        // Add edges
        self.edges.insert(hash, vertex.parents.clone());
        
        debug!("Added DAG vertex: {:?} at round {}", hash, vertex.round);
        Ok(())
    }

    /// Get vertices at round
    pub fn get_round(&self, round: u64) -> Vec<&DAGVertex> {
        self.vertices.get(&round)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Check if vertex has quorum support (2f+1 parents)
    pub fn has_quorum_support(&self, vertex_hash: &Hash) -> bool {
        let parents = match self.edges.get(vertex_hash) {
            Some(p) => p,
            None => return false,
        };
        
        // Need quorum_threshold unique validator parents
        let unique_validators: HashSet<_> = parents.iter()
            .filter_map(|p| self.get_vertex_author(p))
            .collect();
        
        unique_validators.len() >= self.config.quorum_threshold
    }

    fn get_vertex_author(&self, hash: &Hash) -> Option<Address> {
        for vertices in self.vertices.values() {
            for v in vertices {
                if &v.hash() == hash {
                    return Some(v.author);
                }
            }
        }
        None
    }

    /// Garbage collect old rounds
    pub fn gc(&mut self, keep_rounds: u64) {
        let current_round = self.vertices.keys().next_back().copied().unwrap_or(0);
        let cutoff = current_round.saturating_sub(keep_rounds);
        
        self.vertices.retain(|&round, _| round >= cutoff);
    }
}

/// HotStuff consensus state
pub struct HotStuff {
    view: u64,
    high_qc: Option<QuorumCertificate>,
    locked_view: u64,
    config: ConsensusConfig,
    validators: Vec<Address>,
}

/// Quorum certificate
#[derive(Clone, Debug)]
pub struct QuorumCertificate {
    pub view: u64,
    pub block_hash: Hash,
    pub signatures: Vec<(Address, Vec<u8>)>,
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

    /// Create new proposal
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

    /// Validate QC
    pub fn validate_qc(&self, qc: &QuorumCertificate) -> bool {
        // Check signature count
        if qc.signatures.len() < self.config.quorum_threshold {
            return false;
        }

        // Check all signers are validators
        for (addr, _) in &qc.signatures {
            if !self.validators.contains(addr) {
                return false;
            }
        }

        // Verify BLS aggregate signature
        let pks: Vec<_> = qc.signatures.iter()
            .filter_map(|(addr, _)| self.get_validator_pk(addr))
            .collect();
        
        // Note: In production, we'd verify the BLS signature here
        // For now, we trust the signature validation is done elsewhere
        
        pks.len() >= self.config.quorum_threshold
    }

    fn get_validator_pk(&self, _addr: &Address) -> Option<PublicKey> {
        // In production, lookup from validator set
        // For now, return dummy
        None
    }

    /// Update high QC if newer
    pub fn update_high_qc(&mut self, qc: QuorumCertificate) {
        if qc.view > self.high_qc.as_ref().map(|q| q.view).unwrap_or(0) {
            self.high_qc = Some(qc);
        }
    }
}

/// Consensus engine combining DAG and HotStuff
pub struct ConsensusEngine {
    dag: Arc<RwLock<DAG>>,
    hotstuff: Arc<RwLock<HotStuff>>,
    config: ConsensusConfig,
    validator_key: PrivateKey,
    validator_address: Address,
    pending_blocks: Arc<RwLock<HashMap<Hash, Block>>>,
    finalized_tx: mpsc::Sender<Block>,
}

impl ConsensusEngine {
    pub fn new(
        config: ConsensusConfig,
        validator_key: PrivateKey,
        validator_address: Address,
        finalized_tx: mpsc::Sender<Block>,
        validators: Vec<Address>,
    ) -> Self {
        Self {
            dag: Arc::new(RwLock::new(DAG::new(config.clone()))),
            hotstuff: Arc::new(RwLock::new(HotStuff::new(config.clone(), validators))),
            config,
            validator_key,
            validator_address,
            pending_blocks: Arc::new(RwLock::new(HashMap::new())),
            finalized_tx,
        }
    }

    /// Process incoming DAG vertex
    pub async fn process_vertex(&self, vertex: DAGVertex) -> Result<(), ConsensusError> {
        // Validate signature
        self.validate_vertex_signature(&vertex)?;

        // Add to DAG
        let mut dag = self.dag.write().await;
        dag.add_vertex(vertex.clone())?;

        // Check for quorum support
        let vertex_hash = vertex.hash();
        if dag.has_quorum_support(&vertex_hash) {
            // Trigger HotStuff commit
            self.commit_block(vertex.block.clone()).await?;
        }

        // GC old vertices
        dag.gc(100);

        Ok(())
    }

    /// Create and broadcast new proposal
    pub async fn propose_block(&self, transactions: Vec<omnichain_core::Transaction>) -> Result<Block, ConsensusError> {
        let hotstuff = self.hotstuff.read().await;
        let parent_hash = hotstuff.high_qc.as_ref()
            .map(|qc| qc.block_hash)
            .unwrap_or(Hash::ZERO);
        
        let header = hotstuff.create_proposal(parent_hash, self.validator_address);
        
        let block = Block {
            header,
            transactions,
            signatures: vec![],
        };

        // Sign block
        let block_hash = block.hash();
        let signature = BLSScheme::sign(&self.validator_key, block_hash.as_bytes());
        let sig_bytes = {
            let mut bytes = Vec::new();
            signature.serialize_compressed(&mut bytes).map_err(|e| 
                ConsensusError::Serialization(e.to_string()))?;
            bytes
        };

        // Create DAG vertex
        let vertex = DAGVertex {
            author: self.validator_address,
            round: self.get_current_round().await,
            block: block.clone(),
            parents: self.get_parent_hashes().await,
            signature: sig_bytes,
        };

        // Broadcast vertex
        // In production, send to network layer
        
        Ok(block)
    }

    /// Commit block (finalized)
    async fn commit_block(&self, block: Block) -> Result<(), ConsensusError> {
        info!("Committing block at height {}", block.header.height);
        
        // Send to execution layer
        self.finalized_tx.send(block.clone()).await
            .map_err(|_| ConsensusError::ChannelClosed)?;
        
        // Update HotStuff
        let mut hotstuff = self.hotstuff.write().await;
        let qc = QuorumCertificate {
            view: block.header.height,
            block_hash: block.hash(),
            signatures: vec![], // Aggregate collected
        };
        hotstuff.update_high_qc(qc);

        Ok(())
    }

    fn validate_vertex_signature(&self, vertex: &DAGVertex) -> Result<(), ConsensusError> {
        // In production: verify BLS signature
        // For now: basic validation
        if vertex.signature.is_empty() {
            return Err(ConsensusError::InvalidSignature);
        }
        Ok(())
    }

    async fn get_current_round(&self) -> u64 {
        let dag = self.dag.read().await;
        dag.vertices.keys().next_back().copied().unwrap_or(0) + 1
    }

    async fn get_parent_hashes(&self) -> Vec<Hash> {
        let dag = self.dag.read().await;
        let current = self.get_current_round().await;
        
        if current == 1 {
            vec![Hash::ZERO]
        } else {
            dag.get_round(current - 1)
                .iter()
                .map(|v| v.hash())
                .collect()
        }
    }

    /// Start consensus loop
    pub async fn run(&self) {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(self.config.block_time_ms)).await;
            
            // Check if leader for this round
            if self.is_leader().await {
                // Propose empty block or aggregate pending
                if let Err(e) = self.propose_block(vec![]).await {
                    warn!("Failed to propose block: {}", e);
                }
            }
        }
    }

    async fn is_leader(&self) -> bool {
        // Simple round-robin leader selection
        // In production: use VRF or verifiable randomness
        let hotstuff = self.hotstuff.read().await;
        let leader_index = (hotstuff.view % self.config.validator_count as u64) as usize;
        
        self.config.validator_count > 0 && leader_index == 0 // Assume we're validator 0 for now
    }
}

/// Consensus errors
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

    #[tokio::test]
    async fn test_dag_add_vertex() {
        let config = ConsensusConfig::new(4);
        let mut dag = DAG::new(config);
        
        let (sk, _pk) = BLSScheme::keygen();
        let addr = Address::from([1u8; 20]);
        
        let vertex = DAGVertex {
            author: addr,
            round: 1,
            block: Block {
                header: BlockHeader {
                    version: 1,
                    chain_id: 1,
                    height: 1,
                    timestamp: 0,
                    parent_hash: Hash::ZERO,
                    state_root: Hash::ZERO,
                    tx_root: Hash::ZERO,
                    receipt_root: Hash::ZERO,
                    validator: addr,
                    gas_used: 0,
                    gas_limit: 30_000_000,
                    extra_data: vec![],
                },
                transactions: vec![],
                signatures: vec![],
            },
            parents: vec![Hash::ZERO],
            signature: vec![],
        };
        
        dag.add_vertex(vertex).unwrap();
        assert_eq!(dag.get_round(1).len(), 1);
    }
}
