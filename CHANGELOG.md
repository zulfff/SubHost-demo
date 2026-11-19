# Changelog

All notable changes to this repository. Every entry below was applied and
verified; nothing is aspirational.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Full-repository audit and hardening pass. Verified with `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
(clean), and `cargo test --workspace --all-features` (all suites green).

### Breaking changes

- **Legacy BLS validator proofs and signatures are refused.** RFC 9380 G2 hashing
  replaces a construction that allowed arbitrary signature forgery from a public
  PoP. Use fresh genesis proofs and signing/history directories; preserve old data
  and keys. Public/scalar/signature codecs retain their sizes.
- **`ConsensusError` gained a `Halted` variant.** Code that matches the enum
  exhaustively must handle it; signing calls return it while a conflicting-final-
  evidence halt is active. No wire or on-disk format changes.
- **Validator consensus has separate Prepare and final Commit phases.** Signed
  leader envelopes, phase-separated genesis-bound votes, authenticated timeout
  reports and independent views replace the linear view/height contract. All
  validator peers upgrade together. Signing journals now use `SUBVOTE2` and
  reject legacy data. Unsigned node demos require explicit `--single-writer`.

- **Validator submissions return before commitment.** With a genesis validator
  set, a transaction hash precedes its receipt; clients must poll until a QC
  commits the block. Proposal/vote gossip gains view and signer fields, and
  certified-block gossip is new, so devnet peers must upgrade together.
- **RPC metrics now carry a `method` label.** The request/error counters and
  duration histogram keep their names, but unlabelled exposition consumers must
  be updated. Legacy recording methods retain their Rust signatures and record
  under `unattributed`.
- **`ValidatorInfo` gained `proof_of_possession`.** Rust code that constructs the
  struct literally must set the field. JSON genesis files written without it still
  deserialize because the field is `#[serde(default)]`.
- **The on-disk ledger format is now version 2.** The magic is `SUBHOST1` with
  version `2`; version 1 files are refused with
  `StorageError::UnsupportedVersion(1)` rather than being migrated. No live chain
  data exists, but any ledger written by an earlier build must be deleted.
- **The transaction wire format changed.** `Transaction` carries a 32-byte
  `public_key`, so its canonical encoding, hash, and signing payload all differ
  from earlier builds. `Transaction::verify_signature` is now the single
  authentication rule: it binds the key to `from`, then verifies ed25519 over
  `signing_payload`. Pre-change signed transactions are invalid, and pre-change
  transaction hashes are not present in any receipt.
- **State and receipt commitments changed value.** `state_root` is now a raw
  Merkle root over address-sorted account leaves with the chain ID mixed into each
  leaf, and every block's `receipt_root` commits to per-receipt
  `(tx hash, height, index, gas, cumulative gas)`. Blocks produced before this
  change do not reproduce these roots.
- **`Receipt` gained `transaction_index` and `cumulative_gas_used`**, and
  `subhost_storage::receipt_root` was replaced by `receipt_commitment` plus
  `receipts_root`.
- **A submission can return a multi-transaction block.** `eth_sendTransaction`
  still returns the submitted transaction's hash, but the block it lands in may
  contain other senders' transactions, so `gasUsed` and `cumulativeGasUsed` in its
  receipt are no longer always `21_000`.

### Removed

- **10 orphan `omnichain-*` directories.** Duplicated the `subhost-*` crates,
  were not workspace members, and were compiled by nothing.
- **34 dead files in `subhost-core`.** `module1.rs` through `module10.rs` and the
  `constants/`, `primitives/`, `types/`, `utils/` trees were never declared as
  modules and never compiled. Each was a copy of the same `{ id, value }` struct.
- **7 placeholder crates.** `subhost-types`, `subhost-utils`, `subhost-p2p`,
  `subhost-evm`, `subhost-wasm`, `subhost-zk`, and `subhost-governance` were
  byte-identical copies of one `Config`/`Module`/`Error` template with a renamed
  type, had no consumer anywhere in the workspace, and implemented nothing. The
  README no longer claims they are in progress.
- **Unused workspace dependencies.** `wasmtime`, `wasmer`, `revm`, `rocksdb`,
  `redb`, `quinn`, `rustls`, `rayon`, `criterion`, `alloy-primitives`,
  `alloy-sol-types`, `primitive-types`, `ethereum-types`, `fixed-hash`, `uint`,
  `num_cpus`, `parking_lot`, `dashmap` (workspace-level), `proptest`,
  `tokio-test`, and `sha3` where unused. None were referenced by any source file;
  several pulled multi-hundred-crate graphs that made a workspace test run OOM on
  a 4 GB machine.
- **Dead template items** carried by every rewritten crate: `Subhost*Module`,
  `Subhost*Config`, `Subhost*Error`, and `Metrics { requests, errors, latency_ms }`
  structs that were constructed, incremented, and never read.
- **CLI commands with no backend.** `contract deploy`, `contract call`,
  `query account`, and `query validators` printed a log line and returned
  success. They are gone rather than pretending to act.
- **Unused CLI flags.** Global `--config` was parsed and ignored;
  `node --bootnodes` implied peer connectivity that does not exist.

### Fixed

