use super::*;
use crate::consensus_journal::JOURNAL_FILE;
use crate::tests::{signed_transfer, signing_key, validator_nodes, wait_until_listening};
use subhost_consensus::driver::VotePhase;

fn produce(node: &Node) -> Result<Option<ProducedRound>, NodeError> {
    produce_once_inner(
        node.state(),
        &mut node.consensus().unwrap().lock().unwrap(),
        node.consensus_journal.as_deref(),
        node.certified_history.as_deref(),
    )
}

fn deliver(node: &Node, message: NetworkMessage) -> Result<Option<Vote>, NodeError> {
    handle_consensus_message_inner(
        node.state(),
        &mut node.consensus().unwrap().lock().unwrap(),
        message,
        node.consensus_journal.as_deref(),
        node.certified_history.as_deref(),
    )
}

fn proposal_message(round: &ProducedRound) -> NetworkMessage {
    NetworkMessage::ConsensusProposal { data: encode_canonical(&round.proposal) }
}

#[test]
fn genesis_binding_is_stable_across_allocation_insertion_orders() {
    let mut genesis = GenesisConfig::default();
    for index in 1..=20u8 {
        genesis.allocations.insert(Address::new([index; 20]), index as u128);
    }
    let mut reordered = genesis.clone();
    reordered.allocations.clear();
    for index in (1..=20u8).rev() {
        reordered.allocations.insert(Address::new([index; 20]), index as u128);
    }
    assert_eq!(genesis.consensus_context(), reordered.consensus_context());
}

#[tokio::test]
async fn the_node_exports_successful_and_failed_rpc_calls_from_its_shared_registry() {
    let directory = tempfile::tempdir().unwrap();
    let listeners: Vec<_> =
        (0..2).map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap()).collect();
    let ports: Vec<_> =
        listeners.iter().map(|listener| listener.local_addr().unwrap().port()).collect();
    drop(listeners);
    let mut config = NodeConfig::new(directory.path());
    config.rpc_addr = ([127, 0, 0, 1], ports[0]).into();
    config.metrics_addr = Some(([127, 0, 0, 1], ports[1]).into());
    config.single_writer = true;
    let task = tokio::spawn(Node::bootstrap(config).unwrap().run());
    wait_until_listening(ports[0]).await;
    wait_until_listening(ports[1]).await;
    let client = reqwest::Client::new();
    for method in ["eth_chainId", "never_registered"] {
        client
            .post(format!("http://127.0.0.1:{}", ports[0]))
            .json(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":method, "params":[],
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
    }
    let metrics = client
        .get(format!("http://127.0.0.1:{}/metrics", ports[1]))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    assert!(metrics.contains("subhost_rpc_requests_total{method=\"eth_chainId\"} 1"));
    assert!(metrics.contains("subhost_rpc_requests_total{method=\"unknown\"} 1"));
    assert!(metrics.contains("subhost_rpc_errors_total{method=\"unknown\"} 1"));
    assert!(
        metrics.contains("subhost_rpc_request_duration_seconds_count{method=\"eth_chainId\"} 1")
    );
    assert!(!metrics.contains("never_registered"));
}

