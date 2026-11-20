# Starting and stopping a node

A node is a single writer over its data directory. Never run two nodes against
the same directory, and never copy, prune, migrate or restore it while a node is
running. Stop the process first and wait for it to exit.

## Before you start

- Verify the binary or image digest and Sigstore identity per
  [release.md](../release.md), and read the changelog for genesis/crypto/wire
  changes.
- Confirm the data directory holds a coherent set: `genesis.json`, the
  `ledger/` manifest/snapshots/segments, the producer identity, the voting
  journal, and (if present) `fork-incidents.bin` and the certified-history
  archive. Keep their names and permissions together.
- A validator needs a genesis with a validator set and a producer key that is
  one of its validators. `subhost node --validator` refuses to start without a
  genesis validator set.

## Start

```bash
# Single-node demo (no peers, no validator set required):
subhost node --single-writer --data-dir /var/lib/subhost

# Validator on a private network with metrics and authenticated public RPC:
subhost node --validator \
  --data-dir /var/lib/subhost \
  --p2p-listen /ip4/0.0.0.0/tcp/4001 \
  --bootstrap-peer /ip4/<peer>/tcp/4001/p2p/<peer-id> \
  --metrics-addr 127.0.0.1:9100 \
  --rpc-auth-token-file /etc/subhost/rpc-token \
  --rpc-allow-public
```

- `--metrics-addr` serves the Prometheus exporter and `/health/*`. Bind it to
  loopback or a private interface; it is unauthenticated (see
  [observability.md](observability.md)).
- A non-loopback RPC bind requires `--rpc-allow-public` **and**
  `--rpc-auth-token-file`. There is no server-side TLS; the supported public
  deployment terminates TLS at a reverse proxy.
- Omit `--p2p-listen` to run offline. `--bootstrap-peer` requires it.
- `--producer-credential-file FILE` (requires `--validator`) boots from the
  encrypted producer keystore instead of the plaintext `producer.key`;
  fail-closed, no plaintext fallback. See
  [producer-keystore.md](producer-keystore.md).
- `--retention-blocks <N>` configures automatic ledger retention (unset = off).
  Accepted now, inert until the Q-036 trigger slice lands; semantics and the
  snapshot floor are in [ledger-repair.md](ledger-repair.md).
- `--mempool-persist` opts into a durable pending pool
  (`<data-dir>/mempool.bin`, restored on start-up; a corrupt or foreign
  snapshot refuses boot). Default off keeps the pool volatile. See
  [mempool-persistence.md](mempool-persistence.md).
- `--otlp-endpoint <URL>` (env `SUBHOST_OTLP_ENDPOINT`) opts into OTLP span
  export. It is a global flag accepted on any subcommand; unset keeps export
  off and log output unchanged. The endpoint is used verbatim, so include
  `/v1/traces`. See [observability.md](observability.md).

## Confirm it is healthy

- `GET /health/live` → 200 once the exporter answers. Liveness is not readiness.
- `GET /health/ready` → 200 only when `ledger_loaded && !syncing &&
  consensus_alive`; otherwise 503. Wire readiness probes here.
- A node that boots onto an unresolved conflicting-final-evidence incident
  re-halts before signing its first view: `/health/ready` stays 503 and the log
  says `durable fork incident recorded`. Investigate per
  [incident-investigation.md](incident-investigation.md); do not delete the
  incident file to force a start.
- A lagging node sets `catching_up` and suppresses production until it reaches
  the announced peer tip (see [certified-sync.md](certified-sync.md)). Readiness
  reflects this; it is not a fault.

## Stop

- Terminal: `Ctrl-C`. Managed service: stop it through its manager. On Unix the
  node also handles `SIGTERM`. Both resolve a controlled shutdown that stops
  every spawned service.
- Wait for the process to exit before touching the data directory. The storage
  API assumes one writer; copying files while the manifest changes can produce
  an incoherent backup (see [backup-restore.md](backup-restore.md)).
- A clean stop preserves the journal and incident state. The next start replays
  and re-authenticates them; nothing is lost by stopping.

## Restart safety

Restarting never rolls finalized history back. The ledger is append-only; the
node replays the retained suffix against the verified head and re-authenticates
restored voting obligations. If the previous run halted on a fork incident, the
restart stays halted until an operator resolves it: a restart is not a recovery
path.

The pending pool ages from the restart, not from original acceptance: the
mempool snapshot stores transactions, not admission timestamps, so restored
entries get a fresh TTL window. With the 30-minute default, an entry parked
across a restart expires 30 minutes after the node starts again. That is
intended revalidation, not data loss; see
[mempool-persistence.md](mempool-persistence.md).