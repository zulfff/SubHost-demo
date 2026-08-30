<p align="center">
  <img src="assets/logo.svg" width="200" alt="Subhost Web3">
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

Subhost Web3 is a Rust workspace containing a working **single-node blockchain**:
an Ethereum-shaped JSON-RPC endpoint, signed transfers, deterministic block
production, and a crash-safe ledger on disk.

It is **not a distributed network**. There is no consensus loop and no block
propagation between peers. The table below is the authoritative statement of what
is implemented; nothing in this repository should be treated as production ready.

### Implementation status

| Crate | Status |
|-------|--------|
| `subhost-core` | **Complete.** Hash/Address (BLAKE3), block, transaction, receipt, genesis document, canonical encoding. |
| `subhost-crypto` | **Complete.** BLS12-381 sign/verify/aggregate with proof of possession, ed25519, X25519 (contributory-checked), ChaCha20-Poly1305. |
| `subhost-wallet` | **Complete.** scrypt + AES-256-GCM keystore, atomic `0600` writes, address verified against the decrypted key. |
| `subhost-state` | **Complete.** Balances, nonce ordering, replay rejection, checked arithmetic, deterministic state root. |
| `subhost-mempool` | **Complete.** Per-sender nonce pool, replace-by-fee, capacity eviction, deterministic proposal order. |
| `subhost-storage` | **Complete.** Versioned, checksummed, atomically written ledger; every block and receipt commitment is replayed on load. |
| `subhost-rpc` | **Working subset.** 10 JSON-RPC methods, mandatory signature verification, one transaction per block, persist-before-commit. |
| `subhost-node` | **Complete.** Genesis load, ledger restore, RPC and metrics wiring, graceful shutdown. |
| `subhost-telemetry` | **Complete.** One `RUST_LOG`-aware subscriber, optional JSON output. |
| `subhost-metrics` | **Complete.** Prometheus registry and `/metrics` + `/health` exporter. |
| `subhost-consensus` | **Verifiable primitives only.** DAG admission and quorum support, BLS-verified quorum certificates, staking and slashing. **No consensus loop, no block production.** |
| `subhost-network` | **Transport only.** libp2p gossip publish and receive, connection limits. No Dandelion++ relay, no peer scoring. |
| `subhost-ibc` | **Local state machine only.** Channel handshake, sequencing, timeouts, replay rejection. **No light-client or commitment proof verification.** |
| `subhost-faucet` | **Complete.** Signs and submits real transfers from an encrypted wallet, per-address cooldown. |
| `subhost-cli` | **Complete.** Every command performs a real operation. |
| `subhost-explorer` | **Complete.** Read-only dashboard plus a loopback-only demo signing endpoint. |
| `subhost-bench` | **Complete.** Load and latency measurement, reporting successes and failures separately. |

Not implemented anywhere in this repository: EVM execution, WASM contracts, zk
proofs, on-chain governance, a Merkle Patricia Trie, erasure-coded storage, or a
threshold-encrypted mempool.

---

## Quick start

Requires Rust 1.89 or newer (`rust-toolchain.toml` pins 1.93.0 for development).
About 4 GB of RAM is enough; `.cargo/config.toml` caps build parallelism at two
jobs for that reason.

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

The scripts under `scripts/` do the whole flow with throwaway directories, so
your real `~/.subhost` is never touched:

```bash
cargo build -p subhost-cli
scripts/two-user-setup.sh          # creates Alice and Bob, writes genesis
# then, following the printed instructions:
./target/debug/subhost node --listen 127.0.0.1:8545 --data-dir /tmp/subhost-demo-data.XXXX
source /tmp/subhost-two-users-current.env && scripts/user1-alice.sh
source /tmp/subhost-two-users-current.env && scripts/user2-bob.sh
```

The environment file holds addresses only, never private keys. Amounts,
passwords, and the RPC URL are overridable through environment variables, for
example `ALICE_AMOUNT=50 scripts/user1-alice.sh`.

### Web dashboard

