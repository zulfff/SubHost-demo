//! S8.1 contract tests for the consensus-step spans (Q-035): one `info` span
//! per driver step at the node orchestration boundary — never one per vote —
//! with bounded operational fields and no raw peer error text.
//!
//! Capture follows the observability suite's pattern: one process-global
//! subscriber (the callsite interest cache is global, so thread-local
//! subscribers race it in parallel tests) writing span CLOSE events into a
//! thread-local buffer. Every test here drives its nodes synchronously on the
//! test thread, or on a current-thread runtime, so its spans land only in its
//! own buffer.

use crate::tests::{deliver_for_test, produce_for_test, proposal_for_test, validator_nodes};
use crate::{vote_message, NodeServices, NETWORK_TEST_LOCK};
use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Duration;
use subhost_core::encode_canonical;
use subhost_network::NetworkMessage;

static CAPTURE_INSTALLED: OnceLock<()> = OnceLock::new();

thread_local! {
    static SPAN_OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct ThreadLocalWriter;

impl std::io::Write for ThreadLocalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SPAN_OUTPUT.with(|output| output.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalWriter {
    type Writer = ThreadLocalWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalWriter
    }
}

/// Install the global capture once, then clear this thread's buffer so only
/// the measured section is asserted on. Both span targets are enabled: the
/// node's consensus steps and the ledger's `commit_qc` write.
fn install_span_capture() {
    CAPTURE_INSTALLED.get_or_init(|| {
        use tracing_subscriber::layer::{Layer, SubscriberExt};
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(ThreadLocalWriter)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_ansi(false)
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("subhost_node", tracing::Level::INFO)
                        .with_target("subhost_rpc::block", tracing::Level::INFO),
                ),
        );
        tracing::subscriber::set_global_default(subscriber)
            .expect("the node test binary installs one global capture subscriber");
    });
    SPAN_OUTPUT.with(|output| output.borrow_mut().clear());
}

fn rendered() -> String {
    SPAN_OUTPUT.with(|output| String::from_utf8(output.borrow().clone()).unwrap())
}

#[test]
fn a_full_round_emits_one_span_per_step_and_none_per_vote() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let (_dirs, nodes) = validator_nodes(4);
    install_span_capture();

    nodes[0]
        .state()
        .submit_transaction(crate::tests::signed_transfer(&crate::tests::signing_key(7), 0, 10))
        .unwrap();
    let round = produce_for_test(&nodes[0]).unwrap().unwrap();
    let mut prepares = vec![round.vote.as_ref().unwrap().clone()];
    for node in &nodes[1..] {
        prepares.push(deliver_for_test(node, proposal_for_test(&round.proposal)).unwrap().unwrap());
    }
    for node in &nodes {
        for vote in &prepares {
            deliver_for_test(node, vote_message(vote)).unwrap();
        }
    }
    let commits: Vec<_> = nodes
        .iter()
        .filter_map(|node| node.consensus().unwrap().lock().unwrap().own_commit_vote().cloned())
        .collect();
    assert_eq!(commits.len(), 4);
    for node in &nodes {
        for vote in &commits {
            deliver_for_test(node, vote_message(vote)).unwrap();
        }
    }

    let text = rendered();
    // One propose on the leader, one prepare per follower, one commit step
    // and one ledger write per node. Thirty-two vote deliveries add no spans.
    assert_eq!(text.matches("consensus_propose").count(), 1, "propose spans: {text}");
    assert_eq!(text.matches("consensus_prepare").count(), 3, "prepare spans: {text}");
    assert_eq!(text.matches("consensus_commit").count(), 4, "commit spans: {text}");
    assert_eq!(text.matches("commit_qc").count(), 4, "ledger spans: {text}");
    assert_eq!(text.matches("outcome=").count(), 12, "no extra spans: {text}");
    assert!(text.contains("outcome=\"ok\""));
    assert!(text.contains("height=1"));
    assert!(text.contains("view=1"));
    assert!(text.contains("votes_need=3"));
    assert!(text.contains("votes_have=1"));
    assert!(text.contains(&format!("block_hash={}", round.proposal.block.hash())));
    assert!(!text.contains("error_kind="), "a clean round carries no error kind: {text}");
}