- **Wallet files could abort the process through their scrypt cost parameters.**
  `KdfParams::validate` now refuses file-supplied costs above 256 MiB of memory
  (`128 * N * r`) or 2^22 work units (`N * r * p`) with checked arithmetic before
  any KDF allocation, on both the encrypt and decrypt paths. Default-cost
  wallets (N = 2^15, r = 8, p = 1) and legacy files without a `kdf` field still
  load. Closes SUBHOST-2026-002.
- **Ledger appends no longer re-validate the whole retained history
  (SUBHOST-2026-003, storage slice).** `LedgerStore` keeps the last fully verified
  head; a persist that extends it validates only the appended suffix, including a
  state replay that refuses transactions the committed state cannot apply
  (replayed or nonce-skipping transactions with otherwise honest commitments,
  which the old root-only check accepted into a ledger that would fail to load).
  Commit latency no longer grows with history: the audit PoC measured 57.3x
  submit-latency growth over 1000 empty blocks on `01a59df`, 1.2x with the fix
  and 1.0x on an independent re-run (run-to-run baseline spread 38.4x-57.3x;
  the same evidence set measured 47.5x on the calldata case, 6.7x after).
  A cache miss (fresh process before load, legacy file, prune, unchanged history)
  falls back to the previous full validation. Unpriced calldata (Bima/Raka fee
  contract) and the node-side O(N) block-list clone per commit remain separate
  follow-ups.
- Rejected oversized transaction admission/replacement preserves the existing
  pool and ledger state. Actual JSON gossip envelopes are checked against the
  transport budget before publication.
- Successful consensus signature checks are cached with a bounded complete-byte
  fingerprint to avoid repeated pairings on proof retransmission. The cache is
  never serialized and does not bypass semantic or execution validation.

- **Appending to a truncated committed segment could commit an unreadable head.**
  The writer now refuses a segment shorter than the manifest's committed length
  before resizing or appending it, preserving the damaged bytes and manifest for
  recovery. Uncommitted tails are still ignored on load and replaced on retry.
- **`subhost db migrate` reported success for corrupt segmented ledgers.**
  A repeated migration now validates the authoritative manifest, snapshot and
  retained segments before returning `AlreadySegmented`, including when a valid
  legacy file remains after an interrupted upgrade.
- **Block-query parameters are validated consistently.** Noncanonical height
  quantities and incorrectly typed hydration flags now return `-32602`, including
  on an empty ledger, instead of being treated as missing blocks or ignored.

- **`accept_block` accepted peer blocks over the block gas limit
  (SUBHOST-2026-001).** The producer capped a block at `BLOCK_GAS_LIMIT` while
  selecting transactions, but the peer path added the same gas figures without
  ever comparing them to the limit. A gossip peer could publish a block with
  `gas_limit = 30,000,000` and `gas_used = 40,000,000`; the node accepted,
  applied, and persisted it, and its own ledger validator then refused that file
  on the next restart, leaving the node down until the ledger was repaired. The
  accept path now rejects any block whose running gas total exceeds the header
  limit, before the transaction is applied. Three regression tests cover the
  rejected cases and a block that uses exactly the limit.
- **`cargo deny` failed on the 2026-11 advisory batch.** All 24 advisories were
  cleared rather than ignored. `libp2p` moved from 0.56 to 0.57, which brings
  `hickory-proto`/`hickory-resolver` 0.26.3 (15 DNS advisories),
  `libp2p-gossipsub` 0.50.1 (the PRUNE backoff advisory), and patched `rustls`,
  `rustls-webpki`, `webpki-roots`, and `tokio` releases. `chrono`
  (RUSTSEC-2026-0543, unmaintained) was replaced by `time` in the explorer, and
  the two stale `hickory` exceptions were removed from `deny.toml`.
- **The manual-testing walkthrough could not complete step 8.** Bob started at
  zero and received 100, which is less than the 21000 transfer fee, so his send
  was rejected. The genesis now funds Bob, the expected balances are updated, and
  the documented result for a stale nonce or an overdrawn sender now matches the
  message the node actually returns.
- **Several documentation claims did not match the code.** The README no longer
  says two producers always fork: only producers that seal at the same height do.
  The threat model now quotes the real `MIN_VALIDATOR_STAKE` of 10,000,000 SUB
  instead of 10,000. The manual testing guide now records that the node updates
  the block-height and pending-transaction gauges but leaves the RPC request
  counters at zero.
- **Faucet returned a fabricated transaction hash.** `handle_drip` hashed the
  address and the current time and returned that as `tx_hash`; no transfer was
  ever submitted and no balance changed. The faucet now loads an encrypted
  wallet, signs a real transfer, submits it over JSON-RPC, and returns the hash
  the node accepted.
- **`HotStuff::validate_qc` could never return true.** The function looped over
  signers and returned `false` unconditionally on the first iteration, then
  returned `false` again. Quorum certificates are now verified for real: every
  signature is checked against a registered BLS public key over a payload binding
  the view and the block hash, and a quorum of distinct registered signers is
  required. `clippy::never_loop` was firing on this as a hard error.
- **`DAG::has_quorum_support` measured the wrong thing.** It counted distinct
  authors among a vertex's own parents (its ancestry), which any vertex can
  satisfy with unrelated history. It now counts distinct next-round authors that
  reference the vertex as a parent, which is what support means.
