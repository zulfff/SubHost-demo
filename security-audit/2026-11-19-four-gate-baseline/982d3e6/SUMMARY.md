# Independent four-gate baseline: origin/main 982d3e6

Seat: **nara, Security & QA (track-b)**. Run on 2026-11-19 (start 21:11, end 21:29 WIB).

## Target

- Commit: `982d3e678e79ae7e06d5662e62722b145ea6d684` (`Merge pull request #47 from zulfff/s8/platform-observability`).
- This is `origin/main`. The local branch `main` (`a210f29`, merge of PR #29) is
  63 commits behind and is **not** the baseline target.
- Clean detached worktree: `/home/zulfff/subhost-wt/qa-baseline`.

## Results

| Gate | Command | Exit | Evidence |
| --- | --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | 0 | `fmt.log` (empty) |
| clippy | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | `clippy.log`, zero warnings |
| test | `cargo test --workspace --all-features` | 0 | `test.log`: 34 suites, 460 passed, 0 failed |
| deny | `cargo-deny --all-features check` | 0 | `deny.log`: `advisories ok, bans ok, licenses ok, sources ok` |

Deny reports duplicate-crate warnings only (`multiple-versions = "warn"` in
`deny.toml`); no advisory, ban, license, or source failures.

## Method

- Runner: `../run-gates.sh` (logs and exit codes recorded in `run.txt`).
- Toolchain: cargo 1.93.0, cargo-deny 0.21.0 (`~/.local/bin`).
- Deny invocation (the `/usr/local/bin/cargo` wrapper forces a read-only
  `CARGO_HOME`, so call the binary directly):

  ```sh
  RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/zulfff/.cargo \
  CARGO=/usr/local/cargo/bin/cargo \
  ~/.local/bin/cargo-deny --all-features check
  ```

  `--all-features` is a top-level cargo-deny option; `check --all-features` is
  invalid and exits 2.

## Reproduce

```sh
git worktree add --detach /home/zulfff/subhost-wt/qa-baseline 982d3e6
bash security-audit/2026-11-19-four-gate-baseline/run-gates.sh
```

## Related

- Superseded: `origin/main` advanced to `fa87215` (PRs #48/#49/#50/#51/#52). The
  current-tip baseline is `../fa87215/`; this `982d3e6` run remains valid evidence
  for that commit only.
- Threat-model refresh: branch `qa/s12.1-threat-model`, rebased onto `fa87215`,
  commit `9cab6b7` (`docs/security/threat-model.md`, docs-only).
- A separate unseated QA session ran the gates on the stale `a210f29`; that run is
  not this baseline and is labelled `a210f29-stale` under `../a210f29/`.