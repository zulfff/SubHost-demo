# Manual testing: two wallets

This walkthrough runs a chain end to end: wallet creation, ed25519 signing,
balances, nonces, fees, block production, account proofs, restart persistence, and
two-node block gossip.

It is not a multi-node testnet. `--p2p-listen` relays blocks between nodes, but the
node has no consensus, no fork choice, and no block sync, so only one node may
produce.

## 1. Build

```bash
cargo build -p subhost-cli
export SUBHOST_BIN="$(pwd)/target/debug/subhost"
```

`.cargo/config.toml` caps build parallelism at two jobs, which keeps the build
inside about 4 GB of RAM.

### Scripted path

```bash
scripts/two-user-setup.sh
```

The script creates Alice and Bob in a throwaway directory, funds both in genesis,
and prints the exact commands for three terminals:

```bash
# Terminal 1: the node command the script printed
# Terminal 2:
source /tmp/subhost-two-users-current.env && scripts/user1-alice.sh
# Terminal 3:
source /tmp/subhost-two-users-current.env && scripts/user2-bob.sh
```

Alice sends 100 units to Bob, and Bob sends 25 back. Each script prints the balance
before and after, the receipt, and the resulting block. The scripts let the CLI
resolve the nonce, so you can rerun them without editing a counter.

Override the port with `RPC_URL`, for example
`RPC_URL=http://127.0.0.1:18545 scripts/user1-alice.sh`.

For the browser dashboard, use `scripts/start-web-demo.sh`, which starts a node and
the explorer together on `http://127.0.0.1:3000`.

The rest of this document does the same thing by hand.

## 2. Isolate the demo from your real wallets

```bash
export SUBHOST_TEST_HOME="$(mktemp -d /tmp/subhost-home.XXXXXX)"
export SUBHOST_TEST_DATA="$(mktemp -d /tmp/subhost-data.XXXXXX)"
export SUBHOST_HOME="$SUBHOST_TEST_HOME/.subhost"
```

`SUBHOST_HOME` is what the CLI reads, so nothing touches `~/.subhost`.

## 3. Create Alice and Bob

The passwords below are examples. Passing a password as a CLI argument exposes it
to shell history and the process list, so do not use this pattern for real funds.

```bash
"$SUBHOST_BIN" wallet new --password "alice-pass-123" --name alice
"$SUBHOST_BIN" wallet new --password "bob-pass-123" --name bob
"$SUBHOST_BIN" wallet list
```

Save the two printed addresses:

```bash
export ALICE_ADDRESS="0x..."
export BOB_ADDRESS="0x..."
```

Encrypted wallet files live in `$SUBHOST_HOME/wallets/` with `0600` permissions.
Inspect one with:

```bash
"$SUBHOST_BIN" wallet show "$ALICE_ADDRESS"
```

## 4. Write genesis and start the node

`init` must run before the node starts for the first time. The node applies
allocations only when no ledger exists yet.

```bash
"$SUBHOST_BIN" init \
  --chain-id 1 \
  --data-dir "$SUBHOST_TEST_DATA" \
  --alloc "$ALICE_ADDRESS=1000000" \
  --alloc "$BOB_ADDRESS=100000"
```

`init` reports that no validators are configured. That is expected: the genesis is
valid for a single-node chain, and only `node --validator` requires a validator
set.

In a second terminal:

```bash
export SUBHOST_BIN="$(pwd)/target/debug/subhost"
export SUBHOST_TEST_DATA="/tmp/subhost-data.REPLACE_ME"
"$SUBHOST_BIN" node --listen 127.0.0.1:8545 --data-dir "$SUBHOST_TEST_DATA"
```

Leave it running. The endpoint is unauthenticated, so keep it on loopback.

## 5. Check the chain and the balances

Back in the first terminal:

```bash
"$SUBHOST_BIN" query chain
"$SUBHOST_BIN" query balance "$ALICE_ADDRESS"
"$SUBHOST_BIN" query balance "$BOB_ADDRESS"
```

Expected:

