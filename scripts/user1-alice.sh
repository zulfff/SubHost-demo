#!/usr/bin/env bash
# Send a transfer as Alice, then show the receipt and the resulting block.
#
# The nonce is left to the CLI, which queries the node, so this script can run
# repeatedly without hand-editing a counter.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SUBHOST_BIN="${SUBHOST_BIN:-$ROOT_DIR/target/debug/subhost}"
DEMO_ENV="${SUBHOST_TWO_USERS_ENV:-/tmp/subhost-two-users-current.env}"

if [[ -z "${SUBHOST_TEST_HOME:-}" && -f "$DEMO_ENV" ]]; then
    # shellcheck disable=SC1090
    source "$DEMO_ENV"
fi

RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
ALICE_PASSWORD="${ALICE_PASSWORD:-alice-pass-123}"
AMOUNT="${ALICE_AMOUNT:-100}"

if [[ ! -x "$SUBHOST_BIN" ]]; then
    printf 'Binary not found: %s\n' "$SUBHOST_BIN" >&2
    exit 1
fi
if [[ -z "${ALICE_ADDRESS:-}" || -z "${BOB_ADDRESS:-}" ]]; then
    printf 'Wallet addresses are unknown. Run scripts/two-user-setup.sh first.\n' >&2
    exit 1
fi

export SUBHOST_HOME="${SUBHOST_TEST_HOME:?SUBHOST_TEST_HOME is required}/.subhost"

printf '\n[Alice] balance before\n'
"$SUBHOST_BIN" query balance "$ALICE_ADDRESS" --rpc-url "$RPC_URL"

printf '\n[Alice] sending %s to Bob\n' "$AMOUNT"
TX_HASH="$("$SUBHOST_BIN" tx send \
    --from "$ALICE_ADDRESS" \
    --to "$BOB_ADDRESS" \
    --amount "$AMOUNT" \
    --password "$ALICE_PASSWORD" \
    --rpc-url "$RPC_URL")"
printf '%s\n' "$TX_HASH"

printf '\n[Alice] receipt\n'
"$SUBHOST_BIN" tx status "$TX_HASH" --rpc-url "$RPC_URL"

printf '\n[Alice] balance after\n'
"$SUBHOST_BIN" query balance "$ALICE_ADDRESS" --rpc-url "$RPC_URL"

printf '\n[Alice] latest block\n'
"$SUBHOST_BIN" query block --full --rpc-url "$RPC_URL"
