# Roadmap to a full release

Status: 2026-11-18 · baseline `main` = `6be9cc4` · S0.1 and S0.2 merged · S0.3 in review · `Q-001` agreed
This document is the work plan for turning the current pre-production workspace into
a releasable network. It is written so two agents can work at the same time without
editing the same files.

---

## 1. Where the code is today

| Area | Reality |
| --- | --- |
| Chain | Single-writer node. One submission seals one block with every executable pending transaction. |
| Block auth | `BlockHeader.validator` is `Address::ZERO` and `Block.signatures` is empty on produced blocks. `accept_block` does not check who produced a block. |
| Consensus | `subhost-consensus` has a tested in-process `2f+1` driver (`propose`, `receive_proposal`, `receive_vote`, commit). **No crate depends on it**; the node never calls it. |
| Networking | libp2p gossip publishes only locally sealed blocks. Messages for votes, DAG vertices and transactions are already defined but unused by the node. No block sync, peer scoring, per-peer limits, or tx gossip. |
| Storage | One `node-state.bin`, hard cap 64 MiB, full replay on every start, format v2. No segments, snapshots, pruning, or migration tool. |
| State | Balances + nonces + a Merkle root over sorted leaves. Transfers only; contract creation is refused. |
| RPC | 11 methods, no auth, no TLS, no rate limit. RPC counters in the metrics registry are never incremented. |
| Mempool | In memory only; fixed gas price 1. |
| IBC | State machine + single-key ed25519 light client. No validator-set rotation, no multisig QC, no non-membership proof. |
| Release | `0.1.0`, no tag, no release workflow, no signed binaries or SBOM. |

Measured baseline: 251 tests, clippy/rustfmt/rustdoc clean, `cargo deny` clean.

---

## 2. What "full release" means

v1.0 is done only when every line below is true. Anything that is not true is either
fixed or written down as an explicit, reviewed limitation.

1. **Authenticated block production.** Every block carries a signature that verifies
   against the active validator set; an unregistered producer is rejected.
2. **Consensus is live.** Blocks commit only after a `2f+1` quorum certificate. No
   direct "RPC submit → commit" path remains in a validator deployment.
3. **One chain under partition.** Fork choice plus block sync means two producers
   converge and an offline node catches up.
4. **Durable, bounded storage.** The ledger is segmented, has a snapshot path, and
   can be migrated and pruned without full replay of the whole chain.
5. **Safe network surface.** RPC/metrics/faucet have authentication, TLS or an
   explicitly documented reverse-proxy requirement, and rate limits.
6. **Observable.** Every RPC request, block, peer, and consensus step is counted or
   traced; alerts and runbooks exist.
7. **Reproducible release artifacts.** Tagged version, signed binaries, SBOM,
   scanned image, documented upgrade path.
8. **Audited and soaked.** External audit closed, four-validator public testnet ran
   for at least four weeks without a manual ledger repair.

---

## 3. Two-agent working agreement

### Tracks

| Track | Owns | Stages |
| --- | --- | --- |
| **A — Protocol** | `subhost-core`, `subhost-consensus`, `subhost-network` messages, node block/consensus path, RPC block path | S0, S1, S2, S3, S7 |
| **B — Platform** | `subhost-storage`, RPC surface/auth/methods, `subhost-mempool`, `subhost-cli`, `subhost-metrics`, `subhost-telemetry`, `explorer`, `subhost-faucet`, CI/Docker/release | S4, S5, S6, S8, S9 |

Track C (execution, IBC) is deliberately deferred to S10/S11 and goes to whichever
track is free. Do not start it before S3 lands.

### Communication is a hard gate

Both tracks run in the same working tree, so the checkout is the channel: questions
and replies are plain untracked files under `docs/agents/`, never a branch, a commit,
or a PR. The protocol is `docs/agents/README.md`; the daily rules are `AGENTS.md`.

- Questions live in `docs/agents/questions/open/` and replies in
  `docs/agents/replies/`. `ls docs/agents/questions/open/` is the inbox.
