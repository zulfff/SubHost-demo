#!/usr/bin/env bash
# Independent four-gate baseline for origin/main 982d3e6 (seat nara, track-b).
# Run from a clean detached worktree; logs land next to this script.
set -u
W="${1:-/home/zulfff/orca/workspaces/SubHost-demo/qa-baseline}"
OUT="${2:-/home/zulfff/SubHost-demo/security-audit/2026-11-19-four-gate-baseline/982d3e6}"
mkdir -p "$OUT"
cd "$W" || exit 1

{
  echo "worktree=$W"
  echo "commit=$(git rev-parse HEAD)"
  echo "describe=$(git log --oneline -1)"
  echo "start=$(date -Is)"
  echo "cargo=$(cargo --version)"
  echo "cargo-deny=$(/home/zulfff/.local/bin/cargo-deny --version)"
} > "$OUT/run.txt"

gate() {
  name=$1; shift
  echo "== $name: $*" >> "$OUT/run.txt"
  "$@" > "$OUT/$name.log" 2>&1
  echo "$name exit=$?" >> "$OUT/run.txt"
}

gate fmt    cargo fmt --all -- --check
gate clippy cargo clippy --workspace --all-targets --all-features -- -D warnings
gate test   cargo test --workspace --all-features
gate deny   env RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/home/zulfff/.cargo \
            CARGO=/usr/local/cargo/bin/cargo \
            /home/zulfff/.local/bin/cargo-deny --all-features check

echo "end=$(date -Is)" >> "$OUT/run.txt"