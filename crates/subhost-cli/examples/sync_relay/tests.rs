use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::net::TcpListener;
use subhost_consensus::{QuorumCertificate, ValidatorRegistry};
use subhost_core::{
    encode_canonical, Address, Block, Hash, Transaction, TransactionSignature, TransactionType,
    ValidatorInfo, ValidatorSignature,
};
use subhost_crypto::{BLSScheme, PrivateKey};
use subhost_network::NetworkMessage;
use subhost_node::{Node, NodeConfig};
use subhost_rpc::{BlockAuthority, RpcState};

struct Fixture {
    genesis: GenesisConfig,
    producer: RpcState,
    keys: Vec<(Address, PrivateKey)>,
    sender: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        let sender = SigningKey::from_bytes(&[7; 32]);
        let mut genesis = GenesisConfig { chain_id: 31338, ..Default::default() };
        let mut keys = Vec::new();
        for index in 0..4u8 {
            let (secret, public) = BLSScheme::keygen();
            let address = Address::new([0x20 + index; 20]);
            genesis.initial_validators.push(ValidatorInfo {
                address,
                public_key: BLSScheme::encode_public_key(&public),
                power: 1,
                proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                    &secret,
                )),
            });
            keys.push((address, secret));
        }
        genesis
            .allocations
            .insert(Address::from_public_key(&sender.verifying_key().to_bytes()), 1_000_000_000);
        let registry = ValidatorRegistry::from_genesis(&genesis.initial_validators).unwrap();
        let producer = RpcState::in_memory(genesis.chain_id)
            .unwrap()
            .with_authority(BlockAuthority::new(registry, keys[0].0, keys[0].1.clone()).unwrap())
            .with_consensus_context(genesis.consensus_context());
        producer.seed_accounts(genesis.allocations.iter().map(|(a, v)| (*a, *v))).unwrap();
        Self { genesis, producer, keys, sender }
    }

    fn next(&self, nonce: u64) -> NetworkMessage {
        self.next_in_view(nonce, nonce + 1)
    }

    fn next_in_view(&self, nonce: u64, view: u64) -> NetworkMessage {
        let mut tx = Transaction {
            chain_id: self.genesis.chain_id,
            nonce,
            from: Address::from_public_key(&self.sender.verifying_key().to_bytes()),
            to: Some(Address::new([9; 20])),
            value: 10,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            tx_type: TransactionType::Transfer,
            public_key: self.sender.verifying_key().to_bytes(),
            signature: TransactionSignature::EMPTY,
        };
        tx.signature =
            TransactionSignature::from_ed25519(&self.sender.sign(&tx.signing_payload()).to_bytes());
        self.producer.submit_transaction(tx).unwrap();
        let block = self.producer.build_block().unwrap().unwrap();
        let qc = self.certificate(&block, view, self.producer.consensus_context());
        self.producer.commit_qc(block.clone(), &qc).unwrap();
        Self::envelope(block, qc)
    }

    fn certificate(&self, block: &Block, view: u64, context: Hash) -> QuorumCertificate {
        let payload = QuorumCertificate::payload_for_context(context, view, &block.hash());
        QuorumCertificate {
            protocol_context: context,
            view,
            block_hash: block.hash(),
            signatures: self.keys[..3]
                .iter()
                .map(|(address, key)| {
                    (*address, BLSScheme::encode_signature(&BLSScheme::sign(key, &payload)))
                })
                .collect(),
        }
    }

    fn envelope(block: Block, qc: QuorumCertificate) -> NetworkMessage {
        NetworkMessage::BlockCommit {
            view: qc.view,
            data: encode_canonical(&block),
            signatures: qc.signatures.into_iter().map(|(a, s)| (*a.as_bytes(), s)).collect(),
        }
    }
}

