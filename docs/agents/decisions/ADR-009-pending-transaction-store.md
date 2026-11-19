# ADR-009: Bounded pending snapshots and admission durability

Status: Accepted · 2026-11-19

## Decision

`MempoolStore::open(chain_id, genesis_hash, data_dir)` binds `mempool.bin` to a
nonzero chain and canonical trusted genesis commitment. `load()` returns a
complete validated list or an empty list when absent. `persist(&Mempool)` writes
a deterministic snapshot ordered by sender/nonce, premeasuring the full encoded
size before allocating the snapshot. The caller serializes directory writes.

Format v1 uses `SUBPOOL1`, a version, BLAKE3 checksum, chain/genesis binding and
length-framed fixed-integer bincode transaction records. Limits are 64 MiB,
10,000 entries and 1 MiB per encoded record. Load checks signatures, transfer
shape, duplicate slots, context, version, checksum and bounds before returning
anything. The file contains signed transactions, not keys, accounts, receipts,
votes or QCs. Ledger/journal formats remain unchanged.

Create a sibling owner-only temporary file, sync its content, rename it over the
snapshot and sync the directory on Unix. Candidate acknowledgement and live pool
installation require successful persistence. A post-rename sync failure may
leave the complete new snapshot visible; this is documented durability
uncertainty, not a successful acknowledgement. Startup validates the surviving
snapshot. No partial pending pool is returned on validation failure.

## Consumer contract

S6.1b loads after journal/ledger reconciliation, drops mined nonces/receipts and
re-admits under current pool/proposal policy before publication. It must provide
an aggregate runtime byte budget even when persistence is off. Candidate
admissions/replacements/evictions persist before visible replacement and RPC
acknowledgement. Cleanup after a durable ledger commit is retryable/best-effort;
its failure cannot make that committed block appear failed. Stale disk entries
are filtered at restart. Node/RPC/CLI consumers follow a separate code contract.

S6.1a supplies only the store/API; bundled-node admissions remain volatile
outside journalled proposals until consumer wiring lands. The checksum detects
damage/context mistakes, not privileged tampering or execution/finality.
Current transfer authentication is ed25519 and independent of the BLS upgrade;
a regenerated genesis changes the store context. Later execution/IBC transaction
variants need reviewed format/validation extensions, not implicit rejection or
reinterpretation of old records. Keep the newest voting journal independently.

## Evidence

- https://doc.rust-lang.org/1.89.0/std/fs/struct.File.html: file sync exposes durability errors (verified 2026-11-18 in Q-011).
- https://docs.rs/bincode/1.3.3/bincode/config/struct.DefaultOptions.html: explicit fixed encoding, byte limits and trailing-byte rejection (verified against cached 1.3.3 source, 2026-11-19).
- Existing RPC candidate-pool and persist-before-visibility paths require this failure ordering.
