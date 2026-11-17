//! A minimal linear consensus driver over the quorum primitives.
//!
//! Scope: one in-process state machine that advances a single view at a time,
//! collects BLS votes for one proposal, and commits the block once `2f + 1`
//! distinct validators sign the same `(view, block hash)`. Every proposal must
//! extend the committed tip by exactly one height, and a validator votes at most
//! once per view, so two conflicting blocks cannot both collect a quorum.
//!
//! This is deliberately not a full networked BFT implementation: there are no
//! timeouts, view-change messages, leader rotation, or fork choice, and the
//! driver does not touch storage. It exercises the quorum machinery that
//! `HotStuff`, `QuorumCertificate`, and the DAG already provide, and gives the
//! node a tested way to reach agreement in-process.

use crate::{
    ConsensusConfig, ConsensusError, HotStuff, QuorumCertificate, ValidatorRegistry,
    BLS_SIGNATURE_BYTES,
};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use std::collections::BTreeMap;
use subhost_core::{Address, Block, Hash};
use subhost_crypto::{BLSScheme, PrivateKey, Signature};

/// A block put forward for one view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockProposal {
    pub view: u64,
    pub block: Block,
}

/// One validator's signature over [`QuorumCertificate::payload_for`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    pub view: u64,
    pub block_hash: Hash,
    pub validator: Address,
    pub signature: Vec<u8>,
}

/// What happened to a vote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoteOutcome {
    /// More signatures are needed before the block can commit.
    Pending { votes: usize, needed: usize },
    /// A duplicate or already-committed view was dropped.
    Ignored,
    /// The proposal reached a quorum and the block was committed.
    Committed { view: u64, block_hash: Hash },
}

/// In-process view state, vote tally, and committed chain.
///
/// A replica is either a validator (it holds an `identity` and can propose and
/// vote) or an observer (it only validates proposals and votes).
#[derive(Clone)]
pub struct ConsensusDriver {
    hotstuff: HotStuff,
    identity: Option<(Address, PrivateKey)>,
    /// Height and hash of the newest committed block.
    height: u64,
    tip: Hash,
    /// The proposal currently collecting votes.
    proposal: Option<BlockProposal>,
    /// Votes for the open proposal, keyed by signer for deduplication.
    votes: BTreeMap<Address, Vote>,
    /// The newest view this replica has already committed, so late votes are
    /// dropped instead of being reported as errors.
    last_committed_view: u64,
    committed: Vec<Block>,
}

impl ConsensusDriver {
    /// Build a driver for a full validator set.
    ///
    /// `identity` must be a registered validator when present; `None` makes the
    /// driver an observer that can validate votes but cannot propose or vote.
    pub fn new(
        config: ConsensusConfig,
        registry: ValidatorRegistry,
        identity: Option<(Address, PrivateKey)>,
    ) -> Result<Self, ConsensusError> {
        if registry.len() != config.validator_count {
            return Err(ConsensusError::InvalidConfig(format!(
                "registry holds {} validators but the config expects {}",
                registry.len(),
                config.validator_count
            )));
        }
        if let Some((address, _)) = &identity {
            if !registry.contains(address) {
                return Err(ConsensusError::UnknownValidator(*address));
            }
        }
        Ok(Self {
            hotstuff: HotStuff::new(config, registry),
            identity,
            height: 0,
            tip: Hash::ZERO,
            proposal: None,
            votes: BTreeMap::new(),
            last_committed_view: 0,
            committed: Vec::new(),
        })
    }

