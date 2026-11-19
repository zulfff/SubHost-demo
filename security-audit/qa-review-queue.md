# QA review queue: seat nara (track-b)

Updated 2026-11-21 (WAVE COMPLETE: L2 #94, L0 #95, OTLP #88, #97, spans #98
(`c5ce60c`), architecture #99 (`dd5b514`), start-stop #102 (`27b2c92`), L3
#101 (`4dcd6d3`), sync-relay #100 (`936f3d1`) all MERGED; main = `936f3d1`;
R-L2-01/02/03, R-88-01, R-84-01, R-96-01 CLOSED; sccache S3 cache RE-ENABLED,
three clean cache-path samples. Q-037 ladder L0-L3 on main; L4 (bima)
building, L5 (yoga) staged. Activation gate: L4 merged+verified + nara
SUBHOST-2026-003 re-probe). Evidence logs under
`2026-11-19-four-gate-baseline/` are kept intact (only `target/` build dirs
were cleaned; the re-enabled sccache S3 cache makes rebuilds cheap).

## Active / incoming wave

| Item | Branch / PR | Scope | State | QA focus |
| --- | --- | --- | --- | --- |
| L2 calldata floor (bima) | PR #94 | `CalldataPolicy::check_floor`, genesis binding, v3 accept-only codec (Q-037, ADR-015) | **MERGED** (`6acc9bb`), ADR-015 landed on main. APPROVE; R-L2-01/02/03 all CLOSED. Merge verified content-identical to reviewed `6cd6ea1` outside CHANGELOG. nara gates at `73af84e` ALL GREEN 669/0; bima final-head markers 676/0 | Track A. Accept-only until L4. Report: `2026-11-21-pr94-calldata-floor-review.md`. L3 merged (#101); L4 (bima) building, test plan ack'd in Q-037-track-b. |
| L0 calldata cap + fee density (dewi) | PR #95 | mempool 16 KiB cap, fee-density ranking/RBF/eviction, 256 KiB selection budget (Q-037 L0) | **MERGED** (`0627d39`), merge identical to reviewed `3b36c84`. Closure chain: `1d37695` (nara gates ALL GREEN 675/0) -> `21b63ce` (pure union, one cosmetic blank flagged) -> `3b36c84` (blank collapsed, whitespace-only) -> merge; dewi final markers 682/0. Closure comments issuecomment-7023224435 / issuecomment-7023314633 | Track B. Local/interim only: proposal ordering stays gas-price-first, no consensus change; activation gate still L2-L4. Info: builder knob below the mempool cap makes txs unselectable (doc warning, no enforcement). |
| PR #88 | `s8/otlp-cli-wiring` | S8.1 OTLP CLI wiring (Q-035) | **MERGED** (`1e45462`). APPROVE reinstated; R-88-01 CLOSED. Full identity chain verified: `eb65437` (nara gates ALL GREEN 661/0) -> `1e4093a` -> `d5386c8` -> `be50eab` -> merge, all CHANGELOG-only steps; tania final markers 726/0 incl. exact CI command + genesis dry-run | Track B (tania). Opt-in default byte-identical, non-fatal fallback, guard in runtime. Info: https for remote collectors; no auth-header field yet. R-88-01 was the known Q-035 bare-chain test bug at `fd005b8` (test=101, enumeration 640/1), fixed as a strengthening at `eb65437`. |
| PR #84 | `s6.1b/mempool-persist-node` | opt-in mempool persistence consumer (Q-016/Q-038) | **MERGED** (`f59fe71`, ADR-016 mechanical rebase, 0-line interdiff verified) | Track A (raka). Report: `2026-11-20-pr84-mempool-persist-review.md`. R-84-01 closed by merged #96. |
| yoga retention flag | PR #91 | `--retention-blocks` CLI + runbook | **MERGED** (`b49d05c`). APPROVE (config-only, inert until the node trigger) | Track B. Q5/Q7 honored; boot warning <4096 belongs to the trigger slice. |
| yoga mempool-persist flag | PR #96 | `--mempool-persist` CLI | **MERGED** (`b5db69b`). APPROVE; R-84-01 CLOSED. R-96-01 (Low, duplicated doc paragraph) **CLOSED**: #97 merged (`b1f93c5`), dedupe verified on main (docs-only +3/-5, both facts kept) | Track B. Opt-in wired at both run_node call sites; default off, volatile pool. |
| tania Q4 prune metrics | PR #92 | `subhost_ledger_prune_total{outcome}`, `subhost_ledger_base_height` | **MERGED** (`db2231d`) on CI green before the review landed (deviation noted by tech-lead; content verified). Post-merge APPROVE at `db2231d`: content-identical to reviewed `a670b00` outside CHANGELOG; nara gates at `a670b00` ALL GREEN 670/0 (first run on the re-enabled sccache path; cache-path validated, no sccache errors) | Track B. Three bounded labels, no `failed` outcome, no height labels; gauge seeded from the restored base at attach, moves only on `Pruned`; errors never counted; refusals are normal no-ops (no refusal alert). |
| raka node trigger | `s12.3/retention-node-trigger` (PR pending) | prune-after-commit loop consuming `retain_from` | Trigger implemented on branch `s12.3/retention-node-trigger` (Q-041 agreed): prunes after a successful commit, refuses `0` at bootstrap, warns below the 4096-item certified archive window, excludes `--single-writer` demo mode. PR pending; recovery backlog (4 items) in `docs/agents/replies/raka-recovery-backlog.md`. **Prerequisite satisfied:** R-RET-01 fix merged (#89) before any trigger exists | Track A. Review against Q-036 contract: prune after commit, failure never halts, boot warning below 4096, demo single-writer caveat. |
| sari block/consensus spans | PR #98 | block/consensus telemetry spans (S8.1, Q-035 contract over tania's #74 provider) | **MERGED** (`c5ce60c`, announced by tech-lead). Tech-lead contract APPROVE + yoga APPROVE + tania APPROVE (provider owner); nara security spot-check PASS at `f7d0c19`, carried to `5e6f294` (R-02 punctuation-only docs delta, zero code diff) and to the merge. Q-035 contract complete end-to-end: provider #74 + CLI #88 + spans #98 | Track A (sari). One span per driver step (never per vote); bounded `error_kind` static classification, no raw peer text; span cardinality / no secret labels; near-free no-ops without a subscriber. |
| tania sync-relay OTLP | PR #100 | sync-relay example opt-in OTLP export (S8.1/Q-035 follow-up, mirrors #88) | **MERGED** (`936f3d1`, last of the wave). nara APPROVE at `e82f545`, carried through `962c225` (docs-only rebase, keep-both observability conflict with #98; range-diff patch delta observability.md-only, code + CHANGELOG content-identical; tech-lead precedent: gates + CI carry) to the merge (range-diff metadata-only). Contract: opt-in default off (unset/empty/whitespace = plain logging, no exporter), non-fatal fallback, guard retained in async main, malformed endpoint refused before global install, no credentials touched (Q-040 binds the auth-header slice, now GO), no new deps (otlp feature on subhost-cli since #88). Patch identity: `d8f3cf7` -> `e82f545` CHANGELOG-only 2-line interdiff (#96 precedent). nara gates at `e82f545` ALL GREEN 702/0 + example 16/16, zero sccache warnings (clean cache sample); tania bonus full re-gate at `962c225` 774/0 (63 focused + 711 workspace, zero warnings, tech-lead verified); CI 9/9. S8.1 export path complete end-to-end incl. the relay example | Track B (tania). Info: no-endpoint path relaxes `init(...)?` to `init_or_warn` (sole installer, no observable default change, mirrors #88). Evidence: issuecomment-7024298386 / issuecomment-7024446064 / issuecomment-7028567027; nara review on PR. |
| dewi L3 replay dispatch | PR #101 | manifest calldata_policy sibling binding + replay floor dispatch (Q-037 L3) | **MERGED** (`4dcd6d3`). Tech-lead contract review (rulings 1+2: calldata-only base-fee-0 chains, ALL-v3 priced ledgers; fail-closed missing schedule, mixed-version refusal, schedule through all paths incl. prune, RPC refuses repricing pre-v3 restored ledgers) + nara post-merge security verification PASS (full diff read at `4dcd6d3`, range-diff vs gated `5e735d9` metadata-only; all six bind guards, additive manifest version-2 byte-identity pin, check_floor per tx BEFORE execution on every replay path, calldata-only base-fee-0 refusal, RPC gas-limit frame + identical store schedule; 21 new tests). nara INDEPENDENT gates at `5e735d9` ALL GREEN 719/0, four markers + exit 0, zero sccache warnings (clean cache sample), exact match with dewi 719/0; CI 9/9; merge evidence per tech-lead | Track B (dewi). Replay-only: nothing produces v3 or enforces the floor on admission until L4. Info: `#[allow(clippy::too_many_arguments)]` on persist, documented structural rationale. Residual: restore_pending gas-mismatch regression + accept/persist round-trip + floor-invalid-untouched-tip tests in bima's L4 plan. Review: PR #101 issuecomment-7028820366. |
| bima L4 activation wiring | (building on post-L3 main) | six insertion points + cap_dropped split + floor_dropped counter, restore filter order pin, select no-resurrection, accept/persist floor-before-state-transition (Q-024 pin), gas-mismatch regression | GO issued at `4dcd6d3`; sari peer-reviews; nara regressions + joint 003 re-probe on merge | Track A. Counter set mined/floor_dropped/oversized/cap_dropped/refused/restored (dewi ack'd). Two added tests: accept/persist round-trip; floor-invalid refusal with untouched tip. |
| yoga L5 CLI/docs | (staged) | genesis calldata-schedule flag + docs | Builds after L4 merge; L3 API pre-verification assigned | Track B. |
| tania auth-header slice | (GO per Q-040) | OTLP auth headers: presence-only override warning + documented precedence, fail-closed bad token file, zeroize with SDK-copy caveat, never logged/never in span attributes | Starts on #100 merge (done); branch from `936f3d1` | Track B. Agreed Q-040 secrets contract binds. |
| yoga architecture refresh | PR #99 | Q-037 L0 admission rules + stale persistence line (docs-only, +24/-9) | **MERGED** (`dd5b514`). Tech-lead GO + docs-only verified; nara spot-checked the two load-bearing source claims (16 KiB pool cap at mempool lib.rs:44, 128 KiB protocol bound at core calldata.rs:39): both accurate on main | Track B (yoga). No code or semantic change. |
| yoga start-stop flag list | PR #102 | list `--otlp-endpoint` in start-stop opt-in flags (docs-only) | **MERGED** (`27b2c92`), tech-lead docs-only verified | Track B (yoga). |

## Tracked blocker: SUBHOST-2026-003 (High), three-fix ladder

- **Fix 2 (suffix-only validation): MERGED** (#50). Append-time O(history) replay gone.
- **Fix 3 (automatic retention): MERGED** (#87, `93aeadc`) **+ R-RET-01 fixed**
  (#89, `3148a4e`). Mechanism verified: snapshot-floor refusals write nothing,
  whole-segment trim + coverage invariant, manifest-before-cleanup atomicity,
  commit-lock serialization, fail-safe head cache. **No production caller yet:**
  the node trigger is implemented on branch `s12.3/retention-node-trigger`
  (raka, Q-041 agreed), PR pending, so nothing prunes automatically on main
  today. Reports: `2026-11-21-pr87-ledger-retention-review.md`,
  `2026-11-21-pr89-ret-height-self-check-review.md`.
- **Fix 1 (calldata pricing): L0/L1/L2 MERGED, all findings closed**
  (Q-037, ADR-015 landed on main). L1 = Q-034 commit-path wiring (#80, merged
  earlier); L0 = #95 (`0627d39`, local cap/ranking/budget, no consensus
  change); L2 = #94 (`6acc9bb`). R-L2-01 (ADR/ROADMAP overclaim) fixed in
  `8a5f995`; R-L2-02 (protocol length cap as consensus validity) fixed in
  `56690b7`: `check_floor` refuses `data.len() > MAX_TX_DATA_BYTES` before the
  gas comparison, boundary-tested; R-L2-03 (blocking fixture regression) fixed
  in `73af84e` (sentinel `u32::MAX` -> `CALLDATA_HEADER_VERSION`, fixint u32,
  framing unchanged) and CLOSED on nara gates 669/0 + bima final-head markers
  676/0 at `6cd6ea1` (merge content-identical). **L3 (dewi, manifest binding +
  replay validity dispatch) MERGED (#101, `4dcd6d3`); L4 (bima, activation
  wiring) building**,
  design contract closed: six insertion points, restore filter order pin
  (mined-nonce -> floor_dropped -> oversized -> admit_restored; cap_dropped
  match-arm split in L4 restore wiring, dewi ack'd: named counter set
  complete, observation-only pin agreed), select no-resurrection,
  per-reason counter contract accepted by nara. Tech-lead L4 ruling:
  `header_version()` = 3 whenever a calldata policy is bound; no v2 production
  on a bound genesis.
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
| OTLP `fd005b8` (#88) | OK | OK | **FAILED (101)** | OK | R-88-01: the PR's own new flag test parsed bare `chain` (not a top-level subcommand; known Q-035 root cause). `--no-fail-fast` enumeration: exactly that 1 failure, 640 passed workspace-wide. APPROVE suspended, re-targeted per tech-lead. Logs `/tmp/opencode/otlp-gates/`. |
| L2 fix `73af84e` (#94) | OK | OK | OK (669/0) | OK | **ALL GREEN**, cache bypass, detached, four markers + done. Both R-L2-03 fixtures pass. Covers final head `6cd6ea1` (content-identical outside CHANGELOG); bima's final-head markers 676/0. R-L2-03 CLOSED. Logs `/tmp/opencode/l2-fix-gates/`. |
| OTLP fix `eb65437` (#88) | OK | OK | OK (661/0) | OK | **ALL GREEN**, cache bypass, detached, four markers + done. Fixed test passes. Covers `1e4093a` and `d5386c8` (CHANGELOG-only interdiffs verified); tania final markers 726/0. R-88-01 CLOSED. Logs `/tmp/opencode/otlp-fix-gates/`. |
| L0 `1d37695` (#95) | OK | OK | OK (675/0) | OK | **ALL GREEN**, cache bypass, detached, four markers + done; matches dewi's reported 675/0. Closure chain verified: `21b63ce` pure union (one cosmetic blank line flagged) -> `3b36c84` whitespace-only (blank collapsed, CHANGELOG rebase onto `6acc9bb`); dewi final markers 682/0. CLOSED. Logs `/tmp/opencode/l0-gates/`. |
| prune metrics `a670b00` (#92, = merged `db2231d`) | OK | OK | OK (670/0) | OK | **ALL GREEN**, four markers + `gates exit: 0`; all 6 new tests pass. **First run on the RE-ENABLED sccache path** (sources s3-env.sh, `CARGO_PROFILE_DEV_DEBUG=0` for disk headroom): cache-path validated, no sccache/connection errors. Logs `/tmp/opencode/pr92-gates/`. |
| sync-relay OTLP `e82f545` (#100, code = gated `d8f3cf7`) | OK | OK | OK (702/0) | OK | **ALL GREEN**, four markers + BUILD_EXAMPLE_OK + `gates exit: 0`, re-enabled sccache path, zero sccache warnings (clean cache-path sample). Example binary 16/16, four new tests by name. Author 765 = 63 focused-leg + 702 workspace-leg; workspace legs match exactly. CHANGELOG-only interdiff verified. Logs `/tmp/opencode/pr100-gates/`. |
| L3 replay dispatch `5e735d9` (#101, = merged `4dcd6d3`) | OK | OK | OK (719/0) | OK | **ALL GREEN**, four markers + `gates exit: 0`, re-enabled sccache path, zero sccache warnings (clean cache-path sample). 21 new tests pass; exact match with dewi 719/0; merge identity range-diff metadata-only. Merge evidence per tech-lead. Logs `/tmp/opencode/pr101-gates/`. |

Gate practice (2026-11-20/21, tech-lead mandate): run gates via a SCRIPT FILE
(`bash /path/gates.sh`), never inline `bash -c` chains; detached
(`setsid nohup`); kill ONLY your own PID/PGID verified via ps, never by
name/pattern. A killed gate (exit 143 / abrupt log end) is never green;
verify FMT_OK / CLIPPY_OK / TEST_OK / DENY_OK plus the exit line before
treating a run as green. An sccache/connection error is never green either.
**Cache RE-ENABLED (tech-lead decision 2026-11-20):** the sccache S3 backend
recovered (clean probe on the real cargo path: avg write 0.112s, read hit
0.097s, 0 timeouts/errors). NEW gate runs source `~/.config/subhost/s3-env.sh`
(sets `RUSTC_WRAPPER=sccache` + `CARGO_INCREMENTAL=0`; the file is mode 0600
outside the repo: never print, commit, or paste it). In-flight wrapper-free
runs finished as-is per the decision (the bypass rows above are historical
snapshots of that window). sccache 0.19 does not cache `--crate-type bin`
(normal; workspace libs are cached). An sccache/connection error is never
green: re-run. CLEAN cache-path samples (zero sccache warnings): #92 gate
670/0 (nara), #100 supplementary re-gate 774/0 (tania, `962c225`), #101
independent 719/0 (nara). The #88 supplementary gate 734/0 (813 hits / 0
errors) is a VALID result but NOT a clean sample: 12 server-restart fallback
warnings (nara grep-verified; tania correction issuecomment-7023450340).
sccache fell back gracefully, no compile failures; cache stays re-enabled
(clean probe + clean runs carry the decision).
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
| #88 `s8/otlp-cli-wiring` | S8.1 OTLP CLI wiring (Q-035) | MERGED (`1e45462`). APPROVE; R-88-01 closed; full identity chain verified; supplementary gates 734/0 (12 sccache restart-fallback warnings noted, results valid). |
| #95 `s6/calldata-l0` | Q-037 L0 calldata cap + fee-density ranking + selection budget | MERGED (`0627d39`). APPROVE; closure chain 1d37695 -> 21b63ce -> 3b36c84 verified; nara 675/0 + dewi 682/0. |
| #97 `yoga/r96-doc-nit` | R-96-01 runbook dedupe | MERGED (`b1f93c5`). APPROVE; R-96-01 CLOSED; docs-only +3/-5, both facts kept. |
| #94 `s6/calldata-floor-api` | calldata pricing floor + v3 accept-only codec (Q-037 L2, ADR-015) | MERGED (`6acc9bb`). APPROVE; R-L2-01/02/03 closed; merge identity verified. L3 merged (#101), L4 building. |
| #92 `s8/ledger-prune-metrics` | Q-036 Q4 prune outcomes + base-height gauge | MERGED (`db2231d`) on CI green; post-merge APPROVE, content-identical to reviewed `a670b00`; nara gates 670/0 (cache-path validated). No blocking. |
| #96 `yoga/mempool-persist-flag` | `--mempool-persist` CLI opt-in (Q-038) | MERGED (`b5db69b`). APPROVE; R-84-01 closed; R-96-01 Low doc nit fixed in merged #97 (`b1f93c5`), CLOSED. |
| #91 `yoga/retention-blocks-flag` | `--retention-blocks` CLI + runbook (Q-036 Q5) | MERGED (`b49d05c`). APPROVE; inert until the node trigger. |
| #84 `s6.1b/mempool-persist-node` | opt-in mempool persistence consumer (Q-016/Q-038) | MERGED (`f59fe71`). APPROVE; ADR-016 rebase identity verified. |
| #98 `s8/block-consensus-spans` | S8.1 block/consensus spans (Q-035) | MERGED (`c5ce60c`). nara spot-check PASS carried through the R-02 docs-only amend; Q-035 complete end-to-end. |
| #99 (docs-only) | architecture.md Q-037 L0 admission rules + stale persistence line | MERGED (`dd5b514`). Load-bearing source claims spot-checked accurate on main. |
| #102 (docs-only) | start-stop opt-in flag list adds `--otlp-endpoint` | MERGED (`27b2c92`). |
| #101 `s6/calldata-l3` | Q-037 L3 manifest binding + replay floor dispatch | MERGED (`4dcd6d3`). Tech-lead contract review + nara post-merge security PASS; nara independent 719/0 + dewi 719/0, CI 9/9. |
| #100 `s8/sync-relay-otlp` | S8.1 sync-relay OTLP export (Q-035) | MERGED (`936f3d1`). nara APPROVE + 702/0 carried; tania bonus re-gate 774/0; S8.1 export path complete incl. the relay example. |
| #93 `qa/records-gates-results` | gate results + R-L2-03 evidence + bypass wording | MERGED (`84a1639`). Records. |
| #90 `qa/records-87-89-78` | #87/#89/#78 records, R-RET-01 Fixed | MERGED (`e2d3967`). Records. |
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
`2026-11-21-pr89-ret-height-self-check-review.md`,
`2026-11-21-pr94-calldata-floor-review.md`.
Wave reviews recorded as PR comments + seat threads (no separate file):
#95 L0 (APPROVE), #96 flag (APPROVE + R-96-01), #97 dedupe (APPROVE),
#92 prune metrics (APPROVE, post-merge), #88 OTLP (APPROVE reinstated,
R-88-01 closed); see `docs/agents/replies/Q-035-track-b.md`,
`Q-037-track-b.md`, `Q-038-track-b.md`.
Audit prep: `AUDIT-PACKAGE.md` (S12.2 prep only; audit + soak human/time-gated).

Gate reference: four-gate baseline on `fa87215`
(`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`), deny workaround per Q-029.
Builds: sccache S3 cache **RE-ENABLED** by tech-lead decision 2026-11-20
(backend recovered, clean probe). New runs source `~/.config/subhost/s3-env.sh`
(sccache, secret: never commit/print); the earlier bypass window is a
historical snapshot in the gate-runs table above.