#[test]
fn a_rejected_proposal_records_a_bounded_outcome_without_error_text() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let (_dirs, nodes) = validator_nodes(4);
    install_span_capture();

    nodes[0]
        .state()
        .submit_transaction(crate::tests::signed_transfer(&crate::tests::signing_key(7), 0, 10))
        .unwrap();
    let mut round = produce_for_test(&nodes[0]).unwrap().unwrap();
    // Tamper with the leader's proposal signature: the block itself stays
    // executable, so the failure happens inside the authenticated step.
    round.proposal.signature[0] ^= 0xff;
    let error = deliver_for_test(&nodes[1], proposal_for_test(&round.proposal)).unwrap_err();

    let text = rendered();
    assert_eq!(text.matches("consensus_prepare").count(), 1, "one span per step: {text}");
    assert!(text.contains("outcome=\"rejected\""));
    // Prepare spans carry no error_kind field at all (Q-035).
    assert!(!text.contains("error_kind="), "prepare spans have no error kind: {text}");
    assert!(!text.contains(&error.to_string()), "raw error text leaked into span: {text}");
}

#[test]
fn view_change_reports_and_certificates_carry_the_authenticated_tally() {
    let _cluster = NETWORK_TEST_LOCK.blocking_lock();
    let (_dirs, nodes) = validator_nodes(4);
    install_span_capture();

    // Three validators time out view 1 and sign reports for view 2.
    let mut changes = Vec::new();
    for node in &nodes[1..] {
        let mut driver = node.consensus().unwrap().lock().unwrap();
        assert!(driver.tick(Duration::ZERO).unwrap().is_none());
        changes.push(driver.tick(Duration::from_millis(2_000)).unwrap().unwrap());
    }
    for change in &changes {
        deliver_for_test(&nodes[0], NetworkMessage::ViewChange { data: encode_canonical(change) })
            .unwrap();
    }

    let text = rendered();
    assert_eq!(text.matches("consensus_view_change").count(), 3, "one span per report: {text}");
    assert_eq!(text.matches("target_view=2").count(), 3);
    assert_eq!(text.matches("reports_need=3").count(), 3);
    // Tally sequence 1, 3, 4: the own report joins at the (f+1)-th report
    // (second delivery: 2 peers + own = 3), the third peer makes 4.
    assert!(text.contains("reports_have=1"));
    assert!(text.contains("reports_have=3"));
    assert!(text.contains("reports_have=4"));
    assert!(text.contains("has_prepared=false"));
    assert!(text.contains("outcome=\"ok\""));
    assert!(!text.contains("error_kind="), "view-change spans have no error kind: {text}");

    // The quorum of reports forms a certificate, re-formed to include every
    // authenticated report at the target view; adopting it is one more
    // view-change step whose tally is the certificate's message list.
    let certificate = nodes[0].consensus().unwrap().lock().unwrap().timeout_certificate().unwrap();
    assert_eq!(certificate.view, 2);
    assert_eq!(certificate.messages.len(), 4);
    install_span_capture();
    deliver_for_test(
        &nodes[1],
        NetworkMessage::TimeoutCertificate { data: encode_canonical(&certificate) },
    )
    .unwrap();
    let text = rendered();
    assert_eq!(text.matches("consensus_view_change").count(), 1, "one span per TC: {text}");
    assert!(text.contains("target_view=2"));
    assert!(text.contains("reports_have=4"));
    assert!(text.contains("reports_need=3"));
    assert!(text.contains("outcome=\"ok\""));
}

#[tokio::test]
async fn a_producer_timeout_emits_one_marker_and_noop_ticks_emit_none() {
    let _cluster = NETWORK_TEST_LOCK.lock().await;
    let (_dirs, nodes) = validator_nodes(4);
    install_span_capture();

    // A non-leader with no pending work: the ticker only arms and fires its
    // view timeout. The default view timeout is 2s; the ticker runs at 250ms.
    let state = nodes[3].state().clone();
    let driver = nodes[3].consensus().unwrap().clone();
    let producer =
        tokio::spawn(crate::produce_blocks(state, Some(driver), None, NodeServices::default()));
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    producer.abort();

    let text = rendered();
    assert_eq!(
        text.matches("consensus_view_change").count(),
        1,
        "one marker for the signed report, nothing for no-op ticks: {text}"
    );
    assert!(text.contains("view=1"));
    assert!(text.contains("target_view=2"));
    // The driver retains its own signed report, so the tally starts at one.
    assert!(text.contains("reports_have=1"));
    assert!(text.contains("reports_need=3"));
    assert!(text.contains("has_prepared=false"));
    assert!(text.contains("outcome=\"ok\""));
    assert!(!text.contains("consensus_propose"), "a follower proposes nothing: {text}");
}
