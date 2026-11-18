# Automatic certified-history catch-up

The optional `sync-relay` example keeps complete block/quorum-certificate pairs
and replays the authenticated history over the existing P2P transport. A fresh
node with the same genesis can receive old blocks without a manual snapshot.
Each block goes through normal QC, producer, execution and node-journal checks.
Archive v2 requires RFC 9380 BLS signatures and final-commit certificates bound
to the full genesis. Preserve legacy archives separately and generate a fresh
validator genesis when upgrading from the broken scalar-hash suite. Legacy
proofs, signatures and v1 records are refused; there is no compatibility verifier.

The old scalar-hash construction permitted signatures to be forged from public
genesis possession proofs. Archive records bind to the exact new genesis, use
phased final-commit certificates, and order records by block height while signed
timeout views may advance independently.

Build the compatible node and relay together:

```sh
cargo build --release -p subhost-cli --bin subhost --example sync-relay
```

Start the relay before the testnet's first transaction, using the same genesis
and a separate directory:

```sh
target/release/examples/sync-relay \
  --genesis ./testnet/genesis.json --data-dir ./testnet/certified-history \
  --p2p-listen /ip4/127.0.0.1/tcp/30344 \
  --bootstrap-peer /ip4/127.0.0.1/tcp/30340
```

Run a second relay with another archive/listener and connect it to the same
mesh. This preserves certificate availability if one relay restarts or misses
traffic. The relay stores no signing keys and cannot produce a quorum itself.
Bootstrap redial comes from the existing network transport.

Join a new observer using the public genesis and a reachable peer:

```sh
subhost node --data-dir ./observer --listen 127.0.0.1:18549 \
  --p2p-listen /ip4/127.0.0.1/tcp/30349 \
  --bootstrap-peer /ip4/127.0.0.1/tcp/30340 --no-mdns
```

Place the testnet's `genesis.json` in `./observer` before starting. When using
Cloudflare's TCP publication, run its client-side TCP proxy and use the proxy's
local port as the bootstrap address. Public HTTP RPC remains a separate route.

## Recovery, resource bounds and limitations

The archive binds all records to the exact genesis, checks versions/checksums,
holds an exclusive file lock and validates every QC/state transition on startup.
It checks the same read-only certified-block boundary as `commit_qc` before any
record write. Blocks are ordered by height; signed final-QC views may skip ahead
after timeouts and must increase across the committed prefix.
It refuses corruption, a missing prefix or a different genesis. A persistence
error does not advance the relay's head; restart revalidates completed records.
Back up the full archive. Removing the lock file cannot replace stopping the
live process.

Messages are replayed every 50 ms by default; `--replay-interval-ms` adjusts
that pace. Replay starts only after a connected peer announces its gossip-topic
subscription, preventing a startup prefix from being lost before the receiver
is ready. Each message follows the existing transport size bound. The relay
buffers at most 64 future heights and caps record bytes at 64 MiB by default;
configure `--max-history-bytes` for a larger verified history. The cap stops
ingestion instead of silently deleting old certificates. It is not a pruning
policy.

Catch-up scans the recorded prefix, so large histories require more time than a
range-based protocol. If all archives miss a block and no peer retains its QC,
automatic recovery cannot manufacture that certificate; preserve/recover a
verified archive or create a fresh testnet. The ordinary S2 ledger alone is not
a historical QC index.

Do not restore an older validator journal to force a lagging ledger to start.
Normal offline nodes keep their latest matching journal/ledger and can receive
subsequent certificates; incoherent signing-state rollback remains refused.
Native range requests, fork choice/reorg and production finality remain separate
protocol work. Timeout and leader behavior comes from the node's consensus
implementation; the relay only replays already finalized blocks.

Verification covers invalid/insufficient QCs, tampered execution, corruption,
wrong genesis, exclusive-writer locking, capacity failure, out-of-order blocks,
archive restart, skipped/stale views, genesis/phase replay, legacy-version and
public-PoP forgery rejection, and a real P2P observer backfilling 100 certified
blocks. Cancelling a relay aborts its owned network task and releases its listener.
