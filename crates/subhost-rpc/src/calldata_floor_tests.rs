//! Q-037 L4 contract tests: calldata floor activation wiring.
//!
//! These bind the six production insertion points to the endorsed contract:
//! the header-version switch, the admission floor, the selection lane break,
//! the acceptance floor before any state transition, and the restore filter
//! order (mined nonce, floor, budget, admission) with the observation-only
//! `floor_dropped` / `cap_dropped` counter split. The accept-path coverage
//! matrix mirrors the SUBHOST-2026-003 re-probe: the protocol cap is strictly
//! greater-than, so a transaction exactly at [`MAX_TX_DATA_BYTES`] dies at the
//! intrinsic floor when under-priced and is accepted when validly priced,
//! while the 16 KiB mempool cap stays local and never binds acceptance.

use crate::methods::DEFAULT_TX_GAS_LIMIT;
use crate::state::{read, write, RpcError, RpcState};
use crate::test_support::{signed_transfer, signing_key};
use ed25519_dalek::{Signer, SigningKey};
use std::io::Write;
use std::sync::{Arc, Mutex};
use subhost_core::{
    tx_root_of, Address, Block, BlockHeader, CalldataPolicy, FeePolicy, Gas, Hash, Nonce,
    Transaction, TransactionSignature, BLOCK_GAS_LIMIT, CALLDATA_HEADER_VERSION,
    CALLDATA_POLICY_VERSION, FEE_CHANGE_DENOMINATOR, FEE_ELASTICITY_MULTIPLIER, FEE_HEADER_VERSION,
    MAX_TX_DATA_BYTES,
};
use subhost_mempool::{Mempool, MempoolConfig, MempoolStore};
use subhost_storage::{receipt_commitment, receipts_root};
use tracing_subscriber::layer::{Layer, SubscriberExt};

/// The approved public-testnet schedule: 16 gas per byte. 16 * 128 KiB is
/// 2,097,152, well inside the 30M block gas limit, so a maximum-size
/// transaction is still includable.
fn calldata_policy() -> CalldataPolicy {
    CalldataPolicy { version: CALLDATA_POLICY_VERSION, gas_per_byte: 16 }
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

fn resign(mut transaction: Transaction, key: &SigningKey) -> Transaction {
    transaction.signature =
        TransactionSignature::from_ed25519(&key.sign(&transaction.signing_payload()).to_bytes());
    assert!(transaction.verify_signature());
    transaction
}

/// A signed transfer carrying `data` and an explicit `gas_limit`, so it can be
/// priced against, or fall below, the intrinsic calldata floor.
fn data_transfer(
    key: &SigningKey,
    nonce: Nonce,
    value: u128,
    data: Vec<u8>,
    gas_limit: Gas,
) -> Transaction {
    let mut transaction = signed_transfer(key, nonce, value);
    transaction.data = data;
    transaction.gas_limit = gas_limit;
    resign(transaction, key)
}

fn calldata_state(key: &SigningKey, balance: u128) -> RpcState {
    let state = RpcState::in_memory(1).unwrap().with_calldata_policy(calldata_policy()).unwrap();
    let address = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(address, balance)]).unwrap();
    state
}

/// A well-formed version-3 block around `tx`, with commitments computed by
/// executing it on a clone of the state. The floor is not enforced here, so
/// this can carry a floor-invalid transaction and prove acceptance refuses it
/// on the floor itself rather than on a commitment mismatch.
fn v3_block_around(state: &RpcState, transaction: Transaction) -> Block {
    let mut candidate = read(&state.state).clone();
    candidate.apply_transaction(&transaction).unwrap();
    let height = state.block_height() + 1;
    let parent_hash = read(&state.blocks).last().map_or(Hash::ZERO, |block| block.hash());
    let gas_used = transaction.gas_limit;
    let commitments =
        vec![receipt_commitment(transaction.hash(), height, 0, transaction.gas_limit, gas_used)];
    Block {
        header: BlockHeader {
            version: CALLDATA_HEADER_VERSION,
            chain_id: 1,
            height,
            timestamp: subhost_core::unix_timestamp(),
            parent_hash,
            state_root: candidate.root(),
            tx_root: tx_root_of(&[transaction.hash()]),
            receipt_root: receipts_root(&commitments),
            validator: Address::ZERO,
            gas_used,
            gas_limit: state.block_gas_limit(),
            extra_data: Vec::new(),
            base_fee_per_gas: 0,
        },
        transactions: vec![transaction],
        signatures: Vec::new(),
    }
}