- **`StakingModule::slash` was a no-op.** It computed a penalty and returned it
  without deducting anything. It now deducts the stake, ejects a fully slashed
  validator, burns delegations to an ejected validator, and records the evidence
  before mutating anything. Slashing now requires a non-empty proof.
- **`StakingModule::delegate` credited a delegation to a non-existent validator**
  and used unchecked `+=` on both the delegation and the stake. It now requires a
  registered validator and uses checked arithmetic.
- **Inbound gossip was dropped.** `subhost-network` created a channel, kept the
  sender, and discarded every received message, so the transport could publish
  but never deliver. Received messages are now decoded, validated, and forwarded
  to the application over an inbound channel.
- **`DAGVertex::hash` committed to a `Debug` string.** The identity of a vertex
  changed whenever an unrelated `Debug` implementation was reformatted, and
  parent order changed the hash. It now hashes a canonical encoding of the author,
  round, block hash, and sorted parents.
- **Transaction hashing was duplicated and divergent.** `Mempool::transaction_hash`
  and the RPC's inline hashing could disagree. There is now one
  `Transaction::hash`, and one `Transaction::signing_payload` used by every signer
  and verifier.
- **A poisoned lock could crash every later request.** The RPC used
  `unwrap_or_else(into_inner)` inline at 20 call sites; it is now three documented
  helpers, applied consistently.
- **`eth_getBlockByNumber` required the second parameter.** It now defaults to
  `false` as the spec allows, and accepts `safe` and `finalized` tags.
- **`eth_sendTransaction` accepted only `gasLimit`.** It now accepts the spec's
  `gas` as well, and `input` alongside `data`.
- **Contract creation reached the mempool before failing.** A transaction with no
  `to` was admitted, then rejected during execution. It is now refused during
  parameter parsing.
- **`eth_gasPrice` returned a hardcoded `0x1`.** It now reports the mempool's
  actual minimum accepted gas price.
- **Mempool replacement consumed the per-sender budget.** Replacing a transaction
  at an existing nonce counted against `max_per_sender`, so a sender at its limit
  could not raise a fee. Replacement no longer charges the budget.
- **Capacity eviction was non-deterministic.** Ties among equal gas prices were
  broken by `HashMap` iteration order. The tie-break now falls through to the
  transaction hash.
- **`GenesisConfig::validate` rejected every single-node genesis.** It required a
  validator, so `subhost init` produced a file that `GenesisConfig::load` refused.
  Validator requirements moved to `requires_validators`, which
  `node --validator` calls; the CLI no longer emits a genesis it cannot load.
- **`GenesisConfig::default` could panic** on a clock set before 1970
  (`duration_since(UNIX_EPOCH).unwrap()`). Timestamps now saturate at 0.
- **`subhost init` silently overwrote an existing genesis**, orphaning the ledger
  beside it. It now refuses unless `--force` is given.
- **Wallet permissions were set after the key was written.** `set_permissions`
  ran after `write_all`, leaving a window where the file was world-readable. The
  mode is now set before any key material is written.
- **`SymmetricEncryption::encrypt` panicked on AEAD failure.** It returns a
  `Result`.
- **`Metrics::record_request` accepted NaN and negative durations** into the
  histogram. They are now counted as requests but excluded from the distribution.
- **Faucet cooldown could be bypassed and could lock a caller out.** The cooldown
  key is now the lowercased address, and a failed drip releases the slot so a node
  outage does not impose a full-day penalty.
- **CLI `--nonce` and `--chain-id` were mandatory and hand-maintained**, so a
  stale value produced a silently rejected transaction. Both are now queried from
  the node when omitted.
- **CLI wallet names were used as file names unchecked**, allowing `../` and path
  separators, and silently overwrote an existing wallet. Both are now rejected.
- **`Dockerfile` could not build.** It copied only `crates/` while the workspace
  included `explorer/`, and referenced binaries that did not exist. It now builds
  the four real binaries with `--locked`, runs as an unprivileged user under
  `tini`, and ships no toolchain in the runtime image.
- **`docker-compose.yml` described a network that cannot exist.** It ran four
  validators of a single-node producer, mounted a `monitoring/` directory that was
  absent, published every port on all interfaces, and set a default Grafana
  password. It is now one node plus the explorer and Prometheus, all on loopback,
  with the monitoring configuration present.

### Added

- **Release engineering and checked optimized tests (S9).** Added exact tag/version
  validation, an immutable compiler-image binary build, deterministic archives
  and SHA256 inventories, pinned CycloneDX/Sigstore tooling, reusable locked CI,
  pre-publication image scanning, and an unprivileged read-only deployment
  template. Test builds use optimization level 1 while retaining debug assertions
  and overflow checks. Upgrade/rollback boundaries are documented; no stable
  release, audit completion or independent full rebuild claim is implied.

- **Certified candidate classification and finalized-fork accountability
  (S3, ADR-007).** `ConsensusDriver::classify_final_evidence` authenticates a
  final certificate and reports `Duplicate`, `Next`, `Conflicting`,
  `Historical` or `MissingAncestors`, so a repeated certificate is never a fork,
  junk evidence is a plain error, and a genuine conflict becomes an incident.
  `halt`/`halted` stop every signature through the single signing path while
  certified history still applies; the sticky reason is process-local, keeping
  the `SUBVOTE2` journal format unchanged. No finalized rollback:
  `subhost_reorg_total` stays at 0.

