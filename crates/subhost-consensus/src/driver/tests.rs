use super::*;
use subhost_core::{BlockHeader, BLOCK_GAS_LIMIT};

fn block(height: u64, parent: Hash, seed: u8) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            chain_id: 1,
            height,
            timestamp: 1_700_000_000 + height,
            parent_hash: parent,
            state_root: Hash::from_data(&[seed]),
            tx_root: Hash::ZERO,
            receipt_root: Hash::ZERO,
            validator: Address::ZERO,
            gas_used: 0,
            gas_limit: BLOCK_GAS_LIMIT,
            extra_data: Vec::new(),
            base_fee_per_gas: 0,
        },
        transactions: Vec::new(),
        signatures: Vec::new(),
    }
}

fn setup(count: u8) -> (ValidatorRegistry, Vec<(Address, PrivateKey)>, Vec<ConsensusDriver>) {
    let mut registry = ValidatorRegistry::new();
    let mut keys = Vec::new();
    for i in 1..=count {
        let (key, public) = BLSScheme::keygen();
        let address = Address::new([i; 20]);
        registry.register(address, public, &BLSScheme::proof_of_possession(&key)).unwrap();
        keys.push((address, key));
    }
    let drivers = keys
        .iter()
        .map(|key| {
            ConsensusDriver::new(
                ConsensusConfig::new(count as usize).unwrap(),
                registry.clone(),
                Some(key.clone()),
            )
            .unwrap()
        })
        .collect();
    (registry, keys, drivers)
}

fn sign_vote(
    keys: &[(Address, PrivateKey)],
    index: usize,
    phase: VotePhase,
    view: u64,
    hash: Hash,
) -> Vote {
    Vote {
        phase,
        view,
        block_hash: hash,
        validator: keys[index].0,
        signature: BLSScheme::encode_signature(&BLSScheme::sign(
            &keys[index].1,
            &Vote::payload_for(phase, view, &hash),
        )),
    }
}

fn sign_proposal(
    keys: &[(Address, PrivateKey)],
    index: usize,
    view: u64,
    block: Block,
    tc: Option<TimeoutCertificate>,
) -> BlockProposal {
    let mut proposal = BlockProposal {
        protocol_context: Hash::ZERO,
        view,
        block,
        proposer: keys[index].0,
        signature: Vec::new(),
        timeout_certificate: tc,
    };
    proposal.signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&keys[index].1, &proposal.signing_payload()));
    proposal
}

fn timeout(drivers: &mut [ConsensusDriver], indices: &[usize]) -> Vec<ViewChange> {
    indices
        .iter()
        .map(|i| {
            assert!(drivers[*i].tick(Duration::ZERO).unwrap().is_none());
            drivers[*i].tick(Duration::from_millis(2000)).unwrap().unwrap()
        })
        .collect()
}

fn certify(
    drivers: &mut [ConsensusDriver],
    indices: &[usize],
    proposal: BlockProposal,
) -> QuorumCertificate {
    let prepares: Vec<_> = indices
        .iter()
        .map(|i| drivers[*i].observe_proposal(proposal.clone()).unwrap().unwrap())
        .collect();
    for i in indices {
        for vote in &prepares {
            drivers[*i].record_vote(vote.clone()).unwrap();
        }
    }
    let commits: Vec<_> =
        indices.iter().map(|i| drivers[*i].own_commit_vote().unwrap().clone()).collect();
    for i in indices {
        for vote in &commits {
            drivers[*i].record_vote(vote.clone()).unwrap();
        }
    }
    let qc = drivers[indices[0]].pending_qc().unwrap();
    for i in indices {
        assert!(matches!(
            drivers[*i].commit_open_proposal().unwrap(),
            VoteOutcome::Committed { .. }
        ));
    }
    qc
}

