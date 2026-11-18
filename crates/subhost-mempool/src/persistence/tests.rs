use super::*;
use crate::MempoolConfig;
use ed25519_dalek::{Signer, SigningKey};
use subhost_core::{Address, TransactionSignature};

fn tx(seed: u8, nonce: u64, gas_price: u128, data: Vec<u8>) -> Transaction {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let public_key = key.verifying_key().to_bytes();
    let mut tx = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
        to: Some(Address::new([9; 20])),
        value: 1,
        gas_price,
        gas_limit: 21_000,
        data,
        chain_id: 1,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    tx.signature = TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
    tx
}

fn store(dir: &Path) -> MempoolStore {
    MempoolStore::open(1, Hash::from_data(b"test genesis"), dir).unwrap()
}

fn pool(transactions: Vec<Transaction>) -> Mempool {
    let mut pool = Mempool::default();
    for tx in transactions {
        pool.add(tx).unwrap();
    }
    pool
}

fn raw_payload(store: &MempoolStore, transactions: &[Transaction]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&store.chain_id.to_le_bytes());
    payload.extend_from_slice(store.genesis_hash.as_bytes());
    payload.extend_from_slice(&(transactions.len() as u32).to_le_bytes());
    for tx in transactions {
        let record = bincode::serialize(tx).unwrap();
        payload.extend_from_slice(&(record.len() as u32).to_le_bytes());
        payload.extend_from_slice(&record);
    }
    payload
}

#[test]
fn missing_snapshot_is_empty_and_open_does_not_create_files() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("new-node");
    let store = store(&data);
    assert!(store.load().unwrap().is_empty());
    assert!(!data.exists());
    assert!(matches!(
        MempoolStore::open(0, Hash::ZERO, &data),
        Err(MempoolPersistenceError::InvalidChainId)
    ));
}

#[test]
fn restart_restores_replacements_in_deterministic_order() {
    let first = tx(1, 0, 1, Vec::new());
    let replacement = tx(1, 0, 2, Vec::new());
    let second = tx(2, 0, 3, Vec::new());
    let next = tx(1, 1, 4, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let original = pool(vec![first.clone(), next.clone(), second.clone(), replacement.clone()]);
    store.persist(&original).unwrap();
    let bytes = fs::read(store.path()).unwrap();
    let loaded = MempoolStore::open(1, store.genesis_hash, dir.path()).unwrap().load().unwrap();
    let mut restored = Mempool::default();
    for transaction in loaded {
        restored.add(transaction).unwrap();
    }
    assert!(!restored.contains(&first.hash()));
    for tx in [&replacement, &second, &next] {
        assert!(restored.contains(&tx.hash()));
    }
    store.persist(&pool(vec![second, replacement, next])).unwrap();
    assert_eq!(
        fs::read(store.path()).unwrap(),
        bytes,
        "insertion order must not change the snapshot"
    );
}

#[test]
fn empty_snapshot_clears_pending_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    store.persist(&Mempool::default()).unwrap();
    assert!(store.load().unwrap().is_empty());
}

#[test]
fn corrupted_truncated_and_unknown_version_snapshots_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    let original = fs::read(store.path()).unwrap();
    let mut corrupt = original.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    fs::write(store.path(), &corrupt).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::ChecksumMismatch)));
    for length in [0, 7, 43, original.len() - 1] {
        fs::write(store.path(), &original[..length]).unwrap();
        assert!(store.load().is_err());
    }
    let mut version = original.clone();
    version[8..12].copy_from_slice(&99u32.to_le_bytes());
    fs::write(store.path(), version).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::UnsupportedVersion(99))));
    let mut magic = original.clone();
    magic[0] ^= 1;
    fs::write(store.path(), magic).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::Format(_))));
    fs::write(store.path(), original).unwrap();
    assert_eq!(store.load().unwrap().len(), 1);
}

#[test]
fn chain_and_genesis_contexts_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    let other_chain = MempoolStore::open(2, store.genesis_hash, dir.path()).unwrap();
    assert!(matches!(other_chain.load(), Err(MempoolPersistenceError::ChainMismatch { .. })));
    let other_genesis = MempoolStore::open(1, Hash::ZERO, dir.path()).unwrap();
    assert!(matches!(other_genesis.load(), Err(MempoolPersistenceError::GenesisMismatch)));
}

#[test]
fn invalid_candidate_does_not_replace_the_previous_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    let original = fs::read(store.path()).unwrap();
    let mut invalid = tx(1, 1, 1, Vec::new());
    invalid.signature = TransactionSignature::EMPTY;
    assert!(matches!(
        store.persist(&pool(vec![invalid])),
        Err(MempoolPersistenceError::InvalidTransaction(_))
    ));
    assert_eq!(fs::read(store.path()).unwrap(), original);
    assert_eq!(store.load().unwrap().len(), 1);
}

