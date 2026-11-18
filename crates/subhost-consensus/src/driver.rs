//! Per-height, two-phase quorum consensus with authenticated leader rotation.
//!
//! Prepare certificates lock a block; only commit-phase certificates finalize it.
//! A timed-out view may be replaced only using a quorum of signed view-change
//! reports, whose highest prepared certificate determines the next proposal.
//! Callers validate block execution and durably save every checkpoint before
//! publishing signatures. This module neither executes blocks nor writes storage.
//!
//! With at most `f` faulty identities, a commit quorum leaves at least `n - 2f`
//! honest replicas locked on its prepared block. Any `n - f` view-change report
//! set intersects those replicas. Its highest prepared QC therefore carries the
//! committed block forward; a lower proof cannot unlock a replica. Signatures
//! from the prepare, commit, proposal, and view-change domains are distinct.
//! Registered identities have equal voting weight; genesis power is not a vote
//! multiplier. Liveness needs eventual message delivery, an honest rotating
//! leader, delivery within the configured capped timeout, and durable signing
//! state across process recovery. Timeout-only replicas synchronize using the
//! f+1-th highest authenticated target and cannot outrun lagging peers alone.
//! The protocol follows the prepare/commit and view-change structure described
//! in <https://pmg.csail.mit.edu/papers/osdi99.pdf>; it is not chained HotStuff.

