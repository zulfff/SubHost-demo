# Security review: PR #57 — S3.3 native certified sync + fork accountability

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-19.
PR head: `s3.3/native-sync` (fetched as `pr-57-review`), base `main`.
Design: ADR-007 (native certified candidate selection + finalized-fork
accountability), Q-013/Q-027/Q-028, `docs/operations/certified-sync.md`.
CI at review: Test/Clippy/Format/Supply chain/Release/MSRV pass; "Image build and
scan" pending.

## Assessment

The design is sound and most security-critical paths are correctly fail-closed.
**One Medium finding (R-57-01) should be confirmed/addressed before merge**: a
pruning node can be halted by legitimate historical certificates for pruned
heights. The rest are informational.

## What was verified (positive)

- **Junk cannot halt.** `ConsensusDriver::classify_final_evidence` checks
  `qc.block_hash == block.hash()` and runs `validate_commit_certificate`
  (signature verification) *before* any classification. Unauthenticated or
  mismatched evidence returns `Err`, never an incident. Only an authenticated
  conflicting final certificate reaches `record_conflict` → `halt`. Matches
  ADR-007's "junk evidence cannot be used to deny service".
- **No reorg / rollback.** `Conflicting` persists an incident and halts signing
  and readiness; the append-only ledger is never rewritten. `subhost_reorg_total`
  stays 0 (ADR-013). Halt is sticky; the node re-halts on boot from
  `ForkLog::latest` before its first tick.
- **Served history is ledger-anchored.** `serve_certified_range` clamps `to` to
  the local head, and for each item requires `state.block_at_height(height)` to
  exist and `block.hash() == committed.hash()` before it goes on the wire; it
  truncates to a valid prefix and re-validates the `Blocks` message. It never
  invents history.
- **Bounded catch-up.** `MAX_SYNC_ITEMS = 128` caps both the `GetBlocks` span and
  the `Blocks` item count; `GetBlocks` requires `from >= 1`, `to >= from`;
  `Blocks` rejects empty items and checks total-byte overflow; responses also
  respect a payload budget. The node archive retains the newest 4096 items with
  amortised pruning; the candidate buffer is bounded (64). A 30 s no-progress
  stall releases signing so a silent peer cannot pin a validator mute.
- **Durable incident ring.** `fork-incidents.bin` is genesis-bound, checksummed,
  bounded to 8 records, dedups repeated contradictions, reconciles a torn tail by
  rewriting the valid prefix, and refuses a foreign genesis. Archive
  (`certified-history.bin`) likewise genesis-bound, ascending, checksummed,
  retry-safe, and refuses a conflicting tip certificate.
- **Request originates from authenticated evidence.** `MissingAncestors` is only
  produced after the certificate validates, so a `GetBlocks` range is never
  triggered by unauthenticated junk.

## Findings

### R-57-01 (Medium) — pruned heights are treated as a fork, halting pruning nodes

Code path (verified):

1. `RpcState::block_at_height` returns `None` for any height below `base_height`
   (pruned): `height.checked_sub(self.base_height)?.checked_sub(1)?` then a read
   of the retained contiguous window. Comment: "A pruned height has no block to
   serve."
2. `route_final_evidence`, `FinalEvidence::Historical { height, hash }` branch:
   ```rust
   let local = state.block_at_height(height).map(|block| block.hash());
   if local == Some(hash) { return Ok(None); }
   record_conflict(state, driver, height, hash, &qc, commit)?;  // halts
   ```
   When the height is pruned, `local == None`, so `None != Some(hash)` and the
   node records a conflict and halts — even though the incoming certificate is
   the *legitimate canonical* block at that height.
3. The `NetworkMessage::BlockCommit` handler calls `route_final_evidence`
   directly with no `base_height`/pruned-height guard.

Reachability (likely; confirm with a test): a node running with pruning
(`--keep-segments`, `base_height > 0`) that receives an authenticated historical
certificate for a pruned height — e.g. joining a `sync-relay` that replays history
from genesis, or a peer re-gossiping old canonical commits — classifies it as
`Historical` (height < driver.height), cannot find the pruned block, and halts
signing/readiness and writes a false fork incident. No Byzantine quorum is
needed: the certificates are the real canonical ones.

This contradicts ADR-007's `Historical` semantics ("compare with the ledger hash:
equal is Duplicate, different is Conflicting"): a pruned height has no available
ledger hash, and "absent because pruned" must not be treated as "different".

Impact: denial of service against pruning validators (sticky halt across
restarts) plus pollution of the incident evidence log with false conflicts.

Recommendation: make the `Historical` comparison pruning-aware. If
`height < state.base_height()` (or the block is otherwise unavailable but below
the retained head), do not halt — treat as unverifiable-but-not-conflicting
(ignore / Duplicate-like), or compare against a retained hash/commitment index
that survives pruning. Halt should require positive proof of conflict: a
*retained* local hash that differs from the authenticated incoming hash. Add a
regression test: a pruning node fed an authenticated canonical certificate for a
pruned height must not halt and must not record an incident.

