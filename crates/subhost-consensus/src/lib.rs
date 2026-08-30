//! Consensus building blocks: a Narwhal-style DAG for dissemination, HotStuff
//! quorum rules, and the staking/slashing registry.
//!
//! Scope: these are the verifiable pieces of the protocol — vertex admission,
//! quorum support, QC signature validation, and stake accounting. There is no
//! driving consensus loop or block production here; the single-node producer
//! lives in `subhost-rpc`. Nothing in this crate fabricates agreement.

use ark_serialize::CanonicalDeserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use subhost_core::{encode_canonical, Address, Block, Hash};
use subhost_crypto::{BLSScheme, PublicKey, Signature};
use tracing::debug;

pub mod staking;

/// Compressed BLS12-381 G2 signature length.
pub const BLS_SIGNATURE_BYTES: usize = 96;
/// Compressed BLS12-381 G1 public key length.
pub const BLS_PUBLIC_KEY_BYTES: usize = 48;

/// Quorum sizing for a fixed validator count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsensusConfig {
    pub validator_count: usize,
    /// Signatures required to form a quorum certificate: `2f + 1`.
    pub quorum_threshold: usize,
    /// Tolerated byzantine validators: `f = (n - 1) / 3`.
    pub max_faulty: usize,
    pub block_time_ms: u64,
    pub dag_round_timeout_ms: u64,
}

impl ConsensusConfig {
    /// Derive BFT quorum sizing from a validator count.
    ///
    /// Returns an error for an empty set rather than asserting: validator counts
    /// come from genesis, which is operator input.
    pub fn new(validator_count: usize) -> Result<Self, ConsensusError> {
        if validator_count == 0 {
            return Err(ConsensusError::InvalidConfig("validator_count must be > 0".into()));
        }
        let max_faulty = (validator_count - 1) / 3;
        Ok(Self {
            validator_count,
            quorum_threshold: 2 * max_faulty + 1,
            max_faulty,
            block_time_ms: subhost_core::DEFAULT_BLOCK_TIME_MS,
            dag_round_timeout_ms: 500,
        })
    }
}

/// Maps validator addresses to their BLS public keys.
///
/// Registration requires a proof of possession. Without it, aggregating public
/// keys is open to the rogue-key attack: a malicious participant picks its key as
/// `g - sum(others)` and forges an aggregate signature for the whole set.
#[derive(Clone, Default)]
pub struct ValidatorRegistry {
    keys: HashMap<Address, PublicKey>,
}

