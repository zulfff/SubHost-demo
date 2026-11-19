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

- `SECURITY.md` — authoritative, code-verified current posture and the enforced
  security properties (a regression in any is a genuine vulnerability).
- `docs/security/threat-model.md` — threats, mitigations, residual risks, and the
  implemented/not-implemented boundary.
- `docs/security/bug-bounty.md` — research scope, safe harbor, disclosure policy
  (no funded bounty).
- `docs/consensus.md` — phased prepare/commit consensus, timeouts/locks, recovery,
  native certified sync (S3.3), and remaining stages.
- `docs/architecture.md`, `docs/operations/certified-sync.md`,
  `docs/operations/producer-keystore.md`.
- Decisions: `docs/agents/decisions/` — full index below.

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

- **SUBHOST-2026-003 (High, partial):** storage slice fixed; calldata pricing /
  O(history) replay work amplification remainder is open with Track A (Q-024).
- **R-57-01 (Medium):** fixed in #67 (merged) — pruned historical evidence is
  unverifiable-not-fork; decision-table audited, no new halt vector.
- Roadmap gaps (documented, not findings): per-peer rate limiting / peer scoring
  (S7), encrypted mempool / Dandelion++ / threshold encryption, inactivity leaks,
  weak-subjectivity checkpoints, unbonding, validator-set rotation, encrypted
  producer key for node start-up (S9.6 bootstrap pending).

S12.2 requires closing every High/Medium before the audit can be signed off.

## Build and gate evidence

Four gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace --all-features`,
`cargo-deny --all-features check`.

- `2026-11-19-four-gate-baseline/fa87215/` — green baseline (34 suites, 479 tests),
  with `SUMMARY.md`, per-gate logs, and `run.txt`.
- `2026-11-19-four-gate-baseline/982d3e6/` — green baseline at the prior tip.
- `2026-11-19-four-gate-baseline/a210f29/` — stale local-`main` run, labelled.
- `2026-11-19-four-gate-baseline/run-gates.sh` — reproducible runner.

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
| SUBHOST-2026-003 | RPC work amplification / unpriced calldata | High | Partial | `2026-11-19-rpc-work-amplification/`; storage-only slice fixed, calldata pricing open with Track A (Q-024) |
| R-57-01 | Pruning node false-halt on historical certs | Medium | Fixed (#67 merged) | `2026-11-19-pr57-native-sync-review.md` (fix verification) |
| R-59-01 | Keystore duplicate-field rejection gap | Low | Closed (#59) | `2026-11-19-pr59-keystore-review.md` (re-review) |

**Open items an auditor should prioritize:** the calldata pricing remainder of
SUBHOST-2026-003. R-57-01 (Medium) is fixed in #67 (pending merge). S12.2 requires
closing every High/Medium.

## Security reviews performed (internal QA, not an external audit)

- `2026-11-19-pr57-native-sync-review.md` — S3.3 native sync + fork accountability
  (PR #57). Verified fail-closed classification, ledger-anchored serving, bounded
  catch-up; raised R-57-01.
- `2026-11-19-pr59-keystore-review.md` — S9.6 encrypted producer keystore (PR #59).
  First pass + re-review; R-59-01 closed; approved.
- `qa-review-queue.md` — current QA queue and closed items.

## Known limitations (documented, not undisclosed findings)

Opt-in JSON-RPC auth defaulting off; no in-process TLS; unauthenticated
metrics/faucet; single-key IBC light client (no rotation/multisig/non-membership);
membership-only state proofs; no per-peer rate limiting / peer scoring (S7);
encrypted mempool / Dandelion++ / threshold encryption not implemented; no
inactivity leaks / weak-subjectivity checkpoints / unbonding / validator-set
rotation; encrypted producer key for node start-up pending (S9.6, keystore crate
landed but bootstrap not wired). See `SECURITY.md` "Known security posture" and
the threat-model not-implemented list.

## What this package does NOT establish

- No independent third-party audit has been completed (S12.2 open).
- No 30-day public testnet soak has run (S12.3 open).
- Internal QA reviews and green gates are evidence of due diligence, not a
  substitute for the external audit or the soak.