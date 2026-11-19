# Observability: health, runtime metrics and alerts

Status: S8 Platform API (this document, ADR-013). The node runtime hooks that
feed the runtime gauges are Track A work and land after this API; until then
the new gauges read their registered defaults and `/health/ready` stays 503.
Nothing here infers readiness from peer counts or block height.

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
| `subhost_finalized_height` | gauge | Node commit path, only after a verified final Commit QC. Never from a peer announcement. |
| `subhost_qc_commit_latency_seconds` | histogram | Node, proposal to verified final commit. Invalid samples (NaN/negative/infinite) are dropped. |
| `subhost_ledger_active_bytes` | gauge | `LedgerStore::disk_stats()` under the commit lock. |
| `subhost_ledger_snapshot_modified_seconds` | gauge | Same measurement; Unix seconds. **0 means unavailable**, not a real epoch time. |
| `subhost_reorg_total` | counter | Registered at 0 with no increment API: reorgs are unsupported until a reviewed reorg protocol ships. Safety incidents are not reorgs. |
| `subhost_mempool_transactions_total{event}` | counter | Pool lifecycle: `admitted`, `replaced`, `evicted`, `expired` (S6.3). Labels come from `MempoolEvent` only. |

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

## Sample alert rules (S8.5)

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

`SubhostNodeNotReady` assumes a blackbox exporter probing `/health/ready`;
height/finality alerts assume the Track A runtime hooks have landed. Until
then `subhost_finalized_height` stays 0 and `SubhostFinalityLag` would fire on
any committed chain — enable it only after the hooks ship.

## Limitations

- Node/consensus/network call-sites for the runtime gauges and health
  predicates are Track A work (Q-020); this API publication changes no
  A-owned source.
- OTLP tracing export (S8.1) and RPC method spans are later S8 slices.
- The exporter is unauthenticated: bind loopback or a private interface, or
  front it with an authenticating proxy. `/health/*` reveals readiness state.
