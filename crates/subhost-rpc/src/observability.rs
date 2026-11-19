//! Metrics and tracing spans for completed decoded JSON-RPC calls on HTTP and
//! WebSocket.
//!
//! One `info` span per decoded call rides the same middleware as the metrics:
//! the span is named `rpc_call` (tracing requires constant span names) and
//! carries the registered static method name (or `unknown`) in `rpc.method`,
//! recording only operational fields — outcome, duration and the JSON-RPC
//! error code. Params, payloads, tokens and caller-derived strings never enter
//! a span (Q-020). Spans are emitted through the existing `tracing` API: with
//! no subscriber installed they are near-free no-ops, and the default `fmt`
//! layer does not log span events, so default log output is unchanged.
//! Visibility is controlled by the installed subscriber (env filter today, the
//! opt-in OTLP provider later). Like the metrics, spans exist only when a
//! metrics registry is supplied via `RpcServer::with_metrics`; the node always
//! supplies one.

use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::server::MethodResponse;
use jsonrpsee::types::Request;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use subhost_metrics::Metrics;
use tower::Layer;
use tracing::Instrument;

/// Instrument calls using only names from the server's method registry.
#[derive(Clone)]
pub(crate) struct RpcMetricsLayer {
    metrics: Metrics,
    methods: Arc<HashSet<&'static str>>,
}

impl RpcMetricsLayer {
    pub(crate) fn new(metrics: Metrics, methods: impl Iterator<Item = &'static str>) -> Self {
        Self { metrics, methods: Arc::new(methods.collect()) }
    }
}

impl<S> Layer<S> for RpcMetricsLayer {
    type Service = RpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcMetricsService { inner, metrics: self.metrics.clone(), methods: self.methods.clone() }
    }
}

#[derive(Clone)]
pub(crate) struct RpcMetricsService<S> {
    inner: S,
    metrics: Metrics,
    methods: Arc<HashSet<&'static str>>,
}

