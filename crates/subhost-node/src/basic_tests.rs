use super::*;
use ed25519_dalek::{Signer, SigningKey};
use subhost_consensus::driver::VotePhase;
use subhost_core::{
    Address, FeePolicy, Transaction, TransactionSignature, TransactionType, ValidatorInfo,
    BLOCK_GAS_LIMIT, FEE_CHANGE_DENOMINATOR, FEE_ELASTICITY_MULTIPLIER, FEE_HEADER_VERSION,
};
use subhost_mempool::{Mempool, MempoolStore, MEMPOOL_FILE_NAME};

fn test_config(directory: impl Into<PathBuf>) -> NodeConfig {
    let directory = directory.into();
    let mut config = NodeConfig::new(&directory);
    config.single_writer = GenesisConfig::load(&directory.join(GENESIS_FILE_NAME))
        .map_or(true, |genesis| genesis.initial_validators.is_empty());
    config
}

fn genesis_with(chain_id: ChainId, allocation: Option<(Address, u128)>) -> GenesisConfig {
    let mut genesis = GenesisConfig { chain_id, ..Default::default() };
    if let Some((address, balance)) = allocation {
        genesis.allocations.insert(address, balance);
    }
    genesis
}

/// A genesis with one real validator at `[9; 20]`, and its signing secret.
fn genesis_with_validator(chain_id: ChainId) -> (GenesisConfig, Address, PrivateKey) {
    let (secret, _) = BLSScheme::keygen();
    let address = Address::new([9; 20]);
    let mut genesis = genesis_with(chain_id, None);
    genesis.initial_validators = vec![ValidatorInfo {
        address,
        public_key: BLSScheme::encode_public_key(&BLSScheme::public_key(&secret)),
        power: 100,
        proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(&secret)),
    }];
    (genesis, address, secret)
}

/// Write a producer key the way `subhost init --validator` will.
fn write_test_producer_key(dir: &Path, secret: &PrivateKey) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(PRODUCER_KEY_FILE_NAME);
    std::fs::write(&path, hex::encode(BLSScheme::encode_secret_key(secret))).unwrap();
    set_mode_0600(&path);
    path
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) {}

#[test]
fn bootstrap_creates_the_data_directory_and_defaults_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("nested").join("data");
    let node = Node::bootstrap(test_config(&data_dir)).unwrap();

    assert!(data_dir.is_dir(), "the data directory must be created");
    assert_eq!(node.chain_id(), 1);
    assert_eq!(node.state().block_height(), 0);
    assert!(node.genesis().is_none());
}

#[test]
fn bootstrap_applies_genesis_allocations_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let address = Address::new([1; 20]);
    genesis_with(7, Some((address, 500))).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(node.chain_id(), 7, "genesis chain ID must win");
    assert_eq!(node.state().balance(&address), 500);
    drop(node);

    // A restart must not re-apply allocations over the restored ledger.
    let restarted = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(restarted.state().balance(&address), 500);
    assert!(restarted.state().has_persisted_state());
}

#[test]
fn bootstrap_rejects_an_invalid_genesis_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(GENESIS_FILE_NAME), "{ not json").unwrap();
    assert!(matches!(Node::bootstrap(test_config(dir.path())), Err(NodeError::Genesis(_))));
}

#[test]
fn validator_mode_requires_a_genesis_validator_set() {
    let dir = tempfile::tempdir().unwrap();
    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };

    // No genesis at all.
    assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::MissingGenesis(_))));

    // Genesis with an empty validator set.
    genesis_with(1, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    assert!(matches!(Node::bootstrap(config), Err(NodeError::Genesis(_))));
}

#[test]
fn validator_mode_boots_with_the_default_producer_key() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, address, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let key_path = write_test_producer_key(dir.path(), &secret);

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert_eq!(config.producer_key_path(), Some(key_path));
    let node = Node::bootstrap(config).unwrap();

    assert_eq!(node.genesis().unwrap().total_voting_power(), 100);
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    node.state().seed_accounts([(sender, 1_000_000)]).unwrap();
    node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
    assert_eq!(
        node.state().block_height(),
        0,
        "under consensus the RPC submission only admits; the producer commits"
    );

    let driver = node.consensus().expect("a validator node must own a driver");
    let mut driver = driver.lock().unwrap();
    assert!(produce_once_inner(
        node.state(),
        &mut driver,
        node.consensus_journal.as_deref(),
        node.certified_history.as_deref(),
        node.retention.as_deref(),
    )
    .unwrap()
    .is_some());
    drop(driver);

    let block = node.state().block_at_height(1).unwrap();
    assert_eq!(block.header.validator, address, "blocks must be signed as the genesis validator");
    assert_eq!(block.signatures.len(), 1);
}

pub(super) fn validator_nodes(count: u8) -> (Vec<tempfile::TempDir>, Vec<Node>) {
    validator_nodes_with_retention(count, None)
}

/// The same cluster with an optional automatic-retention bound (Q-036).
fn validator_nodes_with_retention(
    count: u8,
    retention_blocks: Option<u64>,
) -> (Vec<tempfile::TempDir>, Vec<Node>) {
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let keys: Vec<_> = (0..count).map(|_| BLSScheme::keygen().0).collect();
    let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.initial_validators = keys
        .iter()
        .enumerate()
        .map(|(index, secret)| ValidatorInfo {
            address: Address::new([index as u8 + 1; 20]),
            public_key: BLSScheme::encode_public_key(&BLSScheme::public_key(secret)),
            power: 100,
            proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                secret,
            )),
        })
        .collect();
    let dirs: Vec<_> = keys
        .iter()
        .map(|secret| {
            let dir = tempfile::tempdir().unwrap();
            genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
            write_test_producer_key(dir.path(), secret);
            dir
        })
        .collect();
    let nodes = dirs
        .iter()
        .map(|dir| {
            Node::bootstrap(NodeConfig {
                validator: true,
                single_writer: false,
                retention_blocks,
                ..test_config(dir.path())
            })
            .unwrap()
        })
        .collect();
    (dirs, nodes)
}

#[test]
fn four_validator_nodes_require_three_votes_before_a_ledger_write() {
    // Q-031: one live cluster at a time, shared with the real-network suites.
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    for voting_nodes in [4, 3, 2] {
        let (dirs, nodes) = validator_nodes(4);
        let manifests: Vec<_> = dirs
            .iter()
            .map(|dir| {
                std::fs::read(
                    dir.path()
                        .join(subhost_storage::LEDGER_DIR_NAME)
                        .join(subhost_storage::MANIFEST_FILE_NAME),
                )
                .unwrap()
            })
            .collect();
        let hash =
            nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
        let round = produce_for_test(&nodes[0]).unwrap().unwrap();
        let mut prepares = vec![round.vote.as_ref().unwrap().clone()];
        for node in &nodes[1..voting_nodes] {
            prepares
                .push(deliver_for_test(node, proposal_for_test(&round.proposal)).unwrap().unwrap());
        }
        assert!(prepares.iter().all(|vote| vote.phase == VotePhase::Prepare));
        for (index, node) in nodes[..voting_nodes].iter().enumerate() {
            for vote in &prepares {
                deliver_for_test(node, vote_message(vote)).unwrap();
            }
            // A prepare quorum creates the commit-phase obligation, never a
            // receipt or a canonical block. Both quorums are required.
            let driver = node.consensus().unwrap().lock().unwrap();
            assert_eq!(node.state().block_height(), 0);
            assert_eq!(driver.height(), 0);
            assert!(node.state().receipt(&hash).is_none());
            assert_eq!(node.state().balance(&Address::new([2; 20])), 0);
            assert_eq!(driver.prepared_certificate().is_some(), voting_nodes >= 3);
            assert_eq!(driver.own_commit_vote().is_some(), voting_nodes >= 3);
            assert!(driver.pending_qc().is_none());
            assert_eq!(
                std::fs::read(
                    dirs[index]
                        .path()
                        .join(subhost_storage::LEDGER_DIR_NAME)
                        .join(subhost_storage::MANIFEST_FILE_NAME)
                )
                .unwrap(),
                manifests[index]
            );
        }
        let commits: Vec<_> = nodes[..voting_nodes]
            .iter()
            .filter_map(|node| node.consensus().unwrap().lock().unwrap().own_commit_vote().cloned())
            .collect();
        assert!(commits.iter().all(|vote| vote.phase == VotePhase::Commit));
        for (index, node) in nodes[..voting_nodes].iter().enumerate() {
            for vote in &commits {
                deliver_for_test(node, vote_message(vote)).unwrap();
            }
            let driver = node.consensus().unwrap().lock().unwrap();
            if voting_nodes >= 3 {
                assert_eq!(node.state().block_height(), 1);
                assert_eq!(driver.height(), 1);
                assert_eq!(driver.view(), 2);
                assert_eq!(driver.tip(), round.proposal.block.hash());
                assert!(node.state().receipt(&hash).is_some());
                assert_eq!(node.state().balance(&Address::new([2; 20])), 10);
                assert_eq!(
                    driver.committed_qc().unwrap().protocol_context,
                    node.state().consensus_context()
                );
            } else {
                assert_eq!(node.state().block_height(), 0);
                assert_eq!(driver.height(), 0);
                assert!(node.state().receipt(&hash).is_none());
                assert_eq!(node.state().balance(&Address::new([2; 20])), 0);
                assert_eq!(
                    std::fs::read(
                        dirs[index]
                            .path()
                            .join(subhost_storage::LEDGER_DIR_NAME)
                            .join(subhost_storage::MANIFEST_FILE_NAME)
                    )
                    .unwrap(),
                    manifests[index]
                );
            }
        }
    }
}