#[test]
fn certified_archive_replays_on_restart_and_excludes_a_second_writer() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    assert!(History::open(dir.path(), &fixture.genesis, 1_000_000).is_err());
    for nonce in 0..3 {
        assert_eq!(archive.ingest(fixture.next(nonce)).unwrap(), 1);
    }
    let root = archive.state_root();
    drop(archive);
    let reopened = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    assert_eq!(reopened.height(), 3);
    assert_eq!(reopened.state_root(), root);
    assert_eq!(reopened.state_root(), fixture.producer.state_root());
}

#[test]
fn insufficient_or_forged_quorums_and_tampered_execution_never_enter_history() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    let valid = fixture.next(0);
    let mut short = valid.clone();
    if let NetworkMessage::BlockCommit { signatures, .. } = &mut short {
        signatures.pop();
    }
    assert!(matches!(archive.ingest(short), Err(IngestError::Invalid(_))));
    let mut forged = valid.clone();
    if let NetworkMessage::BlockCommit { signatures, .. } = &mut forged {
        signatures[0].1[0] ^= 1;
    }
    assert!(matches!(archive.ingest(forged), Err(IngestError::Invalid(_))));
    let mut tampered = valid.clone();
    if let NetworkMessage::BlockCommit { data, .. } = &mut tampered {
        let mut block: Block = bincode::deserialize(data).unwrap();
        block.transactions[0].value += 1;
        *data = encode_canonical(&block);
    }
    assert!(matches!(archive.ingest(tampered), Err(IngestError::Invalid(_))));
    assert_eq!(archive.height(), 0);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|s| s == "cert"))
            .count(),
        0
    );
    assert_eq!(archive.ingest(valid).unwrap(), 1);
}

#[test]
fn corrupt_archives_and_wrong_genesis_are_refused() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    archive.ingest(fixture.next(0)).unwrap();
    drop(archive);
    let mut other = fixture.genesis.clone();
    other.chain_id += 1;
    assert!(History::open(dir.path(), &other, 1_000_000).is_err());
    let path = dir.path().join("00000000000000000001.cert");
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(History::open(dir.path(), &fixture.genesis, 1_000_000).is_err());
}

#[test]
fn public_possession_proofs_cannot_forge_history_certificates() {
    use ark_bls12_381::Fr;
    use ark_ff::{Field, PrimeField};
    use sha3::{Digest, Sha3_384};

    fn legacy_scalar(domain: &[u8], message: &[u8]) -> Fr {
        let mut digest = Sha3_384::new();
        digest.update(domain);
        digest.update((message.len() as u64).to_be_bytes());
        digest.update(message);
        Fr::from_le_bytes_mod_order(&digest.finalize())
    }

    let fixture = Fixture::new();
    let mut forged = fixture.next(0);
    if let NetworkMessage::BlockCommit { view, data, signatures } = &mut forged {
        let block: Block = bincode::deserialize(data).unwrap();
        let payload = QuorumCertificate::payload_for_context(
            fixture.producer.consensus_context(),
            *view,
            &block.hash(),
        );
        let message_scalar = legacy_scalar(b"subhost-bls-01", &payload);
        *signatures = fixture.genesis.initial_validators[..3]
            .iter()
            .map(|validator| {
                let proof = BLSScheme::decode_signature(&validator.proof_of_possession).unwrap();
                let proof_scalar = legacy_scalar(b"subhost-bls-pop-01", &validator.public_key);
                let fake_signature = proof * (message_scalar * proof_scalar.inverse().unwrap());
                (*validator.address.as_bytes(), BLSScheme::encode_signature(&fake_signature))
            })
            .collect();
    }
    let directory = tempfile::tempdir().unwrap();
    let mut archive = History::open(directory.path(), &fixture.genesis, 1_000_000).unwrap();
    assert!(
        matches!(archive.ingest(forged), Err(IngestError::Invalid(_))),
        "public genesis PoPs must not manufacture historical quorum evidence"
    );
    assert_eq!(archive.height(), 0);
}

#[test]
fn full_archive_fails_before_advancing_or_writing_a_block() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1).unwrap();
    assert!(matches!(archive.ingest(fixture.next(0)), Err(IngestError::Storage(_))));
    assert_eq!(archive.height(), 0);
    assert!(!dir.path().join("00000000000000000001.cert").exists());
}