- **Platform observability API (S8, ADR-013).** `Metrics::set_health` publishes
  one coherent `HealthStatus` snapshot; the exporter serves `/health/live`
  (and legacy `/health`) as liveness-only and `/health/ready` as 200 only when
  `ledger_loaded && !syncing && consensus_alive`, default not ready. New
  families: `subhost_finalized_height`, `subhost_qc_commit_latency_seconds`,
  `subhost_ledger_active_bytes`, `subhost_ledger_snapshot_modified_seconds`
  (0 = unavailable), `subhost_reorg_total` (registered at 0, unsupported until
  a reviewed reorg protocol) and `subhost_mempool_transactions_total{event}`
  with bounded lifecycle labels. `LedgerStore::disk_stats()` measures the
  committed manifest/snapshot/segment footprint without replay, excluding
  backups and orphans and erroring on missing active data. Node runtime hooks
  are Track A follow-up work; until they land the gauges read defaults and
  readiness stays 503. Operator guide and sample alerts:
  `docs/operations/observability.md`.

- **Mempool lifecycle counters wired (S8.2, Q-026).** `Mempool::admit` buffers
  typed `MempoolEvent`s inside the pool; the submit path's candidate clone
  carries them, so rejected candidates drop their events and only
  pool-committed admissions count. `Mempool::flush_events` drains the buffer into
  `subhost_mempool_transactions_total`, called by the `eth_sendTransaction`
  handler after a successful submit. The drain is unconditional: without a
  registry, `Mempool::drain_events` discards the buffer, so it never grows and
  a registry attached later counts only subsequent admissions. `expired`
  remains registered at 0 until the S6.3 TTL path exists. `subhost-mempool`
  now depends on `subhost-metrics`
  (enum + flush only; no registry handle is stored in the pool).

- **Corrected BLS authentication for the testnet.** RFC 9380 G2 hashing replaces
  the scalar-hash construction that allowed arbitrary signatures to be rescaled
  from public possession proofs. Official curve vectors and direct forgery
  regressions cover the suite; the relay also rejects manufactured historical
  quorum evidence. Upgrade with fresh genesis proofs, preserve legacy data and
  do not enable a legacy verifier. Phased timeout/leader consensus remains
  separate from this corrected stable-API prototype.

- **Automatic certified-history replay.** The optional `sync-relay` Platform
  example records genesis-bound, checksummed block/QC pairs after quorum and
  full execution validation, then replays them over existing P2P for fresh or
  offline nodes. It has exclusive-writer locking, atomic synced records, bounded
  future buffering and a configurable byte cap. Redundant archives preserve
  history across relay downtime. Native range requests and history pruning are
  separate work; see `docs/operations/certified-sync.md` and ADR-006.
  Archive v2 requires RFC 9380 BLS and genesis-bound final-commit certificates,
  prevalidates the complete commit boundary before writing, and orders records
  by block height independently of timeout views. Legacy v1 archives/signatures,
  prepare-phase replay, public-PoP forgery and stale views are rejected. Replay
  waits for a peer's topic subscription so startup messages are not consumed by
  gossipsub's duplicate cache before any receiver is ready.

- **Bounded atomic mempool snapshot API.** `subhost_mempool::MempoolStore` loads
  and persists chain/genesis-bound `mempool.bin` snapshots, validates signed
  transfers and duplicate nonce slots, and caps file/record/count sizes. Writes
  replace/sync owner-only snapshots without cloning the whole pool. Failure and
  restart tests cover malformed data and interrupted filesystem writes. Node/RPC
  admission and CLI consumers remain S6.1b; the API alone does not make bundled
  node admissions durable. See `docs/operations/mempool-persistence.md`.
- Authenticated leader rotation with quorum timeout certificates and prepared
  locks retained through timeout/restart; bounded consensus block, wire and
  journal budgets; read-only certified-block validation and verified tip-view
  recovery; controlled node shutdown and cancellation cleanup.

- **Recoverable four-validator local testnet.** A node-local, checksummed voting
  journal saves open proposals, verified votes and the latest QC before votes
  are published. Prepared commit intents reconcile interruption before/after
  ledger append; corrupt/missing journals, incompatible genesis/identity and
  a ledger behind newer voting state are refused. A data-directory lock prevents
  concurrent local signers. Bootstrap endpoints retry with capped backoff and
  connection counts retain a peer until its final connection closes. The
  `local-testnet` example creates distinct keys/shared genesis and checks signed
  RPC submissions and equal tips/state roots across four loopback validators.
  See `docs/local-testnet.md` for restart, backup and remaining protocol limits.
- **Node RPC metrics reach the exporter (S5.3b).** The bundled node injects its
  existing registry through ADR-004; an HTTP regression verifies exported success,
  error and latency counts without unbounded caller labels.
- **Validator consensus wiring (S2.1–S2.2 and the S2.3 commit API).** RPC admission
  stays pending when genesis defines validators. Nodes exchange signed proposals,
  votes and certified commits; `commit_qc` checks the certificate before the
  ledger write. Observers verify without signing. A failed write retains the
  certificate for retry and never advances the visible or driver tip. Quorums
  require `n - f` validators, preventing single-signer commits for sets of two or
  three. Demo chains without validators retain synchronous single-writer blocks.
  Timeout/view change, authenticated historical QCs, historical sync and the
  explicit CLI single-writer flag remain deferred; see `docs/consensus.md`.