#[test]
fn an_observer_can_commit_a_certified_block_without_a_signing_key() {
    let (dirs, nodes) = validator_nodes(1);
    let observer_dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        dirs[0].path().join(GENESIS_FILE_NAME),
        observer_dir.path().join(GENESIS_FILE_NAME),
    )
    .unwrap();
    let observer = Node::bootstrap(test_config(observer_dir.path())).unwrap();
    assert!(!observer.consensus().unwrap().lock().unwrap().is_validator());
    nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let round = produce_for_test(&nodes[0]).unwrap().unwrap();
    let qc = nodes[0].consensus().unwrap().lock().unwrap().committed_qc().unwrap().clone();
    let block = nodes[0].state().block_at_height(1).unwrap();
    assert!(deliver_for_test(&observer, proposal_for_test(&round.proposal)).unwrap().is_none());
    assert_eq!(observer.state().block_height(), 0);
    assert!(observer.consensus().unwrap().lock().unwrap().own_vote().is_none());
    assert!(observer.consensus().unwrap().lock().unwrap().own_commit_vote().is_none());
    let message = NetworkMessage::BlockCommit {
        view: qc.view,
        data: encode_block(&block),
        signatures: qc
            .signatures
            .into_iter()
            .map(|(address, signature)| (*address.as_bytes(), signature))
            .collect(),
    };
    assert!(deliver_for_test(&observer, message.clone()).unwrap().is_none());
    assert_eq!(observer.state().block_height(), 1);
    assert_eq!(observer.state().tip_hash(), block.hash());
    deliver_for_test(&observer, message).unwrap();
    assert_eq!(observer.state().balance(&Address::new([2; 20])), 10);
}

#[test]
fn a_failed_quorum_write_keeps_the_tip_and_retries_the_certificate() {
    let (dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    let tx = signed_transfer(&signing_key(7), 0, 10);
    let hash = node.state().submit_transaction(tx).unwrap();
    let ledger = dirs[0].path().join(subhost_storage::LEDGER_DIR_NAME);
    let saved = dirs[0].path().join("saved-ledger");
    std::fs::rename(&ledger, &saved).unwrap();
    std::fs::write(&ledger, b"block ledger directory creation").unwrap();
    let mut driver = node.consensus().unwrap().lock().unwrap();
    assert!(produce_once(node.state(), &mut driver).is_err());
    assert_eq!(driver.height(), 0);
    assert_eq!(node.state().block_height(), 0);
    assert!(driver.pending_qc().is_some());
    assert!(node.state().receipt(&hash).is_none());
    assert_eq!(node.state().pending_count(), 1);
    let block_hash = driver.open_proposal().unwrap().block.hash();
    std::fs::remove_file(&ledger).unwrap();
    std::fs::rename(&saved, &ledger).unwrap();
    produce_once(node.state(), &mut driver).unwrap();
    assert_eq!(driver.tip(), block_hash);
    assert_eq!(node.state().tip_hash(), block_hash);
    assert_eq!(node.state().pending_count(), 0);
    assert!(node.state().receipt(&hash).is_some());
}

#[test]
fn restarting_a_validator_restores_the_committed_height_and_view() {
    let (_dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    node.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce_for_test(node).unwrap();
    let first_hash = node.state().tip_hash();
    let config = node.config().clone();
    drop(nodes);
    let restarted = Node::bootstrap(config).unwrap();
    assert_eq!(restarted.consensus().unwrap().lock().unwrap().view(), 2);
    assert_eq!(restarted.state().committed_view(), Some(1));
    restarted.state().submit_transaction(signed_transfer(&signing_key(7), 1, 20)).unwrap();
    produce_for_test(&restarted).unwrap();
    assert_eq!(restarted.state().block_height(), 2);
    assert_eq!(restarted.state().block_at_height(2).unwrap().header.parent_hash, first_hash);
    assert_eq!(restarted.state().committed_view(), Some(2));
}

#[test]
fn a_lagging_validator_backfills_certified_history_and_restarts_at_the_synced_tip() {
    let (dirs, nodes) = validator_nodes(1);
    let producer = &nodes[0];
    for nonce in 0..3 {
        producer.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 10)).unwrap();
        produce_for_test(producer).unwrap();
    }
    assert_eq!(producer.state().block_height(), 3);
    assert_eq!(
        producer.certified_history.as_ref().map(|history| history.lock().unwrap().len()),
        Some(3),
        "every committed certificate is archived before serving"
    );

    let dir = tempfile::tempdir().unwrap();
    for file in [GENESIS_FILE_NAME, PRODUCER_KEY_FILE_NAME] {
        std::fs::copy(dirs[0].path().join(file), dir.path().join(file)).unwrap();
    }
    let lagging =
        Node::bootstrap(NodeConfig { validator: true, ..test_config(dir.path()) }).unwrap();
    assert_eq!(lagging.state().block_height(), 0, "the replica starts behind");

    let (items, applied) = {
        let history = producer.certified_history.as_ref().unwrap();
        let served = serve_certified_range(producer.state(), history, 1, 3).unwrap().unwrap();
        let NetworkMessage::Blocks { items } = served else {
            panic!("serving certified history must answer with a block range");
        };
        let mut driver = lagging.consensus().unwrap().lock().unwrap();
        let mut candidates = CandidateBuffer::default();
        let mut commit = node_commit(&lagging, &mut candidates);
        let applied =
            apply_certified_range(lagging.state(), &mut driver, &mut commit, &items).unwrap();
        (items.len(), applied)
    };
    assert_eq!(items, 3);
    assert_eq!(applied, 3);
    assert_eq!(lagging.state().block_height(), 3);
    assert_eq!(lagging.state().tip_hash(), producer.state().tip_hash());
    assert_eq!(lagging.state().state_root(), producer.state().state_root());

    let config = lagging.config().clone();
    assert_eq!(
        lagging.certified_history.as_ref().unwrap().lock().unwrap().tip(),
        Some(3),
        "adopted history is archived for the next peer"
    );
    drop(lagging);
    let reopened = Node::bootstrap(config).unwrap();
    assert_eq!(reopened.state().block_height(), 3);
    assert_eq!(reopened.state().tip_hash(), producer.state().tip_hash());
}

/// Two validators that share a genesis and producer key but commit different
/// blocks at height 1. Both certificates are authentic, so the pair is a real
/// finality contradiction rather than junk.
fn diverged_validators() -> (Vec<tempfile::TempDir>, Node, Node) {
    let (dirs, nodes) = validator_nodes(1);
    let first = nodes.into_iter().next().unwrap();
    first.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce_for_test(&first).unwrap();
    assert_eq!(first.state().block_height(), 1);

    let dir = tempfile::tempdir().unwrap();
    for file in [GENESIS_FILE_NAME, PRODUCER_KEY_FILE_NAME] {
        std::fs::copy(dirs[0].path().join(file), dir.path().join(file)).unwrap();
    }
    let second =
        Node::bootstrap(NodeConfig { validator: true, ..test_config(dir.path()) }).unwrap();
    second.state().submit_transaction(signed_transfer(&signing_key(7), 0, 777)).unwrap();
    produce_for_test(&second).unwrap();
    assert_eq!(second.state().block_height(), 1);
    assert_ne!(first.state().tip_hash(), second.state().tip_hash());

    (dirs, first, second)
}

#[test]
fn conflicting_final_evidence_is_persisted_halts_and_survives_restart() {
    let (_dirs, local, remote) = diverged_validators();
    let local_hash = local.state().tip_hash();
    let message = {
        let driver = remote.consensus().unwrap().lock().unwrap();
        certified_tip_message(remote.state(), &driver).expect("the remote tip is certified")
    };

    let mut candidates = CandidateBuffer::default();
    let mut driver = local.consensus().unwrap().lock().unwrap();
    let mut commit = node_commit(&local, &mut candidates);
    let before_view = driver.view();
    handle_consensus_message_inner(local.state(), &mut driver, message, &mut commit).unwrap();

    assert!(driver.halted().is_some(), "contradictory finality must halt signing");
    assert_eq!(driver.view(), before_view, "a halt changes no vote obligation");
    assert_eq!(local.state().tip_hash(), local_hash, "the ledger is never rolled back");
    assert_eq!(local.fork_log.as_ref().unwrap().lock().unwrap().latest().unwrap().height, 1);
    assert!(!local.metrics.health().consensus_alive);
    drop(driver);

    let config = local.config().clone();
    drop(local);
    let restarted = Node::bootstrap(config).unwrap();
    let driver = restarted.consensus().unwrap().lock().unwrap();
    assert!(driver.halted().is_some(), "a durable incident re-halts before the first signed view");
    assert_eq!(restarted.state().block_height(), 1);
    assert_eq!(restarted.state().tip_hash(), local_hash);
    assert!(!restarted.metrics.health().consensus_alive);
}

