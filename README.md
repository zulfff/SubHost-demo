<p align="center">
  <img src="assets/brand/subhost-logo.svg" width="200" alt="Subhost Web3">
</p>

<h1 align="center">Subhost Web3</h1>

<p align="center">
  <strong>A Rust workspace exploring decentralized cloud infrastructure</strong>
</p>

<p align="center">
  <a href="https://github.com/zulfff/SubHost-demo/actions/workflows/ci.yml"><img src="https://github.com/zulfff/SubHost-demo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <img src="https://img.shields.io/badge/rustc-1.89%2B-orange.svg" alt="MSRV">
</p>

---

## What this is

Subhost Web3 is a Rust workspace built around a working blockchain node. The node
serves an Ethereum-shaped JSON-RPC endpoint, accepts signed transfers, and seals
blocks that carry several transactions at once. The ledger is crash-safe, and the
node replays it on load. Blocks commit to Merkle state and receipt roots, and
accounts can be proven against the state root. An optional libp2p transport
gossips blocks between nodes. The workspace also holds an IBC packet state machine
with a single-signer light client, and an in-process BFT agreement driver.

This is not a production network. Block production belongs to whichever node
receives the transaction, and there is no fork choice, view change, or leader
rotation. Two producers that seal at the same height fork instead of converging.
The table below lists what each crate does today, and nothing in this repository
should be treated as production ready.

### Implementation status

| Crate | Status |
|-------|--------|
| `subhost-core` | Complete. Hash/Address (BLAKE3), block, transaction (with sender public key), receipt, genesis document, canonical encoding, domain-separated Merkle tree. |
| `subhost-crypto` | Complete. BLS12-381 sign/verify/aggregate with proof of possession, ed25519, X25519 (contributory-checked), ChaCha20-Poly1305. |
| `subhost-wallet` | Complete. scrypt + AES-256-GCM keystore, atomic `0600` writes, address verified against the decrypted key. |
| `subhost-state` | Complete. Balances, nonce ordering, replay rejection, checked arithmetic, Merkle state root bound to the chain ID, per-account inclusion proofs. |
| `subhost-mempool` | Complete. Per-sender nonce pool, replace-by-fee, capacity eviction, deterministic proposal order. |
| `subhost-storage` | Complete. Versioned (v2), checksummed, atomically written ledger with multi-transaction blocks; every block and receipt commitment is replayed on load. v1 files are refused. |
| `subhost-rpc` | Working subset. 11 JSON-RPC methods, mandatory signature verification, one block per submission carrying every executable pending transaction, optional BLS signing of produced blocks, producer-signature verification when the genesis has a validator set, full re-validation of peer blocks, persist-before-commit. |
| `subhost-node` | Complete. Genesis load, validator registry from genesis, producer key load with permission and scalar checks, ledger restore, RPC and metrics wiring, graceful shutdown, optional peer gossip that publishes locally produced blocks and applies peer blocks in height order. |
| `subhost-telemetry` | Complete. One `RUST_LOG`-aware subscriber, optional JSON output. |
| `subhost-metrics` | Complete. Prometheus registry and `/metrics` + `/health` exporter. |
| `subhost-consensus` | Primitives plus an in-process driver. DAG admission, quorum support, BLS-verified quorum certificates, staking and slashing, and a `2f + 1` view-by-view agreement driver. No networking, timeouts, view changes, or leader rotation. |
| `subhost-network` | Transport. libp2p gossip publish and receive, connection limits, and block-proposal delivery consumed by `subhost-node`. No Dandelion++ relay, no peer scoring, no block sync protocol. |
| `subhost-ibc` | State machine plus a single-signer light client. Channel lifecycle, sequencing, timeouts, replay rejection, signed-header verification with monotonic height/time, and Merkle commitment proofs for packet delivery. No validator-set rotation or multisig quorum certificates. |
| `subhost-faucet` | Complete. Signs and submits real transfers from an encrypted wallet, per-address cooldown. |
| `subhost-cli` | Complete. Every command performs a real operation. |
| `subhost-explorer` | Complete. Read-only dashboard plus a loopback-only demo signing endpoint. |
| `subhost-bench` | Complete. Load and latency measurement, reporting successes and failures separately. |

Not implemented anywhere in this repository: EVM execution, WASM contracts, zk
proofs, on-chain governance, a Merkle Patricia Trie, erasure-coded storage, or a
threshold-encrypted mempool. The network also lacks fork choice, reorg handling,
block sync/backfill, peer scoring, and any multi-validator light-client
verification. Two block producers that seal at the same height will fork, because
nothing arbitrates between competing tips.

---

## Quick start

Rust 1.89 or newer is required. `rust-toolchain.toml` pins 1.93.0 for
development. About 4 GB of RAM is enough, which is why `.cargo/config.toml` caps
build parallelism at two jobs.

