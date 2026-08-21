# Changelog

This file records the concrete changes made during the audit-and-hardening pass on
this repository. Every entry below was actually applied and verified with
`cargo check --workspace` and targeted `cargo test`. Anything **not** listed was
left as-is.

Verification at the end of the pass: `cargo check --workspace` is clean (0
errors, 0 warnings) and the following test suites pass:

- `subhost-crypto`: 7 tests
- `subhost-consensus`: 5 tests
- `subhost-mempool`: 7 tests
- `subhost-state`: 8 tests
- `subhost-rpc`: 2 tests
- `subhost-ibc`: 5 tests

---

## 1. Crypto (`subhost-crypto`)

- **Implemented `key_exchange` (X25519).** It was a stub returning all-zero keys
  and all-zero shared secrets. Now generates real X25519 keypairs and computes
  real Diffie-Hellman shared secrets via `x25519-dalek`.
- **Added `shared_secret_checked`** (contributory-behavior enforcement): rejects
  all-zero/low-order peer public keys and all-zero shared secrets with
  `CryptoError::InvalidPublicKey`. The plain `shared_secret` is kept for
  compatibility and documented as unsafe for untrusted peers.
- **Added BLS proof-of-possession** (`proof_of_possession` / `verify_possession`)
  to prevent the classic rogue-key attack when aggregating public keys.
- **Added domain separation** in `hash_to_g2` (now SHA3-384 with a fixed domain
  tag instead of SHA3-256 with none).
- New `CryptoError::InvalidPublicKey` variant.
- `Cargo.toml`: added `x25519-dalek` dependency.

## 2. Wallet (`subhost-wallet`)

- **Zeroize the plaintext private-key buffer** after decryption (both the success
  copy and the error path), so the raw key no longer lingers on the heap.
- Note: `scrypt::Params::new(15, 8, 1, 32)` was checked against the crate's
  `Params::recommended()` and matches it — not weakened.

## 3. Consensus (`subhost-consensus`)

- **Wired the staking module**: `staking.rs` existed but was never declared in
  `lib.rs`, so it was never compiled or tested. Added `pub mod staking;`.
- **Fixed `slash()`** so it actually deducts stake and removes the validator when
  fully slashed (previously it only computed and returned an amount without any
  effect).
- **Fixed integer-underflow panic** in `ConsensusConfig::new` for
  `validator_count == 0` (now guarded).
- **Fixed `has_quorum_support` semantics**: it now counts distinct validators in
  the *next* round that reference the vertex (Narwhal-style support), instead of
  counting the vertex's own parents (fan-in).
- Added tests for slash deduction and full-slash removal.

## 4. Mempool (`subhost-mempool`) — rewritten from a stub

The crate previously contained only an empty `Config/Module/Metrics/Error`
template. It is now a real transaction pool with:

- per-sender nonce map and dedupe by tx hash
- replace-by-nonce (only a strictly higher gas price replaces)
- global capacity cap with lowest-priority eviction
- per-sender queue cap
- gas-price/length validation and ordering for a proposer
- `remove`, `get`, `len`, `pending`, `next_nonce_for`
- `Cargo.toml`: added `bincode` for deterministic tx hashing.

## 5. State (`subhost-state`) — rewritten from a stub

Now a real in-memory account store with:

- `Account { nonce, balance }`, seeding, balance/nonce queries
- `apply_transfer` with overdraft protection
- `apply_transaction` with nonce/replay enforcement and fee accounting
- explicit unsupported-type errors (not silent coercion).

## 6. JSON-RPC (`subhost-rpc`)

- Replaced `tokio::sync::RwLock` + `blocking_read` (which panics inside the tokio
  worker) with `std::sync::RwLock`.
- `eth_getBalance` now reads the real `subhost-state` balance and accepts the
  spec's optional 2nd `block` parameter.
- `eth_sendTransaction` now parses hex fields (address/value/nonce/gas/data/chain
  id) and inserts a real `Transaction` into the mempool, returning a real hash.
- `eth_blockNumber` reads a real atomic height counter (start 0), not a hardcoded
  value.
- `eth_getTransactionReceipt` returns `null` (not fabricated `status: 0x1`) since
  there is no confirmation/block-production backend.
- Removed the `block_in_place` + nested `block_on` antipattern.

## 7. Faucet (`subhost-faucet`)

- Fixed a rate-limit bypass: the cooldown was keyed on the case-sensitive address,
  so flipping hex-case reset the limit. Now normalized to lowercase.
