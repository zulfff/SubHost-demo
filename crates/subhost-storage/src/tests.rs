use super::*;
use crate::{legacy, segmented};
use ed25519_dalek::{Signer, SigningKey};
use std::io::Write;
use std::path::{Path, PathBuf};
use subhost_core::{
    Address, BlockHeader, Transaction, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT,
};
use subhost_state::State;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[1u8; 32])
}

fn sender() -> Address {
    Address::from_public_key(&key().verifying_key().to_bytes())
}

fn transfer(nonce: u64) -> Transaction {
    let key = key();
    let public_key = key.verifying_key().to_bytes();
    let mut tx = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
        to: Some(Address::new([2; 20])),
        value: 10,
        gas_price: 1,
        gas_limit: 21_000,
        data: Vec::new(),
        chain_id: 1,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    let signature = key.sign(&tx.signing_payload()).to_bytes();
    tx.signature = TransactionSignature::from_ed25519(&signature);
    tx
}

/// Seal a list of blocks, applying every transaction to a fresh state.
///
/// This mirrors what a producer must do: gas and commitments are accumulated
/// per transaction and the state root is taken after the whole block.
fn build(block_transactions: Vec<Vec<Transaction>>) -> (LedgerSnapshot, State) {
    let mut state = State::with_chain_id(1);
    state.credit(sender(), 10_000_000).unwrap();
    let mut blocks = Vec::new();
    let mut receipts = Vec::new();
    let mut parent = Hash::ZERO;

    for (index, transactions) in block_transactions.into_iter().enumerate() {
        let height = u64::try_from(index).unwrap() + 1;
        let mut hashes = Vec::with_capacity(transactions.len());
        let mut commitments = Vec::with_capacity(transactions.len());
        let mut gas_used = 0u64;
        for (tx_index, tx) in transactions.iter().enumerate() {
            state.apply_transaction(tx).unwrap();
            let tx_hash = tx.hash();
            gas_used += tx.gas_limit;
            hashes.push(tx_hash);
            commitments.push(receipt_commitment(
                tx_hash,
                height,
                u32::try_from(tx_index).unwrap(),
                tx.gas_limit,
                gas_used,
            ));
        }

        let header = BlockHeader {
            version: 1,
            chain_id: 1,
            height,
            timestamp: 1_700_000_000 + height,
            parent_hash: parent,
            state_root: state.root(),
            tx_root: tx_root_of(&hashes),
            receipt_root: receipts_root(&commitments),
            validator: Address::ZERO,
            gas_used,
            gas_limit: BLOCK_GAS_LIMIT,
            extra_data: Vec::new(),
        };
        let block = Block { header, transactions, signatures: Vec::new() };

        let mut cumulative_gas_used = 0u64;
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used += tx.gas_limit;
            receipts.push(Receipt {
                tx_hash: tx.hash(),
                block_hash: block.hash(),
                block_height: height,
                transaction_index: u32::try_from(tx_index).unwrap(),
                gas_used: tx.gas_limit,
                cumulative_gas_used,
                status: ReceiptStatus::Success,
                logs: Vec::new(),
                contract_address: None,
            });
        }

        parent = block.hash();
        blocks.push(block);
    }

    (LedgerSnapshot { chain_id: 1, state: state.snapshot(), blocks, receipts }, state)
}

/// Build a one-transaction-per-block ledger the validator must accept.
fn ledger(block_count: u64) -> (LedgerSnapshot, State) {
    build((0..block_count).map(|nonce| vec![transfer(nonce)]).collect())
}

fn open(dir: &Path) -> LedgerStore {
    LedgerStore::open(1, dir).unwrap()
}

fn manifest_value(dir: &Path) -> serde_json::Value {
    let bytes = fs::read(segmented::manifest_path(dir)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn write_manifest_value(dir: &Path, value: &serde_json::Value) {
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    fs::write(segmented::manifest_path(dir), bytes).unwrap();
}

fn segment_path(dir: &Path, index: u64) -> PathBuf {
    dir.join(LEDGER_DIR_NAME).join("segments").join(format!("{index:06}.log"))
}

fn snapshot_path(dir: &Path, height: u64) -> PathBuf {
    dir.join(LEDGER_DIR_NAME).join(format!("snapshot-{height:010}.bin"))
}

/// Names directly inside `ledger/`, sorted.
fn ledger_dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.join(LEDGER_DIR_NAME))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_block_may_hold_several_transactions() {
    let (snapshot, state) = build(vec![vec![transfer(0), transfer(1), transfer(2)]]);
    assert_eq!(snapshot.blocks.len(), 1);
    assert_eq!(snapshot.blocks[0].transactions.len(), 3);
    assert_eq!(snapshot.receipts.len(), 3);

    let indexes: Vec<u32> = snapshot.receipts.iter().map(|r| r.transaction_index).collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    let cumulative: Vec<u64> = snapshot.receipts.iter().map(|r| r.cumulative_gas_used).collect();
    assert_eq!(cumulative, vec![21_000, 42_000, 63_000]);
    assert_eq!(snapshot.blocks[0].header.gas_used, 63_000);

    assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &state).is_ok());
    assert_eq!(state.balance(&Address::new([2; 20])), 30);
}

