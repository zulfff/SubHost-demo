#!/usr/bin/env bash
# Send a transfer as Bob, then show the receipt and the resulting block.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SUBHOST_BIN="${SUBHOST_BIN:-$ROOT_DIR/target/debug/subhost}"
DEMO_ENV="${SUBHOST_TWO_USERS_ENV:-/tmp/subhost-two-users-current.env}"

if [[ -z "${SUBHOST_TEST_HOME:-}" && -f "$DEMO_ENV" ]]; then
    # shellcheck disable=SC1090
    source "$DEMO_ENV"
fi

RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
BOB_PASSWORD="${BOB_PASSWORD:-bob-pass-123}"
AMOUNT="${BOB_AMOUNT:-25}"

if [[ ! -x "$SUBHOST_BIN" ]]; then
    printf 'Binary not found: %s\n' "$SUBHOST_BIN" >&2
    exit 1
fi
if [[ -z "${ALICE_ADDRESS:-}" || -z "${BOB_ADDRESS:-}" ]]; then
    printf 'Wallet addresses are unknown. Run scripts/two-user-setup.sh first.\n' >&2
    exit 1
fi

export SUBHOST_HOME="${SUBHOST_TEST_HOME:?SUBHOST_TEST_HOME is required}/.subhost"

printf '\n[Bob] balance before\n'
"$SUBHOST_BIN" query balance "$BOB_ADDRESS" --rpc-url "$RPC_URL"

printf '\n[Bob] sending %s to Alice\n' "$AMOUNT"
TX_HASH="$("$SUBHOST_BIN" tx send \
    --from "$BOB_ADDRESS" \
    --to "$ALICE_ADDRESS" \
    --amount "$AMOUNT" \
    --password "$BOB_PASSWORD" \
    --rpc-url "$RPC_URL")"
printf '%s\n' "$TX_HASH"

printf '\n[Bob] receipt\n'
"$SUBHOST_BIN" tx status "$TX_HASH" --rpc-url "$RPC_URL"

printf '\n[Bob] balance after\n'
"$SUBHOST_BIN" query balance "$BOB_ADDRESS" --rpc-url "$RPC_URL"

printf '\n[Bob] latest block\n'
"$SUBHOST_BIN" query block --full --rpc-url "$RPC_URL"
