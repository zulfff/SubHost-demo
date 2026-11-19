# External audit package (S12.2 preparation)

Seat: **nara, Security & QA (track-b)**. Prepared 2026-11-19 against `main`.

> **Status: preparation only.** The external audit (S12.2) and the 30-day public
> four-validator soak (S12.3) are **not performed**. S12.2 is human-gated (an
> independent auditor must be engaged) and S12.3 is time-gated. This package
> assembles the evidence and scope an auditor needs; it does not claim an audit
> or soak has happened, and it does not close S12.2/S12.3.

## Audit scope (per ROADMAP S12.2)

Core, consensus, RPC, storage. Adjacent crates an auditor will also need:
`subhost-crypto`, `subhost-state`, `subhost-network`, `subhost-ibc`,
`subhost-mempool`, `subhost-node`, `subhost-cli`, `subhost-wallet`,
`subhost-keystore`, and `explorer/`.

## Start here (posture and design)

- `SECURITY.md`: authoritative, code-verified current posture and the enforced
  security properties (a regression in any is a genuine vulnerability).
- `docs/security/threat-model.md`: threats, mitigations, residual risks, and the
  implemented/not-implemented boundary.
- `docs/security/bug-bounty.md`: research scope, safe harbor, disclosure policy
  (no funded bounty).
- `docs/consensus.md`: phased prepare/commit consensus, timeouts/locks, recovery,
  native certified sync (S3.3), and remaining stages.
- `docs/architecture.md`, `docs/operations/certified-sync.md`,
  `docs/operations/producer-keystore.md`.
- Decisions: `docs/agents/decisions/`: full index below.

## ADR index (origin/main)

| ADR | Title | Audit relevance |
| --- | --- | --- |
| ADR-001 | block signature | Block producer authentication |
| ADR-002 | rpc auth node config | RPC auth/bind policy |
| ADR-003 | consensus commit contract | Final-commit boundary |
| ADR-004 | rpc metrics registry | Metrics surface (unauthenticated) |
| ADR-005 | local testnet voting recovery | Testnet recovery semantics |
| ADR-006 | certified-history relay | Archive replay / availability |
| ADR-007 | native certified fork accountability | Final-evidence classification, halt, no reorg; R-57-01 fix |
| ADR-008 | phased consensus / standard BLS | Prepare/commit, RFC 9380 suite, legacy forgery refusal |
| ADR-009 | pending transaction store | Mempool admission / volatility |
| ADR-010 | fee market v2 | Fee policy, base fee, settlement (SUBHOST-2026-003) |
| ADR-011 | encrypted producer keystore | Key-at-rest encryption (S9.6) |
| ADR-012 | rpc legacy fees | Legacy fee RPC compatibility |
| ADR-013 | platform observability | Metrics incl. `subhost_reorg_total` = 0 |
| ADR-014 | release build policy | Release / publication gates |

## Open residuals for the auditor

