# Observability: health, runtime metrics and alerts

Status: S8 Platform API (this document, ADR-013) plus the Track A node runtime
hooks (Q-020/Q-032): health predicates come from actual runtime state, finalized
height and QC latency are published at the verified final commit path, and the
ledger footprint/snapshot age are measured periodically under the commit lock.
`subhost_connected_peers` is the remaining gated gauge (needs a network handle
accessor). Nothing here infers readiness from peer counts or block height.

## Health endpoints

The metrics exporter (`subhost_metrics::Metrics::router`, default
`127.0.0.1:9090`) serves:

| Route | Meaning |
| --- | --- |
| `GET /health` (legacy) | 200 `ok` — the exporter process answers. |
| `GET /health/live` | 200 `ok` — same liveness attestation, explicit name. |
| `GET /health/ready` | 200 `ready` only when the node-published snapshot satisfies `ledger_loaded && !syncing && consensus_alive`; otherwise 503 `not ready`. |

`Metrics::set_health(HealthStatus { ledger_loaded, syncing, consensus_alive })`
publishes one coherent snapshot; the default is **not ready**. The predicates
(agreed in Q-020):

- `ledger_loaded` — genesis, ledger, validator registry and the newest
  journal/archive recovery have all reconciled successfully.
- `syncing` — a native catch-up range is active or waiting to complete.
  Ordinary peer absence is not syncing. A fatal sync/archive safety incident
  clears readiness rather than pretending sync completed.
- `consensus_alive` — the configured block pipeline task started and has not
  exited or reported a fatal error. For a signing validator this is the
  reconciled driver/journal service loop; for an observer the non-signing
  consensus ingress/verification loop; for the explicit single-writer demo the
  local sealer/acceptance loop. This is operational liveness, **not BFT
  finality**. It is cleared before controlled shutdown and on fatal task exit.

Liveness never implies readiness. Wire readiness probes to `/health/ready`
and liveness probes to `/health/live`.

## Runtime metrics

All names live in the one `Metrics` registry (ADR-004); cloning shares it.

| Metric | Type | Recorded by |
| --- | --- | --- |
| `subhost_finalized_height` | gauge | Node commit path, only after a verified final Commit QC (`RpcState::finalized_height`, set at `commit_qc`/`restore_committed_qc`). Never from a peer announcement. Registered at 0 until a verified final commit exists; an explicit single-writer demo chain publishes 0 by design, not as a broken hook. |
| `subhost_qc_commit_latency_seconds` | histogram | Node, proposal to verified final commit. Invalid samples (NaN/negative/infinite) are dropped. |
| `subhost_ledger_active_bytes` | gauge | `LedgerStore::disk_stats()` under the commit lock. |
| `subhost_ledger_snapshot_modified_seconds` | gauge | Same measurement; Unix seconds. **0 means unavailable**, not a real epoch time. |
| `subhost_reorg_total` | counter | Registered at 0 with no increment API: reorgs are unsupported until a reviewed reorg protocol ships. Safety incidents are not reorgs. |
| `subhost_mempool_transactions_total{event}` | counter | Pool lifecycle: `admitted`, `replaced`, `evicted`, `expired` (S6.3). Labels come from `MempoolEvent` only. Counts pool-committed admissions only; see below. |
| `subhost_mempool_pending_cleanup_failures_total` | counter | Failed best-effort `mempool.bin` writes after an already durable block commit (S6.1b). Registered at 0 and **will increment** once the consumer wiring lands — unlike `subhost_reorg_total`, whose 0 is permanent. Not a pool lifecycle event, so it is deliberately separate from `transactions_total{event}`. Any increase means the pending file is stale while the chain is fine: ticket, not page. |
| `subhost_ledger_prune_total{outcome}` | counter | Ledger retention attempts (Q-036), recorded by `RpcState::retain_from`. Labels come from `LedgerPruneOutcome` only: `pruned`, `nothing_to_prune`, `refused_by_snapshot` — no height labels, and errors are not an outcome. Registered at 0 with all three series; increments once a retention caller drives `retain_from`. A `refused_by_snapshot` no-op is a **normal** outcome (the newest snapshot is the hard floor), not a fault — do not alert on refusals alone; `subhost_ledger_active_bytes` is the operator growth signal. |
| `subhost_ledger_base_height` | gauge | Oldest retained block height (Q-036). Seeded from the current base when the registry is attached (a restored, already-trimmed ledger publishes its real base), then moved only by a `Pruned` outcome. **0 is a real value** meaning full history retained, not "unavailable". |

