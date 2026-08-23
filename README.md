<p align="center">
  <img src="assets/logo.svg" width="200" alt="Subhost Web3">
</p>

<h1 align="center">Subhost Web3</h1>

<p align="center">
  <strong>The Next-Generation Decentralized Cloud Infrastructure</strong>
</p>

<p align="center">
  <a href="https://subhost-web3.vercel.app/docs"><img src="https://img.shields.io/badge/Docs-Live-green.svg" alt="Documentation"></a>
  <a href="https://github.com/zulfff/SubHost-demo/actions/workflows/rust.yml"><img src="https://github.com/zulfff/SubHost-demo/actions/workflows/rust.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <a href="https://subhost-web3.vercel.app"><img src="https://img.shields.io/badge/Website-subhost--web3.vercel.app-6366f1.svg" alt="Website"></a>
</p>

---

## What is Subhost?

**Subhost Web3** is a Rust workspace exploring a high-performance, decentralized cloud infrastructure (consensus + distributed storage + edge compute). It is an active codebase, **not yet a production network** — the sections below state exactly what is implemented today.

### Current Status (honest, verified against the code)

| Area | Status |
|------|--------|
| **Core types** (`subhost-core`) | Real: `Hash`/`Address` (blake3), `BlockHeader`/`Block`/`Transaction`/`Receipt`, genesis config |
| **Crypto** (`subhost-crypto`) | Real: BLS12-381 sign/verify/aggregate + proof-of-possession, ChaCha20-Poly1305 AEAD, ed25519, X25519 key-exchange, scrypt/AES-GCM wallets |
| **Consensus** (`subhost-consensus`) | Scaffold: DAG + HotStuff structs, staking sets, quorum checks. **No full consensus loop / block production yet** |
| **Networking** (`subhost-network`) | Scaffold: libp2p swarm (gossipsub/kad/mdns/ping) wired for publish. **Message dispatch is minimal** |
| **Mempool** (`subhost-mempool`) | Real: per-sender nonce pool, gas-price ordering, capacity eviction, replace-by-nonce, dedupe |
| **State** (`subhost-state`) | Real (in-memory): accounts, balances, nonce/replay enforcement, transfer execution |
| **JSON-RPC** (`subhost-rpc`) | Real subset: `eth_chainId`, `eth_blockNumber`, `eth_getBalance` (reads state), `eth_sendTransaction` (inserts into mempool), `eth_gasPrice`, `net_version`; `eth_getTransactionReceipt` returns `null` (no confirmation backend yet) |
| **WASM / EVM / zk** | **Not implemented** — crates are placeholders |
| **IBC / Governance / Storage / P2P / Metrics / Faucet** | Partial / scaffolded; not production-ready |

The README's older marketing claims (50k TPS, 800ms finality, parallel EVM, zk-rollups, threshold-encrypted mempool, Dandelion++, MPT, erasure-coded store) describe the **design vision**, not verified measurements — no benchmark currently reproduces them.

---

## Target Architecture

The diagram below is the design direction, not the current runtime topology.
The status table above is the source of truth for what is wired today.

```
┌─────────────────────────────────────────────────────────────────┐
│                        APPLICATION LAYER                         │
├─────────────────────────────────────────────────────────────────┤
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────────┐  │
│  │  Smart      │ │  WASM       │ │  Cross-Chain            │  │
│  │  Contracts  │ │  Runtime    │ │  IBC Bridge             │  │
│  └─────────────┘ └─────────────┘ └─────────────────────────┘  │
├─────────────────────────────────────────────────────────────────┤
│                        EXECUTION LAYER                           │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │         Parallel EVM with Optimistic OCC                │    │
│  │         ┌─────────┐ ┌─────────┐ ┌─────────┐            │    │
│  │         │ Thread  │ │ Thread  │ │ Thread  │            │    │
│  │         │   1     │ │   2     │ │   N     │            │    │
│  │         └─────────┘ └─────────┘ └─────────┘            │    │
│  └─────────────────────────────────────────────────────────┘    │
├─────────────────────────────────────────────────────────────────┤
│                      CONSENSUS LAYER                           │
│  ┌─────────────────────────┐    ┌──────────────────────────┐  │
│  │   Narwhal DAG Mempool   │───▶│   HotStuff Finality      │  │
│  │   (Tx Dissemination)    │    │   (Deterministic Final)  │  │
│  └─────────────────────────┘    └──────────────────────────┘  │
├─────────────────────────────────────────────────────────────────┤
│                       NETWORK LAYER                            │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────────┐   │
│  │  Libp2p     │ │  Dandelion++│ │  Threshold Encryption   │   │
│  │  Transport  │ │  Privacy    │ │  MEV Resistance         │   │
│  └─────────────┘ └─────────────┘ └─────────────────────────┘   │
├─────────────────────────────────────────────────────────────────┤
│                       STORAGE LAYER                            │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  Merkle Patricia Trie + Erasure Coded Distributed Store │   │
│  │  State Rent + Auto-Expiry + Archival Incentives         │   │
│  └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

---

## Features

### CLI Tool

Build the two executable packages first:

```bash
cargo build -p subhost-cli -p subhost-bench -j 1
```

The examples below use the binaries produced by that debug build:

```bash
# Wallet management
./target/debug/subhost wallet new --password change-me --name mywallet
./target/debug/subhost wallet list
./target/debug/subhost wallet import --private-key <32-byte-hex-key> --password change-me