// --- Header version switch (production and acceptance) ---

#[test]
fn header_version_matrix_covers_legacy_fee_calldata_and_joint() {
    let key = signing_key(61);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());

    // Legacy: no policy, version 1, base fee 0.
    let legacy = RpcState::in_memory(1).unwrap();
    legacy.seed_accounts([(sender, 1_000_000)]).unwrap();
    legacy.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
    let block = read(&legacy.blocks).last().cloned().unwrap();
    assert_eq!(block.header.version, 1);
    assert_eq!(block.header.base_fee_per_gas, 0);

    // Fee-only: version 2, initial base fee 8, empty blocks still seal.
    let fee = RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap();
    let block = fee.build_block().unwrap().expect("a fee chain seals empty blocks");
    assert_eq!(block.header.version, FEE_HEADER_VERSION);
    assert_eq!(block.header.base_fee_per_gas, 8);

    // Calldata-only: version 3 with no fee policy, base fee 0. Empty selections
    // are skipped exactly like legacy (empty blocks stay fee-keyed), so a
    // transaction is needed to seal.
    let calldata = calldata_state(&key, 1_000_000);
    assert!(calldata.build_block().unwrap().is_none(), "calldata-only chains skip empty blocks");
    let priced = data_transfer(&key, 0, 10, vec![0; 1024], DEFAULT_TX_GAS_LIMIT);
    calldata.submit_transaction(priced).unwrap();
    let block = read(&calldata.blocks).last().cloned().unwrap();
    assert_eq!(block.header.version, CALLDATA_HEADER_VERSION);
    assert_eq!(block.header.base_fee_per_gas, 0, "a calldata-only chain carries no fee market");

    // Joint fee + calldata: version 3 (the calldata bound wins the version
    // word) while the fee schedule still drives economics.
    let joint = RpcState::in_memory(1)
        .unwrap()
        .with_fee_policy(fee_policy())
        .unwrap()
        .with_calldata_policy(calldata_policy())
        .unwrap();
    let block = joint.build_block().unwrap().expect("a joint chain seals empty blocks");
    assert_eq!(block.header.version, CALLDATA_HEADER_VERSION);
    assert_eq!(block.header.base_fee_per_gas, 8);
}

#[test]
fn acceptance_version_pin_refuses_foreign_header_versions() {
    let key = signing_key(62);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());

    // A valid v3 block sealed on a calldata-bound producer.
    let producer = calldata_state(&key, 1_000_000);
    producer
        .submit_transaction(data_transfer(&key, 0, 10, vec![0; 1024], DEFAULT_TX_GAS_LIMIT))
        .unwrap();
    let v3 = read(&producer.blocks).last().cloned().unwrap();
    assert_eq!(v3.header.version, CALLDATA_HEADER_VERSION);

    // An unbound chain refuses the v3 header (it only produces/accepts v1).
    let unbound = RpcState::in_memory(1).unwrap();
    unbound.seed_accounts([(sender, 1_000_000)]).unwrap();
    assert!(matches!(unbound.accept_block(v3.clone()), Err(RpcError::BlockRejected(_))));
    assert_eq!(unbound.block_height(), 0);

    // A calldata-bound chain refuses a v2 header: the version word commits to
    // the byte floor, so a priced genesis never accepts version 2.
    let mut v2 = v3.clone();
    v2.header.version = FEE_HEADER_VERSION;
    let bound = calldata_state(&key, 1_000_000);
    assert!(matches!(bound.accept_block(v2), Err(RpcError::BlockRejected(_))));
    assert_eq!(bound.block_height(), 0);

    // The same v3 block is accepted by a identically-bound peer.
    let peer = calldata_state(&key, 1_000_000);
    peer.accept_block(v3).unwrap();
    assert_eq!(peer.block_height(), 1);
}

