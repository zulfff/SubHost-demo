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
    CounterVec, Encoder, Gauge, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder,
};
use std::net::SocketAddr;
use std::sync::Arc;
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

        registry.register(Box::new(rpc_requests.clone()))?;
        registry.register(Box::new(rpc_errors.clone()))?;
        registry.register(Box::new(rpc_latency.clone()))?;
        registry.register(Box::new(connected_peers.clone()))?;
        registry.register(Box::new(block_height.clone()))?;
        registry.register(Box::new(pending_transactions.clone()))?;

        Ok(Self {
            registry: Arc::new(registry),
            rpc_requests,
            rpc_errors,
            rpc_latency,
            connected_peers,
            block_height,
            pending_transactions,
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

    /// Render the registry in the Prometheus text exposition format.
    pub fn encode(&self) -> Result<Vec<u8>, MetricsError> {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buffer)?;
        Ok(buffer)
    }

    /// The exporter router, exposing `GET /metrics` and `GET /health`.
    pub fn router(&self) -> Router {
        let metrics = self.clone();
        Router::new()
            .route(
                "/metrics",
                get(move || {
                    let metrics = metrics.clone();
                    async move { metrics.metrics_response() }
                }),
            )
            .route("/health", get(|| async { "ok" }))
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