impl ValidatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a validator after verifying its proof of possession.
    pub fn register(
        &mut self,
        address: Address,
        public_key: PublicKey,
        proof_of_possession: &Signature,
    ) -> Result<(), ConsensusError> {
        if !BLSScheme::verify_possession(&public_key, proof_of_possession) {
            return Err(ConsensusError::InvalidProofOfPossession(address));
        }
        if self.keys.insert(address, public_key).is_some() {
            return Err(ConsensusError::DuplicateValidator(address));
        }
        Ok(())
    }

    pub fn public_key(&self, address: &Address) -> Option<&PublicKey> {
        self.keys.get(address)
    }

    pub fn contains(&self, address: &Address) -> bool {
        self.keys.contains_key(address)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A vertex in the dissemination DAG.
#[derive(Clone, Debug)]
pub struct DAGVertex {
    pub author: Address,
    pub round: u64,
    pub block: Block,
    pub parents: Vec<Hash>,
    pub signature: Vec<u8>,
}

impl DAGVertex {
    /// Identity of this vertex.
    ///
    /// Committed via canonical encoding rather than a `Debug` string, so the hash
    /// cannot change when an unrelated `Debug` implementation is reformatted.
    pub fn hash(&self) -> Hash {
        let mut parents = self.parents.clone();
        parents.sort_unstable();
        Hash::from_data(&encode_canonical(&(self.author, self.round, self.block.hash(), parents)))
    }
}

/// Round-indexed vertex store with quorum-support queries.
pub struct DAG {
    vertices: BTreeMap<u64, Vec<DAGVertex>>,
    /// Vertex hash -> round, so a lookup does not scan every round.
    index: HashMap<Hash, u64>,
    edges: HashMap<Hash, Vec<Hash>>,
    committed: HashSet<Hash>,
    config: ConsensusConfig,
}

impl DAG {
    pub fn new(config: ConsensusConfig) -> Self {
        Self {
            vertices: BTreeMap::new(),
            index: HashMap::new(),
            edges: HashMap::new(),
            committed: HashSet::new(),
            config,
        }
    }

    pub fn config(&self) -> &ConsensusConfig {
        &self.config
    }

    /// Admit a vertex, rejecting one whose parents are not already known.
    ///
    /// Round 1 is the genesis round and may have no parents; every later round
    /// must reference vertices this node already holds, so the DAG stays
    /// connected and a peer cannot inject dangling history.
    pub fn add_vertex(&mut self, vertex: DAGVertex) -> Result<Hash, ConsensusError> {
        if vertex.round == 0 {
            return Err(ConsensusError::InvalidVertex("round must be >= 1".into()));
        }
        let hash = vertex.hash();
        if self.index.contains_key(&hash) {
            return Ok(hash);
        }
        if vertex.round > 1 {
            if vertex.parents.is_empty() {
                return Err(ConsensusError::InvalidVertex(
                    "a non-genesis vertex must reference parents".into(),
                ));
            }
            for parent in &vertex.parents {
                if !self.index.contains_key(parent) {
                    return Err(ConsensusError::MissingParent(*parent));
                }
            }
        }

        let round = vertex.round;
        let parents = vertex.parents.clone();
        self.vertices.entry(round).or_default().push(vertex);
        self.index.insert(hash, round);
        self.edges.insert(hash, parents);
        debug!(%hash, round, "DAG vertex admitted");
        Ok(hash)
    }

    pub fn get_round(&self, round: u64) -> Vec<&DAGVertex> {
        self.vertices.get(&round).map(|vertices| vertices.iter().collect()).unwrap_or_default()
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        self.index.contains_key(hash)
    }

    pub fn vertex_round(&self, hash: &Hash) -> Option<u64> {
        self.index.get(hash).copied()
    }

    pub fn parents_of(&self, hash: &Hash) -> Option<&[Hash]> {
        self.edges.get(hash).map(Vec::as_slice)
    }

    pub fn latest_round(&self) -> u64 {
        self.vertices.keys().next_back().copied().unwrap_or(0)
    }

    pub fn vertex_count(&self) -> usize {
        self.index.len()
    }

    /// Distinct next-round authors that reference `vertex_hash` as a parent.
    ///
    /// This is *support for* the vertex. Counting the distinct authors among the
    /// vertex's own parents instead would measure its ancestry, which any vertex
    /// can satisfy with unrelated history.
    pub fn support_for(&self, vertex_hash: &Hash) -> usize {
        let Some(round) = self.vertex_round(vertex_hash) else {
            return 0;
        };
        let Some(next_round) = round.checked_add(1) else {
            return 0;
        };
        self.vertices
            .get(&next_round)
            .map(|vertices| {
                vertices
                    .iter()
                    .filter(|vertex| vertex.parents.contains(vertex_hash))
                    .map(|vertex| vertex.author)
                    .collect::<HashSet<_>>()
                    .len()
            })
            .unwrap_or(0)
    }

    /// Whether a quorum of distinct next-round authors supports this vertex.
    pub fn has_quorum_support(&self, vertex_hash: &Hash) -> bool {
        self.support_for(vertex_hash) >= self.config.quorum_threshold
    }

    /// Mark a vertex committed. Only a known, quorum-supported vertex qualifies.
    pub fn mark_committed(&mut self, vertex_hash: &Hash) -> Result<(), ConsensusError> {
        if !self.index.contains_key(vertex_hash) {
            return Err(ConsensusError::UnknownVertex(*vertex_hash));
        }
        if !self.has_quorum_support(vertex_hash) {
            return Err(ConsensusError::InsufficientSupport {
                have: self.support_for(vertex_hash),
                need: self.config.quorum_threshold,
            });
        }
        self.committed.insert(*vertex_hash);
        Ok(())
    }

    pub fn is_committed(&self, vertex_hash: &Hash) -> bool {
        self.committed.contains(vertex_hash)
    }

    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }

    /// Drop rounds older than `keep_rounds` behind the newest round.
    ///
    /// Uncommitted vertices are retained: garbage collecting a vertex that has
    /// not been committed would break the parent check for a peer that is behind.
    pub fn gc(&mut self, keep_rounds: u64) -> usize {
        let cutoff = self.latest_round().saturating_sub(keep_rounds);
        let mut removed = 0;
        let mut retained_index = HashMap::with_capacity(self.index.len());

        self.vertices.retain(|round, vertices| {
            if *round >= cutoff {
                return true;
            }
            vertices.retain(|vertex| {
                let hash = vertex.hash();
                let keep = !self.committed.contains(&hash);
                if keep {
                    retained_index.insert(hash, *round);
                }
                keep
            });
            !vertices.is_empty()
        });

        // Rebuild the auxiliary indexes from what actually survived.
        let live: HashSet<Hash> = self
            .vertices
            .values()
            .flat_map(|vertices| vertices.iter().map(DAGVertex::hash))
            .collect();
        self.index.retain(|hash, _| {
            let keep = live.contains(hash);
            if !keep {
                removed += 1;
            }
            keep
        });
        self.edges.retain(|hash, _| live.contains(hash));
        self.committed.retain(|hash| live.contains(hash));
        removed
    }
}

