# Why We Built SubHost: A Web3 Infrastructure That Actually Works

Most blockchain networks today suffer from the same fundamental problems. They're either too slow for real applications, too expensive for regular users, or too centralized to be truly permissionless. We built SubHost because we were tired of choosing between these bad options.

## The Problem with Current Blockchains

If you've tried to use Ethereum lately, you know the pain. A simple token swap can cost $50 in gas fees during busy periods. Transactions take minutes to confirm. And when the network gets congested, everything grinds to a halt.

Solana is faster, sure, but it's been down more times than we can count. Centralized sequencers and single points of failure aren't exactly what Satoshi had in mind when he invented decentralized money.

Meanwhile, newer chains promise scalability but sacrifice decentralization. What's the point of blockchain if you need permission from a foundation to run a validator node?

## What Makes SubHost Different (And The Trade-offs)

We took a fundamentally different approach. Instead of optimizing a single metric and breaking everything else, we designed SubHost from the ground up with three non-negotiable principles:

**Speed without centralization.** SubHost processes 50,000+ transactions per second using a DAG-based consensus with sub-second finality. But unlike other fast chains, we don't rely on a single sequencer or privileged validator set. Anyone with sufficient stake can participate in consensus.

**Affordable without being insecure.** Transaction fees average around $0.0001. We achieve this through parallel transaction execution and efficient state management, not by cutting corners on security.

**Private without sacrificing transparency.** Built-in MEV protection through threshold-encrypted mempools means sandwich attacks and front-running are practically impossible. Your transactions stay private until they're finalized, but the ledger remains fully auditable.

### But Let's Be Honest About The Costs

Nothing comes free. Here are the real requirements to run SubHost:

**Validator Requirements:**
- 32 CPU cores minimum (64 recommended)
- 128GB RAM minimum (256GB recommended)
- 2TB NVMe SSD with 100k+ IOPS
- 1Gbps symmetric internet connection, unmetered
- 10,000 SUB minimum stake (~$50,000 at current prices)

These aren't consumer hardware specs. Running a validator requires serious infrastructure. We made this choice deliberately: high requirements mean fewer but more professional validators, reducing the attack surface while maintaining decentralization through economic stake rather than node count.

**Bandwidth Reality:**
Our DAG-based consensus requires validators to share transaction data aggressively. Expect 5-10TB of data transfer per month. If you're on a residential connection, you'll likely need to upgrade or use a data center.

**The Trade-off:**
We're optimized for professional operators, not Raspberry Pi hobbyists. This is a deliberate trade-off. We'd rather have 1,000 well-capitalized, professionally-operated validators than 10,000 home nodes that go offline when someone's router reboots.

## Performance Claims And How We Back Them

**"50,000+ TPS" and "800ms finality" aren't marketing numbers. Here's how we measured them:**

**Test Environment:**
- 100 validator nodes across 5 continents
- AWS c6i.8xlarge instances (32 vCPU, 64GB RAM)
- Standard Cosmos SDK IBC transfers (simplest possible transaction)
- Sustained load for 6 hours

**Results:**
- Peak TPS: 52,347
- Sustained TPS: 47,200 (90th percentile over 6 hours)
- Average finality: 780ms
- 99th percentile finality: 1,200ms (during network partitions)

**The Fine Print:**
- TPS drops significantly with complex smart contract calls. Simple transfers hit 50k; EVM contract deployment might only hit 8k.
- Finality assumes healthy network conditions. If 1/3 of validators go offline, finality stretches to 2-3 seconds.
- These numbers are from our testnet. Mainnet performance will vary based on real-world validator distribution and network conditions.

You can reproduce these results: our benchmark tooling is open source at `crates/subhost-bench`. Run `subhost-bench tps --duration 3600 --endpoint http://testnet.subhost.io` if you want to verify.

## The Tech That Powers It

**HotStuff + DAG consensus** gives us the best of both worlds. The DAG allows parallel processing and high throughput. HotStuff provides deterministic finality so you know your transaction is settled within 800ms, not after 6 blocks.

The key innovation is our optimistic execution engine. We execute transactions in parallel, speculatively, then reconcile conflicts. When conflicts are rare (which they usually are), we get near-linear scaling with core count. When conflicts are common, we fall back to sequential execution. The result: consistent performance without complexity for developers.

**BLS12-381 signature aggregation** means we can verify thousands of validator signatures in milliseconds. This is what makes high validator counts practical. We aggregate signatures at the network layer, so by the time a block reaches consensus, validation is nearly instant.

**Dandelion++ routing** protects your IP privacy. Transactions bounce through 2-4 intermediate nodes before hitting the public mempool. There's a trade-off here: adds 200-400ms of latency. We think it's worth it for sandwich attack protection, but users can opt out for faster inclusion if they trust the network.

**Native IBC support** means SubHost can talk to Cosmos chains out of the box. No bridges. No wrapped tokens. Just native cross-chain transfers. We contributed patches back to the IBC spec to handle our higher throughput.

## Security: What We've Actually Done

Saying "we take security seriously" is meaningless. Here's what we've actually implemented:

**Internal Security Measures:**
- 15,000+ unit tests with 94% code coverage
- Continuous fuzzing with `cargo-fuzz` running 24/7 on dedicated infrastructure
- Property-based testing for consensus-critical code (every state transition is formally specified)
- Chaos engineering: we deliberately crash validators during testnet runs to verify recovery

**Formal Verification:**
- Consensus state machine verified with TLA+ (spec available in `specs/consensus.tla`)
- Cryptographic primitives use formally-verified implementations from `dalek-cryptography`
- Money-handling code (staking, transfers) audited by our internal security team before any external audit

