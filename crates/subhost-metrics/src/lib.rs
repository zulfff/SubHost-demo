//! Prometheus metrics registry and HTTP exporter.
//!
//! Every metric a node exposes is registered here so the names stay in one place
//! and a duplicate registration is a construction error rather than a panic.

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use prometheus::{
    Counter, CounterVec, Encoder, Gauge, Histogram, HistogramOpts, HistogramVec, Opts, Registry,
    TextEncoder,
};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Exporter configuration.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub listen_addr: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { listen_addr: SocketAddr::from(([127, 0, 0, 1], 9090)) }
    }
}

/// One coherent runtime-readiness snapshot, published by the node.
///
/// Readiness is exactly `ledger_loaded && !syncing && consensus_alive` for
/// validator, observer and explicit single-writer demo modes alike (Q-020).
/// `consensus_alive` is operational block-pipeline liveness, not BFT finality.
/// The default is not ready: a node must prove each predicate, never inherit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HealthStatus {
    /// Genesis, ledger, validator registry and journal/archive recovery all
    /// reconciled successfully.
    pub ledger_loaded: bool,
    /// An active catch-up range is in flight or waiting to complete. Ordinary
    /// peer absence is not syncing.
    pub syncing: bool,
    /// The configured block pipeline task started and has not exited or
    /// reported a fatal error. Cleared before controlled shutdown.
    pub consensus_alive: bool,
}

impl HealthStatus {
    /// Whether `/health/ready` may answer 200.
    pub fn is_ready(&self) -> bool {
        self.ledger_loaded && !self.syncing && self.consensus_alive
    }
}

/// Bounded mempool lifecycle events (S6.3). Labels come from this enum only,
/// never from sender input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MempoolEvent {
    Admitted,
    Replaced,
    Evicted,
    Expired,
}

impl MempoolEvent {
    fn label(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Replaced => "replaced",
            Self::Evicted => "evicted",
            Self::Expired => "expired",
        }
    }
}

/// The node's metric set.
///
/// Cloning shares the same underlying registry, so any component can record into
/// it without threading a reference through every call site.
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,
    rpc_requests: CounterVec,
    rpc_errors: CounterVec,
    rpc_latency: HistogramVec,
    connected_peers: Gauge,
    block_height: Gauge,
    pending_transactions: Gauge,
    health: Arc<Mutex<HealthStatus>>,
    finalized_height: Gauge,
    qc_latency: Histogram,
    ledger_active_bytes: Gauge,
    ledger_snapshot_modified: Gauge,
    mempool_events: CounterVec,
    pending_cleanup_failures: Counter,
}

impl Metrics {
    /// Build and register the metric set.
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        let rpc_requests = CounterVec::new(
            Opts::new("subhost_rpc_requests_total", "Total number of completed JSON-RPC calls"),
            &["method"],
        )?;
        let rpc_errors = CounterVec::new(
            Opts::new(
                "subhost_rpc_errors_total",
                "Total number of JSON-RPC calls that returned an error",
            ),
            &["method"],
        )?;
        let rpc_latency = HistogramVec::new(
            HistogramOpts::new(
                "subhost_rpc_request_duration_seconds",
                "JSON-RPC request duration in seconds",
            )
            // 1ms to ~16s, which spans a healthy call and a pathological one.
            .buckets(prometheus::exponential_buckets(0.001, 2.0, 15)?),
            &["method"],
        )?;
        // Keep a zero-valued series for legacy recording without a method, and
        // keep these metric families visible before the first call completes.
        rpc_requests.with_label_values(&["unattributed"]);
        rpc_errors.with_label_values(&["unattributed"]);
        rpc_latency.with_label_values(&["unattributed"]);
        let connected_peers =
            Gauge::new("subhost_connected_peers", "Number of currently connected libp2p peers")?;
        let block_height =
            Gauge::new("subhost_block_height", "Height of the newest committed block")?;
        let pending_transactions = Gauge::new(
            "subhost_pending_transactions",
            "Number of transactions currently in the mempool",
        )?;
        let finalized_height = Gauge::new(
            "subhost_finalized_height",
            "Height of the newest block committed with a verified final quorum certificate",
        )?;
        let qc_latency = Histogram::with_opts(
            HistogramOpts::new(
                "subhost_qc_commit_latency_seconds",
                "Seconds from proposal to verified final commit, as measured by the node",
            )
            // 50ms to ~100s: a healthy quorum and a stalled one.
            .buckets(prometheus::exponential_buckets(0.05, 2.0, 12)?),
        )?;
        let ledger_active_bytes = Gauge::new(
            "subhost_ledger_active_bytes",
            "On-disk bytes of the committed manifest, snapshot and segments (no replay)",
        )?;
        let ledger_snapshot_modified = Gauge::new(
            "subhost_ledger_snapshot_modified_seconds",
            "Unix timestamp of the committed snapshot file; 0 means no timestamp is available",
        )?;
        // Registered at zero and deliberately without an increment API: a reorg
        // count must stay unsupported until a reviewed reorg protocol exists.
        let reorgs = Counter::new(
            "subhost_reorg_total",
            "Chain reorganizations; always 0 until a reviewed reorg protocol ships",
        )?;
        let mempool_events = CounterVec::new(
            Opts::new(
                "subhost_mempool_transactions_total",
                "Mempool lifecycle events by bounded event label",
            ),
            &["event"],
        )?;
        for event in [
            MempoolEvent::Admitted,
            MempoolEvent::Replaced,
            MempoolEvent::Evicted,
            MempoolEvent::Expired,
        ] {
            mempool_events.with_label_values(&[event.label()]);
        }