Existing families (`subhost_rpc_*`, `subhost_connected_peers`,
`subhost_block_height`, `subhost_pending_transactions`) are unchanged; see
`docs/operations/rpc-metrics.md` for the RPC contract.

### Ledger disk measurement

`LedgerStore::disk_stats() -> LedgerDiskStats { active_file_bytes,
snapshot_modified }` reads the validated manifest plus filesystem metadata —
no block replay, no checksum verification. It is an operational measurement,
not block/QC authentication.

- Counts only the manifest, the snapshot it names and the segments it names.
  Backups (`node-state.bin.v2-backup`) and orphan files are excluded.
- A missing named file or an invalid manifest is an error, never a false zero.
- Ephemeral and fresh stores report empty metadata; a legacy single-file
  ledger reports that file's size and `snapshot_modified: None`. A legacy
  file deleted after open also reports empty metadata (fresh vs deleted is
  indistinguishable without extra state); segmented stores error instead.
- The caller serializes measurement with persist/prune under the node's
  commit lock.

### Mempool lifecycle counting

`admit()` buffers typed events inside the pool instead of touching the
registry. The submit path admits into a cloned candidate pool and drops the
clone when a later step (dry-run build or seal) fails, so buffered events
vanish with rejected candidates; only the committed write-back publishes
them. The `eth_sendTransaction` handler drains the live pool's buffer after a
successful submit, recording into the registry when one is attached and
discarding otherwise (`Mempool::drain_events`), so a server without metrics
cannot grow the buffer. The counters therefore track **pool-committed
admissions**, not ledger commits: on the demo receipts-missing path
`seal_block` can commit the block to the ledger and then return an error
before the candidate pool is written back (`block.rs`); the live pool keeps
the mined transactions and the buffered events drop, since the handler
flushes only on `Ok` (a pre-existing Track A commit-path inconsistency,
tracked as A follow-up). Idempotent resubmissions
and lower-price rejections buffer nothing; mined removals are not lifecycle
events.

`expired` stays at 0: no TTL/expiry path exists in the pool or node yet. It
wires when the S6.3 expiry lifecycle lands.

## RPC method spans (S8.1)

Every decoded JSON-RPC call on HTTP and WebSocket emits one `info` span
through the existing `tracing` API, riding the same middleware as the RPC
metrics (Q-033, contract Q-020):

- Span name `rpc_call`, target `subhost_rpc` (tracing requires constant span
  names; the method travels in a field).
- Fields: `rpc.method` — the registered static method name or `unknown`,
  never the caller's raw string; `rpc.outcome` — `ok`/`error`;
  `rpc.duration_ms`; and `rpc.error_code` on failure.
- **Privacy boundary:** params, payloads, tokens, passwords and keys never
  enter a span. Tests assert caller-derived input appears in no span field.
- Bypass semantics match the metrics: notifications, malformed JSON and HTTP
  guard rejections (e.g. 401) emit no span; a batch emits one span per call.

Visibility belongs to the installed subscriber, not a feature flag: with no
subscriber the spans are near-free no-ops, and the default `fmt` layer does
not log span events, so default log output is unchanged. An env-filtered or
OTLP subscriber (the opt-in S8.1 export slice) makes them visible.

Caveat: spans exist only when a metrics registry is supplied via
`RpcServer::with_metrics` — they ride that middleware. The node always
supplies one; a library consumer running without metrics gets no spans.

## OTLP export wiring (`subhost` CLI, S8.1 / Q-035)