#[test]
fn historical_contradiction_is_a_conflict_but_a_matching_hash_is_not() {
    let (_dirs, local, remote) = diverged_validators();
    // Give the local node a second block so the remote height-1 certificate is
    // below its final tip and the classifier returns `Historical`.
    local.state().submit_transaction(signed_transfer(&signing_key(7), 1, 20)).unwrap();
    produce_for_test(&local).unwrap();
    assert_eq!(local.state().block_height(), 2);

    let remote_block = remote.state().block_at_height(1).unwrap();
    let remote_certificate = certified_commit_at(&remote, 1);
    assert_ne!(
        local.state().block_at_height(1).unwrap().hash(),
        remote_block.hash(),
        "the two authentic certificates must disagree at height 1"
    );

    // The local ledger holds a different block at height 1: a conflict below the tip.
    let mut candidates = CandidateBuffer::default();
    let mut driver = local.consensus().unwrap().lock().unwrap();
    let mut commit = node_commit(&local, &mut candidates);
    handle_consensus_message_inner(local.state(), &mut driver, remote_certificate, &mut commit)
        .unwrap();
    assert!(driver.halted().is_some(), "a historical mismatch is contradictory evidence");
    assert_eq!(local.state().block_height(), 2, "the append-only ledger keeps its tip");
    drop(driver);

    // Re-announcing the local node's own certificate at a lower height matches
    // the ledger and must never halt.
    let (_dirs2, nodes) = validator_nodes(1);
    let node = &nodes[0];
    for nonce in 0..2 {
        node.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 10)).unwrap();
        produce_for_test(node).unwrap();
    }
    let own = certified_commit_at(node, 1);
    let mut candidates = CandidateBuffer::default();
    let mut driver = node.consensus().unwrap().lock().unwrap();
    let mut commit = node_commit(node, &mut candidates);
    handle_consensus_message_inner(node.state(), &mut driver, own, &mut commit).unwrap();
    assert!(driver.halted().is_none(), "matching historical evidence is a duplicate");
    assert_eq!(node.fork_log.as_ref().unwrap().lock().unwrap().len(), 0);
}

#[test]
fn junk_or_duplicate_evidence_never_halts_signing() {
    let (_dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    node.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce_for_test(node).unwrap();

    let own_tip = {
        let driver = node.consensus().unwrap().lock().unwrap();
        certified_tip_message(node.state(), &driver).unwrap()
    };
    // Its own certificate again: a duplicate is not a fork.
    deliver_for_test(node, own_tip).unwrap();
    assert!(node.consensus().unwrap().lock().unwrap().halted().is_none());
    assert_eq!(node.fork_log.as_ref().unwrap().lock().unwrap().len(), 0);

    // An unauthenticated certificate is a plain rejection, never an incident.
    let forged = {
        let driver = node.consensus().unwrap().lock().unwrap();
        let mut message = certified_tip_message(node.state(), &driver).unwrap();
        if let NetworkMessage::BlockCommit { ref mut signatures, .. } = message {
            for (_, signature) in signatures.iter_mut() {
                *signature = vec![0xff; 96];
            }
        }
        message
    };
    assert!(deliver_for_test(node, forged).is_err());
    assert!(node.consensus().unwrap().lock().unwrap().halted().is_none());
    assert_eq!(node.fork_log.as_ref().unwrap().lock().unwrap().len(), 0);
}

#[test]
fn a_historical_certificate_at_a_pruned_height_is_unverifiable_not_a_fork() {
    // R-57-01: pruning drops the block at a height but not the finality it
    // proved, so a canonical certificate replayed there can be neither confirmed
    // nor refuted. Only a *retained* differing hash may halt a node; treating a
    // pruned height as a fork lets anyone replay old canonical history until a
    // pruning validator stops signing.
    //
    // A real pruned-ledger fixture would need a >64 MiB segmented history and the
    // store exposes no prune seam to the node, so this pins the exact decision
    // `route_final_evidence` applies and the router's pruning guard.
    let local = Hash::new([7; 32]);
    let incoming = Hash::new([8; 32]);

    // At or below the retained base there is no block to compare.
    assert_eq!(historical_meaning(100, 100, None, local), HistoricalMeaning::Unverifiable);
    assert_eq!(historical_meaning(100, 50, None, local), HistoricalMeaning::Unverifiable);
    assert_eq!(historical_meaning(0, 0, None, local), HistoricalMeaning::Unverifiable);

    // A retained height still compares the hashes exactly.
    assert_eq!(historical_meaning(0, 5, Some(local), local), HistoricalMeaning::Duplicate);
    assert_eq!(historical_meaning(0, 5, Some(local), incoming), HistoricalMeaning::Conflict);

    // Above the retained base without a block (defensive): never a fork either.
    assert_eq!(historical_meaning(100, 150, None, local), HistoricalMeaning::Unverifiable);
}

#[test]
fn an_archive_that_does_not_extend_the_restored_ledger_is_refused_at_boot() {
    let (_dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    for nonce in 0..2 {
        node.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 10)).unwrap();
        produce_for_test(node).unwrap();
    }
    let block1 = node.state().block_at_height(1).unwrap();
    let block2 = node.state().block_at_height(2).unwrap();
    let encoded = encode_block(&block2);
    let path = Path::new(certified_history::HISTORY_FILE);

    // Nothing archived, an archive level with the ledger, and the normal crash
    // state (one archived child of the restored tip) are all coherent.
    check_archive_matches_ledger(path, None, 2, block2.hash()).unwrap();
    check_archive_matches_ledger(path, Some((2, &encoded)), 2, block2.hash()).unwrap();
    check_archive_matches_ledger(path, Some((2, &encoded)), 1, block1.hash()).unwrap();

    // A larger lead, undecodable bytes, or a non-child at one lead means the
    // archive and ledger came from different points.
    assert!(
        check_archive_matches_ledger(path, Some((3, &encoded)), 1, block1.hash()).is_err(),
        "two items of lead is a mixed restore"
    );
    assert!(
        check_archive_matches_ledger(path, Some((2, b"not a block")), 1, block1.hash()).is_err(),
        "a one-item lead must still be a real child of the restored tip"
    );
    assert!(
        check_archive_matches_ledger(path, Some((2, &encoded)), 1, Hash::ZERO).is_err(),
        "the archived child must extend this ledger, not an arbitrary parent"
    );
}

