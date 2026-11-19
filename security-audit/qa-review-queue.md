# QA review queue: seat nara (track-b)

Updated 2026-11-20 (tech-lead direction). Evidence logs under
`2026-11-19-four-gate-baseline/` are kept intact (only `target/` build dirs were
cleaned; sccache S3 cache makes rebuilds cheap).

## Active / incoming wave

| Item | Branch / PR | Scope | State | QA focus |
| --- | --- | --- | --- | --- |
| PR #84 | `s6.1b/mempool-persist-node` | opt-in mempool persistence consumer (Q-016/Q-038) | OPEN, reviewed approve | Track A (raka). Report: `2026-11-20-pr84-mempool-persist-review.md`. Opt-in default-off, genesis-bound, fail-closed boot (foreign/corrupt snapshot refuses). R-84-01 Low: `--mempool-persist` CLI flag promised in doc comment but not wired. Merge once cargo gates green. |
| PR #78 | `s8/node-runtime-hooks` | S8 runtime hooks (health/finality/disk gauges) | OPEN | Track A/B. Truthful gauges, no secret labels. |
| dewi Q-036 retention slice | (incoming) | `prune_to_height` + `retain_from` | pending publish (tech-lead pre-reviewed correct) | Track B (dewi). **Verify atomic trim + snapshot-floor refusals** (fix 3 of 003). |
| sari block/consensus spans | (incoming) | block/consensus telemetry spans | pending publish | Track A (sari). Span cardinality / no secret labels. |

## Tracked blocker: SUBHOST-2026-003 (High), three-fix ladder

- **Fix 2 (suffix-only validation): MERGED** (#50). Append-time O(history) replay gone.
- **Fix 3 (automatic retention): in progress** (Q-036, dewi). `retention_blocks`
  config landed (#76); atomic trim + snapshot-floor slice incoming under QA.
- **Fix 1 (calldata pricing): ladder agreed** (Q-037 L2 floor model, nara confirmed).
  Intrinsic gas as validity floor on `gas_limit`; consumed stays `gas_limit`;
  conservation unchanged; refunds deferred to S10. Awaiting L2–L4 (bima).
- **Must close before public testnet activation (S12.3).** The High closes only when
  fix 1 lands and pricing + amplification are re-verified together. Evidence:
  `2026-11-19-rpc-work-amplification/`, Q-024, Q-037.

## Recently reviewed and merged

| PR | Scope | Outcome |
| --- | --- | --- |
| #85 `yoga/node-producer-credential` | S9.6 completion: `--producer-credential-file` unlock | MERGED. No blocking; S9.6 complete (#59 + #79 + #85). |
| #82 `qa/audit-residuals-refresh` | audit residuals (003 ladder) + queue refresh | MERGED. |
| #80 `s6.1b/pending-store-wiring` | S6.1b pending store commit-path wiring (Q-034) | MERGED. |
| #79 `s9/producer-bootstrap-api` | S9.6 Track A bootstrap + hardened loader | MERGED. No blocking; activation-gate item (S9.6). |
| #76 / #75 / #74 | retention config / pending-store hooks / OTLP exporter | MERGED. |
| #73 `qa/s12-r57-residual-note` | R-57-01 residual note fixed | MERGED. |
| #72 `qa/s12-audit-package` | version security-audit package | MERGED. |
| #71 `s6/fee-market-v2` | S6.2 step 3 fee-enabled block path | MERGED. |
| #70 `s8/rpc-finality-disk-contract` | S8 contract accessors | MERGED. |
| #69 `yoga/rpc-method-spans` | S8.1 RPC method spans | MERGED. |
| #67 `s3.4/pruned-history-and-backup` | R-57-01 fix + archive restore coherence | MERGED. R-57-01 closed. |
| #65 `s6/storage-fee-replay` | S6.2 step 2 storage fee replay | MERGED. No blocking issues. |
| #64 `qa/s12.4-bug-bounty` | S12.4 bounty/disclosure docs | MERGED. |
| #63 `qa/s12-sync-fork-refresh` | S12.1 threat-model + SECURITY.md reconcile | MERGED. |
| #59 `yoga/encrypted-producer-keystore` | S9.6 encrypted producer keystore | MERGED. R-59-01 resolved. |
| #57 `s3.3/native-sync` | native certified sync + fork accountability | MERGED. R-57-01 raised, fixed in #67. |
| #58 / #60 | S12.1 threat-model / fee-market step 1 | MERGED. |

Reports: `2026-11-19-pr57-native-sync-review.md` (incl. R-57-01 fix verification +
decision-table audit), `2026-11-19-pr59-keystore-review.md`,
`2026-11-19-pr65-storage-fee-replay-review.md`,
`2026-11-20-pr79-producer-bootstrap-review.md`,
`2026-11-20-pr84-mempool-persist-review.md`,
`2026-11-20-pr85-credential-flag-review.md`.
Audit prep: `AUDIT-PACKAGE.md` (S12.2 prep only; audit + soak human/time-gated).

Gate reference: four-gate baseline on `fa87215`
(`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`), deny workaround per Q-029.
Builds: source `~/.config/subhost/s3-env.sh` (sccache, secret: never commit/print).