/// A quorum certificate over one block at one view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuorumCertificate {
    pub view: u64,
    pub block_hash: Hash,
    /// One `(validator, BLS signature)` pair per signer.
    pub signatures: Vec<(Address, Vec<u8>)>,
}

impl QuorumCertificate {
    /// The bytes every signer must have signed.
    ///
    /// Binding the view as well as the block hash stops a signature from being
    /// replayed into a different view.
    pub fn signing_payload(&self) -> Vec<u8> {
        Self::payload_for(self.view, &self.block_hash)
    }

    pub fn payload_for(view: u64, block_hash: &Hash) -> Vec<u8> {
        let mut payload = Vec::with_capacity(8 + subhost_core::HASH_SIZE + 16);
        payload.extend_from_slice(b"subhost-qc-v1");
        payload.extend_from_slice(&view.to_be_bytes());
        payload.extend_from_slice(block_hash.as_bytes());
        payload
    }
}

/// HotStuff view state and QC validation.
pub struct HotStuff {
    view: u64,
    high_qc: Option<QuorumCertificate>,
    /// Highest view this replica has locked on; a proposal below it is unsafe.
    locked_view: u64,
    config: ConsensusConfig,
    registry: ValidatorRegistry,
}

impl HotStuff {
    pub fn new(config: ConsensusConfig, registry: ValidatorRegistry) -> Self {
        Self { view: 0, high_qc: None, locked_view: 0, config, registry }
    }

    pub fn view(&self) -> u64 {
        self.view
    }

    pub fn locked_view(&self) -> u64 {
        self.locked_view
    }

    pub fn high_qc(&self) -> Option<&QuorumCertificate> {
        self.high_qc.as_ref()
    }

    pub fn config(&self) -> &ConsensusConfig {
        &self.config
    }

    pub fn registry(&self) -> &ValidatorRegistry {
        &self.registry
    }

    /// Advance to the next view.
    pub fn enter_next_view(&mut self) -> Result<u64, ConsensusError> {
        self.view = self
            .view
            .checked_add(1)
            .ok_or_else(|| ConsensusError::InvalidProposal("view counter overflow".into()))?;
        Ok(self.view)
    }

    /// Validate a quorum certificate.
    ///
    /// Every signature is checked against the registered BLS key for its signer
    /// over [`QuorumCertificate::signing_payload`]. A QC is accepted only when at
    /// least `quorum_threshold` *distinct, registered* validators each contribute
    /// a valid signature. Unknown signers, duplicates, and malformed signature
    /// bytes are all rejected.
    pub fn validate_qc(&self, qc: &QuorumCertificate) -> Result<(), ConsensusError> {
        if self.registry.is_empty() {
            return Err(ConsensusError::EmptyValidatorRegistry);
        }

        let payload = qc.signing_payload();
        let mut verified: HashSet<Address> = HashSet::with_capacity(qc.signatures.len());

        for (address, signature_bytes) in &qc.signatures {
            if signature_bytes.len() != BLS_SIGNATURE_BYTES {
                return Err(ConsensusError::MalformedSignature(*address));
            }
            let public_key = self
                .registry
                .public_key(address)
                .ok_or(ConsensusError::UnknownValidator(*address))?;
            let signature = Signature::deserialize_compressed(signature_bytes.as_slice())
                .map_err(|_| ConsensusError::MalformedSignature(*address))?;
            if !BLSScheme::verify(public_key, &payload, &signature) {
                return Err(ConsensusError::InvalidSignature(*address));
            }
            if !verified.insert(*address) {
                return Err(ConsensusError::DuplicateSigner(*address));
            }
        }

        if verified.len() < self.config.quorum_threshold {
            return Err(ConsensusError::InsufficientSupport {
                have: verified.len(),
                need: self.config.quorum_threshold,
            });
        }
        Ok(())
    }

