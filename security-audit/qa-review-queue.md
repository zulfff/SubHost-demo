# QA review queue: seat nara (track-b)

Updated 2026-11-20 (tech-lead direction). Evidence logs under
`2026-11-19-four-gate-baseline/` are kept intact (only `target/` build dirs were
cleaned for disk space).

## Incoming wave (active)

| Item | Branch / PR | Scope | State | QA focus |
| --- | --- | --- | --- | --- |
| PR #69 | `s8.1` RPC method spans | metrics middleware spans (Q-033 / Q-020) | OPEN, approved, yoga rebasing | Track B (yoga). Re-review after rebase; span cardinality / no secret labels / overhead. |
| bima step 3 | (publishes when gates finish) | fee-market / calldata step 3 | pending publish | Track A (bima). Review on publish; ties to SUBHOST-2026-003 remainder. |
| sari accessor contract + hooks | (incoming) | state accessor contract + hooks | pending | Track A (sari). Contract boundary + hook safety. |
| tania OTLP | (incoming) | OTLP telemetry export | pending | Track B (tania). Export surface auth / no secret leakage. |

## Tracked blocker

- **SUBHOST-2026-003 remainder (High):** calldata pricing / O(history) replay work
  amplification. Storage slice fixed (#50/#65); the calldata-pricing contract
  thread is queued to bima (tech-lead). **Must close before public testnet
  activation (S12.3).** Evidence: `2026-11-19-rpc-work-amplification/`, Q-024.

## Recently reviewed and merged

| PR | Scope | Outcome |
| --- | --- | --- |
| #67 `s3.4/pruned-history-and-backup` | R-57-01 fix + archive restore coherence | MERGED. R-57-01 closed; decision-table audited, no new halt vector. |
| #65 `s6/storage-fee-replay` | S6.2 step 2 storage fee replay | MERGED. No blocking issues. |
| #64 `qa/s12.4-bug-bounty` | S12.4 bounty/disclosure docs | MERGED. |
| #63 `qa/s12-sync-fork-refresh` | S12.1 threat-model + SECURITY.md reconcile | MERGED. (Follow-up: drop the R-57-01 residual note now that #67 landed.) |
| #59 `yoga/encrypted-producer-keystore` | S9.6 encrypted producer keystore | MERGED. R-59-01 resolved + CLI tests. |
| #57 `s3.3/native-sync` | native certified sync + fork accountability | MERGED. R-57-01 raised here, fixed in #67. |
| #58 `qa/s12.1-threat-model` | S12.1 threat-model refresh | MERGED. |
| #60 fee-market-api | S6.2 step 1 fee-market core/state API | MERGED. No pre-merge QA needed (tech-lead). |

Reports: `2026-11-19-pr57-native-sync-review.md` (incl. R-57-01 fix verification),
`2026-11-19-pr59-keystore-review.md`, `2026-11-19-pr65-storage-fee-replay-review.md`.
Audit prep: `AUDIT-PACKAGE.md` (S12.2 prep only; audit + soak human/time-gated).

Gate reference: four-gate baseline on `fa87215`
(`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`), deny workaround per Q-029.