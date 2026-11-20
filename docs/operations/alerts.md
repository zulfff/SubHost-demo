# Sample Prometheus alert rules

Sample rules for a Subhost node, grounded in the metrics the node actually
publishes. The exporter is unauthenticated: scrape it over loopback or a private
interface, or front it with an authenticating proxy (see
[observability.md](observability.md)). `SubhostNodeNotReady` assumes a blackbox
exporter probing `/health/ready`.

## Which signals are live today

The metrics API registers every family below, but a gauge only moves when a
call-site writes it. On `main` the node wires some and not others; enable the
gated rules only after their call-sites land, or they fire on a healthy chain.

| Signal | Metric | State on `main` |
| --- | --- | --- |
| Committed height | `subhost_block_height` | **live** (node commit path) |
| Pending pool depth | `subhost_pending_transactions` | **live** |
| Readiness/liveness | `/health/ready`, `/health/live` | **live** (`ledger_loaded && !syncing && consensus_alive`) |
| RPC requests/errors/latency | `subhost_rpc_*` | **live** |
| Mempool lifecycle | `subhost_mempool_transactions_total{event}` | **live** (pool-committed; `expired` moves on admission sweeps and on the 30 s node timer) |
| Finalized height | `subhost_finalized_height` | **live** (verified final Commit path; a single-writer demo chain publishes 0 by design) |
| QC commit latency | `subhost_qc_commit_latency_seconds` | **live** (samples only for proposals this node authenticated; catch-up commits add none) |
| Ledger disk footprint | `subhost_ledger_active_bytes` | **live** (`disk_stats` under the commit lock; a failed measurement keeps the last value) |
| Snapshot age | `subhost_ledger_snapshot_modified_seconds` | **live** (same measurement; 0 means unavailable, not an epoch) |
| Connected peers | `subhost_connected_peers` | **live**: transport connections, updated on each change; a connected peer may not have announced the topic yet |
| Reorg count | `subhost_reorg_total` | registered at 0, **no increment API**: reorgs unsupported; never alert on it |
| Pending-file cleanup failures | `subhost_mempool_pending_cleanup_failures_total` | **gated**: registered at 0 and *will* increment once the S6.1b node wiring lands (unlike reorgs, whose 0 is permanent). Enable `SubhostPendingCleanupFailing` with that wiring |
| Ledger prune outcomes | `subhost_ledger_prune_total{outcome}` | **gated**: registered at 0 (three bounded labels); recording is wired into `RpcState::retain_from` and increments once a retention caller drives it. Refusals are normal no-ops, not faults: no refusal alert |
| Ledger base height | `subhost_ledger_base_height` | **live at attach**: seeded from the restored base when the registry is attached; moved only by a real prune. 0 = full history retained (a real value, not unavailable) |

There is **no direct quorum-participation metric**. "No quorum" below is a
finality-stall proxy, not a measurement of votes; it is gated on
`subhost_finalized_height` and must be read with that limitation.

## Rules

```yaml
groups:
  - name: subhost
    rules:
      # --- live today ---
      - alert: SubhostNodeNotReady
        expr: probe_success{job="subhost-ready"} == 0
        for: 5m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} /health/ready is 503 for 5m"
          runbook: docs/operations/incident-checklist.md
      - alert: SubhostHeightStall
        expr: increase(subhost_block_height[10m]) == 0
        for: 15m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} committed no block for 15m"
          runbook: docs/operations/incident-checklist.md
      - alert: SubhostRpcErrorRate
        expr: sum by (instance) (rate(subhost_rpc_errors_total[5m])) > 5
        for: 10m
        labels: { severity: ticket }
        annotations:
          summary: "{{ $labels.instance }} >5 RPC errors/s for 10m"
      # Gated: enable only with the S6.1b node wiring. A committed block never
      # fails because of this; it means mempool.bin is stale, so accepted-but-
      # unmined transactions may not survive a restart. Ticket, not page.
      - alert: SubhostPendingCleanupFailing
        expr: increase(subhost_mempool_pending_cleanup_failures_total[15m]) > 0
        labels: { severity: ticket }
        annotations:
          summary: "{{ $labels.instance }} could not rewrite mempool.bin after a committed block"

      # --- gated: enable only after the node call-sites land ---
      - alert: SubhostNoQuorum
        expr: increase(subhost_finalized_height[10m]) == 0
        for: 15m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} finalized no block for 15m (finality-stall proxy)"
          description: >
            Proxy only: there is no quorum-participation metric. Finality can
            stall while the local tip still advances. Requires the
            subhost_finalized_height call-site; do not enable before it lands.
          runbook: docs/operations/incident-investigation.md
      - alert: SubhostFinalityLag
        expr: subhost_block_height - subhost_finalized_height > 10
        for: 10m
        labels: { severity: page }
        annotations:
          summary: "{{ $labels.instance }} finality trails the tip by >10 blocks"
          description: Requires subhost_finalized_height; until wired it is 0 and this fires on any committed chain.
      - alert: SubhostQcLatencyHigh
        expr: histogram_quantile(0.95, sum by (instance, le) (rate(subhost_qc_commit_latency_seconds_bucket[5m]))) > 30
        for: 10m
        labels: { severity: ticket }
        annotations:
          summary: "{{ $labels.instance }} p95 QC commit latency >30s"
      - alert: SubhostLedgerGrowth
        expr: delta(subhost_ledger_active_bytes[1h]) > 1e9
        for: 3h
        labels: { severity: ticket }
        annotations:
          summary: "{{ $labels.instance }} ledger grew >1 GiB/h for 3h"
          runbook: docs/operations/ledger-repair.md
```

## Notes

- A halted node (conflicting final evidence) keeps `/health/ready` at 503, so
  `SubhostNodeNotReady` pages on it. That is an accountability event, not a
  transient fault: follow
  [incident-investigation.md](incident-investigation.md), not a restart loop.
- A `catching_up` node suppresses production until it reaches the peer tip;
  readiness reflects this and a height stall during catch-up can be expected.
  Correlate with [certified-sync.md](certified-sync.md) before paging.
- `subhost_ledger_snapshot_modified_seconds == 0` means *unavailable*, never a
  real epoch time; do not build a "snapshot too old" alert on it until the
  call-site publishes a real value.
- Thresholds (15m stall, >10 finality lag, 1 GiB/h growth, p95 30s) are samples
  for a small testnet, not tuned production SLOs. Adjust to the chain's block
  cadence and retention policy.