// --- Admission floor (submit_transaction) ---

#[test]
fn submit_refuses_under_intrinsic_calldata_before_the_pool() {
    let key = signing_key(63);
    let state = calldata_state(&key, 1_000_000);
    // 2048 bytes at 16 gas/byte needs 32,768 gas; the signed limit is 21,000.
    let cheap = data_transfer(&key, 0, 10, vec![0; 2048], DEFAULT_TX_GAS_LIMIT);
    let error = state.submit_transaction(cheap.clone()).unwrap_err();
    assert!(matches!(error, RpcError::TransactionRejected(_)), "{error:?}");
    assert!(error.to_string().contains("intrinsic calldata gas"), "{error}");
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.block_height(), 0, "a refused submission seals nothing");
    assert!(!state.mempool.read().unwrap().contains(&cheap.hash()));
}

#[test]
fn submit_refuses_over_protocol_cap_with_protocol_numbers() {
    let key = signing_key(64);
    let state = calldata_state(&key, 1_000_000_000);
    // One byte over the 128 KiB protocol maximum, with the gas floor met, so
    // the refusal is the cap itself. The floor runs before the mempool, so the
    // error names the protocol numbers (131073 / 131072), not the local 16 KiB.
    let len = MAX_TX_DATA_BYTES + 1;
    let gas_limit = 16 * len as u64;
    let oversized = data_transfer(&key, 0, 10, vec![0; len], gas_limit);
    let error = state.submit_transaction(oversized).unwrap_err();
    assert!(matches!(error, RpcError::TransactionRejected(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("protocol maximum"), "{text}");
    assert!(text.contains("131073"), "{text}");
    assert!(text.contains("131072"), "{text}");
    assert!(!text.contains("16384"), "the floor, not the local cap, spoke: {text}");
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.block_height(), 0);
}

#[test]
fn submit_admits_validly_priced_calldata_and_seals_v3() {
    let key = signing_key(65);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let state = calldata_state(&key, 1_000_000);
    // 1024 bytes needs 16,384 gas; 21,000 covers it and stays under the cap.
    let priced = data_transfer(&key, 0, 10, vec![0; 1024], DEFAULT_TX_GAS_LIMIT);
    let hash = state.submit_transaction(priced.clone()).unwrap();
    assert_eq!(hash, priced.hash());
    assert_eq!(state.block_height(), 1, "demo chains seal synchronously");
    assert_eq!(state.nonce(&sender), 1);
    assert!(state.receipt(&priced.hash()).is_some());
    assert_eq!(read(&state.blocks).last().unwrap().header.version, CALLDATA_HEADER_VERSION);
}

#[test]
fn submit_without_a_policy_keeps_legacy_calldata_admission() {
    let key = signing_key(66);
    let state = RpcState::in_memory(1).unwrap();
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    // 8 KiB of data with a 21,000 gas limit would be far below the intrinsic
    // floor, but no schedule is bound, so the floor is dormant and admission
    // is byte-for-byte the legacy behaviour (8 KiB is under the 16 KiB cap).
    let legacy = data_transfer(&key, 0, 10, vec![0; 8 * 1024], DEFAULT_TX_GAS_LIMIT);
    state.submit_transaction(legacy).unwrap();
    assert_eq!(state.block_height(), 1);
    assert_eq!(read(&state.blocks).last().unwrap().header.version, 1);
}

#[test]
fn submit_floor_binds_even_under_a_relaxed_mempool_cap() {
    let key = signing_key(67);
    let state = calldata_state(&key, 1_000_000);
    // A library caller relaxes the local cap to the protocol maximum. The
    // intrinsic floor is independent of MempoolConfig, so sub-cap unpriced
    // bytes still die at admission.
    *write(&state.mempool) =
        Mempool::new(MempoolConfig { max_tx_data_bytes: MAX_TX_DATA_BYTES, ..Default::default() });
    let cheap = data_transfer(&key, 0, 10, vec![0; 2048], DEFAULT_TX_GAS_LIMIT);
    let error = state.submit_transaction(cheap).unwrap_err();
    assert!(error.to_string().contains("intrinsic calldata gas"), "{error}");
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.block_height(), 0);
}

