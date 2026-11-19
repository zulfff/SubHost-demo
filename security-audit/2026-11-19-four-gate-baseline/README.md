# Four-gate baselines, 2026-11-19

Three independent runs. `fa87215/` is the current `origin/main` tip; the other
two are historical revisions.

| Subdir | Revision | Relation | Runner |
| --- | --- | --- | --- |
| `a210f29/` | `a210f29` = local `main` | 82 behind `origin/main` | unseated QA support, `ses_e0be59875ffeS326qAVSdzBml4` |
| `982d3e6/` | `982d3e6` (PR #47, S8) | 19 behind `origin/main` | seat nara, `ses_e0be5a8c5ffeM8Y46WD6dr0fDh` |
| `fa87215/` | `fa87215` (PR #50) | current `origin/main` tip | seat nara, `ses_e0be5a8c5ffeM8Y46WD6dr0fDh` |

Refs verified 2026-11-19: `origin/main` = `fa87215` (Merge PR #50). The `982d3e6`
run's SUMMARY calls `982d3e6` "origin/main"; that was true when it started but
upstream has since moved. The `a210f29` run is the baseline the maintainer
named ("main sekarang a210f29"); nara asked that it be labelled stale relative
to upstream; both labels are recorded here rather than silently overwritten.

All runs: format/clippy/tests green; deny green only after working around the
sandbox cargo wrapper (`/usr/local/bin/cargo` forces a read-only `CARGO_HOME`).
See `a210f29/report.md` for the root cause and two verified workarounds, and
`docs/agents/questions/open/Q-029-cargo-deny-advisory-db-path.md`.