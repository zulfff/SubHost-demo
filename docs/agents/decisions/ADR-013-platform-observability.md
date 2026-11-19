# ADR-013: Platform observability API — health, runtime metrics, disk stats

Status: Accepted · 2026-11-19

## Context

Q-020 (agreed) defines the S8 Platform consumer contract: the node needs
truthful runtime readiness and operational measurements without duplicating
node/consensus/network implementation in Platform code. The exporter's
`/health` was an unconditional `ok`, and no finality, ledger-footprint or
mempool-lifecycle measurements existed. Track A installs runtime hooks only
after this API is published.

## Decision

`subhost-metrics` adds, additively (existing names and recorder signatures
unchanged):

- `HealthStatus { ledger_loaded, syncing, consensus_alive }` and
  `Metrics::set_health` / `Metrics::health`. One coherent snapshot replaces the
  previous one; the default is not ready. Readiness is exactly
  `ledger_loaded && !syncing && consensus_alive` for validator, observer and
  explicit single-writer demo modes. `consensus_alive` is operational
  block-pipeline liveness, not BFT finality (semantics: Q-020-track-a).
- Exporter routes: `/health` (legacy) and `/health/live` attest only that the
  exporter answers; `/health/ready` returns 200 only for a ready snapshot,
  else 503.
- `subhost_finalized_height` (gauge), `subhost_qc_commit_latency_seconds`
  (histogram, invalid samples dropped), `subhost_ledger_active_bytes` and
  `subhost_ledger_snapshot_modified_seconds` (gauges; timestamp 0 means
  unavailable), `subhost_reorg_total` (counter registered at 0, deliberately
  without an increment API until a reviewed reorg protocol exists), and
  `subhost_mempool_transactions_total{event}` with the bounded labels
  `admitted`/`replaced`/`evicted`/`expired` from a `MempoolEvent` enum.

`subhost-storage` adds `LedgerStore::disk_stats() -> LedgerDiskStats
{ active_file_bytes, snapshot_modified: Option<SystemTime> }`. It reads the
validated manifest's own file references plus filesystem metadata — no replay,
no checksum verification; this is measurement, not authentication. Backups and
orphans are excluded; missing/invalid active data is an error, never a false
zero; ephemeral/fresh stores report empty metadata. The caller serializes it
with persist/prune under the existing commit lock.

## Consequences

Track A owns all node/consensus/network call-sites: publishing health
predicates from actual runtime state, advancing finalized height only on
verified final Commit evidence, QC latency, and periodic disk-stat measurement.
Until those hooks land, the new gauges read their defaults and `/health/ready`
stays 503; operator alerts that assume the hooks must stay disabled (see
`docs/operations/observability.md`).

Unknown or unavailable measurements remain absent or zero with explicit
documentation, never inferred from unauthenticated peer announcements. Safety
incidents clear readiness; they are not relabelled as reorgs. OTLP tracing
export and RPC method spans are later S8 slices under the same contract.

## Evidence

- `docs/agents/questions/agreed/Q-020-platform-observability-contract.md` and
  `docs/agents/replies/Q-020-track-a.md` — agreed API, predicate semantics and
  hook ordering (2026-11-19).
- https://prometheus.io/docs/practices/instrumentation/ — counter/gauge/
  histogram semantics, bounded labels, timestamps rather than time-since-event
  gauges (verified 2026-11-19 via Q-020).
- ADR-004 — cloned metrics share one registry; ADR-008 — only verified final
  Commit evidence may advance canonical state/finality measurements.
- `crates/subhost-storage/src/segmented.rs`, `read_manifest` — structural
  manifest validation without block replay, reused by `disk_stats`.