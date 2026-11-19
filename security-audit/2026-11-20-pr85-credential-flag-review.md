# Security review: PR #85, S9.6 completion, `subhost node --producer-credential-file`

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-20.
Status: reviewed at PR head, subsequently **merged** (main `7a380f3`).
PR head: `yoga/node-producer-credential` (fetched as `pr-85-review`), base `main`.
Scope: `crates/subhost-cli/src/main.rs` (+126/-1), ADR-011 /
`docs/operations/producer-keystore.md` / `start-stop.md`. Tech-lead approved;
this is the independent security record. Completes S9.6: keystore crate + CLI
(#59), node bootstrap API + hardened loader (#79), and this unlock flag.

## Assessment

**No blocking issues. APPROVE.** The unlock path is fail-closed end to end and
adds no new trust surface: it reuses the #59 keystore primitives and the #79
supplied-key bootstrap, both previously reviewed.

## What was verified

- **Fail-closed, no fallback, no keygen.** With the credential flag the node
  unlocks or bails; there is no plaintext fallback. A missing keystore refuses
  start-up with `producer create` / `producer migrate` guidance, and the test
  asserts neither a keystore nor a legacy key file is created. A legacy hex file
  at the keystore path is refused (routing hint plus strict parser).
- **Owner-only bounded credential read.** `load_credential_file` uses the shared
  `read_bounded_owner_only` (O_NOFOLLOW, regular-file check, owner-only mode,
  effective-uid ownership, bounded read on the opened handle), returns
  `Zeroizing<Vec<u8>>`, strips exactly one trailing LF/CRLF, refuses embedded
  CR/LF/NUL and empty results, and enforces the password cap. The keystore file
  itself is read through the same bounded owner-only path.
- **Supplied key inherits #79 validation.** The unlocked key goes through
  `bootstrap_with_producer_key`: canonical encode/decode round-trip, exactly-one
  genesis match, and proof-of-possession via `ValidatorRegistry::from_genesis`
  before authority resolution. The configured key file is never read.
  `requires = "validator"` is enforced at clap and re-checked node-side.
- **No secrets in errors or logs.** Error contexts carry file paths only; the
  password is zeroized on drop. No key material reaches the node's log surface.
- **Tests present.** Validator requirement; default and `--producer-key-file`
  override keystore paths; wrong credential fail-closed; group-readable
  credential refused; missing keystore refuses without generating; legacy hex
  refused.

## Informational notes (non-blocking)

- `--producer-key-file` doubles as the keystore-path override when the credential
  flag is present; documented in the flag help and `start-stop.md`.
- The `keystore.exists()` pre-check is UX only; security enforcement is fd-based
  inside the keystore crate at read time (no stat-then-open gap).
- CI green except a Vercel deployment rate-limit (infra, unrelated to code).

## Activation-gate context

With #79 merged and this PR, the S9.6 "encrypted producer key for node start-up"
activation-gate item is complete. The remaining gate item is SUBHOST-2026-003
fix 1 (calldata pricing, Q-037 L2 floor model, confirmed). Public testnet
activation (S12.3) still waits on that fix plus the re-verification.

## Evidence

Read at PR head `pr-85-review`: `crates/subhost-cli/src/main.rs`
(`unlock_producer_keystore`, `run_node` dispatch, fail-closed tests),
`crates/subhost-keystore/src/lib.rs` (`load_credential_file`,
`read_encrypted_producer_key`, `read_bounded_owner_only`). Related:
`2026-11-19-pr59-keystore-review.md`, `2026-11-20-pr79-producer-bootstrap-review.md`,
ADR-011.