# Manual Testing: Two Wallets

This walks through a single-node chain end to end: wallet creation, ed25519
signing, balances, nonces, fees, block production, and restart persistence.

It is not a multi-node testnet. There is no P2P and no consensus.

## 1. Build

```bash
cargo build -p subhost-cli
export SUBHOST_BIN="$(pwd)/target/debug/subhost"
```

Build parallelism is capped at two jobs by `.cargo/config.toml`, which keeps the
build inside about 4 GB of RAM.

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

Alice sends 100 units to Bob; Bob sends 25 back. Each script prints the balance
before and after, the receipt, and the resulting block. The scripts let the CLI
resolve the nonce, so they can be rerun without editing a counter.

Override the port with `RPC_URL`, for example
`RPC_URL=http://127.0.0.1:18545 scripts/user1-alice.sh`.

For the browser dashboard, use `scripts/start-web-demo.sh`, which starts a node
and the explorer together on `http://127.0.0.1:3000`.

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
to shell history and the process list; do not use this pattern for real funds.

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

`init` must run before the node starts for the first time; the node applies
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

With `gas_price = 1` and `gas_limit = 21000` the fee is 21000, so Alice goes to
`1000000 - 100 - 21000 = 978900` and Bob receives `100`.

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

This becomes block 2. Each accepted transaction is sealed into its own block.

## 9. Confirm the failure paths

Each of these must be rejected. If any succeeds, that is a bug worth reporting.

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

Stop the node with `Ctrl-C`, then start it again with the same `--data-dir`. It
logs `restored existing ledger` with the height, and every balance and nonce
survives. The ledger is checksummed and every block and receipt commitment is
replayed on load, so a corrupted file is refused rather than partially loaded.

## 11. Optional: metrics

Start the node with `--metrics-addr 127.0.0.1:9090`, then:

```bash
curl -s http://127.0.0.1:9090/metrics | grep '^subhost'
curl -s http://127.0.0.1:9090/health
```

`subhost_block_height` tracks the chain tip and `subhost_pending_transactions`
tracks mempool depth.

## Common errors

- `no wallet for 0x… in …` — `SUBHOST_HOME` differs from where the wallet was
  created, or the address is wrong.
- `decryption failed (wrong password or corrupt wallet)` — wrong password, or the
  file was modified.
- `invalid nonce for 0x…: expected N, got M` — check `query nonce`.
- `insufficient balance` — `amount + gas_price * gas_limit` exceeds the balance.
- `invalid chain ID` — `--chain-id` must match the node.
- `cannot reach the node at …` — the node is not running on that port.
- `a validator network requires at least one initial validator` — `node
  --validator` needs `init --validator`.

## 12. Clean up

```bash
# Stop the node with Ctrl-C first.
rm -rf "$SUBHOST_TEST_HOME" "$SUBHOST_TEST_DATA"
```

Only removes the `/tmp` directories created above.
