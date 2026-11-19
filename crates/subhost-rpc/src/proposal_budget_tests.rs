use crate::state::read;
use crate::test_support::{signed_transfer, signing_key};
use crate::{BlockAuthority, RpcError, RpcState};
use ed25519_dalek::{Signer, SigningKey};
use subhost_consensus::{driver::PrepareCertificate, QuorumCertificate, ValidatorRegistry};
use subhost_core::{encode_canonical, Address, Block, Hash, Transaction, TransactionSignature};
use subhost_crypto::{BLSScheme, PrivateKey};

fn signing_registry() -> (ValidatorRegistry, Address, PrivateKey) {
    let (secret, public) = BLSScheme::keygen();
    let producer = Address::new([9; 20]);
    let mut registry = ValidatorRegistry::new();
    registry.register(producer, public, &BLSScheme::proof_of_possession(&secret)).unwrap();
    (registry, producer, secret)
}

fn resign(mut transaction: Transaction, key: &SigningKey) -> Transaction {
    transaction.signature =
        TransactionSignature::from_ed25519(&key.sign(&transaction.signing_payload()).to_bytes());
    assert!(transaction.verify_signature());
    transaction
}

fn manifest(directory: &std::path::Path) -> Vec<u8> {
    std::fs::read(
        directory.join(subhost_storage::LEDGER_DIR_NAME).join(subhost_storage::MANIFEST_FILE_NAME),
    )
    .unwrap()
}

#[test]
fn oversize_admission_and_replacement_preserve_pool_state_and_ledger() {
    let directory = tempfile::tempdir().unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let (registry, producer, secret) = signing_registry();
    let limit = 4096;
    let state = RpcState::with_data_dir(1, directory.path())
        .unwrap()
        .with_consensus_context(Hash::new([3; 32]))
        .with_authority(BlockAuthority::new(registry, producer, secret).unwrap())
        .with_proposal_bytes_limit(limit);
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    let root = state.state_root();
    let disk = manifest(directory.path());
    let mut events = state.subscribe_blocks();
    let mut oversized = signed_transfer(&key, 0, 10);
    oversized.data = vec![u8::MAX; limit];
    oversized = resign(oversized, &key);
    let oversized_hash = oversized.hash();
    assert!(encode_canonical(&oversized).len() > limit);
    assert!(matches!(state.submit_transaction(oversized), Err(RpcError::TransactionRejected(_))));
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.state_root(), root);
    assert_eq!(state.block_height(), 0);
    assert_eq!(state.tip_hash(), Hash::ZERO);
    assert_eq!(state.nonce(&sender), 0);
    assert_eq!(state.balance(&sender), 1_000_000);
    assert!(state.receipt(&oversized_hash).is_none());
    assert_eq!(manifest(directory.path()), disk);
    assert!(events.try_recv().is_err());

    let original = signed_transfer(&key, 0, 10);
    let original_hash = state.submit_transaction(original.clone()).unwrap();
    let mut replacement = original.clone();
    replacement.gas_price = original.gas_price + 1;
    replacement.data = vec![u8::MAX; limit];
    replacement = resign(replacement, &key);
    let replacement_hash = replacement.hash();
    assert!(matches!(state.submit_transaction(replacement), Err(RpcError::TransactionRejected(_))));
    assert_eq!(read(&state.mempool).pending(), vec![original]);
    assert_eq!(state.pending_count(), 1);
    assert_eq!(state.state_root(), root);
    assert_eq!(state.block_height(), 0);
    assert_eq!(state.tip_hash(), Hash::ZERO);
    assert_eq!(state.nonce(&sender), 0);
    assert_eq!(state.balance(&sender), 1_000_000);
    assert!(state.receipt(&replacement_hash).is_none());
    assert!(state.receipt(&original_hash).is_none());
    assert_eq!(manifest(directory.path()), disk);
    assert!(events.try_recv().is_err());
}

fn proposed_block(
    registry: &ValidatorRegistry,
    producer: Address,
    secret: &PrivateKey,
    key: &SigningKey,
    context: Hash,
    data_bytes: usize,
) -> Block {
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let state = RpcState::in_memory(1)
        .unwrap()
        .with_consensus_context(context)
        .with_authority(BlockAuthority::new(registry.clone(), producer, secret.clone()).unwrap());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    let mut transaction = signed_transfer(key, 0, 10);
    transaction.data = vec![u8::MAX; data_bytes];
    state.submit_transaction(resign(transaction, key)).unwrap();
    state.build_block().unwrap().unwrap()
}

fn certificate(
    block: &Block,
    producer: Address,
    secret: &PrivateKey,
    context: Hash,
    prepare: bool,
) -> QuorumCertificate {
    let view = 3;
    let hash = block.hash();
    let payload = if prepare {
        PrepareCertificate::payload_for_context(context, view, &hash)
    } else {
        QuorumCertificate::payload_for_context(context, view, &hash)
    };
    QuorumCertificate {
        protocol_context: context,
        view,
        block_hash: hash,
        signatures: vec![(
            producer,
            BLSScheme::encode_signature(&BLSScheme::sign(secret, &payload)),
        )],
    }
}