- (Still returns a placeholder `tx_hash`; it does not credit a live chain — noted
  in README.)

## 8. Networking (`subhost-network`)

- Fixed the dropped receiver: `NetworkManager::new` used to create the channel
  and immediately drop the receive half, making every `send()` fail. The receiver
  is now stored and inbound `NetworkMessage`s are broadcast on a gossip topic.
- Switched to `IdentTopic` (the `Topic` type is generic in libp2p 0.53).
- `NetworkMessage` now derives `Serialize`/`Deserialize`.
- `Cargo.toml`: added `serde_json`.

## 9. Benchmark tool (`subhost-bench`)

- `--endpoint` is now actually used by the TPS/latency/load commands (previously
  hardcoded to `http://localhost:8545`).
- Removed an unneeded `mut`.

## 10. CLI (`subhost-cli`)

- Fixed a clap panic: `node --validator` short `-v` collided with global
  `--verbose` `-v`.
- `run_node` now actually starts the JSON-RPC server on `--listen`.
- `tx send` now builds a real `Transaction` and prints its hash (consistent with
  the RPC mempool hashing).
- `wallet export` now really decrypts and prints the private key.
- `query balance` no longer fabricates `0` — it warns that it needs a running node.
- `init` now warns when it writes a genesis with zero validators (which
  `GenesisConfig::validate()` rejects).
- Removed unused imports.

## 11. IBC (`subhost-ibc`)

- Removed an unused `Hash` import.
- Rejects packets from the wrong counterparty channel/port.
- Rejects expired or zero-timeout packets, replayed acknowledgements, invalid
  ordered sequences, oversized payloads, and sequence overflow.

## 12. Repository guardrails

- Added a tracked `Cargo.lock` workflow (the lockfile is no longer ignored).
- CI now enforces formatting, workspace type-checking, safe targeted tests,
  clippy with `-D warnings`, and `cargo audit`.
- Added Dependabot, CODEOWNERS, a security disclosure policy, and a two-job
  Cargo concurrency default for the constrained build environment.

## 13. Documentation (de-hallucination)

- `README.md`: corrected the license badge (MIT -> Apache-2.0), marked audits as
  planned/scheduled (none completed), corrected the install binary name
  (`subhost-web3` -> `subhost`), and replaced inflated marketing claims (50k TPS,
  parallel EVM, zk-rollups, encrypted mempool, etc.) with an honest "Current
  Status" table.
- `docs/tokenomics.md`: added a "design spec, not deployed" banner; reconciled
  contradictory numbers (minimum stake 1,000 vs 10,000 vs 10,000,000; circulation
  15% vs allocation 100%).
- `docs/security/threat-model.md`: added an implementation-status disclaimer
  (many mitigations are design goals, not live); removed the bug-bounty section
  entirely (there is no bug bounty program).

---

## Known limitations left intentionally (documented, not hidden)

1. `subhost-consensus` `HotStuff::validate_qc` fails closed until a validator
   public-key registry is wired. It does not accept arbitrary signature bytes.
2. RPC transaction submission requires an Ed25519 signature and exact account
   nonce, but there is no server-side keystore or client-side signing helper.
3. `subhost-faucet` returns a placeholder tx hash and does not credit real state.
4. `subhost-network` builds a libp2p swarm with `with_async_std()` while the rest
   of the app runs on tokio (latent runtime mismatch; not exercised by the CLI).
5. `subhost-evm`, `subhost-wasm`, `subhost-zk` are placeholders (not implemented).

## Follow-up fixes (second pass — reconciling GPT 5.6's changes)

- Reverted an inert `revm` workspace-dependency change (`default-features = false`,
  `secp256k1`): `revm` is not referenced by any workspace crate, so the change had
  no effect and was reverted to `features = ["std", "serde"]`.
- Aligned wallet address derivation with the RPC signature gate: the wallet now
  derives an address from the ed25519 **public** key
  (`Address::from_public_key(ed25519 verifying key)`) instead of hashing the raw
  32-byte secret. Added `PrivateKey::public_key()`. This removes the mismatch where
  a wallet address (`blake3(secret)`) could never satisfy the RPC check
  (`blake3(public_key)`). Wallet's unused `blake3` dependency was removed.
- Normalized the security contact email to `security@subhost.xyz` (SECURITY.md
  previously used `.io`; README and threat-model use `.xyz`).
- Documented in README that `eth_sendTransaction` is non-standard (requires
  `publicKey` + `signature` fields).
- Added a wallet regression test locking the address/public-key invariant.