#[test]
fn receipt_fields_are_committed_even_when_the_list_is_reordered() {
    let (snapshot, state) = build(vec![vec![transfer(0), transfer(1), transfer(2)]]);
    assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &state).is_ok());

    // Receipts are addressed by transaction hash, so the order of the
    // persisted list is not itself part of the commitment.
    let mut reordered = snapshot.clone();
    reordered.receipts.swap(0, 2);
    assert!(validate_chain(&reordered.blocks, &reordered.receipts, 1, &state).is_ok());

    // Rewriting the cumulative figure is caught even when gas_used is right.
    let mut regassed = snapshot.clone();
    regassed.receipts[1].cumulative_gas_used += 1;
    assert!(validate_chain(&regassed.blocks, &regassed.receipts, 1, &state).is_err());

    // Reassigning a transaction index is caught.
    let mut remapped = snapshot.clone();
    remapped.receipts[0].transaction_index = 1;
    assert!(validate_chain(&remapped.blocks, &remapped.receipts, 1, &state).is_err());

    // Duplicating a transaction inside one block no longer matches tx_root.
    let duplicated_tx = snapshot.blocks[0].transactions[0].clone();
    let mut duplicated = snapshot.clone();
    duplicated.blocks[0].transactions.push(duplicated_tx);
    assert!(validate_chain(&duplicated.blocks, &duplicated.receipts, 1, &state).is_err());
}

#[test]
fn an_unsigned_transaction_is_rejected_even_with_honest_commitments() {
    // `build` commits whatever it is given, so the only check left that can
    // reject this block is the signature rule in `validate_chain`.
    let mut unsigned = transfer(0);
    unsigned.signature = TransactionSignature::EMPTY;
    let (snapshot, state) = build(vec![vec![unsigned]]);
    assert!(matches!(
        validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &state),
        Err(StorageError::Invalid(_))
    ));
}

#[test]
fn round_trip_restores_state_blocks_and_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert!(!store.exists());
    assert_eq!(store.load().unwrap().height(), 0);

    let (snapshot, state) = ledger(3);
    store.persist(&snapshot).unwrap();
    assert!(store.exists());
    assert!(segmented::manifest_path(dir.path()).is_file());
    assert!(segment_path(dir.path(), 0).is_file());
    assert!(snapshot_path(dir.path(), 3).is_file());

    let restored = store.load().unwrap();
    assert_eq!(restored.height(), 3);
    assert_eq!(restored.blocks.len(), 3);
    assert_eq!(restored.receipts.len(), 3);
    assert_eq!(restored.state.root(), state.root());
    assert_eq!(restored.state.balance(&Address::new([2; 20])), 30);
}

#[test]
fn ephemeral_store_validates_but_never_writes() {
    let store = LedgerStore::ephemeral(1).unwrap();
    let (snapshot, _) = ledger(1);
    store.persist(&snapshot).unwrap();
    assert!(!store.exists());
    assert!(store.path().is_none());
    assert_eq!(store.load().unwrap().height(), 0);

    // Validation still applies to an ephemeral store.
    let mut foreign = snapshot;
    foreign.chain_id = 9;
    assert!(matches!(
        store.persist(&foreign),
        Err(StorageError::ChainMismatch { expected: 1, got: 9 })
    ));
}

#[test]
fn zero_chain_id_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(LedgerStore::open(0, dir.path()), Err(StorageError::InvalidChainId)));
    assert!(matches!(LedgerStore::ephemeral(0), Err(StorageError::InvalidChainId)));
}