#[test]
fn booting_with_an_archive_ahead_of_the_ledger_is_refused() {
    let (_dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    node.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce_for_test(node).unwrap();
    {
        let history = node.certified_history.as_ref().unwrap();
        let mut history = history.lock().unwrap();
        // Two heights beyond the committed ledger cannot be a crash retry.
        history.append(2, b"block", b"qc").unwrap();
        history.append(3, b"block", b"qc").unwrap();
    }
    let config = node.config().clone();
    drop(nodes);

    let Err(error) = Node::bootstrap(config) else {
        panic!("a partial restore must be refused at boot")
    };
    assert!(
        matches!(
            error,
            NodeError::CertifiedHistory { ref detail, .. } if detail.contains("stopped node")
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn a_forged_far_height_commit_never_starts_catch_up() {
    // Sari's PR #57 review: the range target may only come from an authenticated
    // certificate. A forged far-ahead commit must be a plain error with no
    // buffered candidate, no catch-up state and no incident.
    let (dirs, nodes) = validator_nodes(1);
    let producer = &nodes[0];
    for nonce in 0..3 {
        producer.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 10)).unwrap();
        produce_for_test(producer).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    for file in [GENESIS_FILE_NAME, PRODUCER_KEY_FILE_NAME] {
        std::fs::copy(dirs[0].path().join(file), dir.path().join(file)).unwrap();
    }
    let lagging =
        Node::bootstrap(NodeConfig { validator: true, ..test_config(dir.path()) }).unwrap();

    let mut forged = certified_commit_at(producer, 3);
    let NetworkMessage::BlockCommit { ref mut signatures, .. } = forged else {
        panic!("expected a commit")
    };
    for (_, signature) in signatures.iter_mut() {
        *signature = vec![0xff; 96];
    }

    let mut candidates = CandidateBuffer::default();
    let mut commit = node_commit(&lagging, &mut candidates);
    let mut driver = lagging.consensus().unwrap().lock().unwrap();
    assert!(
        handle_consensus_message_inner(lagging.state(), &mut driver, forged, &mut commit).is_err(),
        "a forged certificate is rejected"
    );
    assert!(commit.candidates.highest().is_none(), "unauthenticated evidence is never buffered");
    assert_eq!(driver.height(), 0, "no state changes on rejection");
    assert!(driver.halted().is_none(), "junk never halts");
    assert_eq!(
        lagging.fork_log.as_ref().unwrap().lock().unwrap().len(),
        0,
        "junk records no incident"
    );
    assert!(!lagging.catching_up.load(Ordering::SeqCst), "an announcement cannot start catch-up");
}

#[test]
fn a_buffered_candidate_applies_once_its_ancestors_backfill() {
    let (dirs, nodes) = validator_nodes(1);
    let producer = &nodes[0];
    for nonce in 0..3 {
        producer.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 10)).unwrap();
        produce_for_test(producer).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    for file in [GENESIS_FILE_NAME, PRODUCER_KEY_FILE_NAME] {
        std::fs::copy(dirs[0].path().join(file), dir.path().join(file)).unwrap();
    }
    let lagging =
        Node::bootstrap(NodeConfig { validator: true, ..test_config(dir.path()) }).unwrap();

    let tip_three = certified_commit_at(producer, 3);

    let mut candidates = CandidateBuffer::default();
    let mut commit = node_commit(&lagging, &mut candidates);
    let mut driver = lagging.consensus().unwrap().lock().unwrap();
    handle_consensus_message_inner(lagging.state(), &mut driver, tip_three, &mut commit).unwrap();
    assert_eq!(driver.height(), 0, "a candidate past a gap is not applied");
    assert_eq!(commit.candidates.highest(), Some(3));

    // Backfill only heights 1-2: the buffered height 3 applies from the store.
    let history = producer.certified_history.as_ref().unwrap();
    let NetworkMessage::Blocks { items } =
        serve_certified_range(producer.state(), history, 1, 2).unwrap().unwrap()
    else {
        panic!("expected a range")
    };
    assert_eq!(
        apply_certified_range(lagging.state(), &mut driver, &mut commit, &items).unwrap(),
        2
    );
    assert_eq!(driver.height(), 2, "the range alone stops before the buffered item");
    assert_eq!(drain_candidates(lagging.state(), &mut driver, &mut commit).unwrap(), 1);
    assert_eq!(driver.height(), 3);
    assert_eq!(lagging.state().tip_hash(), producer.state().tip_hash());
    assert!(commit.candidates.highest().is_none(), "the store is drained");

    // The journal and archive advanced with the drain, so a restart resumes at 3.
    let config = lagging.config().clone();
    drop(driver);
    drop(lagging);
    let reopened = Node::bootstrap(config).unwrap();
    assert_eq!(reopened.state().block_height(), 3);
    assert_eq!(reopened.consensus().unwrap().lock().unwrap().height(), 3);
}

#[test]
fn a_stalled_catch_up_releases_signing_after_its_budget() {
    let catching_up = AtomicBool::new(true);
    let mut sync = SyncState::default();
    sync.begin(50);
    assert_eq!(sync.target, Some(50));
    assert!(!sync.expire(&catching_up), "a fresh run is allowed to make progress");

    // Contiguous progress keeps the run alive indefinitely.
    sync.progressed_at =
        Some(std::time::Instant::now() - CATCH_UP_STALL - std::time::Duration::from_secs(1));
    sync.progressed();
    assert!(!sync.expire(&catching_up));

    // No progress within the budget drops the unserved range and resumes signing.
    sync.progressed_at =
        Some(std::time::Instant::now() - CATCH_UP_STALL - std::time::Duration::from_secs(1));
    assert!(sync.expire(&catching_up));
    assert!(!catching_up.load(Ordering::SeqCst));
    assert!(sync.target.is_none());
    assert!(sync.request(0).is_none(), "an expired run must not keep requesting");
    assert!(!sync.expire(&catching_up), "expiry is idempotent");
}

#[test]
fn sync_requests_and_served_windows_are_bounded() {
    let mut sync = SyncState {
        target: Some(1_000),
        requested_at: None,
        progressed_at: Some(std::time::Instant::now()),
    };
    let NetworkMessage::GetBlocks { from, to } = sync.request(10).unwrap() else {
        panic!("a gap request must be a GetBlocks message");
    };
    assert_eq!(from, 11);
    assert_eq!(to, 10 + subhost_network::MAX_SYNC_ITEMS as u64);
    assert!(sync.request(1_000).is_none(), "no request once the tip is reached");
    assert!(sync.retry(1_000).is_none());

    let (_dirs, nodes) = validator_nodes(1);
    let node = &nodes[0];
    node.state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce_for_test(node).unwrap();
    let history = node.certified_history.as_ref().unwrap();
    assert!(serve_certified_range(node.state(), history, 2, 9).unwrap().is_none());
    assert!(serve_certified_range(node.state(), history, 1, 9).unwrap().is_some());
}

#[test]
fn invalid_or_unseen_consensus_traffic_cannot_trigger_a_vote() {
    let (_dirs, nodes) = validator_nodes(2);
    nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let round = produce_for_test(&nodes[0]).unwrap().unwrap();
    let follower = &nodes[1];
    let mut driver = follower.consensus().unwrap().lock().unwrap();
    let before = driver.checkpoint();
    assert!(handle_consensus_message(
        follower.state(),
        &mut driver,
        vote_message(round.vote.as_ref().unwrap())
    )
    .is_err());
    let mut tampered = round.proposal.clone();
    tampered.block.transactions[0].value += 1;
    assert!(handle_consensus_message(follower.state(), &mut driver, proposal_for_test(&tampered))
        .is_err());
    assert!(handle_consensus_message(
        follower.state(),
        &mut driver,
        NetworkMessage::BlockProposal { view: 1, data: encode_block(&round.proposal.block) }
    )
    .is_err());
    assert_eq!(driver.view(), 1);
    assert_eq!(driver.checkpoint(), before);
    assert!(driver.open_proposal().is_none());
    let mut malformed = round.vote.as_ref().unwrap().clone();
    malformed.signature = vec![0xff; 96];
    handle_consensus_message(follower.state(), &mut driver, proposal_for_test(&round.proposal))
        .unwrap();
    assert!(
        handle_consensus_message(follower.state(), &mut driver, vote_message(&malformed)).is_err()
    );
    handle_consensus_message(
        follower.state(),
        &mut driver,
        vote_message(round.vote.as_ref().unwrap()),
    )
    .unwrap();
    let accepted = driver.checkpoint();
    // Only exact authenticated retransmissions may bypass signature verification.
    assert!(
        handle_consensus_message(follower.state(), &mut driver, vote_message(&malformed)).is_err()
    );
    assert_eq!(driver.checkpoint(), accepted);
    assert_eq!(follower.state().block_height(), 0);
    assert!(driver.pending_qc().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_validators_commit_over_real_loopback_gossip() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let (_dirs, nodes) = validator_nodes(2);
    let (first_port, second_port) = two_free_loopback_ports();
    let first_task = tokio::spawn(run_network_with_driver(
        nodes[0].state().clone(),
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{first_port}"),
            enable_mdns: false,
            ..Default::default()
        },
        nodes[0].consensus().unwrap().clone(),
    ));
    wait_until_listening(first_port).await;
    let second_task = tokio::spawn(run_network_with_driver(
        nodes[1].state().clone(),
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{second_port}"),
            enable_mdns: false,
            bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{first_port}")],
            ..Default::default()
        },
        nodes[1].consensus().unwrap().clone(),
    ));
    let hash =
        nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while nodes.iter().any(|node| node.state().block_height() != 1) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    first_task.abort();
    second_task.abort();
    assert!(result.is_ok(), "both validators must commit after gossip quorum");
    assert_eq!(nodes[0].state().tip_hash(), nodes[1].state().tip_hash());
    assert_eq!(nodes[0].state().state_root(), nodes[1].state().state_root());
    for node in &nodes {
        assert!(node.state().receipt(&hash).is_some());
    }
}

#[test]
fn validator_mode_without_a_producer_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert!(matches!(Node::bootstrap(config), Err(NodeError::ProducerKey { .. })));
}

#[cfg(unix)]
#[test]
fn a_producer_key_that_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let key_path = write_test_producer_key(dir.path(), &secret);
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    match Node::bootstrap(config) {
        Err(NodeError::InsecureProducerKey { path, mode }) => {
            assert_eq!(path, key_path);
            assert_eq!(mode, 0o644);
        }
        Err(other) => panic!("expected InsecureProducerKey, got {other:?}"),
        Ok(_) => panic!("a world-readable producer key must not boot a validator"),
    }
}

#[test]
fn a_producer_key_outside_the_genesis_set_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let (stranger, _) = BLSScheme::keygen();
    write_test_producer_key(dir.path(), &stranger);

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert!(matches!(Node::bootstrap(config), Err(NodeError::UnknownProducer(_))));
}

