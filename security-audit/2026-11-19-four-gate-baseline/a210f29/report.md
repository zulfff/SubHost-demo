# Independent four-gate baseline: main `a210f29`

- Target: `SubHost-demo`, local `main`
- Commit: `a210f290da242228c70c01ecf1695aaf000b3606` (Merge PR #29, `s1/verify-on-accept`)
- Date: 2026-11-19 (UTC)
- Track: B, Platform (QA support, unseated session `ses_e0be59875ffeS326qAVSdzBml4`)
- Scope: the four pre-push gates from `AGENTS.md` §7, run independently of any
  agent's working tree. No source was modified. No E2E demo run in this pass.

## Revision identity (verified, not assumed)

`git rev-parse main` (local) = `a210f29`. Refs verified at run time:

- local `main` = `a210f29`, **82 commits behind** `origin/main`
- `origin/main` = `fa87215` (Merge PR #50, `security/ledger-append-suffix-validation`)
- `982d3e6` (PR #47, S8) is an ancestor of `origin/main`, **19 behind** it
- `a210f29` is 31 behind `96f0cd2` (base of the 2026-11-18 QA baseline) and 63
  behind `982d3e6`

The maintainer named local `main` (`a210f29`) as this baseline's target; this
report records exactly that revision and claims no coverage of anything merged
after it. In particular the S8 `/health/live` / `/health/ready` wiring is
**not** exercised here. The sibling run in `../982d3e6/` (seat nara) labels
`982d3e6` as `origin/main`; as of this run `origin/main` has moved to `fa87215`,
so neither run covers current upstream main. See `../README.md`.

## Method

- Clean detached worktree, originally `/tmp/opencode/qa-a210f29`. That checkout
  was deleted mid-session by the recurring `/tmp/opencode` cleanup (third
  incident today); recreated at `/home/zulfff/subhost-wt/qa-a210f29` from the
  same ref `a210f29`, `git status --porcelain` empty. All evidence logs were
  written into this workspace directory, so none were lost. Deny re-verified
  green from the new location (`evidence/deny-reverify-after-move.log`).
  Other agents' uncommitted edits in the shared tree cannot influence these
  results.
- Cold, private target directory (worktree-local `target/`), so no cached
  artifacts from another branch leak in.
- Toolchain pinned by `rust-toolchain.toml`: cargo/rustc 1.93.0.
- `Cargo.lock` sha256 `fc47527745a8664ae3dedbe5e38afbb3dfdaf2fc535559d59b6685813d37e579`;
  all cargo gates run with `--locked` where the gate definition uses it.
- Raw logs: `evidence/{format,clippy,tests,deny,deny-retry}.log`, machine-readable
  results `evidence/gates.json`, environment `evidence/env.txt`.

## Results

| Gate | Command | Exit | Seconds |
| --- | --- | --- | --- |
| format | `cargo fmt --all -- --check` | 0 | 1 |
| clippy | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 0 | 66 |
| tests | `cargo test --workspace --all-features --locked` | 0 | 290 |
| deny (as written in AGENTS.md) | `cargo deny --all-features check` | **1** | 2 |
| deny (db-path override) | `cargo deny --config <override> --all-features check` | 0 | 3 |
| deny (direct binary, writable CARGO_HOME) | `cargo-deny --all-features check` | 0 | 4 |

Tests: 282 passed, 0 failed, 0 ignored across 33 result lines (19 test binaries
plus doc-tests). Clippy: zero warnings under `-D warnings`.

Supply-chain verdict with the override: `advisories ok, bans ok, licenses ok,
sources ok`. 22 `warning[duplicate]` entries (e.g. `aead` 0.5/0.6,
`x25519-dalek` 2.0.1/3.0.0 via `libp2p-noise`). These are `warn`-level under
`[bans] multiple-versions = "warn"` and do not fail the gate; they are recorded,
not silently dropped.

## Defect found: the deny gate is not reproducible in this sandbox

First run of the gate exactly as `AGENTS.md` §7 writes it failed before doing
any check:

```
failed to acquire advisory database lock: file lock error: failed to obtain lock
file '/usr/local/cargo/advisory-dbs': failed to create parent directories for
lock path: Permission denied (os error 13)
```

`/usr/local/cargo` is owned by `ubuntu` and not writable by this user.
**Root cause (verified):** `/usr/local/bin/cargo` is a shell wrapper that
hard-exports `CARGO_HOME=/usr/local/cargo` before exec'ing the real cargo, so
any `CARGO_HOME` set by the caller is overwritten. cargo-deny 0.21.0 resolves
its advisory DB under that cargo home and cannot create it. Exporting
`CARGO_HOME` therefore has no effect when the gate is invoked as `cargo deny`.
This is an environment reproducibility defect, not a code or dependency
defect: CI's `Supply chain` job runs the same command on a runner with a
writable cargo home, so CI is green while a local/sandboxed QA run of the
documented gate fails.

Two verified workarounds, both leaving repo config and CI untouched:

1. **Direct binary** (preferred; found by seat nara, re-verified here at
   `a210f29`, `evidence/deny-direct.log`):

   ```sh
   RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/zulfff/.cargo \
   CARGO=/usr/local/cargo/bin/cargo \
   ~/.local/bin/cargo-deny --all-features check
   ```

2. **db-path override** (`evidence/deny-retry.log`,
   `evidence/deny-qa-config.toml`): the repo `deny.toml` plus one line,
   `db-path = "/tmp/opencode/advisory-dbs"` under `[advisories]`; diff against
   the repo file shows no other change. Historical artifact: that `/tmp` path
   was later deleted by the same cleanup, so prefer workaround 1. The saved
   config is kept unmodified as the record of what produced `deny-retry.log`.

With either, all four checks pass. `run-gates.sh` in this directory reproduces
the baseline from `/home/zulfff/subhost-wt/qa-a210f29` using workaround 1.

Tracked as `docs/agents/questions/open/Q-029-cargo-deny-advisory-db-path.md`
(`blocks: none`).

## Limitations

- Static gates only. The two-user demo (`scripts/two-user-setup.sh`,
  `user1-alice.sh`, `user2-bob.sh`) and the S8 health endpoints were not run in
  this pass; the earlier E2E task was scoped to `982d3e6`, which the maintainer
  superseded with this baseline. A follow-up E2E pass should state its own
  revision explicitly.
- One shared machine: timings are wall-clock under concurrent agent builds and
  are not comparable to the 2026-11-18 numbers.
- The deny retry used a freshly fetched advisory DB; a later run may see new
  advisories. The pass is a statement about 2026-11-19, not a permanent one.