#[test]
fn legacy_version_two_file_is_read_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(2);
    let path = dir.path().join(LEDGER_FILE_NAME);
    let original = legacy::encode(&snapshot);
    fs::write(&path, &original).unwrap();
    let store = open(dir.path());
    assert!(store.exists());
    assert!(store.load().is_ok());

    // Flip the last payload byte and the checksum must catch it.
    let mut corrupted = original.clone();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0x01;
    fs::write(&path, &corrupted).unwrap();
    assert!(store.load().is_err());

    // Corrupt the magic prefix.
    let mut bad_magic = original.clone();
    bad_magic[0] ^= 0xff;
    fs::write(&path, &bad_magic).unwrap();
    assert!(matches!(store.load(), Err(StorageError::UnsupportedFormat)));

    // Truncation must fail to decode rather than load a partial ledger.
    fs::write(&path, &original[..original.len() / 2]).unwrap();
    assert!(store.load().is_err());

    fs::write(&path, &original).unwrap();
    assert!(store.load().is_ok(), "the untouched file must still load");
}

#[test]
fn ledger_from_another_chain_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    open(dir.path()).persist(&ledger(1).0).unwrap();
    let other_chain = LedgerStore::open(2, dir.path()).unwrap();
    assert!(matches!(other_chain.load(), Err(StorageError::ChainMismatch { expected: 2, got: 1 })));
}

#[test]
fn validator_rejects_tampered_history() {
    let (snapshot, state) = ledger(2);

    // Renumbered height breaks the contiguous sequence.
    let mut renumbered = snapshot.clone();
    renumbered.blocks[1].header.height = 5;
    assert!(validate_chain(&renumbered.blocks, &renumbered.receipts, 1, &state).is_err());

    // Rewritten parent breaks the chain link.
    let mut reparented = snapshot.clone();
    reparented.blocks[1].header.parent_hash = Hash::ZERO;
    assert!(validate_chain(&reparented.blocks, &reparented.receipts, 1, &state).is_err());

    // Swapped transaction no longer matches the committed tx_root.
    let mut retx = snapshot.clone();
    retx.blocks[1].transactions = vec![transfer(99)];
    assert!(validate_chain(&retx.blocks, &retx.receipts, 1, &state).is_err());

    // Gas above the block limit.
    let mut overspent = snapshot.clone();
    overspent.blocks[1].header.gas_used = BLOCK_GAS_LIMIT + 1;
    assert!(validate_chain(&overspent.blocks, &overspent.receipts, 1, &state).is_err());

    // Missing receipt.
    let mut short = snapshot.clone();
    short.receipts.pop();
    assert!(validate_chain(&short.blocks, &short.receipts, 1, &state).is_err());

    // Duplicate receipt for one block.
    let mut duplicated = snapshot.clone();
    duplicated.receipts[1] = duplicated.receipts[0].clone();
    assert!(validate_chain(&duplicated.blocks, &duplicated.receipts, 1, &state).is_err());

    // Receipt claiming a different gas figure.
    let mut regassed = snapshot.clone();
    regassed.receipts[1].gas_used += 1;
    assert!(validate_chain(&regassed.blocks, &regassed.receipts, 1, &state).is_err());

    // A failed receipt cannot appear in a committed block.
    let mut failed = snapshot.clone();
    failed.receipts[1].status = ReceiptStatus::Failure;
    assert!(validate_chain(&failed.blocks, &failed.receipts, 1, &state).is_err());

    // State that does not match the newest state root.
    let mut divergent = State::with_chain_id(1);
    divergent.credit(Address::new([7; 20]), 1).unwrap();
    assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &divergent).is_err());

    // The untouched ledger still validates.
    assert!(validate_chain(&snapshot.blocks, &snapshot.receipts, 1, &state).is_ok());
}

#[test]
fn empty_history_is_valid() {
    let state = State::with_chain_id(1);
    assert!(validate_chain(&[], &[], 1, &state).is_ok());
}

#[test]
fn persist_keeps_the_current_and_previous_snapshot_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // Shrink the interval so each of the three heads writes a snapshot.
    for height in [1, 3, 5] {
        store.persist_with_limits(&ledger(height).0, segmented::SEGMENT_MAX_BYTES, 2).unwrap();
    }

    assert_eq!(store.load().unwrap().height(), 5);
    assert_eq!(
        ledger_dir_names(dir.path()),
        vec![
            String::from("manifest.json"),
            String::from("segments"),
            String::from("snapshot-0000000003.bin"),
            String::from("snapshot-0000000005.bin"),
        ]
    );
    // No temporary files may be left behind.
    let leftovers: Vec<_> = fs::read_dir(dir.path().join(LEDGER_DIR_NAME))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "unexpected temporaries: {leftovers:?}");
}