#[test]
fn a_malformed_producer_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
    std::fs::write(&path, "not-a-scalar").unwrap();
    set_mode_0600(&path);

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert!(matches!(Node::bootstrap(config), Err(NodeError::InvalidProducerKey(_))));
}

#[test]
fn the_producer_key_writer_round_trips_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
    let (secret, _) = BLSScheme::keygen();
    write_producer_key(&path, &secret).unwrap();

    let loaded = read_producer_key(&path).unwrap();
    assert_eq!(BLSScheme::encode_secret_key(&loaded), BLSScheme::encode_secret_key(&secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh producer key must be owner-only");
    }

    // `subhost init` calls this on every run: an existing key is the
    // validator identity, so replacing it must fail instead.
    let before = std::fs::read_to_string(&path).unwrap();
    let (other, _) = BLSScheme::keygen();
    assert!(write_producer_key(&path, &other).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn a_non_validator_node_with_a_validator_set_verifies_peer_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    // A follower: no producer key, so it has no signing identity but it still
    // knows who may produce.
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    let registry = ValidatorRegistry::from_genesis(&genesis.initial_validators).unwrap();
    let address = genesis.initial_validators[0].address;
    let producer = RpcState::in_memory(1)
        .unwrap()
        .with_authority(BlockAuthority::new(registry, address, secret.clone()).unwrap());

    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    producer.seed_accounts([(sender, 1_000_000)]).unwrap();
    producer.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
    let block = producer.build_block().unwrap().unwrap();
    node.state().seed_accounts([(sender, 1_000_000)]).unwrap();

    let mut unsigned = block.clone();
    unsigned.signatures.clear();
    assert!(matches!(
        node.state().accept_block(unsigned),
        Err(subhost_rpc::RpcError::UnauthorizedProducer)
    ));
    assert_eq!(node.state().block_height(), 0);

    let qc = QuorumCertificate {
        protocol_context: node.state().consensus_context(),
        view: 1,
        block_hash: block.hash(),
        signatures: vec![(
            address,
            BLSScheme::encode_signature(&BLSScheme::sign(
                &secret,
                &QuorumCertificate::payload_for_context(
                    node.state().consensus_context(),
                    1,
                    &block.hash(),
                ),
            )),
        )],
    };
    node.state().commit_qc(block, &qc).unwrap();
    assert_eq!(node.state().block_height(), 1);
    assert_eq!(node.state().balance(&Address::new([2; 20])), 10);
}

#[test]
fn a_zero_fallback_chain_id_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Node::bootstrap(NodeConfig { fallback_chain_id: 0, ..test_config(dir.path()) }),
        Err(NodeError::InvalidChainId)
    ));
}

#[test]
fn a_ledger_from_another_chain_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    genesis_with(7, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    node.state().seed_accounts([(Address::new([1; 20]), 1)]).unwrap();
    drop(node);

    // Rewrite genesis to a different chain over the same ledger.
    genesis_with(8, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    assert!(matches!(Node::bootstrap(test_config(dir.path())), Err(NodeError::Storage(_))));
}

#[test]
fn metrics_start_in_step_with_the_restored_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    let rendered = String::from_utf8(node.metrics().encode().unwrap()).unwrap();
    assert!(rendered.contains("subhost_block_height 0"));
    assert!(rendered.contains("subhost_pending_transactions 0"));
}

#[test]
fn config_exposes_the_genesis_path_under_the_data_dir() {
    let config = test_config("/tmp/subhost-node-test");
    assert_eq!(
        config.genesis_path(),
        PathBuf::from("/tmp/subhost-node-test").join(GENESIS_FILE_NAME)
    );
    assert!(config.metrics_addr.is_none());
    assert!(!config.validator);
}

pub(super) fn produce_for_test(node: &Node) -> Result<Option<ProducedRound>, NodeError> {
    produce_once_inner(
        node.state(),
        &mut node.consensus().unwrap().lock().unwrap(),
        node.consensus_journal.as_deref(),
        node.certified_history.as_deref(),
        node.retention.as_deref(),
    )
}

pub(super) fn deliver_for_test(
    node: &Node,
    message: NetworkMessage,
) -> Result<Option<Vote>, NodeError> {
    let mut candidates = CandidateBuffer::default();
    let mut commit = node_commit(node, &mut candidates);
    handle_consensus_message_inner(
        node.state(),
        &mut node.consensus().unwrap().lock().unwrap(),
        message,
        &mut commit,
    )
}

/// The commit context a running pump would build for this node.
pub(super) fn node_commit<'a>(
    node: &'a Node,
    candidates: &'a mut CandidateBuffer,
) -> CommitContext<'a> {
    CommitContext {
        journal: node.consensus_journal.as_deref(),
        history: node.certified_history.as_deref(),
        fork_log: node.fork_log.as_deref(),
        candidates,
        metrics: Some(&node.metrics),
        runtime: Some(&node.runtime),
        retention: node.retention.as_deref(),
    }
}

/// The `BlockCommit` a peer sends for a height this node archived.
pub(super) fn certified_commit_at(node: &Node, height: BlockHeight) -> NetworkMessage {
    let history = node.certified_history.as_ref().unwrap();
    let served = serve_certified_range(node.state(), history, height, height).unwrap().unwrap();
    let NetworkMessage::Blocks { items } = served else { panic!("expected a range") };
    let (block_bytes, qc_bytes) = certified_history::decode_item(&items[0]).unwrap();
    let qc: QuorumCertificate = decode_consensus(qc_bytes).unwrap();
    NetworkMessage::BlockCommit {
        view: qc.view,
        data: block_bytes.to_vec(),
        signatures: qc
            .signatures
            .iter()
            .map(|(address, signature)| (*address.as_bytes(), signature.clone()))
            .collect(),
    }
}

pub(super) fn proposal_for_test(proposal: &DriverProposal) -> NetworkMessage {
    NetworkMessage::ConsensusProposal { data: encode_canonical(proposal) }
}

pub(super) fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(super) fn signed_transfer(key: &SigningKey, nonce: u64, value: u128) -> Transaction {
    let public_key = key.verifying_key().to_bytes();
    let mut tx = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
        to: Some(Address::new([2; 20])),
        value,
        gas_price: 1,
        gas_limit: 21_000,
        data: Vec::new(),
        chain_id: 1,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    tx.signature = TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
    tx
}

/// A producer with two committed blocks, plus a follower at genesis.
fn chain_with_two_blocks() -> (RpcState, RpcState, Address) {
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let producer = RpcState::in_memory(1).unwrap();
    producer.seed_accounts([(sender, 1_000_000)]).unwrap();
    producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
    producer.submit_transaction(signed_transfer(&key, 1, 10)).unwrap();

    let follower = RpcState::in_memory(1).unwrap();
    follower.seed_accounts([(sender, 1_000_000)]).unwrap();
    (producer, follower, sender)
}

#[test]
fn out_of_order_peer_blocks_are_applied_in_height_order() {
    let (producer, follower, sender) = chain_with_two_blocks();
    let first = producer.block_at_height(1).unwrap();
    let second = producer.block_at_height(2).unwrap();

    let mut buffer = BlockBuffer::default();
    buffer.offer(second, 1);
    buffer.offer(first, 1);
    assert_eq!(buffer.len(), 2);
    apply_buffered(&follower, &mut buffer);

    assert_eq!(follower.block_height(), 2);
    // Two transfers pay both the moved value and 21_000 gas each.
    assert_eq!(follower.balance(&sender), 1_000_000 - 20 - 42_000);
    assert_eq!(follower.balance(&Address::new([2; 20])), 20);
    assert_eq!(buffer.len(), 0, "both buffered blocks were consumed");

    // Replaying committed history is discarded, not re-applied.
    buffer.offer(producer.block_at_height(1).unwrap(), follower.block_height() + 1);
    assert_eq!(buffer.len(), 0);
}

#[test]
fn the_buffer_refuses_stale_and_far_future_blocks() {
    let (producer, _, _) = chain_with_two_blocks();
    let first = producer.block_at_height(1).unwrap();
    let mut buffer = BlockBuffer::default();

    // Below the expected height.
    buffer.offer(first.clone(), 5);
    assert_eq!(buffer.len(), 0);

    // Further ahead than the node is willing to hold.
    let mut future = first.clone();
    future.header.height = 1 + MAX_BLOCKS_AHEAD + 1;
    buffer.offer(future, 1);
    assert_eq!(buffer.len(), 0);

    // At the horizon is allowed, one past it is not.
    let mut horizon = first.clone();
    horizon.header.height = 1 + MAX_BLOCKS_AHEAD;
    buffer.offer(horizon.clone(), 1);
    assert_eq!(buffer.len(), 1);

    // A duplicate cannot replace what is already buffered.
    buffer.offer(horizon, 1);
    assert_eq!(buffer.len(), 1);
}