- **Before starting a task, check the inbox.** If an open question names your track
  and its `blocks:` field lists that task, you must write a reply first and wait for
  the author to move it to `agreed/` (or escalate it). Starting the task anyway is a
  process violation, not initiative.
- Research first: official docs, the crate repository, RustSec, RFCs. Cite the URL
  and the claim you verified in `## Evidence`. Do not ask what a document answers.
- Open a question proactively for any shared contract change, even when it does not
  block you (`blocks: none`, `review_by: <stage>`). Disagreement in writing is
  expected; silent compliance is not.
- Keep the exchange conversational: append `## Follow-up <date>` to the same reply
  file instead of opening a formal document per round. No PR is needed to talk.
- Unresolved disagreement escalates to the maintainer through
  `docs/agents/questions/escalated/`, and the blocked task stops until an ADR in
  `docs/agents/decisions/` records the decision. The ADR is the only piece of the
  discussion that gets committed, and it lands with the code change it governs.

`Q-001` (block signature scheme) is agreed; `ADR-001` is the contract for S1.

### Rules that keep you out of each other's diffs

1. **One stage = one branch = one PR.** Branch names: `s1/signed-blocks`,
   `s4/storage-scaling`, … Branch from `main` after rebasing.
2. **Never `git add -A`.** Stage explicit paths. Untracked audit/scratch files and
   the `docs/agents/` question and reply threads must never be swept into a commit.
3. **Respect the ownership matrix.** If a task needs a file owned by the other
   track, land a tiny "contract PR" that only adds the type/signature, then both
   sides build on it.
4. **Split `subhost-rpc/src/lib.rs` first (S0.1).** Until that lands, Track B must
   not edit it. After the split, Track A owns `block.rs`, Track B owns `methods.rs`
   and `auth.rs`, and `state.rs` changes need a contract commit.
5. **Shared files are additive only:** `Cargo.toml` (workspace dependency lines),
   `CHANGELOG.md` (new bullets at the top of `Added`/`Fixed`), `README.md`.
   Resolve the trivial conflict on rebase; never reformat the whole file.
6. **Rebase before every push.** `--force-with-lease` only on your own branches.
7. **CI must be green before merge.** The seven required jobs are the gate.
8. **Every PR that changes behaviour updates `CHANGELOG.md` and the relevant doc
   under `docs/`, and adds failure-path tests.**
9. **No `todo!`/`unimplemented!`/`dbg!`/`unsafe`.** Workspace lints deny them.
10. **Write the limitation down if you defer it.** Silent gaps are how this roadmap
    got its current list.

### Whenever a stage lands

Update the stage table in this file (`Status` column) in the same PR, so the other
agent always sees the real state.

---

## 4. Stage index

| Stage | Goal | Track | Depends on | Size |
| --- | --- | --- | --- | --- |
| S0 | Prepare `rpc` for parallel work | A | — | S |
| S1 | Signed blocks and validator identity | A | S0 | M |
| S2 | Wire the consensus driver into the node | A | S1 | L |
| S3 | Fork choice, reorg, and block sync | A | S2 | L |
| S4 | Segmented storage, snapshots, migration | B | — | L |
| S5 | RPC auth, TLS posture, rate limits, method gaps | B | S0 | M |
| S6 | Durable mempool and fee market | B | S0 | M |
| S7 | Network hardening: tx gossip, scoring, peer limits | A | S3 | M |
| S8 | Observability, health, runbooks | B | S0 | M |
| S9 | Release engineering | B | S5 | M |
| S10 | Execution layer (EVM or WASM) | C | S3 | XL |
| S11 | IBC production light client + relayer | C | — | L |
| S12 | External audit and public testnet soak | all | S1–S9 | XL |

Size is relative effort, not calendar time. S10 alone is larger than S0–S9 combined.

---

## 5. Stage details

### S0 — Prepare `subhost-rpc` for parallel work (Track A)