- **SUBHOST-2026-003 (High, partial: three-fix ladder):**
  - **Fix 2 (suffix-only validation): MERGED** (#50). Append-time O(history) replay
    is gone; `segmented::persist` validates only the appended suffix on the verified
    head.
  - **Fix 3 (automatic retention): MERGED** (#87, `93aeadc`) **with R-RET-01
    fixed** (#89, `3148a4e`). Snapshot-floor refusals write nothing, whole-segment
    trim with coverage invariant, manifest-before-cleanup atomicity, commit-lock
    serialization, fail-safe head cache. **No production caller yet:** the node
    trigger slice is pending (raka), so nothing prunes automatically on main.
    Reports: `2026-11-21-pr87-ledger-retention-review.md`,
    `2026-11-21-pr89-ret-height-self-check-review.md`.
  - **Fix 1 (calldata pricing / intrinsic gas): L2 reviewed; R-L2-03 open**
    (Q-037, ADR-015). Intrinsic gas as a validity floor on `gas_limit`;
    consumed stays `gas_limit`; conservation unchanged; refunds deferred to S10.
    R-L2-01 (docs overclaim) and R-L2-02 (protocol length cap in `check_floor`)
    both fixed. **R-L2-03 (blocking) open at head `94aa940`:** two node journal
    size fixtures encode `version: u32::MAX`, refused by the new encode-time
    header version check; one-line fixture fix recommended, APPROVE suspended
    pending fix + green gates. L3 (dewi) and L4 (bima) follow; accept-only until
    L4 binds the policy.
  The High closes only when fix 1 lands and the pricing + amplification mitigations
  are re-verified together (Q-024, Q-037).
- **R-RET-01 (Medium):** fixed in #89 (merged); height self-check in
  `block_at_height` so a concurrent online trim can never serve a wrong block;
  regression test pins the stale-base window. Landed before any node trigger.
- **R-57-01 (Medium):** fixed in #67 (merged); pruned historical evidence is
  unverifiable-not-fork; decision-table audited, no new halt vector.
- Roadmap gaps (documented, not findings): per-peer rate limiting / peer scoring
  (S7), encrypted mempool / Dandelion++ / threshold encryption, inactivity leaks,
  weak-subjectivity checkpoints, unbonding, validator-set rotation. Encrypted
  producer key for node start-up (S9.6) is **complete**: keystore #59, bootstrap
  API + hardened loader #79, CLI unlock flag #85 (all merged). The legacy
  plaintext loader remains the default without the credential flag (hardened,
  but unencrypted at rest).

S12.2 requires closing every High/Medium before the audit can be signed off.

## Build and gate evidence

Four gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace --all-features`,
`cargo-deny --all-features check`.

- `2026-11-19-four-gate-baseline/fa87215/`: green baseline (34 suites, 479 tests),
  with `SUMMARY.md`, per-gate logs, and `run.txt`.
- `2026-11-19-four-gate-baseline/982d3e6/`: green baseline at the prior tip.
- `2026-11-19-four-gate-baseline/a210f29/`: stale local-`main` run, labelled.
- `2026-11-19-four-gate-baseline/run-gates.sh`: reproducible runner.

Environment caveat (Q-029): `/usr/local/bin/cargo` is a wrapper forcing a
read-only `CARGO_HOME`; run deny via the direct binary
`RUSTUP_HOME=/usr/local/rustup CARGO_HOME=$HOME/.cargo CARGO=/usr/local/cargo/bin/cargo
~/.local/bin/cargo-deny --all-features check`. `--all-features` is a top-level
option. Worktrees live under `/home/zulfff/subhost-wt/` (not `/tmp`, which is
wiped mid-session).

## Findings register

| ID | Title | Severity | Status | Evidence |
| --- | --- | --- | --- | --- |
| SUBHOST-2026-001 | Block gas-cap bypass | High (8.7) | Fixed | `2026-11-18-gas-cap-bypass/`; re-verified on `fa87215` (Q-024) |
| SUBHOST-2026-002 | Wallet scrypt cost unbounded (abort) | High | Fixed (#49) | `2026-11-18-wallet-kdf-unbounded/`; bounded KDF in `subhost-wallet` |
| SUBHOST-2026-003 | RPC work amplification / unpriced calldata | High | Partial (fix2 merged; fix3 merged, trigger pending; fix1 L2 reviewed, L3/L4 pending) | `2026-11-19-rpc-work-amplification/`; fix2 #50, fix3 #87+#89, fix1 Q-037 L2 floor (Q-024) |
| R-RET-01 | Online retention stale-base wrong-block serving | Medium | Fixed (#89 merged) | `2026-11-21-pr87-ledger-retention-review.md`, `2026-11-21-pr89-ret-height-self-check-review.md` |
| R-57-01 | Pruning node false-halt on historical certs | Medium | Fixed (#67 merged) | `2026-11-19-pr57-native-sync-review.md` (fix verification) |
| R-59-01 | Keystore duplicate-field rejection gap | Low | Closed (#59) | `2026-11-19-pr59-keystore-review.md` (re-review) |

**Open items an auditor should prioritize:** SUBHOST-2026-003 fix 1 (calldata
pricing, Q-037 L2 reviewed; R-L2-01/R-L2-02 closed, R-L2-03 blocking fixture
regression open; L3/L4 pending). Fix 3
(automatic retention) is merged (#87) with R-RET-01 fixed (#89), but the node
trigger slice is still pending, so retention is not active on main yet; fix 2
(suffix validation) is merged. R-57-01 (Medium) is fixed (#67 merged; residual
note updated in #73). S12.2 requires closing every High/Medium.

## Security reviews performed (internal QA, not an external audit)

- `2026-11-19-pr57-native-sync-review.md`: S3.3 native sync + fork accountability
  (PR #57). Verified fail-closed classification, ledger-anchored serving, bounded
  catch-up; raised R-57-01.
- `2026-11-19-pr59-keystore-review.md`: S9.6 encrypted producer keystore (PR #59).
  First pass + re-review; R-59-01 closed; approved.
- `2026-11-19-pr65-storage-fee-replay-review.md`: S6.2 step 2 storage fee replay
  (PR #65). Replay+root check, fee conservation, policy binding; approved.
- `2026-11-20-pr79-producer-bootstrap-review.md`: S9.6 Track A bootstrap + hardened
  loader (PR #79). Fail-closed supplied-key bootstrap, PoP not bypassed, legacy
  loader TOCTOU + non-Unix bypass closed; approved.
- `2026-11-20-pr84-mempool-persist-review.md`: opt-in mempool persistence consumer
  (PR #84, Q-016/Q-038). Opt-in default-off, genesis-bound, fail-closed boot on
  foreign/corrupt snapshot; approved with R-84-01 (Low, CLI-flag doc divergence).
- `2026-11-20-pr85-credential-flag-review.md`: S9.6 completion, node
  `--producer-credential-file` unlock (PR #85). Fail-closed unlock, owner-only
  bounded credential, no fallback/keygen, #79 PoP path; approved. S9.6 complete.
- `2026-11-21-pr78-runtime-hooks-note.md`: S8 runtime hooks (PR #78, merged).
  Retroactive spot-check (merged without dedicated pre-review): bounded latency
  map, truthful health semantics, fail-safe disk gauges; no blocking issues.
- `2026-11-21-pr87-ledger-retention-review.md`: height-based online ledger
  retention (PR #87, merged). 003 fix 3; snapshot floor, atomicity, commit-lock
  serialization verified; approved with R-RET-01 (Medium, fixed in #89).
- `2026-11-21-pr89-ret-height-self-check-review.md`: R-RET-01 fix (PR #89,
  merged). Height self-check verified on main; both interleavings safe,
  regression test pins the race window; approved, R-RET-01 Fixed.
- `qa-review-queue.md`: current QA queue and closed items.

## Known limitations (documented, not undisclosed findings)

Opt-in JSON-RPC auth defaulting off; no in-process TLS; unauthenticated
metrics/faucet; single-key IBC light client (no rotation/multisig/non-membership);
membership-only state proofs; no per-peer rate limiting / peer scoring (S7);
encrypted mempool / Dandelion++ / threshold encryption not implemented; no
inactivity leaks / weak-subjectivity checkpoints / unbonding / validator-set
rotation; the legacy plaintext producer-key loader remains the default without
`--producer-credential-file` (S9.6 encrypted keystore is complete and recommended
for production, but the legacy path is unencrypted at rest). See `SECURITY.md`
"Known security posture" and the threat-model not-implemented list.

## What this package does NOT establish

- No independent third-party audit has been completed (S12.2 open).
- No 30-day public testnet soak has run (S12.3 open).
- Internal QA reviews and green gates are evidence of due diligence, not a
  substitute for the external audit or the soak.