#[test]
fn apply_buffered_drops_a_block_that_fails_validation() {
    let (producer, follower, sender) = chain_with_two_blocks();
    let mut tampered = producer.block_at_height(1).unwrap();
    tampered.transactions[0].value += 1;

    let mut buffer = BlockBuffer::default();
    buffer.offer(tampered, 1);
    apply_buffered(&follower, &mut buffer);

    assert_eq!(follower.block_height(), 0, "a forged block must not commit");
    assert_eq!(follower.balance(&sender), 1_000_000);
    assert_eq!(buffer.len(), 0);
}

#[test]
fn blocks_survive_the_gossip_encoding() {
    let (producer, _, _) = chain_with_two_blocks();
    let block = producer.block_at_height(1).unwrap();
    let encoded = encode_block(&block);
    assert_eq!(decode_block(&encoded).unwrap(), block);

    // Truncated input is an error, never a partial block.
    assert!(decode_block(&encoded[..encoded.len() / 2]).is_err());
    assert!(decode_block(b"not a block").is_err());
}

#[test]
fn networking_is_off_unless_configured() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    assert!(node.config().network.is_none());

    let config = NodeConfig {
        network: Some(NetworkConfig { enable_mdns: false, ..Default::default() }),
        ..test_config(dir.path())
    };
    let node = Node::bootstrap(config).unwrap();
    assert!(node.config().network.is_some());
}

/// Two distinct free loopback ports, both bound until chosen.
///
/// Binding both listeners at once guarantees the two ports differ; asking for
/// one port twice can return the same number and make one node fail to bind.
fn two_free_loopback_ports() -> (u16, u16) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (first.local_addr().unwrap().port(), second.local_addr().unwrap().port())
}

/// Wait until a libp2p TCP listener accepts connections on `port`.
///
/// A follower dials its bootstrap peer once, at startup. On a loaded CI runner
/// that dial can happen before the producer has bound its listener, and because
/// there is no redial the mesh never forms. Waiting for the port first removes
/// that ordering dependency.
pub(super) async fn wait_until_listening(port: u16) {
    for _ in 0..250 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no listener on 127.0.0.1:{port} after 5s");
}

/// Two real nodes, real libp2p over loopback TCP, real gossip.
///
/// The follower runs the production [`run_network`] task, so a pass covers
/// the whole path: swarm delivery, `decode_block`, the out-of-order buffer,
/// `apply_buffered`, and `RpcState::accept_block` with its full
/// re-validation. Blocks are re-published until the follower catches up,
/// which makes the test independent of when the gossip mesh forms; duplicate
/// heights are dropped by the follower rather than applied twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_exchange_blocks_over_real_gossip() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let (producer, follower, sender) = chain_with_two_blocks();
    let (producer_port, follower_port) = two_free_loopback_ports();

    let (manager, handle) = NetworkManager::new(NetworkConfig {
        listen_addr: format!("/ip4/127.0.0.1/tcp/{producer_port}"),
        enable_mdns: false,
        ..NetworkConfig::default()
    })
    .unwrap();
    let producer_task = tokio::spawn(manager.run());
    wait_until_listening(producer_port).await;

    let follower_task = tokio::spawn(run_network(
        follower.clone(),
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{follower_port}"),
            bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{producer_port}")],
            enable_mdns: false,
            ..NetworkConfig::default()
        },
    ));

    let first = producer.block_at_height(1).unwrap();
    let second = producer.block_at_height(2).unwrap();

    let mut caught_up = false;
    for _ in 0..100 {
        // Newest first: the follower must buffer height 2 until height 1
        // arrives, then apply both.
        for block in [&second, &first] {
            let message = NetworkMessage::BlockProposal { view: 0, data: encode_block(block) };
            if handle.outbound.send(message).await.is_err() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if follower.block_height() == 2 {
            caught_up = true;
            break;
        }
    }

    assert!(
        caught_up,
        "the follower never applied both blocks; it is at height {}",
        follower.block_height()
    );
    assert_eq!(follower.block_at_height(2).unwrap().hash(), second.hash());
    assert_eq!(follower.state_root(), producer.state_root());
    assert_eq!(follower.balance(&sender), producer.balance(&sender));
    for block in [&first, &second] {
        for tx in &block.transactions {
            assert!(
                follower.receipt(&tx.hash()).is_some(),
                "the follower did not derive a receipt for {}",
                tx.hash()
            );
        }
    }

    follower_task.abort();
    producer_task.abort();
}

fn fee_policy() -> FeePolicy {
    FeePolicy {
        initial_base_fee_per_gas: 8,
        min_base_fee_per_gas: 1,
        max_base_fee_per_gas: u128::MAX / u128::from(BLOCK_GAS_LIMIT),
        elasticity_multiplier: FEE_ELASTICITY_MULTIPLIER,
        change_denominator: FEE_CHANGE_DENOMINATOR,
    }
}

/// A transfer re-signed with an explicit bid, so it can meet the base fee.
fn priced_transfer(key: &SigningKey, nonce: u64, value: u128, gas_price: u128) -> Transaction {
    let mut tx = signed_transfer(key, nonce, value);
    tx.gas_price = gas_price;
    tx.signature = TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
    tx
}

#[test]
fn fee_enabled_genesis_without_allocations_persists_the_anchor_and_seals_v2() {
    let dir = tempfile::tempdir().unwrap();
    let mut genesis = genesis_with(1, None);
    genesis.fee_policy = Some(fee_policy());
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(node.state().block_height(), 0);
    assert!(node.state().fee_policy().is_some(), "the node binds the genesis policy");

    // An idle fee-enabled chain still builds an empty v2 block at the initial
    // fee, and accepting it lowers the fee for the next block.
    let empty = node.state().build_block().unwrap().expect("a v2 chain seals empty blocks");
    assert_eq!(empty.header.version, FEE_HEADER_VERSION);
    assert_eq!(empty.header.base_fee_per_gas, 8);
    assert!(empty.transactions.is_empty());
    node.state().accept_block(empty).unwrap();
    assert_eq!(node.state().block_height(), 1);
    assert_eq!(node.state().build_block().unwrap().unwrap().header.base_fee_per_gas, 7);

    // Restart keeps the policy and derives the same next fee from the tip.
    let restarted = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(restarted.state().block_height(), 1);
    assert!(restarted.state().fee_policy().is_some());
    assert_eq!(restarted.state().build_block().unwrap().unwrap().header.base_fee_per_gas, 7);
}

#[test]
fn fee_enabled_demo_chain_seals_a_priced_transfer_and_burns_the_tip() {
    let dir = tempfile::tempdir().unwrap();
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.fee_policy = Some(fee_policy());
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    // A bid below the initial base fee is refused at admission, before any seal.
    assert!(node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).is_err());
    assert_eq!(node.state().block_height(), 0);

    node.state().submit_transaction(priced_transfer(&tx_key, 0, 10, 10)).unwrap();
    assert_eq!(node.state().block_height(), 1, "demo chains seal synchronously");
    assert_eq!(node.state().balance(&sender), 1_000_000 - 10 - 10 * 21_000);
    assert_eq!(node.state().balance(&Address::ZERO), 0, "validatorless mode burns the tip");

    // The durable ledger restores the same balances under the same policy.
    let restarted = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(restarted.state().block_height(), 1);
    assert_eq!(restarted.state().balance(&sender), 1_000_000 - 10 - 10 * 21_000);
    assert_eq!(restarted.state().build_block().unwrap().unwrap().header.base_fee_per_gas, 7);
}

#[test]
fn a_fee_policy_refuses_to_extend_a_legacy_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let mut legacy = genesis_with(1, Some((sender, 1_000_000)));
    legacy.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
    assert_eq!(node.state().block_height(), 1);
    drop(node);

    // The same directory with a fee-enabled genesis is refused, not rewritten.
    legacy.fee_policy = Some(fee_policy());
    legacy.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let error = match Node::bootstrap(test_config(dir.path())) {
        Err(error) => error,
        Ok(_) => panic!("bootstrap must refuse this genesis/ledger combination"),
    };
    // Either layer may speak first: the store binds the policy before the RPC
    // state does, so the refusal can come from storage's binding rule or from
    // `RpcState::with_fee_policy`. Both must name the legacy incompatibility.
    let text = error.to_string();
    assert!(text.contains("legacy"), "the refusal must name the legacy ledger, got: {text}");
}

#[test]
fn a_genesis_gas_limit_is_honoured_by_the_block_path_and_the_fee_policy() {
    let dir = tempfile::tempdir().unwrap();
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.gas_limit = 1_000_000;
    genesis.fee_policy = Some(fee_policy());
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(node.state().block_gas_limit(), 1_000_000);
    node.state().submit_transaction(priced_transfer(&tx_key, 0, 10, 10)).unwrap();
    assert_eq!(node.state().block_height(), 1);

    // The next header carries the genesis limit and derives its fee from it:
    // 21_000 of a 500_000 target is below target, so 8 falls to 7.
    let next = node.state().build_block().unwrap().unwrap();
    assert_eq!(next.header.gas_limit, 1_000_000);
    assert_eq!(next.header.base_fee_per_gas, 7);

    let restarted = Node::bootstrap(test_config(dir.path())).unwrap();
    assert_eq!(restarted.state().block_gas_limit(), 1_000_000);
    assert_eq!(restarted.state().block_height(), 1);
}

