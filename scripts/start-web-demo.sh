#!/usr/bin/env bash
# Start a single node plus the web explorer against two funded demo wallets.
#
# Both listeners bind to loopback: the explorer signs with a local wallet, so it
# must never be reachable off-host.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SUBHOST_BIN="${SUBHOST_BIN:-$ROOT_DIR/target/debug/subhost}"
EXPLORER_BIN="${EXPLORER_BIN:-$ROOT_DIR/target/debug/subhost-explorer}"
RPC_PORT="${RPC_PORT:-18545}"
WEB_PORT="${WEB_PORT:-3000}"
INITIAL_BALANCE="${INITIAL_BALANCE:-1000000000}"
DEMO_HOME="${SUBHOST_TEST_HOME:-$(mktemp -d /tmp/subhost-web-home.XXXXXX)}"
DEMO_DATA="${SUBHOST_TEST_DATA:-$(mktemp -d /tmp/subhost-web-data.XXXXXX)}"
DEMO_ENV="${SUBHOST_TWO_USERS_ENV:-/tmp/subhost-web-demo-current.env}"

if [[ ! -x "$SUBHOST_BIN" || ! -x "$EXPLORER_BIN" ]]; then
    printf 'Demo binaries are missing. Build them first:\n  cargo build -p subhost-cli -p subhost-explorer\n' >&2
    exit 1
fi

SUBHOST_TEST_HOME="$DEMO_HOME" \
SUBHOST_TEST_DATA="$DEMO_DATA" \
SUBHOST_TWO_USERS_ENV="$DEMO_ENV" \
INITIAL_BALANCE="$INITIAL_BALANCE" \
    "$ROOT_DIR/scripts/two-user-setup.sh"

cleanup() {
    if [[ -n "${NODE_PID:-}" ]]; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

"$SUBHOST_BIN" node --listen "127.0.0.1:$RPC_PORT" --data-dir "$DEMO_DATA" &
NODE_PID=$!

# Poll until the node answers, failing fast if it exited instead.
ready=0
for _ in $(seq 1 50); do
    if "$SUBHOST_BIN" query chain --rpc-url "http://127.0.0.1:$RPC_PORT" >/dev/null 2>&1; then
        ready=1
        break
    fi
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        printf 'The node exited during startup. Is port %s already in use?\n' "$RPC_PORT" >&2
        exit 1
    fi
    sleep 0.2
done
if [[ "$ready" -ne 1 ]]; then
    printf 'The node did not become ready in time.\n' >&2
    exit 1
fi

cat <<EOF

Web demo ready:
  http://127.0.0.1:$WEB_PORT

Starting balances:
  Alice: $INITIAL_BALANCE units (password: alice-pass-123)
  Bob:   $INITIAL_BALANCE units (password: bob-pass-123)

Press Ctrl-C to stop both the node and the explorer.
EOF

SUBHOST_TEST_HOME="$DEMO_HOME" \
SUBHOST_TWO_USERS_ENV="$DEMO_ENV" \
SUBHOST_RPC_URL="http://127.0.0.1:$RPC_PORT" \
EXPLORER_LISTEN_ADDR="127.0.0.1:$WEB_PORT" \
    "$EXPLORER_BIN"
