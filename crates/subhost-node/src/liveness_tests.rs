//! End-to-end regressions for consensus progress and durable voting obligations.

use super::*;
use crate::tests::{signed_transfer, signing_key, wait_until_listening};
use std::net::TcpListener;
use std::time::Duration;
use subhost_consensus::driver::{TimeoutCertificate, ViewChange, VotePhase};
use subhost_core::{Receipt, Transaction, ValidatorInfo};
use subhost_storage::{LEDGER_DIR_NAME, MANIFEST_FILE_NAME};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

const RECIPIENT: Address = Address::new([2; 20]);

struct Validators {
    directories: Vec<tempfile::TempDir>,
    configs: Vec<NodeConfig>,
    listeners: Vec<Option<TcpListener>>,
    ports: Vec<u16>,
    sender: Address,
}

impl Validators {
    fn four() -> Self {
        let sender = Address::from_public_key(&signing_key(7).verifying_key().to_bytes());
        let keys: Vec<_> = (0..4).map(|_| BLSScheme::keygen().0).collect();
        let mut genesis = GenesisConfig {
            chain_id: 1,
            genesis_time: 1_700_000_000,
            block_time_ms: 100,
            ..Default::default()
        };
        genesis.allocations.insert(sender, 1_000_000);
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
        let directories: Vec<_> = keys
            .iter()
            .map(|secret| {
                let directory = tempfile::tempdir().unwrap();
                genesis.save(&directory.path().join(GENESIS_FILE_NAME)).unwrap();
                write_producer_key(&directory.path().join(PRODUCER_KEY_FILE_NAME), secret).unwrap();
                directory
            })
            .collect();
        let listeners: Vec<_> =
            (0..4).map(|_| Some(TcpListener::bind(("127.0.0.1", 0)).unwrap())).collect();
        let ports: Vec<_> = listeners
            .iter()
            .map(|listener| listener.as_ref().unwrap().local_addr().unwrap().port())
            .collect();
        let configs = directories
            .iter()
            .map(|directory| {
                let mut config = NodeConfig::new(directory.path());
                config.validator = true;
                config.view_timeout_ms = 750;
                // State handles exercise admission; reserve no unnecessary RPC ports.
                config.rpc_addr = ([127, 0, 0, 1], 0).into();
                config
            })
            .collect();
        Self { directories, configs, listeners, ports, sender }
    }

    fn address(&self, index: usize) -> String {
        format!("/ip4/127.0.0.1/tcp/{}", self.ports[index])
    }

    fn bootstrap(&mut self, index: usize, peers: Vec<String>) -> Node {
        let mut config = self.configs[index].clone();
        config.network = Some(P2pConfig {
            listen_addr: self.address(index),
            bootstrap_peers: peers,
            enable_mdns: false,
            ..Default::default()
        });
        Node::bootstrap(config).unwrap()
    }

    fn start(&mut self, index: usize, node: Node) -> Service {
        // Keep the reservation throughout crypto/storage bootstrap, releasing
        // it only when the async service is ready to bind its actual listener.
        drop(self.listeners[index].take());
        Service::start(node)
    }

    fn transaction(&self, nonce: u64) -> Transaction {
        signed_transfer(&signing_key(7), nonce, 10)
    }