**S0.1 Split the crate.**
- `src/lib.rs` keeps re-exports and crate docs.
- `src/state.rs`: `RpcState`, `RpcError`, `BlockEvent`, locks, constructors.
- `src/block.rs`: `select_transactions`, `seal_block`, `persist`, `accept_block`,
  `submit_transaction`, `include_transaction`.
- `src/methods.rs`: `build_module` and every `register_method` closure.
- No behaviour change. Tests move with their code.
- Acceptance: `cargo test -p subhost-rpc` unchanged count, clippy clean.

**S0.2 Publish the contract types** (additive, `subhost-core`):
- `BLOCK_SIGNING_DOMAIN: &[u8]`, `Block::signing_payload() -> Vec<u8>` over
  `domain || chain_id || height || block_hash`.
- `ValidatorInfo` gains `proof_of_possession: Vec<u8>` with `#[serde(default)]`.
- Acceptance: unit tests for payload stability and domain separation.

**S0.3 Document the invariants** in `docs/architecture.md`: block lifecycle,
who may produce, what the ledger guarantees, which component owns which decision.

### S1 — Signed blocks and validator identity (Track A)

`Q-001` is agreed (see `docs/agents/decisions/ADR-001-block-signature.md`):
BLS12-381, a dedicated producer key file, and a mandatory proof of possession on its
own domain tag. S1.1 and S1.4 land together so a genesis file without real proofs
cannot load quietly.

**S1.1 Registry from genesis** (`subhost-consensus`)
- `ValidatorRegistry::from_genesis(&[ValidatorInfo]) -> Result<Self, ConsensusError>`
  that verifies each proof of possession and rejects duplicates/zero power.
- Error: `InvalidProofOfPossession(Address)`.

**S1.2 Sign on produce** (`subhost-rpc/src/block.rs`)
- `RpcState` gains an optional `BlockAuthority { validators, producer }`.
- `seal_block` sets `header.validator` and pushes one `ValidatorSignature` over
  `Block::signing_payload()` when an authority is configured.
- No authority configured = current single-node behaviour, unchanged for tests and
  `subhost bench`.

**S1.3 Verify on accept** (`subhost-rpc/src/block.rs`)
- When an authority is configured: producer must be registered, exactly one
  signature from that producer, valid BLS signature over the payload.
- Reject before any state change with `RpcError::UnauthorizedProducer`.
- Keep the gas-limit check from `cb062c0`.

**S1.4 Node and CLI wiring** (`subhost-node/src/lib.rs`, `subhost-cli/src/main.rs`)
- Build the registry from `genesis.initial_validators` at bootstrap.
- `subhost node --validator` requires a producer key; add
  `--producer-key-file <path>` (hex or JSON keystore) and refuse to start as a
  validator without it.
- `init --validator` now also accepts and stores a proof of possession.

**S1.5 Tests and docs**
- Reject: missing signature, wrong signer address, unknown validator, malformed BLS,
  signature over a mutated header.
- Accept: exact producer signature; two-node gossip where the follower verifies.
- Update `docs/manual-testing.md`, `SECURITY.md` (the "empty signatures" paragraph),
  `CHANGELOG.md`.

**Exit criteria:** a block produced without a valid signature from the genesis set is
rejected by every other node; the ledger still replays.

### S2 — Wire the consensus driver into the node (Track A)

**S2.1 Driver ownership** (`subhost-node/src/lib.rs`)
- `Node` holds a `ConsensusDriver` built from the genesis set and its identity.
- Producer path: `seal_block` returns a proposal instead of committing; the node
  calls `driver.propose`, broadcasts `BlockProposal` + `Vote`, and commits only when
  `receive_vote` returns `VoteOutcome::Committed`.

**S2.2 Vote transport** (`subhost-network`, `subhost-node`)
- Handle the existing `NetworkMessage::Vote`; verify, tally, re-broadcast.
- Do not relay votes for a view you have not seen.