```text
chain_id  1
height    0
0x...alice   1000000
0x...bob     100000
```

Balances print in decimal. Raw JSON-RPC returns hex.

## 6. Alice sends to Bob

```bash
"$SUBHOST_BIN" tx send \
  --from "$ALICE_ADDRESS" \
  --to "$BOB_ADDRESS" \
  --amount 100 \
  --password "alice-pass-123"
```

This decrypts Alice's key, builds an unsigned transaction, signs it, queries the
chain ID and nonce from the node, submits `eth_sendTransaction`, and prints the
hash of the committed block's transaction. Pass `--nonce` and `--chain-id`
explicitly to override the queried values.

With `gas_price = 1` and `gas_limit = 21000` the fee is 21000. Alice therefore ends
at `1000000 - 100 - 21000 = 978900`, and Bob ends at `100000 + 100 = 100100`.

## 7. Verify the receipt and the block

```bash
export TX_HASH="0x..."   # the hash printed above
"$SUBHOST_BIN" tx status "$TX_HASH"
"$SUBHOST_BIN" query balance "$ALICE_ADDRESS"
"$SUBHOST_BIN" query balance "$BOB_ADDRESS"
"$SUBHOST_BIN" query nonce "$ALICE_ADDRESS"
"$SUBHOST_BIN" query block --full
```

A successful receipt has `"status": "0x1"`, `"blockNumber": "0x1"`, and
`"gasUsed": "0x5208"`. Alice's nonce is now `1`.

## 8. Bob sends back

```bash
"$SUBHOST_BIN" tx send \
  --from "$BOB_ADDRESS" \
  --to "$ALICE_ADDRESS" \
  --amount 25 \
  --password "bob-pass-123"
```

This becomes block 2. Bob pays the 25 plus the 21000 fee, so Bob ends at `79075`
and Alice at `978925`. The node seals one block per submission and fills it with
every pending transaction that is currently executable. If another sender submits
at the same time, both transfers share a block, and their receipts report distinct
`transactionIndex` and `cumulativeGasUsed` values.

## 9. Confirm the failure paths

Each of these must be rejected. If any of them succeeds, that is a bug worth
reporting.

```bash
# Replayed nonce.
"$SUBHOST_BIN" tx send --from "$ALICE_ADDRESS" --to "$BOB_ADDRESS" \
  --amount 1 --password "alice-pass-123" --nonce 0

# Wrong password.
"$SUBHOST_BIN" tx send --from "$ALICE_ADDRESS" --to "$BOB_ADDRESS" \
  --amount 1 --password "wrong-password"

# More than the balance covers.
"$SUBHOST_BIN" tx send --from "$ALICE_ADDRESS" --to "$BOB_ADDRESS" \
  --amount 999999999999 --password "alice-pass-123"

# Wrong chain.
"$SUBHOST_BIN" tx send --from "$ALICE_ADDRESS" --to "$BOB_ADDRESS" \
  --amount 1 --password "alice-pass-123" --chain-id 99

# An unsigned request straight to the RPC.
curl -s http://127.0.0.1:8545 -H 'content-type: application/json' --data "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendTransaction\",
  \"params\":[{\"from\":\"$ALICE_ADDRESS\",\"to\":\"$BOB_ADDRESS\",\"value\":\"0x1\"}]}"
```

Expected results, in order: `transaction is not executable against the current
state` for the stale nonce, `decryption failed` for the password, the same
executable-state message for the overdrawn transfer, `invalid chain ID`, and
`-32602` for the unsigned request. The node validates the nonce and the balance at
execution time and collapses both failures into one rejection, so it does not say
which check failed.

## 10. Confirm persistence

Stop the node with `Ctrl-C`, then start it again with the same `--data-dir`. It logs
`restored existing ledger` with the height, and every balance and nonce survives.
The ledger is checksummed and every block and receipt commitment is replayed on
load, so a corrupted file is refused rather than partially loaded.

## 11. Optional: account proof

```bash
curl -s http://127.0.0.1:8545 -H 'content-type: application/json' --data "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getProof\",
  \"params\":[\"$ALICE_ADDRESS\",[],\"latest\"]}"
```