#[test]
fn an_open_vote_survives_restart_and_refuses_a_conflicting_block() {
    let (dirs, nodes) = validator_nodes(2);
    let hash =
        nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let round = produce(&nodes[0]).unwrap().unwrap();
    let vote = round.vote.clone().unwrap();
    let configs: Vec<_> = nodes.iter().map(|node| node.config().clone()).collect();
    drop(nodes);
    let producer = Node::bootstrap(configs[0].clone()).unwrap();
    let follower = Node::bootstrap(configs[1].clone()).unwrap();
    assert_eq!(producer.state().block_height(), 0);
    assert_eq!(producer.state().pending_count(), 0, "pool entries are still volatile");
    assert_eq!(producer.consensus().unwrap().lock().unwrap().own_vote(), Some(&vote));
    assert_eq!(deliver(&producer, proposal_message(&round)).unwrap(), Some(vote.clone()));
    let mut conflicting = round.proposal.block.clone();
    conflicting.header.extra_data = b"different proposal".to_vec();
    let secret = read_producer_key(&dirs[0].path().join(PRODUCER_KEY_FILE_NAME)).unwrap();
    conflicting.signatures[0].signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&secret, &conflicting.signing_payload()));
    let mut conflicting_proposal = round.proposal.clone();
    conflicting_proposal.block = conflicting;
    conflicting_proposal.signature = BLSScheme::encode_signature(&BLSScheme::sign(
        &secret,
        &conflicting_proposal.signing_payload(),
    ));
    assert!(deliver(
        &producer,
        NetworkMessage::ConsensusProposal { data: encode_canonical(&conflicting_proposal) }
    )
    .is_err());
    assert_eq!(producer.consensus().unwrap().lock().unwrap().own_vote(), Some(&vote));
    let other_vote = deliver(&follower, proposal_message(&round)).unwrap().unwrap();
    deliver(&producer, vote_message(&other_vote)).unwrap();
    deliver(&follower, vote_message(&vote)).unwrap();
    assert_eq!(producer.state().block_height(), 0, "prepare quorum is not final");
    assert_eq!(follower.state().block_height(), 0);
    let producer_commit =
        producer.consensus().unwrap().lock().unwrap().own_commit_vote().unwrap().clone();
    let follower_commit =
        follower.consensus().unwrap().lock().unwrap().own_commit_vote().unwrap().clone();
    assert_eq!(producer_commit.phase, VotePhase::Commit);
    assert_eq!(follower_commit.phase, VotePhase::Commit);
    deliver(&producer, vote_message(&follower_commit)).unwrap();
    deliver(&follower, vote_message(&producer_commit)).unwrap();
    assert_eq!(producer.state().block_height(), 1);
    assert_eq!(producer.state().state_root(), follower.state().state_root());
    assert!(producer.state().receipt(&hash).is_some());
}

#[test]
fn journal_write_failure_releases_no_vote_or_driver_mutation() {
    let (dirs, nodes) = validator_nodes(2);
    nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let before = nodes[0].consensus().unwrap().lock().unwrap().checkpoint();
    let journal = dirs[0].path().join(JOURNAL_FILE);
    let saved = dirs[0].path().join("saved-journal");
    std::fs::rename(&journal, &saved).unwrap();
    std::fs::create_dir(&journal).unwrap();
    assert!(produce(&nodes[0]).is_err());
    assert_eq!(nodes[0].consensus().unwrap().lock().unwrap().checkpoint(), before);
    assert_eq!(nodes[0].state().block_height(), 0);
    std::fs::remove_dir(&journal).unwrap();
    std::fs::rename(&saved, &journal).unwrap();
    assert!(produce(&nodes[0]).unwrap().unwrap().vote.is_some());
}

#[test]
fn a_prepared_qc_survives_failed_append_and_commits_on_restart() {
    let (dirs, nodes) = validator_nodes(2);
    let hash =
        nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let round = produce(&nodes[0]).unwrap().unwrap();
    let vote = deliver(&nodes[1], proposal_message(&round)).unwrap().unwrap();
    deliver(&nodes[0], vote_message(&vote)).unwrap();
    deliver(&nodes[1], vote_message(round.vote.as_ref().unwrap())).unwrap();
    assert_eq!(nodes[0].state().block_height(), 0, "prepare-only state must not persist a block");
    let commit = nodes[1].consensus().unwrap().lock().unwrap().own_commit_vote().unwrap().clone();
    assert_eq!(commit.phase, VotePhase::Commit);
    let ledger = dirs[0].path().join(subhost_storage::LEDGER_DIR_NAME);
    let saved = dirs[0].path().join("saved-ledger");
    std::fs::rename(&ledger, &saved).unwrap();
    std::fs::write(&ledger, b"block append").unwrap();
    assert!(deliver(&nodes[0], vote_message(&commit)).is_err());
    assert_eq!(nodes[0].state().block_height(), 0);
    assert!(nodes[0].consensus().unwrap().lock().unwrap().pending_qc().is_some());
    let config = nodes[0].config().clone();
    drop(nodes);
    std::fs::remove_file(&ledger).unwrap();
    std::fs::rename(&saved, &ledger).unwrap();
    let restarted = Node::bootstrap(config).unwrap();
    assert_eq!(restarted.state().block_height(), 1);
    assert_eq!(restarted.state().tip_hash(), round.proposal.block.hash());
    assert!(restarted.state().receipt(&hash).is_some());
    assert!(restarted.consensus().unwrap().lock().unwrap().committed_qc().is_some());
}