Confidence: code path verified by reading `state.rs`, `node/lib.rs`
(`route_final_evidence`, `BlockCommit` handler) and `driver.rs`
(`classify_final_evidence`) at the PR head; reachability via history replay is
reasoned and should be confirmed with the suggested test.

### R-57-02 (Info) — no per-peer rate limiting on GetBlocks (S7)

`GetBlocks` handling has no per-peer rate limit (peer-abuse controls remain S7).
Responses are bounded (128 items + payload budget + ledger match), so wire
amplification is limited, but each request scans the retained archive
(O(retention)) and does ledger lookups; repeated requests are a CPU cost. Known
S7 gap, not a blocker for this PR.

### R-57-03 (Info) — non-cryptographic frame checksums

Archive and incident frames use FNV-1a checksums. These detect torn/corrupt
writes, not adversarial tampering; integrity against an attacker who can write
the data directory relies on filesystem ownership, the same trust boundary as
the voting journal and ledger. Acceptable and consistent with the rest of the
node; noted for completeness.

## Coordination note (threat model)

This PR implements request/response catch-up and finalized-fork incident
handling, which the merged threat-model PR #58 currently lists under "Not
implemented". PR #57 carries its own `docs/security/threat-model.md` update; since
#58 merged first, #57 needs a rebase and the two threat-model edits must be
reconciled so the sync/fork claims land consistently (likely a merge conflict in
that file). Flagging for raka/tech-lead.

## Evidence

- Read at PR head `pr-57-review`: `crates/subhost-consensus/src/driver.rs`
  (`classify_final_evidence`, `HaltReason`), `crates/subhost-node/src/lib.rs`
  (`route_final_evidence`, `record_conflict`, `serve_certified_range`,
  `BlockCommit` handler, sync pump), `crates/subhost-node/src/certified_history.rs`
  (full), `crates/subhost-node/src/fork_incident.rs` (full),
  `crates/subhost-network/src/lib.rs` (`GetBlocks`/`Blocks` validation),
  `crates/subhost-rpc/src/state.rs` (`block_at_height`, `base_height`).
- Design: ADR-007, `docs/operations/certified-sync.md`.
- Gates: four-gate baseline on `fa87215` green
  (`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`); PR CI Test pass.
---

## R-57-01 fix verification (PR #67, s3.4/pruned-history-and-backup)

**Resolved.** `route_final_evidence` now calls `historical_meaning(base_height,
height, retained, incoming)`:
- `height <= base_height` → `Unverifiable` (pruned; no halt).
- retained `Some(equal)` → `Duplicate`; retained `Some(different)` → `Conflict`
  (the only case that records an incident and halts).
- retained `None` above base → `Unverifiable` (defensive; no halt).

Only a **retained differing hash** is accountable evidence; a canonical
certificate replayed at a pruned height can neither be confirmed nor refuted and
never halts. This closes the false-halt/DoS vector.

Regression test `a_historical_certificate_at_a_pruned_height_is_unverifiable_not_a_fork`
pins every decision case (at/below base, retained equal/different, above-base
absent). The test honestly notes it pins the decision function + router guard
rather than a >64 MiB pruned fixture (the node exposes no prune seam); an
end-to-end pruned-node integration test would be stronger but the security
decision is fully covered.

Bonus hardening in the same PR: `check_archive_matches_ledger` refuses a
non-coherent certified-archive restore at boot (archive lead > 1, undecodable
bytes, or a non-child of the restored tip), with tests. `ForkIncident` documents
`Hash::ZERO` as a "no retained block" sentinel, never a real hash.

Verdict: approve; R-57-01 closed pending merge of #67.

### Decision-table audit: no new halt vector (tech-lead re-review request)

`historical_meaning(base_height, height, retained, incoming)` is exhaustive:

| Condition | Meaning | Halts? |
| --- | --- | --- |
| `height <= base_height` (pruned) | Unverifiable | No |
| `height > base`, retained `Some(== incoming)` | Duplicate | No |
| `height > base`, retained `Some(!= incoming)` | Conflict | Yes (authenticated + retained differ = genuine fork) |
| `height > base`, retained `None` (defensive) | Unverifiable | No |

Boundary is correct: `base_height` is the last pruned height and retained history
starts at `base_height + 1`, matching `block_at_height`'s `height - base - 1`
index. The only halt paths in the PR are: boot re-halt from a durable incident
(local), `Historical -> Conflict`, and `Conflicting`. All require a
signature-verified certificate (`classify_final_evidence` still verifies
signatures first); the Historical path additionally requires a retained differing
hash. The change strictly removes the pruned false-halt vector and adds none.

`check_archive_matches_ledger` is a **local boot-time integrity check**, not a
signing halt and not remotely triggerable: it refuses to start on a mixed
archive/ledger restore (archive lead > 1, or a +1 item that is undecodable or not
a child of the restored tip), and accepts empty/level/normal-crash (+1 real child)
states. The error directs the operator to restore archive, ledger, journal and
incident log from the same stopped node. No new remote denial-of-service path.