The response carries `stateRoot`, the account's balance and nonce, and an
`accountProof` array whose first element is the leaf and whose remainder is the
sibling path. An address that has never held a balance returns an error, because
this tree proves membership and not absence.

## 12. Optional: two nodes over gossip

Gossip delivers live blocks only. Both nodes must start at height 0, and the
follower must connect before the first block is produced. There is no backfill for
a node that was offline.

```bash
export SUBHOST_PRODUCER_DATA="$(mktemp -d /tmp/subhost-producer.XXXXXX)"
export SUBHOST_FOLLOWER_DATA="$(mktemp -d /tmp/subhost-follower.XXXXXX)"
"$SUBHOST_BIN" init --chain-id 1 --data-dir "$SUBHOST_PRODUCER_DATA" \
  --alloc "$ALICE_ADDRESS=1000000"
cp "$SUBHOST_PRODUCER_DATA/genesis.json" "$SUBHOST_FOLLOWER_DATA/genesis.json"
```

```bash
# Terminal A: the producer.
"$SUBHOST_BIN" node --listen 127.0.0.1:8545 --data-dir "$SUBHOST_PRODUCER_DATA" \
  --p2p-listen /ip4/127.0.0.1/tcp/30333 --no-mdns

# Terminal B: the follower. It restores the same genesis and never receives a
# transaction of its own.
"$SUBHOST_BIN" node --listen 127.0.0.1:18545 --data-dir "$SUBHOST_FOLLOWER_DATA" \
  --p2p-listen /ip4/127.0.0.1/tcp/30334 \
  --bootstrap-peer /ip4/127.0.0.1/tcp/30333 --no-mdns
```

Send a transfer to node A, then ask node B:

```bash
"$SUBHOST_BIN" query chain --rpc-url http://127.0.0.1:18545
```

The follower logs `accepted a peer block` and reaches the same height, root, and
balance. A block that arrives before its parent is buffered (up to 64 heights) and
applied once the gap fills. If both nodes seal a block at the same height before
either block reaches the other, they produce two different blocks at that height.
Each node rejects the other's block because it does not extend the local tip, and
neither recovers. That is expected until fork choice exists. On loopback, gossip
usually arrives before the second node seals, so the race is not guaranteed.

## 13. Optional: metrics

Start the node with `--metrics-addr 127.0.0.1:9090`, then:

```bash
curl -s http://127.0.0.1:9090/metrics | grep '^subhost'
curl -s http://127.0.0.1:9090/health
```

`subhost_block_height` tracks the chain tip and `subhost_pending_transactions`
tracks mempool depth. The registry also defines `subhost_rpc_requests_total`,
`subhost_rpc_errors_total`, and a request-duration histogram, but the node does
not increment them yet, so they stay at zero.

## 14. Optional: migrate a version 2 ledger

A node built before S4 stores its history in one `node-state.bin`. Point the
maintenance command at that data directory while the node is stopped:

```bash
subhost db migrate --data-dir "$SUBHOST_TEST_DATA" --chain-id 1
```

It validates the whole version 2 file before writing anything, converts it to
`ledger/manifest.json` plus `ledger/segments/`, re-reads the result and compares
the head, and keeps the original as `node-state.bin.v2-backup`. Running it again
prints `is already segmented; nothing to do`; a fresh directory prints `nothing to
do` as well. If the legacy file is corrupt or an unknown version, the command
fails and leaves the directory untouched.

## 15. Optional: prune old segments

Pruning drops the oldest `ledger/segments/` files and is only safe with the node
stopped. It keeps the newest snapshot as the replay anchor:

```bash
subhost db prune --data-dir "$SUBHOST_TEST_DATA" --chain-id 1 --keep-segments 1
```

The command prints the new base height and how many segments it removed. After
restarting the node:

```bash
curl -s http://127.0.0.1:8545 -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
curl -s http://127.0.0.1:8545 -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"eth_getBlockByNumber","params":["earliest",false]}'
curl -s http://127.0.0.1:8545 -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"eth_getBlockByNumber","params":["0x1",false]}'
```