    /// Adopt `qc` as the highest known certificate if it is valid and newer.
    ///
    /// Adopting a QC also locks its view, which is what makes
    /// [`Self::is_safe_proposal`] meaningful.
    pub fn update_high_qc(&mut self, qc: QuorumCertificate) -> Result<bool, ConsensusError> {
        self.validate_qc(&qc)?;
        let current_view = self.high_qc.as_ref().map_or(0, |existing| existing.view);
        if self.high_qc.is_some() && qc.view <= current_view {
            return Ok(false);
        }
        self.locked_view = qc.view;
        self.view = self.view.max(qc.view);
        self.high_qc = Some(qc);
        Ok(true)
    }

    /// The HotStuff safety rule: a proposal must not regress below the locked view.
    pub fn is_safe_proposal(&self, view: u64) -> bool {
        view > self.locked_view || self.high_qc.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConsensusError {
    #[error("invalid consensus configuration: {0}")]
    InvalidConfig(String),

    #[error("missing parent vertex: {0}")]
    MissingParent(Hash),

    #[error("unknown vertex: {0}")]
    UnknownVertex(Hash),

    #[error("invalid vertex: {0}")]
    InvalidVertex(String),

    #[error("insufficient quorum support: have {have}, need {need}")]
    InsufficientSupport { have: usize, need: usize },

    #[error("validator {0} is not registered")]
    UnknownValidator(Address),

    #[error("validator {0} signed more than once")]
    DuplicateSigner(Address),

    #[error("validator {0} is already registered")]
    DuplicateValidator(Address),

    #[error("validator {0} provided a malformed signature")]
    MalformedSignature(Address),

    #[error("validator {0} provided an invalid signature")]
    InvalidSignature(Address),

    #[error("validator {0} provided an invalid proof of possession")]
    InvalidProofOfPossession(Address),

    #[error("no validator public keys are registered")]
    EmptyValidatorRegistry,

    #[error("invalid proposal: {0}")]
    InvalidProposal(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_serialize::CanonicalSerialize;
    use subhost_core::{BlockHeader, BLOCK_GAS_LIMIT};
    use subhost_crypto::PrivateKey;

    fn address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn block(height: u64) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                chain_id: 1,
                height,
                timestamp: 1_700_000_000 + height,
                parent_hash: Hash::ZERO,
                state_root: Hash::ZERO,
                tx_root: Hash::ZERO,
                receipt_root: Hash::ZERO,
                validator: Address::ZERO,
                gas_used: 0,
                gas_limit: BLOCK_GAS_LIMIT,
                extra_data: Vec::new(),
            },
            transactions: Vec::new(),
            signatures: Vec::new(),
        }
    }

    fn vertex(author: u8, round: u64, parents: Vec<Hash>) -> DAGVertex {
        DAGVertex {
            author: address(author),
            round,
            block: block(round),
            parents,
            signature: Vec::new(),
        }
    }

    fn compress(signature: &Signature) -> Vec<u8> {
        let mut bytes = Vec::new();
        signature.serialize_compressed(&mut bytes).unwrap();
        bytes
    }

    /// A registry of `count` validators plus their signing keys.
    fn registry(count: u8) -> (ValidatorRegistry, Vec<(Address, PrivateKey)>) {
        let mut registry = ValidatorRegistry::new();
        let mut validators = Vec::new();
        for index in 1..=count {
            let (secret, public) = BLSScheme::keygen();
            let pop = BLSScheme::proof_of_possession(&secret);
            let addr = address(index);
            registry.register(addr, public, &pop).unwrap();
            validators.push((addr, secret));
        }
        (registry, validators)
    }

    fn certificate(
        view: u64,
        block_hash: Hash,
        signers: &[(Address, PrivateKey)],
    ) -> QuorumCertificate {
        let payload = QuorumCertificate::payload_for(view, &block_hash);
        QuorumCertificate {
            view,
            block_hash,
            signatures: signers
                .iter()
                .map(|(addr, secret)| (*addr, compress(&BLSScheme::sign(secret, &payload))))
                .collect(),
        }
    }