#[test]
fn three_of_four_replicas_finalize_only_after_both_phases() {
    let (registry, _, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let qc = certify(&mut drivers, &[0, 1, 2], proposal);
    assert!(drivers[..3]
        .iter()
        .all(|d| d.tip() == first.hash() && d.height() == 1 && d.view() == 2));
    let mut observer =
        ConsensusDriver::new(ConsensusConfig::new(4).unwrap(), registry, None).unwrap();
    observer.receive_commit(first.clone(), qc).unwrap();
    assert_eq!(observer.tip(), first.hash());
    assert_eq!(drivers[0].committed(), &[first]);
}

#[test]
fn prepare_quorum_is_never_a_final_certificate() {
    let (_, _, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let votes: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for vote in votes {
        drivers[0].record_vote(vote).unwrap();
    }
    assert!(drivers[0].prepared_certificate().is_some());
    assert!(drivers[0].own_commit_vote().is_some());
    assert!(drivers[0].pending_qc().is_none());
    assert_eq!(drivers[0].height(), 0);
    let prepare = drivers[0].prepared_certificate().unwrap();
    let fake_final = QuorumCertificate {
        protocol_context: Hash::ZERO,
        view: prepare.qc.view,
        block_hash: prepare.qc.block_hash,
        signatures: prepare.qc.signatures,
    };
    assert!(drivers[1].receive_commit(proposal.block, fake_final).is_err());
}

#[test]
fn phase_view_and_block_replay_do_not_consume_a_signer_slot() {
    let (_, keys, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let vote = sign_vote(&keys, 1, VotePhase::Prepare, 1, proposal.block.hash());
    let mut wrong_phase = vote.clone();
    wrong_phase.phase = VotePhase::Commit;
    let mut wrong_view = vote.clone();
    wrong_view.view += 1;
    let mut wrong_block = vote.clone();
    wrong_block.block_hash = Hash::ZERO;
    for bad in [wrong_phase, wrong_view, wrong_block] {
        assert!(drivers[0].record_vote(bad).is_err());
    }
    assert!(drivers[0].record_vote(vote.clone()).unwrap());
    assert!(!drivers[0].record_vote(vote).unwrap());
    assert!(drivers[0].pending_qc().is_none());
}

#[test]
fn malformed_unknown_and_forged_votes_are_rejected() {
    let (_, keys, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let vote = sign_vote(&keys, 1, VotePhase::Prepare, 1, proposal.block.hash());
    let mut empty = vote.clone();
    empty.signature.clear();
    let mut corrupt = vote.clone();
    corrupt.signature = vec![0xff; 96];
    let mut unknown = vote.clone();
    unknown.validator = Address::new([99; 20]);
    let mut forged = vote;
    forged.signature = BLSScheme::encode_signature(&BLSScheme::sign(&keys[2].1, b"wrong"));
    for bad in [empty, corrupt, unknown, forged] {
        assert!(drivers[0].record_vote(bad).is_err());
    }
}

#[test]
fn authenticated_leader_envelope_cannot_be_replaced_by_a_producer_signature() {
    let (_, keys, mut drivers) = setup(4);
    assert_eq!(drivers[0].leader(), keys[0].0);
    assert!(drivers[1].propose(block(1, Hash::ZERO, 1)).is_err());
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let mut wrong_proposer = proposal.clone();
    wrong_proposer.proposer = keys[1].0;
    let mut wrong_block = proposal.clone();
    wrong_block.block.header.extra_data.push(1);
    let mut wrong_signature = proposal.clone();
    wrong_signature.signature = vec![0xff; 96];
    for bad in [wrong_proposer, wrong_block, wrong_signature] {
        assert!(drivers[1].observe_proposal(bad).is_err());
    }
    drivers[1].observe_proposal(proposal).unwrap();
}

#[test]
fn two_authenticated_conflicting_proposals_in_one_view_cannot_both_receive_our_vote() {
    let (_, keys, mut drivers) = setup(4);
    let first = sign_proposal(&keys, 0, 1, block(1, Hash::ZERO, 1), None);
    let second = sign_proposal(&keys, 0, 1, block(1, Hash::ZERO, 2), None);
    let vote = drivers[1].observe_proposal(first.clone()).unwrap().unwrap();
    assert!(drivers[1].observe_proposal(second).is_err());
    assert_eq!(drivers[1].observe_proposal(first).unwrap(), Some(vote));
}

#[test]
fn a_silent_initial_leader_is_replaced_by_three_timeout_reports() {
    let (_, keys, mut drivers) = setup(4);
    let changes = timeout(&mut drivers, &[1, 2, 3]);
    for driver in drivers.iter_mut().skip(1) {
        for change in &changes {
            driver.record_view_change(change.clone()).unwrap();
        }
    }
    assert_eq!(drivers[1].leader(), keys[1].0);
    let (proposal, _) = drivers[1].propose(block(1, Hash::ZERO, 2)).unwrap();
    assert_eq!(proposal.view, 2);
    assert!(proposal.timeout_certificate.is_some());
    let qc = certify(&mut drivers, &[1, 2, 3], proposal.clone());
    assert_eq!(qc.view, 2);
    assert_eq!(drivers[1].height(), 1);
    drivers[0].receive_commit(proposal.block, qc).unwrap();
    assert_eq!(drivers[0].tip(), drivers[1].tip());
}

#[test]
fn two_live_replicas_advance_local_views_without_authorizing_a_proposal() {
    let (_, _, mut drivers) = setup(4);
    let changes = timeout(&mut drivers, &[0, 1]);
    for driver in drivers.iter_mut().take(2) {
        for change in &changes {
            driver.record_view_change(change.clone()).unwrap();
        }
    }
    assert_eq!(drivers[1].view(), 2);
    assert!(drivers[1].is_leader());
    assert!(drivers[1].propose(block(1, Hash::ZERO, 2)).is_err());
    assert!(drivers[1].tick(Duration::from_millis(6000)).unwrap().is_some());
    assert_eq!(drivers[1].view(), 3);
    assert!(drivers[..2]
        .iter()
        .all(|d| d.height() == 0 && d.tip() == Hash::ZERO && d.pending_qc().is_none()));
}

#[test]
fn a_hidden_final_qc_survives_view_change_and_the_new_leader_reproposes_its_block() {
    let (_, keys, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let prepares: Vec<_> = [0, 1, 3]
        .into_iter()
        .map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap())
        .collect();
    for i in [0, 1] {
        for prepare in &prepares {
            drivers[i].record_vote(prepare.clone()).unwrap();
        }
    }
    // D is Byzantine: it supplies both old phases but later omits its lock.
    let final_votes = [
        drivers[0].own_commit_vote().unwrap().clone(),
        drivers[1].own_commit_vote().unwrap().clone(),
        sign_vote(&keys, 3, VotePhase::Commit, 1, first.hash()),
    ];
    for vote in final_votes {
        drivers[0].record_vote(vote).unwrap();
    }
    let hidden = drivers[0].pending_qc().unwrap();
    drivers[0].commit_open_proposal().unwrap();
    assert_eq!(drivers[1].height(), 0);
    let mut changes = timeout(&mut drivers, &[1, 2, 3]);
    changes[2].prepared = None;
    changes[2].signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&keys[3].1, &changes[2].signing_payload()));
    for i in [1, 2, 3] {
        for change in &changes {
            drivers[i].record_view_change(change.clone()).unwrap();
        }
    }
    assert_eq!(drivers[1].preferred_block(), Some(first.clone()));
    assert!(drivers[1].propose(block(1, Hash::ZERO, 2)).is_err());
    let (reproposal, _) = drivers[1].propose(first.clone()).unwrap();
    let later = certify(&mut drivers, &[1, 2, 3], reproposal);
    assert_eq!(hidden.block_hash, later.block_hash);
    assert!(drivers.iter().all(|d| d.tip() == first.hash()));
}

#[test]
fn a_prepare_only_lock_can_yield_to_a_later_authenticated_final_commit() {
    let (_, _, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let votes: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for vote in votes {
        drivers[0].record_vote(vote).unwrap();
    }
    assert!(drivers[0].prepared_certificate().is_some());
    // No other honest replica sees this prepare QC; it cannot secretly finalize.
    let changes = timeout(&mut drivers, &[1, 2, 3]);
    for i in [1, 2, 3] {
        for change in &changes {
            drivers[i].record_view_change(change.clone()).unwrap();
        }
    }
    let winning = block(1, Hash::ZERO, 2);
    let (proposal, _) = drivers[1].propose(winning.clone()).unwrap();
    let qc = certify(&mut drivers, &[1, 2, 3], proposal);
    drivers[0].receive_commit(winning.clone(), qc).unwrap();
    assert_eq!(drivers[0].tip(), winning.hash());
}

#[test]
fn prepared_proof_supplies_a_missing_prepare_tally_and_survives_restart() {
    let (registry, keys, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for vote in prepares {
        drivers[0].record_vote(vote).unwrap();
    }
    let prepared = drivers[0].prepared_certificate().unwrap();
    assert!(drivers[3].own_vote().is_none());
    let commit = drivers[3].receive_prepare(prepared).unwrap().unwrap();
    assert_eq!(commit.phase, VotePhase::Commit);
    let saved = drivers[3].checkpoint();
    let restored = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry,
        Some(keys[3].clone()),
        saved.clone(),
    )
    .unwrap();
    assert_eq!(restored.checkpoint(), saved);
    assert_eq!(restored.own_commit_vote(), Some(&commit));
}

#[test]
fn timeout_checkpoints_retain_the_lock_and_already_signed_report() {
    let (registry, keys, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for vote in prepares {
        drivers[0].record_vote(vote).unwrap();
    }
    let report = timeout(&mut drivers, &[0]).pop().unwrap();
    let saved = drivers[0].checkpoint();
    assert!(saved.proposal.is_none());
    assert!(saved.prepared.is_some());
    let restored = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry,
        Some(keys[0].clone()),
        saved.clone(),
    )
    .unwrap();
    assert_eq!(restored.checkpoint(), saved);
    assert_eq!(restored.own_view_change(), Some(&report));
}

#[test]
fn forged_duplicate_replayed_and_mixed_timeout_certificates_are_rejected() {
    let (_, _, mut drivers) = setup(4);
    let changes = timeout(&mut drivers, &[0, 1, 2]);
    let valid = TimeoutCertificate { view: 2, height: 1, tip: Hash::ZERO, messages: changes };
    drivers[3].validate_timeout_certificate(&valid).unwrap();
    let mut short = valid.clone();
    short.messages.pop();
    let mut duplicate = valid.clone();
    duplicate.messages[2] = duplicate.messages[1].clone();
    let mut forged = valid.clone();
    forged.messages[1].signature = vec![0xff; 96];
    let mut replay = valid.clone();
    replay.view = 3;
    for message in &mut replay.messages {
        message.view = 3;
    }
    let mut wrong_tip = valid.clone();
    wrong_tip.tip = Hash::from_data(b"other chain");
    let mut mixed = valid;
    mixed.messages[0].height = 2;
    for bad in [short, duplicate, forged, replay, wrong_tip, mixed] {
        assert!(drivers[3].validate_timeout_certificate(&bad).is_err());
    }
}

#[test]
fn checkpoint_restore_refuses_missing_forged_duplicate_and_detached_own_votes() {
    let (registry, keys, mut drivers) = setup(4);
    drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let saved = drivers[0].checkpoint();
    let restored = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry.clone(),
        Some(keys[0].clone()),
        saved.clone(),
    )
    .unwrap();
    assert_eq!(restored.checkpoint(), saved);
    let mut missing = saved.clone();
    missing.votes.clear();
    let mut forged = saved.clone();
    forged.votes[0].signature = vec![0xff; 96];
    let mut duplicate = saved.clone();
    duplicate.votes.push(duplicate.votes[0].clone());
    let mut detached = saved.clone();
    detached.proposal = None;
    let mut stale = saved;
    stale.view = 0;
    for bad in [missing, forged, duplicate, detached, stale] {
        assert!(ConsensusDriver::from_checkpoint(
            ConsensusConfig::new(4).unwrap(),
            registry.clone(),
            Some(keys[0].clone()),
            bad
        )
        .is_err());
    }
}

#[test]
fn checkpoint_restore_refuses_an_own_commit_without_its_prepare_proof() {
    let (registry, keys, mut drivers) = setup(1);
    drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let mut saved = drivers[0].checkpoint();
    saved.prepared = None;
    assert!(ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(1).unwrap(),
        registry,
        Some(keys[0].clone()),
        saved
    )
    .is_err());
}

#[test]
fn one_validator_still_requires_the_commit_phase_and_extends_its_restored_tip() {
    let (registry, keys, mut drivers) = setup(1);
    let (proposal, prepare) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    assert_eq!(prepare.phase, VotePhase::Prepare);
    assert_eq!(drivers[0].own_commit_vote().unwrap().phase, VotePhase::Commit);
    assert!(matches!(drivers[0].receive_vote(prepare).unwrap(), VoteOutcome::Committed { .. }));
    let saved = drivers[0].checkpoint();
    let mut restored = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(1).unwrap(),
        registry,
        Some(keys[0].clone()),
        saved,
    )
    .unwrap();
    assert!(restored.propose(block(2, Hash::ZERO, 2)).is_err());
    restored.propose(block(2, proposal.block.hash(), 2)).unwrap();
    restored.commit_open_proposal().unwrap();
    assert_eq!(restored.height(), 2);
}