# Transactions
./target/debug/subhost tx send --from <address> --to <address> --amount 1000
./target/debug/subhost tx status <tx-hash>

# Queries
./target/debug/subhost query balance <address>
./target/debug/subhost query block --height 12345
./target/debug/subhost query validators

# Node operations
./target/debug/subhost init --chain-id 1 --data-dir ./data
./target/debug/subhost node --listen 127.0.0.1:8545
```

`wallet` writes encrypted files under `~/.subhost/wallets`. The transaction,
query, contract, `--validator`, and `--bootnodes` commands are not wired to a
remote node or a live consensus network yet; consult `--help` and the status
table before treating their output as an on-chain action.

### JSON-RPC API

- Ethereum-shaped JSON-RPC subset on the address passed to `node --listen`
  (the quick start below uses `127.0.0.1:8545`)
- Methods: `eth_chainId`, `eth_blockNumber`, `eth_getBalance`, `eth_sendTransaction`, `eth_gasPrice`, `net_version`
- `eth_getTransactionReceipt` returns `null` (no confirmation backend yet)
- `eth_sendTransaction` is **not** standard Ethereum: it requires an extra
  `"publicKey"` (32-byte hex ed25519 public key) and `"signature"` (64-byte hex
  ed25519 signature over the bincode-serialized unsigned transaction). Unsigned or
  wrongly-signed payloads are rejected. It also requires the configured chain ID
  and the account's exact current nonce.

### Wallet & Key Management
- AES-256-GCM encryption with **scrypt** key derivation _(implemented in `subhost-wallet`)_
- Note: "PBKDF2" and HD/BIP-39/BIP-44 derivation are **not implemented** — private keys are raw 32-byte values, encrypted at rest.

### Docker Compose Reference

Do not use `docker-compose.yml` as the quick start. It is an architecture
reference, not a working testnet: consensus/block production is not wired, the
current Dockerfile expects a `subhost-faucet` executable that the workspace does
not provide, and several published ports do not match the CLI listen address.

### Testnet Faucet

- `subhost-faucet` currently provides a library server, not a standalone binary
- When embedded and started, its default API address is `0.0.0.0:8080`
- Rate limited (1 request per day per address, case-insensitive)
- Request test tokens: `POST /drip { "address": "0x..." }`
- Caveat: currently returns a fabricated `tx_hash`; it does **not** actually credit
  a live chain state.

### Benchmark Tool
```bash
./target/debug/subhost-bench --endpoint http://127.0.0.1:8545 --duration-secs 60 --concurrency 100 tps
./target/debug/subhost-bench --endpoint http://127.0.0.1:8545 --duration-secs 60 latency
./target/debug/subhost-bench --endpoint http://127.0.0.1:8545 --duration-secs 300 load
```
The tool generates requests against a JSON-RPC endpoint. TPS currently counts
attempts even when a request fails and the tool does not validate response
payloads, so its output is a load-generator measurement, not proof of successful
transactions or consensus throughput.

### Block Explorer
- An `explorer/` project is scaffolded (Rust) but is **not part of the workspace** and not confirmed to run as "Web UI at port 3000".

### Prometheus Metrics

- `subhost-metrics` is a library crate with a `/metrics` exporter and defaults to
  `0.0.0.0:9090`; the main CLI does not start it
- The Grafana/Prometheus compose configuration is unverified

### IBC Bridge
- `subhost-ibc` implements in-memory channel/transfer/ack **bookkeeping only**.
- It does **not** perform real cross-chain transfers to Cosmos SDK chains yet.

---

### Prerequisites

- Rust 1.75+
- About 4 GB RAM is enough for the debug quick start when build parallelism is
  kept low
- Docker is optional and the included compose stack is not a working testnet

### Quick Start

```bash
git clone https://github.com/zulfff/SubHost-demo.git
cd SubHost-demo