use crate::{ConsensusConfig, ConsensusError, QuorumCertificate, ValidatorRegistry};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use subhost_core::{encode_canonical, Address, Block, Hash};
use subhost_crypto::{BLSScheme, PrivateKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VotePhase {
    Prepare,
    Commit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub view: u64,
    pub block_hash: Hash,
    pub validator: Address,
    pub signature: Vec<u8>,
    pub phase: VotePhase,
}

impl Vote {
    pub fn payload_for(phase: VotePhase, view: u64, block_hash: &Hash) -> Vec<u8> {
        Self::payload_for_context(Hash::ZERO, phase, view, block_hash)
    }

    pub fn payload_for_context(
        context: Hash,
        phase: VotePhase,
        view: u64,
        block_hash: &Hash,
    ) -> Vec<u8> {
        match phase {
            VotePhase::Prepare => {
                PrepareCertificate::payload_for_context(context, view, block_hash)
            }
            VotePhase::Commit => QuorumCertificate::payload_for_context(context, view, block_hash),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareCertificate {
    pub protocol_context: Hash,
    pub view: u64,
    pub block_hash: Hash,
    pub signatures: Vec<(Address, Vec<u8>)>,
}

impl PrepareCertificate {
    pub fn payload_for(view: u64, block_hash: &Hash) -> Vec<u8> {
        Self::payload_for_context(Hash::ZERO, view, block_hash)
    }

    pub fn payload_for_context(context: Hash, view: u64, block_hash: &Hash) -> Vec<u8> {
        let mut payload = b"subhost-prepare-v2".to_vec();
        payload.extend_from_slice(context.as_bytes());
        payload.extend_from_slice(&view.to_be_bytes());
        payload.extend_from_slice(block_hash.as_bytes());
        payload
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedBlock {
    pub block: Block,
    pub qc: PrepareCertificate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewChange {
    pub protocol_context: Hash,
    /// The target view, distinct from the block height.
    pub view: u64,
    /// The height being decided (committed height plus one).
    pub height: u64,
    pub tip: Hash,
    pub validator: Address,
    pub prepared: Option<PreparedBlock>,
    pub signature: Vec<u8>,
}

impl ViewChange {
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = b"subhost-view-change-v2".to_vec();
        // Bind the complete certificate as well as its block. An author cannot
        // swap in another report or remove its prepared obligation after signing.
        payload.extend_from_slice(&encode_canonical(&(
            self.protocol_context,
            self.view,
            self.height,
            self.tip,
            self.validator,
            self.prepared.as_ref().map(|p| Hash::from_data(&encode_canonical(p))),
        )));
        payload
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutCertificate {
    pub view: u64,
    pub height: u64,
    pub tip: Hash,
    pub messages: Vec<ViewChange>,
}

impl TimeoutCertificate {
    pub fn highest_prepared(&self) -> Option<&PreparedBlock> {
        self.messages.iter().filter_map(|m| m.prepared.as_ref()).max_by_key(|p| p.qc.view)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockProposal {
    pub protocol_context: Hash,
    pub view: u64,
    pub block: Block,
    /// The current leader may repropose a block signed by an earlier producer.
    pub proposer: Address,
    pub signature: Vec<u8>,
    pub timeout_certificate: Option<TimeoutCertificate>,
}

impl BlockProposal {
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = b"subhost-proposal-v2".to_vec();
        payload.extend_from_slice(&encode_canonical(&(
            self.protocol_context,
            self.view,
            self.block.hash(),
            self.proposer,
            self.timeout_certificate.as_ref().map(|tc| Hash::from_data(&encode_canonical(tc))),
        )));
        payload
    }
}

/// Key-free state; must be saved before any vote, proposal or timeout is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverCheckpoint {
    pub height: u64,
    pub tip: Hash,
    pub high_qc: Option<QuorumCertificate>,
    pub proposal: Option<BlockProposal>,
    pub votes: Vec<Vote>,
    pub view: u64,
    pub prepared: Option<PreparedBlock>,
    pub timeout_certificate: Option<TimeoutCertificate>,
    pub view_changes: Vec<ViewChange>,
    pub own_view_change: Option<ViewChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoteOutcome {
    Pending { votes: usize, needed: usize },
    Ignored,
    Committed { view: u64, block_hash: Hash },
}

#[derive(Clone)]
pub struct ConsensusDriver {
    config: ConsensusConfig,
    registry: ValidatorRegistry,
    identity: Option<(Address, PrivateKey)>,
    leaders: Vec<Address>,
    height: u64,
    tip: Hash,
    view: u64,
    high_qc: Option<QuorumCertificate>,
    proposal: Option<BlockProposal>,
    prepares: BTreeMap<Address, Vote>,
    commits: BTreeMap<Address, Vote>,
    prepared: Option<PreparedBlock>,
    certificate: Option<TimeoutCertificate>,
    /// One highest authenticated progress report per signer, including reports
    /// below our local view. Keeping lower reports lets restarted peers align.
    changes: BTreeMap<Address, ViewChange>,
    own_change: Option<ViewChange>,
    started_at: Option<Duration>,
    timeout_attempts: u32,
    committed: Vec<Block>,
    /// Successful checks only, scoped to this immutable registry. This cache is
    /// never serialized; recovery authenticates every restored obligation again.
    verified_signatures: RefCell<BTreeSet<Hash>>,
}

fn invalid(detail: &str) -> ConsensusError {
    ConsensusError::InvalidProposal(detail.into())
}

impl ConsensusDriver {
    pub fn new(
        config: ConsensusConfig,
        registry: ValidatorRegistry,
        identity: Option<(Address, PrivateKey)>,
    ) -> Result<Self, ConsensusError> {
        Self::with_tip(config, registry, identity, 0, Hash::ZERO)
    }

    /// Resume a trusted ledger anchor. Recover signing obligations with
    /// `from_checkpoint`; a ledger alone cannot reconstruct timed-out votes.
    pub fn with_tip(
        config: ConsensusConfig,
        registry: ValidatorRegistry,
        identity: Option<(Address, PrivateKey)>,
        height: u64,
        tip: Hash,
    ) -> Result<Self, ConsensusError> {
        let expected = ConsensusConfig::new(config.validator_count)?;
        if registry.len() != config.validator_count
            || config.quorum_threshold != expected.quorum_threshold
            || config.max_faulty != expected.max_faulty
            || config.view_timeout_ms == 0
        {
            return Err(ConsensusError::InvalidConfig(
                "invalid registry, quorum or timeout".into(),
            ));
        }
        if (height == 0) != (tip == Hash::ZERO) {
            return Err(ConsensusError::InvalidConfig("height and tip disagree".into()));
        }
        if let Some((address, secret)) = &identity {
            let key =
                registry.public_key(address).ok_or(ConsensusError::UnknownValidator(*address))?;
            if *key != BLSScheme::public_key(secret) {
                return Err(ConsensusError::InvalidConfig(
                    "validator identity key mismatch".into(),
                ));
            }
        }
        let mut leaders: Vec<_> = registry.keys.keys().copied().collect();
        leaders.sort_unstable();
        Ok(Self {
            config,
            registry,
            identity,
            leaders,
            height,
            tip,
            view: height.checked_add(1).ok_or_else(|| invalid("view counter overflow"))?,
            high_qc: None,
            proposal: None,
            prepares: BTreeMap::new(),
            commits: BTreeMap::new(),
            prepared: None,
            certificate: None,
            changes: BTreeMap::new(),
            own_change: None,
            started_at: None,
            timeout_attempts: 0,
            committed: Vec::new(),
            verified_signatures: RefCell::new(BTreeSet::new()),
        })
    }

    pub fn checkpoint(&self) -> DriverCheckpoint {
        DriverCheckpoint {
            height: self.height,
            tip: self.tip,
            high_qc: self.high_qc.clone(),
            proposal: self.proposal.clone(),
            votes: self.prepares.values().chain(self.commits.values()).cloned().collect(),
            view: self.view,
            prepared: self.prepared.clone(),
            timeout_certificate: self.certificate.clone(),
            view_changes: self.changes.values().cloned().collect(),
            own_view_change: self.own_change.clone(),
        }
    }

    pub fn from_checkpoint(
        config: ConsensusConfig,
        registry: ValidatorRegistry,
        identity: Option<(Address, PrivateKey)>,
        checkpoint: DriverCheckpoint,
    ) -> Result<Self, ConsensusError> {
        let mut driver =
            Self::with_tip(config, registry, identity, checkpoint.height, checkpoint.tip)?;
        if let Some(qc) = checkpoint.high_qc {
            if driver.height == 0 || qc.block_hash != driver.tip || qc.view < driver.height {
                return Err(invalid("saved final QC does not match the tip"));
            }
            driver.validate_commit_certificate(&qc)?;
            driver.high_qc = Some(qc);
        }
        if checkpoint.view < driver.initial_view()? {
            return Err(invalid("saved view regresses below the committed anchor"));
        }
        driver.view = checkpoint.view;
        if let Some(prepared) = checkpoint.prepared {
            driver.validate_prepared(&prepared)?;
            if prepared.qc.view > driver.view {
                return Err(invalid("saved lock is ahead of the saved view"));
            }
            driver.prepared = Some(prepared);
        }
        if let Some(tc) = checkpoint.timeout_certificate {
            driver.validate_timeout_certificate(&tc)?;
            if tc.view != driver.view {
                return Err(invalid("saved timeout certificate has a different view"));
            }
            driver.certificate = Some(tc);
        }
        if let Some(proposal) = checkpoint.proposal {
            // A prepared certificate independently authorizes its own proposal;
            // it may have arrived without the original leader envelope.
            let certified = driver
                .prepared
                .as_ref()
                .is_some_and(|p| p.qc.view == proposal.view && p.block == proposal.block);
            if proposal.view != driver.view {
                return Err(invalid("saved proposal is outside the active view"));
            }
            let unsigned_prepared_placeholder = certified
                && proposal.signature.is_empty()
                && proposal.timeout_certificate.is_none()
                && proposal.proposer == driver.leader_for(proposal.view)
                && proposal.protocol_context == driver.config.protocol_context;
            if !unsigned_prepared_placeholder {
                driver.validate_proposal(&proposal)?;
            }
            driver.ensure_extends_tip(&proposal.block)?;
            driver.proposal = Some(proposal);
        } else if !checkpoint.votes.is_empty() {
            return Err(invalid("saved votes have no proposal"));
        }
        for vote in checkpoint.votes {
            let proposal =
                driver.proposal.as_ref().ok_or_else(|| invalid("saved vote has no proposal"))?;
            if vote.view != proposal.view || vote.block_hash != proposal.block.hash() {
                return Err(invalid("saved vote does not match the proposal"));
            }
            driver.validate_vote(&vote)?;
            let tally = match vote.phase {
                VotePhase::Prepare => &mut driver.prepares,
                VotePhase::Commit => &mut driver.commits,
            };
            if tally.insert(vote.validator, vote).is_some() {
                return Err(invalid("duplicate saved phase vote"));
            }
        }
        if let Some((address, _)) = &driver.identity {
            if driver.proposal.is_some()
                && !driver.prepares.contains_key(address)
                && !driver.commits.contains_key(address)
            {
                return Err(invalid("saved proposal is missing our signing obligation"));
            }
            if driver.commits.contains_key(address)
                && !driver.prepared.as_ref().is_some_and(|p| {
                    p.qc.view == driver.view
                        && driver
                            .proposal
                            .as_ref()
                            .is_some_and(|proposal| p.block == proposal.block)
                })
            {
                return Err(invalid("saved own commit vote is missing its prepare certificate"));
            }
        }
        if checkpoint.view_changes.len() > driver.config.validator_count {
            return Err(invalid("too many saved view-change messages"));
        }
        for change in checkpoint.view_changes {
            driver.validate_view_change(&change)?;
            driver.check_report_compatibility(&change)?;
            if driver.changes.insert(change.validator, change).is_some() {
                return Err(invalid("duplicate saved view-change signer"));
            }
        }
        if let Some(change) = checkpoint.own_view_change {
            driver.validate_view_change(&change)?;
            if driver.identity.as_ref().map(|(a, _)| *a) != Some(change.validator)
                || change.view != driver.view
            {
                return Err(invalid("saved own view-change message has wrong signer or view"));
            }
            if driver.changes.get(&change.validator) != Some(&change) {
                return Err(invalid(
                    "saved own view-change message is missing its retained report",
                ));
            }
            driver.own_change = Some(change);
        }
        Ok(driver)
    }

    pub fn view(&self) -> u64 {
        self.view
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
    pub fn committed_qc(&self) -> Option<&QuorumCertificate> {
        self.high_qc.as_ref()
    }
    pub fn is_validator(&self) -> bool {
        self.identity.is_some()
    }
    pub fn leader(&self) -> Address {
        self.leader_for(self.view)
    }
    pub fn is_leader(&self) -> bool {
        self.identity.as_ref().is_some_and(|(address, _)| *address == self.leader())
    }
    pub fn own_vote(&self) -> Option<&Vote> {
        self.identity.as_ref().and_then(|(address, _)| self.prepares.get(address))
    }
    pub fn own_commit_vote(&self) -> Option<&Vote> {
        self.identity.as_ref().and_then(|(address, _)| self.commits.get(address))
    }
    pub fn own_view_change(&self) -> Option<&ViewChange> {
        self.own_change.as_ref()
    }
    pub fn timeout_certificate(&self) -> Option<TimeoutCertificate> {
        self.certificate.clone()
    }
    pub fn preferred_block(&self) -> Option<Block> {
        self.certificate.as_ref().and_then(|tc| tc.highest_prepared()).map(|p| p.block.clone())
    }
    pub fn prepared_certificate(&self) -> Option<PreparedBlock> {
        self.prepared.clone()
    }

    fn leader_for(&self, view: u64) -> Address {
        self.leaders[((view - 1) % self.leaders.len() as u64) as usize]
    }
    fn initial_view(&self) -> Result<u64, ConsensusError> {
        self.high_qc
            .as_ref()
            .map_or(self.height, |qc| qc.view)
            .checked_add(1)
            .ok_or_else(|| invalid("view counter overflow"))
    }

    pub fn propose(&mut self, block: Block) -> Result<(BlockProposal, Vote), ConsensusError> {
        if self.identity.is_none() {
            return Err(ConsensusError::NotAValidator);
        }
        if !self.is_leader() {
            return Err(invalid("only the designated leader may propose"));
        }
        if self.proposal.is_some() {
            return Err(invalid("a proposal is already awaiting quorum"));
        }
        let mut proposal = BlockProposal {
            protocol_context: self.config.protocol_context,
            view: self.view,
            block,
            proposer: self.leader(),
            signature: Vec::new(),
            timeout_certificate: self.certificate.clone(),
        };
        proposal.signature = self.sign(&proposal.signing_payload())?;
        let vote = self.observe_proposal(proposal.clone())?.ok_or(ConsensusError::NotAValidator)?;
        Ok((proposal, vote))
    }

    pub fn receive_proposal(&mut self, proposal: BlockProposal) -> Result<Vote, ConsensusError> {
        if self.identity.is_none() {
            return Err(ConsensusError::NotAValidator);
        }
        self.observe_proposal(proposal)?.ok_or(ConsensusError::NotAValidator)
    }

    pub fn observe_proposal(
        &mut self,
        proposal: BlockProposal,
    ) -> Result<Option<Vote>, ConsensusError> {
        if self.proposal.as_ref() == Some(&proposal) {
            return Ok(self.own_vote().cloned());
        }
        if proposal.view < self.view {
            return Err(invalid("stale proposal view"));
        }
        self.validate_proposal(&proposal)?;
        if proposal.view == self.view {
            if let Some(open) = &self.proposal {
                if open == &proposal {
                    return Ok(self.own_vote().cloned());
                }
                return Err(invalid("a conflicting proposal is already open"));
            }
        }
        let vote = self.make_vote(VotePhase::Prepare, proposal.view, proposal.block.hash())?;
        if proposal.view > self.view {
            self.enter_view(proposal.view);
        }
        self.certificate = proposal.timeout_certificate.clone();
        self.proposal = Some(proposal);
        // Start a full Prepare deadline on first authenticated proposal
        // acceptance. Exact retransmissions return above without extending it.
        self.started_at = None;
        self.prepares.clear();
        self.commits.clear();
        if let Some(vote) = &vote {
            self.prepares.insert(vote.validator, vote.clone());
        }
        self.form_prepare_certificate()?;
        Ok(vote)
    }

    fn validate_proposal(&self, proposal: &BlockProposal) -> Result<(), ConsensusError> {
        if proposal.protocol_context != self.config.protocol_context {
            return Err(invalid("proposal genesis context mismatch"));
        }
        self.ensure_extends_tip(&proposal.block)?;
        if proposal.view < self.initial_view()? || proposal.view == 0 || proposal.view == u64::MAX {
            return Err(invalid("proposal view precedes its committed parent"));
        }
        if proposal.proposer != self.leader_for(proposal.view) {
            return Err(invalid("proposal is not from the designated leader"));
        }
        self.verify_signature(proposal.proposer, &proposal.signing_payload(), &proposal.signature)?;
        let highest = match &proposal.timeout_certificate {
            Some(tc) => {
                self.validate_timeout_certificate(tc)?;
                if tc.view != proposal.view {
                    return Err(invalid("proposal timeout certificate has wrong view"));
                }
                tc.highest_prepared()
            }
            None => {
                if proposal.view != self.initial_view()? {
                    return Err(invalid("rotated proposal requires a timeout certificate"));
                }
                None
            }
        };
        if let Some(highest) = highest {
            if highest.block != proposal.block {
                return Err(invalid("leader did not select the highest prepared block"));
            }
        }
        if let Some(lock) = &self.prepared {
            if lock.block.hash() != proposal.block.hash()
                && highest.is_none_or(|p| p.qc.view <= lock.qc.view)
            {
                return Err(invalid("proposal cannot replace the prepared lock"));
            }
        }
        Ok(())
    }

    pub fn record_vote(&mut self, vote: Vote) -> Result<bool, ConsensusError> {
        if vote.view < self.initial_view()? {
            return Ok(false);
        }
        let proposal = self.proposal.as_ref().ok_or(ConsensusError::NoProposal)?;
        if vote.view != proposal.view || vote.block_hash != proposal.block.hash() {
            return Err(invalid("vote does not match the open proposal"));
        }
        self.validate_vote(&vote)?;
        let tally = match vote.phase {
            VotePhase::Prepare => &mut self.prepares,
            VotePhase::Commit => &mut self.commits,
        };
        if tally.contains_key(&vote.validator) {
            return Ok(false);
        }
        tally.insert(vote.validator, vote);
        self.form_prepare_certificate()?;
        Ok(true)
    }

    pub fn receive_vote(&mut self, vote: Vote) -> Result<VoteOutcome, ConsensusError> {
        let fresh = self.record_vote(vote)?;
        if self.pending_qc().is_some() {
            return self.commit_open_proposal();
        }
        if !fresh {
            return Ok(VoteOutcome::Ignored);
        }
        Ok(VoteOutcome::Pending { votes: self.commits.len(), needed: self.config.quorum_threshold })
    }

    fn form_prepare_certificate(&mut self) -> Result<(), ConsensusError> {
        if self.prepares.len() < self.config.quorum_threshold {
            return Ok(());
        }
        let proposal = self.proposal.as_ref().ok_or(ConsensusError::NoProposal)?;
        let prepared = PreparedBlock {
            block: proposal.block.clone(),
            qc: PrepareCertificate {
                protocol_context: self.config.protocol_context,
                view: proposal.view,
                block_hash: proposal.block.hash(),
                signatures: self
                    .prepares
                    .values()
                    .map(|v| (v.validator, v.signature.clone()))
                    .collect(),
            },
        };
        self.lock_prepared(prepared)?;
        Ok(())
    }

    fn lock_prepared(&mut self, prepared: PreparedBlock) -> Result<Option<Vote>, ConsensusError> {
        self.validate_prepared(&prepared)?;
        if let Some(lock) = &self.prepared {
            if prepared.qc.view < lock.qc.view
                || (prepared.qc.view == lock.qc.view && prepared.block.hash() != lock.block.hash())
            {
                return Err(invalid("prepared certificate regresses or conflicts with the lock"));
            }
        }
        let first_commit_phase =
            self.prepared.as_ref().is_none_or(|lock| lock.qc.view != prepared.qc.view);
        let vote = match self.own_commit_vote() {
            Some(existing) => Some(existing.clone()),
            None => self.make_vote(VotePhase::Commit, prepared.qc.view, prepared.block.hash())?,
        };
        self.prepared = Some(prepared);
        if first_commit_phase {
            // A Prepare proof arriving near the old deadline must give Commit
            // delivery its own full interval. Extra signatures do not rearm it.
            self.started_at = None;
        }
        if let Some(vote) = &vote {
            self.commits.insert(vote.validator, vote.clone());
        }
        Ok(vote)
    }

    /// A prepare QC independently proves that honest replicas validated the block.
    /// Callers still validate its execution before passing it here.
    pub fn receive_prepare(
        &mut self,
        prepared: PreparedBlock,
    ) -> Result<Option<Vote>, ConsensusError> {
        if prepared.qc.view < self.view {
            return Err(invalid("stale prepared certificate"));
        }
        if self.prepared.as_ref() == Some(&prepared) && prepared.qc.view == self.view {
            return Ok(self.own_commit_vote().cloned());
        }
        self.validate_prepared(&prepared)?;
        if let Some(lock) = &self.prepared {
            if prepared.qc.view < lock.qc.view
                || (prepared.qc.view == lock.qc.view && prepared.block.hash() != lock.block.hash())
            {
                return Err(invalid("prepared certificate regresses or conflicts with the lock"));
            }
        }
        if prepared.qc.view < self.view {
            return Err(invalid("stale prepared certificate"));
        }
        if prepared.qc.view == self.view {
            if self.proposal.as_ref().is_some_and(|p| p.block.hash() != prepared.block.hash()) {
                return Err(invalid(
                    "prepared certificate conflicts with our proposal in this view",
                ));
            }
        } else {
            self.enter_view(prepared.qc.view);
        }
        if self.proposal.is_none() {
            self.proposal = Some(BlockProposal {
                protocol_context: self.config.protocol_context,
                view: prepared.qc.view,
                block: prepared.block.clone(),
                proposer: self.leader(),
                signature: Vec::new(),
                timeout_certificate: None,
            });
        }
        self.lock_prepared(prepared)
    }

    pub fn pending_qc(&self) -> Option<QuorumCertificate> {
        let proposal = self.proposal.as_ref()?;
        (self.commits.len() >= self.config.quorum_threshold).then(|| QuorumCertificate {
            protocol_context: self.config.protocol_context,
            view: proposal.view,
            block_hash: proposal.block.hash(),
            signatures: self.commits.values().map(|v| (v.validator, v.signature.clone())).collect(),
        })
    }

    pub fn commit_open_proposal(&mut self) -> Result<VoteOutcome, ConsensusError> {
        let Some(qc) = self.pending_qc() else {
            return Ok(if self.proposal.is_none() {
                VoteOutcome::Ignored
            } else {
                VoteOutcome::Pending {
                    votes: self.commits.len(),
                    needed: self.config.quorum_threshold,
                }
            });
        };
        let block = self.proposal.as_ref().ok_or(ConsensusError::NoProposal)?.block.clone();
        self.receive_commit(block, qc.clone())?;
        Ok(VoteOutcome::Committed { view: qc.view, block_hash: self.tip })
    }

    pub fn validate_commit_certificate(
        &self,
        qc: &QuorumCertificate,
    ) -> Result<(), ConsensusError> {
        if qc.protocol_context != self.config.protocol_context {
            return Err(invalid("final certificate genesis context mismatch"));
        }
        self.verify_signatures(&qc.signatures, &qc.signing_payload())
    }

    pub fn receive_commit(
        &mut self,
        block: Block,
        qc: QuorumCertificate,
    ) -> Result<(), ConsensusError> {
        self.ensure_extends_tip(&block)?;
        if qc.block_hash != block.hash() || qc.view < self.initial_view()? {
            return Err(invalid(
                "final certificate does not match the next block or committed view",
            ));
        }
        self.validate_commit_certificate(&qc)?;
        let next_view = qc.view.checked_add(1).ok_or_else(|| invalid("view counter overflow"))?;
        self.height = block.header.height;
        self.tip = block.hash();
        self.high_qc = Some(qc);
        self.committed.push(block);
        self.enter_view(next_view);
        self.prepared = None;
        self.changes.clear();
        self.timeout_attempts = 0;
        Ok(())
    }

    /// Reset an idle timer when newly admitted work first arrives. No view,
    /// signing obligation, prepared lock, or certificate is changed.
    pub fn wake_on_work(&mut self, now: Duration) -> Result<(), ConsensusError> {
        if self.started_at.is_some_and(|started| now < started) {
            return Err(invalid("consensus clock moved backwards"));
        }
        self.started_at = Some(now);
        self.timeout_attempts = 0;
        Ok(())
    }

    /// The delay is bounded for operational recovery. Liveness assumes eventual
    /// delivery within this bound; operators can raise the configured base.
    fn timeout_duration(&self) -> Duration {
        let base = Duration::from_millis(self.config.view_timeout_ms);
        base.saturating_mul(1u32 << self.timeout_attempts.min(20))
            .min(base.max(Duration::from_secs(30)))
    }

    /// A lone replica may leave the initial view, but cannot run arbitrarily far
    /// ahead while its peers are recovering. At least f+1 distinct reports at or
    /// above this timeout-only view establish that an honest peer has joined it.
    fn can_timeout(&self) -> Result<bool, ConsensusError> {
        Ok(self.view == self.initial_view()?
            || self.certificate.is_some()
            || self.prepared.as_ref().is_some_and(|p| p.qc.view == self.view)
            || self.changes.values().filter(|report| report.view >= self.view).count()
                > self.config.max_faulty)
    }

    /// Monotonic elapsed time supplied by the caller. Local timeout alone never
    /// authorizes a replacement proposal; n-f exact-target reports are required.
    pub fn tick(&mut self, now: Duration) -> Result<Option<ViewChange>, ConsensusError> {
        if self.started_at.is_some_and(|started| now < started) {
            return Err(invalid("consensus clock moved backwards"));
        }
        if self.view == u64::MAX {
            return Err(invalid("view counter overflow"));
        }
        if self.pending_qc().is_some() {
            return Ok(None);
        }
        if !self.can_timeout()? {
            // Start a fresh full deadline once f+1 peers catch up, instead of
            // expiring immediately on their first late report and running away.
            self.started_at = None;
            return Ok(None);
        }
        let Some(started) = self.started_at else {
            self.started_at = Some(now);
            return Ok(None);
        };
        if now - started < self.timeout_duration() {
            return Ok(None);
        }
        let target = self
            .view
            .checked_add(1)
            .filter(|view| *view < u64::MAX)
            .ok_or_else(|| invalid("view counter overflow"))?;
        let change = self.make_view_change(target)?;
        self.enter_view(target);
        self.started_at = Some(now);
        self.timeout_attempts = self.timeout_attempts.saturating_add(1);
        self.retain_own_change(change.clone());
        self.form_timeout_certificate(target)?;
        Ok(change)
    }

    fn make_view_change(&self, target: u64) -> Result<Option<ViewChange>, ConsensusError> {
        let Some((address, _)) = &self.identity else { return Ok(None) };
        let mut change = ViewChange {
            protocol_context: self.config.protocol_context,
            view: target,
            height: self.height.checked_add(1).ok_or_else(|| invalid("height overflow"))?,
            tip: self.tip,
            validator: *address,
            prepared: self.prepared.clone(),
            signature: Vec::new(),
        };
        change.signature = self.sign(&change.signing_payload())?;
        Ok(Some(change))
    }

    fn retain_own_change(&mut self, change: Option<ViewChange>) {
        self.own_change = change.clone();
        if let Some(change) = change {
            self.changes.insert(change.validator, change);
        }
    }

    fn check_report_compatibility(&self, change: &ViewChange) -> Result<(), ConsensusError> {
        if let Some(incoming) = &change.prepared {
            if self.prepared.as_ref().is_some_and(|lock| {
                lock.qc.view == incoming.qc.view && lock.block.hash() != incoming.block.hash()
            }) {
                return Err(invalid("view-change report conflicts with the local prepared QC"));
            }
            if self.changes.values().any(|report| {
                report.prepared.as_ref().is_some_and(|existing| {
                    existing.qc.view == incoming.qc.view
                        && existing.block.hash() != incoming.block.hash()
                })
            }) {
                return Err(invalid("view-change report conflicts with a retained prepared QC"));
            }
        }
        Ok(())
    }

    /// Retain only the highest authenticated report per signer. The f+1-th
    /// highest target is supported by at least one honest signer: a Byzantine
    /// singleton can neither force a high-view jump nor authorize a proposal.
    pub fn record_view_change(&mut self, change: ViewChange) -> Result<bool, ConsensusError> {
        if self.changes.get(&change.validator).is_some_and(|saved| saved.view > change.view) {
            return Ok(false);
        }
        if self.changes.get(&change.validator) == Some(&change) {
            return Ok(false);
        }
        self.validate_view_change(&change)?;
        if self.pending_qc().is_some() {
            return Ok(false);
        }
        if let Some(existing) = self.changes.get(&change.validator) {
            if existing.view > change.view || existing == &change {
                return Ok(false);
            }
            if existing.view == change.view {
                return Err(invalid("conflicting reports from one signer in the same view"));
            }
        }
        // Our own future signature would imply lost signing state or another
        // process using this identity. Do not manufacture a lower own report.
        if self.identity.as_ref().is_some_and(|(address, _)| *address == change.validator)
            && change.view > self.view
        {
            return Err(invalid("own future report requires newer signing recovery state"));
        }
        self.check_report_compatibility(&change)?;
        self.changes.insert(change.validator, change);
        let mut targets: Vec<_> = self.changes.values().map(|report| report.view).collect();
        targets.sort_unstable_by(|a, b| b.cmp(a));
        if let Some(join) = targets.get(self.config.max_faulty).copied() {
            if join > self.view {
                let own_change = self.make_view_change(join)?;
                self.enter_view(join);
                self.retain_own_change(own_change);
            }
        }
        // Exact reports at the active target alone constitute a TC. Reports
        // from other views provide synchronization evidence, never voting proof.
        self.form_timeout_certificate(self.view)?;
        Ok(true)
    }

    /// A lagging designated leader can adopt a full TC without waiting for a
    /// proposal that only it can sign. A ready final QC is never abandoned.
    pub fn receive_timeout_certificate(
        &mut self,
        tc: TimeoutCertificate,
    ) -> Result<bool, ConsensusError> {
        if tc.view < self.view {
            return Ok(false);
        }
        if self.certificate.as_ref() == Some(&tc) {
            return Ok(false);
        }
        self.validate_timeout_certificate(&tc)?;
        if tc.view < self.view || self.pending_qc().is_some() {
            return Ok(false);
        }
        if self.certificate.as_ref() == Some(&tc) {
            return Ok(false);
        }
        for report in &tc.messages {
            self.check_report_compatibility(report)?;
            if self.identity.as_ref().is_some_and(|(address, _)| *address == report.validator)
                && self
                    .changes
                    .get(&report.validator)
                    .is_some_and(|own| own.view == report.view && own != report)
            {
                return Err(invalid("timeout certificate conflicts with our saved report"));
            }
        }
        let highest = tc.highest_prepared().cloned();
        if let (Some(lock), Some(incoming)) = (&self.prepared, &highest) {
            if lock.qc.view == incoming.qc.view && lock.block.hash() != incoming.block.hash() {
                return Err(invalid("timeout certificate conflicts with the prepared lock"));
            }
        }
        // Learning a higher valid prepare proof preserves its obligation in our
        // next report. Keep an already-signed same-view proposal's older lock
        // unchanged when its block differs; that vote cannot be reissued.
        let can_learn = tc.view > self.view
            || self.proposal.as_ref().is_none_or(|proposal| {
                highest.as_ref().is_none_or(|p| p.block.hash() == proposal.block.hash())
            });
        let mut next_lock = self.prepared.clone();
        if can_learn {
            if let Some(highest) = highest {
                if next_lock.as_ref().is_none_or(|lock| highest.qc.view > lock.qc.view) {
                    next_lock = Some(highest);
                }
            }
        }
        let target = tc.view;
        // Sign before mutating the live state. The node persists this candidate
        // before publishing the resulting own view-change report.
        let own = if target > self.view {
            let mut candidate = self.clone();
            candidate.prepared = next_lock.clone();
            candidate.make_view_change(target)?
        } else {
            None
        };
        self.prepared = next_lock;
        if target > self.view {
            self.enter_view(target);
        }
        for report in &tc.messages {
            if self
                .changes
                .get(&report.validator)
                .is_none_or(|existing| existing.view <= report.view)
            {
                self.changes.insert(report.validator, report.clone());
            }
        }
        if own.is_some() {
            self.retain_own_change(own);
        }
        if self.certificate.is_none() {
            self.started_at = None;
        }
        self.certificate = Some(tc);
        // Include our newly reported higher lock if the original TC omitted it.
        self.form_timeout_certificate(target)?;
        Ok(true)
    }

    fn form_timeout_certificate(&mut self, target: u64) -> Result<(), ConsensusError> {
        let messages: Vec<_> =
            self.changes.values().filter(|message| message.view == target).cloned().collect();
        if messages.len() < self.config.quorum_threshold {
            return Ok(());
        }
        let tc = TimeoutCertificate {
            view: target,
            height: self.height.checked_add(1).ok_or_else(|| invalid("height overflow"))?,
            tip: self.tip,
            messages,
        };
        self.validate_timeout_certificate(&tc)?;
        if self.certificate.is_none() {
            self.started_at = None;
        }
        self.certificate = Some(tc);
        Ok(())
    }

    fn enter_view(&mut self, target: u64) {
        self.view = target;
        self.proposal = None;
        self.prepares.clear();
        self.commits.clear();
        self.certificate = None;
        self.own_change = None;
        self.started_at = None;
    }

    pub fn validate_timeout_certificate(
        &self,
        tc: &TimeoutCertificate,
    ) -> Result<(), ConsensusError> {
        if tc.view == u64::MAX
            || tc.view <= self.initial_view()?
            || tc.height != self.height.saturating_add(1)
            || tc.tip != self.tip
        {
            return Err(invalid("timeout certificate has wrong view or chain anchor"));
        }
        if tc.messages.len() < self.config.quorum_threshold
            || tc.messages.len() > self.config.validator_count
        {
            return Err(invalid("timeout certificate has invalid support count"));
        }
        let mut signers = BTreeSet::new();
        let mut prepared_by_view = BTreeMap::new();
        for message in &tc.messages {
            if message.view != tc.view || message.height != tc.height || message.tip != tc.tip {
                return Err(invalid("timeout certificate mixes views or chain anchors"));
            }
            if !signers.insert(message.validator) {
                return Err(ConsensusError::DuplicateSigner(message.validator));
            }
            self.validate_view_change(message)?;
            if let Some(prepared) = &message.prepared {
                if let Some(existing) =
                    prepared_by_view.insert(prepared.qc.view, prepared.block.hash())
                {
                    if existing != prepared.block.hash() {
                        return Err(invalid(
                            "timeout certificate contains conflicting prepared QCs",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_view_change(&self, change: &ViewChange) -> Result<(), ConsensusError> {
        if change.protocol_context != self.config.protocol_context
            || change.view == u64::MAX
            || change.view <= self.initial_view()?
            || change.height != self.height.saturating_add(1)
            || change.tip != self.tip
        {
            return Err(invalid("view-change report has wrong view or chain anchor"));
        }
        self.verify_signature(change.validator, &change.signing_payload(), &change.signature)?;
        if let Some(prepared) = &change.prepared {
            if prepared.qc.view >= change.view {
                return Err(invalid("view-change lock is not from an earlier view"));
            }
            self.validate_prepared(prepared)?;
        }
        Ok(())
    }

    pub fn validate_prepared(&self, prepared: &PreparedBlock) -> Result<(), ConsensusError> {
        if prepared.qc.protocol_context != self.config.protocol_context {
            return Err(invalid("prepare certificate genesis context mismatch"));
        }
        self.ensure_extends_tip(&prepared.block)?;
        if prepared.qc.block_hash != prepared.block.hash()
            || prepared.qc.view < self.initial_view()?
            || prepared.qc.view == u64::MAX
        {
            return Err(invalid("prepare certificate has wrong block or view"));
        }
        self.verify_signatures(
            &prepared.qc.signatures,
            &PrepareCertificate::payload_for_context(
                self.config.protocol_context,
                prepared.qc.view,
                &prepared.qc.block_hash,
            ),
        )
    }

    fn validate_vote(&self, vote: &Vote) -> Result<(), ConsensusError> {
        self.verify_signature(
            vote.validator,
            &Vote::payload_for_context(
                self.config.protocol_context,
                vote.phase,
                vote.view,
                &vote.block_hash,
            ),
            &vote.signature,
        )
    }
    fn verify_signatures(
        &self,
        signatures: &[(Address, Vec<u8>)],
        payload: &[u8],
    ) -> Result<(), ConsensusError> {
        if signatures.len() < self.config.quorum_threshold {
            return Err(ConsensusError::InsufficientSupport {
                have: signatures.len(),
                need: self.config.quorum_threshold,
            });
        }
        if signatures.len() > self.config.validator_count {
            return Err(invalid("certificate has too many signatures"));
        }
        let mut signers = BTreeSet::new();
        for (address, signature) in signatures {
            if !signers.insert(*address) {
                return Err(ConsensusError::DuplicateSigner(*address));
            }
            self.verify_signature(*address, payload, signature)?;
        }
        Ok(())
    }
    fn verify_signature(
        &self,
        address: Address,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<(), ConsensusError> {
        let key =
            self.registry.public_key(&address).ok_or(ConsensusError::UnknownValidator(address))?;
        let fingerprint = Hash::from_data(&encode_canonical(&(address, payload, signature)));
        if self.verified_signatures.borrow().contains(&fingerprint) {
            return Ok(());
        }
        let signature = BLSScheme::decode_signature(signature)
            .ok_or(ConsensusError::MalformedSignature(address))?;
        if !BLSScheme::verify(key, payload, &signature) {
            return Err(ConsensusError::InvalidSignature(address));
        }
        let mut verified = self.verified_signatures.borrow_mut();
        let capacity = self.config.validator_count.saturating_mul(16).clamp(16, 2048);
        if verified.len() >= capacity {
            verified.clear();
        }
        verified.insert(fingerprint);
        Ok(())
    }
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, ConsensusError> {
        let (_, key) = self.identity.as_ref().ok_or(ConsensusError::NotAValidator)?;
        Ok(BLSScheme::encode_signature(&BLSScheme::sign(key, payload)))
    }
    fn make_vote(
        &self,
        phase: VotePhase,
        view: u64,
        block_hash: Hash,
    ) -> Result<Option<Vote>, ConsensusError> {
        let Some((address, _)) = &self.identity else { return Ok(None) };
        Ok(Some(Vote {
            view,
            block_hash,
            validator: *address,
            signature: self.sign(&Vote::payload_for_context(
                self.config.protocol_context,
                phase,
                view,
                &block_hash,
            ))?,
            phase,
        }))
    }
    fn ensure_extends_tip(&self, block: &Block) -> Result<(), ConsensusError> {
        let height = self.height.checked_add(1).ok_or_else(|| invalid("height overflow"))?;
        if block.header.height != height || block.header.parent_hash != self.tip {
            return Err(invalid("block must extend the committed tip by exactly one height"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod signature_cache_tests;
