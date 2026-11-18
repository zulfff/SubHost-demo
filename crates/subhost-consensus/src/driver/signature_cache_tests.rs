use super::*;
use subhost_core::{BlockHeader, BLOCK_GAS_LIMIT};

fn fixture(
    count: u8,
) -> (ConsensusConfig, ValidatorRegistry, Vec<(Address, PrivateKey)>, ConsensusDriver) {
    let mut registry = ValidatorRegistry::new();
    let mut keys = Vec::new();
    for index in 1..=count {
        let (secret, public) = BLSScheme::keygen();
        let address = Address::new([index; 20]);
        registry.register(address, public, &BLSScheme::proof_of_possession(&secret)).unwrap();
        keys.push((address, secret));
    }
    let mut config = ConsensusConfig::new(count as usize).unwrap();
    config.protocol_context = Hash::new([31; 32]);
    let driver =
        ConsensusDriver::new(config.clone(), registry.clone(), Some(keys[0].clone())).unwrap();
    (config, registry, keys, driver)
}

fn signature(key: &PrivateKey, payload: &[u8]) -> Vec<u8> {
    BLSScheme::encode_signature(&BLSScheme::sign(key, payload))
}

fn block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            chain_id: 1,
            height: 1,
            timestamp: 1_700_000_001,
            parent_hash: Hash::ZERO,
            state_root: Hash::new([5; 32]),
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

#[test]
fn cached_success_remains_bound_to_payload_context_phase_signer_and_signature() {
    let (config, _, keys, driver) = fixture(2);
    let hash = Hash::new([7; 32]);
    let payload = Vote::payload_for_context(config.protocol_context, VotePhase::Prepare, 1, &hash);
    let valid = signature(&keys[0].1, &payload);
    driver.verify_signature(keys[0].0, &payload, &valid).unwrap();
    driver.verify_signature(keys[0].0, &payload, &valid).unwrap();

    let mut changed_payload = payload.clone();
    changed_payload.push(0);
    let changed_context =
        Vote::payload_for_context(Hash::new([32; 32]), VotePhase::Prepare, 1, &hash);
    let changed_phase =
        Vote::payload_for_context(config.protocol_context, VotePhase::Commit, 1, &hash);
    let changed_view =
        Vote::payload_for_context(config.protocol_context, VotePhase::Prepare, 2, &hash);
    let changed_hash = Vote::payload_for_context(
        config.protocol_context,
        VotePhase::Prepare,
        1,
        &Hash::new([8; 32]),
    );
    for candidate in [changed_payload, changed_context, changed_phase, changed_view, changed_hash] {
        assert!(driver.verify_signature(keys[0].0, &candidate, &valid).is_err());
    }
    assert!(driver.verify_signature(keys[1].0, &payload, &valid).is_err());
    assert!(driver.verify_signature(Address::new([99; 20]), &payload, &valid).is_err());
    let mut negated = valid.clone();
    negated[0] ^= 0x20; // Another valid subgroup point, not merely malformed input.
    assert!(BLSScheme::decode_signature(&negated).is_some());
    assert!(driver.verify_signature(keys[0].0, &payload, &negated).is_err());

    // The concatenated bytes are unchanged, but moving the boundary cannot
    // alias a cache entry: canonical encoding must distinguish the two slices.
    let mut shifted_payload = payload.clone();
    shifted_payload.push(valid[0]);
    assert!(driver.verify_signature(keys[0].0, &shifted_payload, &valid[1..]).is_err());
    driver.verify_signature(keys[0].0, &payload, &valid).unwrap();
}

#[test]
fn repeated_invalid_attempts_never_gain_authentication_or_poison_valid_checks() {
    let (config, _, keys, driver) = fixture(1);
    let hash = Hash::new([11; 32]);
    let payload = Vote::payload_for_context(config.protocol_context, VotePhase::Prepare, 1, &hash);
    let valid = signature(&keys[0].1, &payload);
    driver.verify_signature(keys[0].0, &payload, &valid).unwrap();
    let before = driver.verified_signatures.borrow().len();
    let other = Vote::payload_for_context(config.protocol_context, VotePhase::Prepare, 2, &hash);
    let mut negated = valid.clone();
    negated[0] ^= 0x20;
    for _ in 0..2 {
        assert!(driver.verify_signature(keys[0].0, &other, &valid).is_err());
        assert!(driver.verify_signature(keys[0].0, &payload, &negated).is_err());
        assert!(driver.verify_signature(keys[0].0, &payload, &valid[..95]).is_err());
    }
    assert_eq!(driver.verified_signatures.borrow().len(), before);
    let correct_other = signature(&keys[0].1, &other);
    driver.verify_signature(keys[0].0, &other, &correct_other).unwrap();
    driver.verify_signature(keys[0].0, &payload, &valid).unwrap();
}

