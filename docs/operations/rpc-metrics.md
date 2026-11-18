# RPC call metrics

`RpcServer::with_metrics(metrics)` records completed decoded JSON-RPC calls in an
existing `subhost_metrics::Metrics` registry. Pass the same registry to the RPC
server and exporter so the values observed by the server appear in `/metrics`:

```rust
let metrics = subhost_metrics::Metrics::new()?;
let rpc = subhost_rpc::RpcServer::new(state).with_metrics(metrics.clone());
let (addr, handle) = rpc.start(subhost_rpc::RpcServerConfig::default()).await?;
// Serve metrics.router() or metrics.serve(...) with this same registry.
```

This is the S5.3a server/API contract. The bundled node does not yet pass its
registry to `with_metrics`; that bootstrap change belongs to Track A after this
contract lands. Its RPC counters remain zero until that wiring is implemented.
`RpcServer::new` remains available without metrics for existing callers.

## What is recorded

Each completed decoded call increments `subhost_rpc_requests_total{method="..."}`
once. A JSON-RPC error also increments `subhost_rpc_errors_total` with the same
label. Errors count even when the HTTP response status is 200, including invalid
parameters, unsupported methods and unknown methods. Each completed call records
one sample in `subhost_rpc_request_duration_seconds`.

HTTP and WebSocket calls share the same middleware. Each decoded call in a batch
is counted separately, rather than counting the batch envelope as another call.
Duration starts at middleware entry and ends when the method response is ready;
it includes execution and waiting inside that call, but not socket delivery time.

Labels for known calls are copied from the server's registered method names.
All arbitrary unknown names use `method="unknown"`, so a caller cannot create
new time series by varying a method string. The only application label is
`method`; request IDs, addresses, parameters, tokens and error messages are never
labels. Histogram buckets also carry the standard Prometheus `le` label.

The legacy `Metrics::record_request` and `record_error` APIs keep their signatures
and use `method="unattributed"`. That series is initialized to zero so the RPC
metric families remain visible before any calls complete. `record_rpc_result`
records the request, duration and error together, using a registered static name
or a fixed fallback. Invalid durations are omitted from the histogram but still
count as requests and, when applicable, errors.

## Query examples

The metric names are unchanged, but now carry a method label. Sum across methods
for a node-wide total, and preserve the scrape target labels when comparing nodes:

```promql
sum by (instance) (rate(subhost_rpc_requests_total[5m]))
sum by (instance, method) (rate(subhost_rpc_errors_total[5m]))
histogram_quantile(0.95,
  sum by (instance, method, le) (
    rate(subhost_rpc_request_duration_seconds_bucket[5m])
  )
)
```

Existing consumers that expect unlabelled exposition rows must be updated. A
separate registry per server/node keeps their counts isolated; cloning `Metrics`
shares its registry and is the intended way to pass it to the exporter.

## Coverage limits

jsonrpsee 0.24.11 routes only decoded calls through RPC middleware. Notifications,
malformed JSON, invalid request envelopes, batch/body-limit rejections, and HTTP
authentication/rate-limit rejections bypass it. They do not change these call
counters or latency samples. Transport-level observability remains S8 work.

The middleware records a completed response. A cancelled call or process crash
before response completion produces no sample. These counters describe completed
calls and do not establish transaction inclusion, consensus finality or network
sync status.

Tests cover success/error counts, batch entries, bounded unknown labels,
HTTP/WebSocket recording, shared exporter handles, independent registries,
invalid durations and the transport cases that bypass call instrumentation.