#[test]
fn unsafe_configuration_bad_identity_and_invalid_ledger_anchors_are_rejected() {
    let (registry, keys, _) = setup(4);
    let mut weak = ConsensusConfig::new(4).unwrap();
    weak.quorum_threshold = 1;
    let mut no_timeout = ConsensusConfig::new(4).unwrap();
    no_timeout.view_timeout_ms = 0;
    for config in [weak, no_timeout] {
        assert!(ConsensusDriver::new(config, registry.clone(), Some(keys[0].clone())).is_err());
    }
    assert!(ConsensusDriver::new(
        ConsensusConfig::new(4).unwrap(),
        registry.clone(),
        Some((keys[0].0, keys[1].1.clone()))
    )
    .is_err());
    assert!(ConsensusDriver::with_tip(
        ConsensusConfig::new(4).unwrap(),
        registry.clone(),
        Some(keys[0].clone()),
        0,
        Hash::from_data(b"tip")
    )
    .is_err());
    assert!(ConsensusDriver::with_tip(
        ConsensusConfig::new(4).unwrap(),
        registry,
        Some(keys[0].clone()),
        1,
        Hash::ZERO
    )
    .is_err());
}

#[test]
fn overflow_and_a_backwards_clock_do_not_change_the_checkpoint() {
    let (_, _, mut drivers) = setup(4);
    drivers[0].tick(Duration::from_secs(1)).unwrap();
    let before = drivers[0].checkpoint();
    assert!(drivers[0].tick(Duration::ZERO).is_err());
    assert_eq!(drivers[0].checkpoint(), before);
    drivers[0].view = u64::MAX;
    let before = drivers[0].checkpoint();
    assert!(drivers[0].tick(Duration::from_secs(5)).is_err());
    assert_eq!(drivers[0].checkpoint(), before);
}