impl<'a, S> RpcServiceT<'a> for RpcMetricsService<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = Pin<Box<dyn Future<Output = MethodResponse> + Send + 'a>>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        // Copy the registered static name, never the caller's method string.
        let method = self.methods.get(request.method.as_ref()).copied().unwrap_or("unknown");
        // The span name is a constant (tracing metadata requirement); the
        // bounded method name travels in the rpc.method field. Only operational
        // fields are recorded below; the request itself never enters a span.
        let span = tracing::info_span!(
            target: "subhost_rpc",
            "rpc_call",
            rpc.method = method,
            rpc.outcome = tracing::field::Empty,
            rpc.duration_ms = tracing::field::Empty,
            rpc.error_code = tracing::field::Empty,
        );
        let metrics = self.metrics.clone();
        let service = self.inner.clone();
        let started = Instant::now();
        Box::pin(
            async move {
                let response = service.call(request).await;
                let elapsed = started.elapsed();
                let failed = response.is_error();
                let span = tracing::Span::current();
                span.record("rpc.outcome", if failed { "error" } else { "ok" });
                span.record("rpc.duration_ms", elapsed.as_millis() as u64);
                if let Some(code) = response.as_error_code() {
                    span.record("rpc.error_code", code);
                }
                metrics.record_rpc_result(method, elapsed.as_secs_f64(), failed);
                response
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RpcSecurityConfig, RpcServer, RpcServerConfig, RpcState, SecretToken};
    use jsonrpsee::server::ServerHandle;
    use serde_json::{json, Value};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn start(metrics: &Metrics, security: RpcSecurityConfig) -> (SocketAddr, ServerHandle) {
        RpcServer::new(RpcState::in_memory(1).unwrap())
            .with_metrics(metrics.clone())
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                security,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    async fn http(addr: SocketAddr, payload: &str, token: Option<&str>) -> (u16, Option<Value>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let authorization = token
                .map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n{authorization}Connection: close\r\n\r\n{payload}",
                payload.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
            (status, serde_json::from_str(body).ok())
        })
        .await
        .expect("HTTP call timed out")
    }

    fn count(metrics: &Metrics, name: &str, method: &str) -> f64 {
        metrics
            .registry()
            .gather()
            .iter()
            .find(|family| family.name() == name)
            .and_then(|family| {
                family.get_metric().iter().find(|metric| {
                    metric
                        .get_label()
                        .iter()
                        .any(|label| label.name() == "method" && label.value() == method)
                })
            })
            .map_or(0.0, |metric| {
                if name.ends_with("duration_seconds") {
                    metric.get_histogram().get_sample_count() as f64
                } else {
                    metric.get_counter().get_value()
                }
            })
    }

    fn call(id: u64, method: &str) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": []}).to_string()
    }

    #[tokio::test]
    async fn http_success_and_parameter_errors_are_counted_once() {
        let metrics = Metrics::new().unwrap();
        let (addr, handle) = start(&metrics, RpcSecurityConfig::default()).await;
        let (_, success) = http(addr, &call(1, "eth_chainId"), None).await;
        assert_eq!(success.unwrap()["result"], "0x1");
        let (_, missing) = http(addr, &call(2, "eth_getBalance"), None).await;
        assert_eq!(missing.unwrap()["error"]["code"], -32602);
        let malformed_address = json!({
            "jsonrpc": "2.0", "id": 3, "method": "eth_getBalance", "params": ["bad address"]
        });
        let (_, invalid) = http(addr, &malformed_address.to_string(), None).await;
        assert!(invalid.unwrap().get("error").is_some());

        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_chainId"), 1.0);
        assert_eq!(count(&metrics, "subhost_rpc_errors_total", "eth_chainId"), 0.0);
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_getBalance"), 2.0);
        assert_eq!(count(&metrics, "subhost_rpc_errors_total", "eth_getBalance"), 2.0);
        assert_eq!(count(&metrics, "subhost_rpc_request_duration_seconds", "eth_getBalance"), 2.0);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn batches_count_each_call_and_bound_unknown_method_labels() {
        let metrics = Metrics::new().unwrap();
        let (addr, handle) = start(&metrics, RpcSecurityConfig::default()).await;
        let attacker_names = ["caller-001", "caller-002", "caller-003", "caller\"004\n"];
        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId"},
            {"jsonrpc": "2.0", "id": 2, "method": attacker_names[0]},
            {"jsonrpc": "2.0", "id": 3, "method": attacker_names[1]}
        ]);
        let (_, response) = http(addr, &batch.to_string(), None).await;
        let response = response.unwrap();
        assert_eq!(response.as_array().unwrap().len(), 3);
        assert_eq!(response[1]["error"]["code"], -32601);
        assert_eq!(response[2]["error"]["code"], -32601);
        for (index, method) in attacker_names[2..].iter().enumerate() {
            let (_, response) = http(addr, &call(index as u64 + 4, method), None).await;
            assert_eq!(response.unwrap()["error"]["code"], -32601);
        }
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_chainId"), 1.0);
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "unknown"), 4.0);
        assert_eq!(count(&metrics, "subhost_rpc_errors_total", "unknown"), 4.0);
        assert_eq!(count(&metrics, "subhost_rpc_request_duration_seconds", "unknown"), 4.0);

        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        for method in attacker_names {
            assert!(!rendered.contains(method), "caller input reached a metric label");
        }
        let requests = metrics.registry().gather();
        let requests =
            requests.iter().find(|family| family.name() == "subhost_rpc_requests_total").unwrap();
        assert_eq!(requests.get_metric().len(), 3, "only unattributed, known and unknown labels");
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn guard_rejections_malformed_json_and_notifications_bypass_call_metrics() {
        let metrics = Metrics::new().unwrap();
        let token = "subhost test authorization token";
        let (addr, handle) = start(
            &metrics,
            RpcSecurityConfig {
                auth_token: Some(SecretToken::new(token).unwrap()),
                ..Default::default()
            },
        )
        .await;
        let (status, _) = http(addr, &call(1, "eth_chainId"), None).await;
        assert_eq!(status, 401);
        let (_, malformed) = http(addr, "{\"jsonrpc\":", Some(token)).await;
        assert!(malformed.unwrap().get("error").is_some());
        let notification = json!({"jsonrpc": "2.0", "method": "eth_chainId"});
        let (_, response) = http(addr, &notification.to_string(), Some(token)).await;
        assert!(response.is_none());
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_chainId"), 0.0);
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "unknown"), 0.0);

        let (_, response) = http(addr, &call(2, "eth_chainId"), Some(token)).await;
        assert_eq!(response.unwrap()["result"], "0x1");
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_chainId"), 1.0);
        assert_eq!(count(&metrics, "subhost_rpc_request_duration_seconds", "eth_chainId"), 1.0);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    async fn websocket_call(addr: SocketAddr) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    format!(
                        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.unwrap());
                assert!(headers.len() <= 4096, "unexpected WebSocket handshake size");
            }
            assert!(String::from_utf8(headers).unwrap().starts_with("HTTP/1.1 101"));

            let payload = call(1, "eth_chainId");
            let mask = [1u8, 2, 3, 4];
            assert!(payload.len() < 126);
            let mut frame = vec![0x81, 0x80 | u8::try_from(payload.len()).unwrap()];
            frame.extend_from_slice(&mask);
            frame.extend(payload.bytes().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
            stream.write_all(&frame).await.unwrap();

            loop {
                let opcode = stream.read_u8().await.unwrap() & 0x0f;
                let length = stream.read_u8().await.unwrap();
                assert_eq!(length & 0x80, 0, "server frames must not be masked");
                let length = match length {
                    126 => usize::from(stream.read_u16().await.unwrap()),
                    127 => usize::try_from(stream.read_u64().await.unwrap()).unwrap(),
                    value => usize::from(value),
                };
                assert!(length <= 4096, "unexpected WebSocket response size");
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                if opcode == 1 {
                    return serde_json::from_slice(&body).unwrap();
                }
                assert_eq!(opcode, 9, "unexpected WebSocket opcode");
                // A ping may race with the reply. Return a masked pong.
                let mut pong = vec![0x8a, 0x80 | u8::try_from(body.len()).unwrap()];
                pong.extend_from_slice(&mask);
                pong.extend(body.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
                stream.write_all(&pong).await.unwrap();
            }
        })
        .await
        .expect("WebSocket call timed out")
    }

    #[tokio::test]
    async fn websocket_calls_use_the_supplied_exporter_registry() {
        let metrics = Metrics::new().unwrap();
        let (addr, handle) = start(&metrics, RpcSecurityConfig::default()).await;
        let response = websocket_call(addr).await;
        assert_eq!(response["result"], "0x1");
        assert_eq!(count(&metrics, "subhost_rpc_requests_total", "eth_chainId"), 1.0);
        assert_eq!(count(&metrics, "subhost_rpc_errors_total", "eth_chainId"), 0.0);
        assert_eq!(count(&metrics, "subhost_rpc_request_duration_seconds", "eth_chainId"), 1.0);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn separate_server_registries_do_not_mix_request_counts() {
        let first = Metrics::new().unwrap();
        let second = Metrics::new().unwrap();
        let (first_addr, first_handle) = start(&first, RpcSecurityConfig::default()).await;
        let (second_addr, second_handle) = start(&second, RpcSecurityConfig::default()).await;
        http(first_addr, &call(1, "eth_chainId"), None).await;
        http(first_addr, &call(2, "eth_chainId"), None).await;
        http(second_addr, &call(3, "eth_chainId"), None).await;
        assert_eq!(count(&first, "subhost_rpc_requests_total", "eth_chainId"), 2.0);
        assert_eq!(count(&second, "subhost_rpc_requests_total", "eth_chainId"), 1.0);
        first_handle.stop().unwrap();
        second_handle.stop().unwrap();
        first_handle.stopped().await;
        second_handle.stopped().await;
    }

    use std::cell::RefCell;
    use std::sync::OnceLock;

    /// One process-global capture subscriber, installed on first use. Span
    /// tests cannot use thread-local subscribers: tracing's callsite interest
    /// cache is global, and parallel tests without a subscriber race the
    /// cache, dropping spans non-deterministically. `set_global_default`
    /// rebuilds interest against the capture subscriber, and every later
    /// callsite registration sees it on all threads.
    static CAPTURE_INSTALLED: OnceLock<()> = OnceLock::new();

    thread_local! {
        /// Per-thread span-close output. Each test runs its server on its own
        /// current-thread runtime, so a test's spans land only in its buffer.
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

    /// Install the global capture subscriber once and clear this thread's
    /// buffer. Filtered to the RPC span target; the server's own log events
    /// (`subhost_rpc::methods`) are excluded so only spans reach assertions.
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
                            .with_target("subhost_rpc", tracing::Level::INFO)
                            .with_target(
                                "subhost_rpc::methods",
                                tracing_subscriber::filter::LevelFilter::OFF,
                            ),
                    ),
            );
            tracing::subscriber::set_global_default(subscriber)
                .expect("the test binary installs one global capture subscriber");
        });
        SPAN_OUTPUT.with(|output| output.borrow_mut().clear());
    }

    fn rendered() -> String {
        SPAN_OUTPUT.with(|output| String::from_utf8(output.borrow().clone()).unwrap())
    }

    #[tokio::test]
    async fn spans_carry_only_bounded_operational_fields() {
        let metrics = Metrics::new().unwrap();
        install_span_capture();
        let (addr, handle) = start(&metrics, RpcSecurityConfig::default()).await;

        let (_, ok) = http(addr, &call(1, "eth_chainId"), None).await;
        assert_eq!(ok.unwrap()["result"], "0x1");
        let (_, err) = http(addr, &call(2, "eth_getBalance"), None).await;
        assert_eq!(err.unwrap()["error"]["code"], -32602);
        let attacker = "caller-private-method-9f3k";
        let (_, unknown) = http(addr, &call(3, attacker), None).await;
        assert_eq!(unknown.unwrap()["error"]["code"], -32601);
        handle.stop().unwrap();
        handle.stopped().await;

        let text = rendered();
        assert_eq!(text.matches("rpc.outcome=").count(), 3, "one span per decoded call");
        assert!(text.contains("rpc.method=\"eth_chainId\""), "span fields: {text}");
        assert!(text.contains("rpc.method=\"eth_getBalance\""), "span fields: {text}");
        assert!(text.contains("rpc.method=\"unknown\""), "span fields: {text}");
        assert!(text.contains("rpc.outcome=\"ok\""), "span fields: {text}");
        assert!(text.contains("rpc.outcome=\"error\""), "span fields: {text}");
        assert!(text.contains("rpc.error_code=-32602"), "span fields: {text}");
        assert!(text.contains("rpc.error_code=-32601"), "span fields: {text}");
        assert!(text.contains("rpc.duration_ms="), "span fields: {text}");
        assert!(!text.contains(attacker), "caller input reached a span: {text}");
    }

    #[tokio::test]
    async fn batches_emit_one_span_per_call_and_bypass_paths_emit_none() {
        let metrics = Metrics::new().unwrap();
        install_span_capture();
        let (addr, handle) = start(&metrics, RpcSecurityConfig::default()).await;

        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId"},
            {"jsonrpc": "2.0", "id": 2, "method": "batch-attacker-a"},
            {"jsonrpc": "2.0", "id": 3, "method": "batch-attacker-b"}
        ]);
        let (_, response) = http(addr, &batch.to_string(), None).await;
        assert_eq!(response.unwrap().as_array().unwrap().len(), 3);

        // Notifications and malformed JSON bypass the decoded-call middleware.
        let notification = json!({"jsonrpc": "2.0", "method": "eth_chainId"});
        let (_, response) = http(addr, &notification.to_string(), None).await;
        assert!(response.is_none());
        let (_, malformed) = http(addr, "{\"jsonrpc\":", None).await;
        assert!(malformed.unwrap().get("error").is_some());
        handle.stop().unwrap();
        handle.stopped().await;

        let text = rendered();
        assert_eq!(
            text.matches("rpc.outcome=").count(),
            3,
            "one span per batch call, none for notifications or malformed JSON: {text}"
        );
        assert!(text.contains("rpc.method=\"unknown\""), "span fields: {text}");
        assert!(!text.contains("batch-attacker-a"), "caller input reached a span: {text}");
        assert!(!text.contains("batch-attacker-b"), "caller input reached a span: {text}");
    }

    #[tokio::test]
    async fn guard_rejections_emit_no_spans() {
        let metrics = Metrics::new().unwrap();
        install_span_capture();
        let token = "subhost test authorization token";
        let (addr, handle) = start(
            &metrics,
            RpcSecurityConfig {
                auth_token: Some(SecretToken::new(token).unwrap()),
                ..Default::default()
            },
        )
        .await;

        let (status, _) = http(addr, &call(1, "eth_chainId"), None).await;
        assert_eq!(status, 401);
        handle.stop().unwrap();
        handle.stopped().await;

        let text = rendered();
        assert!(!text.contains("rpc.outcome="), "a guard rejection emitted a span: {text}");
    }

    #[tokio::test]
    async fn without_a_metrics_registry_no_spans_are_emitted() {
        install_span_capture();
        // The documented caveat: spans ride the metrics middleware, so a
        // library consumer that never calls with_metrics gets no spans.
        let (addr, handle) = RpcServer::new(RpcState::in_memory(1).unwrap())
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                security: RpcSecurityConfig::default(),
                ..Default::default()
            })
            .await
            .unwrap();

        let (_, ok) = http(addr, &call(1, "eth_chainId"), None).await;
        assert_eq!(ok.unwrap()["result"], "0x1");
        handle.stop().unwrap();
        handle.stopped().await;

        let text = rendered();
        assert!(!text.contains("rpc.outcome="), "spans without metrics: {text}");
    }
}
