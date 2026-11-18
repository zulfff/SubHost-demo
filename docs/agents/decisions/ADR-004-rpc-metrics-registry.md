# ADR-004: RPC metrics use an injected node registry

Status: Accepted · 2026-11-18

## Context

The node constructs a Prometheus registry and exposes it through its metrics
exporter, but the RPC server receives only chain state. The registered RPC
counters therefore never observe RPC calls. Constructing another registry in the
server would leave its values unexported and mix up registry ownership.

## Decision

Add `RpcServer::with_metrics(subhost_metrics::Metrics) -> Self` as an optional
builder. Existing `RpcServer::new(state)` callers remain compatible. The node
passes a clone of its existing metrics handle after this API contract lands:

```rust
let rpc = RpcServer::new(self.state.clone()).with_metrics(self.metrics.clone());
```

RPC middleware records one request, one execution-duration sample and, when
applicable, one error for each completed decoded call. HTTP and WebSocket calls
use the same middleware; each decoded batch member is recorded separately.
Classification follows `MethodResponse::is_error()`, rather than HTTP status or
transaction inclusion. A successfully admitted pending transaction is a
successful RPC result even while its receipt is absent.

The three existing RPC metric names gain a `method` label. Known labels come
from `RpcModule::method_names()`; all caller-supplied unknown names map to
`unknown`. IDs, addresses, parameters, tokens and error messages never become
labels. Existing `Metrics::record_request` and `record_error` signatures remain
available and use `unattributed`. `Metrics::record_rpc_result` records a completed
call using a registered static name or a fixed fallback.

## Consequences

Cloning the metrics handle shares one registry per node. Separate nodes and
server registries remain independent. Exposition consumers must accommodate the
new method label; aggregate queries can sum over methods.

The S5.3a implementation contains the RPC middleware, metrics support and this
API contract. Node bootstrap wiring follows the landed contract as S5.3b; until
then the bundled node's RPC counters remain zero. This change does not alter
consensus state, block commit rules or the ledger format.

jsonrpsee 0.24.11 skips RPC middleware for malformed envelopes, notifications,
batch/body-limit rejection and HTTP guard rejection. These are explicit
transport-observability limits for S8. A cancelled call or process crash before
response completion records no sample. Duration ends when the method response is
ready and excludes socket delivery time.

## Evidence

- https://docs.rs/jsonrpsee-server/0.24.11/jsonrpsee_server/middleware/rpc/trait.RpcServiceT.html — decoded-call middleware returns `MethodResponse` (verified 2026-11-18).
- https://github.com/paritytech/jsonrpsee/blob/v0.24.11/server/src/server.rs — notifications, parse failures and invalid requests bypass call middleware (verified against the cached 0.24.11 source, 2026-11-18).
- https://prometheus.io/docs/practices/instrumentation/ — label cardinality determines the number of time series (verified 2026-11-18).
- `crates/subhost-node/src/lib.rs`, `Node::run` — exporter owns `self.metrics` while the current RPC constructor receives only the state.
