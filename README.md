<p align="center">
  <img src="https://raw.githubusercontent.com/zulfff/SubHost-demo/main/assets/logo.svg" width="200" alt="Subhost Web3">
</p>

<h1 align="center">Subhost Web3</h1>

<p align="center">
  <strong>The Next-Generation Decentralized Cloud Infrastructure</strong>
</p>

<p align="center">
  <a href="https://subhost.vercel.app"><img src="https://img.shields.io/badge/Docs-Live-green.svg" alt="Documentation"></a>
  <a href="https://github.com/zulfff/SubHost/actions/workflows/rust.yml"><img src="https://github.com/zulfff/SubHost-demo/actions/workflows/rust.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <a href="https://subhost.vercel.app"><img src="https://img.shields.io/badge/Website-subhost.vercel.app-6366f1.svg" alt="Website"></a>
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

## Architecture

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
```bash
# Wallet management
subhost wallet new --password <pass> --name mywallet
subhost wallet list
subhost wallet import --private-key <key> --password <pass>

# Transactions
subhost tx send --from <addr> --to <addr> --amount 1000
subhost tx status <tx_hash>

# Queries
subhost query balance <address>
subhost query block --height 12345
subhost query validators

# Node operations
subhost init --chain-id 1 --data-dir ./data
subhost node --validator --bootnodes /dns4/boot.subhost.io
```

### JSON-RPC API
- Ethereum-compatible API on port 8545
- Methods: `eth_chainId`, `eth_blockNumber`, `eth_getBalance`, `eth_sendTransaction`, `eth_gasPrice`, `net_version`
- `eth_getTransactionReceipt` returns `null` (no confirmation backend yet)
- `eth_sendTransaction` is **not** standard Ethereum: it requires an extra
  `"publicKey"` (32-byte hex ed25519 public key) and `"signature"` (64-byte hex
  ed25519 signature over the bincode-serialized unsigned transaction). Unsigned or
  wrongly-signed payloads are rejected with `Invalid signature`.
- `eth_sendTransaction` requires a `0x`-encoded 32-byte Ed25519 public key, a
  `0x`-encoded 64-byte signature over the canonical unsigned transaction, the
  configured chain ID, and the account's exact current nonce.

### Wallet & Key Management
- AES-256-GCM encryption with **scrypt** key derivation _(implemented in `subhost-wallet`)_
- Note: "PBKDF2" and HD/BIP-39/BIP-44 derivation are **not implemented** — private keys are raw 32-byte values, encrypted at rest.

### Docker Compose Testnet
```bash
docker-compose up -d
```
`docker-compose.yml` is included as a reference (validators, RPC, faucet, Prometheus,
Grafana) but the containers are **not end-to-end functional yet** — consensus/block
production is not wired, so this is not a usable testnet at the moment.

### Testnet Faucet
- Web API at port 8080
- Rate limited (1 request per day per address, case-insensitive)
- Request test tokens: `POST /drip { "address": "0x..." }`
- Caveat: currently returns a fabricated `tx_hash`; it does **not** actually credit
  a live chain state.

### Benchmark Tool
```bash
subhost-bench tps --endpoint http://localhost:8545 --duration 60 --concurrency 100
subhost-bench latency --endpoint http://localhost:8545 --duration 60
subhost-bench load --endpoint http://localhost:8545 --duration 300
```
Measures TPS/latency against a live JSON-RPC endpoint. It reports what the endpoint
actually serves; it does **not** benchmark consensus throughput.

### Block Explorer
- An `explorer/` project is scaffolded (Rust) but is **not part of the workspace** and not confirmed to run as "Web UI at port 3000".

### Prometheus Metrics
- `subhost-metrics` exposes a `/metrics` endpoint at port 9090 with request/latency/peers/height gauges.
- The grafana dashboard referenced in docker-compose is provided but unverified.

### IBC Bridge
- `subhost-ibc` implements in-memory channel/transfer/ack **bookkeeping only**.
- It does **not** perform real cross-chain transfers to Cosmos SDK chains yet.

---

### Prerequisites

- Rust 1.75+
- - Docker (optional, for running node)

### Installation

```bash
git clone https://github.com/zulfff/SubHost.git
cd SubHost

# Build optimized release
cargo build --release

# Initialize genesis state (writes ./data/genesis.json)
./target/release/subhost init --chain-id 1 --data-dir ./data

# Run a node (currently: exposes the JSON-RPC endpoint; no P2P/consensus yet)
./target/release/subhost node --listen 127.0.0.1:8545
```

> Note: the release binary is named `subhost` (from `crates/subhost-cli`). The
> `--validator --bootnodes` flags are parsed but do not yet start a real
> P2P/consensus network.

### Docker

The repo ships a `Dockerfile`; `docker-compose.yml` is provided as a reference but
the containers it describes are not yet end-to-end functional. Validate images
locally before relying on them.

---

## Documentation

- **[Tokenomics](docs/tokenomics.md)** - SUB token economics & incentives
- **[Security](docs/security/threat-model.md)** - Threat model & audit reports
- **[API Reference](https://subhost-web3.vercel.app/docs)** - Full API documentation

### Documentation Website

Complete documentation available at:

- **Main Site**: [https://subhost.vercel.app](https://subhost-web3.vercel.app)
- **Documentation**: [https://subhost.vercel.app/docs](https://subhost-web3.vercel.app/docs)
- **Getting Started**: [https://subhost.vercel.app/docs/getting-started](https://subhost-web3.vercel.app/docs/getting-started)
- **Features**: [https://subhost.vercel.app/docs/features](https://subhost-web3.vercel.app/docs/features)
- **Smart Contracts**: [https://subhost.vercel.app/docs/contracts](https://subhost-web3.vercel.app/docs/contracts)
- **Staking**: [https://subhost.vercel.app/docs/staking](https://subhost-web3.vercel.app/docs/staking)

---

## Security

### Audits

Planned / scheduled — **no audit has been completed yet.** Results will be linked here when/if they ship.

- **Trail of Bits** - Consensus & Cryptography (proposed Q1 2026)
- **OpenZeppelin** - Smart Contracts & IBC (proposed Q1 2026)
- **Least Authority** - Zero-Knowledge Circuits (proposed Q2 2026)

### Changelog & Pentest Plan

- **[CHANGELOG.md](CHANGELOG.md)** — exact list of every change made during the
  audit/hardening pass (kept accurate; no un-verified claims).
- **[PENTEST.md](PENTEST.md)** — a drop-in prompt for an LLM security audit with
  a strict fix → test → repeat loop and anti-hallucination rules.

### Reporting Security Issues

There is **no bug bounty program**. Report vulnerabilities privately via:
- Email: **security@subhost.xyz**
- GitHub: [Security Advisories](https://github.com/zulfff/SubHost/security/advisories)

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
