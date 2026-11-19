//! S8.1 contract tests for the block-path spans (Q-035): one `info` span per
//! seal/accept/commit with bounded operational fields, structural `error_kind`
//! tagging, and no calldata, transaction hashes, signatures or raw error text.

use crate::state::read;
use crate::test_support::{funded_state, signed_transfer, signing_key};
use crate::{BlockAuthority, RpcState};
use std::io::Write;
use std::sync::{Arc, Mutex};
use subhost_consensus::{QuorumCertificate, ValidatorRegistry};
use subhost_core::{Address, Block, Hash};
use subhost_crypto::{BLSScheme, PrivateKey};
use tracing_subscriber::layer::{Layer, SubscriberExt};

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

/// Run `f` under a thread-local subscriber that records span CLOSE events,
/// then return everything it wrote. All block-path calls under test are
/// synchronous and run on this thread inside the scoped window, so the
/// scoped subscriber sees every span.
///
/// The callsite interest cache is one global cache: a callsite first hit
/// while no subscriber is active caches `never`, and `with_default` does not
/// rebuild it. Rebuilding inside the scoped window re-registers every
/// callsite. This coexists with the observability suite's process-global
/// capture because the filters agree: this scoped filter mirrors the global
/// capture's permissiveness exactly (`subhost_rpc` INFO, methods OFF), so
/// the interest fold — and the sole-dispatcher `JustOne` rebuild path —
/// cache the same opinion either capture would (Q-035 item 8 review).
fn capture_spans(f: impl FnOnce()) -> String {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(BufferWriter(buffer.clone()))
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_ansi(false)
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("subhost_rpc", tracing::Level::INFO)
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

fn signing_registry() -> (ValidatorRegistry, Address, PrivateKey) {
    let (secret, public) = BLSScheme::keygen();
    let producer = Address::new([9; 20]);
    let mut registry = ValidatorRegistry::new();
    registry.register(producer, public, &BLSScheme::proof_of_possession(&secret)).unwrap();
    (registry, producer, secret)
}

fn final_qc(
    block: &Block,
    producer: Address,
    secret: &PrivateKey,
    context: Hash,
    view: u64,
) -> QuorumCertificate {
    let hash = block.hash();
    QuorumCertificate {
        protocol_context: context,
        view,
        block_hash: hash,
        signatures: vec![(
            producer,
            BLSScheme::encode_signature(&BLSScheme::sign(
                secret,
                &QuorumCertificate::payload_for_context(context, view, &hash),
            )),
        )],
    }
}

fn validator_state(context: Hash) -> (RpcState, Address, PrivateKey) {
    let (registry, producer, secret) = signing_registry();
    let state = RpcState::in_memory(1)
        .unwrap()
        .with_consensus_context(context)
        .with_validators(registry.clone())
        .with_authority(BlockAuthority::new(registry, producer, secret.clone()).unwrap());
    (state, producer, secret)
}

fn fund_and_build(state: &RpcState) -> Block {
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
    state.build_block().unwrap().unwrap()
}

#[test]
fn seal_span_carries_bounded_fields_and_no_transaction_identity() {
    let key = signing_key(7);
    let state = funded_state(&key, 1_000_000);
    let tx = signed_transfer(&key, 0, 10);
    let tx_hash = tx.hash();
    let log = capture_spans(|| {
        state.submit_transaction(tx).unwrap();
    });
    assert!(log.contains("seal_block"), "one span per seal: {log}");
    assert_eq!(log.matches("seal_block").count(), 1, "not one span per transaction");
    assert!(log.contains("height=1"));
    assert!(log.contains("tx_count=1"));
    assert!(log.contains("gas_used="));
    assert!(log.contains("outcome=\"ok\""));
    // Transaction hashes are not block hashes and never enter a span (Q-035).
    assert!(!log.contains(&tx_hash.to_string()), "tx hash leaked into span: {log}");
}

#[test]
fn an_empty_seal_reports_zero_counts_without_a_fabricated_height() {
    let state = RpcState::in_memory(1).unwrap();
    let mut pool = read(&state.mempool).clone();
    let log = capture_spans(|| {
        let receipts = state.seal_block(&mut pool).unwrap();
        assert!(receipts.is_empty());
    });
    assert!(log.contains("seal_block"));
    assert!(log.contains("tx_count=0"));
    assert!(log.contains("gas_used=0"));
    assert!(log.contains("outcome=\"ok\""));
    assert!(!log.contains("height="), "no candidate means no height: {log}");
}

#[test]
fn accept_span_reports_success_and_rejection_without_raw_error_text() {
    let key = signing_key(7);
    // Build on a validator-configured state (submission does not seal there),
    // then accept on a plain demo state funded identically: same genesis, same
    // transition, so the re-executed state root matches.
    let (builder, _, _) = validator_state(Hash::from_data(b"span-accept-builder"));
    let block = fund_and_build(&builder);
    let state = funded_state(&key, 1_000_000);
    let log = capture_spans(|| {
        state.accept_block(block.clone()).unwrap();
    });
    assert!(log.contains("accept_block"));
    assert!(log.contains("height=1"));
    assert!(log.contains(&format!("block_hash={}", block.hash())));
    assert!(log.contains("tx_count=1"));
    assert!(log.contains("producer="));
    assert!(log.contains("outcome=\"ok\""));

    // A tampered parent fails validation; the span carries the bounded kind,
    // never the raw rejection text.
    let mut bad = block.clone();
    bad.header.parent_hash = Hash::from_data(b"not-the-parent");
    let raw = state.accept_block(bad.clone()).unwrap_err().to_string();
    let log = capture_spans(|| {
        assert!(state.accept_block(bad).is_err());
    });
    assert!(log.contains("outcome=\"rejected\""));
    assert!(log.contains("error_kind=\"rejected\""));
    assert!(!log.contains(&raw), "raw error text leaked into span: {log}");
}

#[test]
fn commit_qc_span_tags_certificate_failures_structurally() {
    let context = Hash::from_data(b"span-commit-qc");
    let (state, producer, secret) = validator_state(context);
    let block = fund_and_build(&state);
    let qc = final_qc(&block, producer, &secret, context, 3);
    let log = capture_spans(|| {
        state.commit_qc(block.clone(), &qc).unwrap();
    });
    assert!(log.contains("commit_qc"));
    assert!(log.contains("height=1"));
    assert!(log.contains("view=3"));
    assert!(log.contains(&format!("block_hash={}", block.hash())));
    assert!(log.contains("outcome=\"ok\""));
    assert!(!log.contains("error_kind="), "a clean commit carries no error kind");

    // A forged signature is a certificate failure regardless of the error
    // variant's text: the span must say invalid_qc, not a generic rejection.
    let mut forged = qc.clone();
    forged.signatures[0].1 = vec![0xff; 96];
    let raw = state.commit_qc(block.clone(), &forged).unwrap_err().to_string();
    let log = capture_spans(|| {
        assert!(state.commit_qc(block.clone(), &forged).is_err());
    });
    assert!(log.contains("error_kind=\"invalid_qc\""));
    assert!(log.contains("outcome=\"rejected\""));
    assert!(!log.contains(&raw), "raw error text leaked into span: {log}");

    // A certificate naming a different block is the same structural failure.
    let mut wrong_hash = qc.clone();
    wrong_hash.block_hash = Hash::from_data(b"another-block");
    let log = capture_spans(|| {
        assert!(state.commit_qc(block, &wrong_hash).is_err());
    });
    assert!(log.contains("error_kind=\"invalid_qc\""));
    assert!(log.contains("outcome=\"rejected\""));
}