    #[test]
    fn quorum_sizing_follows_the_bft_bound() {
        for (validators, faulty, quorum) in [(1, 0, 1), (3, 0, 1), (4, 1, 3), (7, 2, 5), (10, 3, 7)]
        {
            let config = ConsensusConfig::new(validators).unwrap();
            assert_eq!(config.max_faulty, faulty, "n={validators}");
            assert_eq!(config.quorum_threshold, quorum, "n={validators}");
        }
        assert!(matches!(ConsensusConfig::new(0), Err(ConsensusError::InvalidConfig(_))));
    }

    #[test]
    fn vertex_hash_commits_to_content_not_debug_formatting() {
        let base = vertex(1, 1, Vec::new());
        assert_eq!(base.hash(), vertex(1, 1, Vec::new()).hash());
        assert_ne!(base.hash(), vertex(2, 1, Vec::new()).hash(), "author matters");
        assert_ne!(base.hash(), vertex(1, 2, Vec::new()).hash(), "round matters");

        // Parent order must not change identity.
        let first = Hash::from_data(b"a");
        let second = Hash::from_data(b"b");
        assert_eq!(
            vertex(1, 2, vec![first, second]).hash(),
            vertex(1, 2, vec![second, first]).hash()
        );
        assert_ne!(vertex(1, 2, vec![first]).hash(), vertex(1, 2, vec![first, second]).hash());
    }

    #[test]
    fn dag_admission_requires_known_parents() {
        let mut dag = DAG::new(ConsensusConfig::new(4).unwrap());
        let genesis = dag.add_vertex(vertex(1, 1, Vec::new())).unwrap();
        assert_eq!(dag.vertex_count(), 1);
        assert!(dag.contains(&genesis));
        assert_eq!(dag.vertex_round(&genesis), Some(1));

        // Re-admitting is idempotent.
        assert_eq!(dag.add_vertex(vertex(1, 1, Vec::new())).unwrap(), genesis);
        assert_eq!(dag.vertex_count(), 1);

        // A dangling parent is refused.
        let orphan = Hash::from_data(b"orphan");
        assert!(matches!(
            dag.add_vertex(vertex(2, 2, vec![orphan])),
            Err(ConsensusError::MissingParent(_))
        ));

        // Round 0 and a parentless non-genesis round are both invalid.
        assert!(matches!(
            dag.add_vertex(vertex(1, 0, Vec::new())),
            Err(ConsensusError::InvalidVertex(_))
        ));
        assert!(matches!(
            dag.add_vertex(vertex(2, 2, Vec::new())),
            Err(ConsensusError::InvalidVertex(_))
        ));

        let child = dag.add_vertex(vertex(2, 2, vec![genesis])).unwrap();
        assert_eq!(dag.parents_of(&child), Some([genesis].as_slice()));
        assert_eq!(dag.latest_round(), 2);
        assert_eq!(dag.get_round(2).len(), 1);
    }

    #[test]
    fn quorum_support_counts_distinct_next_round_authors() {
        let mut dag = DAG::new(ConsensusConfig::new(4).unwrap());
        let target = dag.add_vertex(vertex(1, 1, Vec::new())).unwrap();
        assert_eq!(dag.support_for(&target), 0);

        // The same author twice is one supporter.
        dag.add_vertex(vertex(2, 2, vec![target])).unwrap();
        let mut duplicate = vertex(2, 2, vec![target]);
        duplicate.block = block(99);
        dag.add_vertex(duplicate).unwrap();
        assert_eq!(dag.support_for(&target), 1);
        assert!(!dag.has_quorum_support(&target));

        dag.add_vertex(vertex(3, 2, vec![target])).unwrap();
        assert_eq!(dag.support_for(&target), 2);
        assert!(!dag.has_quorum_support(&target), "quorum is 3 of 4");

        dag.add_vertex(vertex(4, 2, vec![target])).unwrap();
        assert_eq!(dag.support_for(&target), 3);
        assert!(dag.has_quorum_support(&target));

        // An unknown vertex has no support.
        assert_eq!(dag.support_for(&Hash::from_data(b"missing")), 0);
    }

    #[test]
    fn ancestry_alone_does_not_create_support() {
        // A vertex with three distinct parents has fan-in 3 but no supporters,
        // which is the bug the previous implementation had.
        let mut dag = DAG::new(ConsensusConfig::new(4).unwrap());
        let parents: Vec<Hash> =
            (1..=3).map(|author| dag.add_vertex(vertex(author, 1, Vec::new())).unwrap()).collect();
        let child = dag.add_vertex(vertex(4, 2, parents)).unwrap();
        assert_eq!(dag.support_for(&child), 0);
        assert!(!dag.has_quorum_support(&child));
    }