#[test]
fn a_legacy_genesis_gas_limit_is_honoured_too() {
    let dir = tempfile::tempdir().unwrap();
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.gas_limit = 2_000_000;
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
    assert_eq!(node.state().block_height(), 1);
    let sealed = node.state().build_block().unwrap();
    assert!(sealed.is_none(), "a legacy chain still skips empty selections");
    assert_eq!(node.state().block_gas_limit(), 2_000_000);
}

#[test]
fn a_restored_ledger_refuses_a_different_genesis_gas_limit() {
    let dir = tempfile::tempdir().unwrap();
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    let mut genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let node = Node::bootstrap(test_config(dir.path())).unwrap();
    node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
    assert_eq!(node.state().block_height(), 1);
    drop(node);

    genesis.gas_limit = 2_000_000;
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let error = match Node::bootstrap(test_config(dir.path())) {
        Err(error) => error,
        Ok(_) => panic!("bootstrap must refuse this genesis/ledger combination"),
    };
    assert!(
        matches!(error, NodeError::Rpc(subhost_rpc::RpcError::BlockRejected(ref message))
            if message.contains("gas limit")),
        "the refusal must name the gas limit, got: {error}"
    );
}

#[test]
fn node_config_defaults_to_no_automatic_retention() {
    // Q-036 Q5: the CLI's `--retention-blocks` stacks on this field, so `None`
    // must stay the default and mean exactly today's offline-only prune.
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(test_config(dir.path()).retention_blocks, None);
    let configured = NodeConfig { retention_blocks: Some(50_000), ..test_config(dir.path()) };
    assert_eq!(configured.retention_blocks, Some(50_000));
}

/// Commit `blocks` one-transaction blocks to a durable ledger.
fn chain_with_blocks(dir: &Path, blocks: u64) -> RpcState {
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let state = RpcState::with_data_dir(1, dir).unwrap();
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    for nonce in 0..blocks {
        state.submit_transaction(signed_transfer(&key, nonce, 0)).unwrap();
    }
    state
}

#[test]
fn retention_attempts_only_past_the_bound_and_waits_out_the_backoff() {
    // Q-036: `tip - base > keep` is the trigger, at most one attempt per commit,
    // and an attempt that made no progress defers by `max(1, keep / 8)`.
    let trigger = RetentionTrigger::new(64);
    assert_eq!(trigger.retry_after(), 8);
    // At the bound, not past it: no attempt.
    assert_eq!(trigger.target(64, 0), None);
    // Past it: the target keeps exactly `keep` blocks above the base.
    assert_eq!(trigger.target(65, 0), Some(1));
    assert_eq!(trigger.target(100, 0), Some(36));
    // A later base does not widen the window: the bound is on the retained span.
    assert_eq!(trigger.target(200, 100), Some(136));

    let refusal = Ok(PruneOutcome::RefusedBySnapshot { snapshot_height: 0, requested_height: 36 });
    trigger.record(100, &refusal);
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), 108);
    assert_eq!(trigger.target(107, 0), None, "the backoff defers every commit");
    assert_eq!(trigger.target(108, 0), Some(44), "the window reopens at the bound");

    // A successful prune is progress, so it arms no backoff.
    let pruned =
        Ok(PruneOutcome::Pruned { dropped_segments: 1, retained_segments: 1, base_height: 44 });
    trigger.record(150, &pruned);
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), 108);
    assert_eq!(trigger.target(150, 44), Some(86));

    // The retry window never collapses to zero, even for a tiny bound.
    assert_eq!(RetentionTrigger::new(1).retry_after(), 1);
    assert_eq!(RetentionTrigger::new(8).retry_after(), 1);
    assert_eq!(RetentionTrigger::new(16).retry_after(), 2);
}

#[test]
fn retention_stays_inactive_inside_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let state = chain_with_blocks(dir.path(), 2);
    let trigger = RetentionTrigger::new(2);
    // `tip - base == keep`, not above it, so no attempt is made.
    assert!(trigger.attempt(&state).is_none());
    assert_eq!(state.base_height(), 0);
    assert_eq!(state.block_count(), 2);
}

#[test]
fn retention_refusal_at_the_snapshot_floor_is_a_no_op_and_defers() {
    let dir = tempfile::tempdir().unwrap();
    let state = chain_with_blocks(dir.path(), 5);
    let trigger = RetentionTrigger::new(2);
    let tip = state.head_height();
    let outcome = trigger.attempt(&state).expect("the retained span exceeds the bound");
    assert!(
        matches!(
            outcome,
            Ok(PruneOutcome::RefusedBySnapshot { snapshot_height: 0, requested_height: 3 })
        ),
        "the snapshot anchor is the hard floor: {outcome:?}"
    );
    // A refusal is a normal no-op: memory and the base stay untouched and the
    // commit path is not failed.
    assert_eq!(state.base_height(), 0);
    assert_eq!(state.block_count(), 5);
    // The refusal consumed the n/8 backoff, so the same tip does not retry.
    assert_eq!(trigger.retry_after(), 1);
    assert!(trigger.attempt(&state).is_none());
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), tip + 1);
}

#[test]
fn a_failed_retention_attempt_is_swallowed_and_later_commits_still_work() {
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    // An ephemeral store cannot prune; this is a genuine storage error, not the
    // typed snapshot refusal.
    let state = RpcState::in_memory(1).unwrap();
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    for nonce in 0..4 {
        state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
    }
    let trigger = RetentionTrigger::new(1);
    assert!(trigger.attempt(&state).expect("the retained span exceeds the bound").is_err());
    assert_eq!(state.base_height(), 0);
    assert_eq!(state.block_count(), 4);
    // `after_commit` has no error path, so a prune fault cannot fail a commit.
    trigger.after_commit(&state);
    state.submit_transaction(signed_transfer(&key, 4, 1)).unwrap();
    assert_eq!(state.block_height(), 5);
}

/// Produce and commit one block through the real commit path of a live cluster.
///
/// The leader rotates per view, so the producer is whichever node leads the
/// current one; every other node adopts the proposal and votes.
fn commit_next_block(nodes: &[Node]) {
    let leader = nodes
        .iter()
        .position(|node| node.consensus().unwrap().lock().unwrap().is_leader())
        .expect("a live cluster has a leader for the current view");
    let round = {
        let node = &nodes[leader];
        let nonce = node.state().block_height();
        node.state().submit_transaction(signed_transfer(&signing_key(7), nonce, 1)).unwrap();
        produce_for_test(node).unwrap().expect("the leader must propose")
    };
    let mut prepares = vec![round.vote.clone().expect("the leader must vote")];
    for (index, node) in nodes.iter().enumerate() {
        if index != leader {
            prepares
                .push(deliver_for_test(node, proposal_for_test(&round.proposal)).unwrap().unwrap());
        }
    }
    for node in nodes {
        for vote in &prepares {
            deliver_for_test(node, vote_message(vote)).unwrap();
        }
    }
    let commits: Vec<_> = nodes
        .iter()
        .filter_map(|node| node.consensus().unwrap().lock().unwrap().own_commit_vote().cloned())
        .collect();
    assert!(!commits.is_empty(), "every node must reach the commit phase");
    for node in nodes {
        for vote in &commits {
            deliver_for_test(node, vote_message(vote)).unwrap();
        }
    }
}

#[test]
fn commits_reach_the_retention_trigger_without_ever_failing_the_block() {
    // Q-036: the trigger is node-owned, so this drives a real certified commit
    // and checks that the commit path reaches it. `keep = 1` is inside the bound
    // after one block, so the first commit must not attempt a prune; after the
    // second the retained span is above the bound and the attempt is a typed
    // snapshot refusal, which must leave the block committed.
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let (_dirs, nodes) = validator_nodes_with_retention(4, Some(1));
    let trigger = nodes[0].retention.clone().expect("the configured bound installs the trigger");
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), 0);

    commit_next_block(&nodes);
    assert_eq!(nodes[0].state().block_height(), 1);
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), 0, "inside the bound: no attempt");

    commit_next_block(&nodes);
    assert_eq!(nodes[0].state().block_height(), 2, "retention must not fail the commit");
    assert_eq!(nodes[0].state().base_height(), 0);
    assert_eq!(nodes[0].state().block_count(), 2);
    // One attempt was made and refused at the snapshot anchor (height 0), and
    // `max(1, keep / 8)` armed the next one at tip + 1.
    assert_eq!(trigger.next_attempt.load(Ordering::SeqCst), 3);
}

#[test]
fn bootstrap_rejects_zero_retention_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let config = NodeConfig { retention_blocks: Some(0), ..test_config(dir.path()) };
    let error = match Node::bootstrap(config) {
        Err(error) => error,
        Ok(_) => panic!("zero retention is not a valid bound"),
    };
    assert!(
        matches!(error, NodeError::InvalidConsensusConfiguration(ref message)
            if message.contains("at least 1")),
        "got: {error}"
    );
}

