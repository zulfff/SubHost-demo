# Security review: PR #87, height-based online ledger retention (003 fix 3, Q-036)

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-21.
PR head reviewed: `11e009a` (`s12/ledger-retention`). Merged as `93aeadc` on
`main`. Track B (dewi). Scope: `subhost-storage` `prune_to_height` +
`subhost-rpc` `retain_from` / atomic `base_height`, tests, docs.

## Assessment

**APPROVE**, one Medium finding (R-RET-01, latent until the node trigger
exists). The finding was accepted and fixed in follow-up PR #89 (merged
`3148a4e`); see `2026-11-21-pr89-ret-height-self-check-review.md`. The retention
mechanism has **no production caller yet**: the node trigger slice (raka) is
still pending, so nothing prunes automatically on `main` today.

## What was verified (static pre-review at 11e009a)

- **Snapshot floor refusals write nothing.** `prune_to_height` refuses when the
  target would drop the newest snapshot anchor
  (`StorageError::PruneWouldDropSnapshot`, segmented.rs:1145/1224 on merged
  main); the manifest is byte-identical after a refusal. Typed
  `PruneOutcome::RefusedBySnapshot` is a normal no-op, not an error that could
  halt or fail a commit.
- **Whole-segment trim with a coverage invariant.** Only whole segments below
  the target are dropped; the retained coverage check (segmented.rs:1237 region)
  guarantees the retained suffix still covers the snapshot anchor, so a prune
  can never strand the recovery path.
- **Atomic manifest-before-cleanup.** The new manifest is committed before any
  segment file is deleted, so an interrupted prune stays loadable (same
  discipline as the offline `prune`).
- **Commit-lock serialization.** `retain_from` takes `RpcState.commit_lock`
  (state.rs:295-296 on merged main), as do `persist` (state.rs:541) and `load`
  (state.rs:631): storage prune and the in-memory trim cannot race a commit.
  Storage itself documents the caller-held-lock requirement.
- **Durable-before-visible ordering.** `retain_from` prunes storage first and
  only trims the in-memory views on `Pruned`; refusal/failure leaves memory
  untouched. A crash between storage commit and trim is safe because `load()`
  rebuilds views from the retained suffix.
- **Fail-safe head cache.** A successful prune refreshes the cached
  `VerifiedHead.base_height` in place; any other cache/manifest mismatch keeps
  the existing fail-closed full-validation fallback. Warm-append after an online
  prune still takes the suffix path (regression test, storage tests.rs
  2051-2160 region at review time).
- **Atomic base across clones.** `base_height` moved to `Arc<AtomicU64>` with
  the same ordering as `block_height`; the gauge can publish the real base.

## Finding R-RET-01 (Medium, latent), fixed in #89

`block_at_height` read the atomic base outside the blocks lock, then indexed
the blocks vector. A concurrent trim between the two reads (old base, trimmed
vector) maps a pruned height onto a retained block and serves the **wrong
block**. Interleaving: reader loads old base -> trimmer trims blocks and stores
the new base under the write lock -> reader computes `height - old_base - 1`
into the trimmed vector.

Fix (recommended one-liner, landed in #89): filter the fetched block on
`header.height == height`, so every interleaving returns the correct block or
null. Regression test constructs the stale-base/trimmed-blocks window directly.
The fix also protects the node fork-evidence paths (false fork-conflict vector
removed). Condition attached to the verdict: land before the node trigger
slice. Satisfied: #89 merged before any trigger exists.

## Gate evidence

- Tech-lead local gates at `11e009a`: fmt, clippy `-D warnings`, 647 workspace
  tests, deny: green (reported in-thread, Q-036).
- nara independent four-gate run at `11e009a`: started, fmt green, clippy
  SIGTERM'd mid-run by an over-broad `pkill` from another seat (acknowledged in
  `docs/agents/replies/Q-037-track-a.md`). A killed gate is not green; the run
  was superseded rather than trusted.
- Superseding run: the merged retention files are byte-identical between
  `11e009a` and `93aeadc` (verified by diff; the only deltas were unrelated
  main commits #78/#86). The independent four-gate run at `671dc02` (#89 head =
  merged #87 + fix) therefore covers the merged retention stack; results are
  recorded in the #89 review note when complete.

## Limitations

- Static review plus gate evidence; no live multi-node prune race was executed.
  The regression test constructs the race window deterministically.
- The node trigger (prune-after-commit, failure never halts, boot warning below
  4096) is a separate slice and is reviewed when it lands. Demo single-writer
  mode caveat (committed != final) belongs to that trigger and the runbook.