#[test]
fn snapshot_interval_defers_the_snapshot_and_replays_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let interval = 4;

    store.persist_with_limits(&ledger(1).0, segmented::SEGMENT_MAX_BYTES, interval).unwrap();
    assert_eq!(manifest_value(dir.path())["snapshot"]["height"], 1);

    // Below the next boundary: the height-1 snapshot is reused, not rewritten.
    store.persist_with_limits(&ledger(4).0, segmented::SEGMENT_MAX_BYTES, interval).unwrap();
    let manifest = manifest_value(dir.path());
    assert_eq!(manifest["snapshot"]["height"], 1);
    assert_eq!(manifest["head_height"], 4);
    assert!(!snapshot_path(dir.path(), 4).exists());
    assert!(snapshot_path(dir.path(), 1).is_file());

    // Loading replays blocks 2..=4 on top of the height-1 snapshot.
    let restored = store.load().unwrap();
    assert_eq!(restored.height(), 4);
    assert_eq!(restored.receipts.len(), 4);
    assert_eq!(restored.state.balance(&Address::new([2; 20])), 40);

    // The next block lands exactly on the interval boundary.
    store.persist_with_limits(&ledger(5).0, segmented::SEGMENT_MAX_BYTES, interval).unwrap();
    let manifest = manifest_value(dir.path());
    assert_eq!(manifest["snapshot"]["height"], 5);
    assert!(snapshot_path(dir.path(), 5).is_file());
    // The superseded snapshot is kept until the next commit, so a manifest that
    // was never replaced still points at live data.
    assert!(snapshot_path(dir.path(), 1).is_file());

    store.persist_with_limits(&ledger(6).0, segmented::SEGMENT_MAX_BYTES, interval).unwrap();
    assert!(!snapshot_path(dir.path(), 1).exists(), "the superseded snapshot must be pruned");
    assert!(snapshot_path(dir.path(), 5).is_file());
    assert_eq!(store.load().unwrap().height(), 6);
}

#[test]
fn snapshot_state_root_must_match_the_block_at_its_height() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist_with_limits(&ledger(1).0, segmented::SEGMENT_MAX_BYTES, 4).unwrap();

    // A snapshot that is internally consistent (correct checksum and its own
    // state root) but does not describe the state after block 1.
    let mut state = State::from_snapshot(ledger(1).0.state).unwrap();
    state.credit(Address::new([9; 20]), 1).unwrap();
    let doctored = state.snapshot();
    let checksum = segmented::overwrite_snapshot(dir.path(), 1, &doctored).unwrap();

    let mut manifest = manifest_value(dir.path());
    manifest["snapshot"]["state_root"] = serde_json::json!(state.root().to_string());
    manifest["snapshot"]["checksum"] = serde_json::json!(checksum.to_string());
    write_manifest_value(dir.path(), &manifest);

    let error = store.load().unwrap_err();
    assert!(matches!(error, StorageError::Snapshot(_)), "unexpected error: {error:?}");
    assert!(error.to_string().contains("does not match block 1"), "{error}");
}

#[test]
fn snapshot_height_ahead_of_the_head_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist_with_limits(&ledger(1).0, segmented::SEGMENT_MAX_BYTES, 4).unwrap();

    let mut manifest = manifest_value(dir.path());
    manifest["snapshot"]["height"] = serde_json::json!(2);
    manifest["snapshot"]["file"] = serde_json::json!("snapshot-0000000002.bin");
    write_manifest_value(dir.path(), &manifest);

    let error = store.load().unwrap_err();
    assert!(matches!(error, StorageError::Manifest(_)), "unexpected error: {error:?}");
    assert!(error.to_string().contains("ahead of the head"), "{error}");
}

#[test]
fn oversized_legacy_file_is_refused_on_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let path = dir.path().join(LEDGER_FILE_NAME);
    let file = fs::File::create(&path).unwrap();
    file.set_len(MAX_LEDGER_BYTES + 1).unwrap();
    assert!(matches!(store.load(), Err(StorageError::TooLarge { .. })));
}

#[test]
fn empty_history_with_funded_state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let address = Address::new([3; 20]);
    let mut state = State::with_chain_id(1);
    state.credit(address, 500).unwrap();
    let snapshot = LedgerSnapshot {
        chain_id: 1,
        state: state.snapshot(),
        blocks: Vec::new(),
        receipts: Vec::new(),
    };
    store.persist(&snapshot).unwrap();
    assert!(store.exists());
    assert!(segmented::manifest_path(dir.path()).is_file());

    let restored = store.load().unwrap();
    assert_eq!(restored.height(), 0);
    assert_eq!(restored.state.balance(&address), 500);
}

