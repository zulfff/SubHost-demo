# Threat Model

This document outlines the security assumptions, potential threats, and mitigations for the Subhost protocol. It is a living document that evolves as the protocol matures and new threats are identified.

> **Implementation status.** A threat model describes the surface we intend to
> defend, so most mitigations below are a roadmap rather than a live control. Read
> them as intent, not as assurance.
>
> **Implemented today:** BLS12-381 and ed25519 signatures with mandatory
> proof-of-possession for validator registration; ChaCha20-Poly1305 AEAD and
> contributory-checked X25519; scrypt + AES-256-GCM wallets; a bounded mempool with
> replace-by-fee and deterministic ordering; account rules with nonce ordering,
> replay rejection, and checked arithmetic; a checksummed, atomically written
> ledger whose every block and receipt commitment is replayed on load; a JSON-RPC
> subset that requires a valid signature bound to the sender; BLS-verified quorum
> certificates; stake-deducting slashing; and a libp2p gossip transport.
>
> **Not implemented:** a consensus loop or block propagation, an encrypted mempool,
> Dandelion++ routing, threshold encryption, on-chain governance, EVM or WASM
> execution, zk proofs, a Merkle Patricia Trie, oracle or MEV defences, an HSM
> program, and any authentication on the JSON-RPC, metrics, or faucet surfaces.
> There is no bug bounty.
>
> `SECURITY.md` is the authoritative, code-verified statement of the current
> posture. Where this document and `SECURITY.md` disagree, `SECURITY.md` wins.

## Security Objectives

Subhost aims to provide the following security guarantees:

- **Liveness**: The network must continue processing transactions even if some validators fail or act maliciously (up to 1/3 of stake)
- **Safety**: Validators cannot finalize conflicting transactions (no double-spends)
- **Data Availability**: All transaction data must be available for verification
- **Censorship Resistance**: No single entity can prevent valid transactions from being included

## Trust Assumptions

### What We Assume

- At least 2/3 of staked tokens are controlled by honest validators
- Network partitions are temporary and eventually resolve
- Cryptographic primitives (BLS12-381, SHA3, Blake3) remain secure
- Economic incentives align validator behavior with protocol health

### What We Do Not Assume

- Validators will not attempt to maximize their own profit
- Nodes will not collude if profitable
- External dependencies (RPCs, price oracles) are trustworthy
- Users will keep their private keys secure

## Threat Categories

### Consensus Attacks

#### Double Signing
A validator signs two conflicting blocks at the same height.

**Impact**: Safety violation, potential chain split

**Mitigation**:
- Slashing of 100% of validator stake
- Automatic validator removal from set
- Evidence-based slashing prevents false accusations
- Validator software enforces non-conflicting signatures

**Detection**: Anyone can submit evidence of double signing

#### Long Range Attacks
An attacker buys old private keys and attempts to rewrite history from an old checkpoint.

**Impact**: Chain reorganization, safety violation

**Mitigation**:
- Weak subjectivity: social consensus on hardcoded checkpoints
- Unbonding period makes key purchases expensive
- Validator set rotation limits old key utility
- Light clients refuse deep reorganizations

**Detection**: Checkpoint monitoring, social consensus alerts

#### Nothing at Stake
Validators stake on multiple forks simultaneously without cost.

**Impact**: Consensus failure, inability to finalize

**Mitigation**:
- Slashing conditions for double voting
- Inactivity leaks for non-participating validators
- Bonding period makes stakes non-transferable

**Detection**: Vote aggregation reveals equivocation

### Network Layer Attacks

#### Eclipse Attacks
An attacker isolates a validator from honest peers, feeding them false information.

**Impact**: Validator makes decisions based on incomplete/wrong data

**Mitigation**:
- Minimum peer requirements (min 10 peers)
- Random peer selection from DHT
- Connection to bootstrap nodes with hardcoded IPs
- Peer diversity scoring

**Detection**: Peer count monitoring, block height comparisons

#### DDoS Attacks
Overwhelming validators or seed nodes with traffic.

**Impact**: Validator downtime, missed blocks, network degradation

**Mitigation**:
- Rate limiting on all public endpoints
- Connection quotas per IP
- Distributed validator architecture
- CDN protection for RPC endpoints

