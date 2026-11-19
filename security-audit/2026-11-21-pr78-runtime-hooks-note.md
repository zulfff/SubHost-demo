# Security note: PR #78, S8 runtime hooks (retroactive spot-check)

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-21.
Merged as `36c41d5` on `main` (Track A, sari; Q-020/Q-032, ADR-013). Scope:
`subhost-node` runtime health/finality/disk gauges consuming the accessor
contract landed in #70, plus `runtime_health_tests.rs` (9 tests) and docs.

## Scope limitation (honest status)

#78 merged **without a dedicated nara pre-review**; this was tracked as a
low-priority gap in the QA queue. This note is a retroactive spot-check of the
merged diff, not a full independent four-gate review. Gate evidence at merge
time is the author's/tech-lead's; no nara gate run was performed for #78
specifically (the merged content is exercised by later workspace gate runs,
including the retention-stack run at `671dc02`).

## Spot-check results (no blocking issues found)

- **Bounded latency state.** Proposal-to-final-commit latency is keyed by view
  in a map capped at `MAX_LATENCY_VIEWS = 128` with oldest-view eviction;
  retransmissions do not restart the clock (`contains_key` guard); the sample
  is removed on commit. No unbounded growth from peer traffic.
- **No secret labels.** The hooks publish numeric gauges only
  (`set_ledger_disk_stats(active_file_bytes, snapshot_modified)`, finalized
  height, health booleans). The view-keyed map is in-memory, never exported as
  a label. Metric label sets were fixed in the reviewed #70 contract.
- **Truthful health semantics.** `ledger_loaded` is set only after recovery
  reconciles (was hardcoded true); `consensus_alive` = pipeline task liveness
  && !halted, cleared before controlled shutdown, on pipeline exit and on RPC
  start failure, so a dead pipeline is distinguishable from a safety halt.
  Demo single-writer mode counts the RPC sealer loop as its block pipeline.
- **Fail-safe disk measurement.** `publish_disk_stats` keeps the previous gauge
  value when `ledger_disk_stats()` fails; a failed measurement never publishes
  a false zero. Measurement runs under the commit lock on the 5s ticker.
- **Finality source.** Finalized height is published from
  `RpcState::finalized_height()` (verified final Commit boundaries only), at
  commit and on the ticker; catch-up commits yield no latency sample.
- **Test surface.** 9 unit tests in `runtime_health_tests.rs`; the only
  existing-test change is one additive line in `basic_tests::node_commit` for
  the new optional `CommitContext.runtime` field.

## Residual

None raised. If a future review of the node trigger or metrics slices touches
these hooks, re-check the health-semantics invariants above.