#[test]
fn persist_appends_only_the_new_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let before = fs::read(segment_path(dir.path(), 0)).unwrap();

    store.persist(&ledger(3).0).unwrap();
    let after = fs::read(segment_path(dir.path(), 0)).unwrap();
    assert!(after.starts_with(&before), "committed bytes must never be rewritten");
    assert!(after.len() > before.len());

    let manifest = manifest_value(dir.path());
    assert_eq!(manifest["segments"][0]["records"], 3);
    assert_eq!(store.load().unwrap().height(), 3);
}

#[test]
fn segments_roll_over_at_the_target_size() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let (snapshot, _) = ledger(3);
    // Any positive record is larger than this target, so every record rolls.
    store.persist_with_segment_limit(&snapshot, 1).unwrap();

    let manifest = manifest_value(dir.path());
    let segments = manifest["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 3);
    for (index, segment) in segments.iter().enumerate() {
        assert_eq!(segment["records"], 1);
        assert_eq!(segment["file"], format!("{index:06}.log"));
        assert!(segment_path(dir.path(), u64::try_from(index).unwrap()).is_file());
    }
    assert_eq!(store.load().unwrap().height(), 3);
}

#[test]
fn uncommitted_tail_is_ignored_and_truncated_before_the_next_append() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();
    let segment = segment_path(dir.path(), 0);
    let committed = fs::read(&segment).unwrap().len();

    let mut file = fs::OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(&[0xab; 37]).unwrap();
    drop(file);

    assert_eq!(store.load().unwrap().height(), 2, "a torn tail must not block start-up");

    store.persist(&ledger(3).0).unwrap();
    assert_eq!(store.load().unwrap().height(), 3);
    assert_eq!(
        u64::try_from(fs::read(&segment).unwrap().len()).unwrap(),
        manifest_value(dir.path())["segments"][0]["bytes"].as_u64().unwrap()
    );
    assert!(fs::read(&segment).unwrap().len() > committed);
}

#[test]
fn committed_record_corruption_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();
    let segment = segment_path(dir.path(), 0);
    let mut bytes = fs::read(&segment).unwrap();
    bytes[100] ^= 0x01;
    fs::write(&segment, &bytes).unwrap();
    assert!(matches!(store.load(), Err(StorageError::ChecksumMismatch)));
}

#[test]
fn missing_segment_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();
    fs::remove_file(segment_path(dir.path(), 0)).unwrap();
    assert!(matches!(store.load(), Err(StorageError::Io { .. })));
}

#[test]
fn truncated_committed_segment_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();
    let segment = segment_path(dir.path(), 0);
    let file = fs::OpenOptions::new().write(true).open(&segment).unwrap();
    let len = file.metadata().unwrap().len();
    file.set_len(len - 1).unwrap();
    assert!(matches!(store.load(), Err(StorageError::Segment(_))));
}

#[test]
fn stale_manifest_segment_length_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();
    let mut manifest = manifest_value(dir.path());
    let bytes = manifest["segments"][0]["bytes"].as_u64().unwrap();
    manifest["segments"][0]["bytes"] = serde_json::json!(bytes + 10);
    write_manifest_value(dir.path(), &manifest);
    assert!(matches!(store.load(), Err(StorageError::Segment(_))));
}

#[test]
fn manifest_with_a_traversal_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let mut manifest = manifest_value(dir.path());
    manifest["segments"][0]["file"] = serde_json::json!("../../../etc/passwd");
    write_manifest_value(dir.path(), &manifest);
    assert!(matches!(store.load(), Err(StorageError::Manifest(_))));
}

#[test]
fn snapshot_corruption_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let snapshot = snapshot_path(dir.path(), 1);
    let mut bytes = fs::read(&snapshot).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&snapshot, &bytes).unwrap();
    assert!(matches!(store.load(), Err(StorageError::ChecksumMismatch)));
}

#[test]
fn manifest_snapshot_checksum_must_match_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let mut manifest = manifest_value(dir.path());
    manifest["snapshot"]["checksum"] = serde_json::json!(Hash::ZERO.to_string());
    write_manifest_value(dir.path(), &manifest);
    assert!(matches!(store.load(), Err(StorageError::ChecksumMismatch)));
}

#[test]
fn crash_before_the_manifest_commit_keeps_the_previous_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let old_manifest = fs::read(segmented::manifest_path(dir.path())).unwrap();

    store.persist(&ledger(3).0).unwrap();
    assert_eq!(store.load().unwrap().height(), 3);

    // Simulate a crash between the snapshot rename and the manifest commit: the
    // previously committed manifest is still on disk and still has its snapshot.
    fs::write(segmented::manifest_path(dir.path()), &old_manifest).unwrap();
    assert!(snapshot_path(dir.path(), 1).is_file());
    let recovered = store.load().unwrap();
    assert_eq!(recovered.height(), 1);
    assert_eq!(recovered.blocks.len(), 1);

    // Replaying the lost blocks rebuilds the same head.
    store.persist(&ledger(3).0).unwrap();
    assert_eq!(store.load().unwrap().height(), 3);
}