```bash
cargo build -p subhost-cli -p subhost-explorer
scripts/start-web-demo.sh          # node + explorer on 127.0.0.1:3000
```

The explorer polls the node and shows the last 12 blocks with their transfers. It
also exposes `POST /api/transfer`, which decrypts a local wallet using a password
from the request body. That is only acceptable on a single-operator machine, so
the explorer **refuses to bind to anything but a loopback address**.

---

## CLI

```bash
subhost init --chain-id 1 --data-dir ./data \
  --alloc ADDRESS=BALANCE \
  --validator ADDRESS=PUBKEY_HEX:POWER

subhost node --listen 127.0.0.1:8545 --data-dir ./data \
  [--validator] [--metrics-addr 127.0.0.1:9090] [--max-connections 1000]

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

Wallets live in `$SUBHOST_HOME/wallets` (default `~/.subhost/wallets`). `--nonce`
and `--chain-id` are queried from the node when omitted. `--verbose` and
`--quiet` control log level; `RUST_LOG` overrides both.

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
| `eth_sendTransaction` | Signed transfers only — see below. |
| `eth_getTransactionReceipt` | Real receipt, or `null`. |
| `eth_getBlockByNumber` | Accepts `latest`, `earliest`, `pending`, or a hex height. |
| `eth_getTransactionByHash` | Committed transactions only. |

`eth_sendTransaction` **deviates from standard Ethereum**. The node never holds a
user key, so the caller must sign and supply:

- `publicKey` — 32-byte hex ed25519 public key, which must hash to `from`
- `signature` — 64-byte hex ed25519 signature over the bincode encoding of the
  transaction with a cleared signature field

The request is rejected unless the signature verifies, the chain ID matches, the
nonce is exactly the account's current nonce, and the balance covers
`value + gas_price * gas_limit`. Only transfers execute; contract creation and
every other transaction type are refused explicitly.

Each accepted transaction is executed and sealed into its own block, and the
ledger is written to disk before the in-memory state is updated. A call therefore
returns either the hash of a committed block or an error — there is no pending
limbo.

### Security posture

The JSON-RPC and metrics endpoints have **no authentication and no TLS**. Bind
them to loopback, or put an authenticating reverse proxy in front. The node logs
a warning when it binds a non-loopback address.

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
case cannot bypass the cooldown, and a failed drip releases the slot so a node
outage does not lock a caller out.

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

The tool probes the endpoint before measuring and fails fast if it is
unreachable. Successes and failures are reported separately, and only successful
requests enter the latency histogram, so a timing run cannot be inflated by
errors. These are RPC-level measurements of a single node, not consensus
throughput.

No performance figure is published here. Anything you may have seen elsewhere
about 50k TPS or sub-second finality describes a design target, not a measurement
of this code.

---

## Docker

```bash
docker compose up --build
```

Starts one node (`127.0.0.1:8545`, metrics on `127.0.0.1:9090`), the explorer
(`127.0.0.1:3000`), and Prometheus (`127.0.0.1:9091`). Every port is published on
loopback only, containers run as an unprivileged user with a read-only root
filesystem, and the build uses `--locked` so an image cannot be produced from an
unreviewed dependency set.

The stack is a single node. Because block production is single-node, running
several `subhost node` services would produce independent chains, not one
network.

---

## Development

```bash
cargo fmt --all
cargo lint                        # clippy over the whole workspace, warnings denied
cargo test --workspace --all-features
cargo deny --all-features check   # advisories, licences, banned crates, sources
```

CI enforces formatting, clippy with `-D warnings` across all targets and
features, the full test suite, the 1.89 MSRV, a release build, `cargo deny`, and
warning-free rustdoc. The workspace forbids `unsafe_code` and denies `todo!`,
`unimplemented!`, and `dbg!`.

---

## Documentation

- [Threat model](docs/security/threat-model.md)
- [Manual testing guide](docs/manual-testing.md)
- [Tokenomics design](docs/tokenomics.md) — an undeployed economic proposal, not a live token
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
