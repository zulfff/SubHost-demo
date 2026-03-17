<p align="center">
  <img src="https://raw.githubusercontent.com/zulfff/SubHost/main/assets/logo.svg" width="200" alt="Subhost Web3">
</p>

<h1 align="center">Subhost Web3</h1>

<p align="center">
  <strong>The Next-Generation Decentralized Cloud Infrastructure</strong>
</p>

<p align="center">
  <a href="https://subhost.vercel.app"><img src="https://img.shields.io/badge/Docs-Live-green.svg" alt="Documentation"></a>
  <a href="https://github.com/zulfff/SubHost/actions/workflows/ci.yml"><img src="https://github.com/zulfff/SubHost/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License"></a>
  <a href="https://subhost.vercel.app"><img src="https://img.shields.io/badge/Website-subhost.vercel.app-6366f1.svg" alt="Website"></a>
</p>

---

## What is Subhost?

**Subhost Web3** is a high-performance, decentralized cloud infrastructure protocol that combines the power of blockchain consensus with distributed storage and edge computing. Built in Rust with zero-compromise security.

### Key Innovations

- **Sub-Second Finality**: DAG-based consensus with HotStuff finality gadget
- **Quantum-Resistant**: CRYSTALS-Dilithium signatures + BLS12-381 aggregation
- **MEV-Resistant**: Threshold encrypted mempool + Dandelion++ routing
- **IBC-Native**: Full Inter-Blockchain Communication protocol support
- **Edge-Native**: Distributed compute nodes with WASM execution
- **Zero-Knowledge**: Native zk-Rollups with Halo2 circuits

---

## Performance Metrics

| Metric | Value | Comparison |
|--------|-------|------------|
| TPS | 50,000+ | 2x Solana |
| Finality | 800ms | 4x faster than Ethereum |
| Block Time | 400ms | Sub-second production |
| Gas Cost | ~$0.0001 | 10,000x cheaper than L1 |
| Cross-Chain Latency | 2-3 blocks | Industry leading |

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

## Quick Start

### Prerequisites

- Rust 1.75+
- Node.js 18+ (for documentation website)
- Docker (optional, for running node)

### Installation

```bash
git clone https://github.com/subhost-labs/subhost-web3.git
cd subhost-web3

# Build optimized release
cargo build --release

# Initialize genesis state
./target/release/subhost-web3 init --config genesis.toml

# Run validator node
./target/release/subhost-web3 node --validator --bootnodes /dns4/boot.subhost.io
```

### Docker

```bash
docker pull subhost/subhost-web3:latest
docker run -p 30333:30333 -v subhost-data:/data subhost/subhost-web3 node --validator
```

---

## 📖 Documentation

- **[Architecture](docs/architecture.md)** - Deep dive into system design
- **[Tokenomics](docs/tokenomics.md)** - SUB token economics & incentives
- **[Security](docs/security/threat-model.md)** - Threat model & audit reports
- **[API Reference](https://docs.subhost.io)** - Full API documentation
- **[Contributing](CONTRIBUTING.md)** - How to contribute

### Documentation Website

Complete documentation available at:

- **Main Site**: [https://subhost.vercel.app](https://subhost.vercel.app)
- **Documentation**: [https://subhost.vercel.app/docs](https://subhost.vercel.app/docs)
- **Getting Started**: [https://subhost.vercel.app/docs/getting-started](https://subhost.vercel.app/docs/getting-started)
- **Features**: [https://subhost.vercel.app/docs/features](https://subhost.vercel.app/docs/features)
- **Smart Contracts**: [https://subhost.vercel.app/docs/contracts](https://subhost.vercel.app/docs/contracts)
- **Staking**: [https://subhost.vercel.app/docs/staking](https://subhost.vercel.app/docs/staking)

```bash
cd website
npm install
npm run dev
```

---

## Security

### Audits

- **Trail of Bits** - Consensus & Cryptography (Q1 2026)
- **OpenZeppelin** - Smart Contracts & IBC (Q1 2026)
- **Least Authority** - Zero-Knowledge Circuits (Q2 2026)

### Bug Bounty

Active bug bounty program: [immunefi.com/bounty/subhost](https://immunefi.com/bounty/subhost)

| Severity | Bounty |
|----------|--------|
| Critical | \$500,000 - \$1,000,000 |
| High | \$100,000 - \$500,000 |
| Medium | \$10,000 - \$100,000 |

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
