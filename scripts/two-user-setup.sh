#!/usr/bin/env bash
# Create two demo wallets and a genesis file that funds both.
#
# Everything lives under throwaway directories so the script never touches a real
# ~/.subhost. The environment file it writes contains addresses only, never keys.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SUBHOST_BIN="${SUBHOST_BIN:-$ROOT_DIR/target/debug/subhost}"
DEMO_HOME="${SUBHOST_TEST_HOME:-$(mktemp -d /tmp/subhost-demo-home.XXXXXX)}"
DEMO_DATA="${SUBHOST_TEST_DATA:-$(mktemp -d /tmp/subhost-demo-data.XXXXXX)}"
DEMO_ENV="${SUBHOST_TWO_USERS_ENV:-/tmp/subhost-two-users-current.env}"
ALICE_PASSWORD="${ALICE_PASSWORD:-alice-pass-123}"
BOB_PASSWORD="${BOB_PASSWORD:-bob-pass-123}"
INITIAL_BALANCE="${INITIAL_BALANCE:-1000000000}"
CHAIN_ID="${CHAIN_ID:-1}"

if [[ ! -x "$SUBHOST_BIN" ]]; then
    printf 'Binary not found: %s\nBuild it first:\n  cargo build -p subhost-cli\n' "$SUBHOST_BIN" >&2
    exit 1
fi

# Refuse to overwrite an initialized data directory: doing so would orphan the
# existing ledger against a new genesis.
if [[ -e "$DEMO_DATA/genesis.json" || -e "$DEMO_DATA/node-state.bin" ]]; then
    printf 'Data directory is already initialized: %s\nUse a fresh SUBHOST_TEST_DATA path.\n' "$DEMO_DATA" >&2
    exit 1
fi

# Key material must not be group or world readable.
umask 077
mkdir -p "$DEMO_HOME/.subhost/wallets" "$DEMO_DATA"

SUBHOST_HOME="$DEMO_HOME/.subhost" "$SUBHOST_BIN" wallet new --password "$ALICE_PASSWORD" --name alice
SUBHOST_HOME="$DEMO_HOME/.subhost" "$SUBHOST_BIN" wallet new --password "$BOB_PASSWORD" --name bob

read_address() {
    sed -n 's/.*"address": *"\([^"]*\)".*/\1/p' "$1" | head -n 1
}
ALICE_ADDRESS="$(read_address "$DEMO_HOME/.subhost/wallets/alice.json")"
BOB_ADDRESS="$(read_address "$DEMO_HOME/.subhost/wallets/bob.json")"

if [[ -z "$ALICE_ADDRESS" || -z "$BOB_ADDRESS" ]]; then
    printf 'Could not read the generated wallet addresses.\n' >&2
    exit 1
fi

printf 'export SUBHOST_TEST_HOME=%q\nexport SUBHOST_TEST_DATA=%q\nexport ALICE_ADDRESS=%q\nexport BOB_ADDRESS=%q\n' \
    "$DEMO_HOME" "$DEMO_DATA" "$ALICE_ADDRESS" "$BOB_ADDRESS" > "$DEMO_ENV"

"$SUBHOST_BIN" init \
    --chain-id "$CHAIN_ID" \
    --data-dir "$DEMO_DATA" \
    --alloc "$ALICE_ADDRESS=$INITIAL_BALANCE" \
    --alloc "$BOB_ADDRESS=$INITIAL_BALANCE"

cat <<EOF

Setup complete. The environment file below holds addresses only, no private keys:
  source '$DEMO_ENV'

Start the node:
  '$SUBHOST_BIN' node --listen 127.0.0.1:8545 --data-dir '$DEMO_DATA'

Then, in two more terminals:
  source '$DEMO_ENV' && '$ROOT_DIR/scripts/user1-alice.sh'
  source '$DEMO_ENV' && '$ROOT_DIR/scripts/user2-bob.sh'
EOF