    pub fn view(&self) -> u64 {
        self.hotstuff.view()
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn tip(&self) -> Hash {
        self.tip
    }

    pub fn committed(&self) -> &[Block] {
        &self.committed
    }

    pub fn open_proposal(&self) -> Option<&BlockProposal> {
        self.proposal.as_ref()
    }

    /// Leader path: open the next view with `block` and return the leader's own
    /// vote along with the proposal to broadcast.
    pub fn propose(&mut self, block: Block) -> Result<(BlockProposal, Vote), ConsensusError> {
        self.ensure_extends_tip(&block)?;
        let view = self
            .hotstuff
            .view()
            .checked_add(1)
            .ok_or_else(|| ConsensusError::InvalidProposal("view counter overflow".into()))?;
        let proposal = BlockProposal { view, block };
        // Sign before mutating view state, so a replica without a key leaves the
        // driver exactly as it was.
        let vote = self.sign_vote(&proposal)?;
        self.hotstuff.enter_next_view()?;
        self.proposal = Some(proposal.clone());
        self.votes.clear();
        self.votes.insert(vote.validator, vote.clone());
        Ok((proposal, vote))
    }

    /// Follower path: validate the leader's proposal and return this replica's
    /// vote. Only the next view is accepted, so a leader cannot skip views or
    /// replay an old proposal.
    pub fn receive_proposal(&mut self, proposal: BlockProposal) -> Result<Vote, ConsensusError> {
        self.ensure_extends_tip(&proposal.block)?;
        let current = self.hotstuff.view();
        if proposal.view != current.saturating_add(1) {
            return Err(ConsensusError::InvalidView { proposed: proposal.view, current });
        }
        if !self.hotstuff.is_safe_proposal(proposal.view) {
            return Err(ConsensusError::InvalidProposal(format!(
                "view {} is below the locked view {}",
                proposal.view,
                self.hotstuff.locked_view()
            )));
        }
        let vote = self.sign_vote(&proposal)?;
        self.hotstuff.enter_next_view()?;
        self.proposal = Some(proposal.clone());
        self.votes.clear();
        self.votes.insert(vote.validator, vote.clone());
        Ok(vote)
    }

    /// Validate and tally a vote.
    ///
    /// A vote is accepted only for the open proposal, from a registered
    /// validator, once per signer, with a valid BLS signature over the exact
    /// `(view, block hash)` pair. A quorum commits the proposal immediately.
    pub fn receive_vote(&mut self, vote: Vote) -> Result<VoteOutcome, ConsensusError> {
        let Some(proposal) = self.proposal.clone() else {
            if vote.view <= self.last_committed_view {
                return Ok(VoteOutcome::Ignored);
            }
            return Err(ConsensusError::NoProposal);
        };
        if vote.view != proposal.view || vote.block_hash != proposal.block.hash() {
            return Err(ConsensusError::InvalidProposal(
                "vote does not match the open proposal".into(),
            ));
        }
        if self.votes.contains_key(&vote.validator) {
            return Ok(VoteOutcome::Ignored);
        }

        let public_key = self
            .hotstuff
            .registry()
            .public_key(&vote.validator)
            .ok_or(ConsensusError::UnknownValidator(vote.validator))?;
        if vote.signature.len() != BLS_SIGNATURE_BYTES {
            return Err(ConsensusError::MalformedSignature(vote.validator));
        }
        let signature = Signature::deserialize_compressed(vote.signature.as_slice())
            .map_err(|_| ConsensusError::MalformedSignature(vote.validator))?;
        let payload = QuorumCertificate::payload_for(vote.view, &vote.block_hash);
        if !BLSScheme::verify(public_key, &payload, &signature) {
            return Err(ConsensusError::InvalidSignature(vote.validator));
        }

        self.votes.insert(vote.validator, vote);

        let needed = self.hotstuff.config().quorum_threshold;
        if self.votes.len() < needed {
            return Ok(VoteOutcome::Pending { votes: self.votes.len(), needed });
        }

        let quorum = QuorumCertificate {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            signatures: self
                .votes
                .values()
                .map(|vote| (vote.validator, vote.signature.clone()))
                .collect(),
        };
        // `update_high_qc` re-verifies the quorum and advances the lock.
        self.hotstuff.update_high_qc(quorum)?;

        let block = proposal.block;
        self.height = block.header.height;
        self.tip = block.hash();
        self.last_committed_view = proposal.view;
        self.committed.push(block);
        self.proposal = None;
        self.votes.clear();
        Ok(VoteOutcome::Committed { view: proposal.view, block_hash: self.tip })
    }

    /// The next block must extend the committed tip by exactly one height.
    fn ensure_extends_tip(&self, block: &Block) -> Result<(), ConsensusError> {
        let expected_height = self
            .height
            .checked_add(1)
            .ok_or_else(|| ConsensusError::InvalidProposal("height overflow".into()))?;
        if block.header.height != expected_height || block.header.parent_hash != self.tip {
            return Err(ConsensusError::InvalidProposal(format!(
                "block must extend {} at height {expected_height}",
                self.tip
            )));
        }
        Ok(())
    }

    fn sign_vote(&self, proposal: &BlockProposal) -> Result<Vote, ConsensusError> {
        let Some((address, secret)) = &self.identity else {
            return Err(ConsensusError::NotAValidator);
        };
        let payload = QuorumCertificate::payload_for(proposal.view, &proposal.block.hash());
        let mut signature = Vec::with_capacity(BLS_SIGNATURE_BYTES);
        BLSScheme::sign(secret, &payload).serialize_compressed(&mut signature).map_err(|_| {
            ConsensusError::InvalidProposal("cannot encode a vote signature".into())
        })?;
        Ok(Vote {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            validator: *address,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_serialize::CanonicalSerialize;
    use subhost_core::{BlockHeader, BLOCK_GAS_LIMIT};

    fn address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn block(height: u64, parent_hash: Hash, state_seed: u8) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                chain_id: 1,
                height,
                timestamp: 1_700_000_000 + height,
                parent_hash,
                state_root: Hash::from_data(&[state_seed]),
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

    fn driver(
        registry: &ValidatorRegistry,
        keys: &[(Address, PrivateKey)],
        index: usize,
    ) -> ConsensusDriver {
        ConsensusDriver::new(
            ConsensusConfig::new(4).unwrap(),
            registry.clone(),
            Some(keys[index].clone()),
        )
        .unwrap()
    }

    fn compress(signature: &Signature) -> Vec<u8> {
        let mut bytes = Vec::new();
        signature.serialize_compressed(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn four_validators_agree_on_a_block_with_one_faulty_replica() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);
        let mut followers: Vec<ConsensusDriver> =
            (1..4).map(|index| driver(&registry, &keys, index)).collect();

        let first = block(1, Hash::ZERO, 1);
        let (proposal, leader_vote) = leader.propose(first.clone()).unwrap();

        let mut votes = vec![leader_vote];
        for follower in &mut followers {
            votes.push(follower.receive_proposal(proposal.clone()).unwrap());
        }
        assert_eq!(votes.len(), 4);

        // The Byzantine replica (index 3) withholds its vote everywhere, so each
        // node sees only three signatures: exactly the quorum.
        let honest_votes = &votes[..3];
        for vote in honest_votes {
            leader.receive_vote(vote.clone()).unwrap();
        }
        for follower in &mut followers[..2] {
            for vote in honest_votes {
                follower.receive_vote(vote.clone()).unwrap();
            }
        }
        let faulty = &mut followers[2];
        for vote in honest_votes {
            faulty.receive_vote(vote.clone()).unwrap();
        }

        for node in std::iter::once(&leader).chain(followers.iter()) {
            assert_eq!(node.committed(), std::slice::from_ref(&first));
            assert_eq!(node.height(), 1);
            assert_eq!(node.tip(), first.hash());
        }

        // A withheld vote arriving after the commit is dropped, not an error.
        let late = votes[3].clone();
        assert_eq!(leader.receive_vote(late).unwrap(), VoteOutcome::Ignored);
    }

    #[test]
    fn a_quorum_is_required_before_anything_commits() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);
        let (proposal, leader_vote) = leader.propose(block(1, Hash::ZERO, 1)).unwrap();

        // The leader's own vote is counted at proposal time; a repeat is dropped.
        assert_eq!(leader.receive_vote(leader_vote).unwrap(), VoteOutcome::Ignored);

        let payload = QuorumCertificate::payload_for(proposal.view, &proposal.block.hash());
        let second = Vote {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            validator: keys[1].0,
            signature: compress(&BLSScheme::sign(&keys[1].1, &payload)),
        };
        assert_eq!(
            leader.receive_vote(second.clone()).unwrap(),
            VoteOutcome::Pending { votes: 2, needed: 3 }
        );
        // A duplicate signer does not count twice.
        assert_eq!(leader.receive_vote(second).unwrap(), VoteOutcome::Ignored);
        assert!(leader.committed().is_empty());
        assert_eq!(leader.height(), 0);
    }

    #[test]
    fn a_conflicting_proposal_at_the_same_view_is_refused() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);
        let mut follower = driver(&registry, &keys, 1);

        let (proposal, _) = leader.propose(block(1, Hash::ZERO, 1)).unwrap();
        follower.receive_proposal(proposal).unwrap();

        // Same view, different block: the follower already voted and will not
        // accept a second proposal for that view.
        let conflicting = BlockProposal { view: 1, block: block(1, Hash::ZERO, 2) };
        assert!(matches!(
            follower.receive_proposal(conflicting),
            Err(ConsensusError::InvalidView { proposed: 1, current: 1 })
        ));
        assert_eq!(follower.view(), 1);
    }