**S2.3 Commit path** (`subhost-rpc/src/block.rs`)
- Rename the direct path to `commit_qc(block, qc)`; it verifies the QC before
  persisting. Remove the unauthenticated "submit and commit" path from validator
  mode, keep it behind an explicit `--single-writer` flag for demos/bench.

**S2.4 Timeouts and view change** (`subhost-consensus/src/driver.rs`)
- `tick(now)` returns `Timeout { view }` after `ConsensusConfig.view_timeout`.
- `enter_next_view` on timeout, leader = validators[view % n], re-propose the locked
  block. Test with a validator that never votes.

**S2.5 Tests**
- 4-validator in-process happy path.
- One silent validator: still commits with 3.
- Two silent: no commit, no ledger write, view advances.
- Two-node loopback: proposal → vote → commit, follower height matches.

**Exit criteria:** blocks are only committed with a QC; no single node can advance
the chain alone once a validator set is configured.

### S3 — Fork choice, reorg, and block sync (Track A)

**S3.1 Fork choice**
- Keep side branches keyed by block hash; choose the tip with the highest committed
  view/height, tie-break by QC weight then hash.
- `accept_block` becomes `accept_block_at(parent, block, qc)`; a block whose parent is
  not known is buffered, not rejected.

**S3.2 Reorg**
- `RpcState::reorg_to(tip)` replays the winning branch from the common ancestor.
- Receipts and state are rebuilt from that ancestor; the ledger append is atomic.

**S3.3 Sync protocol** (`subhost-network`, `subhost-node`)
- `GetBlocks { from, to }` / `Blocks { items }` messages with a hard item cap.
- On startup and on a height gap, request the missing range; apply in order.
- Do not serve blocks below the finalized height minus the retention window.

**S3.4 Storage support** (`subhost-storage`, contract with Track B)
- Append with explicit parent hash; `truncate_to(height)` for reorg rollback.
- Ledger v3 with the block's view/QC commitment.

**S3.5 Tests**
- Two producers, partition then heal: one branch wins, both converge.
- Node offline for 100 blocks: backfills to the tip.
- Reorg across 3 blocks: balances, nonces, receipts, and state root match a fresh
  replay.

**Exit criteria:** "two producers fork permanently" disappears from the docs because
it is no longer true.

### S4 — Segmented storage, snapshots, migration (Track B)

**S4.1 Segmented log**
- `node-state.bin` becomes `ledger/manifest.json` + `segments/000001.log`…
- Each segment: magic, version, length, BLAKE3 checksum, records; fsync per append.
- Manifest: chain id, format version, segment list, last height/hash.

**S4.2 Snapshot**
- Periodic `snapshot-<height>.bin` = serialized state + last block + receipt index.
- Load = newest snapshot + replay of trailing segments.
- Snapshot written to a temp file, fsynced, renamed.

**S4.3 Pruning**
- Retain the last `N` segments and the newest snapshot; refuse to prune below the
  finalized height or below a configured retention floor.
- `subhost db prune --keep-segments N`.

**S4.4 Migration**
- `subhost db migrate` converts v2 `node-state.bin` to v3; writes a backup, verifies
  the migrated chain by full replay, and is idempotent.
- Refuse unknown versions with the existing `UnsupportedVersion` error.

**S4.5 Failure tests**
- Truncated segment, corrupt checksum, missing segment, stale manifest.
- Crash between snapshot rename and manifest update.
- Migration interrupted, then re-run.

**Exit criteria:** a chain larger than 64 MiB loads in bounded time, and a corrupt
tail never prevents start-up from the last good snapshot.

### S5 — RPC security and method gaps (Track B)

**S5.1 Auth and transport**
- Bearer token (`--rpc-auth-token-file`), constant-time compare, 401 on mismatch.
- TLS via rustls (`--rpc-tls-cert/key`), or a documented, tested reverse-proxy mode.
- Per-IP token bucket (`--rpc-rate-limit`), 429 with `Retry-After`.
- Bind loopback by default; require `--rpc-allow-public` plus auth for anything else.