#[test]
fn pending_final_certificate_is_retained_for_storage_retry_instead_of_timing_out() {
    let (_, _, mut drivers) = setup(1);
    drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    drivers[0].tick(Duration::ZERO).unwrap();
    let saved = drivers[0].checkpoint();
    assert!(drivers[0].tick(Duration::from_secs(100)).unwrap().is_none());
    assert_eq!(drivers[0].checkpoint(), saved);
    assert!(drivers[0].pending_qc().is_some());
}

#[test]
fn observer_never_signs_or_proposes_but_can_advance_and_verify() {
    let (registry, _, mut drivers) = setup(4);
    let mut observer =
        ConsensusDriver::new(ConsensusConfig::new(4).unwrap(), registry, None).unwrap();
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    assert!(observer.propose(proposal.block.clone()).is_err());
    assert_eq!(observer.observe_proposal(proposal).unwrap(), None);
    observer.tick(Duration::ZERO).unwrap();
    assert_eq!(observer.tick(Duration::from_secs(2)).unwrap(), None);
    assert_eq!(observer.view(), 2);
    assert!(observer.own_view_change().is_none());
}

#[test]
fn a_rotated_leader_cannot_hide_the_receivers_existing_lock() {
    let (_, keys, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for vote in prepares {
        drivers[0].record_vote(vote).unwrap();
    }
    // An otherwise valid TC excludes A's isolated prepared lock. It authorizes
    // a new candidate for unlocked nodes, but cannot make A forget its lock.
    let reports = timeout(&mut drivers, &[1, 2, 3]);
    let tc = TimeoutCertificate { view: 2, height: 1, tip: Hash::ZERO, messages: reports };
    let conflicting = sign_proposal(&keys, 1, 2, block(1, Hash::ZERO, 2), Some(tc));
    let saved = drivers[0].checkpoint();
    assert!(drivers[0].observe_proposal(conflicting).is_err());
    assert_eq!(drivers[0].checkpoint(), saved);
}

#[test]
fn duplicate_validator_registration_does_not_replace_the_trusted_key() {
    let (mut registry, keys, _) = setup(4);
    let trusted = *registry.public_key(&keys[0].0).unwrap();
    let (replacement, public) = BLSScheme::keygen();
    assert!(registry
        .register(keys[0].0, public, &BLSScheme::proof_of_possession(&replacement))
        .is_err());
    assert_eq!(registry.public_key(&keys[0].0), Some(&trusted));
}

#[test]
fn one_public_key_cannot_count_as_multiple_registered_signers() {
    let (mut registry, keys, _) = setup(4);
    let public = BLSScheme::public_key(&keys[0].1);
    let pop = BLSScheme::proof_of_possession(&keys[0].1);
    assert!(registry.register(Address::new([99; 20]), public, &pop).is_err());
    assert_eq!(registry.len(), 4);
}

#[test]
fn nil_lock_view_change_reports_are_bound_to_the_genesis_context() {
    let (registry, keys, mut drivers) = setup(4);
    let reports = timeout(&mut drivers, &[0, 1, 2]);
    assert!(reports.iter().all(|report| report.prepared.is_none() && report.tip == Hash::ZERO));
    let mut other_config = ConsensusConfig::new(4).unwrap();
    other_config.protocol_context =
        Hash::from_data(b"another genesis with the same validator keys");
    let mut other =
        ConsensusDriver::new(other_config.clone(), registry.clone(), Some(keys[3].clone()))
            .unwrap();
    let saved = other.checkpoint();
    for report in &reports {
        assert!(other.record_view_change(report.clone()).is_err());
        let mut tampered = report.clone();
        tampered.protocol_context = other_config.protocol_context;
        assert!(other.record_view_change(tampered).is_err());
    }
    assert_eq!(other.checkpoint(), saved);
    let certificate = TimeoutCertificate { view: 2, height: 1, tip: Hash::ZERO, messages: reports };
    assert!(other.validate_timeout_certificate(&certificate).is_err());
    assert!(ConsensusDriver::from_checkpoint(
        other_config,
        registry,
        Some(keys[0].clone()),
        drivers[0].checkpoint()
    )
    .is_err());
}

#[test]
fn a_rotated_commit_restores_its_view_and_the_next_height_uses_the_next_leader() {
    let (registry, keys, mut drivers) = setup(4);
    let reports = timeout(&mut drivers, &[1, 2, 3]);
    for driver in drivers.iter_mut().skip(1) {
        for report in &reports {
            driver.record_view_change(report.clone()).unwrap();
        }
    }
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[1].propose(first.clone()).unwrap();
    certify(&mut drivers, &[1, 2, 3], proposal);
    let saved = drivers[1].checkpoint();
    assert_eq!(saved.height, 1);
    assert_eq!(saved.high_qc.as_ref().unwrap().view, 2);
    assert_eq!(saved.view, 3);
    drivers[1] = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry,
        Some(keys[1].clone()),
        saved,
    )
    .unwrap();
    assert_eq!(drivers[1].leader(), keys[2].0);
    let second = block(2, first.hash(), 2);
    let (proposal, _) = drivers[2].propose(second.clone()).unwrap();
    assert_eq!(proposal.view, 3);
    assert!(proposal.timeout_certificate.is_none());
    let qc = certify(&mut drivers, &[1, 2, 3], proposal);
    assert_eq!(qc.view, 3);
    assert!(drivers[1..]
        .iter()
        .all(|driver| driver.height() == 2 && driver.view() == 4 && driver.tip() == second.hash()));
}

fn nil_report(keys: &[(Address, PrivateKey)], index: usize, view: u64) -> ViewChange {
    let mut report = ViewChange {
        protocol_context: Hash::ZERO,
        view,
        height: 1,
        tip: Hash::ZERO,
        validator: keys[index].0,
        prepared: None,
        signature: Vec::new(),
    };
    report.signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&keys[index].1, &report.signing_payload()));
    report
}

fn restore_at_view(
    registry: &ValidatorRegistry,
    keys: &[(Address, PrivateKey)],
    index: usize,
    view: u64,
) -> ConsensusDriver {
    let mut checkpoint = ConsensusDriver::new(
        ConsensusConfig::new(4).unwrap(),
        registry.clone(),
        Some(keys[index].clone()),
    )
    .unwrap()
    .checkpoint();
    let report = nil_report(keys, index, view);
    checkpoint.view = view;
    checkpoint.view_changes = vec![report.clone()];
    checkpoint.own_view_change = Some(report);
    ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry.clone(),
        Some(keys[index].clone()),
        checkpoint,
    )
    .unwrap()
}

