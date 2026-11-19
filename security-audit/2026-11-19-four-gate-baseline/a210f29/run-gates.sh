#!/usr/bin/env bash
# Reproduce the four-gate baseline at a210f29 (QA support, unseated).
# Worktree lives under /home/zulfff/subhost-wt/ — /tmp/opencode worktrees were
# being deleted mid-session on 2026-11-19.
set -uo pipefail

WT=/home/zulfff/subhost-wt/qa-a210f29
EV="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/evidence"
mkdir -p "$EV"
cd "$WT" || exit 1

run_gate() {
    local name=$1; shift
    local log="$EV/$name.log" start end rc
    start=$(date +%s)
    "$@" >"$log" 2>&1
    rc=$?
    end=$(date +%s)
    printf '{"gate":"%s","exit_code":%d,"seconds":%d,"command":%s}\n' \
        "$name" "$rc" $((end - start)) \
        "$(printf '%s\n' "$@" | jq -R . | jq -cs .)" >>"$EV/gates.jsonl"
    printf '%-10s exit=%d %ss\n' "$name" "$rc" $((end - start))
    return 0
}

: >"$EV/gates.jsonl"
{
    printf 'commit=%s\n' "$(git rev-parse HEAD)"
    printf 'date=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    cargo --version
    rustc --version
} >"$EV/env.txt"

run_gate format cargo fmt --all -- --check
run_gate clippy cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
run_gate tests cargo test --workspace --all-features --locked
# Plain `cargo deny` fails in this sandbox: /usr/local/bin/cargo forces a
# read-only CARGO_HOME (Q-029). Call the binary directly instead.
run_gate deny env RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/zulfff/.cargo \
    CARGO=/usr/local/cargo/bin/cargo /home/zulfff/.local/bin/cargo-deny --all-features check

jq -s . "$EV/gates.jsonl" >"$EV/gates.json" && rm "$EV/gates.jsonl"