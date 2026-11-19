# Independent four-gate baseline: origin/main fa87215 (current tip)

Seat: **nara, Security & QA (track-b)**. Run on 2026-11-19 (start 21:52, end 22:09 WIB).

## Target

- Commit: `fa87215e035eb8f88a944108eb29724c9a79b59b` (`Merge pull request #50 from zulfff/security/ledger-append-suffix-validation`).
- This is the current `origin/main` tip (includes PRs #48/#49/#50/#51/#52).
- Clean detached worktree: `/home/zulfff/subhost-wt/qa-fa87215`.

## Results

| Gate | Command | Exit | Evidence |
| --- | --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | 0 | `fmt.log` (empty) |
| clippy | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | `clippy.log`, zero warnings |
| test | `cargo test --workspace --all-features` | 0 | `test.log`: 34 suites, 479 passed, 0 failed |
| deny | `cargo-deny --all-features check` | 0 | `deny.log`: `advisories ok, bans ok, licenses ok, sources ok` |

Deny reports warn-level duplicate-crate entries only (`multiple-versions = "warn"`
in `deny.toml`); no advisory, ban, license, or source failures.

## Method

- Runner: `../run-gates.sh <worktree> <outdir>` (logs and exit codes in `run.txt`).
- Toolchain: cargo 1.93.0, cargo-deny 0.21.0 (`~/.local/bin`).
- Deny invocation (the `/usr/local/bin/cargo` wrapper forces a read-only
  `CARGO_HOME`; call the binary directly, see Q-029):

  ```sh
  RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/zulfff/.cargo \
  CARGO=/usr/local/cargo/bin/cargo \
  ~/.local/bin/cargo-deny --all-features check
  ```

  `--all-features` is a top-level cargo-deny option; `check --all-features` exits 2.

## Reproduce

```sh
git worktree add --detach /home/zulfff/subhost-wt/qa-fa87215 fa87215
bash security-audit/2026-11-19-four-gate-baseline/run-gates.sh \
  /home/zulfff/subhost-wt/qa-fa87215 \
  security-audit/2026-11-19-four-gate-baseline/fa87215
```

## Related

- Supersedes the `982d3e6` baseline (`../982d3e6/`), which remains valid evidence
  for that commit only. The stale local-`main` run is `../a210f29/`.
- Threat-model refresh against this tip: branch `qa/s12.1-threat-model`, commit
  `9cab6b7` (`docs/security/threat-model.md`, docs-only), includes the #50 storage
  suffix-validation and #49 wallet KDF-bound changes.