`eth_blockNumber` is unchanged, `earliest` is the oldest retained block, and a
pruned height answers `null`. A pruned transaction also answers `null`; there is no
backfill, so do not prune a node you rely on as an archive. `--keep-segments 0` is
refused, and so is a prune that would drop the block the snapshot anchors
(`refusing to prune: the retained history would start at height …`); on refusal the
directory is untouched.

## 16. Optional: require a bearer token

Stop the node, then create a token and restart with auth plus a slow rate limit:

```bash
openssl rand -hex 32 > "$SUBHOST_TEST_HOME/rpc-token"
chmod 600 "$SUBHOST_TEST_HOME/rpc-token"
subhost node --listen 127.0.0.1:8545 --data-dir "$SUBHOST_TEST_DATA" \
  --rpc-auth-token-file "$SUBHOST_TEST_HOME/rpc-token" --rpc-rate-limit 1
```

Without the header the node answers `401` and JSON-RPC error `-32000`:

```bash
curl -s -i http://127.0.0.1:8545 -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

With the token it answers normally, and the CLI does the same when you pass
`--rpc-auth-token-file` (or export `SUBHOST_RPC_AUTH_TOKEN_FILE`):

```bash
TOKEN="$(cat "$SUBHOST_TEST_HOME/rpc-token")"
curl -s http://127.0.0.1:8545 -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
subhost query chain --rpc-auth-token-file "$SUBHOST_TEST_HOME/rpc-token"
```

Two requests over **one** connection exceed the limit and the second answers `429`
with `Retry-After: 1` and error `-32005`. curl reuses the connection when it is
given both URLs in one invocation:

```bash
curl -s -o /dev/null -w '%{http_code} ' -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  http://127.0.0.1:8545 -o /dev/null http://127.0.0.1:8545
```

The output is `200 429`. Separate `curl` invocations each open a new connection
and therefore each get a fresh bucket; that is the per-connection limitation, not
a broken limit.

A non-loopback bind is refused before the socket opens unless the operator opts in,
and the CLI will not accept the opt-in without a token:

```bash
subhost node --listen 0.0.0.0:8545 --data-dir "$SUBHOST_TEST_DATA"
# refusing to bind JSON-RPC to the non-loopback address 0.0.0.0:8545: pass
# --rpc-allow-public to expose it deliberately
subhost node --listen 0.0.0.0:8545 --data-dir "$SUBHOST_TEST_DATA" --rpc-allow-public
# error: the following required arguments were not provided:
#   --rpc-auth-token-file <RPC_AUTH_TOKEN_FILE>
```

The library enforces the same rule below clap: `subhost-rpc::start` refuses a
public address without a token (`BindPolicyError::AuthRequired`), so an embedder
that builds `RpcSecurityConfig` directly cannot skip it.

There is no in-process TLS. For a public deployment, keep `--listen
127.0.0.1:8545` and terminate TLS in a reverse proxy that injects or forwards the
`Authorization: Bearer` header.

## 17. Clean up

Stop the node with `Ctrl-C` first.

```bash
rm -rf "$SUBHOST_TEST_HOME" "$SUBHOST_TEST_DATA" "$SUBHOST_PRODUCER_DATA" \
  "$SUBHOST_FOLLOWER_DATA"
```

That only removes the `/tmp` directories created above.

## Common errors

- `no wallet for 0x… in …`: `SUBHOST_HOME` differs from where the wallet was
  created, or the address is wrong.
- `decryption failed (wrong password or corrupt wallet)`: wrong password, or the
  file was modified.
- `transaction is not executable against the current state`: the nonce is not the
  account's next nonce, or the balance does not cover `amount + gas_price *
  gas_limit`. The node reports both as one rejection, so check `query nonce` and
  `query balance`.
- `invalid chain ID`: `--chain-id` must match the node.
- `cannot reach the node at …`: the node is not running on that port.
- `a validator network requires at least one initial validator`: `node
  --validator` needs `init --validator`.