#[test]
fn migrates_a_version_two_ledger_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(2);
    let legacy_path = dir.path().join(LEDGER_FILE_NAME);
    fs::write(&legacy_path, legacy::encode(&snapshot)).unwrap();

    let store = open(dir.path());
    assert!(store.exists());
    assert_eq!(store.load().unwrap().height(), 2);

    store.persist(&snapshot).unwrap();
    assert!(segmented::manifest_path(dir.path()).is_file());
    assert!(!legacy_path.exists(), "the legacy file must not be loaded once a manifest exists");
    assert!(dir.path().join(LEDGER_BACKUP_FILE_NAME).is_file());
    assert_eq!(store.load().unwrap().height(), 2);

    // A second persist is a no-op append, not a second migration.
    store.persist(&snapshot).unwrap();
    assert_eq!(store.load().unwrap().height(), 2);
    assert_eq!(manifest_value(dir.path())["segments"][0]["records"], 2);
}

#[test]
fn non_append_persist_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(3).0).unwrap();

    // Same length, different first block: the head no longer matches.
    let (forked, _) = build(vec![
        vec![transfer(0), transfer(1)],
        vec![transfer(2)],
        vec![transfer(3), transfer(4)],
    ]);
    assert!(matches!(store.persist(&forked), Err(StorageError::NonAppendPersist)));

    // Shorter history would roll the head back.
    assert!(matches!(store.persist(&ledger(1).0), Err(StorageError::NonAppendPersist)));

    assert_eq!(store.load().unwrap().height(), 3);
}

#[test]
fn segment_header_must_carry_the_configured_chain() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(1).0).unwrap();
    let segment = segment_path(dir.path(), 0);
    let mut bytes = fs::read(&segment).unwrap();
    // The header is magic(8) || version(4) || chain_id(8) || first_height(8).
    bytes[12..20].copy_from_slice(&99u64.to_le_bytes());
    fs::write(&segment, &bytes).unwrap();
    assert!(matches!(store.load(), Err(StorageError::ChainMismatch { expected: 1, got: 99 })));
}

#[test]
fn migrate_converts_a_version_two_file_and_verifies_the_replay() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, state) = ledger(3);
    let legacy_path = dir.path().join(LEDGER_FILE_NAME);
    fs::write(&legacy_path, legacy::encode(&snapshot)).unwrap();

    let store = open(dir.path());
    assert_eq!(store.migrate().unwrap(), MigrationOutcome::Migrated { height: 3 });
    assert!(segmented::manifest_path(dir.path()).is_file());
    assert!(!legacy_path.exists());
    assert!(dir.path().join(LEDGER_BACKUP_FILE_NAME).is_file());

    let restored = store.load().unwrap();
    assert_eq!(restored.height(), 3);
    assert_eq!(restored.state.root(), state.root());
    assert_eq!(restored.receipts.len(), 3);
}

#[test]
fn migrate_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(2);
    fs::write(dir.path().join(LEDGER_FILE_NAME), legacy::encode(&snapshot)).unwrap();

    let store = open(dir.path());
    assert_eq!(store.migrate().unwrap(), MigrationOutcome::Migrated { height: 2 });
    assert_eq!(store.migrate().unwrap(), MigrationOutcome::AlreadySegmented);
    assert_eq!(store.load().unwrap().height(), 2);
    assert_eq!(manifest_value(dir.path())["segments"][0]["records"], 2);
}

#[test]
fn migrate_reports_a_fresh_directory_as_nothing_to_migrate() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert_eq!(store.migrate().unwrap(), MigrationOutcome::NothingToMigrate);
    assert!(!segmented::manifest_path(dir.path()).exists());
    assert!(!store.exists());
}

#[test]
fn migrate_refuses_a_corrupt_legacy_file_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(2);
    let mut bytes = legacy::encode(&snapshot);
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let legacy_path = dir.path().join(LEDGER_FILE_NAME);
    fs::write(&legacy_path, &bytes).unwrap();

    let store = open(dir.path());
    assert!(matches!(store.migrate(), Err(StorageError::ChecksumMismatch)));
    assert!(!segmented::manifest_path(dir.path()).exists(), "a failed migration must not commit");
    assert!(legacy_path.is_file(), "the legacy file must survive a failed migration");
}