```bash
git clone https://github.com/zulfff/SubHost-demo.git
cd SubHost-demo
cargo build -p subhost-cli

# Write a genesis file that funds one account.
./target/debug/subhost init --chain-id 1 --data-dir ./data \
  --alloc 0x1111111111111111111111111111111111111111=1000000

# Run the node.
./target/debug/subhost node --listen 127.0.0.1:8545 --data-dir ./data
```

In another terminal:

```bash
./target/debug/subhost query chain --rpc-url http://127.0.0.1:8545
# chain_id  1
# height    0
```

### Two-wallet transfer

The scripts under `scripts/` run the whole flow in throwaway directories, so your
real `~/.subhost` is never touched:

```bash
cargo build -p subhost-cli
scripts/two-user-setup.sh          # creates Alice and Bob, writes genesis
# then, following the printed instructions:
./target/debug/subhost node --listen 127.0.0.1:8545 --data-dir /tmp/subhost-demo-data.XXXX
source /tmp/subhost-two-users-current.env && scripts/user1-alice.sh
source /tmp/subhost-two-users-current.env && scripts/user2-bob.sh
```

The environment file holds addresses only, never private keys. Amounts,
passwords, and the RPC URL can be overridden through environment variables, for
example `ALICE_AMOUNT=50 scripts/user1-alice.sh`.

### Web dashboard

```bash
cargo build -p subhost-cli -p subhost-explorer
scripts/start-web-demo.sh          # node + explorer on 127.0.0.1:3000
```

The explorer polls the node and shows the last 12 blocks with their transfers. It
also exposes `POST /api/transfer`, which decrypts a local wallet using a password
from the request body. That is only acceptable on a single-operator machine, so
the explorer refuses to bind to anything but a loopback address.

---

## CLI

```bash
subhost init --chain-id 1 --data-dir ./data \
  --alloc ADDRESS=BALANCE \
  --validator ADDRESS=PUBKEY_HEX:POWER

subhost node --listen 127.0.0.1:8545 --data-dir ./data \
  [--validator] [--metrics-addr 127.0.0.1:9090] [--max-connections 1000] \
  [--p2p-listen /ip4/0.0.0.0/tcp/30333 [--bootstrap-peer MULTIADDR] [--no-mdns] [--max-peers 50]]

subhost wallet new --password <password> [--name alice]
subhost wallet import --private-key <32-byte-hex> --password <password>
subhost wallet list
subhost wallet show <address>
subhost wallet export <address> --password <password>

subhost tx send --from <address> --to <address> --amount <units> \
  --password <password> [--nonce N] [--chain-id N] [--gas-price N]
subhost tx status <tx-hash>

subhost query balance <address>
subhost query nonce <address>
subhost query block [--height N] [--full]
subhost query chain
```

Wallets live in `$SUBHOST_HOME/wallets` (default `~/.subhost/wallets`). The CLI
queries `--nonce` and `--chain-id` from the node when you omit them. `--verbose`
and `--quiet` control log level; `RUST_LOG` overrides both.

`node --validator` is a validator node. It requires a genesis whose validator entries
each carry a proof of possession, and it reads `<data-dir>/producer.key`: one
hex-encoded canonical 32-byte BLS scalar with mode `0600`. The node derives the public
key from that file and uses the genesis validator whose `public_key` matches, then
signs every block it produces with it. A missing file, a wider mode, a scalar that
does not decode, and a key that belongs to no genesis validator all refuse to start.
A node without `--validator` still builds the registry from genesis and rejects peer
blocks that are not signed by one of those validators; only a genesis with no
validators at all accepts unsigned blocks.

`init --validator` does not generate the key or the proof yet, so a validator genesis
has to be written with real proofs, as `crates/subhost-node` does in its tests. That
CLI half is the remaining part of roadmap stage S1.4.

---

## JSON-RPC

Served on the address given to `node --listen`.

| Method | Notes |
|--------|-------|
| `eth_chainId` | Configured chain ID. |
| `net_version` | Chain ID as a decimal string. |
| `eth_blockNumber` | Height of the newest committed block. |
| `eth_gasPrice` | The mempool's minimum accepted gas price. |
| `eth_getBalance` | Real account balance. |
| `eth_getTransactionCount` | Next expected nonce. |
| `eth_sendTransaction` | Signed transfers only. See below. |
| `eth_getTransactionReceipt` | Real receipt, or `null`. |
| `eth_getBlockByNumber` | Accepts `latest`, `earliest`, `pending`, or a hex height. |
| `eth_getTransactionByHash` | Committed transactions only. |
| `eth_getProof` | Account inclusion proof against the latest state root. `storageProof` is always empty; absence cannot be proven. |