#[test]
fn out_of_order_certificates_wait_for_a_verified_parent() {
    let fixture = Fixture::new();
    let first = fixture.next(0);
    let second = fixture.next(1);
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    assert_eq!(archive.ingest(second).unwrap(), 0);
    assert_eq!(archive.height(), 0);
    assert_eq!(archive.ingest(first).unwrap(), 2);
    assert_eq!(archive.state_root(), fixture.producer.state_root());
}

#[test]
fn timeout_views_are_independent_of_history_height_and_survive_restart() {
    let fixture = Fixture::new();
    let first = fixture.next_in_view(0, 200);
    let second = fixture.next_in_view(1, 350);
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    assert_eq!(archive.ingest(second).unwrap(), 0);
    assert_eq!(archive.ingest(first).unwrap(), 2);
    drop(archive);
    let reopened = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    assert_eq!(reopened.height(), 2);
    assert!(matches!(reopened.message(1), Some(NetworkMessage::BlockCommit { view: 350, .. })));
    assert_eq!(reopened.state_root(), fixture.producer.state_root());
}

#[test]
fn genesis_context_and_final_phase_cannot_be_replayed_into_another_archive() {
    use subhost_consensus::driver::PrepareCertificate;

    let fixture = Fixture::new();
    let valid = fixture.next_in_view(0, 10);
    let NetworkMessage::BlockCommit { data, .. } = &valid else { unreachable!() };
    let block: Block = bincode::deserialize(data).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut other = fixture.genesis.clone();
    other.allocations.insert(Address::new([0x99; 20]), 1);
    let mut archive = History::open(dir.path(), &other, 1_000_000).unwrap();
    assert!(matches!(archive.ingest(valid.clone()), Err(IngestError::Invalid(_))));
    assert!(!dir.path().join("00000000000000000001.cert").exists());
    drop(archive);

    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    let mut prepared = valid.clone();
    let payload = PrepareCertificate::payload_for_context(
        fixture.producer.consensus_context(),
        10,
        &block.hash(),
    );
    if let NetworkMessage::BlockCommit { signatures, .. } = &mut prepared {
        *signatures = fixture.keys[..3]
            .iter()
            .map(|(address, key)| {
                (*address.as_bytes(), BLSScheme::encode_signature(&BLSScheme::sign(key, &payload)))
            })
            .collect();
    }
    assert!(matches!(archive.ingest(prepared), Err(IngestError::Invalid(_))));
    assert_eq!(archive.height(), 0);
    assert!(!dir.path().join("00000000000000000001.cert").exists());
    assert_eq!(archive.ingest(valid).unwrap(), 1);
}

#[test]
fn stale_final_views_and_certified_bad_execution_fail_before_archive_write() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    archive.ingest(fixture.next_in_view(0, 20)).unwrap();
    let valid = fixture.next_in_view(1, 30);
    let NetworkMessage::BlockCommit { data, .. } = &valid else { unreachable!() };
    let mut block: Block = bincode::deserialize(data).unwrap();
    let stale = Fixture::envelope(
        block.clone(),
        fixture.certificate(&block, 20, fixture.producer.consensus_context()),
    );
    assert!(matches!(archive.ingest(stale), Err(IngestError::Invalid(_))));
    block.header.state_root = Hash::ZERO;
    block.signatures = vec![ValidatorSignature {
        validator: fixture.keys[0].0,
        signature: BLSScheme::encode_signature(&BLSScheme::sign(
            &fixture.keys[0].1,
            &block.signing_payload(),
        )),
    }];
    let bad = Fixture::envelope(
        block.clone(),
        fixture.certificate(&block, 30, fixture.producer.consensus_context()),
    );
    assert!(matches!(archive.ingest(bad), Err(IngestError::Invalid(_))));
    assert_eq!(archive.height(), 1);
    assert!(!dir.path().join("00000000000000000002.cert").exists());
    assert_eq!(archive.ingest(valid).unwrap(), 1);
    drop(archive);
    assert_eq!(History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap().height(), 2);
}