        // Registered at zero, and unlike `reorg_total` this one *will* increment:
        // S6.1b's best-effort pending-file cleanup counts its failures here once
        // the node wiring lands. A committed block never fails because of it.
        let pending_cleanup_failures = Counter::new(
            "subhost_mempool_pending_cleanup_failures_total",
            "Failed best-effort pending-snapshot writes after a durable block commit; 0 until the S6.1b consumer wiring lands",
        )?;

        registry.register(Box::new(rpc_requests.clone()))?;
        registry.register(Box::new(rpc_errors.clone()))?;
        registry.register(Box::new(rpc_latency.clone()))?;
        registry.register(Box::new(connected_peers.clone()))?;
        registry.register(Box::new(block_height.clone()))?;
        registry.register(Box::new(pending_transactions.clone()))?;
        registry.register(Box::new(finalized_height.clone()))?;
        registry.register(Box::new(qc_latency.clone()))?;
        registry.register(Box::new(ledger_active_bytes.clone()))?;
        registry.register(Box::new(ledger_snapshot_modified.clone()))?;
        registry.register(Box::new(reorgs.clone()))?;
        registry.register(Box::new(mempool_events.clone()))?;
        registry.register(Box::new(pending_cleanup_failures.clone()))?;

        Ok(Self {
            registry: Arc::new(registry),
            rpc_requests,
            rpc_errors,
            rpc_latency,
            connected_peers,
            block_height,
            pending_transactions,
            health: Arc::new(Mutex::new(HealthStatus::default())),
            finalized_height,
            qc_latency,
            ledger_active_bytes,
            ledger_snapshot_modified,
            mempool_events,
            pending_cleanup_failures,
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Record a served request without a method, using `unattributed`.
    pub fn record_request(&self, duration_secs: f64) {
        self.record_rpc_result("unattributed", duration_secs, false);
    }

    /// Record one completed RPC call using a bounded method name.
    ///
    /// Callers must use registered method names or a fixed fallback such as
    /// `unknown`. Never create labels from caller input. The static lifetime
    /// allows middleware to use names supplied by the server's method registry.
    pub fn record_rpc_result(&self, method: &'static str, duration_secs: f64, is_error: bool) {
        self.rpc_requests.with_label_values(&[method]).inc();
        let errors = self.rpc_errors.with_label_values(&[method]);
        // A negative or non-finite duration would corrupt the histogram.
        if duration_secs.is_finite() && duration_secs >= 0.0 {
            self.rpc_latency.with_label_values(&[method]).observe(duration_secs);
        }
        if is_error {
            errors.inc();
        }
    }

    /// Record an unattributed error. Also call [`Self::record_request`] to count
    /// its request and duration, or use [`Self::record_rpc_result`] for both.
    pub fn record_error(&self) {
        self.rpc_errors.with_label_values(&["unattributed"]).inc();
    }

    pub fn set_connected_peers(&self, count: usize) {
        self.connected_peers.set(count as f64);
    }

    pub fn set_block_height(&self, height: u64) {
        self.block_height.set(height as f64);
    }

    pub fn set_pending_transactions(&self, count: usize) {
        self.pending_transactions.set(count as f64);
    }

    /// Publish one coherent readiness snapshot. Replaces the previous snapshot
    /// entirely so the three predicates can never be observed out of step.
    pub fn set_health(&self, status: HealthStatus) {
        *self.health.lock().expect("health mutex poisoned") = status;
    }

    /// The last published readiness snapshot.
    pub fn health(&self) -> HealthStatus {
        *self.health.lock().expect("health mutex poisoned")
    }

    /// Record the newest height committed with a verified final QC. Only the
    /// node's verified commit path may call this, never a peer announcement.
    pub fn set_finalized_height(&self, height: u64) {
        self.finalized_height.set(height as f64);
    }

    /// Record one proposal-to-final-commit latency. Invalid samples are
    /// dropped, matching the RPC histogram's guard.
    pub fn record_qc_latency(&self, duration_secs: f64) {
        if duration_secs.is_finite() && duration_secs >= 0.0 {
            self.qc_latency.observe(duration_secs);
        }
    }

    /// Publish a `LedgerDiskStats` measurement from `subhost-storage`. The
    /// caller holds the commit lock while measuring. A missing snapshot
    /// timestamp is exported as 0, documented as "unavailable", never as a
    /// real epoch time.
    pub fn set_ledger_disk_stats(
        &self,
        active_file_bytes: u64,
        snapshot_modified: Option<SystemTime>,
    ) {
        self.ledger_active_bytes.set(active_file_bytes as f64);
        let timestamp = snapshot_modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs_f64())
            .unwrap_or(0.0);
        self.ledger_snapshot_modified.set(timestamp);
    }

    /// Count one failed best-effort pending-snapshot cleanup.
    ///
    /// Deliberately separate from `transactions_total{event}`: this is a
    /// durability degradation of the pending file after an already durable block
    /// commit, not a pool lifecycle event. Any increase deserves a ticket.
    pub fn record_pending_cleanup_failure(&self) {
        self.pending_cleanup_failures.inc();
    }

    /// Count one bounded mempool lifecycle event.
    pub fn record_mempool_event(&self, event: MempoolEvent) {
        self.mempool_events.with_label_values(&[event.label()]).inc();
    }

    /// Render the registry in the Prometheus text exposition format.
    pub fn encode(&self) -> Result<Vec<u8>, MetricsError> {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buffer)?;
        Ok(buffer)
    }