#[test]
fn a_crash_after_append_before_journal_completion_never_appends_twice() {
    let (_dirs, nodes) = validator_nodes(2);
    nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    let round = produce(&nodes[0]).unwrap().unwrap();
    let vote = deliver(&nodes[1], proposal_message(&round)).unwrap().unwrap();
    deliver(&nodes[0], vote_message(&vote)).unwrap();
    deliver(&nodes[1], vote_message(round.vote.as_ref().unwrap())).unwrap();
    let commit = nodes[1].consensus().unwrap().lock().unwrap().own_commit_vote().unwrap().clone();
    {
        let mut driver = nodes[0].consensus().unwrap().lock().unwrap();
        driver.record_vote(commit).unwrap();
        let qc = driver.pending_qc().unwrap();
        nodes[0]
            .consensus_journal
            .as_ref()
            .unwrap()
            .prepare_commit(&driver, round.proposal.block.clone(), qc.clone())
            .unwrap();
        nodes[0].state().commit_qc(round.proposal.block.clone(), &qc).unwrap();
        // Intentionally leave both the driver and journal at the prepared state.
    }
    let config = nodes[0].config().clone();
    drop(nodes);
    let restarted = Node::bootstrap(config.clone()).unwrap();
    assert_eq!(restarted.state().block_count(), 1);
    assert_eq!(restarted.consensus().unwrap().lock().unwrap().tip(), restarted.state().tip_hash());
    drop(restarted);
    assert_eq!(Node::bootstrap(config).unwrap().state().block_count(), 1);
}

#[test]
fn restoring_an_older_ledger_cannot_erase_the_latest_signing_high_water_state() {
    let (dirs, nodes) = validator_nodes(1);
    let manifest = dirs[0]
        .path()
        .join(subhost_storage::LEDGER_DIR_NAME)
        .join(subhost_storage::MANIFEST_FILE_NAME);
    let old_manifest = std::fs::read(&manifest).unwrap();
    nodes[0].state().submit_transaction(signed_transfer(&signing_key(7), 0, 10)).unwrap();
    produce(&nodes[0]).unwrap();
    let current_manifest = std::fs::read(&manifest).unwrap();
    let journal_path = dirs[0].path().join(JOURNAL_FILE);
    let latest_journal = std::fs::read(&journal_path).unwrap();
    let config = nodes[0].config().clone();
    drop(nodes);
    std::fs::write(&manifest, old_manifest).unwrap();
    assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::ConsensusJournal { .. })));
    assert_eq!(std::fs::read(&journal_path).unwrap(), latest_journal);
    std::fs::write(&manifest, current_manifest).unwrap();
    assert_eq!(Node::bootstrap(config).unwrap().state().block_height(), 1);
}

#[test]
fn missing_corrupt_incompatible_journals_and_concurrent_validator_processes_are_refused() {
    for damage in ["missing", "checksum", "truncated", "legacy_format", "version", "genesis"] {
        let (dirs, nodes) = validator_nodes(1);
        let config = nodes[0].config().clone();
        assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::ConsensusJournal { .. })));
        drop(nodes);
        let journal = dirs[0].path().join(JOURNAL_FILE);
        match damage {
            "missing" => std::fs::remove_file(&journal).unwrap(),
            "checksum" => {
                let mut bytes = std::fs::read(&journal).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                std::fs::write(&journal, bytes).unwrap();
            }
            "truncated" => std::fs::write(&journal, b"SUBVOTE2").unwrap(),
            "legacy_format" => {
                let mut bytes = std::fs::read(&journal).unwrap();
                bytes[..8].copy_from_slice(b"SUBVOTE1");
                std::fs::write(&journal, bytes).unwrap();
            }
            "version" => {
                let mut bytes = std::fs::read(&journal).unwrap();
                bytes[8..12].copy_from_slice(&3u32.to_le_bytes());
                let end = bytes.len() - 32;
                let checksum = Hash::from_data(&bytes[8..end]);
                bytes[end..].copy_from_slice(checksum.as_bytes());
                std::fs::write(&journal, bytes).unwrap();
            }
            "genesis" => {
                let path = dirs[0].path().join(GENESIS_FILE_NAME);
                let mut genesis = GenesisConfig::load(&path).unwrap();
                genesis.allocations.values_mut().for_each(|balance| *balance += 1);
                genesis.save(&path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(Node::bootstrap(config).is_err(), "{damage} journal must fail closed");
    }
}

fn start_peer(
    node: &Node,
    port: u16,
    hub: Option<u16>,
) -> tokio::task::JoinHandle<Result<(), NodeError>> {
    tokio::spawn(run_network_with_journal(
        node.state().clone(),
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{port}"),
            bootstrap_peers: hub
                .map(|port| vec![format!("/ip4/127.0.0.1/tcp/{port}")])
                .unwrap_or_default(),
            enable_mdns: false,
            ..Default::default()
        },
        node.consensus().unwrap().clone(),
        node.consensus_journal.clone(),
    ))
}

