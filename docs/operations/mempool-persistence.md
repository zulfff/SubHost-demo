# Pending transaction snapshots

S6.1a adds the `MempoolStore` API in `subhost-mempool`. It provides bounded,
validated, atomic pending snapshots. The bundled node/RPC path does not yet use
it: admission durability and CLI configuration are S6.1b consumers of the landed
contract. Accepted transactions remain volatile outside journalled open proposals
until that integration is implemented.

## Store API and limits

`MempoolStore::open(chain_id, genesis_hash, data_dir)` binds a store to a nonzero
chain ID and the caller's canonical trusted genesis commitment. Opening does not
create files. `load()` returns the complete validated transaction list, or an
empty list when `mempool.bin` is absent. It never returns a partial pool on error.
`persist(&pool)` writes a deterministic snapshot ordered by sender and nonce.

The format is `SUBPOOL1`, version 1, a BLAKE3 payload checksum, chain/genesis
binding, a count and length-framed bincode transaction records. Hard limits are
64 MiB for the file, 10,000 entries and 1 MiB per encoded transaction. Writes
borrow the pool's entries and measure the full encoded size before allocating the snapshot;
they do not clone an arbitrarily large pool just to reject its size.

Every stored transfer is checked for the expected chain, recipient, nonzero gas,
representable total cost and an ed25519 signature bound to its sender. Duplicate
sender/nonce slots are refused on load. This validates snapshot integrity and
transaction authentication; it does not establish execution, inclusion, balances
or consensus finality. The checksum/context binding detects damaged files and
accidental cross-genesis copies, not privileged filesystem tampering.

## Durability and recovery contract

The caller must serialize writes for one node directory. A write validates the
candidate, creates a sibling temporary file with owner-only permissions, syncs
its data, atomically replaces `mempool.bin` and syncs the directory on Unix.
Only a successful write may authorize acknowledgement of the candidate admission.
As with filesystem durability errors generally, an error after rename can leave
the complete new snapshot visible; it must not be treated as a successful RPC
acknowledgement. Startup validates whichever complete snapshot survived.

S6.1b's consumer hooks are landed in `subhost-rpc` (Q-034); the node call-site
wiring is the remaining step. `RpcState::with_pending_store` attaches a store,
`publish_pool` persists a candidate before it becomes live, `retire_mined`
cleans the live pool after a durable commit, `retire_mined_candidate` is the
pure no-I/O variant for a candidate that may never be installed, and
`restore_pending` reconciles the snapshot against committed nonces.

`restore_pending` runs only after the ledger and voting journal reconcile. It
drops entries below each sender's committed nonce, entries that cannot fit the
current count/per-sender/calldata/proposal-byte limits, and entries over the
runtime byte budget, in deterministic sender/nonce order with a logged count.
It **never drops an entry merely for being underpriced**: Q-017 requires parked
entries to survive a restart and a fee-floor change, so restore admits without
the local gas-price floor (`Mempool::admit_restored`). Rewriting the filtered
pool to `mempool.bin` is best-effort, so a pending-file problem cannot stop a
node whose ledger is durable.

The aggregate runtime byte budget (`MempoolConfig::max_pending_bytes`, default
32 MiB) binds with persistence on or off; count and per-sender caps alone do not
bound memory, since `max_pending * max_tx_data_bytes` exceeds a GiB at the
defaults. Over-budget admission evicts in the same deterministic lowest-priority
order as the count cap and reports an `evicted` event; a single transaction
larger than the whole budget is refused before the pool is mutated.

Admission/replacement/eviction candidates must persist successfully before live
pool replacement and before returning their transaction hash: `publish_pool`
propagates a persistence failure and leaves the live pool untouched, so no hash
is acknowledged for a non-durable admission. Cleanup of mined entries after a
durable block commit is best-effort/retryable: a pending-file cleanup error is
logged and counted in `subhost_mempool_pending_cleanup_failures_total`, and must
not falsely fail a committed block or stop driver adoption. Stale mined entries
are discarded when restoring against the committed nonces. The node call sites
consume these hooks: admission persists before a hash is returned, a mined
candidate is published cleaned exactly once, and a committed block retires mined
entries with best-effort persistence. A store is attached only when the operator
opts in, so the default pool stays volatile exactly as before.

## Node opt-in and boot recovery

`NodeConfig.mempool_persist` (default `false`) chooses durability; the CLI flag
is `--mempool-persist`. When it is false, no store is attached, `mempool.bin` is
never read or written, and boot behaves exactly as before. When it is true, the
node attaches a store at `<data_dir>/mempool.bin` during bootstrap and restores
it after ledger, journal, archive and fork-log reconciliation and before serving
traffic. The snapshot is bound to the chain's protocol context
(`GenesisConfig::consensus_context()`), so a snapshot copied from another chain,
or one written before a genesis fee-policy or calldata-schedule change, is a
foreign snapshot and refuses to load. Without a genesis file the binding is
`Hash::ZERO` plus the chain id: cross-chain copies are still refused, but two
genesis-less demo chains with the same chain id are indistinguishable.

A failed restore (corrupt, truncated, foreign, oversized, unauthenticated or
partial snapshot) **refuses boot** rather than silently starting with an empty
pool. The boot error names the snapshot path; RPC persistence errors keep no
local detail. Recovery is a one-file action:

1. stop the node;
2. remove `<data_dir>/mempool.bin`;
3. restart.

Deleting the file is safe: it holds signed **pending** transactions only — no
committed ledger history, no votes or QCs, no producer keys. The ledger,
voting journal and archive are untouched. The only loss is accepted-but-unmined
transactions, which clients can resubmit. An absent file is a normal empty
start, not a failure.

Capacity and per-sender flags are an explicit follow-up; the opt-in chooses
durability only, and `Mempool` already enforces count, per-sender and runtime
byte limits with persistence on or off.

## Compatibility and operator boundaries

The store contains signed pending transactions, not producer secrets, accounts,
receipts, votes or QCs. Ledger and voting-journal formats are unchanged. Keep
the newest signing journal independently; a pending snapshot cannot justify
rolling voting state back or certifying missing history.

Retain `mempool.bin` in whole-directory offline backups once consumers enable it.
Unknown versions, wrong context, invalid signatures, duplicate slots, size limits,
truncation and checksum failure are errors. There is no implicit repair or format
migration. The node `mempool_persist` opt-in (CLI `--mempool-persist` in the stacked flag
PR) is exposed; explicit capacity and per-sender flags remain a follow-up.

Tests cover deterministic replacement/restart, empty snapshots, context mismatch,
corruption/truncation, invalid signed entries, duplicate slots, record/file/count
bounds, failed filesystem/rename writes, orphan temporaries and `0600` permissions.