    fn manifest(&self, index: usize) -> Vec<u8> {
        std::fs::read(self.directories[index].path().join(LEDGER_DIR_NAME).join(MANIFEST_FILE_NAME))
            .unwrap()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct VisibleState {
    height: u64,
    tip: Hash,
    root: Hash,
    sender_balance: u128,
    sender_nonce: u64,
    recipient_balance: u128,
    pending_count: usize,
    receipt: Option<Receipt>,
}

impl VisibleState {
    fn capture(state: &RpcState, sender: &Address, transaction: &Hash) -> Self {
        Self {
            height: state.block_height(),
            tip: state.tip_hash(),
            root: state.state_root(),
            sender_balance: state.balance(sender),
            sender_nonce: state.nonce(sender),
            recipient_balance: state.balance(&RECIPIENT),
            pending_count: state.pending_count(),
            receipt: state.receipt(transaction),
        }
    }
}

fn assert_converged(states: &[&RpcState], sender: &Address, hash: &Hash, height: u64) {
    let first = VisibleState::capture(states[0], sender, hash);
    assert_eq!(first.height, height);
    assert!(first.receipt.is_some(), "committed transaction must have a receipt");
    for state in &states[1..] {
        assert_eq!(VisibleState::capture(state, sender, hash), first);
        for block_height in 1..=height {
            assert_eq!(
                state.block_at_height(block_height),
                states[0].block_at_height(block_height)
            );
        }
    }
}

async fn wait_for_height(
    states: &[&RpcState],
    height: u64,
    drivers: &[Arc<Mutex<ConsensusDriver>>],
) {
    // These clusters spend real 750 ms view timeouts and run beside every other
    // cluster test in the same process, so convergence can take ~20 s under
    // load. Keep a bounded failure, but do not turn scheduler pressure into a
    // false liveness regression.
    tokio::time::timeout(Duration::from_secs(60), async {
        while states.iter().any(|state| state.block_height() != height) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let rounds: Vec<_> = drivers
            .iter()
            .map(|driver| {
                let checkpoint = driver.lock().unwrap().checkpoint();
                (
                    checkpoint.view,
                    checkpoint.proposal.as_ref().map(|p| (p.view, p.proposer, p.block.hash())),
                    checkpoint.votes.iter().filter(|v| v.phase == VotePhase::Prepare).count(),
                    checkpoint.votes.iter().filter(|v| v.phase == VotePhase::Commit).count(),
                    checkpoint.prepared.as_ref().map(|p| p.qc.view),
                    checkpoint.timeout_certificate.as_ref().map(|tc| tc.view),
                    checkpoint
                        .view_changes
                        .iter()
                        .map(|vc| (vc.validator, vc.view))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        panic!(
            "validators did not converge to height {height}; observed {:?}; rounds {:?}",
            states.iter().map(|state| state.block_height()).collect::<Vec<_>>(),
            rounds,
        )
    });
}

struct Service {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), NodeError>>>,
}

impl Service {
    fn start(node: Node) -> Self {
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(node.run_until(async move {
            let _ = receiver.await;
            Ok(())
        }));
        Self { shutdown: Some(shutdown), task: Some(task) }
    }

    async fn stop(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task.take().unwrap())
            .await
            .expect("controlled shutdown must release the node's services")
            .expect("node service must not panic")
            .expect("node service must stop successfully");
    }

    async fn abort(mut self) {
        let task = self.task.take().unwrap();
        task.abort();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("node cancellation must finish")
            .expect_err("aborted node must report cancellation");
        assert!(error.is_cancelled());
        self.shutdown.take();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A link between the two test partitions, activated only after the shortfall checks.
/// Noise still authenticates the end-to-end connection between the actual peers.
struct Bridge {
    task: JoinHandle<()>,
}

impl Bridge {
    fn start(listener: TcpListener, target: u16) -> Self {
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let (mut incoming, _) = connection.unwrap();
                        connections.spawn(async move {
                            if let Ok(mut outgoing) =
                                tokio::net::TcpStream::connect(("127.0.0.1", target)).await
                            {
                                let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing)
                                    .await;
                            }
                        });
                    }
                    finished = connections.join_next(), if !connections.is_empty() => {
                        finished.unwrap().expect("test bridge connection must not panic");
                    }
                }
            }
        });
        Self { task }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Dropping the bridge's JoinSet also aborts its connection-copy children.
        self.task.abort();
    }
}