- **Opt-in JSON-RPC call metrics.** `RpcServer::with_metrics` records completed
  HTTP/WebSocket calls, individual batch entries, RPC errors and execution
  duration into a supplied exporter registry. Labels use registered method names
  and one `unknown` fallback; caller strings never create time series. Tests
  cover invalid parameters, batch errors, label cardinality, registry isolation
  and transport rejections that bypass call metrics. The bundled node's registry
  wiring follows this S5.3a contract; see `docs/operations/rpc-metrics.md`.
- **Storage crash recovery coverage and an offline backup/restore runbook.**
  Eight new failure-path tests cover interrupted record headers and payloads,
  failed snapshot writes, damaged orphan segments/snapshots, migration before and
  after manifest commit, and the two false-success regressions. The existing
  crash test now writes a new snapshot and checks state and receipts after retry.
  `docs/operations/backup-restore.md` explains coherent backups, offline
  validation, and recovery limits for committed corruption.
- **RPC block hashes and legacy fee history (S5.2).** Added
  `eth_getBlockByHash`, `eth_syncing`, `eth_feeHistory` and
  `eth_maxPriorityFeePerGas`. History is bounded to 1024 retained blocks and 100
  gas-weighted reward percentiles, with zero pre-EIP-1559 base fees. Sync reports
  no active worker, not peer convergence. Logs/subscriptions now return explicit
  `-32004` unsupported errors. HTTP tests cover pruning, fee weighting, malformed
  parameters, standard error codes, persistence-detail suppression, batch IDs
  and notifications. See `docs/rpc.md` and `ADR-012` for compatibility limits.

- **Validator registry and producer key at bootstrap.** `Node::bootstrap` builds a
  `ValidatorRegistry` from `genesis.initial_validators` whenever the genesis has a
  validator set, and `RpcState::with_validators` enables peer-block verification
  without a signing key, so a follower rejects unsigned blocks. `node --validator`
  additionally reads `<data_dir>/producer.key` (or `NodeConfig::producer_key_file`),
  refuses a file that group or others can read, decodes one canonical 32-byte BLS
  scalar, and resolves the validator by matching the derived public key against
  genesis: zero or several matches refuse to start. New `NodeError` variants carry
  the path, and `PRODUCER_KEY_FILE_NAME` names the default. `read_producer_key` and
  `write_producer_key` keep the format and permission rules in one place, and
  `node --producer-key-file` overrides the default path.
- **Validator genesis from the CLI.** `subhost init --validator` now writes the proof
  of possession that registration verifies, so a genesis it produces boots a
  validator instead of being refused. `ADDRESS:POWER` derives the public key from the
  producer key and signs the proof; `ADDRESS=PUBKEY_HEX:POWER` requires the key to
  match; `ADDRESS=PUBKEY_HEX:POWER:POP_HEX` stores a proof signed elsewhere so a
  coordinator can assemble a set. A missing producer key is generated once at
  `<data_dir>/producer.key` (or `--producer-key-file`) with mode `0600` and is never
  overwritten; a mismatched key, an invalid proof, and a zero-power validator all
  fail before the genesis file is written.
- **Optional JSON-RPC authentication and rate limiting (S5.1a).** `subhost-rpc`
  gains `RpcSecurityConfig` and `crates/subhost-rpc/src/security.rs`: a bearer
  token read from a file (`--rpc-auth-token-file` or
  `SUBHOST_RPC_AUTH_TOKEN_FILE`, minimum 16 bytes), compared in constant time as a
  BLAKE3 digest, with a redacted `Debug` and zeroization on drop; a per-connection
  token-bucket rate limit (`--rpc-rate-limit`) answering 429 with `Retry-After`
  and JSON-RPC error `-32005`; and a bind policy that refuses any non-loopback
  JSON-RPC bind unless `--rpc-allow-public` and a token are both set (error
  `-32000` with `WWW-Authenticate: Bearer` on a rejected token). `subhost-node`
  forwards the policy from `NodeConfig.rpc_security` (contract commit, Q-006 /
  `ADR-002`), and `subhost-cli`, the faucet and the explorer send the token and
  name the missing setting instead of surfacing a bare 401/429. Everything
  defaults to off, so an existing node without flags behaves exactly as before.
  Documented limitations: the rate limit is per connection because jsonrpsee 0.24
  does not expose the peer address, WebSocket tokens are checked only at the
  upgrade handshake, and there is no in-process TLS — the public deployment mode
  is a TLS-terminating reverse proxy. 8 new unit tests plus 4 RPC integration
  tests, and client tests in the CLI (2), faucet (2), and explorer (2).
