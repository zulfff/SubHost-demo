# Threat model

This document records the security assumptions, threats, and mitigations for the
Subhost protocol. It changes as the protocol matures and new threats appear.

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
> subset that requires a valid signature bound to the sender; Merkle state and
> receipt commitments with per-account inclusion proofs; multi-transaction block
> production and full re-validation of peer blocks; BLS-verified quorum
> certificates with separate `n - f` Prepare/Commit quorums, genesis-bound signed
> leaders, durable prepared locks and authenticated timeout rotation;
> stake-deducting slashing; a
> libp2p gossip transport that the node uses for live block propagation; and an IBC
> light client that verifies a single ed25519 signer's headers and Merkle
> commitment proofs.
>
> **Not implemented:** authenticated historical QC replay, native catch-up,
> finalized-fork incident handling,
> block sync/backfill, peer scoring, an encrypted mempool, Dandelion++ routing,
> threshold encryption, on-chain governance, EVM or WASM execution, zk proofs, a
> Merkle Patricia Trie, non-membership proofs, validator-set rotation or multisig
> light clients, oracle or MEV defences, an HSM program, per-IP rate limiting,
> in-process TLS, and authentication on the metrics or faucet surfaces. JSON-RPC
> authentication exists but is opt-in and defaults to off. There is no bug bounty.
>
> `SECURITY.md` is the authoritative, code-verified statement of the current
> posture. Where this document and `SECURITY.md` disagree, `SECURITY.md` wins.

## Security objectives

Subhost aims to provide the following security guarantees:

- **Liveness**: The network must continue processing transactions under eventual bounded delivery and
  at most `f = (n - 1) / 3` faulty distinct validator identities
- **Safety**: Validators cannot finalize conflicting transactions (no
  double-spends)
- **Data Availability**: All transaction data must be available for verification
- **Censorship Resistance**: No single entity can prevent valid transactions from
  being included

## Trust assumptions

### What we assume

- At most `f = (n - 1) / 3` registered identities are Byzantine; distinct
  standard-suite BLS keys have valid proofs and honest signers preserve their
  newest signing journal across restart. Genesis power is not quorum weight
- The IBC light client's genesis header and its single ed25519 key were obtained
  out of band; if that key is replaced, the bridge trusts the replacement
- Network partitions are temporary and eventually resolve
- The corrected RFC 9380 G2 BLS POP suite, ed25519 and BLAKE3 remain secure;
  legacy hash-and-multiply BLS signatures/proofs are forgeable and refused
- Economic incentives align validator behavior with protocol health

### What we do not assume

- Validators will not attempt to maximize their own profit
- Nodes will not collude if profitable
- External dependencies (RPCs, price oracles) are trustworthy
- Users will keep their private keys secure

## Threat categories

### Consensus attacks

#### Double signing

A validator signs two conflicting blocks at the same height.

**Impact**: Safety violation, potential chain split

**Mitigation**:
- Slashing of 100% of validator stake
- Automatic validator removal from set
- Evidence-based slashing prevents false accusations
- Validator software enforces non-conflicting signatures

**Detection**: Anyone can submit evidence of double signing

#### Long range attacks

An attacker buys old private keys and attempts to rewrite history from an old
checkpoint.

**Impact**: Chain reorganization, safety violation

**Mitigation**:
- Weak subjectivity: social consensus on hardcoded checkpoints
- Unbonding period makes key purchases expensive
- Validator set rotation limits old key utility
- Light clients refuse deep reorganizations

**Detection**: Checkpoint monitoring, social consensus alerts

#### Nothing at stake

Validators stake on multiple forks simultaneously without cost.

**Impact**: Consensus failure, inability to finalize

**Mitigation**:
- Slashing conditions for double voting
- Inactivity leaks for non-participating validators
- Bonding period makes stakes non-transferable

**Detection**: Vote aggregation reveals equivocation

### Network layer attacks

#### Eclipse attacks

An attacker isolates a validator from honest peers, feeding them false information.

**Impact**: Validator makes decisions based on incomplete or wrong data

**Mitigation**:
- Minimum peer requirements (min 10 peers)
- Random peer selection from DHT
- Connection to bootstrap nodes with hardcoded IPs
- Peer diversity scoring

**Detection**: Peer count monitoring, block height comparisons

#### DDoS attacks

Overwhelming validators or seed nodes with traffic.

**Impact**: Validator downtime, missed blocks, network degradation

**Mitigation**:
- Rate limiting on all public endpoints
- Connection quotas per IP
- Distributed validator architecture
- CDN protection for RPC endpoints

**Detection**: Traffic pattern analysis, latency spikes

#### Sybil attacks

Creating many fake identities to gain disproportionate influence.

