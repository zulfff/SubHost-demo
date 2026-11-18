# Validator consensus

Node owns a per-height prepare/commit driver for validator genesis. Validators
sign with distinct BLS keys; observers verify without signing. The sorted
validator at `(view - 1) % n` leads. Views start at one, rotate on authenticated
timeout evidence, and advance independently from block height.

## Admission and final commitment

1. RPC authenticates a transaction and dry-runs bounded deterministic selection.
   Accepted transactions remain pending. A returned hash precedes its receipt
   and changes neither account state nor committed height.
2. The leader builds and producer-signs a block against cloned state. It signs a
   genesis-bound proposal envelope including view and any timeout proof. Replicas
   check parent, height, producer, signatures, gas, execution and all commitments
   before voting.
3. `n - f` distinct Prepare votes lock the block, where `f = (n - 1) / 3`.
   A prepared replica sends a separate Commit vote. Only `n - f` Commit votes
   form a final QC. Four validators require three votes in each phase. Genesis
   power does not multiply quorum weight. Signatures bind genesis context,
   phase/domain, view and block hash as applicable.
4. The node journals signing obligations before publication and final commit
   intent before ledger append. `commit_qc` independently authenticates the
   final QC, newer committed view, block and execution. State, receipts, pool
   cleanup and driver tip become visible after durable success. Restart
   reconciles interrupted append without duplicate execution.
5. Typed proposal/vote/prepare/report/timeout messages and the latest certified
   commit are periodically disseminated as peers recover. Duplicate identities
   cannot add quorum weight. A bounded positive signature cache avoids repeated
   pairings for exactly authenticated bytes; semantic checks still run and cache
   trust is never deserialized.

Unsigned demo genesis requires explicit `--single-writer`, seals synchronously
and uses view-zero block gossip. Validator mode rejects uncertified commits and
old unphased proposal/vote messages.

## Timeouts and locks

The timeout defaults to 2000 ms. A timeout advances local view and signs a report
with the complete highest prepared block/certificate. A rotated leader requires
`n - f` exact-target reports and selects the highest prepared block. Time alone
cannot authorize a replacement or erase a lock. The f+1-th highest authenticated
target lets lagging replicas join. A lone timeout-only replica pauses until
another honest replica supports progress. Backoff is capped at
`max(base timeout, 30 seconds)`.

The first accepted proposal, Prepare certificate and timeout certificate each
start a full interval for the next phase. Identical retransmissions and extra
signatures on an existing proof cannot keep extending that deadline.

Two live validators out of four cannot commit; height, receipts, balances and
manifest stay unchanged while views advance. No empty blocks advance time.
See [ADR-008](agents/decisions/ADR-008-phased-consensus-standard-bls.md).

## Recovery and upgrade

Version-two `consensus-state.bin` has magic `SUBVOTE2`, checksum and complete
genesis/identity binding. It retains both vote phases, active view, prepared lock,
reports, timeout proof and latest final QC. An OS lock refuses a second process
in that data directory. Missing/damaged journals on existing validator ledgers
are refused. A ledger alone establishes no verified committed view.

Keep the newest journal with its ledger, genesis and producer key. Never restore
older signing state or run the same identity in another directory. A ledger
behind newer signing state is refused pending recovery.

The RFC 9380 G2 BLS suite deliberately rejects old proofs/signatures: the legacy
construction permits public-PoP signature forgery. Use fresh genesis proofs and
fresh validator directories, preserving old data and keys. There is no legacy
verifier or automatic journal migration. All validator peers upgrade together.

## Bounds and remaining stages

The 4 MiB transport reserves framing headroom and checks actual JSON envelopes.
Canonical block budgets (at most 64 KiB, decreasing for larger sets) reserve
repeated prepared blocks and proof signatures in full timeout envelopes and the
16 MiB journal. Node sets support 1 through 64 validators. Oversized admission
or replacement preserves the existing pool and committed state.

Native historical sync is S3: latest-commit gossip repairs only the next block,
and ledger replay does not yet authenticate every historical QC. Transaction
gossip and peer abuse controls are S7; submit the same transaction to potential
leaders until that stage lands. Pending transactions remain volatile until S6.
The timer assumes eventual delivery within its configured capped bound.
This PBFT-style prototype still needs independent review and real audit/soak
gates before production assurance. It is not chained HotStuff.

## Verification

```sh
cargo test -p subhost-consensus -p subhost-rpc -p subhost-node -p subhost-network --all-features
```

Regressions cover phase/context separation, timeout proof/lock retention,
checkpoint validation, insufficient quorum, journal/append failures, bounded
admission/envelopes, real gossip, silent leaders, partition recovery, different
restored views and service cleanup. The [local-testnet runner](local-testnet.md)
provides a four-validator smoke check.