async fn wait_for_view(drivers: &[&Arc<Mutex<ConsensusDriver>>], minimum: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while drivers.iter().any(|driver| driver.lock().unwrap().view() < minimum) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("local timeouts must advance views even without quorum");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_validators_commit_across_the_silent_fourth_leaders_round() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let nodes: Vec<_> = (0..3)
        .map(|index| {
            let peers = if index == 0 { Vec::new() } else { vec![fixture.address(0)] };
            fixture.bootstrap(index, peers)
        })
        .collect();
    let states: Vec<_> = nodes.iter().map(|node| node.state().clone()).collect();
    let drivers: Vec<_> = nodes.iter().map(|node| node.consensus().unwrap().clone()).collect();
    let initial = fixture.transaction(0);
    for state in &states {
        state.submit_transaction(initial.clone()).unwrap();
    }
    let services: Vec<_> =
        nodes.into_iter().enumerate().map(|(index, node)| fixture.start(index, node)).collect();
    let state_refs: Vec<_> = states.iter().collect();
    for nonce in 0..4 {
        let tx = fixture.transaction(nonce);
        if nonce != 0 {
            for state in &states {
                state.submit_transaction(tx.clone()).unwrap();
            }
        }
        wait_for_height(&state_refs, nonce + 1, &drivers).await;
        assert_converged(&state_refs, &fixture.sender, &tx.hash(), nonce + 1);
        assert_eq!(states[0].nonce(&fixture.sender), nonce + 1);
        assert_eq!(states[0].balance(&RECIPIENT), 10 * u128::from(nonce + 1));
        for driver in &drivers {
            let driver = driver.lock().unwrap();
            let qc = driver.committed_qc().unwrap();
            assert_eq!(qc.signatures.len(), 3, "the offline validator cannot supply a vote");
            assert_eq!(qc.block_hash, states[0].tip_hash());
            driver.validate_commit_certificate(qc).unwrap();
        }
    }
    assert!(
        drivers[0].lock().unwrap().committed_qc().unwrap().view > 4,
        "the unavailable fourth leader must be replaced before the fourth commit"
    );
    for service in services {
        service.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_initial_leader_is_replaced_without_transaction_gossip() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let nodes: Vec<_> = (1..4)
        .map(|index| {
            let peers = if index == 1 { Vec::new() } else { vec![fixture.address(1)] };
            fixture.bootstrap(index, peers)
        })
        .collect();
    let tx = fixture.transaction(0);
    let states: Vec<_> = nodes.iter().map(|node| node.state().clone()).collect();
    let drivers: Vec<_> = nodes.iter().map(|node| node.consensus().unwrap().clone()).collect();
    for (node, state) in nodes.iter().zip(&states) {
        assert_eq!(node.consensus().unwrap().lock().unwrap().leader(), Address::new([1; 20]));
        state.submit_transaction(tx.clone()).unwrap();
    }
    let services: Vec<_> =
        nodes.into_iter().zip(1..4).map(|(node, index)| fixture.start(index, node)).collect();
    let state_refs: Vec<_> = states.iter().collect();
    wait_for_height(&state_refs, 1, &drivers).await;
    assert_converged(&state_refs, &fixture.sender, &tx.hash(), 1);
    for driver in &drivers {
        let driver = driver.lock().unwrap();
        let qc = driver.committed_qc().unwrap();
        assert!(qc.view > 1, "the initial leader supplied neither a proposal nor a vote");
        assert_eq!(qc.signatures.len(), 3);
        driver.validate_commit_certificate(qc).unwrap();
    }
    for service in services {
        service.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_live_validators_advance_views_without_touching_committed_state() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let first = fixture.bootstrap(0, Vec::new());
    let peer = fixture.address(0);
    let second = fixture.bootstrap(1, vec![peer]);
    let nodes = [first, second];
    let tx = fixture.transaction(0);
    let states: Vec<_> = nodes.iter().map(|node| node.state().clone()).collect();
    let drivers: Vec<_> = nodes.iter().map(|node| node.consensus().unwrap().clone()).collect();
    for state in &states {
        state.submit_transaction(tx.clone()).unwrap();
    }
    let before: Vec<_> = states
        .iter()
        .map(|state| VisibleState::capture(state, &fixture.sender, &tx.hash()))
        .collect();
    let manifests = [fixture.manifest(0), fixture.manifest(1)];
    let services: Vec<_> =
        nodes.into_iter().enumerate().map(|(index, node)| fixture.start(index, node)).collect();
    let driver_refs: Vec<_> = drivers.iter().collect();
    wait_for_view(&driver_refs, 3).await;
    for (index, state) in states.iter().enumerate() {
        assert_eq!(VisibleState::capture(state, &fixture.sender, &tx.hash()), before[index]);
        assert_eq!(fixture.manifest(index), manifests[index]);
        assert!(drivers[index].lock().unwrap().pending_qc().is_none());
        assert!(drivers[index].lock().unwrap().prepared_certificate().is_none());
    }
    for service in services {
        service.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_minority_partitions_converge_after_the_network_link_is_restored() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let inter_partition = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let link_port = inter_partition.local_addr().unwrap().port();
    let topology = [
        Vec::new(),
        vec![fixture.address(0)],
        vec![format!("/ip4/127.0.0.1/tcp/{link_port}")],
        vec![fixture.address(2)],
    ];
    let nodes: Vec<_> = topology
        .into_iter()
        .enumerate()
        .map(|(index, peers)| fixture.bootstrap(index, peers))
        .collect();
    let tx = fixture.transaction(0);
    let states: Vec<_> = nodes.iter().map(|node| node.state().clone()).collect();
    let drivers: Vec<_> = nodes.iter().map(|node| node.consensus().unwrap().clone()).collect();
    for state in &states {
        state.submit_transaction(tx.clone()).unwrap();
    }
    let before: Vec<_> = states
        .iter()
        .map(|state| VisibleState::capture(state, &fixture.sender, &tx.hash()))
        .collect();
    let manifests: Vec<_> = (0..4).map(|index| fixture.manifest(index)).collect();
    let services: Vec<_> =
        nodes.into_iter().enumerate().map(|(index, node)| fixture.start(index, node)).collect();
    let driver_refs: Vec<_> = drivers.iter().collect();
    wait_for_view(&driver_refs, 2).await;
    for (index, state) in states.iter().enumerate() {
        assert_eq!(VisibleState::capture(state, &fixture.sender, &tx.hash()), before[index]);
        assert_eq!(fixture.manifest(index), manifests[index]);
    }
    let bridge = Bridge::start(inter_partition, fixture.ports[0]);
    let state_refs: Vec<_> = states.iter().collect();
    wait_for_height(&state_refs, 1, &drivers).await;
    assert_converged(&state_refs, &fixture.sender, &tx.hash(), 1);
    for driver in &drivers {
        let driver = driver.lock().unwrap();
        driver.validate_commit_certificate(driver.committed_qc().unwrap()).unwrap();
    }
    for service in services {
        service.stop().await;
    }
    drop(bridge);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_running_validator_releases_its_journal_and_peer_listener() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let rpc_port = listener.local_addr().unwrap().port();
    fixture.configs[0].rpc_addr = ([127, 0, 0, 1], rpc_port).into();
    let node = fixture.bootstrap(0, Vec::new());
    node.state().submit_transaction(fixture.transaction(0)).unwrap();
    let config = node.config().clone();
    let driver = node.consensus().unwrap().clone();
    drop(listener);
    let service = fixture.start(0, node);
    wait_until_listening(rpc_port).await;
    wait_for_view(&[&driver], 2).await;
    service.abort().await;
    // JoinSet cancellation wakes children independently of the parent's join.
    // Reopening must succeed promptly once those cancellation wakes have run.
    let restarted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match Node::bootstrap(config.clone()) {
                Ok(node) => return node,
                Err(NodeError::ConsensusJournal { .. }) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("unexpected restart failure after cancellation: {error}"),
            }
        }
    })
    .await
    .expect("aborting the node must release every journal owner");
    assert_eq!(restarted.state().block_height(), 0);
    assert!(restarted.consensus().unwrap().lock().unwrap().view() >= 2);
    let listener = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match TcpListener::bind(("127.0.0.1", rpc_port)) {
                Ok(listener) => return listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("unexpected RPC listener check failure: {error}"),
            }
        }
    })
    .await
    .expect("cancelling the node must stop its JSON-RPC server too");
    drop(listener);
    let service = fixture.start(0, restarted);
    wait_until_listening(rpc_port).await;
    wait_until_listening(fixture.ports[0]).await;
    service.stop().await;
    let restarted = Node::bootstrap(config).unwrap();
    assert_eq!(restarted.state().block_height(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controlled_shutdown_allows_an_immediate_restart_on_the_same_rpc_port() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let rpc_port = listener.local_addr().unwrap().port();
    fixture.configs[0].rpc_addr = ([127, 0, 0, 1], rpc_port).into();
    let mut rpc_reservation = Some(listener);
    for _ in 0..2 {
        let node = fixture.bootstrap(0, Vec::new());
        drop(rpc_reservation.take());
        let service = fixture.start(0, node);
        wait_until_listening(rpc_port).await;
        wait_until_listening(fixture.ports[0]).await;
        service.stop().await;
        rpc_reservation = Some(
            TcpListener::bind(("127.0.0.1", rpc_port))
                .expect("controlled shutdown must await RPC listener closure"),
        );
        fixture.listeners[0] = Some(
            TcpListener::bind(("127.0.0.1", fixture.ports[0]))
                .expect("controlled shutdown must await P2P listener closure"),
        );
    }
}

#[test]
fn a_hidden_final_qc_cannot_erase_the_prepared_lock_on_timeout_and_restart() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let mut fixture = Validators::four();
    let mut nodes: Vec<Option<Node>> =
        (0..4).map(|index| Some(fixture.bootstrap(index, Vec::new()))).collect();
    let transaction = fixture.transaction(0);
    let producer = nodes[0].as_ref().unwrap();
    let voter = nodes[1].as_ref().unwrap();
    producer.state().submit_transaction(transaction.clone()).unwrap();
    let block = producer.state().build_block().unwrap().unwrap();
    let proposal = {
        let mut driver = producer.consensus().unwrap().lock().unwrap();
        let (proposal, _) = driver.propose(block.clone()).unwrap();
        proposal
    };
    let prepare_vote = {
        voter.state().validate_proposal(&proposal.block).unwrap();
        let mut driver = voter.consensus().unwrap().lock().unwrap();
        driver.observe_proposal(proposal.clone()).unwrap().unwrap()
    };
    let byzantine_key =
        read_producer_key(&fixture.directories[3].path().join(PRODUCER_KEY_FILE_NAME)).unwrap();
    let prepared = {
        let mut driver = producer.consensus().unwrap().lock().unwrap();
        driver.record_vote(prepare_vote).unwrap();
        driver
            .record_vote(Vote {
                view: proposal.view,
                block_hash: block.hash(),
                validator: Address::new([4; 20]),
                signature: BLSScheme::encode_signature(&BLSScheme::sign(
                    &byzantine_key,
                    &Vote::payload_for_context(
                        proposal.protocol_context,
                        VotePhase::Prepare,
                        proposal.view,
                        &block.hash(),
                    ),
                )),
                phase: VotePhase::Prepare,
            })
            .unwrap();
        driver.prepared_certificate().unwrap()
    };
    let commit_vote = {
        let mut driver = voter.consensus().unwrap().lock().unwrap();
        let vote = driver.receive_prepare(prepared.clone()).unwrap().unwrap();
        voter.consensus_journal.as_ref().unwrap().save(&driver).unwrap();
        vote
    };
    // The final QC reaches only the producer. The other honest signer has
    // released a commit vote and must retain its prepare proof without seeing finality.
    {
        let mut driver = producer.consensus().unwrap().lock().unwrap();
        driver.record_vote(commit_vote).unwrap();
        driver
            .record_vote(Vote {
                view: proposal.view,
                block_hash: block.hash(),
                validator: Address::new([4; 20]),
                signature: BLSScheme::encode_signature(&BLSScheme::sign(
                    &byzantine_key,
                    &Vote::payload_for_context(
                        proposal.protocol_context,
                        VotePhase::Commit,
                        proposal.view,
                        &block.hash(),
                    ),
                )),
                phase: VotePhase::Commit,
            })
            .unwrap();
        let qc = driver.pending_qc().unwrap();
        let journal = producer.consensus_journal.as_ref().unwrap();
        journal.prepare_commit(&driver, block.clone(), qc.clone()).unwrap();
        producer.state().commit_qc(block.clone(), &qc).unwrap();
        driver.receive_commit(block.clone(), qc).unwrap();
        journal.save(&driver).unwrap();
    }
    assert_eq!(producer.state().block_height(), 1);
    assert_eq!(voter.state().block_height(), 0);
    assert!(voter.state().receipt(&transaction.hash()).is_none());
    let report = {
        let mut driver = voter.consensus().unwrap().lock().unwrap();
        driver.tick(Duration::ZERO).unwrap();
        let report = driver.tick(Duration::from_millis(750)).unwrap().unwrap();
        assert_eq!(report.prepared, Some(prepared.clone()));
        voter.consensus_journal.as_ref().unwrap().save(&driver).unwrap();
        report
    };
    let checkpoint = voter.consensus().unwrap().lock().unwrap().checkpoint();
    let config = voter.config().clone();
    drop(nodes[1].take());
    let restarted = Node::bootstrap(config).unwrap();
    {
        let driver = restarted.consensus().unwrap().lock().unwrap();
        assert_eq!(driver.checkpoint(), checkpoint);
        assert_eq!(driver.prepared_certificate(), Some(prepared.clone()));
    }
    let other_report = {
        let node = nodes[2].as_ref().unwrap();
        let mut driver = node.consensus().unwrap().lock().unwrap();
        driver.tick(Duration::ZERO).unwrap();
        driver.tick(Duration::from_millis(750)).unwrap().unwrap()
    };
    let mut omission = ViewChange {
        view: report.view,
        height: report.height,
        tip: report.tip,
        protocol_context: report.protocol_context,
        validator: Address::new([4; 20]),
        prepared: None,
        signature: Vec::new(),
    };
    omission.signature =
        BLSScheme::encode_signature(&BLSScheme::sign(&byzantine_key, &omission.signing_payload()));
    let certificate = TimeoutCertificate {
        view: report.view,
        height: report.height,
        tip: report.tip,
        messages: vec![report.clone(), other_report.clone(), omission.clone()],
    };
    let alternative = {
        let node = nodes[2].as_ref().unwrap();
        node.state().submit_transaction(transaction.clone()).unwrap();
        node.state().build_block().unwrap().unwrap()
    };
    restarted.state().validate_proposal(&alternative).unwrap();
    assert_ne!(alternative.hash(), block.hash(), "use a valid conflicting block");
    {
        let mut driver = restarted.consensus().unwrap().lock().unwrap();
        driver.validate_timeout_certificate(&certificate).unwrap();
        driver.record_view_change(other_report).unwrap();
        driver.record_view_change(omission).unwrap();
        assert_eq!(driver.preferred_block(), Some(block.clone()));
        let before = driver.checkpoint();
        assert!(driver.propose(alternative).is_err());
        assert_eq!(driver.checkpoint(), before, "rejected proposal must not consume a vote");
        let (_, vote) = driver.propose(block.clone()).unwrap();
        assert_eq!(vote.block_hash, block.hash());
        assert_eq!(vote.phase, VotePhase::Prepare);
        restarted.consensus_journal.as_ref().unwrap().save(&driver).unwrap();
    }
    let checkpoint = restarted.consensus().unwrap().lock().unwrap().checkpoint();
    let config = restarted.config().clone();
    drop(restarted);
    let restarted = Node::bootstrap(config).unwrap();
    assert_eq!(restarted.consensus().unwrap().lock().unwrap().checkpoint(), checkpoint);
    assert_eq!(restarted.state().block_height(), 0);
    {
        let mut driver = restarted.consensus().unwrap().lock().unwrap();
        driver.tick(Duration::ZERO).unwrap();
        let next = driver.tick(Duration::from_millis(750)).unwrap().unwrap();
        assert_eq!(next.prepared, Some(prepared));
        let before = driver.checkpoint();
        assert!(!driver.record_view_change(report.clone()).unwrap());
        assert_eq!(driver.checkpoint(), before, "old timeouts must not roll back the view");
        let mut forged = next;
        forged.prepared = None;
        assert!(driver.record_view_change(forged).is_err());
        assert_eq!(
            driver.checkpoint(),
            before,
            "tampered lock reports must not change voting state"
        );
    }
}

