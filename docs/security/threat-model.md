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
> ledger whose block and receipt commitments are re-validated on load; a JSON-RPC
> subset that requires a valid signature bound to the sender; Merkle state and
> receipt commitments with per-account inclusion proofs; multi-transaction block
> production and full re-validation of peer blocks; BLS-verified quorum
> certificates with separate `n - f` Prepare/Commit quorums, genesis-bound signed
> leaders, durable prepared locks and authenticated timeout rotation;
> stake-deducting slashing with ejection (100% for double signing, 1% for
> downtime); native bounded certified-history catch-up and durable finalized-fork
> incident handling (halt, no rollback); a
> libp2p gossip transport that the node uses for live block propagation; and an IBC
> light client that verifies a single ed25519 signer's headers and Merkle
> commitment proofs.
>
> **Not implemented:** peer scoring and per-peer abuse controls, and transaction
> gossip (S7); an encrypted mempool, Dandelion++ routing, threshold encryption,
> inactivity leaks, weak-subjectivity checkpoints, an unbonding period,
> validator-set rotation, an encrypted producer key for node start-up (S9.6),
> on-chain governance, EVM or WASM execution, zk proofs, a Merkle Patricia Trie,
> non-membership proofs, multisig light clients, oracle or MEV defences, an HSM
> program, per-IP rate limiting, in-process TLS, and authentication on the metrics
> or faucet surfaces. A history gap beyond the retained certified window still
> needs a snapshot or a fresh testnet. JSON-RPC authentication exists but is opt-in
> and defaults to off. There is no bug bounty.
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
- Recorded double-sign evidence slashes 100% of the validator's self-stake and
  delegated stake and ejects it from the set (`StakingModule::slash`).
- The consensus driver holds a per-height prepared lock and journals signing
  obligations before release, so a correct validator refuses conflicting
  signatures and reconciles an interrupted append on restart.
- Slashing evidence must carry a proof; proof-free evidence is rejected.

**Residual risk**: Slashing is invoked through the staking registry. This
codebase exposes no public evidence-submission method, no network-wide
equivocation gossip, and no automatic on-chain adjudication.

**Detection**: Compare signed votes and blocks for the same height and view out of
band.

#### Long range attacks

An attacker buys old private keys and attempts to rewrite history from an old
checkpoint.

**Impact**: Chain reorganization, safety violation

**Mitigation**: No weak-subjectivity checkpoints, no unbonding period, no
validator-set rotation, and no fork-choice or light-client rule that refuses a
deep reorganization. A conflicting authenticated final certificate is handled as
a durable incident that halts the node (ADR-007), not as a reorg; this detects a
finalized fork but does not prevent a long-range rewrite from an old checkpoint.

**Residual risk**: Treat validator keys as long-lived, pin the expected chain out
of band, and do not rely on this code for long-range safety. Checkpoints,
unbonding, and rotation remain roadmap items.

**Detection**: Out-of-band checkpoint and chain-identity monitoring.

#### Nothing at stake

Validators stake on multiple forks simultaneously without cost.

**Impact**: Consensus failure, inability to finalize

**Mitigation**:
- Double signing is slashable at 100% of self- and delegated stake with ejection;
  downtime is slashable at 1%.
- Prepared locks plus the prepare-then-commit rule stop a correct replica from
  voting for two blocks at one height.
- Signed timeout reports and leader rotation let the set advance past a silent
  leader.

**Residual risk**: There is no inactivity leak and no reward or penalty schedule;
equivocation evidence is collected out of band.

**Detection**: Out-of-band vote and block comparison.

### Network layer attacks

#### Eclipse attacks

An attacker isolates a validator from honest peers, feeding them false information.

**Impact**: Validator makes decisions based on incomplete or wrong data

**Current behaviour**: The node joins a libp2p gossip mesh and retries configured
bootstrap endpoints with capped backoff. It does not enforce a minimum peer
count, select peers randomly from a DHT, score peer diversity, or eject
low-quality peers.

**Mitigation**: None at the protocol level. Keep the mesh small and well
connected, and monitor peer counts and block height out of band.

**Detection**: Peer count monitoring and block height comparison across nodes.

#### DDoS attacks

Overwhelming validators or seed nodes with traffic.

**Impact**: Validator downtime, missed blocks, network degradation

**Current behaviour**: JSON-RPC binds loopback by default. `--rpc-auth-token-file`
adds a bearer token and `--rpc-rate-limit` adds a per-connection token bucket,
both off unless configured, and a non-loopback bind is refused without
`--rpc-allow-public` plus a token. The metrics exporter and faucet are
unauthenticated. jsonrpsee has no server-side TLS, so the supported deployment is
a TLS-terminating reverse proxy in front of a loopback bind. There is no per-IP
connection quota or CDN integration in the code.

**Detection**: Traffic pattern analysis, latency spikes, and comparison with the
configured bind and auth settings.

