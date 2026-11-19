# ADR-007 — Native certified candidate selection and finalized-fork accountability

- **Status:** Accepted with S3.1/S3.2 implementation (Track A)
- **Date:** 2026-11-19
- **Supersedes:** nothing; corrects the S3 "rollback a finalized branch" plan
  recorded in earlier roadmap text
- **Related:** Q-013 (agreed), Q-027 (agreed), Q-028, ADR-006 (certified-history
  relay), ADR-008 (phased consensus), ADR-013 (`subhost_reorg_total` stays 0)

## Context

Final Commit QCs establish an **append-only** canonical boundary. The original
S3 wording ("choose a different finalized branch by view/weight and roll back
three finalized blocks") contradicts that boundary and the storage contract:
the ledger cannot rewrite committed history without a separate speculative
execution layer.

At `982d3e6` the driver had no candidate buffer and no fork path at all:

- `receive_commit` (`crates/subhost-consensus/src/driver.rs`) accepts only one
  H+1 block extending the tip, with `qc.view >= initial_view()`.
- The node `BlockCommit` handler (`crates/subhost-node/src/lib.rs`) silently
  drops `block.height <= driver.height()`.
- `commit_qc` requires a strictly higher committed view, so a conflicting final
  certificate is rejected with no durable record anywhere.

Live peers therefore cannot distinguish three very different events: a repeated
certificate for the block we already finalized, a genuine conflicting final
certificate, and unauthenticated junk.

## Decision

`subhost-consensus` owns the pure classification and the signing-stop signal.
The node owns storage, transport, persistence and readiness wiring.

### Classification (`ConsensusDriver::classify_final_evidence`)

The certificate's signatures are verified first. Unauthenticated or
block/QC-mismatched evidence is a plain error and **must never** halt a node.
Then, against local finality `(height, tip)`:

| Evidence | Meaning | Obligation |
| --- | --- | --- |
| `Duplicate { height }` | same hash at the finalized height | none; a repeated QC is not a fork |
| `Next` | H+1 child of the tip, `qc.view >= initial_view()` | apply through the existing `receive_commit` path after execution checks |
| `Conflicting { height }` | different hash at a finalized height, a certified child that does not extend the tip, or a certificate behind committed finality | persist an incident, halt signing and readiness |
| `Historical { height, hash }` | below the tip (the driver keeps no history) | compare with the ledger hash: equal is `Duplicate`, different is `Conflicting` |
| `MissingAncestors { from, to }` | certified block past a gap | buffer the candidate and request the missing certified range |

`Next` keeps the prepared-lock rule unchanged: a replacement proposal is still
governed by the highest prepared certificate in the authenticated timeout
certificate. Candidate buffering is bounded and node-owned; the classification
is the only consensus primitive, so there is no second copy of chain history in
the driver.

### Signing stop (`HaltReason`, `halted()`, `halt()`)

- The reason is sticky for the process; the first reason wins.
- Every signature path (proposal, Prepare/Commit vote, view-change report) goes
  through `sign()`, which refuses while halted. Existing vote/view state is
  preserved so the node can report it.
- Certifying and applying already-finalized history still works: S3.2 stops
  signing and readiness, it never freezes or rewrites the append-only ledger.
- The reason is **not** serialized into `DriverCheckpoint`. A persisted incident
  is node-owned; the node must re-halt on startup from its own record, so the
  `SUBVOTE2` journal format is unchanged and old journals stay readable.

### What this decision refuses

- No finalized rollback and no reorg API. `subhost_reorg_total` stays registered
  at 0 (ADR-013). A speculative reorg needs its own storage/execution contract
  and cannot be advertised as finalized reorg.
- No automatic clearing of a halt: that is an operator action after review.

## Consequences

- Honest replicas converge on one append-only history; a genuine fork is an
  auditable incident instead of divergence or a silent data rewrite.
- Junk evidence cannot be used to deny service: only a fully authenticated
  conflicting certificate can halt signing.
- The relay (ADR-006) and certified-history sync keep the same BlockCommit/QC
  contract; no wire change.
- Remaining S3 work (bounded candidate storage, `GetBlocks` range serving, node
  incident persistence, readiness gating) belongs to Track A's S3.3/S3.4 and
  amends this ADR rather than replacing it.

## Evidence

- `crates/subhost-consensus/src/driver.rs` at `982d3e6`:
  `receive_commit`, `ensure_extends_tip`, `initial_view`,
  `validate_commit_certificate`, `sign`; new in this slice:
  `classify_final_evidence`, `FinalEvidence`, `HaltReason`, `halt`/`halted`.
- `crates/subhost-consensus/src/driver/tests.rs`: duplicate vs conflicting vs
  historical vs gap vs non-extending child, junk evidence, sticky halt, and
  halt-not-serialized regressions.
- `crates/subhost-node/src/lib.rs`: per-item `BlockCommit` ordering and journal
  cadence that S3.3 follows (Q-027).
- `docs/agents/questions/agreed/Q-013-native-certified-sync.md` resolution;
  `ROADMAP.md` S3 contract correction.
## Amendment — S3.3 native certified history sync (Track A)

*Status: forward contract. S3.3/S3.4 are not merged into `main` yet, so the
node/archive files cited below live on raka's branch, not in landed code. The
decision itself is agreed (Q-027, Q-028); this label is removed when the slice
lands.*

This section records the transport, archive and node-side catching-up decision
that S3.1/S3.2 above deliberately left to S3.3. It amends, and does not replace,
the classification and halt decision.

### Wire (`subhost-network`)

- `NetworkMessage::GetBlocks { from, to }` and `NetworkMessage::Blocks { items }`
  are appended **after** `TimeoutCertificate`, so every existing bincode/JSON
  discriminant is unchanged.
- `from >= 1`, `to >= from`, and the requested span is at most
  `MAX_SYNC_ITEMS = 128` heights. A response carries at most 128 non-empty items
  and stays inside the existing `MAX_TRANSMIT_BYTES` / `MAX_WIRE_BYTES` caps. An
  inverted, empty, or oversized request is rejected, never relayed.
- `Blocks` items are length-prefixed `(block, QC)` pairs in ascending height
  order, so a receiver can reject a malformed item without guessing item bounds.

### Archive (`crates/subhost-node/src/certified_history.rs`)

- One `certified-history.bin` per data directory, opened only when the node owns
  a voting journal, bound to the consensus context hash so a file from another
  genesis is refused at startup.
- Frames are `height | block length | QC length | block | QC | FNV-1a checksum`.
  A torn or corrupt suffix is dropped and the retained window is rewritten. A
  retried commit for the already-archived tip is a no-op; conflicting data for an
  archived height is an error.
- The archive keeps the newest 4096 items and rewrites down to 3584 when it
  passes that bound, so serving stays bounded and pruning is amortised.
- Items are appended **after** the certificate verifies and **before** the
  ledger append. A crash between the two leaves an archived certificate that is
  not served until the ledger commits the same block hash.

### Serving

- `GetBlocks` is answered from the archive only, ascending, clamped to the local
  committed height, and truncated to a valid prefix when the range would exceed
  the wire budget.
- An item leaves only when `RpcState::block_at_height(height)` holds a block with
  the same hash. The node never invents a certificate and never serves a block it
  did not execute.
- Requests below the retained window are naturally short; there is no snapshot
  or deep-history protocol in this stage.

### Catch-up

- A `BlockCommit` at exactly `tip + 1` keeps the existing single-item path. A
  higher height is a gap: the pump records the announced peer tip, sets a
  node-owned `catching_up` flag, and requests
  `[tip + 1, min(peer_tip, tip + 128)]`.
- `Blocks` items are adopted one at a time on a **driver clone**, in the live
  `BlockCommit` order: `receive_commit` → `validate_certified_block` →
  `journal.prepare_commit` → archive append → `commit_qc` → swap the driver →
  `journal.save`. The journal is saved per commit.
- A non-contiguous or invalid item stops the run; everything already committed
  stays, so the tip never skips a missing certificate. A 500 ms pump ticker
  retries an unanswered window after one second and requests the next window
  once the current one is adopted.
- `catching_up` suppresses `produce_blocks` (proposal, vote, and timeout signing)
  until the announced peer tip is reached. Like `HaltReason`, the flag is
  node-owned; no `subhost-consensus` change is required.
- Both signals are checked in the same place in the produce loop: a halted
  driver never signs even when caught up, and a catching-up node never signs even
  though it is not halted.

### Limits

- A node further behind than the retained window cannot backfill from peers.
  Snapshots or a genesis-start archive are the follow-up; until then the node
  stays caught-up-suppressed rather than signing behind a range it cannot verify.
- `catching_up` is only set by a certified announcement, so a restart can sign
  for one view before the first peer tip arrives. No quorum participates in that
  height, so it cannot finalize a competing block, but this is not a substitute
  for snapshot bootstrap.
- Sync only extends the committed chain; it never rolls finalized history back.

### Evidence for the amendment

- `docs/agents/questions/agreed/Q-013-native-certified-sync.md` — bounded
  request/response, 4096 retention, serve only ledger-matching history.
- `docs/agents/replies/Q-027-track-a.md` — consecutive `receive_commit` is the
  primitive; live per-item ordering on a driver clone; per-commit journal
  cadence; node-owned signing suppression.
- https://github.com/cometbft/cometbft/blob/main/blocksync/reactor.go — a
  fetched commit is verified before execution/persistence (re-checked 2026-11-19).
- `crates/subhost-node/src/basic_tests.rs` — a lagging validator backfills three
  certified blocks and restarts at the synced tip; range request/serve bounds.
- `crates/subhost-node/src/certified_history.rs` — reopen, torn tail, genesis
  binding, retention and wire-item tests.
