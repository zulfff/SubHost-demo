# Incident checklist

A short, ordered checklist for any node incident. Work top to bottom; stop the
node before touching its data directory. Record everything in the incident log
as you go — the public testnet soak (S12.3) requires a published incident log
and no manual ledger repair.

## 1. Stabilize

- [ ] Stop admission/production cleanly (`Ctrl-C`/`SIGTERM` or the service
      manager); wait for exit. See [start-stop.md](start-stop.md).
- [ ] Do **not** delete `fork-incidents.bin`, the journal, or segments to force a
      restart. A restart replays and re-halts; it is not a recovery path.
- [ ] Preserve evidence: copy the data directory (or at least the journal,
      `fork-incidents.bin`, manifest and logs) before any change.

## 2. Classify

- [ ] **Halt / `/health/ready` 503 with a fork-incident log line** →
      conflicting final evidence. Go to
      [incident-investigation.md](incident-investigation.md). Not a reorg.
- [ ] **Load failure (manifest/snapshot/segment/commitment error)** →
      [ledger-repair.md](ledger-repair.md). Restore a coherent backup; no
      in-place rewrite of finalized history.
- [ ] **Height stall, no halt** → check peers, quorum and catch-up
      ([certified-sync.md](certified-sync.md)); use the alerts in
      [alerts.md](alerts.md). A `catching_up` node suppressing production is
      expected, not a fault.
- [ ] **Suspected key compromise / equivocation** →
      [key-rotation.md](key-rotation.md); stop any duplicate producer.

## 3. Diagnose

- [ ] Capture the verbatim error/log line and the committed vs finalized height.
- [ ] Confirm chain ID and genesis match the network.
- [ ] For a halt: record height, local/incoming hashes and views; determine the
      canonical history; look for a second signer with the same key.
- [ ] For disk growth: check `subhost_ledger_active_bytes` and prune policy
      ([ledger-repair.md](ledger-repair.md)); prune is maintenance, not repair.

## 4. Resolve

- [ ] Apply the runbook for the class. Clearing a halt is an operator decision
      after review, never an automatic retry.
- [ ] Restore/resync onto the canonical finalized history; never roll finalized
      history back or hand-edit segments.
- [ ] Restart and confirm `/health/ready` 200 and height matches the network.

## 5. Record and follow up

- [ ] Log: time, class, evidence (hashes/views/errors), action taken, outcome.
- [ ] Note any manual ledger repair explicitly (a soak-blocking event).
- [ ] File a follow-up for the root cause (equivocation, fork, corruption,
      capacity) and link the preserved evidence.
- [ ] If a metric or health predicate was unavailable during the incident, note
      it — several runtime gauges are still pending Track A wiring
      ([observability.md](observability.md)).