    #[test]
    fn commit_requires_quorum_support_and_a_known_vertex() {
        let mut dag = DAG::new(ConsensusConfig::new(4).unwrap());
        let target = dag.add_vertex(vertex(1, 1, Vec::new())).unwrap();

        assert!(matches!(
            dag.mark_committed(&Hash::from_data(b"nope")),
            Err(ConsensusError::UnknownVertex(_))
        ));
        assert!(matches!(
            dag.mark_committed(&target),
            Err(ConsensusError::InsufficientSupport { have: 0, need: 3 })
        ));

        for author in 2..=4 {
            dag.add_vertex(vertex(author, 2, vec![target])).unwrap();
        }
        dag.mark_committed(&target).unwrap();
        assert!(dag.is_committed(&target));
        assert_eq!(dag.committed_count(), 1);
    }

    #[test]
    fn gc_drops_old_committed_rounds_and_keeps_uncommitted_history() {
        let mut dag = DAG::new(ConsensusConfig::new(1).unwrap());
        let first = dag.add_vertex(vertex(1, 1, Vec::new())).unwrap();
        dag.add_vertex(vertex(1, 2, vec![first])).unwrap();
        dag.mark_committed(&first).unwrap();
        for round in 3..=8 {
            let parent = dag.get_round(round - 1)[0].hash();
            dag.add_vertex(vertex(1, round, vec![parent])).unwrap();
        }

        let before = dag.vertex_count();
        let removed = dag.gc(3);
        assert!(removed > 0, "old committed rounds must be collected");
        assert_eq!(dag.vertex_count(), before - removed);
        assert!(!dag.contains(&first), "a committed round-1 vertex is collectable");
        assert_eq!(dag.latest_round(), 8);
        // Uncommitted round 2 is retained even though it is below the cutoff.
        assert_eq!(dag.get_round(2).len(), 1);
        // Indexes stay consistent with what survived.
        for round in 2..=8 {
            for vertex in dag.get_round(round) {
                assert!(dag.contains(&vertex.hash()));
                assert!(dag.parents_of(&vertex.hash()).is_some());
            }
        }
    }

    #[test]
    fn registry_requires_a_valid_proof_of_possession() {
        let mut registry = ValidatorRegistry::new();
        let (secret, public) = BLSScheme::keygen();
        let (other_secret, _) = BLSScheme::keygen();

        // A PoP from a different key is refused: this is the rogue-key defence.
        assert!(matches!(
            registry.register(address(1), public, &BLSScheme::proof_of_possession(&other_secret)),
            Err(ConsensusError::InvalidProofOfPossession(_))
        ));
        assert!(registry.is_empty());

        let pop = BLSScheme::proof_of_possession(&secret);
        registry.register(address(1), public, &pop).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(&address(1)));
        assert!(registry.public_key(&address(2)).is_none());