#[test]
fn valid_checksum_does_not_hide_invalid_signatures_or_duplicate_slots() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let first = tx(1, 0, 1, Vec::new());
    let mut invalid = tx(2, 0, 1, Vec::new());
    invalid.signature = TransactionSignature::EMPTY;
    fs::write(store.path(), envelope(&raw_payload(&store, &[first.clone(), invalid]))).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::InvalidTransaction(1))));
    fs::write(store.path(), envelope(&raw_payload(&store, &[first, tx(1, 0, 2, Vec::new())])))
        .unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::DuplicateSlot(1))));
}

#[test]
fn oversized_file_and_record_counts_are_rejected_before_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    File::create(store.path()).unwrap().set_len(MAX_MEMPOOL_BYTES + 1).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::TooLarge { .. })));
    let mut payload = raw_payload(&store, &[]);
    payload[40..44].copy_from_slice(&(MAX_MEMPOOL_TRANSACTIONS as u32 + 1).to_le_bytes());
    fs::write(store.path(), envelope(&payload)).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::TooMany { .. })));
    payload[40..44].copy_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&(MAX_MEMPOOL_TRANSACTION_BYTES as u32 + 1).to_le_bytes());
    fs::write(store.path(), envelope(&payload)).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::TooLarge { .. })));
}

#[test]
fn oversized_candidate_record_preserves_the_previous_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    let original = fs::read(store.path()).unwrap();
    let mut large =
        Mempool::new(MempoolConfig { max_tx_data_bytes: 2 * 1024 * 1024, ..Default::default() });
    large.add(tx(2, 0, 1, vec![0; MAX_MEMPOOL_TRANSACTION_BYTES as usize])).unwrap();
    assert!(matches!(store.persist(&large), Err(MempoolPersistenceError::TooLarge { .. })));
    assert_eq!(fs::read(store.path()).unwrap(), original);
}

#[test]
fn trailing_and_truncated_records_are_not_loaded_as_a_partial_pool() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut payload = raw_payload(&store, &[tx(1, 0, 1, Vec::new()), tx(2, 0, 1, Vec::new())]);
    payload.pop();
    fs::write(store.path(), envelope(&payload)).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::Format(_))));
    let mut payload = raw_payload(&store, &[]);
    payload.push(0);
    fs::write(store.path(), envelope(&payload)).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::Format(_))));
    let mut payload = raw_payload(&store, &[tx(1, 0, 1, Vec::new())]);
    let length = u32::from_le_bytes(payload[44..48].try_into().unwrap());
    payload[44..48].copy_from_slice(&(length + 1).to_le_bytes());
    payload.push(0);
    fs::write(store.path(), envelope(&payload)).unwrap();
    assert!(matches!(store.load(), Err(MempoolPersistenceError::Encoding(_))));
}

#[test]
fn failed_snapshot_rename_preserves_the_target_directory_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    fs::create_dir(store.path()).unwrap();
    fs::write(store.path().join("keep"), b"existing contents").unwrap();
    let pending = pool(vec![tx(1, 0, 1, Vec::new())]);
    assert!(matches!(store.persist(&pending), Err(MempoolPersistenceError::Io { .. })));
    assert_eq!(fs::read(store.path().join("keep")).unwrap(), b"existing contents");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "failed write cleans its temporary");
    fs::remove_file(store.path().join("keep")).unwrap();
    fs::remove_dir(store.path()).unwrap();
    store.persist(&pending).unwrap();
    assert_eq!(store.load().unwrap().len(), 1);
}

#[test]
fn filesystem_failure_preserves_the_blocker_and_allows_retry_after_repair() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("node-data");
    fs::write(&blocked, b"keep this file").unwrap();
    let store = store(&blocked);
    let pending = pool(vec![tx(1, 0, 1, Vec::new())]);
    assert!(matches!(store.persist(&pending), Err(MempoolPersistenceError::Io { .. })));
    assert_eq!(fs::read(&blocked).unwrap(), b"keep this file");
    fs::remove_file(blocked).unwrap();
    store.persist(&pending).unwrap();
    assert_eq!(store.load().unwrap().len(), 1);
}

#[test]
fn orphan_temporaries_do_not_override_a_committed_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&pool(vec![tx(1, 0, 1, Vec::new())])).unwrap();
    fs::write(dir.path().join(".tmp-interrupted"), b"partial").unwrap();
    assert_eq!(store.load().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn snapshot_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.persist(&Mempool::default()).unwrap();
    assert_eq!(fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
}
