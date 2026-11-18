# ADR-003: Pending RPC admission and durable quorum commits

- Status: accepted
- Date: 2026-11-18
- Settles: Q-007
- Tracks: A (consensus/block/node/network), B (RPC method and CLI consumers)

## Decision

RPC submissions on a chain with genesis validators remain pending until a quorum
certificate commits their block. Clients receive the transaction hash first and
poll for a receipt. No direct inclusion or uncertified accept path may commit in
validator mode. Chains without validators retain the existing demo path; an
explicit CLI policy remains the rest of S2.3.

`RpcState` gains additive `tip_hash` and `has_validator_set` accessors in a separate
contract code commit before the S2 implementation. `build_block` and proposal
validation are read-only; `commit_qc` independently verifies the QC and persists
before visibility. The driver tip is adopted only after persistence succeeds;
valid votes stay available for an I/O retry.

Quorum sizing is `n - f`, `f = (n - 1) / 3`. Using `2f + 1` for arbitrary `n`
would allow a single validator to commit in a two- or three-validator set.
Proposals carry a view, votes carry their signer/view/hash/signature, and certified
next-block gossip carries a QC. Observers verify without signing.

## Consequences and limits

The wire changes require a coordinated devnet upgrade. Receipt timing changes but
no ledger or key format changes. S2.4 must still supply timeouts, leader/view change
and durable unfinished votes; S3.4 must persist/authenticate QCs. The current linear
view equals committed height and does not claim production HotStuff safety.
See `docs/consensus.md` for the remaining limits.

## Evidence

- https://arxiv.org/pdf/1803.05069 — §§3–4 specify `n = 3f + 1` and certificates
  from `n - f` replicas (verified 2026-11-18).
- `crates/subhost-consensus/src/lib.rs`, `ConsensusConfig::new` — derives quorum
  from the genesis registry count.
- `crates/subhost-rpc/src/block.rs`, `commit_qc` — verifies before persistence.
- Q-007 question and Track B reply are workspace-local and gitignored, per the
  inter-agent protocol. Track B agrees with pending receipts and requires the
  additive state API in its own contract commit.