#[test]
fn restarted_timeout_only_views_three_five_seven_converge_with_a_silent_fourth() {
    let (registry, keys, mut drivers) = setup(4);
    for (index, view) in [3, 5, 7].into_iter().enumerate() {
        drivers[index] = restore_at_view(&registry, &keys, index, view);
        drivers[index].wake_on_work(Duration::ZERO).unwrap();
    }
    let first = block(1, Hash::ZERO, 7);
    for step in 0..30 {
        let now = Duration::from_millis(step * 500);
        for driver in drivers.iter_mut().take(3) {
            driver.tick(now).unwrap();
        }
        // Perfect delivery, with each restart initially advertising a different
        // target. This deadlocked forever with an adjacent-view-only inbox.
        for _ in 0..4 {
            let reports: Vec<_> = drivers[..3]
                .iter()
                .filter_map(|driver| driver.own_view_change().cloned())
                .collect();
            for driver in drivers.iter_mut().take(3) {
                for report in &reports {
                    driver.record_view_change(report.clone()).unwrap();
                }
            }
        }
        if let Some(index) = (0..3).find(|index| {
            drivers[*index].is_leader() && drivers[*index].timeout_certificate().is_some()
        }) {
            let (proposal, _) = drivers[index].propose(first.clone()).unwrap();
            certify(&mut drivers, &[0, 1, 2], proposal);
            assert!(drivers[..3]
                .iter()
                .all(|driver| driver.height() == 1 && driver.tip() == first.hash()));
            return;
        }
        assert!(drivers[..3].iter().all(|driver| driver.height() == 0));
        assert!(drivers[..3].iter().all(|driver| driver.checkpoint().view_changes.len() <= 4));
    }
    panic!("authenticated restarted peers never aligned their timeout views");
}

#[test]
fn a_single_byzantine_high_report_cannot_jump_the_view_or_authorize_a_proposal() {
    let (_, keys, mut drivers) = setup(4);
    for view in 1000..1010 {
        drivers[2].record_view_change(nil_report(&keys, 3, view)).unwrap();
        assert_eq!(drivers[2].view(), 1);
        assert_eq!(drivers[2].checkpoint().view_changes.len(), 1);
    }
    drivers[2].record_view_change(nil_report(&keys, 1, 3)).unwrap();
    assert_eq!(drivers[2].view(), 3, "f+1-th highest target is the honest peer's view");
    assert!(drivers[2].is_leader());
    assert!(drivers[2].timeout_certificate().is_none());
    assert!(drivers[2].propose(block(1, Hash::ZERO, 3)).is_err());
    assert_eq!(drivers[2].height(), 0);
}

#[test]
fn a_lone_timeout_only_high_view_pauses_until_peers_join_and_then_rearms() {
    let (registry, keys, _) = setup(4);
    let mut high = restore_at_view(&registry, &keys, 2, 7);
    high.wake_on_work(Duration::ZERO).unwrap();
    for seconds in [0, 2, 100, 1000] {
        assert!(high.tick(Duration::from_secs(seconds)).unwrap().is_none());
        assert_eq!(high.view(), 7);
    }
    high.record_view_change(nil_report(&keys, 1, 7)).unwrap();
    assert!(
        high.tick(Duration::from_secs(1000)).unwrap().is_none(),
        "first late peer gets a full new deadline"
    );
    assert_eq!(high.view(), 7);
    high.record_view_change(nil_report(&keys, 0, 7)).unwrap();
    assert!(high.timeout_certificate().is_some());
    let (proposal, _) = high.propose(block(1, Hash::ZERO, 7)).unwrap();
    assert_eq!(proposal.view, 7);
}

#[test]
fn view_change_signatures_bind_transaction_bodies_producer_signatures_and_qc_order() {
    let (_, _, mut drivers) = setup(4);
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for prepare in prepares {
        drivers[0].record_vote(prepare).unwrap();
    }
    let report = timeout(&mut drivers, &[0]).pop().unwrap();
    drivers[1].validate_view_change(&report).unwrap();
    let mut producer_signature = report.clone();
    producer_signature.prepared.as_mut().unwrap().block.signatures.push(
        subhost_core::ValidatorSignature { validator: Address::ZERO, signature: vec![1; 96] },
    );
    let mut transaction = report.clone();
    transaction.prepared.as_mut().unwrap().block.transactions.push(subhost_core::Transaction {
        tx_type: subhost_core::TransactionType::Transfer,
        nonce: 0,
        from: Address::ZERO,
        to: Some(Address::ZERO),
        value: 1,
        gas_price: 1,
        gas_limit: 1,
        data: vec![1],
        chain_id: 1,
        public_key: [0; 32],
        signature: subhost_core::TransactionSignature::EMPTY,
    });
    let mut qc_order = report.clone();
    qc_order.prepared.as_mut().unwrap().qc.signatures.reverse();
    for altered in [producer_signature, transaction, qc_order] {
        assert_eq!(
            altered.prepared.as_ref().unwrap().block.hash(),
            report.prepared.as_ref().unwrap().block.hash()
        );
        assert_ne!(altered.signing_payload(), report.signing_payload());
        assert!(drivers[1].validate_view_change(&altered).is_err());
    }
}

#[test]
fn a_prepared_checkpoint_still_rejects_a_corrupt_nonempty_leader_envelope() {
    let (registry, keys, mut drivers) = setup(1);
    drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let saved = drivers[0].checkpoint();
    assert!(saved.prepared.is_some());
    let mut corrupt = saved.clone();
    corrupt.proposal.as_mut().unwrap().signature = vec![0xff; 96];
    assert!(ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(1).unwrap(),
        registry.clone(),
        Some(keys[0].clone()),
        corrupt
    )
    .is_err());
    let mut wrong_placeholder = saved;
    wrong_placeholder.proposal.as_mut().unwrap().signature.clear();
    wrong_placeholder.proposal.as_mut().unwrap().proposer = Address::ZERO;
    assert!(ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(1).unwrap(),
        registry,
        Some(keys[0].clone()),
        wrong_placeholder
    )
    .is_err());
}

