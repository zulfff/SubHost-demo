# ADR-005: Node-local voting recovery and bootstrap reconnect

- Status: accepted
- Date: 2026-11-18
- Settles: Q-011
- Tracks: A (consensus/node/network), B (ledger and operator consumers)

## Decision

Save a key-free consensus driver checkpoint in node-local `consensus-state.bin`.
The envelope is versioned, size-bounded and checksummed, and its payload binds
the chain, deterministic genesis commitment and signing identity. Restore verifies
every saved vote/QC; an open validator round must contain its own prior vote.

Write a candidate round through an exclusively created temporary file, sync the
file, rename it and sync the directory before releasing votes. Hold an OS file
lock in `consensus.lock` to exclude another signer in that data directory.
Journal a prepared block/QC before ledger append. Only after ledger persistence
does the live driver advance, and bootstrap reconciles interrupted intents
without appending a block twice. Ledger and producer-key formats are unchanged.

Refuse missing/incompatible journals on existing validator ledgers and reject a
ledger behind newer voting state. Keep the newest signing journal during backup
recovery; restoring an older whole-directory copy can forget already released
votes. This format does not establish authenticated historical QC replay or a
general rollback/catch-up procedure.

Bootstrap addresses retry with exponential backoff capped at 30 seconds. Match
failed attempts by connection ID, and remove a peer only after its last connection
closes. The four-validator runner uses loopback listeners and distinct keys.

Consume the landed ADR-004 metrics contract in the same S2 continuation, passing
the node's existing registry to RPC. A live RPC/exporter test covers successful
and failed decoded calls; transport observability remains separate S8 work.

## Consequences

Directories from older validator builds without a journal require a fresh local
testnet setup; no implicit migration may assume no prior vote was sent. Pending
pool entries remain volatile, but transactions within an open journalled proposal
survive restart. Timeout/view change, leader rotation, historical sync, transaction
gossip and explicit CLI demo-mode policy remain pending, as documented in
`ROADMAP.md` and `docs/local-testnet.md`.

## Evidence

- https://github.com/cometbft/cometbft/blob/main/privval/file.go: saves last-sign state and reuses matching prior signatures (verified 2026-11-18).
- https://doc.rust-lang.org/1.89.0/std/fs/struct.File.html: file locking is available at the Rust 1.89 floor; sync exposes durability errors (verified against upstream source 2026-11-18).
- https://docs.rs/libp2p-swarm/0.48.0/libp2p_swarm/enum.SwarmEvent.html: attempt IDs and remaining connection counts support retry/accounting (verified against the locked source 2026-11-18).
- Track B agreed with journal/backup semantics and requested consumption of the landed ADR-004 contract in the same node branch. The Q-011 conversation stays gitignored in the workspace.
