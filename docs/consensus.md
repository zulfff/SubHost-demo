# Validator consensus wiring

When genesis contains validators, `Node::bootstrap` owns a linear consensus driver
at the restored ledger height/hash. A node with a producer key can propose and vote;
an observer verifies proposals, votes and certificates without signing.

## Transaction and block flow

1. JSON-RPC checks transaction authentication. Admission dry-runs deterministic
   selection, then leaves an executable transaction in the pool and returns its
   hash. Height, account state and receipts stay unchanged. Clients poll for a
   receipt; a returned hash does not establish finality.
2. The producer's block ticker calls `build_block()` to assemble and sign a block
   against a state clone. Proposal construction neither writes the ledger nor
   removes pending transactions.
3. Gossip carries `BlockProposal { view, data }` and `Vote { view, validator,
   block_hash, signature }`. A proposal must extend the current tip, authenticate
   its producer, and reproduce all transaction/state/receipt commitments before
   any validator signs. A vote without a matching open proposal is discarded.
4. Distinct registered BLS keys sign the QC-domain payload. Quorum is `n - f`,
   where `f = (n - 1) / 3`; for four validators it is three. `2f + 1` is equivalent
   only when `n = 3f + 1`. An exact proposal retransmission reuses the existing
   vote, and a conflicting open proposal cannot replace it.
5. `commit_qc(block, qc)` independently checks every QC signer/signature, block
   hash and linear view (equal to height), then revalidates execution and persists
   the block. Only after success are account state, receipts, pool and height
   published and the candidate driver adopted.
6. Failed persistence leaves the live driver tip and visible state unchanged.
   The ready tally remains in memory and the next ticker retries the same block.
   `BlockCommit { view, data, signatures }` carries a verified next-block QC to
   peers that missed the final vote. Proposal/vote/latest-commit dissemination is
   retried as peers join the mesh; duplicates cannot add quorum weight.

A genesis with no validators retains the synchronous demo/benchmark path:
submission seals immediately and legacy gossip uses view zero. `include_transaction`
and uncertified `accept_block` refuse the validator path.

## Current limits

This is a prototype linear quorum driver, not the full three-phase HotStuff
protocol or production BFT. There is no timeout, view change or leader rotation.
Concurrent proposals can stall, and two silent validators in a set of four prevent
commit indefinitely. No new empty blocks are created to advance time.

Unfinished votes/locks are not durable; restarting during an open round can permit
another vote for that height. Committed heights restore the linear view, but QCs
remain in memory and the existing ledger does not authenticate historical QCs on
replay. Durable vote recovery belongs to S2.4 and a QC-aware ledger to S3.4.
Do not claim crash/partition safety or finality from this slice.

There is no historical block sync/backfill, fork choice or reorg. Certified gossip
accepts only the next block. A long-offline node cannot recover a history gap from
the latest commit alone. There is no transaction gossip: submit to a producer.
Accepted pool entries are volatile until S6 adds persistence. Failed gossip tasks
are logged; there is no health/readiness guarantee before S8.

The explicit `--single-writer` CLI policy remains part of the Track B contract for
S2.3; this slice retains implicit demo mode for genesis without validators. The
wire layout changed, so all peers in a devnet must upgrade together. No ledger
format or producer-key format changes are introduced here.

## Verification

The node tests cover four validators with four/three/two voting participants,
unchanged manifest bytes without quorum, a failed write followed by certificate
retry, restart at a committed tip, forged/unseen traffic, and two validators over
real libp2p loopback gossip. RPC tests reject missing/insufficient/duplicate/invalid
certificates and check that admission/building never exposes state or receipts.

```sh
cargo test -p subhost-consensus -p subhost-rpc -p subhost-node -p subhost-network --all-features
```