#[test]
fn an_empty_timeout_report_cannot_be_replayed_into_another_genesis() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let mut fixture = Validators::four();
    let node = fixture.bootstrap(0, Vec::new());
    let report = {
        let mut driver = node.consensus().unwrap().lock().unwrap();
        driver.tick(Duration::ZERO).unwrap();
        driver.tick(Duration::from_millis(750)).unwrap().unwrap()
    };
    assert!(report.prepared.is_none(), "exercise a report with no block hash binding");
    let directory = tempfile::tempdir().unwrap();
    let mut genesis = node.genesis().unwrap().clone();
    genesis.allocations.insert(Address::new([0xaa; 20]), 100);
    genesis.save(&directory.path().join(GENESIS_FILE_NAME)).unwrap();
    let key =
        read_producer_key(&fixture.directories[2].path().join(PRODUCER_KEY_FILE_NAME)).unwrap();
    write_producer_key(&directory.path().join(PRODUCER_KEY_FILE_NAME), &key).unwrap();
    let mut config = NodeConfig::new(directory.path());
    config.validator = true;
    config.rpc_addr = ([127, 0, 0, 1], 0).into();
    config.view_timeout_ms = 750;
    let other_genesis = Node::bootstrap(config).unwrap();
    let mut driver = other_genesis.consensus().unwrap().lock().unwrap();
    driver.tick(Duration::ZERO).unwrap();
    let own_report = driver.tick(Duration::from_millis(750)).unwrap().unwrap();
    assert_ne!(own_report.protocol_context, report.protocol_context);
    let before = driver.checkpoint();
    assert!(driver.record_view_change(report).is_err());
    assert_eq!(driver.checkpoint(), before);
    assert_eq!(other_genesis.state().block_height(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_validators_align_different_saved_views_and_reach_quorum() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let mut fixture = Validators::four();
    let targets = [3, 5, 7];
    let nodes: Vec<_> = (0..3)
        .map(|index| {
            let peers = if index == 0 { Vec::new() } else { vec![fixture.address(0)] };
            fixture.bootstrap(index, peers)
        })
        .collect();
    let configs: Vec<_> = nodes.iter().map(|node| node.config().clone()).collect();
    for ((index, node), target) in nodes.iter().enumerate().zip(targets) {
        let mut driver = node.consensus().unwrap().lock().unwrap();
        driver.tick(Duration::ZERO).unwrap();
        let mut report = driver.tick(Duration::from_millis(750)).unwrap().unwrap();
        let key =
            read_producer_key(&fixture.directories[index].path().join(PRODUCER_KEY_FILE_NAME))
                .unwrap();
        // Model valid timeout-only state saved by the earlier pacemaker. The
        // new pacemaker must recover it without requiring a quorum that was
        // never collected; this fixture contains no forgotten voting lock.
        report.view = target;
        report.signature =
            BLSScheme::encode_signature(&BLSScheme::sign(&key, &report.signing_payload()));
        let mut checkpoint = driver.checkpoint();
        checkpoint.view = target;
        checkpoint.view_changes = vec![report.clone()];
        checkpoint.own_view_change = Some(report.clone());
        let mut config = ConsensusConfig::new(4).unwrap();
        config.view_timeout_ms = 750;
        config.block_time_ms = node.genesis().unwrap().block_time_ms;
        config.protocol_context = report.protocol_context;
        let registry =
            ValidatorRegistry::from_genesis(&node.genesis().unwrap().initial_validators).unwrap();
        *driver = ConsensusDriver::from_checkpoint(
            config,
            registry,
            Some((report.validator, key)),
            checkpoint,
        )
        .unwrap();
        node.consensus_journal.as_ref().unwrap().save(&driver).unwrap();
        assert_eq!(driver.view(), target);
        assert!(driver.prepared_certificate().is_none());
        assert_eq!(node.state().block_height(), 0);
    }
    // The three honest replicas recover different durable rounds; no fourth
    // process or post-restart manual tick can manufacture their quorum.
    drop(nodes);
    let nodes: Vec<_> =
        configs.into_iter().map(|config| Node::bootstrap(config).unwrap()).collect();
    let tx = fixture.transaction(0);
    let states: Vec<_> = nodes.iter().map(|node| node.state().clone()).collect();
    let drivers: Vec<_> = nodes.iter().map(|node| node.consensus().unwrap().clone()).collect();
    for ((node, state), target) in nodes.iter().zip(&states).zip(targets) {
        assert_eq!(node.consensus().unwrap().lock().unwrap().view(), target);
        state.submit_transaction(tx.clone()).unwrap();
    }
    let services: Vec<_> =
        nodes.into_iter().enumerate().map(|(index, node)| fixture.start(index, node)).collect();
    let state_refs: Vec<_> = states.iter().collect();
    wait_for_height(&state_refs, 1, &drivers).await;
    assert_converged(&state_refs, &fixture.sender, &tx.hash(), 1);
    for driver in &drivers {
        let driver = driver.lock().unwrap();
        let qc = driver.committed_qc().unwrap();
        assert_eq!(qc.signatures.len(), 3);
        driver.validate_commit_certificate(qc).unwrap();
    }
    for service in services {
        service.stop().await;
    }
}

#[test]
fn a_lagging_designated_leader_persists_a_complete_timeout_certificate_from_the_wire() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let mut fixture = Validators::four();
    let node = fixture.bootstrap(0, Vec::new());
    let tx = fixture.transaction(0);
    node.state().submit_transaction(tx.clone()).unwrap();
    let context = node.state().consensus_context();
    assert_ne!(context, Hash::ZERO, "node consensus must bind its actual genesis");
    let messages: Vec<_> = (1..4)
        .map(|index| {
            let key =
                read_producer_key(&fixture.directories[index].path().join(PRODUCER_KEY_FILE_NAME))
                    .unwrap();
            let mut report = ViewChange {
                view: 5,
                height: 1,
                tip: Hash::ZERO,
                protocol_context: context,
                validator: Address::new([index as u8 + 1; 20]),
                prepared: None,
                signature: Vec::new(),
            };
            report.signature =
                BLSScheme::encode_signature(&BLSScheme::sign(&key, &report.signing_payload()));
            report
        })
        .collect();
    let certificate = TimeoutCertificate { view: 5, height: 1, tip: Hash::ZERO, messages };
    let before = VisibleState::capture(node.state(), &fixture.sender, &tx.hash());
    let committed_view = node.state().committed_view();
    let manifest = fixture.manifest(0);
    let checkpoint = {
        let mut driver = node.consensus().unwrap().lock().unwrap();
        assert_eq!(driver.view(), 1);
        let _ = handle_consensus_message_inner(
            node.state(),
            &mut driver,
            NetworkMessage::TimeoutCertificate { data: encode_canonical(&certificate) },
            node.consensus_journal.as_deref(),
            node.certified_history.as_deref(),
        )
        .unwrap();
        assert_eq!(driver.view(), 5);
        assert!(driver.is_leader(), "the lagging replica is the target view's designated leader");
        let accepted = driver.timeout_certificate().unwrap();
        assert_eq!(accepted.view, 5);
        driver.validate_timeout_certificate(&accepted).unwrap();
        assert!(driver.pending_qc().is_none());
        driver.checkpoint()
    };
    assert_eq!(VisibleState::capture(node.state(), &fixture.sender, &tx.hash()), before);
    assert_eq!(node.state().committed_view(), committed_view);
    assert_eq!(fixture.manifest(0), manifest);
    let config = node.config().clone();
    drop(node);
    let restarted = Node::bootstrap(config).unwrap();
    assert_eq!(restarted.consensus().unwrap().lock().unwrap().checkpoint(), checkpoint);
    assert_eq!(restarted.state().block_height(), 0);
    assert!(restarted.state().receipt(&tx.hash()).is_none());
}
