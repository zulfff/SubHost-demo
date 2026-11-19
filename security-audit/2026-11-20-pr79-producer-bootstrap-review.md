# Security review: PR #79 — S9.6 Track A producer bootstrap API + hardened legacy loader

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-20.
PR head: `s9/producer-bootstrap-api` (fetched as `pr-79-review`), base `main`.
Scope: `crates/subhost-node/src/lib.rs` (+162/-28), `basic_tests.rs` (+123),
ADR-011 / `docs/operations/producer-keystore.md`. This is the Track A half of S9.6
(the keystore crate + CLI landed in #59); it wires the unlocked key into node
bootstrap and hardens the legacy plaintext loader. Gates reported green (628 tests).

## Assessment

**No blocking issues.** This is a net security improvement: it closes a silent
non-Unix permission bypass and a stat-then-open TOCTOU in the legacy loader, and
adds a fail-closed supplied-key bootstrap path. Recommend merge once CI is green.

## What was verified

- **Supplied-key bootstrap is fail-closed.** `bootstrap_with_producer_key` rejects
  a non-validator configuration; a supplied key with no genesis validator set is an
  error. `resolve_supplied_producer` enforces a canonical encode/decode round-trip
  (non-canonical scalar refused), derives the public key, and requires **exactly
  one** genesis match (`AmbiguousSuppliedProducer` / `UnknownSuppliedProducer`).
- **PoP is not bypassed.** `ValidatorRegistry::from_genesis` (proof-of-possession +
  corrected-suite verification) runs at `lib.rs:237`, **before** the authority
  resolution. The supplied secret must derive a genesis-registered public key whose
  PoP was already verified, so unlocking a keystore cannot bypass PoP. Verified by
  reading the call order, not just the doc comment.
- **Unlocking bypasses nothing else.** The supplied key flows through the same
  `bootstrap_inner`; validator configuration, journal recovery and certified-history
  reconciliation run unchanged. A supplied key wins over any configured file, which
  is **never read** (test: `a_supplied_producer_key_never_reads_the_configured_key_file`).
  No password or plaintext path reaches the node.
- **Legacy loader hardened.** `read_producer_key` now reads through
  `subhost_keystore::read_bounded_owner_only`: `O_NOFOLLOW`, regular-file check,
  owner-only mode, effective-uid ownership, and a bounded read **on the opened
  handle** (no separate `stat` + second open). This closes the stat-then-open TOCTOU
  and the init consumer's double-read window, and brings the legacy path to parity
  with the keystore path so they cannot drift.
- **Non-Unix silent bypass removed.** The old `#[cfg(not(unix))]
  check_producer_permissions` returned `Ok(())` (no permission check). It is deleted;
  non-Unix now fails explicitly via `UnsupportedPlatform` → `UnsafeProducerKey`.
  This is a real fix, not just a refactor.
- **Bounded read.** `MAX_PRODUCER_KEY_FILE_BYTES = 256` (64 hex digits + optional
  `0x` + newline + whitespace headroom); an attacker-sized file is refused
  (`FileTooLarge`). UTF-8 validated before hex decode; canonical scalar decode kept.
- **Error surface.** `producer_read_error` maps keystore refusals to named node
  errors (permissions keep their dedicated variant; symlink/special-file, foreign
  owner, unsupported platform, oversized each name an actionable reason). No secret
  material in errors.

## Test coverage (verified present)

Symlinked key refused; oversized key file refused; non-regular file refused;
others-can-read refused; malformed key refused; outside-genesis refused (file and
supplied paths); supplied key boots a validator without any key file; supplied key
requires a validator configuration; supplied key never reads the configured file;
writer round-trips and never overwrites.

## Informational notes (non-blocking)

- The supplied-key canonical round-trip is defense-in-depth (the keystore already
  decodes canonically); keeping it is correct.
- `MAX_PRODUCER_KEY_FILE_BYTES = 256` assumes the legacy hex format; if a future
  format legitimately needs more headroom the bound must be revisited deliberately,
  not silently widened.

## Activation-gate context

This closes the S9.6 "encrypted producer key for node start-up" item (the keystore
crate + CLI in #59, the bootstrap API + hardened loader here). The separate
activation-gate item is the SUBHOST-2026-003 calldata-pricing residual (Q-037 L2
floor model, confirmed). Both must land before public testnet activation; this PR
is one of the two.

## Evidence

- Read at PR head `pr-79-review`: `crates/subhost-node/src/lib.rs`
  (`bootstrap_with_producer_key`, `bootstrap_inner`, `resolve_supplied_producer`,
  `read_producer_key`, `producer_read_error`, `from_genesis` call order at :237),
  `basic_tests.rs` (fail-closed suite).
- Related: #59 keystore review (`2026-11-19-pr59-keystore-review.md`), ADR-011, Q-018.
- Gates: PR reports green (628 tests); four-gate baseline reference
  `2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`.