    /// The exporter router.
    ///
    /// `GET /metrics` exposes the registry. `GET /health` (legacy) and
    /// `GET /health/live` only attest that the exporter answers.
    /// `GET /health/ready` answers 200 only when the node-published snapshot
    /// satisfies `ledger_loaded && !syncing && consensus_alive`, else 503.
    pub fn router(&self) -> Router {
        let metrics = self.clone();
        let ready = self.clone();
        Router::new()
            .route(
                "/metrics",
                get(move || {
                    let metrics = metrics.clone();
                    async move { metrics.metrics_response() }
                }),
            )
            .route("/health", get(|| async { "ok" }))
            .route("/health/live", get(|| async { "ok" }))
            .route(
                "/health/ready",
                get(move || {
                    let ready = ready.clone();
                    async move {
                        if ready.health().is_ready() {
                            (StatusCode::OK, "ready")
                        } else {
                            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
                        }
                    }
                }),
            )
    }

    fn metrics_response(&self) -> Response {
        match self.encode() {
            Ok(buffer) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, TextEncoder::new().format_type())
                .body(Body::from(buffer))
                // A builder failure here is a programming error, not operator input.
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            Err(error) => {
                warn!(%error, "cannot encode Prometheus metrics");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }

    /// Serve the exporter until the process exits.
    ///
    /// The endpoint is unauthenticated and reveals node internals; bind it to
    /// loopback or a private interface only.
    pub async fn serve(&self, config: MetricsConfig) -> Result<(), MetricsError> {
        let listener = tokio::net::TcpListener::bind(config.listen_addr)
            .await
            .map_err(|source| MetricsError::Bind { addr: config.listen_addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| MetricsError::Bind { addr: config.listen_addr, source })?;
        if !addr.ip().is_loopback() {
            warn!(%addr, "metrics exporter is bound to a non-loopback address without authentication");
        }
        info!(%addr, "metrics exporter started");
        axum::serve(listener, self.router()).await.map_err(MetricsError::Serve)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("cannot register metric: {0}")]
    Registration(#[from] prometheus::Error),

    #[error("cannot bind metrics exporter to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("metrics exporter stopped: {0}")]
    Serve(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_construction_registers_every_metric_once() {
        let metrics = Metrics::new().unwrap();
        // Two independent registries must not collide with each other.
        assert!(Metrics::new().is_ok());

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        for name in [
            "subhost_rpc_requests_total",
            "subhost_rpc_errors_total",
            "subhost_rpc_request_duration_seconds",
            "subhost_connected_peers",
            "subhost_block_height",
            "subhost_pending_transactions",
            "subhost_finalized_height",
            "subhost_qc_commit_latency_seconds",
            "subhost_ledger_active_bytes",
            "subhost_ledger_snapshot_modified_seconds",
            "subhost_reorg_total",
            "subhost_mempool_transactions_total",
            "subhost_mempool_pending_cleanup_failures_total",
        ] {
            assert!(rendered.contains(name), "{name} is missing from the exposition");
        }
    }

    #[test]
    fn recorded_values_appear_in_the_exposition() {
        let metrics = Metrics::new().unwrap();
        metrics.record_request(0.25);
        metrics.record_request(0.5);
        metrics.record_error();
        metrics.set_block_height(42);
        metrics.set_connected_peers(3);
        metrics.set_pending_transactions(7);

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_rpc_requests_total{method=\"unattributed\"} 2"));
        assert!(rendered.contains("subhost_rpc_errors_total{method=\"unattributed\"} 1"));
        assert!(rendered.contains("subhost_block_height 42"));
        assert!(rendered.contains("subhost_connected_peers 3"));
        assert!(rendered.contains("subhost_pending_transactions 7"));
        assert!(rendered
            .contains("subhost_rpc_request_duration_seconds_count{method=\"unattributed\"} 2"));
    }

    #[test]
    fn invalid_durations_are_ignored_rather_than_recorded() {
        let metrics = Metrics::new().unwrap();
        metrics.record_request(f64::NAN);
        metrics.record_request(-1.0);
        metrics.record_request(f64::INFINITY);

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        // All three still count as requests, none pollutes the histogram.
        assert!(rendered.contains("subhost_rpc_requests_total{method=\"unattributed\"} 3"));
        assert!(rendered
            .contains("subhost_rpc_request_duration_seconds_count{method=\"unattributed\"} 0"));
        assert!(rendered
            .contains("subhost_rpc_request_duration_seconds_sum{method=\"unattributed\"} 0"));
    }

    #[test]
    fn method_results_count_errors_once_and_share_the_registry() {
        let metrics = Metrics::new().unwrap();
        let recorder = metrics.clone();
        recorder.record_rpc_result("eth_chainId", 0.25, false);
        recorder.record_rpc_result("eth_getBalance", 0.5, true);
        recorder.record_rpc_result("eth_getBalance", 0.75, false);

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_rpc_requests_total{method=\"eth_chainId\"} 1"));
        assert!(rendered.contains("subhost_rpc_errors_total{method=\"eth_chainId\"} 0"));
        assert!(rendered.contains("subhost_rpc_requests_total{method=\"eth_getBalance\"} 2"));
        assert!(rendered.contains("subhost_rpc_errors_total{method=\"eth_getBalance\"} 1"));
        assert!(rendered
            .contains("subhost_rpc_request_duration_seconds_count{method=\"eth_getBalance\"} 2"));
        assert!(rendered
            .contains("subhost_rpc_request_duration_seconds_sum{method=\"eth_getBalance\"} 1.25"));
    }

    #[test]
    fn invalid_method_durations_still_count_requests_and_errors() {
        let metrics = Metrics::new().unwrap();
        for duration in [f64::NAN, -1.0, f64::INFINITY] {
            metrics.record_rpc_result("unknown", duration, true);
        }
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_rpc_requests_total{method=\"unknown\"} 3"));
        assert!(rendered.contains("subhost_rpc_errors_total{method=\"unknown\"} 3"));
        // No latency sample is created for invalid durations.
        assert!(
            !rendered.contains("subhost_rpc_request_duration_seconds_count{method=\"unknown\"}")
        );
    }

    #[test]
    fn cloned_handles_share_one_registry() {
        let metrics = Metrics::new().unwrap();
        let clone = metrics.clone();
        clone.set_block_height(9);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_block_height 9"));
    }

    #[tokio::test]
    async fn exporter_serves_metrics_and_health() {
        let metrics = Metrics::new().unwrap();
        metrics.set_block_height(5);
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = metrics.router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let body = http_get(addr, "/metrics").await;
        assert!(body.contains("subhost_block_height 5"));
        assert!(http_get(addr, "/health").await.contains("ok"));

        server.abort();
    }

    #[test]
    fn default_health_is_not_ready_and_snapshots_replace_atomically() {
        let metrics = Metrics::new().unwrap();
        assert_eq!(metrics.health(), HealthStatus::default());
        assert!(!metrics.health().is_ready());

        // Each partial state must stay not ready.
        for partial in [
            HealthStatus { ledger_loaded: true, syncing: false, consensus_alive: false },
            HealthStatus { ledger_loaded: true, syncing: true, consensus_alive: true },
            HealthStatus { ledger_loaded: false, syncing: false, consensus_alive: true },
        ] {
            metrics.set_health(partial);
            assert!(!metrics.health().is_ready());
        }

        let ready = HealthStatus { ledger_loaded: true, syncing: false, consensus_alive: true };
        metrics.set_health(ready);
        assert_eq!(metrics.health(), ready);
        assert!(ready.is_ready());

        // A clone shares the same snapshot, and shutdown clears readiness.
        let clone = metrics.clone();
        clone.set_health(HealthStatus::default());
        assert!(!metrics.health().is_ready());
    }

    #[tokio::test]
    async fn ready_route_refuses_until_every_predicate_holds() {
        let metrics = Metrics::new().unwrap();
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = metrics.router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        // Liveness only proves the exporter answers, even while not ready.
        for path in ["/health", "/health/live"] {
            let response = http_get(addr, path).await;
            assert!(response.contains("200"), "{path}: {response}");
        }
        let response = http_get(addr, "/health/ready").await;
        assert!(response.contains("503"), "default must be not ready: {response}");

        metrics.set_health(HealthStatus {
            ledger_loaded: true,
            syncing: true,
            consensus_alive: true,
        });
        let response = http_get(addr, "/health/ready").await;
        assert!(response.contains("503"), "syncing must be not ready: {response}");

        metrics.set_health(HealthStatus {
            ledger_loaded: true,
            syncing: false,
            consensus_alive: true,
        });
        let response = http_get(addr, "/health/ready").await;
        assert!(response.contains("200"), "coherent ready must pass: {response}");
        assert!(response.contains("ready"));

        server.abort();
    }

    #[test]
    fn runtime_measurements_appear_in_the_exposition() {
        let metrics = Metrics::new().unwrap();
        metrics.set_finalized_height(41);
        metrics.record_qc_latency(0.5);
        metrics.set_ledger_disk_stats(
            4096,
            Some(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        );
        metrics.record_mempool_event(MempoolEvent::Admitted);
        metrics.record_mempool_event(MempoolEvent::Admitted);
        metrics.record_mempool_event(MempoolEvent::Evicted);

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_finalized_height 41"));
        assert!(rendered.contains("subhost_qc_commit_latency_seconds_count 1"));
        assert!(rendered.contains("subhost_ledger_active_bytes 4096"));
        assert!(rendered.contains("subhost_ledger_snapshot_modified_seconds 1700000000"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 2"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"evicted\"} 1"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"replaced\"} 0"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"expired\"} 0"));
        // Reorgs stay registered at zero with no increment API.
        assert!(rendered.contains("subhost_reorg_total 0"));
    }

    #[test]
    fn missing_snapshot_timestamp_exports_zero_not_a_fake_time() {
        let metrics = Metrics::new().unwrap();
        metrics.set_ledger_disk_stats(0, None);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_ledger_active_bytes 0"));
        assert!(rendered.contains("subhost_ledger_snapshot_modified_seconds 0"));
    }

    #[test]
    fn invalid_qc_latencies_are_ignored() {
        let metrics = Metrics::new().unwrap();
        for duration in [f64::NAN, -1.0, f64::INFINITY] {
            metrics.record_qc_latency(duration);
        }
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_qc_commit_latency_seconds_count 0"));
    }

    #[tokio::test]
    async fn exporter_bind_failure_is_reported() {
        let occupied =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = occupied.local_addr().unwrap();
        let metrics = Metrics::new().unwrap();
        let error = metrics.serve(MetricsConfig { listen_addr: addr }).await.unwrap_err();
        assert!(matches!(error, MetricsError::Bind { .. }));
    }

    async fn http_get(addr: SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).to_string()
    }
}
