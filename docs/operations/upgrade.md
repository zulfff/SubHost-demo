# Upgrade, migration and rollback

Before replacing a binary or image, verify its Sigstore identity and checksum
as described in [release.md](../release.md), read its changelog, and confirm that
all validators will use compatible genesis/crypto/wire/journal rules. Save the
old verified binary/image digest. Stop admission/production cleanly before
backups, and preserve the complete data directory, signing journal, genesis,
producer identity and certificate archive together.

## Supported format boundaries

The current release code supports the segmented ledger under `ledger/`, whose
manifest is version 2 and includes `base_height`; `subhost db migrate` converts
the older version-2 single-file `node-state.bin` to the segmented v3 storage
layout. These numbers describe different envelopes. Never edit a manifest
version or rewrite block/hash bytes by hand. Unknown versions are refused.
Migration preserves a backup and validates the migrated chain; review the
[backup/restore runbook](backup-restore.md) and run it offline:

```sh
subhost db migrate --data-dir /path/to/stopped-data
```

Ledger migration does not upgrade cryptographic proofs or signing obligations.
Phased consensus uses `SUBVOTE2`; legacy `SUBVOTE1` is incompatible. The corrected
RFC 9380 BLS suite refuses legacy proofs/signatures because the old construction
was forgeable. Archive v2 binds actual independent views/final Commit certificates
to exact genesis context. Use fresh corrected genesis proofs and coordinated
fresh signing/history directories for that protocol change. Preserve old data
and keys for investigation; do not reactivate a legacy verifier or invent missing
historical quorum evidence. Subsequent fee/header/keystore changes must ship
their own agreed version/migration contract and failure tests.

## Restart verification

After a compatible upgrade, start an observer first against its preserved data.
Confirm the restored tip/hash/state root, receipt availability, genesis identity,
archive coverage and health/metrics. A validator must reconcile its newest signing
journal with the ledger before it can sign. Test peers at the same protocol
revision and verify actual quorum commits and catch-up before resuming admission.
Do not treat responsive RPC, peer connection count or a copied snapshot as proof
of canonical finality or a completed catch-up.

## Rollback boundaries

Binary rollback is supported only while the previous binary understands all
on-disk formats, genesis rules and signatures written by the new one. If startup
refuses a newer format, stop and investigate; do not change version markers or
silently select an older snapshot. Once a validator has published votes at the
new revision, restoring an older journal can erase signing obligations and cause
equivocation. Never replace the newest journal with an older backup to make
startup succeed. A ledger behind newer voting state requires the reviewed
recovery/catch-up procedure, preserving that newer journal.

Restore only a coherent complete backup under the protocol's recovery rules;
keep failed data separate for incident analysis. Fresh-genesis incompatible
upgrades cannot be rolled back onto old signing/history data by automatic
conversion. If a release cannot safely consume the newest data, keep services
stopped and follow the incident process rather than attempting repair by guess.