#[test]
fn a_full_tc_brings_its_lagging_designated_leader_directly_to_view_nine() {
    let (_, keys, mut drivers) = setup(4);
    let tc = TimeoutCertificate {
        view: 9,
        height: 1,
        tip: Hash::ZERO,
        messages: (1..4).map(|i| nil_report(&keys, i, 9)).collect(),
    };
    let mut subquorum = tc.clone();
    subquorum.messages.pop();
    let mut forged = tc.clone();
    forged.messages[0].signature = vec![0xff; 96];
    let saved = drivers[0].checkpoint();
    for bad in [subquorum, forged] {
        assert!(drivers[0].receive_timeout_certificate(bad).is_err());
        assert_eq!(drivers[0].checkpoint(), saved);
    }
    assert!(drivers[0].receive_timeout_certificate(tc).unwrap());
    assert_eq!(drivers[0].view(), 9);
    assert!(drivers[0].is_leader());
    assert_eq!(drivers[0].own_view_change().unwrap().view, 9);
    assert_eq!(drivers[0].height(), 0);
    let first = block(1, Hash::ZERO, 9);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let qc = certify(&mut drivers, &[0, 1, 2], proposal);
    assert_eq!(qc.view, 9);
    assert!(drivers[..3].iter().all(|driver| driver.tip() == first.hash()));
}

#[test]
fn adopting_a_full_tc_preserves_and_reports_a_local_prepared_lock() {
    let (registry, keys, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for prepare in prepares {
        drivers[0].record_vote(prepare).unwrap();
    }
    let locked = drivers[0].prepared_certificate().unwrap();
    let tc = TimeoutCertificate {
        view: 9,
        height: 1,
        tip: Hash::ZERO,
        messages: (1..4).map(|i| nil_report(&keys, i, 9)).collect(),
    };
    drivers[0].receive_timeout_certificate(tc).unwrap();
    assert_eq!(drivers[0].prepared_certificate(), Some(locked.clone()));
    assert_eq!(drivers[0].own_view_change().unwrap().prepared, Some(locked));
    assert_eq!(drivers[0].preferred_block(), Some(first.clone()));
    assert!(drivers[0].propose(block(1, Hash::ZERO, 2)).is_err());
    let saved = drivers[0].checkpoint();
    assert_eq!(
        ConsensusDriver::from_checkpoint(
            ConsensusConfig::new(4).unwrap(),
            registry,
            Some(keys[0].clone()),
            saved.clone()
        )
        .unwrap()
        .checkpoint(),
        saved
    );
    drivers[0].propose(first).unwrap();
}

#[test]
fn full_timeout_certificates_do_not_discard_a_ready_final_certificate() {
    let (_, keys, mut drivers) = setup(1);
    drivers[0].propose(block(1, Hash::ZERO, 1)).unwrap();
    let saved = drivers[0].checkpoint();
    let tc = TimeoutCertificate {
        view: 3,
        height: 1,
        tip: Hash::ZERO,
        messages: vec![nil_report(&keys, 0, 3)],
    };
    assert!(!drivers[0].receive_timeout_certificate(tc).unwrap());
    assert_eq!(drivers[0].checkpoint(), saved);
    assert!(drivers[0].pending_qc().is_some());
}

#[test]
fn work_wakes_a_bounded_timer_without_changing_any_signing_obligation() {
    let (_, _, mut drivers) = setup(4);
    drivers[0].timeout_attempts = 20;
    drivers[0].started_at = Some(Duration::ZERO);
    assert_eq!(drivers[0].timeout_duration(), Duration::from_secs(30));
    let saved = drivers[0].checkpoint();
    drivers[0].wake_on_work(Duration::from_secs(100)).unwrap();
    assert_eq!(drivers[0].checkpoint(), saved);
    assert_eq!(drivers[0].timeout_duration(), Duration::from_secs(2));
    assert!(drivers[0].wake_on_work(Duration::from_secs(99)).is_err());
    assert!(drivers[0].tick(Duration::from_secs(101)).unwrap().is_none());
    assert!(drivers[0].tick(Duration::from_secs(102)).unwrap().is_some());
    drivers[0].config.view_timeout_ms = 60_000;
    drivers[0].timeout_attempts = 20;
    assert_eq!(drivers[0].timeout_duration(), Duration::from_secs(60));
}

#[test]
fn all_consensus_domains_bind_genesis_even_when_chain_and_block_hash_match() {
    let (registry, keys, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let prepares: Vec<_> =
        (0..3).map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap()).collect();
    for prepare in prepares {
        drivers[0].record_vote(prepare).unwrap();
    }
    let prepared = drivers[0].prepared_certificate().unwrap();
    let commit_payload = QuorumCertificate::payload_for(1, &first.hash());
    let final_qc = QuorumCertificate {
        protocol_context: Hash::ZERO,
        view: 1,
        block_hash: first.hash(),
        signatures: (0..3)
            .map(|i| {
                (
                    keys[i].0,
                    BLSScheme::encode_signature(&BLSScheme::sign(&keys[i].1, &commit_payload)),
                )
            })
            .collect(),
    };
    let mut other_config = ConsensusConfig::new(4).unwrap();
    other_config.protocol_context =
        Hash::from_data(b"same chain ID and allocations but different genesis time/policy");
    let mut other =
        ConsensusDriver::new(other_config.clone(), registry, Some(keys[1].clone())).unwrap();
    assert!(other.observe_proposal(proposal.clone()).is_err());
    let mut relabelled = proposal;
    relabelled.protocol_context = other_config.protocol_context;
    assert!(other.observe_proposal(relabelled).is_err());
    assert!(other.receive_prepare(prepared.clone()).is_err());
    let mut relabelled = prepared;
    relabelled.qc.protocol_context = other_config.protocol_context;
    assert!(other.receive_prepare(relabelled).is_err());
    assert!(other.receive_commit(first.clone(), final_qc.clone()).is_err());
    let mut relabelled = final_qc;
    relabelled.protocol_context = other_config.protocol_context;
    assert!(other.receive_commit(first, relabelled).is_err());
    assert_eq!(other.height(), 0);
}

#[test]
fn fresh_phases_get_full_deadlines_without_retransmission_extensions() {
    let (_, _, mut drivers) = setup(2);
    drivers[0].config.view_timeout_ms = 1000;
    drivers[0].tick(Duration::ZERO).unwrap();
    let (proposal, _) = drivers[0].propose(block(1, Hash::ZERO, 7)).unwrap();
    assert!(drivers[0].tick(Duration::from_millis(500)).unwrap().is_none());
    assert!(drivers[0].tick(Duration::from_millis(1490)).unwrap().is_none());
    let prepare = drivers[1].receive_proposal(proposal.clone()).unwrap();
    drivers[0].record_vote(prepare.clone()).unwrap();
    let prepared = drivers[0].prepared_certificate().unwrap();
    assert!(drivers[0].own_commit_vote().is_some());
    assert!(drivers[0].pending_qc().is_none());
    assert!(drivers[0].tick(Duration::from_millis(1495)).unwrap().is_none());
    assert!(drivers[0].tick(Duration::from_millis(2000)).unwrap().is_none());
    assert!(!drivers[0].record_vote(prepare).unwrap());
    drivers[0].receive_prepare(prepared.clone()).unwrap();
    drivers[0].observe_proposal(proposal).unwrap();
    let timeout = drivers[0].tick(Duration::from_millis(2495)).unwrap().unwrap();
    assert_eq!(timeout.view, 2);
    assert_eq!(timeout.prepared, Some(prepared));
}

fn final_qc(
    keys: &[(Address, PrivateKey)],
    indices: &[usize],
    view: u64,
    hash: Hash,
) -> QuorumCertificate {
    QuorumCertificate {
        protocol_context: Hash::ZERO,
        view,
        block_hash: hash,
        signatures: indices
            .iter()
            .map(|index| {
                let (address, key) = &keys[*index];
                (
                    *address,
                    BLSScheme::encode_signature(&BLSScheme::sign(
                        key,
                        &QuorumCertificate::payload_for(view, &hash),
                    )),
                )
            })
            .collect(),
    }
}

fn committed_pair(drivers: &mut [ConsensusDriver]) -> (Block, QuorumCertificate) {
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first.clone()).unwrap();
    let qc = certify(drivers, &[0, 1, 2], proposal);
    (first, qc)
}

