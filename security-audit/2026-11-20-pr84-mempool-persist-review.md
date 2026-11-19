# Security review: PR #84, opt-in mempool persistence consumer (Q-016/Q-038)

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-20.
PR head: `s6.1b/mempool-persist-node` (fetched as `pr-84-review`), base `main`.
Scope: `crates/subhost-node/src/lib.rs` (+38), `basic_tests.rs` (+142),
`docs/operations/mempool-persistence.md`. Track A (raka). Wires the durable
pending store (landed earlier in the mempool crate) into node bootstrap.

## Assessment

**No blocking issues. APPROVE**, merge once the cargo gates are green (at review
time only Vercel/CodeRabbit checks had reported). One Low informational finding
(R-84-01, doc/code divergence on the CLI flag).

## What was verified

- **Opt-in, default off.** `mempool_persist: false` keeps the volatile pool and
  writes nothing. Tests: default-off config; opt-in required even when a valid
  snapshot exists; no `mempool.bin` created before the first admission.
- **Genesis binding.** `MempoolStore::open(chain_id, protocol_context, ...)` uses
  the same protocol context as the journal and certified history, so a fee-policy
  or calldata-schedule change makes an old snapshot foreign instead of silently
  reusable. Test: a syntactically valid snapshot bound to another context refuses
  boot with `genesis mismatch`.
- **Boot order (read in context).** The store is attached after ledger, journal,
  archive, fork-log reconciliation and genesis seeding; `restore_pending()` runs
  before metrics construction and traffic (Q-034 requirement 1), so the
  pending-depth gauge reports the restored pool.
- **Q-038 fail-closed boot policy.** Both `MempoolStore::open` and
  `restore_pending()` errors refuse boot via
  `NodeError::PendingSnapshot { path, detail }` rather than silently starting
  empty. Test: a corrupt snapshot refuses boot with `invalid snapshot header`.
  The operator-facing boot error names the exact file to remove.
- **Error-detail split.** Path and detail stay in the local boot error; RPC
  persistence errors keep no detail (no information leak over RPC).
- **Snapshot file hygiene.** The store writes `mempool.bin` owner-only (0o600,
  tested in the mempool crate). Contents are public signed transactions; no key
  material is persisted.

## Findings

- **R-84-01 (Low, informational, non-blocking).** The `NodeConfig.mempool_persist`
  doc comment says "The CLI flag is `--mempool-persist`", but no such flag exists
  in this PR or on `main` (`subhost-cli` is untouched). Either wire the flag or
  adjust the comment to say the CLI surface is pending; as written it promises a
  flag operators cannot use.

## Evidence

Read at PR head `pr-84-review`: `crates/subhost-node/src/lib.rs` (bootstrap
placement, `PendingSnapshot` error), `basic_tests.rs` (opt-in, foreign-genesis,
corrupt-snapshot, no-file-before-admission tests),
`crates/subhost-mempool/src/persistence.rs` (0o600 write). Related: Q-016,
Q-034, Q-038; #75/#80 pending-store wiring reviews.