# Build only the CLI with one compiler job (recommended on a 4 GB machine)
cargo build -p subhost-cli -j 1

# Initialize genesis state (writes ./data/genesis.json)
./target/debug/subhost init --chain-id 1 --data-dir ./data

# Run a node (currently: exposes the JSON-RPC endpoint; no P2P/consensus yet)
./target/debug/subhost node --listen 127.0.0.1:8545
```

In another terminal, verify the RPC server:

```bash
curl -s http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}'
```

Expected result: a JSON-RPC response whose `result` is `"0x1"`.

The generated genesis currently has no validators and fails the core validator
check; the running CLI node does not load that file yet. `init` is therefore a
file-generation demo, not network bootstrap. The `--validator`, `--bootnodes`,
and global `--config` flags are parsed but do not activate those systems yet.

For an optimized CLI binary, use the narrower release build below. The workspace
release profile uses fat LTO and one codegen unit, so it is slower and more
memory-intensive than the debug quick start:

```bash
cargo build -p subhost-cli --release -j 1
./target/release/subhost --help
```

### Docker

The repo ships a `Dockerfile` and `docker-compose.yml`, but they are not currently
an executable deployment path. See **Docker Compose Reference** above.

---

## Documentation

- **[Tokenomics](docs/tokenomics.md)** - undeployed SUB economic design
- **[Security](docs/security/threat-model.md)** - threat model and mitigation roadmap
- **[Hosted documentation](https://subhost-web3.vercel.app/docs)** - generated project documentation

### Documentation Website

Hosted documentation is available at:

- **Main site**: [subhost-web3.vercel.app](https://subhost-web3.vercel.app)
- **Documentation**: [Overview](https://subhost-web3.vercel.app/docs)
- **Guides**: [Getting Started](https://subhost-web3.vercel.app/docs/getting-started), [Features](https://subhost-web3.vercel.app/docs/features), [Smart Contracts](https://subhost-web3.vercel.app/docs/contracts), [Staking](https://subhost-web3.vercel.app/docs/staking)

The hosted site is maintained separately and can lag behind this repository.
For commands, package names, and implementation status, this README and the
checked-in source are authoritative.

---

## Security

### Audits

No completed third-party audit report is checked into this repository. Previously
listed auditor names and target dates were proposals only and are not evidence of
an engagement or completed review.

### Changelog & Pentest Plan

- **[CHANGELOG.md](CHANGELOG.md)** — exact list of every change made during the
  audit/hardening pass (kept accurate; no un-verified claims).
- **[PENTEST.md](PENTEST.md)** — a drop-in prompt for an LLM security audit with
  a strict fix → test → repeat loop and anti-hallucination rules.

### Reporting Security Issues

There is **no bug bounty program**. Report vulnerabilities privately via:
- Email: **security@subhost.xyz**
- GitHub: [Security Advisories](https://github.com/zulfff/SubHost-demo/security/advisories)

### Known Limitations (By Design)

All architectural limitations are documented with mitigation strategies. See [Threat Model](docs/security/threat-model.md).

---

## Tokenomics

### SUB Token

| Parameter | Value |
|-----------|-------|
| Total Supply | 1,000,000,000 SUB |
| Initial Circulation | 150,000,000 SUB (15%) |
| Inflation | 5% annually, decaying |
| Staking Rewards | 70% of inflation |
| Treasury | 20% of inflation |
| Burn Mechanism | 50% of fees burned |

See [detailed tokenomics](docs/tokenomics.md).

---

## Contributing

We welcome contributions! Please see our [Contributing Guide](CONTRIBUTING.md).

---

## License

Licensed under the Apache License, Version 2.0.

---

<p align="center">
  <sub>Built by the Subhost Labs team</sub>
</p>