// --- Selection lane break (select_transactions) ---

#[test]
fn select_breaks_the_floor_invalid_lane_without_resurrection() {
    let cheap_key = signing_key(68);
    let good_key = signing_key(69);
    let cheap_sender = Address::from_public_key(&cheap_key.verifying_key().to_bytes());
    let good_sender = Address::from_public_key(&good_key.verifying_key().to_bytes());
    let state = RpcState::in_memory(1).unwrap().with_calldata_policy(calldata_policy()).unwrap();
    state.seed_accounts([(cheap_sender, 1_000_000), (good_sender, 1_000_000)]).unwrap();

    // Inject past the admission floor directly into the pool, as a snapshot
    // written before the schedule was bound could hold. The cheap lane's
    // nonce 0 is under-intrinsic; its nonce 1 is a valid empty transfer.
    let cheap0 = data_transfer(&cheap_key, 0, 10, vec![0; 2048], DEFAULT_TX_GAS_LIMIT);
    let cheap1 = signed_transfer(&cheap_key, 1, 10);
    let good0 = signed_transfer(&good_key, 0, 10);
    {
        let mut pool = write(&state.mempool);
        pool.add(cheap0.clone()).unwrap();
        pool.add(cheap1.clone()).unwrap();
        pool.add(good0.clone()).unwrap();
    }

    // Selection breaks the cheap lane at nonce 0 and never resurrects nonce 1;
    // the other sender's lane is unaffected.
    let block = state.build_block().unwrap().expect("the good lane still selects");
    assert_eq!(block.transactions, vec![good0.clone()]);
    assert_eq!(block.header.version, CALLDATA_HEADER_VERSION);
    // The break is not a drop: the cheap entries stay parked in the pool.
    // Scope the read guard so it is released before accept_block takes the
    // mempool write lock (a held read guard would deadlock the same thread).
    {
        let pool = read(&state.mempool);
        assert!(pool.contains(&cheap0.hash()));
        assert!(pool.contains(&cheap1.hash()));
    }

    // Mining the good lane does not unlock the cheap lane on the next build.
    state.accept_block(block).unwrap();
    assert!(state.build_block().unwrap().is_none(), "no resurrection of the blocked lane");
    {
        let pool = read(&state.mempool);
        assert!(pool.contains(&cheap0.hash()));
        assert!(pool.contains(&cheap1.hash()));
    }
    assert_eq!(state.nonce(&cheap_sender), 0);
    assert_eq!(state.nonce(&good_sender), 1);
}

// --- Acceptance floor (validate_candidate), before any state transition ---

