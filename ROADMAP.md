# Roadmap to a full release

Status: 2026-11-18 · baseline `main` = `1043268` · S0.1–S0.3, S1.1–S1.4, S4.1–S4.5, S5.1a and S5.3a merged · S2.1–S2.2, S2.3 QC API, S2.4a recovery/P2P and S5.3b implemented on the S2 branch · `Q-001`–`Q-011` agreed
This document is the work plan for turning the current pre-production workspace into
a releasable network. It is written so two agents can work at the same time without
editing the same files.

---

## 1. Where the code is today

| Area | Reality |
| --- | --- |
| Chain | Validator deployments admit to the pool and commit with a QC. Demo chains without validators seal synchronously on submission. |
| Block auth | A node with a configured authority signs each produced block with the validator's BLS key and verifies that signature on accept; a node without one still produces and accepts unsigned blocks. |
| Consensus | Node owns a linear `n - f` quorum driver restored at the ledger tip. Observers verify without voting. Durable open rounds/latest QC; no timeout, view change or leader rotation yet. |
| Networking | libp2p relays proposals, verified votes and QC-certified next blocks; demo block gossip remains. Bootstrap retries; no historical block sync, peer scoring, per-peer limits or tx gossip. |
| Storage | `ledger/manifest.json` (v2, with `base_height`) + append-only `ledger/segments/NNNNNN.log` + periodic `ledger/snapshot-<height>.bin` (every 1024 blocks). `subhost db migrate` converts a v2 `node-state.bin`; `subhost db prune` drops the oldest segments. Loading reads every retained segment, the snapshot carries no receipt index, and pruning is offline only. |
| State | Balances + nonces + a Merkle root over sorted leaves. Transfers only; contract creation is refused. |
| RPC | 11 methods. Optional bearer token (`--rpc-auth-token-file`), per-connection rate limit, and a non-loopback bind policy (`--rpc-allow-public` requires a token); all off by default. No TLS — the documented public deployment is a reverse proxy. Completed decoded RPC calls and returned errors now reach the node metrics registry. |
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
2. **Consensus is live.** Blocks commit only after an `n - f` quorum certificate. No
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

| Stage | Goal | Track | Depends on | Size | Status |
| --- | --- | --- | --- | --- | --- |
| S0 | Prepare `rpc` for parallel work | A | — | S | Merged |
| S1 | Signed blocks and validator identity | A | S0 | M | Merged through S1.4 |
| S2 | Wire the consensus driver into the node | A | S1 | L | Partial: QC API, durable rounds, local testnet/P2P; timeout/CLI pending |
| S3 | Fork choice, reorg, and block sync | A | S2 | L | Planned |
| S4 | Segmented storage, snapshots, migration | B | — | L | Merged through S4.5 |
| S5 | RPC auth, TLS posture, rate limits, method gaps | B | S0 | M | S5.1a/S5.3a merged; S5.3b implemented |
| S6 | Durable mempool and fee market | B | S0 | M | Planned |
| S7 | Network hardening: tx gossip, scoring, peer limits | A | S3 | M | Planned |
| S8 | Observability, health, runbooks | B | S0 | M | Planned |
| S9 | Release engineering | B | S5 | M | Planned |
| S10 | Execution layer (EVM or WASM) | C | S3 | XL | Planned |
| S11 | IBC production light client + relayer | C | — | L | Planned |
| S12 | External audit and public testnet soak | all | S1–S9 | XL | Planned |

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
  `--producer-key-file <path>`. The format is one hex-encoded canonical 32-byte
  BLS scalar (see `Q-002`); an encrypted keystore is S9.6, not this stage. Refuse to
  start as a validator without the file.
- `init --validator` now also accepts and stores a proof of possession:
  `ADDRESS:POWER` derives it from the producer key, `ADDRESS=PUBKEY_HEX:POWER` adds
  the key match, and `ADDRESS=PUBKEY_HEX:POWER:POP_HEX` stores a proof signed
  elsewhere. A missing producer key is generated once with mode `0600` and never
  overwritten.

**S1.5 Tests and docs**
- Reject: missing signature, wrong signer address, unknown validator, malformed BLS,
  signature over a mutated header.