#[test]
fn a_second_certificate_for_the_finalized_hash_is_duplicate_evidence() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);
    let view_before = drivers[1].view();
    let duplicate = final_qc(&keys, &[0, 1, 2], 2, first.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&first, &duplicate).unwrap(),
        FinalEvidence::Duplicate { height: 1 }
    );
    // Duplicate evidence must not disturb any active vote, view or lock.
    assert_eq!(drivers[1].view(), view_before);
    assert_eq!(drivers[1].tip(), first.hash());
    assert!(drivers[1].own_vote().is_none());
    assert!(drivers[1].prepared_certificate().is_none());
    assert!(drivers[1].halted().is_none());
}

#[test]
fn conflicting_final_hashes_at_a_finalized_height_are_accountable_evidence() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);
    let other = block(1, Hash::ZERO, 9);
    assert_ne!(other.hash(), first.hash());
    let qc = final_qc(&keys, &[0, 1, 2], 2, other.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&other, &qc).unwrap(),
        FinalEvidence::Conflicting { height: 1 }
    );
    assert_eq!(drivers[1].tip(), first.hash(), "classification never rolls back");
    assert!(drivers[1].halted().is_none(), "classifying alone must not halt");
}

#[test]
fn certified_history_below_the_tip_defers_to_the_ledger() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);
    let second = block(2, first.hash(), 2);
    let (proposal, _) = drivers[1].propose(second).unwrap();
    certify(&mut drivers, &[0, 1, 2], proposal);
    let qc = final_qc(&keys, &[0, 1, 2], 3, first.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&first, &qc).unwrap(),
        FinalEvidence::Historical { height: 1, hash: first.hash() }
    );
}

#[test]
fn certified_next_block_gap_and_non_extending_child_are_distinguished() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);

    let next = block(2, first.hash(), 2);
    let qc = final_qc(&keys, &[0, 1, 2], 2, next.hash());
    assert_eq!(drivers[1].classify_final_evidence(&next, &qc).unwrap(), FinalEvidence::Next);

    let far = block(4, Hash::from_data(&[7]), 4);
    let qc = final_qc(&keys, &[0, 1, 2], 2, far.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&far, &qc).unwrap(),
        FinalEvidence::MissingAncestors { from: 2, to: 3 }
    );

    let off_tip = block(2, Hash::from_data(&[8]), 8);
    let qc = final_qc(&keys, &[0, 1, 2], 2, off_tip.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&off_tip, &qc).unwrap(),
        FinalEvidence::Conflicting { height: 2 },
        "a certified child that ignores the finalized tip is accountable"
    );

    // Finality is monotone: a certificate from a view behind committed finality
    // cannot be the next block.
    let qc = final_qc(&keys, &[0, 1, 2], 1, next.hash());
    assert_eq!(
        drivers[1].classify_final_evidence(&next, &qc).unwrap(),
        FinalEvidence::Conflicting { height: 2 }
    );
}

#[test]
fn unauthenticated_or_mismatched_evidence_is_rejected_without_an_incident() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);
    let other = block(1, Hash::ZERO, 9);

    // Two signers are below the 3-of-4 quorum.
    let weak = final_qc(&keys, &[0, 1], 2, other.hash());
    assert!(drivers[1].classify_final_evidence(&other, &weak).is_err());

    // Valid signatures, but the certificate names the finalized hash while the
    // presented block is a different one.
    let valid = final_qc(&keys, &[0, 1, 2], 2, first.hash());
    assert!(drivers[1].classify_final_evidence(&other, &valid).is_err());
    assert!(drivers[1].halted().is_none(), "junk evidence must not halt");
}

#[test]
fn halting_is_sticky_and_stops_every_signature_but_not_certified_history() {
    let (_, keys, mut drivers) = setup(4);
    let (first, _) = committed_pair(&mut drivers);
    let mut leader = drivers[1].clone();
    let (proposal, _) = leader.propose(block(2, first.hash(), 2)).unwrap();

    drivers[1].halt(HaltReason::ConflictingFinalEvidence { height: 1 });
    drivers[1].halt(HaltReason::ConflictingFinalEvidence { height: 2 });
    assert_eq!(
        drivers[1].halted(),
        Some(&HaltReason::ConflictingFinalEvidence { height: 1 }),
        "the first reason wins"
    );
    assert!(matches!(
        drivers[1].observe_proposal(proposal.clone()),
        Err(ConsensusError::Halted(_))
    ));
    assert!(matches!(
        drivers[1].propose(block(2, first.hash(), 3)),
        Err(ConsensusError::Halted(_))
    ));

    // Certified finalized history still applies: S3.2 halts signing and
    // readiness, never the append-only ledger.
    let next = proposal.block.clone();
    let qc = final_qc(&keys, &[0, 1, 2], 2, next.hash());
    drivers[1].receive_commit(next.clone(), qc).unwrap();
    assert_eq!(drivers[1].tip(), next.hash());
}