#[test]
fn acceptance_refuses_floor_invalid_v3_block_before_any_state_transition() {
    let key = signing_key(70);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let state = calldata_state(&key, 1_000_000);
    let root = state.state_root();
    let tip = state.tip_hash();

    // An otherwise well-formed v3 block whose single transaction is
    // under-intrinsic. The floor must refuse it before execution or persist.
    let cheap = data_transfer(&key, 0, 10, vec![0; 2048], DEFAULT_TX_GAS_LIMIT);
    let block = v3_block_around(&state, cheap.clone());
    let error = state.accept_block(block).unwrap_err();
    assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("transaction 0 in block 1"), "{text}");
    assert!(text.contains("intrinsic calldata gas"), "{text}");

    // Nothing moved: tip, state root, nonce, balance and receipts are intact,
    // and no persist was attempted (the in-memory store wrote no new head).
    assert_eq!(state.block_height(), 0);
    assert_eq!(state.tip_hash(), tip);
    assert_eq!(state.state_root(), root);
    assert_eq!(state.nonce(&sender), 0);
    assert_eq!(state.balance(&sender), 1_000_000);
    assert!(state.receipt(&cheap.hash()).is_none());
}

#[test]
fn acceptance_refuses_over_protocol_cap_v3_block() {
    let key = signing_key(71);
    let state = calldata_state(&key, 1_000_000_000);
    let len = MAX_TX_DATA_BYTES + 1;
    let gas_limit = 16 * len as u64;
    let oversized = data_transfer(&key, 0, 10, vec![0; len], gas_limit);
    let block = v3_block_around(&state, oversized);
    let error = state.accept_block(block).unwrap_err();
    assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("protocol maximum"), "{text}");
    assert!(text.contains("131073"), "{text}");
    assert_eq!(state.block_height(), 0);
}

#[test]
fn exactly_at_protocol_cap_validly_priced_block_is_accepted() {
    // Nara's boundary nuance: 131,072 bytes is EXACTLY the protocol cap, and
    // the cap refusal is strictly greater-than. With the gas floor met this
    // transaction is consensus-valid and must be accepted even though it is
    // far over the 16 KiB mempool-local cap: acceptance never consults the
    // mempool config.
    let key = signing_key(72);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let gas_limit = 16 * MAX_TX_DATA_BYTES as u64;
    let at_cap = data_transfer(&key, 0, 10, vec![0; MAX_TX_DATA_BYTES], gas_limit);

    // The producer relaxes its local cap so the large transaction can be
    // submitted and sealed; the floor passes (exactly at cap, gas met).
    let producer = calldata_state(&key, 1_000_000_000);
    *write(&producer.mempool) =
        Mempool::new(MempoolConfig { max_tx_data_bytes: MAX_TX_DATA_BYTES, ..Default::default() });
    producer.submit_transaction(at_cap.clone()).unwrap();
    let block = read(&producer.blocks).last().cloned().unwrap();
    assert_eq!(block.header.version, CALLDATA_HEADER_VERSION);
    assert_eq!(block.transactions[0].data.len(), MAX_TX_DATA_BYTES);

    // A peer with the DEFAULT 16 KiB mempool accepts the 128 KiB block: the
    // large-but-validly-priced bytes are correct behaviour, not a bypass.
    let peer = calldata_state(&key, 1_000_000_000);
    assert_eq!(peer.mempool.read().unwrap().config().max_tx_data_bytes, 16 * 1024);
    peer.accept_block(block.clone()).unwrap();
    assert_eq!(peer.block_height(), 1);
    assert_eq!(peer.state_root(), block.header.state_root);
    assert_eq!(peer.nonce(&sender), 1);
    assert!(peer.receipt(&at_cap.hash()).is_some());
}