**Detection**: Traffic pattern analysis, latency spikes

#### Sybil Attacks
Creating many fake identities to gain disproportionate influence.

**Impact**: Vote manipulation, eclipse attacks

**Mitigation**:
- Proof of stake: influence requires capital, not just identities
- Minimum stake requirements (10,000 SUB)
- Validator caps per entity

**Detection**: On-chain stake analysis

### Economic Attacks

#### MEV Extraction
Validators reorder, insert, or censor transactions for profit.

**Impact**: User harm, fairness violations

**Mitigation**:
- Encrypted mempool (Dandelion++ routing)
- Threshold encryption for transaction ordering
- Commit-reveal schemes for block proposals
- Protocol-level MEV capture and redistribution

**Detection**: Block content analysis, transaction timing studies

#### Flash Loan Attacks
Borrowing large amounts to manipulate protocol state atomically.

**Impact**: Price oracle manipulation, arbitrage exploits

**Mitigation**:
- TWAP oracles (time-weighted average prices)
- Multiple oracle sources with median aggregation
- Circuit breakers for extreme price moves
- Atomic transaction limits

**Detection**: Oracle deviation monitoring, profit analysis

### Smart Contract Vulnerabilities

#### Reentrancy
Contracts calling external contracts before updating state.

**Impact**: Fund draining, state corruption

**Mitigation**:
- Checks-effects-interactions pattern enforcement
- Reentrancy guards in standard libraries
- Static analysis in deployment pipeline
- Formal verification of critical contracts

**Detection**: Bytecode analysis, transaction simulation

#### Integer Overflow/Underflow
Arithmetic operations exceeding type bounds.

**Impact**: Unexpected behavior, potential fund loss

**Mitigation**:
- Safe math libraries (checked arithmetic)
- Compiler warnings and errors
- Audits focusing on arithmetic sections

**Detection**: Fuzzing, symbolic execution

### Infrastructure Attacks

#### Key Compromise
Validator or user private keys stolen.

**Impact**: Unauthorized transactions, slashing evasion

**Mitigation**:
- Hardware security modules (HSMs) recommended
- Key rotation procedures
- Multi-sig for high-value operations
- Threshold signing for validators

**Detection**: Unusual transaction patterns, key rotation events

#### Supply Chain Attacks
Malicious code in dependencies or build process.

**Impact**: Backdoors, compromised binaries

**Mitigation**:
- Minimal dependency tree
- Reproducible builds
- Dependency auditing (cargo audit)
- Binary verification via checksums

**Detection**: Build hash mismatches, CVE monitoring

## Risk Assessment Matrix

| Threat | Likelihood | Impact | Priority |
|--------|-----------|--------|----------|
| Double signing | Low | Critical | High |
| Long range | Low | Critical | High |
| Eclipse attack | Medium | High | High |
| DDoS | Medium | Medium | Medium |
| MEV extraction | High | Medium | High |
| Flash loan | Medium | High | High |
| Reentrancy | Low | High | Medium |
| Key compromise | Medium | Critical | High |

## Planned Monitoring and Response

The repository does not deploy a production monitoring or incident-response
system. The items below are operational goals for a future network.

### What We Monitor

- Validator uptime and participation rates
- Peer counts and network health
- Oracle price deviations
- Mempool transaction patterns
- Block finality times
- Token flow anomalies

### Response Procedures

**Severity 1 (Critical)**: Active exploit in progress
- Emergency halt if possible
- Multi-sig council intervention
- Community notification
- Post-mortem and fix

**Severity 2 (High)**: Imminent threat detected
- Enhanced monitoring
- Preventive measures activated
- Validator coordination

**Severity 3 (Medium)**: Potential vulnerability
- Scheduled patching
- Additional monitoring
- Documentation updates

## Reporting Security Issues

There is **no bug bounty program** and no reward pool. If you find a suspected
vulnerability, report it privately to `security@subhost.xyz` — include the affected
commit, exact file and line, impact, and reproduction steps. Please do not open a
public issue for an undisclosed vulnerability.

## Audit Status

No completed third-party audit report is checked into this repository. Do not
interpret previously proposed auditor names or dates as evidence of an engagement
or completed review.

---

This document is maintained by the Subhost security team. Questions or concerns should be directed to security@subhost.xyz.