#[test]
fn certified_validation_rejects_budget_context_and_prepare_domains_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let (registry, producer, secret) = signing_registry();
    let context = Hash::new([7; 32]);
    let state = RpcState::with_data_dir(1, directory.path())
        .unwrap()
        .with_validators(registry.clone())
        .with_consensus_context(context)
        .with_proposal_bytes_limit(4096);
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    let root = state.state_root();
    let disk = manifest(directory.path());
    let mut events = state.subscribe_blocks();
    let block = proposed_block(&registry, producer, &secret, &key, context, 0);
    let large = proposed_block(&registry, producer, &secret, &key, context, 8192);
    assert!(encode_canonical(&large).len() > state.proposal_bytes_limit().unwrap());
    let too_large = certificate(&large, producer, &secret, context, false);
    let foreign = certificate(&block, producer, &secret, Hash::new([8; 32]), false);
    let prepare = certificate(&block, producer, &secret, context, true);
    for (candidate, qc) in [(&large, too_large), (&block, foreign), (&block, prepare)] {
        assert!(state.validate_certified_block(candidate, &qc).is_err());
        assert_eq!(state.state_root(), root);
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.tip_hash(), Hash::ZERO);
        assert_eq!(state.committed_view(), Some(0));
        assert_eq!(state.pending_count(), 0);
        assert!(state.receipt(&candidate.transactions[0].hash()).is_none());
        assert_eq!(manifest(directory.path()), disk);
        assert!(events.try_recv().is_err());
    }
    let valid = certificate(&block, producer, &secret, context, false);
    assert!(state.validate_certified_block(&block, &valid).is_ok());
    assert_eq!(state.state_root(), root);
    assert_eq!(state.block_height(), 0);
    assert_eq!(state.committed_view(), Some(0));
    assert!(state.receipt(&block.transactions[0].hash()).is_none());
    assert_eq!(manifest(directory.path()), disk);
    assert!(events.try_recv().is_err());
}

#[test]
fn the_default_calldata_budget_is_256_kibibytes() {
    let state = RpcState::in_memory(1).unwrap();
    assert_eq!(state.proposal_calldata_bytes_limit(), Some(256 * 1024));
    let tightened = state.with_proposal_calldata_bytes_limit(4096);
    assert_eq!(tightened.proposal_calldata_bytes_limit(), Some(4096));
}

#[test]
fn the_calldata_budget_binds_selection_before_gas_or_total_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let key = signing_key(8);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let (registry, producer, secret) = signing_registry();
    let store = subhost_mempool::MempoolStore::open(1, Hash::ZERO, directory.path()).unwrap();
    let state = RpcState::with_data_dir(1, directory.path())
        .unwrap()
        .with_consensus_context(Hash::new([4; 32]))
        .with_authority(BlockAuthority::new(registry, producer, secret).unwrap())
        .with_proposal_calldata_bytes_limit(16 * 1024)
        .with_pending_store(store.clone());
    state.seed_accounts([(sender, 1_000_000_000)]).unwrap();

    // Seed both entries through the restore path: submit dry-runs the next
    // block, and the second payload deliberately does not fit beside the
    // first under this budget (pinned separately below).
    let mut pool = subhost_mempool::Mempool::default();
    for nonce in 0..2 {
        let mut transaction = signed_transfer(&key, nonce, 10);
        transaction.data = vec![0; 12 * 1024];
        pool.add(resign(transaction, &key)).unwrap();
    }
    store.persist(&pool).unwrap();
    state.restore_pending().unwrap();
    assert_eq!(state.pending_count(), 2);

    // Gas and total bytes both fit two entries; the 16 KiB calldata budget
    // admits only the first 12 KiB payload (Q-037 L0).
    let block = state.build_block().unwrap().unwrap();
    assert_eq!(block.transactions.len(), 1);
    assert_eq!(block.transactions[0].nonce, 0);
    assert_eq!(state.pending_count(), 2, "build_block must not drain the pool");
}

#[test]
fn a_full_calldata_budget_refuses_the_submit_without_touching_the_pool() {
    let directory = tempfile::tempdir().unwrap();
    let key = signing_key(10);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let (registry, producer, secret) = signing_registry();
    let state = RpcState::with_data_dir(1, directory.path())
        .unwrap()
        .with_consensus_context(Hash::new([6; 32]))
        .with_authority(BlockAuthority::new(registry, producer, secret).unwrap())
        .with_proposal_calldata_bytes_limit(16 * 1024);
    state.seed_accounts([(sender, 1_000_000_000)]).unwrap();
    let mut first = signed_transfer(&key, 0, 10);
    first.data = vec![0; 12 * 1024];
    state.submit_transaction(resign(first, &key)).unwrap();

    // The second payload fits the pool cap but not the next block beside the
    // first: the submit dry-run refuses it and the pool stays exactly as it
    // was — the same semantics as the total-byte budget.
    let mut second = signed_transfer(&key, 1, 10);
    second.data = vec![0; 12 * 1024];
    let second = resign(second, &key);
    assert!(matches!(
        state.submit_transaction(second.clone()),
        Err(RpcError::TransactionRejected(_))
    ));
    assert_eq!(state.pending_count(), 1);
    assert!(!read(&state.mempool).contains(&second.hash()));
}

#[test]
fn the_calldata_budget_accepts_a_block_exactly_at_the_budget() {
    let directory = tempfile::tempdir().unwrap();
    let key = signing_key(9);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let (registry, producer, secret) = signing_registry();
    let state = RpcState::with_data_dir(1, directory.path())
        .unwrap()
        .with_consensus_context(Hash::new([5; 32]))
        .with_authority(BlockAuthority::new(registry, producer, secret).unwrap())
        .with_proposal_calldata_bytes_limit(24 * 1024);
    state.seed_accounts([(sender, 1_000_000_000)]).unwrap();
    for nonce in 0..2 {
        let mut transaction = signed_transfer(&key, nonce, 10);
        transaction.data = vec![0; 12 * 1024];
        state.submit_transaction(resign(transaction, &key)).unwrap();
    }
    let block = state.build_block().unwrap().unwrap();
    assert_eq!(block.transactions.len(), 2, "exactly at the budget is inside it");
}