#[test]
fn accept_persist_round_trip_for_a_validly_priced_v3_block() {
    // Q-024 accept/persist pin: acceptance and the store's replay/persist floor
    // run the same pure check under the same bound schedule, so a block that
    // passes validate_candidate persists cleanly and reopens without a floor
    // failure.
    let key = signing_key(73);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let producer_dir = tempfile::tempdir().unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let priced = data_transfer(&key, 0, 10, vec![0; 1024], DEFAULT_TX_GAS_LIMIT);

    let producer = RpcState::with_data_dir(1, producer_dir.path())
        .unwrap()
        .with_calldata_policy(calldata_policy())
        .unwrap();
    producer.seed_accounts([(sender, 1_000_000)]).unwrap();
    producer.submit_transaction(priced.clone()).unwrap();
    let block = read(&producer.blocks).last().cloned().unwrap();
    assert_eq!(block.header.version, CALLDATA_HEADER_VERSION);

    let peer = RpcState::with_data_dir(1, peer_dir.path())
        .unwrap()
        .with_calldata_policy(calldata_policy())
        .unwrap();
    peer.seed_accounts([(sender, 1_000_000)]).unwrap();
    peer.accept_block(block.clone()).unwrap();
    assert_eq!(peer.block_height(), 1);
    assert_eq!(peer.state_root(), block.header.state_root);
    drop(peer);

    // Reopening replays the floor from the recorded schedule; what acceptance
    // accepted, persist and replay accept too.
    let reopened = RpcState::with_data_dir(1, peer_dir.path())
        .unwrap()
        .with_calldata_policy(calldata_policy())
        .expect("a v3 ledger is extendable under its own schedule");
    assert_eq!(reopened.block_height(), 1);
    assert_eq!(reopened.state_root(), block.header.state_root);
    assert_eq!(reopened.nonce(&sender), 1);
}

// --- Restore filter order and the observation-only counter split ---

#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Capture the restore log under a scoped DEBUG subscriber. The restore
/// counters are emitted at DEBUG, so this filter is more permissive than the
/// process-global INFO capture in `observability.rs`; it therefore cannot
/// cache an INFO callsite as `never`, and any DEBUG-only callsite it leaves
/// interested is still layer-filtered out under the global subscriber. Interest
/// is rebuilt inside the window exactly as `block_span_tests.rs` does.
fn capture_restore_log(f: impl FnOnce()) -> String {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(BufferWriter(buffer.clone()))
            .with_ansi(false)
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("subhost_rpc", tracing::Level::DEBUG)
                    .with_target(
                        "subhost_rpc::methods",
                        tracing_subscriber::filter::LevelFilter::OFF,
                    ),
            ),
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f();
    });
    let bytes = buffer.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn restore_pending_splits_floor_and_cap_drops_and_counts_them() {
    // Filter order: mined nonce, floor, budget, admission (cap, economics),
    // first match wins one reason. The floor drop and the cap drop are split
    // into dedicated counters; the split is observation-only, so the drop set
    // is identical to landed L0 (dewi's
    // `restore_pending_drops_entries_over_the_tighter_calldata_cap`).
    let dir = tempfile::tempdir().unwrap();
    let store = MempoolStore::open(1, Hash::ZERO, dir.path()).unwrap();
    let key = signing_key(74);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let state = RpcState::in_memory(1)
        .unwrap()
        .with_calldata_policy(calldata_policy())
        .unwrap()
        .with_pending_store(store.clone());
    state.seed_accounts([(sender, 1_000_000_000)]).unwrap();

    // floor_invalid: 2 KiB under-intrinsic (under the 16 KiB cap, so the floor
    //   is the only reason it can drop).
    // cap_invalid: 32 KiB with the gas floor met (over the 16 KiB local cap,
    //   floor-valid, so admission's cap is the reason).
    // restored: a plain valid empty transfer.
    let floor_invalid = data_transfer(&key, 0, 10, vec![0; 2048], DEFAULT_TX_GAS_LIMIT);
    let cap_invalid = data_transfer(&key, 1, 10, vec![0; 32 * 1024], 16 * 32 * 1024);
    let restored = signed_transfer(&key, 2, 10);

    // Persist through a relaxed-cap pool so the 32 KiB entry can be stored;
    // restore reads it back under the state's default 16 KiB pool.
    let mut old =
        Mempool::new(MempoolConfig { max_tx_data_bytes: MAX_TX_DATA_BYTES, ..Default::default() });
    old.add(floor_invalid.clone()).unwrap();
    old.add(cap_invalid.clone()).unwrap();
    old.add(restored.clone()).unwrap();
    store.persist(&old).unwrap();

    let log = capture_restore_log(|| {
        state.restore_pending().unwrap();
    });

    let pool = read(&state.mempool);
    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&restored.hash()));
    assert!(!pool.contains(&floor_invalid.hash()), "floor-invalid never parks");
    assert!(!pool.contains(&cap_invalid.hash()), "the L0 cap drop set is unchanged");

    assert!(log.contains("pending pool restored"), "{log}");
    assert!(log.contains("mined=0"), "{log}");
    assert!(log.contains("floor_dropped=1"), "{log}");
    assert!(log.contains("oversized=0"), "{log}");
    assert!(log.contains("cap_dropped=1"), "{log}");
    assert!(log.contains("refused=0"), "{log}");
    assert!(log.contains("restored=1"), "{log}");
}