#[test]
fn migrate_refuses_an_unknown_legacy_version() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(1);
    let mut bytes = legacy::encode(&snapshot);
    // magic(8) || version(u32 LE) starts at byte 8.
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    fs::write(dir.path().join(LEDGER_FILE_NAME), &bytes).unwrap();

    let store = open(dir.path());
    assert!(matches!(store.migrate(), Err(StorageError::UnsupportedVersion(1))));
    assert!(!segmented::manifest_path(dir.path()).exists());
}

#[test]
fn migrate_recovers_from_a_partial_previous_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _) = ledger(2);
    fs::write(dir.path().join(LEDGER_FILE_NAME), legacy::encode(&snapshot)).unwrap();

    // Leftovers from a run that died before the manifest commit.
    let ledger = dir.path().join(LEDGER_DIR_NAME);
    fs::create_dir_all(ledger.join("segments")).unwrap();
    fs::write(ledger.join("snapshot-0000000009.bin"), b"partial").unwrap();
    fs::write(ledger.join("segments").join("000000.log"), b"partial").unwrap();

    let store = open(dir.path());
    assert_eq!(store.migrate().unwrap(), MigrationOutcome::Migrated { height: 2 });
    assert_eq!(store.load().unwrap().height(), 2);
    assert!(!ledger.join("snapshot-0000000009.bin").exists());
}

#[test]
fn ephemeral_store_cannot_be_migrated() {
    let store = LedgerStore::ephemeral(1).unwrap();
    assert!(matches!(store.migrate(), Err(StorageError::Invalid(_))));
}

/// Persist one block at a time so the segment limit controls the file count.
fn persist_each_block(store: &LedgerStore, height: u64, segment_bytes: u64, interval: u64) {
    for prefix in 1..=height {
        let (snapshot, _) = ledger(prefix);
        store.persist_with_limits(&snapshot, segment_bytes, interval).unwrap();
    }
}

fn segment_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.join(LEDGER_DIR_NAME).join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".log"))
        .collect();
    names.sort();
    names
}

#[test]
fn prune_drops_the_oldest_segments_and_loads_the_retained_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // One segment per block, and a snapshot at every height.
    persist_each_block(&store, 6, 1, 1);
    assert_eq!(segment_names(dir.path()).len(), 6);

    let outcome = store.prune(2).unwrap();
    assert_eq!(
        outcome,
        PruneOutcome::Pruned { dropped_segments: 4, retained_segments: 2, base_height: 4 }
    );
    let names = segment_names(dir.path());
    assert_eq!(names, vec!["000004.log", "000005.log"]);
    assert!(snapshot_path(dir.path(), 6).is_file(), "the anchor snapshot must survive");

    let value = manifest_value(dir.path());
    assert_eq!(value["version"], 2);
    assert_eq!(value["base_height"], 4);
    assert_eq!(value["head_height"], 6);

    let restored = store.load().unwrap();
    assert_eq!(restored.base_height, 4);
    assert_eq!(restored.height(), 6);
    assert_eq!(restored.block_count(), 2);
    assert_eq!(restored.blocks[0].header.height, 5);
    assert_eq!(restored.receipts.len(), 2);
    assert!(restored.receipts.iter().all(|receipt| receipt.block_height >= 5));
}

#[test]
fn prune_replays_the_trailing_blocks_on_top_of_a_lagging_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // Snapshots land at height 1 and then 5; the head is 6.
    persist_each_block(&store, 6, 1, 4);
    let value = manifest_value(dir.path());
    assert_eq!(value["snapshot"]["height"], 5);

    let outcome = store.prune(2).unwrap();
    assert_eq!(
        outcome,
        PruneOutcome::Pruned { dropped_segments: 4, retained_segments: 2, base_height: 4 }
    );

    let restored = store.load().unwrap();
    assert_eq!(restored.base_height, 4);
    assert_eq!(restored.height(), 6);
    assert_eq!(restored.blocks.iter().map(|b| b.header.height).collect::<Vec<_>>(), vec![5, 6]);
}

#[test]
fn prune_refuses_to_drop_the_block_the_snapshot_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // A large interval keeps the only snapshot at height 1.
    persist_each_block(&store, 5, 1, 100);
    let before = manifest_value(dir.path());

    let error = store.prune(1).unwrap_err();
    assert!(matches!(
        error,
        StorageError::PruneWouldDropSnapshot { first_retained_height: 5, snapshot_height: 1 }
    ));
    assert_eq!(manifest_value(dir.path()), before, "a refused prune must not write");
    assert_eq!(segment_names(dir.path()).len(), 5, "no segment is deleted on refusal");
}