#[test]
fn the_halt_reason_is_process_local_and_never_serialized() {
    let (registry, keys, mut drivers) = setup(4);
    let _ = committed_pair(&mut drivers);
    drivers[1].halt(HaltReason::ConflictingFinalEvidence { height: 1 });
    let checkpoint = drivers[1].checkpoint();
    let restored = ConsensusDriver::from_checkpoint(
        ConsensusConfig::new(4).unwrap(),
        registry,
        Some(keys[1].clone()),
        checkpoint,
    )
    .unwrap();
    assert!(
        restored.halted().is_none(),
        "the node persists and re-applies the incident outside the checkpoint"
    );
}

#[test]
fn a_halted_driver_refuses_the_commit_vote_when_the_prepare_quorum_forms() {
    let (_, _, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first).unwrap();
    let prepares: Vec<_> = [0, 1, 2]
        .into_iter()
        .map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap())
        .collect();
    // Halt after the Prepare vote, before the Commit phase can be signed.
    drivers[1].halt(HaltReason::ConflictingFinalEvidence { height: 1 });
    drivers[1].record_vote(prepares[0].clone()).unwrap();
    drivers[1].record_vote(prepares[1].clone()).unwrap();
    assert!(matches!(drivers[1].record_vote(prepares[2].clone()), Err(ConsensusError::Halted(_))));
    assert!(drivers[1].own_commit_vote().is_none());
    assert!(drivers[1].pending_qc().is_none());
}

#[test]
fn a_halted_driver_refuses_to_sign_a_view_change_report() {
    let (_, _, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first).unwrap();
    drivers[1].observe_proposal(proposal).unwrap();
    assert!(drivers[1].tick(Duration::ZERO).unwrap().is_none(), "arms the deadline");
    drivers[1].halt(HaltReason::ConflictingFinalEvidence { height: 1 });
    assert!(matches!(drivers[1].tick(Duration::from_millis(2000)), Err(ConsensusError::Halted(_))));
    assert!(drivers[1].own_view_change().is_none());
    assert_eq!(drivers[1].view(), 1, "a refused timeout must not advance the view");
}

#[test]
fn conflicting_reports_from_one_signer_in_one_view_are_refused() {
    let (_, keys, mut drivers) = setup(4);
    let first = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(first).unwrap();
    let prepares: Vec<_> = [0, 1, 2]
        .into_iter()
        .map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap())
        .collect();
    for vote in &prepares {
        drivers[2].record_vote(vote.clone()).unwrap();
    }
    assert!(drivers[2].prepared_certificate().is_some());
    // Signer 2's own timeout report carries its prepared proof at view 2.
    let locked_report = timeout(&mut drivers, &[2]).remove(0);
    assert_eq!(locked_report.view, 2);
    assert!(locked_report.prepared.is_some());
    assert!(drivers[3].record_view_change(locked_report).unwrap());
    let saved = drivers[3].checkpoint();

    // Same signer, same view, different content: equivocation, refused outright.
    assert!(matches!(
        drivers[3].record_view_change(nil_report(&keys, 2, 2)),
        Err(ConsensusError::InvalidProposal(_))
    ));
    assert_eq!(drivers[3].checkpoint(), saved, "a refused report changes nothing");

    // A lower report from the same signer is ignored, never a replacement.
    assert!(!drivers[3].record_view_change(nil_report(&keys, 2, 1)).unwrap());
    assert_eq!(drivers[3].checkpoint(), saved);
}

#[test]
fn a_peer_report_carrying_a_conflicting_prepared_lock_is_refused() {
    let (_, keys, mut drivers) = setup(4);
    let locked = block(1, Hash::ZERO, 1);
    let (proposal, _) = drivers[0].propose(locked).unwrap();
    let prepares: Vec<_> = [0, 1, 2]
        .into_iter()
        .map(|i| drivers[i].observe_proposal(proposal.clone()).unwrap().unwrap())
        .collect();
    for vote in &prepares {
        drivers[1].record_vote(vote.clone()).unwrap();
    }
    assert!(drivers[1].prepared_certificate().is_some());

    // An authenticated prepare proof for a different block at the SAME view.
    let rival = block(1, Hash::ZERO, 9);
    let rival_qc = PrepareCertificate {
        protocol_context: Hash::ZERO,
        view: 1,
        block_hash: rival.hash(),
        signatures: [0, 1, 2]
            .into_iter()
            .map(|i| {
                (
                    keys[i].0,
                    BLSScheme::encode_signature(&BLSScheme::sign(
                        &keys[i].1,
                        &PrepareCertificate::payload_for(1, &rival.hash()),
                    )),
                )
            })
            .collect(),
    };
    let mut report = nil_report(&keys, 3, 2);
    report.prepared = Some(PreparedBlock { block: rival, qc: rival_qc });
    report.signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&keys[3].1, &report.signing_payload()));

    let saved = drivers[1].checkpoint();
    assert!(drivers[1].record_view_change(report).is_err());
    assert_eq!(drivers[1].checkpoint(), saved, "the local lock survives the refusal");
}

#[test]
fn timeout_certificate_support_bounds_hold_at_sixty_four_validators() {
    let (registry, keys, _) = setup(64);
    let config = ConsensusConfig::new(64).unwrap();
    assert_eq!(config.max_faulty, 21);
    assert_eq!(config.quorum_threshold, 43);
    let observer = ConsensusDriver::new(config, registry.clone(), None).unwrap();
    let messages: Vec<_> = (0..43).map(|i| nil_report(&keys, i, 2)).collect();
    let tc = TimeoutCertificate { view: 2, height: 1, tip: Hash::ZERO, messages };
    observer.validate_timeout_certificate(&tc).unwrap();

    let mut short = tc.clone();
    short.messages.pop();
    assert!(observer.validate_timeout_certificate(&short).is_err(), "42 of 64 is not n-f");

    let mut over = tc.clone();
    over.messages = (0..65).map(|i| tc.messages[i % 43].clone()).collect();
    assert!(
        observer.validate_timeout_certificate(&over).is_err(),
        "more reports than registered validators is refused before any dedup"
    );

    // The other supported extreme: a single validator is its own quorum.
    let (registry1, keys1, _) = setup(1);
    let one = ConsensusDriver::new(ConsensusConfig::new(1).unwrap(), registry1, None).unwrap();
    let single = TimeoutCertificate {
        view: 2,
        height: 1,
        tip: Hash::ZERO,
        messages: vec![nil_report(&keys1, 0, 2)],
    };
    one.validate_timeout_certificate(&single).unwrap();
}
