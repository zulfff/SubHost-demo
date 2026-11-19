//! S8 contract tests for the two read-only observability accessors (ADR-013):
//! finalized height advances only at a verified final Commit boundary, and the
//! ledger disk measurement is taken under the commit lock and never fakes a zero.

use crate::test_support::{signed_transfer, signing_key};
use crate::{BlockAuthority, RpcState};
use subhost_consensus::{QuorumCertificate, ValidatorRegistry};
use subhost_core::{Address, Block, Hash};
use subhost_crypto::{BLSScheme, PrivateKey};

fn signing_registry() -> (ValidatorRegistry, Address, PrivateKey) {
    let (secret, public) = BLSScheme::keygen();
    let producer = Address::new([9; 20]);
    let mut registry = ValidatorRegistry::new();
    registry.register(producer, public, &BLSScheme::proof_of_possession(&secret)).unwrap();
    (registry, producer, secret)
}

fn final_qc(
    block: &Block,
    producer: Address,
    secret: &PrivateKey,
    context: Hash,
    view: u64,
) -> QuorumCertificate {
    let hash = block.hash();
    QuorumCertificate {
        protocol_context: context,
        view,
        block_hash: hash,
        signatures: vec![(
            producer,
            BLSScheme::encode_signature(&BLSScheme::sign(
                secret,
                &QuorumCertificate::payload_for_context(context, view, &hash),
            )),
        )],
    }
}

fn validator_state(
    state: RpcState,
    context: Hash,
) -> (RpcState, ValidatorRegistry, Address, PrivateKey) {
    let (registry, producer, secret) = signing_registry();
    let state = state
        .with_consensus_context(context)
        .with_validators(registry.clone())
        .with_authority(BlockAuthority::new(registry.clone(), producer, secret.clone()).unwrap());
    (state, registry, producer, secret)
}

fn fund_and_build(state: &RpcState) -> Block {
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
    state.build_block().unwrap().unwrap()
}

#[test]
fn demo_commits_advance_the_tip_but_never_finalized_height() {
    let state = RpcState::in_memory(1).unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
    assert_eq!(state.block_height(), 1);
    assert_eq!(
        state.finalized_height(),
        0,
        "a single-writer demo commit is not finality evidence (Q-020)"
    );
}

#[test]
fn a_verified_final_commit_publishes_finalized_height() {
    let context = Hash::from_data(b"finality-contract");
    let (state, _, producer, secret) = validator_state(RpcState::in_memory(1).unwrap(), context);
    assert_eq!(state.finalized_height(), 0);
    let block = fund_and_build(&state);
    let qc = final_qc(&block, producer, &secret, context, 3);
    state.commit_qc(block.clone(), &qc).unwrap();
    assert_eq!(state.block_height(), 1);
    assert_eq!(state.finalized_height(), 1);

    // A rejected certificate must not move the gauge.
    let next = state.build_block().unwrap();
    assert!(next.is_none(), "the pool is empty after the commit");
    let mut forged = qc.clone();
    forged.signatures[0].1 = vec![0xff; 96];
    assert!(state.commit_qc(block, &forged).is_err());
    assert_eq!(state.finalized_height(), 1, "a failed commit publishes nothing");
}

#[test]
fn a_restored_tip_certificate_republishes_finalized_height() {
    let dir = tempfile::tempdir().unwrap();
    let context = Hash::from_data(b"finality-restore");
    let (state, registry, producer, secret) =
        validator_state(RpcState::with_data_dir(1, dir.path()).unwrap(), context);
    let block = fund_and_build(&state);
    let qc = final_qc(&block, producer, &secret, context, 3);
    state.commit_qc(block, &qc).unwrap();
    drop(state);

    // A fresh process has no verified finality until the tip certificate is
    // checked again; restored block height alone is not evidence.
    let restored = RpcState::with_data_dir(1, dir.path())
        .unwrap()
        .with_consensus_context(context)
        .with_validators(registry);
    assert_eq!(restored.block_height(), 1);
    assert_eq!(restored.finalized_height(), 0);
    restored.restore_committed_qc(&qc).unwrap();
    assert_eq!(restored.finalized_height(), 1);
}

#[test]
fn disk_stats_measures_the_active_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let context = Hash::from_data(b"disk-measurement");
    let (state, _, producer, secret) =
        validator_state(RpcState::with_data_dir(1, dir.path()).unwrap(), context);
    let block = fund_and_build(&state);
    let qc = final_qc(&block, producer, &secret, context, 3);
    state.commit_qc(block, &qc).unwrap();

    let stats = state.ledger_disk_stats().unwrap();
    assert!(stats.active_file_bytes > 0, "a committed ledger has an active footprint");
}

#[test]
fn missing_active_data_is_an_error_not_a_false_zero() {
    let dir = tempfile::tempdir().unwrap();
    let context = Hash::from_data(b"disk-failure");
    let (state, _, producer, secret) =
        validator_state(RpcState::with_data_dir(1, dir.path()).unwrap(), context);
    let block = fund_and_build(&state);
    let qc = final_qc(&block, producer, &secret, context, 3);
    state.commit_qc(block, &qc).unwrap();
    let before = state.ledger_disk_stats().unwrap();
    assert!(before.active_file_bytes > 0);

    // Delete a named segment: the measurement must fail, never report zero.
    // The node hook keeps the previous gauge value on this error (ADR-013).
    let segments = dir.path().join(subhost_storage::LEDGER_DIR_NAME).join("segments");
    let victim = std::fs::read_dir(&segments).unwrap().next().unwrap().unwrap().path();
    std::fs::remove_file(victim).unwrap();
    assert!(state.ledger_disk_stats().is_err());
}