- **Segment pruning and height mapping.** `subhost db prune --keep-segments N`
  drops the oldest `ledger/segments/NNNNNN.log` files and records the new
  `base_height` in `ledger/manifest.json` (manifest version 2; version 1 is still
  read as an unpruned ledger). The newest snapshot stays the replay anchor, so a
  prune that would drop the block at the snapshot's own height is refused before
  anything is written, and segment files are deleted only after the manifest no
  longer names them. `RestoredLedger` now reports `base_height`, load validates
  and replays the retained suffix instead of assuming heights start at 1, and the
  RPC layer maps `blocks[n]` to `base_height + n + 1`: a pruned height is JSON
  `null`, `earliest` is the oldest retained block, and `block_count()` still means
  the retained count while `head_height()` is the tip. Covered by 12 new storage
  tests and 2 RPC tests (prune, refusal, no-op, restart, persistence after prune,
  v1 manifests, out-of-order segments, corruption after prune).
- **Producer verification on accept.** With a configured authority,
  `accept_block` requires `header.validator` to be a registered validator and
  exactly one BLS signature from that validator over `Block::signing_payload()`,
  checked before the state transition. Missing, extra, foreign, malformed, or stale
  signatures are rejected with `RpcError::UnauthorizedProducer` and leave the tip,
  state, and pool untouched. A node without an authority keeps accepting unsigned
  blocks, so the guarantee is not network-wide yet.
- **Periodic state snapshots.** `persist` writes a fresh
  `ledger/snapshot-<height>.bin` on the first commit and then once every
  `SNAPSHOT_INTERVAL_BLOCKS` (1024) blocks instead of at every commit, and keeps
  the previous snapshot for one more commit. `load` checks that the snapshot's
  state root equals the `state_root` of the block at its own height, then replays
  only the blocks after it with `State::apply_transaction`, so start-up executes
  at most one interval of transactions. Still outstanding and recorded in
  `ROADMAP.md`: segments are still read in full on start-up, the snapshot holds no
  receipt index, and history is not pruned yet (`subhost db prune` lands in S4.3).
- **Block signing on produce.** `RpcState::with_authority` attaches a
  `BlockAuthority`: the validator registry, the producer address, and its BLS
  secret. When one is configured, `seal_block` sets `header.validator` and attaches
  exactly one BLS signature over `Block::signing_payload()`; without one, the
  single-node form stays `Address::ZERO` with an empty signature list.
  `BlockAuthority::new` refuses an unregistered producer, or a secret whose public
  key is not the registered one, with `RpcError::UnauthorizedProducer`.
- **`subhost db migrate`.** A version 2 `node-state.bin` can now be converted
  without starting a node. The command validates the whole legacy file, writes the
  segmented ledger, re-reads it and compares the head with the legacy head, keeps the
  original as `node-state.bin.v2-backup`, and is a no-op on an already-segmented
  ledger. Corrupt or unknown-version files fail without writing anything, so an
  interrupted run can simply be repeated.
- **Segmented ledger store.** `subhost-storage` keeps history in
  `ledger/manifest.json` plus append-only `ledger/segments/NNNNNN.log` (framed
  records, BLAKE3 per record) and account state in
  `ledger/snapshot-<height>.bin`. `persist` validates the snapshot, appends only
  the new block records, fsyncs the segment, writes the state snapshot when due,
  and replaces the manifest atomically last, so bytes the manifest does not
  reference are an interrupted append that is ignored and truncated. The public
  `LedgerStore` API is unchanged. Covered by 18 new tests: round trip, multi-segment
  roll-over, append-only writes, torn tail, corrupt record, missing segment,
  truncated segment, stale manifest length, manifest path traversal, snapshot
  corruption, crash between snapshot and manifest commit, v2 migration, non-append
  refusal, funded-state-at-height-zero, deferred snapshots with trailing replay,
  snapshot-root anchoring, and a snapshot ahead of the head.
- **`StorageError::NonAppendPersist`.** Persisting a history that does not extend
  the committed head is refused until fork choice (S3) can express a reorg.
- **Public layout constants.** `LEDGER_DIR_NAME`, `MANIFEST_FILE_NAME`, and
  `LEDGER_BACKUP_FILE_NAME` describe the segmented store; `LEDGER_FILE_NAME` and
  `MAX_LEDGER_BYTES` now apply to the legacy single file only.
- **Validator registry from genesis.** `ValidatorRegistry::from_genesis` builds the
  signing registry directly from `GenesisConfig::initial_validators`. It checks
  every entry before registering any of them: non-zero voting power, a public key
  and proof that decode as compressed BLS points, and a proof that verifies under
  the proof-of-possession domain. Duplicates fail; an empty slice yields an empty
  registry. New errors: `ZeroVotingPower`, `MalformedPublicKey`, and
  `MalformedProofOfPossession`. Nothing calls it yet; S1.4 wires it into bootstrap.
- **BLS decoding helpers.** `BLSScheme::decode_public_key`,
  `BLSScheme::decode_signature`, `PUBLIC_KEY_BYTES` (48), and `SIGNATURE_BYTES`
  (96) turn compressed wire bytes into points with length and curve checks.
  `encode_public_key`, `encode_signature`, `public_key`, `encode_secret_key`,
  `decode_secret_key`, and `SECRET_KEY_BYTES` (32) define the producer key
  convention: one canonical little-endian scalar, hex-encoded on disk, with its
  48-byte public key and 96-byte signatures. Decoding rejects the zero scalar and
  any non-canonical encoding, so two different files cannot name the same key.
- **Architecture overview.** `docs/architecture.md` explains the block lifecycle
  from submission to acceptance, who may produce a block today, what the ledger
  guarantees and what it does not, and which crate owns each decision. Every claim
  is tied to the code path that enforces it.
