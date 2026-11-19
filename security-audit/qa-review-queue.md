# QA review queue: seat nara (track-b)

Updated 2026-11-21 (tech-lead direction: #89 merged, R-RET-01 Fixed, #78/#87
records folded in). Evidence logs under `2026-11-19-four-gate-baseline/` are
kept intact (only `target/` build dirs were cleaned; sccache S3 cache makes
rebuilds cheap).

## Active / incoming wave

| Item | Branch / PR | Scope | State | QA focus |
| --- | --- | --- | --- | --- |
| L2 calldata floor (bima) | `s6/calldata-floor-api`, final head `94aa940` | `CalldataPolicy::check_floor`, genesis binding, v3 accept-only codec (Q-037, ADR-015) | R-L2-01/R-L2-02 closed; **R-L2-03 blocking** (test fixtures vs new encode-time version check); APPROVE suspended pending fix + green re-run | Track A. Accept-only until L4. nara gates at `94aa940`: fmt/clippy/deny OK, test=101 (Q-037-track-b). |
| PR #88 | `s8/otlp-cli-wiring` (`fd005b8`) | S8.1 OTLP CLI wiring (Q-035) | OPEN, formal review APPROVE, no blocking | Track B (tania). Opt-in default byte-identical, non-fatal fallback, guard in runtime. Info: https for remote collectors; no auth-header field yet. Independent `--all-features` gates queued behind the activation-critical runs. |
| PR #84 | `s6.1b/mempool-persist-node` | opt-in mempool persistence consumer (Q-016/Q-038) | OPEN, APPROVE + re-review; tech-lead mechanical rebase per ADR-016 (head `eb61fce`, patch identity independently verified: 0-line interdiff vs approved `1641df7`); gates running | Track A (raka, session unresponsive, escalated). Report: `2026-11-20-pr84-mempool-persist-review.md`. R-84-01 Low closes when yoga's stacked `--mempool-persist` flag PR lands. |
| yoga retention flag | `yoga/retention-blocks-flag` (`a05b2d2`) | `--retention-blocks` CLI + runbook | Pre-review APPROVE (config-only, inert until the node trigger) | Track B. Q5/Q7 honored; boot warning <4096 belongs to the trigger slice. |
| yoga mempool-persist flag | `yoga/mempool-persist-flag` (`057633d`) | `--mempool-persist` CLI | Stacked on #84; pre-review done; PR pending | Track B. Closes R-84-01. Rebase per ADR-016 consequence note after #84 merges. |
| tania Q4 prune metrics | `s8/ledger-prune-metrics` | `subhost_ledger_prune_total{outcome}`, `subhost_ledger_base_height` | Stacked; dewi reviewed, no objection; rebase onto main after #87 merge | Track B. Three bounded labels, no `failed` outcome; gauge seeded from atomic base. |
| raka node trigger | (pending) | prune-after-commit loop consuming `retain_from` | Blocked on session availability (escalated by tech-lead). **Prerequisite satisfied:** R-RET-01 fix merged (#89) before any trigger exists | Track A. Review against Q-036 contract: prune after commit, failure never halts, boot warning below 4096, demo single-writer caveat. |
| sari block/consensus spans | (incoming) | block/consensus telemetry spans | pending publish | Track A. Span cardinality / no secret labels. |

## Tracked blocker: SUBHOST-2026-003 (High), three-fix ladder

- **Fix 2 (suffix-only validation): MERGED** (#50). Append-time O(history) replay gone.
- **Fix 3 (automatic retention): MERGED** (#87, `93aeadc`) **+ R-RET-01 fixed**
  (#89, `3148a4e`). Mechanism verified: snapshot-floor refusals write nothing,
  whole-segment trim + coverage invariant, manifest-before-cleanup atomicity,
  commit-lock serialization, fail-safe head cache. **No production caller yet:**
  the node trigger slice is pending (raka), so nothing prunes automatically on
  main today. Reports: `2026-11-21-pr87-ledger-retention-review.md`,
  `2026-11-21-pr89-ret-height-self-check-review.md`.
- **Fix 1 (calldata pricing): L2 pre-reviewed, both findings closed** (Q-037,
  ADR-015). R-L2-01 (ADR/ROADMAP overclaim) fixed in `8a5f995`; R-L2-02
  (protocol length cap as consensus validity) fixed in `56690b7`:
  `check_floor` refuses `data.len() > MAX_TX_DATA_BYTES` before the gas
  comparison, boundary-tested. Final head `94aa940` awaits PR + gates + merge;
  L3 (dewi) and L4 (bima) follow. Tech-lead L4 ruling: `header_version()` = 3
  whenever a calldata policy is bound; no v2 production on a bound genesis.
- **Must close before public testnet activation (S12.3).** The High closes only when
  fix 1 lands and pricing + amplification are re-verified together. Evidence:
  `2026-11-19-rpc-work-amplification/`, Q-024, Q-037.

## Independent gate runs (nara)

| Baseline | fmt | clippy | test | deny | Note |
| --- | --- | --- | --- | --- | --- |
| L2 `b75c40d` | OK | OK | killed (143) | OK | Superseded: heads moved to `94aa940`. SIGTERM was collateral of an over-broad `pkill` (acknowledged in Q-037-track-a), not a code failure. |
| retention `11e009a` | OK | killed (143) | n/a | n/a | Superseded: merged retention files byte-identical to `93aeadc`; covered by the `671dc02` run. |
| retfix `671dc02` (= merged #87+#89) | OK | OK | OK (657/0) | OK | **ALL GREEN**, cache bypass, detached, four markers + done. Logs `/tmp/opencode/retfix-gates/`. |
| L2 final `94aa940` | OK | OK | **FAILED (101)** | OK | R-L2-03 blocking: two `proposal_budget_tests` fixtures encode `version: u32::MAX`, refused by L2's new encode-time version check. `--no-fail-fast` enumeration: exactly these 2 failures, 667 passed workspace-wide, nothing else latent. One-line fixture fix recommended (Q-037-track-b); re-run at fixed head. Logs `/tmp/opencode/l2-final-gates/`. |
| OTLP `fd005b8` (#88) | running | running | running | running | `--all-features`, cache bypass, detached. Logs `/tmp/opencode/otlp-gates/`. |

Gate practice (2026-11-20/21): a killed gate (exit 143 / abrupt log end) is
never green; re-run detached (`setsid nohup`), verify FMT_OK / CLIPPY_OK /
TEST_OK / DENY_OK plus the exit line before treating a run as green.
**Cache bypass active (tech-lead decision):** the sccache S3 backend is
degraded (reads ~20s, writes ~44s); gates run WITHOUT the wrapper
(`unset RUSTC_WRAPPER`, `CARGO_INCREMENTAL=0`, no s3-env.sh sourcing) until
the tech lead announces re-enable. Partial target/ artifacts are reused.
Env note: `/usr/local/bin/cargo` is a root wrapper forcing
`CARGO_HOME=/usr/local/cargo`, and the rustup proxy resets CARGO_HOME the
same way; nara runs call `/usr/local/cargo/bin/cargo` directly with
`CARGO_HOME=$HOME/.cargo` exported (registry cache write warnings under the
root-owned home are cosmetic, not fatal). Deny workaround per Q-029:
`RUSTUP_HOME=/usr/local/rustup CARGO_HOME=$HOME/.cargo
CARGO=/usr/local/cargo/bin/cargo ~/.local/bin/cargo-deny --all-features check`.

## Recently reviewed and merged

| PR | Scope | Outcome |
| --- | --- | --- |
| #89 `fix/ret-height-self-check` | R-RET-01 fix: height self-check in `block_at_height` | MERGED (`3148a4e`). APPROVE; R-RET-01 Fixed; verified on main. |
| #87 `s12/ledger-retention` | height-based online ledger retention (003 fix 3) | MERGED (`93aeadc`). APPROVE + R-RET-01 (Medium, fixed in #89). |
| #86 `qa/s96-complete-records` | S9.6 complete, records for #84/#85, posture sync | MERGED. |
| #78 `s8/node-runtime-hooks` | S8 runtime hooks (health/finality/disk gauges) | MERGED (`36c41d5`). Retroactive spot-check note, no blocking issues; merged without dedicated pre-review (gap closed by the note). |
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
`2026-11-20-pr85-credential-flag-review.md`,
`2026-11-21-pr78-runtime-hooks-note.md`,
`2026-11-21-pr87-ledger-retention-review.md`,
`2026-11-21-pr89-ret-height-self-check-review.md`.
Audit prep: `AUDIT-PACKAGE.md` (S12.2 prep only; audit + soak human/time-gated).

Gate reference: four-gate baseline on `fa87215`
(`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`), deny workaround per Q-029.
Builds: sccache S3 cache **bypassed** by tech-lead decision (backend degraded);
gates run with `unset RUSTC_WRAPPER`, `CARGO_INCREMENTAL=0`, no s3-env.sh
sourcing until re-enable is announced. When re-enabled: source
`~/.config/subhost/s3-env.sh` (sccache, secret: never commit/print).