#### Sybil attacks

Creating many fake identities to gain disproportionate influence.

**Impact**: Vote manipulation, eclipse attacks

**Mitigation**:
- Proof of stake: influence requires capital, not just identities
- Minimum stake requirements (10,000,000 SUB, the `MIN_VALIDATOR_STAKE`
  constant)
- Genesis power is not quorum weight; there is no per-entity validator cap

**Detection**: On-chain stake analysis

#### Block withholding and forks

A peer withholds a block, or two nodes produce at the same height.

**Impact**: A node stalls at a height, or the chain splits with no recovery.

**Current behaviour**: With a genesis validator set, the node commits a block only
after independently authenticating its final `n - f` Commit QC and durably
appending it. Native catch-up (S3.3) lets a node behind a gap request bounded
`GetBlocks`/`Blocks` ranges from a peer's certified archive (newest 4096 pairs) and
verify every certificate and state transition in order; the request is derived
from an authenticated certificate, a candidate past a gap is buffered (bounded,
64) until its ancestors commit, and a catch-up with no contiguous progress for
30 s releases signing. A gap beyond the retained window still needs a snapshot or
a fresh testnet. Demo chains without validators use `accept_block`, which accepts
only the exact child of the local tip; early blocks are buffered up to 64 heights.

**Mitigation**: A conflicting authenticated final certificate is a durable
incident: the node writes `fork-incidents.bin`, halts signing and readiness, and
re-halts on restart until an operator resolves it; the append-only ledger is never
rolled back. Duplicate and unauthenticated evidence never halt. Keep the gossip
mesh well connected and run redundant certified-history relays so a missing
certificate stays available.

**Residual risk**: If every archive misses a block, recovery cannot manufacture
its certificate. Per-peer rate limiting and abuse controls are still S7. A pruning
node can currently be halted by a legitimate historical certificate for a pruned
height (the `Historical` comparison treats an absent block as a conflict); see
finding R-57-01 in `security-audit/2026-11-19-pr57-native-sync-review.md`.

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

### Authentication and transport

#### Unauthenticated service exposure

The JSON-RPC subset authenticates each transaction with an ed25519 signature bound
to the sender, but the transport itself is unauthenticated by default: no bearer
token, no rate limit, and a loopback bind. The metrics exporter and faucet have no
authentication, and there is no in-process TLS.

**Impact**: Anything that can reach a loopback or misconfigured public bind can
read state, submit transactions, or scrape metrics.

**Mitigation**: Keep the default loopback bind. To expose JSON-RPC, configure
`--rpc-auth-token-file` and `--rpc-rate-limit` and terminate TLS at a reverse
proxy. Do not expose the metrics or faucet ports. `--rpc-allow-public` without a
token is refused at startup.

**Residual risk**: WebSocket tokens are checked at the upgrade handshake, not per
message, and the rate limit is per connection because the middleware does not see
the peer address. The explorer signs transfers with a local key taken from the
request body and refuses a non-loopback bind for that reason.

### Storage and key material

#### Ledger or journal tampering

**Impact**: A node loads a forged history, double-executes a transaction, or signs
with a stale view.

**Mitigation**: The segmented ledger is append-only with BLAKE3 checksums per
record and snapshot; the manifest is replaced atomically. On load, only the
blocks appended on top of a cached verified head are re-validated against every
retained commitment, and the cache advances only after the manifest commit
succeeds; a checksum mismatch or a truncated suffix is refused. The
consensus journal (`SUBVOTE2`) carries a checksum and complete genesis/identity
binding, and an OS lock refuses a second process in the same data directory. A
ledger behind newer signing state, or a missing or damaged journal on an existing
validator ledger, is refused pending recovery.

**Residual risk**: Checksums detect corruption and naive edits, not an attacker who
can rewrite the whole directory consistently. The producer key is unencrypted and
readable by the node's user, and the state tree proves membership only, so account
absence cannot be proven.

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
- The producer key is a single hex-encoded BLS scalar in
  `<data_dir>/producer.key`, mode `0600`, refused if group or others can read it;
  it is not encrypted (S9.6 replaces it with an encrypted keystore).
- Wallet keystores use scrypt + AES-256-GCM; file-supplied scrypt costs are
  bounded before allocation (at most 256 MiB of memory and 2^22 work units), so a
  crafted wallet file cannot abort the process.
- HSM support, key rotation, multisig, and threshold signing are roadmap items.

**Detection**: Unusual transaction patterns and unexpected producer-key use.

#### Supply chain attacks

Malicious code in dependencies or build process.

**Impact**: Backdoors, compromised binaries

**Mitigation**:
- Minimal dependency tree
- `cargo-deny` in CI over advisories, bans, licenses, and sources
- Reproducible-build verification and binary checksum pipelines are not checked in

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
| Unauthenticated RPC/metrics exposure | Medium | High | High |
| Ledger/journal tampering | Low | High | Medium |

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
