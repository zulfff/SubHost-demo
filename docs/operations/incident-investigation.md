# Investigating a conflicting-final-evidence incident

A halt is an accountability event, not a crash. When a fully authenticated final
certificate contradicts local finality, the consensus classifier returns
`Conflicting`, the node persists a durable incident and halts signing and
readiness. Certified history still applies; the append-only ledger is never
rolled back. This is the S3.2 decision in
[ADR-007](../agents/decisions/ADR-007-native-certified-fork-accountability.md).
It is **not** a reorg: `subhost_reorg_total` stays at 0, and a safety incident is
not a reorg.

## What the node records

- `fork-incidents.bin`, beside the voting journal in the data directory. It is a
  bounded, checksummed ring (newest 8) of records bound to the consensus context,
  so a record from another genesis can never halt this node. Each record carries
  `height`, `local_hash`, `incoming_hash`, `local_view`, `incoming_view` and
  `detected_unix`. A repeated announcement of the same contradiction is a no-op,
  so gossiped copies cannot fill the ring.
- The node log line `durable fork incident recorded; signing and readiness stay
  halted` carries the height, local/incoming views and the incident count.
- On every boot the node reads the newest incident and re-halts **before** it
  signs its first view. A restart does not clear the halt.

## Investigate

1. Confirm the symptom: `/health/ready` is 503, the driver is halted, and the log
   names a fork incident (not a load error — that is
   [ledger-repair.md](ledger-repair.md)).
2. Read the incident fields from the log: the height, the local finalized hash
   and the incoming certified hash, and both views. The incoming certificate is
   authenticated (quorum signatures, genesis context) — junk evidence is rejected
   before it can halt, so a halt means a real quorum signed a contradiction.
3. Determine which history is canonical. Compare the local and incoming hashes
   at that height against the rest of the network's finalized history. The
   append-only boundary means the network's committed history is the reference;
   a local branch that diverges from it is the side that must yield.
4. Look for the cause:
   - **Equivocation:** two nodes signing with one producer key (a copied
     identity) can finalize competing blocks. Check for a second producer with
     the same key ([key-rotation.md](key-rotation.md)).
   - **A genuine fork:** a quorum finalized a different branch. The conflicting
     certificate is evidence; preserve it.
   - **Stale or foreign evidence:** the ring is genesis-bound, so a foreign
     genesis cannot halt this node; if you suspect it, verify the consensus
     context rather than deleting the file.
5. Preserve the evidence: copy `fork-incidents.bin`, the journal and the relevant
   logs before any change. Do not delete the incident file to force a start.

## Resolve

Clearing the halt is an **operator action after review, not a code path**. There
is no automatic recovery and no command that silently resumes signing. Resolution
means establishing which finalized history is canonical and bringing this node
onto it:

- If the local node diverged from the network's finalized history, recover it
  onto the canonical chain (resync/restore per
  [ledger-repair.md](ledger-repair.md) and [certified-sync.md](certified-sync.md))
  rather than forcing it to sign its own branch.
- If equivocation caused the fork, stop the duplicate producer and secure the
  identity first; resuming with two signers will halt again.
- Only after the underlying contradiction is resolved should the incident state
  be cleared and the node restarted. Record the decision and the evidence in the
  incident log ([incident-checklist.md](incident-checklist.md)).

Do not treat a halt as a transient fault to retry away. The halt exists so a
contradiction of finality is auditable instead of becoming a silent divergence
or a data rewrite.