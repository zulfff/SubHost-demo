# Security review note: PR #89, height self-check in block_at_height (R-RET-01 fix)

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-21.
PR head reviewed: `671dc02` (`fix/ret-height-self-check`), rebased directly on
merged #87 (`93aeadc`). Merged as `3148a4e` on `main`. Track B (dewi).
Follow-up to #87, which merged before the R-RET-01 fix landed.

## Assessment

**APPROVE, no blocking findings. R-RET-01 (Medium) is Fixed.** Verified on
`main` at `3148a4e`: the self-check line is present verbatim
(`crates/subhost-rpc/src/state.rs:602`), and `state.rs` is byte-identical
between the reviewed head `671dc02` and the merged tip (empty diff).

## What was verified

- **Scope.** One functional line, an explanatory comment, one 28-line
  regression test; nothing else. Test count 647 -> 648 matches.
- **Both interleavings safe.** With the trim-then-store order in
  `trim_to_pruned_base`, a reader can see (old base, trimmed blocks): the index
  overshoots and the height filter turns every such read into null, never a
  wrong block. (new base, trimmed blocks) is the consistent mapping. The
  reverse order is unreachable given the ordering, but would also filter to
  null.
- **Regression test pins the failure mode**
  (`a_stale_base_read_never_serves_a_wrong_block`, state.rs:992 on merged
  main): blocks retained to [4,5,6] with base still 0; wrong-block heights 1-3
  -> null, transient null for height 4, exact mapping once base 3 is stored,
  height 3 null. This is the stale-base/trimmed-blocks state from the finding,
  not a happy-path proxy.
- **Adjacent serving paths checked, no further findings.**
  `eth_getBlockByNumber` resolves through the self-checked fetch
  (`resolve_block_height`, methods.rs:111); `eth_getBlockByHash` is a hash scan
  under one read guard; fee history selects by real header heights via
  `partition_point` under a single guard, so a stale base only widens or
  narrows the window, never mislabels a block.
- **Node callers degrade to null/skip, never wrong data.**
  `certified_tip_message` (node lib.rs:1009) skips at most one tick's
  BlockCommit broadcast (self-healing); historical finality evidence
  (lib.rs:1521) and `record_conflict` (lib.rs:1563) now get an honest None /
  `Hash::ZERO` sentinel instead of a wrong block, removing a false
  fork-conflict vector; sync relay (lib.rs:1622) breaks the page short and
  retries. The fix is a safety improvement beyond RPC serving.

## Informational

`head_height` (base + count) can skew transiently inside the trim window.
Metadata only; every consumer resolves through the self-checked path. Inherent
to the two-step atomic design, no action needed.

## Activation-gate consequence

The R-RET-01 verdict condition ("land before the node trigger slice") is
**satisfied**: #89 is merged and no production caller of `retain_from` exists
yet. The node trigger slice (raka) remains pending on session availability
(escalated by the tech lead); when it lands it is reviewed against the Q-036
contract (prune after commit, failure never halts, boot warning below 4096).

## Gate evidence

- Tech-lead gates at `671dc02`: fmt, clippy `-D warnings`, 648 workspace tests,
  deny: green (reported in PR #89).
- nara independent four-gate run at `671dc02` (worktree `qa-ret-fix-review`,
  logs `/tmp/opencode/retfix-gates/`): in flight at the time of this note
  (fmt green, clippy running). The merged retention stack is covered by this
  run because `state.rs` and the storage files are byte-identical to the
  reviewed/merged content. Results will be appended here when the run
  completes; a killed gate (exit 143) is never treated as green.