#[test]
fn restore_pending_without_a_policy_keeps_the_legacy_drop_set() {
    // Observation-only guard: with no schedule bound the floor branch is
    // dormant, and the 32 KiB entry still drops via the cap exactly as L0,
    // now counted as cap_dropped rather than folded into refused.
    let dir = tempfile::tempdir().unwrap();
    let store = MempoolStore::open(1, Hash::ZERO, dir.path()).unwrap();
    let key = signing_key(75);
    let state = RpcState::in_memory(1).unwrap().with_pending_store(store.clone());
    let big = data_transfer(&key, 0, 10, vec![0; 32 * 1024], DEFAULT_TX_GAS_LIMIT);
    let small = signed_transfer(&key, 1, 10);
    let mut old =
        Mempool::new(MempoolConfig { max_tx_data_bytes: MAX_TX_DATA_BYTES, ..Default::default() });
    old.add(big.clone()).unwrap();
    old.add(small.clone()).unwrap();
    store.persist(&old).unwrap();

    let log = capture_restore_log(|| {
        state.restore_pending().unwrap();
    });
    let pool = read(&state.mempool);
    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&small.hash()));
    assert!(!pool.contains(&big.hash()));
    assert!(log.contains("floor_dropped=0"), "{log}");
    assert!(log.contains("cap_dropped=1"), "{log}");
}

// --- Bind-time gas-limit frame (closes dewi's disclosed L3 gap) ---

#[test]
fn calldata_policy_refuses_a_restored_tip_under_a_different_gas_limit() {
    // dewi's disclosed L3 limitation: the restored-tip gas-mismatch branch of
    // `with_calldata_policy` was unexercised. Seal a v3 tip under a custom gas
    // limit, reopen (which leaves block_gas_limit at the default), and bind the
    // schedule without re-asserting the custom limit: the branch must refuse.
    let key = signing_key(76);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let custom_limit: Gas = 4_000_000;
    assert_ne!(custom_limit, BLOCK_GAS_LIMIT);
    let dir = tempfile::tempdir().unwrap();
    {
        let state = RpcState::with_data_dir(1, dir.path())
            .unwrap()
            .with_block_gas_limit(custom_limit)
            .unwrap()
            .with_calldata_policy(calldata_policy())
            .unwrap();
        state.seed_accounts([(sender, 1_000_000)]).unwrap();
        state
            .submit_transaction(data_transfer(&key, 0, 10, vec![0; 1024], DEFAULT_TX_GAS_LIMIT))
            .unwrap();
        assert_eq!(read(&state.blocks).last().unwrap().header.gas_limit, custom_limit);
    }

    // Reopen leaves block_gas_limit at BLOCK_GAS_LIMIT; the tip records
    // custom_limit, so binding the schedule hits the gas-mismatch branch.
    let reopened = RpcState::with_data_dir(1, dir.path()).unwrap();
    let error = reopened
        .with_calldata_policy(calldata_policy())
        .err()
        .expect("a gas-mismatched restored tip must be refused");
    assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("does not match the restored ledger's"), "{text}");
    assert!(text.contains(&custom_limit.to_string()), "{text}");
}
