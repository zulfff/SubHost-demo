//! S8 node runtime hook tests (Q-020/ADR-013, Q-032).
//!
//! Unit-level only: no live validator cluster, so these never contend with the
//! network-test suites. Kept in their own module so the S3.3 test files stay
//! untouched (raka, Q-032).

use super::*;
use crate::tests::{signed_transfer, signing_key};
use subhost_consensus::{ConsensusConfig, ValidatorRegistry};

fn runtime() -> RuntimeHealth {
    RuntimeHealth::new(Arc::new(AtomicBool::new(false)))
}

fn recovered_and_running(rt: &RuntimeHealth) {
    rt.ledger_loaded.store(true, Ordering::SeqCst);
    rt.pipeline_alive.store(true, Ordering::SeqCst);
}

fn gauge(metrics: &Metrics, name: &str) -> Option<f64> {
    let text = String::from_utf8(metrics.encode().unwrap()).unwrap();
    text.lines()
        .find(|line| line.starts_with(&format!("{name} ")))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

fn histogram_count(metrics: &Metrics, name: &str) -> u64 {
    let text = String::from_utf8(metrics.encode().unwrap()).unwrap();
    text.lines()
        .find(|line| line.starts_with(&format!("{name}_count ")))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
fn readiness_requires_recovery_and_a_live_pipeline() {
    let metrics = Metrics::new().unwrap();
    let rt = runtime();
    rt.publish(&metrics, false);
    assert_eq!(
        metrics.health(),
        HealthStatus { ledger_loaded: false, syncing: false, consensus_alive: false },
        "the default is not ready"
    );

    rt.ledger_loaded.store(true, Ordering::SeqCst);
    rt.publish(&metrics, false);
    assert!(!metrics.health().consensus_alive, "recovery alone is not a live pipeline");

    rt.pipeline_alive.store(true, Ordering::SeqCst);
    rt.publish(&metrics, false);
    assert_eq!(
        metrics.health(),
        HealthStatus { ledger_loaded: true, syncing: false, consensus_alive: true }
    );
}

#[test]
fn a_pipeline_exit_clears_readiness() {
    let metrics = Metrics::new().unwrap();
    let rt = runtime();
    recovered_and_running(&rt);
    rt.publish(&metrics, false);
    assert!(metrics.health().consensus_alive);
    rt.pipeline_alive.store(false, Ordering::SeqCst);
    rt.publish(&metrics, false);
    assert!(!metrics.health().consensus_alive, "an exited pipeline is not alive");
    assert!(metrics.health().ledger_loaded, "the ledger is still loaded");
}

#[test]
fn a_halt_clears_readiness_without_relabelling_syncing() {
    let metrics = Metrics::new().unwrap();
    let rt = runtime();
    recovered_and_running(&rt);
    rt.publish(&metrics, true);
    let halted = metrics.health();
    assert!(!halted.consensus_alive, "a safety incident is not a live participant");
    assert!(!halted.syncing, "a halt is not a catch-up and not a reorg");
    rt.publish(&metrics, false);
    assert!(metrics.health().consensus_alive, "readiness returns when the halt clears");
}

#[test]
fn catching_up_is_the_only_syncing_predicate() {
    let metrics = Metrics::new().unwrap();
    let catching_up = Arc::new(AtomicBool::new(false));
    let rt = RuntimeHealth::new(catching_up.clone());
    recovered_and_running(&rt);
    rt.publish(&metrics, false);
    assert!(!metrics.health().syncing, "peer absence alone is not syncing");

    catching_up.store(true, Ordering::SeqCst);
    rt.publish(&metrics, false);
    assert!(metrics.health().syncing);
    assert!(metrics.health().consensus_alive, "the pipeline is alive while catching up");

    // The stall budget clearing the flag restores readiness (S3.3 behaviour).
    catching_up.store(false, Ordering::SeqCst);
    rt.publish(&metrics, false);
    assert_eq!(
        metrics.health(),
        HealthStatus { ledger_loaded: true, syncing: false, consensus_alive: true }
    );
}

#[test]
fn a_latency_sample_needs_a_locally_observed_proposal_and_is_consumed_once() {
    let rt = runtime();
    assert!(rt.take_qc_latency(3).is_none(), "catch-up commits produce no sample");
    rt.note_proposal(3);
    let sample = rt.take_qc_latency(3).expect("a locally observed proposal yields a sample");
    assert!(sample.is_finite() && sample >= 0.0);
    assert!(rt.take_qc_latency(3).is_none(), "the sample is consumed once");
}

#[test]
fn a_retransmission_does_not_restart_the_latency_clock() {
    let rt = runtime();
    rt.note_proposal(7);
    std::thread::sleep(std::time::Duration::from_millis(30));
    rt.note_proposal(7);
    let sample = rt.take_qc_latency(7).unwrap();
    assert!(sample >= 0.03, "first observation wins: {sample}");
}

#[test]
fn the_latency_map_is_bounded_and_evicts_the_oldest_view() {
    let rt = runtime();
    for view in 1..=(MAX_LATENCY_VIEWS as u64 + 10) {
        rt.note_proposal(view);
    }
    assert!(rt.proposal_seen.lock().unwrap().len() <= MAX_LATENCY_VIEWS);
    assert!(rt.take_qc_latency(1).is_none(), "the oldest view was evicted");
    assert!(rt.take_qc_latency(MAX_LATENCY_VIEWS as u64 + 10).is_some());
}

#[test]
fn finality_gauge_stays_zero_without_a_verified_final_commit() {
    let dir = tempfile::tempdir().unwrap();
    let state = RpcState::with_data_dir(1, dir.path()).unwrap();
    let mut registry = ValidatorRegistry::new();
    let (secret, public) = BLSScheme::keygen();
    registry
        .register(Address::new([1; 20]), public, &BLSScheme::proof_of_possession(&secret))
        .unwrap();
    let driver = ConsensusDriver::new(ConsensusConfig::new(1).unwrap(), registry, None).unwrap();
    let metrics = Metrics::new().unwrap();
    let rt = runtime();
    rt.note_proposal(1);

    publish_finality(Some(&metrics), Some(&rt), &state, &driver);
    assert_eq!(gauge(&metrics, "subhost_finalized_height"), Some(0.0));
    assert_eq!(
        histogram_count(&metrics, "subhost_qc_commit_latency_seconds"),
        0,
        "no verified commit means no latency sample"
    );
    assert!(rt.take_qc_latency(1).is_some(), "the observation is not consumed without a commit");
}

#[test]
fn a_failed_disk_measurement_keeps_the_previous_gauge_value() {
    let dir = tempfile::tempdir().unwrap();
    let state = RpcState::with_data_dir(1, dir.path()).unwrap();
    let metrics = Metrics::new().unwrap();
    publish_disk_stats(&metrics, &state);
    let fresh = gauge(&metrics, "subhost_ledger_active_bytes").unwrap();

    // Commit a demo block so the ledger has an active footprint.
    let key = signing_key(7);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(sender, 1_000_000)]).unwrap();
    let tx = signed_transfer(&key, 0, 10);
    state.submit_transaction(tx).unwrap();
    publish_disk_stats(&metrics, &state);
    let committed = gauge(&metrics, "subhost_ledger_active_bytes").unwrap();
    assert!(committed > fresh, "a committed ledger grows the active footprint");

    // Delete a named segment: the measurement fails and the gauge must not move.
    let segments = dir.path().join(subhost_storage::LEDGER_DIR_NAME).join("segments");
    let victim = std::fs::read_dir(&segments).unwrap().next().unwrap().unwrap().path();
    std::fs::remove_file(victim).unwrap();
    assert!(state.ledger_disk_stats().is_err());
    publish_disk_stats(&metrics, &state);
    assert_eq!(
        gauge(&metrics, "subhost_ledger_active_bytes"),
        Some(committed),
        "a failed measurement keeps the last value, never a false zero"
    );
}
