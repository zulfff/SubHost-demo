use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::net::TcpListener;
use subhost_consensus::{QuorumCertificate, ValidatorRegistry};
use subhost_core::{
    encode_canonical, Address, Hash, Transaction, TransactionSignature, TransactionType,
    ValidatorInfo,
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
            .with_authority(BlockAuthority::new(registry, keys[0].0, keys[0].1.clone()).unwrap());
        producer.seed_accounts(genesis.allocations.iter().map(|(a, v)| (*a, *v))).unwrap();
        Self { genesis, producer, keys, sender }
    }

    fn next(&self, nonce: u64) -> NetworkMessage {
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
        let payload = QuorumCertificate::payload_for(block.header.height, &block.hash());
        let qc = QuorumCertificate {
            view: block.header.height,
            block_hash: block.hash(),
            signatures: self.keys[..3]
                .iter()
                .map(|(address, key)| {
                    (*address, BLSScheme::encode_signature(&BLSScheme::sign(key, &payload)))
                })
                .collect(),
        };
        self.producer.commit_qc(block.clone(), &qc).unwrap();
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
        let mut block: subhost_core::Block = bincode::deserialize(data).unwrap();
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
        let block: subhost_core::Block = bincode::deserialize(data).unwrap();
        let payload = QuorumCertificate::payload_for(*view, &block.hash());
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
    let node_task = tokio::spawn(observer.run());
    let catchup = tokio::time::timeout(Duration::from_secs(120), async {
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