#[test]
fn legacy_archive_versions_are_refused_without_rewriting_records() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    let message = fixture.next(0);
    archive.ingest(message.clone()).unwrap();
    drop(archive);
    let path = dir.path().join("00000000000000000001.cert");
    let mut legacy = std::fs::read(&path).unwrap();
    legacy[..4].copy_from_slice(&1u32.to_le_bytes());
    let checksum =
        Hash::from_data(&encode_canonical(&(1u32, fixture.genesis.consensus_context(), message)));
    let checksum_start = legacy.len() - checksum.as_bytes().len();
    legacy[checksum_start..].copy_from_slice(checksum.as_bytes());
    std::fs::write(&path, &legacy).unwrap();
    assert!(History::open(dir.path(), &fixture.genesis, 1_000_000).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), legacy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_the_relay_releases_its_network_listener() {
    let fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let archive = History::open(dir.path(), &fixture.genesis, 1_000_000).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let task = tokio::spawn(relay(
        archive,
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{}", address.port()),
            bootstrap_peers: Vec::new(),
            enable_mdns: false,
            ..Default::default()
        },
        Duration::from_millis(50),
        std::future::pending(),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let listener = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(listener) = TcpListener::bind(address) {
                break listener;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborting a relay must not detach its networking task");
    drop(listener);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_node_automatically_backfills_one_hundred_certified_blocks_over_p2p() {
    let fixture = Fixture::new();
    let archive_dir = tempfile::tempdir().unwrap();
    let mut archive = History::open(archive_dir.path(), &fixture.genesis, 1_000_000).unwrap();
    for nonce in 0..100 {
        archive.ingest(fixture.next(nonce)).unwrap();
    }
    drop(archive);
    let archive = History::open(archive_dir.path(), &fixture.genesis, 1_000_000).unwrap();
    eprintln!("Restored and verified all 100 historical certificates; starting live P2P catch-up");
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_port = first.local_addr().unwrap().port();
    let observer_port = second.local_addr().unwrap().port();
    drop((first, second));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let relay_task = tokio::spawn(relay(
        archive,
        NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{relay_port}"),
            bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{observer_port}")],
            enable_mdns: false,
            ..Default::default()
        },
        Duration::from_millis(10),
        async {
            let _ = stop_rx.await;
        },
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", relay_port)).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("certified-history relay must start listening");
    let observer_dir = tempfile::tempdir().unwrap();
    fixture.genesis.save(&observer_dir.path().join("genesis.json")).unwrap();
    let mut config = NodeConfig::new(observer_dir.path());
    config.rpc_addr = ([127, 0, 0, 1], 0).into();
    config.network = Some(NetworkConfig {
        listen_addr: format!("/ip4/127.0.0.1/tcp/{observer_port}"),
        bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{relay_port}")],
        enable_mdns: false,
        ..Default::default()
    });
    let observer = Node::bootstrap(config).unwrap();
    let state = observer.state().clone();
    assert_eq!(state.consensus_context(), fixture.producer.consensus_context());
    let node_task = tokio::spawn(observer.run());
    // Debug BLS pairing is intentionally unoptimized and can take over two
    // minutes for 100 independently verified final QCs on shared CI runners.
    // This bounds only the test; relay pacing and production timeouts are unchanged.
    let catchup = tokio::time::timeout(Duration::from_secs(180), async {
        while state.block_height() < 100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    stop_tx.send(()).unwrap();
    relay_task.await.unwrap().unwrap();
    node_task.abort();
    let _ = node_task.await;
    assert!(catchup.is_ok(), "fresh observer stalled at height {}", state.block_height());
    assert_eq!(state.block_height(), 100);
    assert_eq!(state.tip_hash(), fixture.producer.tip_hash());
    assert_eq!(state.state_root(), fixture.producer.state_root());
    assert_eq!(state.balance(&Address::new([9; 20])), 1_000);
    assert_ne!(state.tip_hash(), Hash::ZERO);
}