- **Block signing payload contract.** `subhost_core::BLOCK_SIGNING_DOMAIN` and
  `Block::signing_payload()` define the bytes a validator will sign:
  `domain || chain_id || height || block_hash`, with both integers big-endian.
  Nothing signs or verifies it yet; S1.2 and S1.3 wire that in.
- **`ValidatorInfo` carries a proof of possession.** The new
  `proof_of_possession: Vec<u8>` field is `#[serde(default)]`, so genesis files
  written before the field existed still load. The CLI and node set it empty until
  S1.4 accepts and validates a real proof.
- **Domain-separated Merkle tree.** `subhost_core::merkle` builds a perfect binary
  tree with distinct leaf, internal-node, and padding domains, so `[a, b, c]` and
  `[a, b, c, c]` cannot share a root. Includes root construction, inclusion
  proofs, verification, and a key/value leaf helper.
- **Merkle state commitment and account proofs.** `State::root` is a Merkle root
  over address-sorted account leaves bound to the chain ID, insertion order does
  not affect it, and `State::account_proof` / `verify_account_proof` produce and
  check inclusion paths. Exposed over JSON-RPC as `eth_getProof`.
- **Multi-transaction block production.** The producer now selects the
  nonce-contiguous prefix for each sender in address order within the 30M block
  gas limit and seals one block per submission. Selection runs against a candidate
  state clone, and the live pool is replaced only after a successful ledger write.
- **Peer block acceptance.** `RpcState::accept_block` re-validates a block from a
  peer end to end: parent, height, chain, gas, signatures, transaction and receipt
  roots, and the resulting state root. It derives receipts locally instead of
  trusting the wire. Malformed blocks are rejected with `BlockRejected`.
- **Block event stream.** `RpcState::subscribe_blocks` reports whether a block was
  sealed locally or accepted from a peer.
- **Node block propagation.** `subhost node --p2p-listen` publishes every locally
  sealed block over libp2p gossip and applies peer blocks in height order, holding
  blocks that arrive before their parent in a bounded 64-block buffer.
- **Consensus driver.** `subhost_consensus::driver` advances one view at a time,
  collects BLS votes for one proposal, and commits on `2f + 1` distinct signers.
  Each proposal must extend the committed tip, and votes are accepted once per
  validator per view. Includes a four-validator test with one faulty replica.
- **IBC light client and commitment proofs.** `subhost_ibc::light_client` verifies
  ed25519-signed counterparty headers with strictly increasing height and time,
  tracks a trusted app hash, and checks Merkle membership proofs against it.
  `IbcModule::receive_packet_with_proof` requires the proof to match the packet's
  commitment key and payload before the normal packet rules run.
- **`subhost-storage` is a real crate.** The ledger format was inline in
  `subhost-rpc`. It is now a documented, separately tested crate: versioned
  magic-and-checksum envelope, atomic write with directory fsync, `0600`
  permissions, size bound, and full replay of every block and receipt commitment
  against the restored state on load. Nine tests, including bit-flip, truncation,
  wrong-magic, wrong-chain, and eight distinct tampering cases.
- **`subhost-node` is a real crate.** Node bootstrap (genesis load, ledger
  restore, one-time allocation application, RPC and metrics wiring, and graceful
  SIGTERM/Ctrl-C shutdown) lived in the CLI's `main.rs`. Eight tests.
- **`subhost-telemetry` is a real crate.** Three binaries each initialized
  `tracing_subscriber` differently and ignored `RUST_LOG`. One
  `RUST_LOG`-aware initializer now serves all of them, with optional JSON output
  for containers and a non-fatal double-initialization path.
- **`subhost-metrics` exposes metrics a node actually reports.** The registry was
  never started by any binary. `subhost node --metrics-addr` now serves
  `/metrics` and `/health`, and block height and mempool depth are updated while
  the node runs.
- **New RPC methods.** `eth_getTransactionByHash` and `eth_getTransactionCount`.
- **New CLI commands.** `query nonce`, `query chain`, `wallet show`,
  `init --validator`, `init --force`, `node --metrics-addr`,
  `node --max-connections`, and `--verbose`/`--quiet`.
- **Mempool nonce-continuity helpers.** `ready_for` returns the gap-free prefix a
  proposer can include; `prune_below_nonce` drops transactions state has already
  executed.
- **IBC channel lifecycle.** Channels opened in `Init` must be confirmed before
  they carry packets; transitions are validated and only move forward. Added
  `timeout_packet`, and receive sequences are tracked per channel instead of
  globally.
- **`ValidatorRegistry`** binding validator addresses to BLS public keys, with
  mandatory proof-of-possession verification at registration.
- **HotStuff safety rule.** `locked_view` and `is_safe_proposal` are now enforced
  rather than being dead fields.
- **Wallet KDF parameters are recorded in the file**, so the work factor can be
  raised later without invalidating existing wallets. Parameters below N = 2^14
  are rejected.
- **Test coverage is now 244 passing tests across all 17 workspace members.**
  The previous workflow ran the suites of six crates, 45 tests in total; the
  other crates had tests that CI never executed, or none at all. Every crate now
  covers its failure paths, not only its happy path: tampering, overflow, replay,
  malformed input, forged signatures and proofs, out-of-order blocks, and
  end-to-end HTTP round trips against a live server.

