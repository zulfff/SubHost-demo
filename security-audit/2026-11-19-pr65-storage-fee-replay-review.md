# Security review: PR #65 — S6.2 step 2 storage fee replay / policy binding / head anchoring

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-19.
PR head: `s6/storage-fee-replay` (fetched as `pr-65-review`), base `main`.
Scope reviewed: `crates/subhost-storage/src/{lib.rs,segmented.rs,tests.rs}` diff
(+953/-41), Q-030 fee-policy binding rules. ADR-002/004 changes are cosmetic
(antislop em-dash cleanup), not security-relevant.

## Assessment

**No blocking issues.** This is a careful, fail-closed extension of the ledger
validation path to fee-enabled (version 2) histories, with extensive failure-path
tests. Recommend merge once CI is green.

## What was verified

- **Replayed root, not trusted root.** A fee-enabled full validation
  (`validate_snapshot_from`) requires a committed `ReplayAnchor`, re-executes the
  offered blocks from it, settles fees per block, and rejects if
  `state.root() != offered.root()`. A fee-enabled history with no anchor is
  refused ("persist the genesis state first") rather than trusted.
- **Anchor integrity.** `committed_anchor` reads the committed snapshot state and
  requires the offered anchor block's `state_root` to equal the committed snapshot
  root; it refuses if the offered history does not carry the anchor.
  `ephemeral_anchor` requires the offered history to carry the cached head block
  with matching height and hash. The replay parent header is taken from the
  offered snapshot/retained history, never from the cache.
- **Fee conservation and schedule.** `replay_blocks` derives the fee context from
  the policy (`initial_context`/`next_context`), requires the recorded
  `base_fee_per_gas` to equal the derived one, sums debited fees with checked
  addition, requires `settle_block_fees(...).total == debited`, and credits the
  producer priority fees only after every transaction of the block (a tip is not
  spendable inside its own block). A legacy block mixed into a fee-enabled history
  is refused.
- **Policy binding (Q-030).** `bind_fee_policy` refuses: a supplied policy that
  differs from the recorded one; legacy v1 blocks in a fee-enabled ledger; v2
  blocks in a committed ledger that records no policy; binding a policy to a
  legacy ledger; persist dropping the recorded policy of a fee-enabled ledger;
  and any v2 history with no policy at all. `load` with no supplied policy uses the
  recorded one (offline tools); `persist` cannot drop it.
- **Append-only, durable and ephemeral.** `check_head_unchanged` rejects a
  shortened history and requires the carried head block to match height and hash.
  The suffix path (`validate_suffix_on`) only covers strict growth; an unchanged
  or shortened history falls back to full validation that re-checks the offered
  prefix. The verified head cache is written only after validation succeeds.
- **Ephemeral trust model.** With no manifest, the cache is the committed record;
  `validate_ephemeral` applies the same suffix/full sequence and append-only check
  against the cached head, and records the head only on success. Documented and
  consistent with the durable path.
- **Manifest compatibility.** `fee_policy` is `serde(default,
  skip_serializing_if = None)`: policy-less manifests stay byte-identical, and an
  older build denies the unknown field (refuses rather than misreads). Prune and
  write_manifest round-trip the policy.

## Test coverage (failure paths)

Verified present: tampered offered state (changed and unchanged history); suffix
not extending the verified head; suffix inconsistent with the committed genesis
state; replay of a transaction in the pruned prefix; nonce skip on durable and
ephemeral; wrong recorded base fee; uncredited producer; v2 without a committed
anchor; different supplied policy on load and persist; policy bound to a legacy
ledger; v2 blocks without a recorded policy; offline load uses recorded policy but
persist cannot drop it; prune round-trips the policy; policy-less manifest
byte-identical; v2 roots agree across durable/ephemeral and warm/cold; prune
refuses to drop the snapshot anchor block; pruned store rejects a full history.

## Informational notes (non-blocking)

- **Re-execution cost.** Fee-enabled full validation re-executes transactions, so
  cost is proportional to the replayed history. The verified-head suffix path
  amortises this for appends; a cold full validation of a large fee-enabled
  history is inherently expensive. Not a vulnerability; noted for operators.
- **Fee context gas limit.** `replay_blocks` derives the fee context using the
  parent (or first) block's `gas_limit` as the limit for the run. This matches a
  constant chain block gas limit; if a future policy allows the block gas limit to
  vary mid-history, confirm the context derivation tracks it. The current tests
  cover base-fee derivation; no defect found.

## Evidence

- Read at PR head `pr-65-review`: `crates/subhost-storage/src/lib.rs`
  (`validate_snapshot_from`, `bind_fee_policy`, `replay_blocks`, `ReplayAnchor`),
  `segmented.rs` (`load`, `persist`, `validate_appended_suffix`,
  `validate_ephemeral*`, `committed_anchor`, `ephemeral_anchor`,
  `check_head_unchanged`, manifest policy field), `tests.rs` (failure-path suite).
- Related: SUBHOST-2026-003 storage slice (Q-024); the verified-head cache landed
  in #50; fee-market core/state API in #60.
- Gates: four-gate baseline on `fa87215` green
  (`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`); PR CI re-runs on head.