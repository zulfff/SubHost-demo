# Ledger diagnosis and recovery

The ledger is append-only. There is no command that rewrites finalized history
in place, and no reorg path: `subhost_reorg_total` stays registered at 0. A
"repair" here means diagnosing why a node will not load and restoring a coherent
state from backup, not performing surgery on committed blocks.

## Stop first

Stop the node and wait for it to exit before inspecting or copying the data
directory (single writer; see [start-stop.md](start-stop.md)). Work on a copy
when diagnosing, so an exploratory command cannot damage the only good state.

## What the loader expects

- `ledger/manifest.json` is authoritative once it exists. The loader does **not**
  fall back to an older legacy `node-state.bin` if committed segmented data is
  damaged. A damaged manifest/segment is a hard load failure, not a silent
  downgrade.
- A snapshot holds account state but no receipt index; retained
  `ledger/segments/` are required to validate commitments and rebuild receipts.
  A snapshot alone cannot restore a node.
- An older directory may hold `node-state.bin` (version 2) or
  `node-state.bin.v2-backup`. Keep these; they are the pre-segmentation state.

## Diagnose

1. Read the node's load error verbatim. It names the failing component
   (manifest, snapshot, segment, commitment, receipt count).
2. Confirm the set is complete: `genesis.json`, `ledger/manifest.json`, the
   snapshots and `ledger/segments/` together, with names/permissions intact.
3. Check chain ID matches the genesis (`--chain-id`); a mismatch is a refusal,
   not corruption.
4. If the directory is pre-segmentation, `subhost db migrate --data-dir <dir>
   --chain-id <id>` validates the version-2 file and migrates it, keeping
   `node-state.bin.v2-backup`. On an already-segmented ledger it is a no-op.

## Recover

- **Coherent backup available:** restore the whole data directory per
  [backup-restore.md](backup-restore.md): genesis, manifest, snapshots,
  segments, journal and producer identity together. A restore must not run a
  second producer with the same identity alongside the original.
- **No backup, damaged finalized data:** there is no supported in-place repair.
  The honest options are to resync from peers if the retained window and
  certified history allow catch-up (see [certified-sync.md](certified-sync.md)),
  or to re-genesis a new chain. Do not hand-edit segments or the manifest to
  force a load; that produces a ledger that fails validation or, worse, one
  that loads but diverges from the network's finalized history.
- **Conflicting final evidence (not corruption):** if the node halted on a fork
  incident rather than a load error, this is an accountability event, not a
  repair case. Follow [incident-investigation.md](incident-investigation.md).

## Prune is maintenance, not repair

`subhost db prune --data-dir <dir> --chain-id <id> --keep-segments <N>` drops the
oldest segments, keeping the newest `N`. The newest snapshot stays the replay
anchor, so it refuses when the retained history would no longer contain the
block at its height. Pruned blocks and receipts are gone permanently: the node
stops serving those heights. Prune to bound disk growth on a healthy node; never
prune to "fix" a load failure, and never prune below the snapshot anchor.

### Automatic retention (`--retention-blocks`, Q-036)

`subhost node --retention-blocks <N>` configures automatic height-based
retention: once the trigger slice lands, the node prunes after a successful
commit when `tip - base_height > N`, without an operator command. Unset = off
(the default; today's offline-only behaviour). The flag is **accepted now but
inert** until the node-side trigger lands: `subhost db prune` remains the only
prune path until then.

Semantics once active (agreed in Q-036):

- **The newest snapshot is the hard floor.** Automatic retention never prunes
  below the newest snapshot anchor, so the real bound is *snapshot cadence + N*,
  not exactly `N`. A prune refused by the floor is a logged no-op, never an
  error that halts the node.
- **Failure never halts.** A failed prune is logged and counted, never
  propagated, and never fails the block commit. `subhost_ledger_active_bytes`
  is the operator signal that retention is not keeping up.
- **Any `N >= 1` is allowed; below 4096 the node warns at boot.** Aggressive
  values make the node a degraded sync source: heights it no longer retains
  are unavailable to peers (never an error, see
  [certified-sync.md](certified-sync.md)), and historical certificates at
  pruned heights are unverifiable, not a fork. A small testnet genesis node may
  run lean; a public serving node should keep at least the certified-archive
  window.
- Retention applies to validator/observer chains where committed = final.
  Explicit `--single-writer` demo mode has committed ≠ final; automatic
  retention there is a demo-only operation.

`subhost db prune --keep-segments` stays the offline tool (segment-count unit,
process stopped); `--retention-blocks` is the online height policy. They share
the same snapshot floor.

## After recovery

Start the node and confirm `/health/ready` returns 200 and the committed height
matches the network. Record what failed and what was restored in the incident
log ([incident-checklist.md](incident-checklist.md)); a manual ledger repair is
a reportable event for the public testnet soak (S12.3 requires none).