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
borrow the pool's entries and bound serialized sizes before allocating records;
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

S6.1b must reconcile the committed ledger and voting journal first, load and
validate the pending snapshot, drop transactions below each sender's committed
nonce, and rebuild a candidate under the current pool and proposal-byte policy.
Only then may that candidate become the live pool. Changed limits/fee policy need
an explicit re-admission/drop policy, rather than a blind in-memory restore.

Admission/replacement/eviction candidates must persist successfully before live
pool replacement and before returning their transaction hash. Cleanup of mined
entries after a durable block commit is best-effort/retryable: a pending-file
cleanup error must not falsely fail a committed block or stop driver adoption.
Stale mined entries are discarded when restoring against the committed nonces.

## Compatibility and operator boundaries

The store contains signed pending transactions, not producer secrets, accounts,
receipts, votes or QCs. Ledger and voting-journal formats are unchanged. Keep
the newest signing journal independently; a pending snapshot cannot justify
rolling voting state back or certifying missing history.

Retain `mempool.bin` in whole-directory offline backups once consumers enable it.
Unknown versions, wrong context, invalid signatures, duplicate slots, size limits,
truncation and checksum failure are errors. There is no implicit repair or format
migration. Current CLI persistence/capacity flags are not exposed by S6.1a.

Tests cover deterministic replacement/restart, empty snapshots, context mismatch,
corruption/truncation, invalid signed entries, duplicate slots, record/file/count
bounds, failed filesystem/rename writes, orphan temporaries and `0600` permissions.
