# Changelog

All notable changes to this repository. Every entry below was applied and
verified; nothing is aspirational.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Full-repository audit and hardening pass. Verified with `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
(clean), and `cargo test --workspace --all-features` (all suites green).

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
  authors among a vertex's own parents — its ancestry — which any vertex can
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

- **`subhost-storage` is a real crate.** The ledger format was inline in
  `subhost-rpc`. It is now a documented, separately tested crate: versioned
  magic-and-checksum envelope, atomic write with directory fsync, `0600`
  permissions, size bound, and full replay of every block and receipt commitment
  against the restored state on load. Nine tests, including bit-flip, truncation,
  wrong-magic, wrong-chain, and eight distinct tampering cases.
- **`subhost-node` is a real crate.** Node bootstrap — genesis load, ledger
  restore, one-time allocation application, RPC and metrics wiring, and graceful
  SIGTERM/Ctrl-C shutdown — lived in the CLI's `main.rs`. Eight tests.
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
- **Test coverage is now 202 passing tests across all 17 workspace members.**
  The previous workflow ran the suites of six crates, 45 tests in total; the
  other crates had tests that CI never executed, or none at all. Every crate now
  covers its failure paths, not only its happy path: tampering, overflow, replay,
  malformed input, and end-to-end HTTP round trips against a live server.

### Changed

- **Workspace lints.** `unsafe_code = "forbid"`, `rust_2018_idioms`,
  `unreachable_pub`, `unused_qualifications`, `clippy::all`, and denied
  `todo!`/`unimplemented!`/`dbg!` across every crate.
- **CI covers the whole workspace.** The previous workflow ran clippy and tests
  on a hand-picked subset of six crates. There are now seven required jobs:
  format, clippy with `-D warnings` over all targets and features, the full test
  suite, the 1.89 MSRV, a release build, `cargo deny`, and warning-free rustdoc.
- **`cargo deny` gates the supply chain** through `deny.toml`: denied advisories
  and yanked crates, a permissive-licence allow-list, banned `openssl`, and a
  registry allow-list. Four advisories are ignored, each with a written reason and
  an exit condition: `bincode` unmaintained (internal encoding only, the ledger is
  separately checksum- and commitment-verified), and three
  `hickory-proto`/`paste` advisories that arrive only through `libp2p 0.56`, which
  pins the unfixed version. `cargo tree -i libp2p` confirms `subhost-network` is
  its only dependant and no binary depends on `subhost-network`, so that code is
  unreachable in a shipped artifact.
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
