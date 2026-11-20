# ADR-006: Certified history relay for automatic testnet catch-up

- Status: accepted consumer contract
- Date: 2026-11-18
- Discussion: Q-012
- Tracks: B (CLI/platform relay), A (existing consensus/node/network contracts)

## Decision

Add an optional Platform relay using the existing `NetworkMessage::BlockCommit`
and public certified-block validation/commit APIs from ADR-003/Q-015. It records complete block/QC
pairs from genesis, verifies the trusted genesis registry, every quorum and full
producer/execution transition, and automatically replays the certified prefix
in ascending order. Existing nodes apply each next block through their normal
QC/journal path. No node, consensus, network wire or LedgerStore API changes are
introduced by this consumer.

The archive is genesis-bound, versioned, checksummed and protected by an exclusive
OS file lock. Records are written atomically with file/directory sync before
they become eligible for relay. Startup validates and executes the entire prefix;
gaps, corruption, different genesis and byte-limit violations fail closed.
At most 64 future heights are buffered; outgoing messages are paced and each
message retains the existing transport bound. Invalid traffic cannot become a
committed archive record. I/O failure stops the relay before advancing its head.

Archive v2 requires RFC 9380 BLS and genesis-bound final-phase certificates.
Full read-only commit validation precedes persistence, including monotonic signed
views and execution. Height orders records and bounds future buffering; views
may advance independently after timeouts. Reject v1 records and legacy proofs or
signatures without fallback. Preserve incompatible data rather than migrating it.

## Deployment and limits

Start archives at genesis, before admitting transactions, or recover an existing
verified archive. A late archive cannot invent QCs absent from both the ledger
and peers. Run redundant relays so one can recover missed records from the
other's certified history. Preserve their full directories during backup.

The default record-byte cap is 64 MiB. Reaching it stops archive ingestion;
operators must increase the configured bound or retain a verified copy. This
slice does not prune certificate history or change LedgerStore retention.

Rebroadcast catch-up is automatic but scans the prefix, rather than implementing
per-peer range requests. Native bounded request/response sync and fork choice/reorg
remain Track A protocol work. Consensus timeout/leader rules are inherited from
the node implementation. Voting
journals still may not be rolled back to bypass newer signing obligations.
The relay does not establish production finality or a multi-host testnet soak.

The public prototype consumes isolated corrected crypto commit `c7b8aa9`:
RFC 9380 BLS with official vectors and public-PoP forgery rejection. Fresh
genesis possession proofs change the archive binding; old proofs/signatures
are refused without a compatibility verifier. The stable linear QC contract
still requires view equal to height. The agreed phased-QC/context consumer is
prepared separately and requires its own tested Protocol revision and rollout.

## Evidence

- https://github.com/cometbft/cometbft/blob/main/blocksync/reactor.go: verifies
  historical validator commits before execution/persistence (verified 2026-11-18).
- ADR-003/ADR-005: certified commit and durable node voting contracts.
- Q-012 replies: both tracks require full block/QC validation and a fresh
  history; S2 revision `cfa9553` has verified quorum/recovery/reconnect support.
- `crates/subhost-cli/examples/sync_relay/history.rs`: full validated archive,
  atomic persistence and limits; failure tests and real 100-block P2P backfill.