- Accept: exact producer signature; two-node gossip where the follower verifies.
- Update `docs/manual-testing.md`, `SECURITY.md` (the "empty signatures" paragraph),
  `CHANGELOG.md`.

**Exit criteria:** a block produced without a valid signature from the genesis set is
rejected by every other node; the ledger still replays.

### S2 — Wire the consensus driver into the node (Track A)

**Status:** S2.1–S2.2, the S2.3 QC commit API and S2.4a durable round recovery/
P2P reconnect are implemented on `s2/consensus-driver-ownership`. The local-testnet
runner and real four-validator regressions cover late bootstrap, quorum shortfall,
late next-block delivery and hub restart. The stage remains incomplete: S2.4b
timeout/view change, the explicit S2.3 `--single-writer` CLI policy and S2.5 timeout
assertions are pending. The node journal retains open votes and the latest QC,
but historical QCs are not authenticated on ledger replay until S3.4 supplies that
format. Concurrent proposals and oversized gossip blocks can stall. Preserve the
latest voting state when restoring backups. See `docs/consensus.md` and
`docs/local-testnet.md`; this is not production BFT.

**S2.1 Driver ownership** (`subhost-node/src/lib.rs`)
- `Node` holds a `ConsensusDriver` built from the genesis set and its identity.
- Producer path: `seal_block` returns a proposal instead of committing; the node
  calls `driver.propose`, broadcasts `BlockProposal` + `Vote`, and commits only when
  `receive_vote` returns `VoteOutcome::Committed`.

**S2.2 Vote transport** (`subhost-network`, `subhost-node`)
- Handle the existing `NetworkMessage::Vote`; verify, tally, re-broadcast.
- Do not relay votes for a view you have not seen.

**S2.3 Commit path** (`subhost-rpc/src/block.rs`)

Platform CLI consumer (Q-014): `--single-writer` and optional positive
`--view-timeout-ms` map to the published node configuration contract. CLI startup
rejects unsigned mode unless explicitly selected and rejects unsigned selection
for validator genesis before invoking node bootstrap. Demo scripts and the
Compose command carry the explicit demo flag. Authoritative node policy,
phased timeout behavior and the standard-BLS public rollout remain S2.4b work.
The Compose stack still requires the documented authenticated non-loopback RPC
configuration; container runtime verification is part of the release/deployment
stage rather than this CLI consumer change.

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

| Task | Status |
| --- | --- |
| S4.1–S4.4 | Merged |
| S4.5 | Merged |

**S4.1 Segmented log**
- `node-state.bin` becomes `ledger/manifest.json` + `segments/000001.log`…
- Each segment: magic, version, length, BLAKE3 checksum, records; fsync per append.
- Manifest: chain id, format version, segment list, last height/hash.

**S4.2 Periodic snapshots**
- `snapshot-<height>.bin` = serialized account state, written on the first persist
  and then every 1024 blocks, not at every commit; the previous snapshot is kept
  until the next commit.
- Load = newest snapshot + replay of the trailing blocks; the snapshot's state root
  must match the block at its own height.
- Snapshot written to a temp file, fsynced, renamed, then committed by the manifest.
- Not done here: the snapshot carries no receipt index, and segments are still read
  in full on start-up. Both belong with retention in S4.3.

**S4.3 Pruning**
- `subhost db prune --keep-segments N` drops the oldest segments and records the
  new `base_height` in the manifest (version 2; version 1 still reads as unpruned).
- The newest snapshot stays the replay anchor, so a prune that would drop the block
  at the snapshot's own height is refused before anything is written; files are
  deleted only after the manifest stops naming them.
- RPC maps `blocks[n]` to `base_height + n + 1`: a pruned height and a pruned
  transaction are `null`, `earliest` is the oldest retained block, and
  `block_count()` still reports the retained count (`head_height()` is the tip).
- Not done: there is no finality before S3, so the only retention floor is
  `--keep-segments`; the snapshot itself is never pruned, and start-up still reads
  every retained segment.

**S4.4 Migration**
- `subhost db migrate` converts v2 `node-state.bin` to v3; writes a backup, verifies
  the migrated chain by full replay, and is idempotent.