`eth_sendTransaction` **deviates from standard Ethereum**. The node never holds a
user key, so the caller must sign and supply two extra fields:

- `publicKey`: 32-byte hex ed25519 public key, which must hash to `from`
- `signature`: 64-byte hex ed25519 signature over the bincode encoding of the
  transaction with a cleared signature field

The node rejects the request unless the signature verifies, the chain ID matches,
the nonce is exactly the account's current nonce, and the balance covers
`value + gas_price * gas_limit`. Only transfers execute. Contract creation and
every other transaction type are refused explicitly.

A submission enters the pool. The producer then seals one block with every pending
transaction that is executable: nonce-contiguous per sender, and bounded by the
30M block gas limit. Senders are visited in address order, so selection is
deterministic. The ledger reaches disk before any in-memory view changes, so a
failed write leaves the pool, state, and receipts untouched. A call returns either
the hash of a committed block or an error. There is no pending limbo.

A block accepted from a peer takes a separate path. The node re-verifies the
parent, height, chain ID, gas accounting, every transaction signature, both Merkle
commitments, the full state transition, and the resulting state root. Receipts are
derived locally and never trusted from the wire. Block signatures are verified
when the node has a configured validator registry.

`eth_getProof` returns the leaf and sibling path for an existing account. The state
tree is a plain Merkle tree over sorted account leaves, so membership is provable
and non-membership is not. An unknown account returns an error rather than an
empty proof.

### Security posture

The JSON-RPC and metrics endpoints have **no authentication and no TLS**. Bind
them to loopback, or put an authenticating reverse proxy in front. The node logs a
warning when it binds a non-loopback address.

---

## Faucet

```bash
export SUBHOST_FAUCET_PASSWORD='...'
FAUCET_WALLET_PATH=./faucet-wallet.json \
SUBHOST_RPC_URL=http://127.0.0.1:8545 \
  ./target/debug/subhost-faucet
```

`POST /drip {"address":"0x..."}` signs a real transfer from the faucet wallet and
returns the hash the node accepted. `GET /status` reports the faucet address,
balance, and drip parameters. Rate limiting is per lowercased address, so letter
case cannot bypass the cooldown. A failed drip releases the slot, so a node outage
does not lock a caller out.

The faucet is unauthenticated by design. Put IP-level rate limiting and TLS in
front of it before exposing it.

---

## Benchmarks

```bash
cargo build -p subhost-bench --release
./target/release/subhost-bench --endpoint http://127.0.0.1:8545 -d 60 -c 100 tps
./target/release/subhost-bench --endpoint http://127.0.0.1:8545 -d 60 latency
./target/release/subhost-bench --endpoint http://127.0.0.1:8545 -d 300 load
```

The tool probes the endpoint before measuring and fails fast if it is unreachable.
Successes and failures are reported separately, and only successful requests enter
the latency histogram, so a timing run cannot be inflated by errors. These are
RPC-level measurements of a single node, not consensus throughput.

No performance figure is published here. Anything you may have seen elsewhere
about 50k TPS or sub-second finality describes a design target, not a measurement
of this code.

---

## Docker

```bash
docker compose up --build
```

This starts one node (`127.0.0.1:8545`, metrics on `127.0.0.1:9090`), the explorer
(`127.0.0.1:3000`), and Prometheus (`127.0.0.1:9091`). Every port is published on
loopback only. Containers run as an unprivileged user with a read-only root
filesystem, and the build uses `--locked`, so an image cannot be produced from an
unreviewed dependency set.

The stack is a single node and a single producer. `--p2p-listen` enables block
gossip between nodes that share a genesis file, and a peer block is adopted only
when it extends the local tip. That does not make the stack a testnet. Each node
still runs its own producer, so two producers receiving transactions will fork at
the same height and reject each other's blocks.

---

## Development

```bash
cargo fmt --all
cargo lint                        # clippy over the whole workspace, warnings denied
cargo test --workspace --all-features
cargo deny --all-features check   # advisories, licences, banned crates, sources
```

CI enforces formatting, clippy with `-D warnings` across all targets and features,
the full test suite, the 1.89 MSRV, a release build, `cargo deny`, and
warning-free rustdoc. The workspace forbids `unsafe_code` and denies `todo!`,
`unimplemented!`, and `dbg!`.

---

## Documentation

- [Threat model](docs/security/threat-model.md)
- [Manual testing guide](docs/manual-testing.md)
- [Brand mark](docs/brand.md)
- [Tokenomics design](docs/tokenomics.md): an undeployed economic proposal, not a live token
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

No third-party audit of this codebase has been completed.

---

## License

Apache License 2.0. See [LICENSE](LICENSE).

<p align="center">
  <sub>Built by the Subhost Labs team</sub>
</p>