#[test]
fn bootstrap_ignores_a_stray_pending_file_without_an_attached_store() {
    // With persistence opt-in unset, the bootstrap restore must be inert: it
    // may not decode, rewrite or fail on a file it does not own.
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().join(MEMPOOL_FILE_NAME);
    std::fs::write(&pending, b"not a snapshot").unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();

    assert_eq!(node.state().pending_count(), 0);
    assert_eq!(std::fs::read(&pending).unwrap(), b"not a snapshot");
}

#[cfg(unix)]
#[test]
fn a_symlinked_producer_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    // A real owner-only key elsewhere, with the configured path pointing at it.
    let target = dir.path().join("elsewhere.key");
    std::fs::write(&target, hex::encode(BLSScheme::encode_secret_key(&secret))).unwrap();
    set_mode_0600(&target);
    let link = dir.path().join(PRODUCER_KEY_FILE_NAME);
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    match Node::bootstrap(config) {
        Err(NodeError::UnsafeProducerKey { path, reason }) => {
            assert_eq!(path, link);
            assert!(reason.contains("symlink"), "{reason}");
        }
        Err(other) => panic!("expected UnsafeProducerKey, got {other:?}"),
        Ok(_) => panic!("a symlinked producer key must not boot a validator"),
    }
    assert!(matches!(read_producer_key(&link), Err(NodeError::UnsafeProducerKey { .. })));
}

#[test]
fn an_oversized_producer_key_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
    std::fs::write(&path, vec![b'a'; 300]).unwrap();
    set_mode_0600(&path);

    assert!(matches!(read_producer_key(&path), Err(NodeError::UnsafeProducerKey { .. })));
}

#[cfg(unix)]
#[test]
fn a_producer_key_that_is_not_a_regular_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
    std::fs::create_dir(&path).unwrap();

    assert!(matches!(read_producer_key(&path), Err(NodeError::UnsafeProducerKey { .. })));
}

#[test]
fn a_supplied_producer_key_boots_a_validator_without_any_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, address, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    // No producer.key on disk at all: the unlocked key is the only input.
    assert!(!dir.path().join(PRODUCER_KEY_FILE_NAME).exists());

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    let node = Node::bootstrap_with_producer_key(config, secret).unwrap();
    assert_eq!(node.genesis().unwrap().total_voting_power(), 100);

    // A legacy chain proposes only when something is executable, so give it a
    // transaction and check the supplied identity actually signs the proposal.
    let tx_key = signing_key(7);
    let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
    node.state().seed_accounts([(sender, 1_000_000)]).unwrap();
    node.state().submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();

    let driver = node.consensus().expect("a validator node must own a driver");
    let mut driver = driver.lock().unwrap();
    let round = produce_once_inner(
        node.state(),
        &mut driver,
        node.consensus_journal.as_deref(),
        node.certified_history.as_deref(),
        node.retention.as_deref(),
    )
    .unwrap()
    .expect("a validator with a pending transaction must propose");
    assert_eq!(round.proposal.block.header.validator, address);
    assert!(!round.proposal.block.signatures.is_empty(), "the supplied key must sign");
    drop(driver);
}

#[test]
fn a_supplied_producer_key_requires_a_validator_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let (secret, _) = BLSScheme::keygen();
    let mut config = test_config(dir.path());
    config.single_writer = true;
    assert!(matches!(
        Node::bootstrap_with_producer_key(config, secret),
        Err(NodeError::InvalidConsensusConfiguration(_))
    ));
}

#[test]
fn a_supplied_producer_key_outside_the_genesis_set_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, _) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let (stranger, _) = BLSScheme::keygen();

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert!(matches!(
        Node::bootstrap_with_producer_key(config, stranger),
        Err(NodeError::UnknownSuppliedProducer)
    ));
}

#[test]
fn a_supplied_producer_key_never_reads_the_configured_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let (genesis, _, secret) = genesis_with_validator(1);
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    // A garbage file at the configured path: supplying a key must ignore it.
    let path = dir.path().join(PRODUCER_KEY_FILE_NAME);
    std::fs::write(&path, "not-a-scalar").unwrap();
    set_mode_0600(&path);

    let config = NodeConfig { validator: true, single_writer: false, ..test_config(dir.path()) };
    assert!(Node::bootstrap_with_producer_key(config, secret).is_ok());
}

fn persist_snapshot(
    dir: &Path,
    genesis: &GenesisConfig,
    transactions: Vec<Transaction>,
) -> PathBuf {
    let store = MempoolStore::open(genesis.chain_id, genesis.consensus_context(), dir).unwrap();
    let mut pool = Mempool::default();
    for tx in transactions {
        pool.add(tx).unwrap();
    }
    store.persist(&pool).unwrap();
    store.path().to_path_buf()
}

fn persistence_config(dir: &Path) -> NodeConfig {
    NodeConfig { mempool_persist: true, ..test_config(dir) }
}

#[test]
fn node_config_defaults_to_no_mempool_persistence() {
    // The CLI `--mempool-persist` flag stacks on this field: `false` must stay
    // the default so an existing operator keeps today's volatile pool.
    let dir = tempfile::tempdir().unwrap();
    assert!(!test_config(dir.path()).mempool_persist);
    let configured = NodeConfig { mempool_persist: true, ..test_config(dir.path()) };
    assert!(configured.mempool_persist);
}

#[test]
fn mempool_persist_restores_a_snapshot_bound_to_this_chain() {
    let dir = tempfile::tempdir().unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let tx = signed_transfer(&key, 0, 10);
    let path = persist_snapshot(dir.path(), &genesis, vec![tx.clone()]);

    let node = Node::bootstrap(persistence_config(dir.path())).unwrap();

    assert_eq!(node.state().pending_count(), 1);
    // Restore rewrites the filtered snapshot in place; it must still hold the
    // pending transaction under the same chain binding.
    let store = MempoolStore::open(1, genesis.consensus_context(), dir.path()).unwrap();
    let on_disk = store.load().unwrap();
    assert_eq!(on_disk.len(), 1);
    assert_eq!(on_disk[0].hash(), tx.hash());
    assert!(path.exists());
}

#[test]
fn mempool_persist_is_opt_in_even_when_a_valid_snapshot_exists() {
    let dir = tempfile::tempdir().unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    let path = persist_snapshot(dir.path(), &genesis, vec![signed_transfer(&key, 0, 10)]);
    let before = std::fs::read(&path).unwrap();

    let node = Node::bootstrap(test_config(dir.path())).unwrap();

    assert_eq!(node.state().pending_count(), 0);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "disabled persistence must not touch the file"
    );
}

#[test]
fn mempool_persist_does_not_create_the_file_before_the_first_admission() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::bootstrap(persistence_config(dir.path())).unwrap();

    assert_eq!(node.state().pending_count(), 0);
    assert!(
        !dir.path().join(MEMPOOL_FILE_NAME).exists(),
        "attaching a store must not write a snapshot by itself"
    );
    drop(node);
}

#[test]
fn mempool_persist_refuses_a_snapshot_from_another_genesis() {
    let dir = tempfile::tempdir().unwrap();
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let genesis = genesis_with(1, Some((sender, 1_000_000)));
    genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
    // A syntactically valid snapshot bound to a different protocol context.
    let store = MempoolStore::open(1, Hash::from_data(b"another chain"), dir.path()).unwrap();
    let mut pool = Mempool::default();
    pool.add(signed_transfer(&key, 0, 10)).unwrap();
    store.persist(&pool).unwrap();

    let error = match Node::bootstrap(persistence_config(dir.path())) {
        Err(error) => error,
        Ok(_) => panic!("a foreign pending snapshot must not be restored"),
    };

    // Q-038: the opt-in asked for durability, so a corrupt or foreign snapshot
    // stops the boot with the cause and the exact file rather than silently
    // starting empty. The path stays in the boot error, never in RPC errors.
    match error {
        NodeError::PendingSnapshot { path, ref detail } => {
            assert_eq!(path, dir.path().join(MEMPOOL_FILE_NAME));
            assert!(detail.contains("genesis mismatch"), "{detail}");
        }
        other => panic!("expected a pending-snapshot boot error, got: {other}"),
    }
}

#[test]
fn mempool_persist_refuses_a_corrupt_snapshot_instead_of_starting_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(MEMPOOL_FILE_NAME), b"not a snapshot").unwrap();

    let error = match Node::bootstrap(persistence_config(dir.path())) {
        Err(error) => error,
        Ok(_) => panic!("a corrupt snapshot must not be silently discarded"),
    };

    match error {
        NodeError::PendingSnapshot { path, ref detail } => {
            assert_eq!(path, dir.path().join(MEMPOOL_FILE_NAME));
            assert!(detail.contains("invalid snapshot header"), "{detail}");
        }
        other => panic!("expected a pending-snapshot boot error, got: {other}"),
    }
}
