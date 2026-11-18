use super::*;
use ed25519_dalek::{Signer, SigningKey};
use subhost_consensus::driver::VotePhase;
use subhost_core::{Address, Transaction, TransactionSignature, TransactionType, ValidatorInfo};

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
    assert!(produce_once_inner(node.state(), &mut driver, node.consensus_journal.as_deref())
        .unwrap()
        .is_some());
    drop(driver);

    let block = node.state().block_at_height(1).unwrap();
    assert_eq!(block.header.validator, address, "blocks must be signed as the genesis validator");
    assert_eq!(block.signatures.len(), 1);
}

pub(super) fn validator_nodes(count: u8) -> (Vec<tempfile::TempDir>, Vec<Node>) {
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
                ..test_config(dir.path())
            })
            .unwrap()
        })
        .collect();
    (dirs, nodes)
}

#[test]
fn four_validator_nodes_require_three_votes_before_a_ledger_write() {
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
    )
}

pub(super) fn deliver_for_test(
    node: &Node,
    message: NetworkMessage,
) -> Result<Option<Vote>, NodeError> {
    handle_consensus_message_inner(
        node.state(),
        &mut node.consensus().unwrap().lock().unwrap(),
        message,
        node.consensus_journal.as_deref(),
    )
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