**Impact**: Vote manipulation, eclipse attacks

**Mitigation**:
- Proof of stake: influence requires capital, not just identities
- Minimum stake requirements (10,000,000 SUB, the `MIN_VALIDATOR_STAKE`
  constant)
- Validator caps per entity

**Detection**: On-chain stake analysis

#### Block withholding and forks

A peer withholds a block, or two nodes produce at the same height.

**Impact**: A node stalls at a height, or the chain splits with no recovery.

**Current behaviour**: Gossip is best-effort and there is no block sync, so a node
that misses a block cannot request it. `accept_block` only accepts the exact child
of the local tip; a competing block at the same height is rejected and dropped
permanently. Blocks that arrive early are buffered for at most 64 heights.

**Mitigation**: None at the protocol level. Run a single producer, and keep the
gossip mesh small and well connected. This is a known gap, not a defended
property.

**Detection**: Compare `eth_blockNumber` and `state_root` across nodes.

#### IBC counterparty impersonation

A relayer delivers a packet that the counterparty chain never committed.

**Impact**: Minted or released funds on the receiving chain.

**Mitigation**: `receive_packet_with_proof` requires a header signed by the
client's trusted key, a strictly increasing height and timestamp, and a Merkle
inclusion proof for the exact `(port, channel, sequence)` key whose committed value
equals the packet payload. The packet then passes the normal channel binding,
timeout, ordering, and replay checks. The proof is checked against the trusted app
hash, not against a hash supplied in the message.

**Residual risk**: The client trusts one key with no rotation, no multisig, and no
non-membership proofs. Compromise of that key compromises the bridge, and packet
absence cannot be proven. Do not treat this as a trust-minimized bridge.

### Economic attacks

#### MEV extraction

Validators reorder, insert, or censor transactions for profit.

**Impact**: User harm, fairness violations

**Mitigation**:
- Encrypted mempool (Dandelion++ routing)
- Threshold encryption for transaction ordering
- Commit-reveal schemes for block proposals
- Protocol-level MEV capture and redistribution

**Detection**: Block content analysis, transaction timing studies

#### Flash loan attacks

Borrowing large amounts to manipulate protocol state atomically.

**Impact**: Price oracle manipulation, arbitrage exploits

**Mitigation**:
- TWAP oracles (time-weighted average prices)
- Multiple oracle sources with median aggregation
- Circuit breakers for extreme price moves
- Atomic transaction limits

**Detection**: Oracle deviation monitoring, profit analysis

### Smart contract vulnerabilities

#### Reentrancy

Contracts calling external contracts before updating state.

**Impact**: Fund draining, state corruption

**Mitigation**:
- Checks-effects-interactions pattern enforcement
- Reentrancy guards in standard libraries
- Static analysis in deployment pipeline
- Formal verification of critical contracts

**Detection**: Bytecode analysis, transaction simulation

#### Integer overflow and underflow

Arithmetic operations exceeding type bounds.

**Impact**: Unexpected behavior, potential fund loss

**Mitigation**:
- Safe math libraries (checked arithmetic)
- Compiler warnings and errors
- Audits focusing on arithmetic sections

**Detection**: Fuzzing, symbolic execution

### Infrastructure attacks

#### Key compromise

Validator or user private keys stolen.

**Impact**: Unauthorized transactions, slashing evasion

**Mitigation**:
- Hardware security modules (HSMs) recommended
- Key rotation procedures
- Multi-sig for high-value operations
- Threshold signing for validators

**Detection**: Unusual transaction patterns, key rotation events

#### Supply chain attacks

Malicious code in dependencies or build process.

**Impact**: Backdoors, compromised binaries

**Mitigation**:
- Minimal dependency tree
- Reproducible builds
- Dependency auditing (cargo audit)
- Binary verification via checksums

**Detection**: Build hash mismatches, CVE monitoring

## Risk assessment matrix

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

## Planned monitoring and response

The repository does not deploy a production monitoring or incident-response
system. The items below are operational goals for a future network.

### What we monitor

- Validator uptime and participation rates
- Peer counts and network health
- Oracle price deviations
- Mempool transaction patterns
- Block finality times
- Token flow anomalies

### Response procedures

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

## Reporting security issues

There is **no bug bounty program** and no reward pool. If you find a suspected
vulnerability, report it privately to `security@subhost.xyz`. Include the affected
commit, exact file and line, impact, and reproduction steps. Please do not open a
public issue for an undisclosed vulnerability.

## Audit status

No completed third-party audit report is checked into this repository. Do not
interpret previously proposed auditor names or dates as evidence of an engagement
or completed review.

---

This document is maintained by the Subhost security team. Questions or concerns
should be directed to security@subhost.xyz.