- Refuse unknown versions with the existing `UnsupportedVersion` error.

**S4.5 Failure tests**
- Truncated segment, corrupt checksum, missing segment, stale manifest.
- Interrupted record headers/payloads and crash between a new snapshot rename
  and manifest update; a retry may replace the uncommitted suffix.
- Failed snapshot writes after segment appends, including segment rollover.
- Migration interrupted before and after manifest commit, then re-run;
  repeated migration validates committed data before reporting success.
- A shortened committed tail is refused before append can pad it with zeroes.
- Runbook: `docs/operations/backup-restore.md`.

**Exit criteria:** state is no longer rewritten at every commit, and an uncommitted
torn tail never prevents start-up from the last committed head. Missing or corrupt
committed data is refused without silently rolling back history; recovery needs a
coherent backup. Tests model interrupted on-disk writes and a real snapshot rename
failure; power-loss testing on physical hardware remains outside this stage.
Loading still reads every retained segment and has no snapshot receipt index, so
start-up time remains proportional to retained history even after pruning (S4.3).

### S5 — RPC security and method gaps (Track B)

| Task | Status |
| --- | --- |
| S5.1a | Merged |
| S5.2 | In progress on the separate `s5/rpc-methods` branch |
| S5.3a | Merged server middleware and registry API contract (ADR-004) |
| S5.3b | Node registry wiring and exported success/error regression implemented on the S2 branch (Track A) |

**S5.1 Auth and transport** *(S5.1a shipped; S5.1b TLS posture is a documented
reverse-proxy mode)*
- Shipped: bearer token (`--rpc-auth-token-file` / `SUBHOST_RPC_AUTH_TOKEN_FILE`),
  constant-time compare over a BLAKE3 digest, redacted `Debug`, zeroized on drop;
  401 with JSON-RPC error `-32000` and `WWW-Authenticate: Bearer` on mismatch.
- Shipped: bind loopback by default; a non-loopback bind needs `--rpc-allow-public`
  plus a token, refused in `subhost-rpc::start` with a non-loopback test
  (`ADR-002`, contract Q-006). `--rpc-rate-limit` answers 429 with `Retry-After`
  and error `-32005`.
- Not done, documented instead: the limit is **per connection**, not per IP,
  because jsonrpsee 0.24 does not expose the peer address to HTTP middleware
  (re-verify when jsonrpsee 0.26 lands); WebSocket tokens are checked only at the
  upgrade handshake; and there is no in-process TLS — jsonrpsee 0.24.11 has no
  server-side TLS feature, so the supported public deployment is a TLS-terminating
  reverse proxy in front of a loopback bind.

**S5.2 Methods**
- Add `eth_getBlockByHash`, `eth_syncing`, `eth_feeHistory`, `eth_maxPriorityFeePerGas`.
- Add `eth_getLogs` and `eth_subscribe` only after S10 produces logs; until then
  return a documented "unsupported" error rather than an ad-hoc message.
- Full JSON-RPC 2.0 error-code conformance test matrix.

**S5.3 Metrics**
- S5.3a implements opt-in `RpcServer::with_metrics(Metrics) -> Self` and RPC
  middleware for completed decoded calls over HTTP/WebSocket, including individual
  batch entries, errors and execution duration. Existing metric names gain a
  `method` label; known names come from the module registry and arbitrary names
  share `unknown`. Legacy recorder APIs use `unattributed`.
- S5.3b passes the node's existing metrics handle into the server under ADR-004.
  Completed decoded calls and RPC errors reach `/metrics`; an HTTP/exporter test
  verifies success, error and latency samples. No node/state/block source was
  changed by S5.3a; the node glue lands with Track A's S2 continuation.
- Notifications, malformed requests, guard/limit rejections and cancelled calls
  bypass completion metrics; transport-level instrumentation remains S8 work.
- Relevant doc: `docs/operations/rpc-metrics.md`.

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
**S9.6 Encrypted producer keystore** — replace the plaintext 32-byte
`producer.key` scalar from S1.4 with a password- or credential-store-wrapped
keystore, and migrate an existing key file at start-up. Until then the file is
`0600` and must be treated as the validator's identity: a copy is a copy of the
right to produce blocks.

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
