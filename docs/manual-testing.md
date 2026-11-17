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
  --alloc "$ALICE_ADDRESS=1000000"
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
0x...bob     0
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
at `1000000 - 100 - 21000 = 978900`, and Bob receives `100`.

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

This becomes block 2. The node seals one block per submission and fills it with
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

Expected errors, in order: `invalid nonce`, `decryption failed`, `insufficient
balance`, `invalid chain ID`, and `-32602` for the unsigned request.

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
applied once the gap fills. Sending a transfer to **both** nodes produces two
different blocks at the same height. Each node rejects the other's block because it
does not extend the local tip, and neither recovers. That is expected until fork
choice exists.

## 13. Optional: metrics

Start the node with `--metrics-addr 127.0.0.1:9090`, then:

```bash
curl -s http://127.0.0.1:9090/metrics | grep '^subhost'
curl -s http://127.0.0.1:9090/health
```

`subhost_block_height` tracks the chain tip and `subhost_pending_transactions`
tracks mempool depth.

## 14. Clean up

Stop the node with `Ctrl-C` first.

```bash
rm -rf "$SUBHOST_TEST_HOME" "$SUBHOST_TEST_DATA" "$SUBHOST_FOLLOWER_DATA"
```

That only removes the `/tmp` directories created above.

## Common errors

- `no wallet for 0x… in …`: `SUBHOST_HOME` differs from where the wallet was
  created, or the address is wrong.
- `decryption failed (wrong password or corrupt wallet)`: wrong password, or the
  file was modified.
- `invalid nonce for 0x…: expected N, got M`: check `query nonce`.
- `insufficient balance`: `amount + gas_price * gas_limit` exceeds the balance.
- `invalid chain ID`: `--chain-id` must match the node.
- `cannot reach the node at …`: the node is not running on that port.
- `a validator network requires at least one initial validator`: `node
  --validator` needs `init --validator`.