The `subhost` binary compiles the `subhost-telemetry` `otlp` feature in, but
export stays **off until an endpoint is configured** — default behaviour and
log output are unchanged, and no exporter is constructed.

- Enable with the global flag `--otlp-endpoint <URL>` or the environment
  variable `SUBHOST_OTLP_ENDPOINT`, on any subcommand (e.g.
  `subhost node --otlp-endpoint http://localhost:4318/v1/traces ...`).
- The endpoint is used **verbatim**, so include the signal path `/v1/traces`.
  The per-export timeout defaults to 10 s; the exporter honours
  `OTEL_EXPORTER_OTLP_TIMEOUT`.
- Telemetry is installed inside the tokio runtime (the batch processor needs a
  runtime context). The shutdown guard is retained for the process lifetime;
  on exit it flushes pending spans.
- Exporter construction or install failure is reported as a warning and falls
  back to plain logging — never fatal to boot. An empty endpoint value behaves
  exactly like unset.
- What exports is governed by the same per-layer `EnvFilter` as the logs
  (`RUST_LOG`, else the verbosity directive): info spans such as `rpc_call`
  pass the default. Only bounded operational fields enter spans, never secrets.
- The `sync-relay` example still uses plain logging; relay span export is a
  follow-up slice.

## Sample alert rules (S8.5)

The canonical, expanded rule set — including a no-quorum finality-stall proxy
and a table of which metrics are live versus still gated on node call-sites —
is in [alerts.md](alerts.md). The snippet below is the original quick summary;
prefer `alerts.md` so the two do not drift.

```yaml
groups:
  - name: subhost
    rules:
      - alert: SubhostNodeNotReady
        expr: probe_success{job="subhost-ready"} == 0
        for: 5m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} /health/ready is 503 for 5m"
      - alert: SubhostHeightStall
        expr: increase(subhost_block_height[10m]) == 0
        for: 15m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} committed no block for 15m"
      - alert: SubhostFinalityLag
        expr: subhost_block_height - subhost_finalized_height > 10
        for: 10m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} finality trails the tip by >10 blocks"
      - alert: SubhostQcLatencyHigh
        expr: histogram_quantile(0.95, sum by (instance, le) (rate(subhost_qc_commit_latency_seconds_bucket[5m]))) > 30
        for: 10m
        labels: { severity: ticket }
      - alert: SubhostLedgerGrowth
        expr: delta(subhost_ledger_active_bytes[1h]) > 1e9
        for: 3h
        labels: { severity: ticket }
        annotations:
          summary: "{{ $labels.instance }} ledger grew >1 GiB/h for 3h"
      - alert: SubhostRpcErrorRate
        expr: sum by (instance) (rate(subhost_rpc_errors_total[5m])) > 5
        for: 10m
        labels: { severity: ticket }
```

`SubhostNodeNotReady` assumes a blackbox exporter probing `/health/ready`.
The node runtime hooks have landed, so `subhost_finalized_height` carries a real
value on validator and observer chains and `SubhostFinalityLag` can be enabled.
An explicit single-writer demo chain publishes finalized height 0 by design (no
quorum certificates exist), so that alert must stay disabled for demo deployments.

## Limitations

- Node/consensus call-sites for the runtime gauges and health predicates are
  landed (Q-020/Q-032). `subhost_connected_peers` still needs a network handle
  accessor and stays 0 until then.
- OTLP tracing export provider (S8.1) is published behind the `subhost-telemetry`
  `otlp` feature (Q-035): `otlp::init_with_otlp` installs the export layer and
  returns a retained shutdown guard. The `subhost` CLI now wires it (global
  `--otlp-endpoint` / `SUBHOST_OTLP_ENDPOINT`, opt-in at runtime); RPC method
  spans (#69) export once an endpoint is set. The Track A block/consensus spans
  and the sync-relay export wiring follow.
- The exporter is unauthenticated: bind loopback or a private interface, or
  front it with an authenticating proxy. `/health/*` reveals readiness state.
