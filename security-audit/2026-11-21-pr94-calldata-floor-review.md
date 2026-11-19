# Security review: PR #94, calldata pricing floor + version-3 header codec (Q-037 L2, ADR-015)

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-21.
PR head: `s6/calldata-floor-api`, final head `6cd6ea1` (rebased onto
`84a1639`; code reviewed at `73af84e`, verified content-identical).
Scope: `subhost-core` calldata policy + header codec, genesis binding,
proposal-budget fixtures. Track A (bima). This is fix 1, layer L2 of the
SUBHOST-2026-003 three-fix ladder; accept-only until L4 binds the policy.

## Assessment

**APPROVE. All three findings closed.** Formal review posted on PR #94.
Merge unblocks L3 (dewi) then L4 (bima). The High (SUBHOST-2026-003) stays
open until fix 1 L2-L4 are merged and pricing + amplification are
re-verified together.

## What was verified

- **Intrinsic gas as validity floor.** `CalldataPolicy::check_floor` makes
  the calldata gas cost a validity floor on `gas_limit`; consumed gas stays
  `gas_limit`; fee conservation unchanged; refunds deferred to S10. The
  floor is a pure function at a single choke point: admission, selection,
  replay and peer-block acceptance all inherit it as consensus validity
  once L4 binds the policy.
- **Protocol length cap (R-L2-02 fix).** `check_floor` refuses
  `data.len() > MAX_TX_DATA_BYTES` (128 KiB) BEFORE the gas comparison,
  with a distinct `FeeError::CalldataTooLarge`. Boundary tests: exactly MAX
  is valid at its floor; MAX+1 is refused even with the gas floor met.
- **Version-3 header codec.** Accept-only: v3 headers decode, v2 production
  is refused on a calldata-bound genesis per the tech-lead L4 ruling
  (`header_version()` = 3 whenever a calldata policy is bound). Encode-time
  version strictness (`version == 0 || version > CALLDATA_HEADER_VERSION`
  refused) is correct fail-closed hygiene.
- **Genesis binding.** The policy binds through genesis; genesis layouts
  and fee continuity untouched by the fix deltas (patch identity checked at
  each step).
- **Scope discipline.** ADR-015/ROADMAP corrected to accept-only-until-L4
  (R-L2-01); no consensus-rule activation in L2 itself.

## Findings ledger

- **R-L2-01 (Low, docs overclaim).** ADR-015/ROADMAP implied the floor was
  active before L4. Fixed at `8a5f995` (accept-only wording). Closed.
- **R-L2-02 (Medium, protocol length cap).** The 128 KiB upper bound was a
  mempool-local cap, not consensus validity; a peer block could carry
  oversized calldata once the policy binds. Fixed at `56690b7`:
  `check_floor` refuses `data.len() > MAX_TX_DATA_BYTES` before the gas
  comparison, distinct typed error, boundary-tested at MAX and MAX+1.
  Closed.
- **R-L2-03 (blocking, test regression).** My independent gates at
  `94aa940` were test-RED (exit 101): two node-journal size-only fixtures
  (`proposal_budget_tests.rs`) encoded the sentinel `version: u32::MAX`,
  refused by the new encode-time version check. `--no-fail-fast`
  enumeration confirmed exactly these 2 failures (667 passed
  workspace-wide), nothing else latent. The encode strictness itself is
  correct; the fixture sentinel was the bug. Fixed at `73af84e`:
  sentinel -> `CALLDATA_HEADER_VERSION` (fixint u32, 4 bytes both, framing
  unchanged; the test self-measures with the production codec). Fixture-
  only delta (+7/-2), no production code touched. Closed on both gate
  runs below.

## Independent gate evidence (nara)

Detached script-file runs, four OK markers + exit line verified; a killed
gate is never green.

- `94aa940` (red baseline): fmt/clippy/deny OK, test=101; enumeration
  667/2. Logs `/tmp/opencode/l2-final-gates/`.
- `73af84e` (fix): **ALL GREEN, 669 passed / 0 failed**, 36 suites incl.
  integration + doc tests; deny OK (advisories, bans, licenses, sources).
  Both R-L2-03 fixtures pass. Logs `/tmp/opencode/l2-fix-gates/`.
- Final head `6cd6ea1`: verified **content-identical** to `73af84e` outside
  CHANGELOG (cumulative patches differ only in blob indexes and one hunk
  offset; the CHANGELOG delta is main's #91 entry). bima's final-head
  markers: ALL GREEN + exit 0, 676/0 (the +7 vs 669 are main's #84/#91
  tests now in the base). My green run therefore covers the final head.

Cache note: both runs were wrapper-free (sccache bypass window); the cache
was re-enabled by tech-lead decision 2026-11-20 and in-flight runs finished
as-is per that decision.

## Activation-gate context

L2 is accept-only: it does not by itself activate calldata pricing on any
chain. L3 (dewi) wires the policy into the fee path and L4 (bima) binds it
at production; only then does the floor become consensus validity. The
public testnet activation gate (S12.3) additionally requires the 30-day
soak and the external audit (S12.2), both human/time-gated and not
performed here.

## Evidence

Read at PR heads `94aa940`, `73af84e`, `6cd6ea1`:
`crates/subhost-core/src/calldata.rs` (`CalldataPolicy::check_floor`,
`MAX_TX_DATA_BYTES`, `CalldataTooLarge`),
`crates/subhost-core/src/lib.rs` (`BlockHeader` encode-time version check
in the Serialize impl), `crates/subhost-core/src/fee.rs`,
`crates/subhost-node/src/proposal_budget_tests.rs` (size fixtures),
genesis binding call sites. Threads:
`docs/agents/replies/Q-037-track-b.md` (raise/fix/green ledger),
`Q-037-track-a.md` (author gate records). Related: Q-024, Q-037, ADR-015
(lands with this PR), `2026-11-19-rpc-work-amplification/`.