**S5.2 Methods**
- Add `eth_getBlockByHash`, `eth_syncing`, `eth_feeHistory`, `eth_maxPriorityFeePerGas`.
- Add `eth_getLogs` and `eth_subscribe` only after S10 produces logs; until then
  return a documented "unsupported" error rather than an ad-hoc message.
- Full JSON-RPC 2.0 error-code conformance test matrix.

**S5.3 Metrics**
- Increment `subhost_rpc_requests_total`, `subhost_rpc_errors_total`, and the
  duration histogram (currently defined but always zero).
- Label by method only, never by caller input.

**Exit criteria:** an unauthenticated caller cannot reach a state-changing method;
the metrics match a request-count test.

### S6 — Durable mempool and fee market (Track B)

**S6.1 Pool persistence**
- Atomic `mempool.bin` (same envelope as the ledger), reloaded on start, dropped for
  transactions already on chain.
- Config: `--mempool-persist`, capacity, per-sender cap.

**S6.2 Fee market**
- Replace the fixed gas price with an EIP-1559-style base fee computed from the
  parent block's utilisation, bounded by a change denominator.
- Keep `eth_gasPrice` and `eth_maxPriorityFeePerGas` consistent with it.
- Ledger records the base fee per block; replay recomputes it.

**S6.3 Eviction and metrics**
- Count admitted, replaced, evicted, expired; expose a pool-depth gauge per sender
  bucket.

**Exit criteria:** restarting a node does not lose accepted-but-unmined
transactions; the base fee moves under load and returns to the floor when idle.

### S7 — Network hardening (Track A)

**S7.1 Transaction gossip** — wire the existing `TransactionStem`/`TransactionFluff`
messages: stem with TTL, fluff after `--fluff-after`, dedupe by tx hash.
**S7.2 Peer scoring** — per-peer counters for invalid messages, duplicate floods, and
dropped connections; ban above a threshold with exponential decay.
**S7.3 Limits** — per-peer message rate, max message size already exists, max peers
enforced at the swarm level, inbound/outbound caps.
**S7.4 Relay policy** — relay any valid block/QC, not only locally sealed ones.
**S7.5 Tests** — flood test, invalid-signature flood, reconnect after ban expiry.

**Exit criteria:** one peer cannot saturate another node's CPU or memory with valid-
looking traffic.

### S8 — Observability, health, runbooks (Track B)

**S8.1 Tracing** — OpenTelemetry OTLP export behind a feature flag; spans for
`accept_block`, `seal_block`, consensus steps, and RPC methods.
**S8.2 Metrics** — height, finalized height, peer count, QC latency, reorg count,
ledger bytes, snapshot age, mempool depth, RPC latency/errors.
**S8.3 Health** — `/health/live` and `/health/ready` (ready = ledger loaded, not
syncing, consensus alive).
**S8.4 Runbooks** — `docs/operations/`: start/stop, key rotation, backup/restore,
ledger repair, reorg investigation, incident checklist.
**S8.5 Alerts** — sample Prometheus rules for height stall, no-quorum, disk growth.

### S9 — Release engineering (Track B)

**S9.1 Versioning** — move to `1.0.0-rc.N`; document semver for the JSON-RPC surface.
**S9.2 Release workflow** — tag → build → test → sign (cosign/sigstore) → SBOM
(`cargo cyclonedx`) → publish binaries + container.
**S9.3 Reproducibility** — `--locked`, pinned toolchain, documented build image,
checksum manifest.
**S9.4 Upgrade docs** — supported versions, ledger migration, rollback.
**S9.5 Image hygiene** — non-root (already), read-only rootfs, image scan in CI.

### S10 — Execution layer (Track C, XL)

Decision required before starting: **EVM via `revm`** (compatibility, ecosystem) or
**WASM via `wasmtime`** (determinism, easier metering). Recommendation: `revm`, because
the RPC surface is already Ethereum-shaped and `eth_call`/`eth_getLogs`/tooling come
almost for free.