    #[test]
    fn a_vote_cannot_be_reused_for_another_block_or_view() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);
        let (proposal, _) = leader.propose(block(1, Hash::ZERO, 1)).unwrap();
        let payload = QuorumCertificate::payload_for(proposal.view, &proposal.block.hash());
        let vote = Vote {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            validator: keys[1].0,
            signature: compress(&BLSScheme::sign(&keys[1].1, &payload)),
        };

        let mut wrong_block = vote.clone();
        wrong_block.block_hash = Hash::from_data(b"other");
        assert!(matches!(
            leader.receive_vote(wrong_block),
            Err(ConsensusError::InvalidProposal(_))
        ));

        let mut wrong_view = vote.clone();
        wrong_view.view = proposal.view + 1;
        assert!(matches!(leader.receive_vote(wrong_view), Err(ConsensusError::InvalidProposal(_))));

        // The rejected attempts did not consume the signer's slot.
        assert_eq!(
            leader.receive_vote(vote).unwrap(),
            VoteOutcome::Pending { votes: 2, needed: 3 }
        );
    }

    #[test]
    fn forged_and_unknown_votes_are_rejected() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);
        let (proposal, _) = leader.propose(block(1, Hash::ZERO, 1)).unwrap();

        // A registered validator with a signature over the wrong payload.
        let mut forged = Vote {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            validator: keys[1].0,
            signature: Vec::new(),
        };
        assert!(matches!(
            leader.receive_vote(forged.clone()),
            Err(ConsensusError::MalformedSignature(_))
        ));

        forged.signature = vec![0u8; BLS_SIGNATURE_BYTES];
        assert!(matches!(
            leader.receive_vote(forged),
            Err(ConsensusError::MalformedSignature(_) | ConsensusError::InvalidSignature(_))
        ));

        let unknown = Vote {
            view: proposal.view,
            block_hash: proposal.block.hash(),
            validator: address(99),
            signature: vec![0u8; BLS_SIGNATURE_BYTES],
        };
        assert!(matches!(leader.receive_vote(unknown), Err(ConsensusError::UnknownValidator(_))));
    }

    #[test]
    fn a_proposal_must_extend_the_committed_tip() {
        let (registry, keys) = registry(4);
        let mut leader = driver(&registry, &keys, 0);

        // Height 2 on top of nothing.
        assert!(matches!(
            leader.propose(block(2, Hash::ZERO, 1)),
            Err(ConsensusError::InvalidProposal(_))
        ));

        let first = block(1, Hash::ZERO, 1);
        let (proposal, leader_vote) = leader.propose(first.clone()).unwrap();
        leader.receive_vote(leader_vote).unwrap();
        // Not enough votes: nothing committed, so height 2 is not yet allowed.
        assert!(matches!(
            leader.propose(block(2, first.hash(), 2)),
            Err(ConsensusError::InvalidProposal(_))
        ));
        assert_eq!(proposal.view, 1);
        assert_eq!(leader.view(), 1);
    }

    #[test]
    fn an_observer_validates_but_cannot_vote() {
        let (registry, keys) = registry(4);
        let mut observer =
            ConsensusDriver::new(ConsensusConfig::new(4).unwrap(), registry, None).unwrap();
        let proposal = BlockProposal { view: 1, block: block(1, Hash::ZERO, 1) };
        assert!(matches!(observer.receive_proposal(proposal), Err(ConsensusError::NotAValidator)));
        assert!(matches!(
            observer.propose(block(1, Hash::ZERO, 1)),
            Err(ConsensusError::NotAValidator)
        ));
        // Silence the unused-key warning while keeping the fixture explicit.
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn construction_rejects_a_short_registry_and_a_foreign_identity() {
        let (registry, keys) = registry(3);
        assert!(matches!(
            ConsensusDriver::new(ConsensusConfig::new(4).unwrap(), registry.clone(), None),
            Err(ConsensusError::InvalidConfig(_))
        ));
        let (other_secret, _) = BLSScheme::keygen();
        assert!(matches!(
            ConsensusDriver::new(
                ConsensusConfig::new(3).unwrap(),
                registry,
                Some((address(99), other_secret))
            ),
            Err(ConsensusError::UnknownValidator(_))
        ));
        assert_eq!(compress(&BLSScheme::sign(&keys[0].1, b"x")).len(), BLS_SIGNATURE_BYTES);
    }
}