        assert!(matches!(
            registry.register(address(1), public, &pop),
            Err(ConsensusError::DuplicateValidator(_))
        ));
    }

    #[test]
    fn qc_validation_accepts_a_real_quorum() {
        let (registry, validators) = registry(4);
        let hotstuff = HotStuff::new(ConsensusConfig::new(4).unwrap(), registry);
        let qc = certificate(1, Hash::from_data(b"block"), &validators[..3]);
        assert!(hotstuff.validate_qc(&qc).is_ok());
    }

    #[test]
    fn qc_validation_rejects_forgeries_and_shortfalls() {
        let (registry, validators) = registry(4);
        let hotstuff = HotStuff::new(ConsensusConfig::new(4).unwrap(), registry);
        let block_hash = Hash::from_data(b"block");

        // Too few signers.
        assert!(matches!(
            hotstuff.validate_qc(&certificate(1, block_hash, &validators[..2])),
            Err(ConsensusError::InsufficientSupport { have: 2, need: 3 })
        ));

        // Empty signatures never satisfy a quorum.
        assert!(hotstuff
            .validate_qc(&QuorumCertificate { view: 1, block_hash, signatures: Vec::new() })
            .is_err());

        // Zero-length signature bytes are malformed, not "trusted".
        assert!(matches!(
            hotstuff.validate_qc(&QuorumCertificate {
                view: 1,
                block_hash,
                signatures: validators[..3].iter().map(|(addr, _)| (*addr, Vec::new())).collect(),
            }),
            Err(ConsensusError::MalformedSignature(_))
        ));

        // Right length, wrong bytes.
        assert!(matches!(
            hotstuff.validate_qc(&QuorumCertificate {
                view: 1,
                block_hash,
                signatures: validators[..3]
                    .iter()
                    .map(|(addr, _)| (*addr, vec![0u8; BLS_SIGNATURE_BYTES]))
                    .collect(),
            }),
            Err(ConsensusError::MalformedSignature(_) | ConsensusError::InvalidSignature(_))
        ));

        // One signer repeated three times is not a quorum.
        let mut repeated = certificate(1, block_hash, &validators[..1]);
        let single = repeated.signatures[0].clone();
        repeated.signatures = vec![single.clone(), single.clone(), single];
        assert!(matches!(hotstuff.validate_qc(&repeated), Err(ConsensusError::DuplicateSigner(_))));

        // An unregistered validator cannot contribute.
        let (unknown_secret, _) = BLSScheme::keygen();
        let mut foreign = certificate(1, block_hash, &validators[..2]);
        let payload = QuorumCertificate::payload_for(1, &block_hash);
        foreign
            .signatures
            .push((address(99), compress(&BLSScheme::sign(&unknown_secret, &payload))));
        assert!(matches!(hotstuff.validate_qc(&foreign), Err(ConsensusError::UnknownValidator(_))));

        // A signature over a different block must not validate.
        let wrong_block = certificate(1, Hash::from_data(b"other"), &validators[..3]);
        assert!(matches!(
            hotstuff.validate_qc(&QuorumCertificate { block_hash, ..wrong_block }),
            Err(ConsensusError::InvalidSignature(_))
        ));

        // A signature from another view must not be replayable.
        let other_view = certificate(9, block_hash, &validators[..3]);
        assert!(matches!(
            hotstuff.validate_qc(&QuorumCertificate { view: 1, ..other_view }),
            Err(ConsensusError::InvalidSignature(_))
        ));
    }

    #[test]
    fn qc_validation_fails_closed_without_a_registry() {
        let hotstuff = HotStuff::new(ConsensusConfig::new(4).unwrap(), ValidatorRegistry::new());
        assert!(matches!(
            hotstuff.validate_qc(&QuorumCertificate {
                view: 1,
                block_hash: Hash::ZERO,
                signatures: Vec::new(),
            }),
            Err(ConsensusError::EmptyValidatorRegistry)
        ));
    }

    #[test]
    fn high_qc_only_advances_and_enforces_the_safety_rule() {
        let (registry, validators) = registry(4);
        let mut hotstuff = HotStuff::new(ConsensusConfig::new(4).unwrap(), registry);
        assert_eq!(hotstuff.locked_view(), 0);
        assert!(hotstuff.is_safe_proposal(0), "no lock yet");

        let view_five = certificate(5, Hash::from_data(b"five"), &validators[..3]);
        assert!(hotstuff.update_high_qc(view_five).unwrap());
        assert_eq!(hotstuff.high_qc().unwrap().view, 5);
        assert_eq!(hotstuff.locked_view(), 5);
        assert_eq!(hotstuff.view(), 5);

        // An older certificate is ignored, not adopted.
        let view_three = certificate(3, Hash::from_data(b"three"), &validators[..3]);
        assert!(!hotstuff.update_high_qc(view_three).unwrap());
        assert_eq!(hotstuff.high_qc().unwrap().view, 5);

        // The same view is not newer.
        let same_view = certificate(5, Hash::from_data(b"other-five"), &validators[..3]);
        assert!(!hotstuff.update_high_qc(same_view).unwrap());

        // Safety: nothing at or below the locked view may be proposed.
        assert!(!hotstuff.is_safe_proposal(5));
        assert!(!hotstuff.is_safe_proposal(4));
        assert!(hotstuff.is_safe_proposal(6));

        // An invalid certificate cannot move the lock.
        assert!(hotstuff
            .update_high_qc(certificate(9, Hash::from_data(b"nine"), &validators[..1]))
            .is_err());
        assert_eq!(hotstuff.locked_view(), 5);

        assert_eq!(hotstuff.enter_next_view().unwrap(), 6);
    }
}