async fn await_height(nodes: &[Option<Node>], height: u64, count: usize, stage: &str) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while nodes.iter().flatten().take(count).any(|node| node.state().block_height() != height) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    if result.is_err() {
        let status: Vec<_> = nodes.iter().flatten().take(count).enumerate().map(|(index, node)| {
            let consensus = match node.consensus().unwrap().try_lock() {
                Ok(driver) => {
                    let checkpoint = driver.checkpoint();
                    let prepares = checkpoint.votes.iter().filter(|vote| vote.phase == VotePhase::Prepare).count();
                    let commits = checkpoint.votes.iter().filter(|vote| vote.phase == VotePhase::Commit).count();
                    format!("view={} leader={} proposal={:?} prepared={:?} votes={prepares}/{commits} tc={:?}",
                        driver.view(), driver.leader(), driver.open_proposal().map(|proposal| proposal.view),
                        driver.prepared_certificate().map(|prepared| prepared.qc.view),
                        driver.timeout_certificate().map(|certificate| certificate.view))
                }
                Err(error) => format!("consensus unavailable: {error}"),
            };
            format!("node{index}: height={} pending={} root={} {consensus}",
                node.state().block_height(), node.state().pending_count(), node.state().state_root())
        }).collect();
        panic!("{stage}: real gossip must converge to height {height}; {}", status.join("; "));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_validator_testnet_recovers_from_late_bootstrap_and_hub_restart() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let (dirs, nodes) = validator_nodes(4);
    let mut nodes: Vec<_> = nodes.into_iter().map(Some).collect();
    let listeners: Vec<_> =
        (0..4).map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap()).collect();
    let ports: Vec<_> =
        listeners.iter().map(|listener| listener.local_addr().unwrap().port()).collect();
    drop(listeners);
    let mut tasks = Vec::new();
    // The first bootstrap attempt fails before the hub exists.
    tasks.push(Some(start_peer(nodes[1].as_ref().unwrap(), ports[1], Some(ports[0]))));
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let hub_task = start_peer(nodes[0].as_ref().unwrap(), ports[0], None);
    wait_until_listening(ports[0]).await;
    let tx = signed_transfer(&signing_key(7), 0, 10);
    let hash = tx.hash();
    // Transaction gossip is separate S7 work. Seed each local pool so whichever
    // authenticated leader is elected after late bootstrap can propose it.
    for node in nodes.iter().flatten() {
        assert_eq!(node.state().submit_transaction(tx.clone()).unwrap(), hash);
    }
    let manifest = dirs[0]
        .path()
        .join(subhost_storage::LEDGER_DIR_NAME)
        .join(subhost_storage::MANIFEST_FILE_NAME);
    let before = std::fs::read(&manifest).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(nodes[0].as_ref().unwrap().state().block_height(), 0, "two votes cannot commit");
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(nodes[0].as_ref().unwrap().state().receipt(&hash).is_none());
    tasks.push(Some(start_peer(nodes[2].as_ref().unwrap(), ports[2], Some(ports[0]))));
    await_height(&nodes, 1, 3, "first three after late bootstrap").await;
    assert_eq!(nodes[3].as_ref().unwrap().state().block_height(), 0);
    tasks.push(Some(start_peer(nodes[3].as_ref().unwrap(), ports[3], Some(ports[0]))));
    await_height(&nodes, 1, 4, "late fourth certified catch-up").await;
    hub_task.abort();
    let _ = hub_task.await;
    let config = nodes[0].as_ref().unwrap().config().clone();
    drop(nodes[0].take());
    nodes[0] = Some(Node::bootstrap(config).unwrap());
    tasks.push(Some(start_peer(nodes[0].as_ref().unwrap(), ports[0], None)));
    wait_until_listening(ports[0]).await;
    let next = signed_transfer(&signing_key(7), 1, 20);
    for node in nodes.iter().flatten() {
        node.state().submit_transaction(next.clone()).unwrap();
    }
    await_height(&nodes, 2, 4, "all four after hub restart").await;
    let tip = nodes[0].as_ref().unwrap().state().tip_hash();
    let root = nodes[0].as_ref().unwrap().state().state_root();
    for node in nodes.iter().flatten() {
        assert_eq!(node.state().tip_hash(), tip);
        assert_eq!(node.state().state_root(), root);
        assert!(node.state().receipt(&hash).is_some());
    }
    for task in tasks.into_iter().flatten() {
        task.abort();
        let _ = task.await;
    }
}