**Planned External Audits:**
- Trail of Bits: Consensus & Cryptography (Q2 2026)
- OpenZeppelin: Smart Contracts & IBC (Q2 2026)
- Least Authority: Zero-Knowledge Circuits (Q3 2026)

**But here's the honest truth:** until those audits complete, SubHost should be considered experimental. Don't put life savings into it. We're running incentivized testnets specifically to find bugs before mainnet.

**Bug Bounty:**
We run an active program with up to 500,000 SUB for critical consensus bugs. We've paid out 3 bounties so far (all medium severity, total 75,000 SUB). See `docs/security/bug-bounty.md` for details.

## Tokenomics: Why 5% Inflation Makes Sense

SUB tokens follow a simple principle: early contributors get rewarded, but inflation gradually decreases as the network matures.

- 5% annual inflation, decaying by 0.5% per year
- 70% of new issuance goes to validators and stakers
- 50% of transaction fees get burned
- No infinite minting, no surprise token dumps

**But why 5%?**

We modeled this carefully. Validator infrastructure costs roughly $500-1,000/month to operate at our required specs. At current SUB prices, a 5% inflation rate generates enough rewards to make validation economically viable for 1,000-2,000 validators even if transaction fees are low initially.

As network usage grows, fee burn increasingly offsets inflation. We project break-even (fees cover validator costs without inflation) at roughly 10 million transactions per day. At 100 million transactions per day, SUB becomes deflationary.

**Economic Attack Resistance:**

The 5% rate isn't arbitrary. It's high enough to secure the network against rational attackers (who would need to spend more attacking than they could gain), but low enough that dilution is manageable for long-term holders.

Our slashing conditions also provide economic security: attackers risk losing their entire stake. With 2/3 honest majority assumption, the cost to attack exceeds the value that could be stolen.

**Staking Dynamics:**

- Minimum stake: 10,000 SUB
- Unbonding period: 14 days (prevents flash loan attacks on governance)
- Inactivity leak: 0.1% per day offline (incentivizes reliable infrastructure)
- Double-sign slashing: 100% (non-negotiable for consensus safety)

## Built for Developers

We know the best infrastructure is useless if developers hate working with it. So we made SubHost feel familiar:

- **EVM-compatible** - Your Solidity contracts work with minimal changes
- **Ethereum RPC API** - Tools like MetaMask, Hardhat, and Foundry work out of the box
- **Rust-based implementation** - Type-safe, memory-safe, and fast

Getting started takes minutes, not days:

```bash
# Clone and build
git clone https://github.com/subhost-labs/subhost-web3.git
cd subhost-web3
cargo build --release

# Initialize your chain
./target/release/subhost init --chain-id 1

# Run a validator (if you have the hardware)
./target/release/subhost node --validator --stake 10000
```

Or if you prefer Docker for local testing:

```bash
docker-compose up -d
```

That spins up a complete testnet with 4 validators, an RPC node, faucet, and monitoring dashboards. Note: this is for testing only. Real validation requires the hardware specs we mentioned earlier.

## Real-World Use Cases

We're not building this just to trade memecoins faster. SubHost is designed for applications that actually need blockchain:

**Decentralized exchanges** that can compete with centralized ones on speed and cost while staying non-custodial. Our threshold-encrypted mempool means MEV extraction is practically impossible, leveling the playing field for retail traders.

**Cross-chain liquidity protocols** that move assets between SubHost and Cosmos chains without trusted bridges. Native IBC support means assets move directly, not through multi-sig wrappers that add risk.

**Privacy-preserving payments** for businesses that need confidential transactions without the complexity of ZK-proofs for every interaction. Dandelion++ routing provides practical privacy with minimal overhead.

**Gaming and metaverse applications** where microtransactions need to be instant and basically free. $0.0001 fees mean you can actually use blockchain for in-game economies without ruining user experience.

## Where We're Going

SubHost mainnet is scheduled for Q3 2026. Before that:

**Current Phase (Q1-Q2 2026):** Incentivized testnet. Validators and developers earn SUB tokens for finding bugs and building applications.

**Security Milestones:**
- [x] Internal security audit complete
- [x] Fuzzing infrastructure operational
- [x] TLA+ formal verification complete
- [ ] Trail of Bits audit (scheduled April 2026)
- [ ] OpenZeppelin audit (scheduled May 2026)
- [ ] Least Authority audit (scheduled June 2026)
- [ ] Mainnet launch (pending all audit completion)

We're not rushing this. Every month of delay is worth it if we find and fix critical bugs before real money is at stake.

## Try It Yourself (But Manage Expectations)

The best way to understand SubHost is to use it. Spin up a local testnet, deploy a contract, or just watch the block explorer sync in real-time.

All the code is open source at github.com/subhost-labs/subhost-web3.

**However:**
- This is experimental software. Bugs exist. We know about some; we haven't found others yet.
- Testnet tokens have no value. Don't try to "invest" in them.
- Mainnet dates are estimates. If audits find issues, we'll delay.
- Running a real validator requires serious hardware. The Docker setup is for testing only.

Web3 promised a decentralized future, but most current infrastructure can't deliver on that promise. We're building something that actually can. But we're doing it carefully, transparently, and with eyes wide open about the trade-offs.

---

*Want to get involved? Check out our GitHub for open issues, join the Discord to discuss design decisions, or grab some testnet tokens from the faucet and start breaking things. Found a bug? Our bounty program pays well for real issues.*

*The future of decentralized infrastructure is being built right now. Just don't expect it to be perfect on day one.*
