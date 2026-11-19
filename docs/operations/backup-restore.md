# Offline ledger backup and restore

Stop the node before copying, migrating, pruning or restoring its data directory.
The storage API assumes one writer; copying files while the manifest changes can
produce an incoherent backup. For a node started in a terminal, use `Ctrl-C` and
wait for the process to exit. For a managed service, stop it through its manager.

## What a backup contains

Copy the whole data directory, including `genesis.json`, `ledger/manifest.json`,
the snapshots and `ledger/segments/`. Keep their names and permissions together.
A snapshot contains account state, but no receipt index; retained segments are
required to validate commitments and rebuild receipts. A snapshot alone cannot
restore a node. A pruned backup contains only its retained history.

A validator directory also holds node-owned consensus files that must come from
the same stopped node as the ledger:

| File | What it proves |
| --- | --- |
| `consensus-state.bin` | The newest signing journal: committed height/view and quorum certificates |
| `certified-history.bin` | The bounded `(block, QC)` archive this node serves to lagging peers |
| `fork-incidents.bin` | Any durable finality contradiction; its presence re-halts the node at boot |
| `producer.key` | The validator identity, when kept in the data directory |

Do not mix these files with a ledger from a different point. The archive may lead
the restored ledger by exactly one item (a certificate is archived before the
ledger commit, and the retry is idempotent); the node refuses to boot when the
archive is further ahead, or when its newest item does not extend the restored
ledger tip, because every later commit would fail. A `fork-incidents.bin` carried
over from a newer backup re-halts the node even if the restored ledger predates
the incident: preserve it as evidence and resolve the incident deliberately rather
than deleting the file.

An older directory may contain `node-state.bin` (format version 2) or
`node-state.bin.v2-backup`. Keep these files too. Once the manifest exists, it is
authoritative; the loader does not fall back to an older legacy file if committed
segmented data is damaged.

If `producer.key` is inside the directory, the copy includes the validator's
identity. Store the backup with restricted access and preserve `0600` key
permissions. Keys configured outside the data directory, including wallets under
`SUBHOST_HOME`, need a separate protected backup. A restore must not run a second
producer with the same identity alongside the original node.

## Make and verify a backup

Set these paths to the stopped node's directory and a new backup directory:

```bash
export SUBHOST_DATA_DIR=/srv/subhost/data
export SUBHOST_BACKUP_DIR=/srv/subhost/backup-2026-11-18
export SUBHOST_CHAIN_ID=1
mkdir -m 700 "$SUBHOST_BACKUP_DIR"
cp -a "$SUBHOST_DATA_DIR/." "$SUBHOST_BACKUP_DIR/"
subhost db migrate --data-dir "$SUBHOST_BACKUP_DIR" --chain-id "$SUBHOST_CHAIN_ID"
```

For segmented data, `db migrate` performs full offline validation and prints
`is already segmented; nothing to do` only when validation succeeds. It does not
rewrite an already-segmented ledger. For a version 2 backup it converts and
verifies the copied legacy ledger, preserving `node-state.bin.v2-backup`. A fresh
directory reports nothing to migrate; that message does not prove that any
history was backed up.

Record the node's last observed height and block hash with the backup so the
restored node can be compared against the expected chain. Resume the original
node after the copy has completed and the backup has passed validation.

## Restore into a new directory

Stop the original node and keep its damaged directory for investigation. Restore
into a new empty directory so unrelated files cannot be mixed with the backup:

```bash
export SUBHOST_RESTORE_DIR=/srv/subhost/restored-2026-11-18
mkdir -m 700 "$SUBHOST_RESTORE_DIR"
cp -a "$SUBHOST_BACKUP_DIR/." "$SUBHOST_RESTORE_DIR/"
subhost db migrate --data-dir "$SUBHOST_RESTORE_DIR" --chain-id "$SUBHOST_CHAIN_ID"
```

After validation, start the node using the restored directory and the intended
genesis/key configuration. For an unsigned single-node demo, an isolated loopback
listener is sufficient:

```bash
subhost node --data-dir "$SUBHOST_RESTORE_DIR" --listen 127.0.0.1:18545
```

In a second terminal, inspect the restored head:

```bash
subhost query chain --rpc-url http://127.0.0.1:18545
subhost query block --rpc-url http://127.0.0.1:18545
```

Compare the height and hash with the backup record before directing clients to
the restored node. Validator deployments also need their configured producer key,
validator mode and authenticated listener options. Restore those deployment
settings through the same mechanism used for the original node.

## Interrupted writes and migrations

The manifest is the commit point. Bytes after a segment's committed length and
segment/snapshot files absent from the manifest are uncommitted. Startup ignores
them; a successful retry truncates the uncommitted tail, replaces orphan files
as needed and cleans up stale files after committing the new manifest.

If snapshot writing fails after segment appends, the previous manifest, snapshot,
state and receipts remain authoritative. Fix the reported filesystem problem
and retry; the next commit must still extend the committed head.

If migration fails before the manifest commits, `node-state.bin` remains the
source of truth. Fix the filesystem problem and rerun `db migrate` with the same
directory and chain ID. If interruption happens after manifest commit but before
legacy cleanup, rerunning validates the segmented ledger and reports that it is
already segmented. A leftover `node-state.bin` is preserved; the next successful
persist moves it to `node-state.bin.v2-backup`.

## Committed corruption

A missing segment, a segment shorter than the manifest's committed length, an
invalid checksum or a damaged manifest/snapshot is a validation error. An append
also refuses a shortened committed tail before modifying the file. Repeated
migration reports committed damage even when a valid legacy file remains.

Preserve the failed directory and error message, then restore a separately
validated coherent backup. Do not reduce manifest lengths, delete committed
segments or substitute a snapshot from another backup: those edits can silently
discard committed history or break its state and receipt commitments. This stage
provides no automatic rollback or repair of committed corruption.

A node with a genesis validator set can catch up over peer gossip: it requests
bounded retained certified ranges and verifies every certificate and state
transition before appending (S3.3, ADR-007). Restoring an older backup therefore
recovers the missing blocks when the peer set still retains them; a gap beyond the
peers' retained window still needs a snapshot. Fork choice and reorg remain
separate work, so a validator must have an agreed recovery procedure for the
network before it resumes producing. Startup still reads every retained segment,
so backup validation time grows with retained history.

## Verified failure coverage

`cargo test -p subhost-storage --locked` exercises interrupted record headers and
payloads, uncommitted complete records, new snapshot rename before manifest
commit, snapshot rename failures after append/rollover, damaged orphan files,
migration before/after commit and refusal of committed damage. These tests model
on-disk crash states and provoke a real filesystem rename failure; they do not
claim physical power-loss testing across filesystems or hardware.