#[test]
fn prune_is_a_no_op_when_the_ledger_already_keeps_that_many_segments() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 3, 1, 1);
    let before = manifest_value(dir.path());

    assert_eq!(store.prune(3).unwrap(), PruneOutcome::NothingToPrune { segments: 3 });
    assert_eq!(store.prune(10).unwrap(), PruneOutcome::NothingToPrune { segments: 3 });
    assert_eq!(manifest_value(dir.path()), before);
}

#[test]
fn prune_requires_at_least_one_segment() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 2, 1, 1);
    assert!(matches!(store.prune(0), Err(StorageError::Invalid(_))));
    assert_eq!(segment_names(dir.path()).len(), 2);

    let ephemeral = LedgerStore::ephemeral(1).unwrap();
    assert!(matches!(ephemeral.prune(1), Err(StorageError::Invalid(_))));

    let empty_dir = tempfile::tempdir().unwrap();
    let empty = open(empty_dir.path());
    assert!(matches!(empty.prune(2), Err(StorageError::Invalid(_))));
}

#[test]
fn persist_continues_from_the_retained_base_after_a_prune() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 4, 1, 1);
    store.prune(2).unwrap();
    let restored = store.load().unwrap();
    assert_eq!(restored.base_height, 2);

    // The node now holds blocks 3 and 4 only; it produces block 5.
    let (full, _) = ledger(5);
    let suffix = LedgerSnapshot {
        chain_id: 1,
        state: full.state,
        blocks: full.blocks[2..].to_vec(),
        receipts: full.receipts.into_iter().filter(|r| r.block_height >= 3).collect(),
    };
    store.persist(&suffix).unwrap();

    let after = store.load().unwrap();
    assert_eq!(after.base_height, 2);
    assert_eq!(after.height(), 5);
    assert_eq!(after.blocks[0].header.height, 3);
    assert_eq!(after.blocks.len(), 3);
    let value = manifest_value(dir.path());
    assert_eq!(value["base_height"], 2);
    assert_eq!(
        value["segments"].as_array().unwrap().len(),
        2,
        "block 5 appends to the last segment"
    );
}

#[test]
fn a_pruned_store_rejects_a_full_history_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 4, 1, 1);
    store.prune(2).unwrap();

    // A caller that kept every block in memory must reload the suffix first.
    let (full, _) = ledger(5);
    assert!(matches!(store.persist(&full), Err(StorageError::Invalid(_))));
    assert_eq!(store.load().unwrap().height(), 4);
}

#[test]
fn a_version_one_manifest_still_loads_as_an_unpruned_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();

    let mut value = manifest_value(dir.path());
    value["version"] = serde_json::json!(1);
    value.as_object_mut().unwrap().remove("base_height");
    write_manifest_value(dir.path(), &value);

    let restored = store.load().unwrap();
    assert_eq!(restored.base_height, 0);
    assert_eq!(restored.height(), 2);
}

#[test]
fn a_version_one_manifest_cannot_claim_pruned_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.persist(&ledger(2).0).unwrap();

    let mut value = manifest_value(dir.path());
    value["version"] = serde_json::json!(1);
    value["base_height"] = serde_json::json!(1);
    write_manifest_value(dir.path(), &value);
    assert!(matches!(store.load(), Err(StorageError::Manifest(_))));
}

#[test]
fn prune_keeps_the_snapshot_root_check_for_the_retained_block() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 5, 1, 1);
    store.prune(2).unwrap();
    assert_eq!(store.load().unwrap().base_height, 3);

    // Replace the snapshot with one from a different state, and keep the
    // manifest checksum consistent with it: the root must still match block 5.
    let (other, _) = build(vec![vec![transfer(0)]]);
    let checksum = segmented::overwrite_snapshot(dir.path(), 5, &other.state).unwrap();
    let mut value = manifest_value(dir.path());
    value["snapshot"]["checksum"] = serde_json::json!(checksum.to_string());
    write_manifest_value(dir.path(), &value);

    assert!(matches!(store.load(), Err(StorageError::Snapshot(_))));
}

#[test]
fn a_manifest_with_out_of_order_segments_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 3, 1, 1);

    let mut value = manifest_value(dir.path());
    let segments = value["segments"].as_array_mut().unwrap();
    segments.swap(1, 2);
    write_manifest_value(dir.path(), &value);
    assert!(matches!(store.load(), Err(StorageError::Manifest(_))));
}

#[test]
fn a_pruned_segment_corruption_is_still_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    persist_each_block(&store, 5, 1, 1);
    store.prune(2).unwrap();

    let path = segment_path(dir.path(), 4);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(store.load(), Err(StorageError::ChecksumMismatch)));
}