**S10.1** New crate `subhost-exec`: state adapter over `subhost-state`, gas metering,
journaled writes, revert on failure.
**S10.2** Contract create/call transaction types (currently refused in the pool).
**S10.3** Logs, receipts with `logsBloom`, `eth_getLogs`, `eth_call`,
`eth_estimateGas`, `eth_getCode`.
**S10.4** Determinism + fuzz + differential tests against a reference EVM.
**S10.5** Gas schedule and block gas limit policy.

### S11 — IBC production (Track C, L)

**S11.1** Tendermint-style light client: validator-set rotation, signed-header
verification, multisig quorum certificate.
**S11.2** Non-membership proofs, or remove the claim that absence can be proven.
**S11.3** Remove the unverified `receive_packet` path from production builds.
**S11.4** Relayer binary with retry, backoff, and metrics.
**S11.5** End-to-end test against a local two-chain harness.

### S12 — Audit and testnet (all tracks, gate)

**S12.1** Refresh `docs/security/threat-model.md` for consensus, sync, auth, and
storage.
**S12.2** External audit of core/consensus/rpc/storage; close every High/Medium.
**S12.3** Public four-validator testnet; 30 days; no manual ledger repair; publish
incident log.
**S12.4** Bug bounty scope and disclosure policy.
**S12.5** v1.0 go/no-go against section 2.

---

## 6. Safe parallel start (what to do first)

If both agents start today, do this and nothing else:

0. **Both:** `ls docs/agents/questions/open/` and clear anything addressed to your
   track. `Q-001` is agreed; its ADR is the contract for S1.
1. **Agent A:** S0.1 → S0.2 → S1.1 → S1.2 → S1.3. Do not touch `subhost-storage`,
  `subhost-cli`, `explorer`, `.github/`.
- **Agent B:** S4.1 → S4.2 (storage crate only) and `docs/operations/backup-restore.md`.
  Do not touch `subhost-core`, `subhost-consensus`, `subhost-node`, or
  `subhost-rpc/src/lib.rs`.
- **Merge order:** S0.1 first (it prevents the only real file conflict), then S1 and
  S4 in parallel.
- **Contract commit:** if B needs a `LedgerStore::append_with_parent` signature before
  A's S3.4, B adds the method with a test in `subhost-storage` only, and A consumes it
  later. No cross-crate edits in the same PR.

### Conflict hotspots

| File | Risk | Mitigation |
| --- | --- | --- |
| `crates/subhost-rpc/src/lib.rs` | Both tracks | S0.1 split; then `block.rs` (A) vs `methods.rs`/`auth.rs` (B) |
| `crates/subhost-node/src/lib.rs` | A owns block/consensus, B needs bootstrap glue | B waits for A's S1.4; otherwise contract commit |
| `Cargo.toml` | workspace deps | additive lines only; rebase before push |
| `CHANGELOG.md` | every PR | insert at the top of the section; never reorder |
| `README.md` status table | both | A updates chain/consensus rows, B updates ops/release rows |

---

## 7. Estimates

Rough, for one experienced Rust engineer; halve for two working in parallel on
non-overlapping tracks.

| Milestone | Stages | Effort |
| --- | --- | --- |
| v0.2 public testnet (honest) | S0, S1, S2, S3, S4, S5 | 6–10 weeks |
| v0.5 feature-complete chain | + S6, S7, S8, S9 | +6–8 weeks |
| v1.0 production L1 | + S10, S11, S12 | +6–12 months |

The single largest uncertainty is S10 (execution layer). Decide EVM vs WASM before
promising a date.

---

## 8. Non-goals for v1.0

- Cross-chain bridge beyond the IBC work in S11.
- On-chain governance and treasury.
- Zero-knowledge proofs or privacy transactions.
- Sharded or erasure-coded storage.
- A hosted explorer SaaS; the bundled explorer stays read-only plus local signing.