### Changed

- The CLI constructs nodes through `NodeConfig::new` and overrides the selected
  options, retaining node-owned defaults when additive policy/timeout settings
  are introduced. Current command behavior is unchanged (Q-014 consumer contract).

- **The on-disk ledger is now segmented (format v3).** A version 2
  `node-state.bin` is still read and validated, and the first `persist` migrates it
  to segments and renames it to `node-state.bin.v2-backup`; it is never loaded again
  once `ledger/manifest.json` exists. This removes the 64 MiB total-history cap and
  turns each block commit from a full-history rewrite into an append. State is also
  snapshotted every 1024 blocks rather than at every commit. Still outstanding and
  recorded in `ROADMAP.md`: loading reads every segment byte, the snapshot holds no
  receipt index, and there is no pruning command yet (S4.3–S4.4).
- **Proofs of possession use their own domain tag.** `BLSScheme::proof_of_possession`
  now hashes under `subhost-bls-pop-01` instead of reusing the protocol signature
  domain, so a proof can no longer be replayed as a signature over the encoded
  public key, and such a signature is no longer accepted as a proof. No node read
  or wrote proofs before this change, so no stored data is affected.
- **`subhost-rpc` is split into `state.rs`, `block.rs`, and `methods.rs`.** `lib.rs`
  now holds only crate docs and re-exports. No behaviour changed: the same 24 unit
  tests and 3 integration tests pass, and the gas-limit check from `cb062c0` lives
  in `block.rs`.
- **The block producer no longer rolls back by re-inserting evicted transactions.**
  Admission, replacement, eviction, and sealing all happen on a pool clone; the
  live pool is swapped only after the ledger write succeeds, so a failed commit
  leaves no partial mutation.
- **Gossip is now consumed.** `subhost-node` runs the network manager, publishes
  only blocks it sealed itself, and feeds inbound block proposals through
  `accept_block`. Received blocks are not re-broadcast.
- **`Mempool` is `Clone`**, which the candidate-pool commit path requires.
- **Workspace lints.** `unsafe_code = "forbid"`, `rust_2018_idioms`,
  `unreachable_pub`, `unused_qualifications`, `clippy::all`, and denied
  `todo!`/`unimplemented!`/`dbg!` across every crate.
- **CI covers the whole workspace.** The previous workflow ran clippy and tests
  on a hand-picked subset of six crates. There are now seven required jobs:
  format, clippy with `-D warnings` over all targets and features, the full test
  suite, the 1.89 MSRV, a release build, `cargo deny`, and warning-free rustdoc.
- **`cargo deny` gates the supply chain** through `deny.toml`: denied advisories
  and yanked crates, a permissive-licence allow-list, banned `openssl`, and a
  registry allow-list. Two maintenance notices are ignored, each with a written
  reason and an exit condition: `bincode` unmaintained (internal encoding only, the
  ledger is separately checksum- and commitment-verified) and `paste`
  unmaintained (a build-time proc-macro reached only through
  `libp2p-mdns`/`libp2p-tcp` -> `if-watch` -> `netlink-packet-*`). No advisory
  affecting runtime code is ignored.
- **`rust-toolchain.toml` pins the compiler** so every developer, CI runner, and
  container build uses one version.
- **MSRV corrected from 1.75 to 1.89.** The manifest claimed 1.75, but the locked
  dependency graph contains 39 edition-2024 crates that Cargo 1.75 cannot even
  parse, so the declared MSRV was never achievable. 1.89 is the true floor:
  `enum-ordinalize 4.4.2`, reached through `educe` -> `ark-ec`, declares
  `rust-version = 1.89`, so `cargo +1.88.0 check` refuses to build the workspace.
  Verified by running both `cargo +1.88.0 check` (fails) and `cargo +1.89.0 check`
  (clean) rather than by reading the manifests. CI verifies 1.89.
- **`reqwest` uses rustls** instead of the default system OpenSSL, so no build
  needs OpenSSL headers.
- **Internal crate dependencies are declared once** in
  `[workspace.dependencies]`.
- **Dependabot runs weekly**, groups minor and patch updates into one pull
  request, and covers Docker as well as Cargo and Actions.
- **`.gitignore` excludes key material and ledgers** (`wallets/`,
  `node-state.bin`, `genesis.json`, `faucet-wallet.json`) and no longer contains
  the four duplicated `website/` lines.
- **Added `.dockerignore`, `.editorconfig`, issue templates, a pull request
  template, and `monitoring/prometheus.yml`.**
- **`CODEOWNERS` names the security-critical paths** rather than assigning one
  global owner.
- **Demo scripts use `SUBHOST_HOME`** instead of overriding `HOME`, let the CLI
  resolve the nonce so a rerun does not need a hand-edited counter, and poll node
  readiness through `subhost query chain` instead of a raw `curl`.
- **README, SECURITY.md, and CONTRIBUTING.md rewritten** to match the code. The
  status table states what each crate does and does not do; the security policy
  separates the deliberate posture (no auth on any listener, no consensus,
  non-standard hash-to-curve, unverified IBC proofs) from the properties that are
  enforced and whose regression would be a real vulnerability.