#[test]
fn cached_prepare_signatures_cannot_authenticate_a_final_certificate() {
    let (config, _, keys, driver) = fixture(2);
    let hash = Hash::new([21; 32]);
    let prepare = PrepareCertificate::payload_for_context(config.protocol_context, 1, &hash);
    let signatures: Vec<_> = keys
        .iter()
        .map(|(address, key)| {
            let signature = signature(key, &prepare);
            driver.verify_signature(*address, &prepare, &signature).unwrap();
            (*address, signature)
        })
        .collect();
    let converted = QuorumCertificate {
        protocol_context: config.protocol_context,
        view: 1,
        block_hash: hash,
        signatures,
    };
    assert!(driver.validate_commit_certificate(&converted).is_err());
}

#[test]
fn recovery_revalidates_signed_obligations_without_serializing_cached_successes() {
    let (config, registry, keys, mut driver) = fixture(2);
    let (_, vote) = driver.propose(block()).unwrap();
    let checkpoint = driver.checkpoint();
    let encoded = encode_canonical(&checkpoint);
    let unrelated = Vote::payload_for_context(
        config.protocol_context,
        VotePhase::Commit,
        42,
        &Hash::new([42; 32]),
    );
    let valid_unrelated = signature(&keys[0].1, &unrelated);
    driver.verify_signature(keys[0].0, &unrelated, &valid_unrelated).unwrap();
    driver
        .verify_signature(
            vote.validator,
            &Vote::payload_for_context(
                config.protocol_context,
                vote.phase,
                vote.view,
                &vote.block_hash,
            ),
            &vote.signature,
        )
        .unwrap();
    assert_eq!(encode_canonical(&driver.checkpoint()), encoded);
    let restored = ConsensusDriver::from_checkpoint(
        config.clone(),
        registry.clone(),
        Some(keys[0].clone()),
        checkpoint.clone(),
    )
    .unwrap();
    assert_eq!(restored.checkpoint(), checkpoint);

    let mut corrupt = checkpoint.clone();
    corrupt.votes[0].signature[0] ^= 0x20;
    assert!(ConsensusDriver::from_checkpoint(
        config.clone(),
        registry.clone(),
        Some(keys[0].clone()),
        corrupt
    )
    .is_err());
    // Use the same addresses/context but a different immutable key registry.
    // Previously memoized triples must not be imported into this new driver.
    let mut replacement = ValidatorRegistry::new();
    let (new_secret, new_public) = BLSScheme::keygen();
    replacement
        .register(keys[0].0, new_public, &BLSScheme::proof_of_possession(&new_secret))
        .unwrap();
    replacement
        .register(
            keys[1].0,
            BLSScheme::public_key(&keys[1].1),
            &BLSScheme::proof_of_possession(&keys[1].1),
        )
        .unwrap();
    assert!(ConsensusDriver::from_checkpoint(config, replacement, None, checkpoint).is_err());
}

#[test]
fn a_stream_of_valid_checks_evicts_small_cache_entries_without_skipping_authentication() {
    let (config, _, keys, driver) = fixture(1);
    let mut previous = 0;
    let mut evicted = false;
    let first_hash = Hash::from_data(&0u64.to_be_bytes());
    let first_payload =
        Vote::payload_for_context(config.protocol_context, VotePhase::Commit, 1, &first_hash);
    let first_signature = signature(&keys[0].1, &first_payload);
    for index in 0..40u64 {
        let hash = Hash::from_data(&index.to_be_bytes());
        let payload =
            Vote::payload_for_context(config.protocol_context, VotePhase::Commit, index + 1, &hash);
        driver.verify_signature(keys[0].0, &payload, &signature(&keys[0].1, &payload)).unwrap();
        let retained = driver.verified_signatures.borrow().len();
        // A one-validator instance uses the smallest configured cache window.
        assert!(retained <= 16);
        evicted |= retained < previous;
        previous = retained;
    }
    assert!(evicted, "a bounded cache must release entries under sustained valid traffic");
    driver.verify_signature(keys[0].0, &first_payload, &first_signature).unwrap();
    let prepare =
        Vote::payload_for_context(config.protocol_context, VotePhase::Prepare, 1, &first_hash);
    assert!(driver.verify_signature(keys[0].0, &prepare, &first_signature).is_err());
}
