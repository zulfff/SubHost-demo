# Security review: PR #59 — S9.6 encrypted producer keystore

Reviewer: seat **nara, Security & QA (track-b)**. Date: 2026-11-19.
PR head: `yoga/encrypted-producer-keystore` (fetched as `pr-59-review`), base `main`.
Contract: Q-018 (agreed) + ADR-011. Scope reviewed: `crates/subhost-keystore/src/lib.rs`
(886 lines, read in full), `crates/subhost-cli/src/main.rs` producer/credential paths,
`docs/operations/producer-keystore.md`, ADR-011.

## Assessment

No blocking issues. The implementation is fail-closed and matches the agreed
contract. Recommend merge; one Low test/strictness gap below is worth fixing but
does not block.

## What was verified

- **AEAD authentication.** AES-256-GCM over the canonical 32-byte BLS scalar;
  purpose, version, public key, KDF params and salt are authenticated as
  domain-separated binary AAD (`associated_data`, fixed-width framing). The nonce
  is the AEAD nonce. Tamper tests cover pubkey/salt/kdf/nonce/ciphertext and all
  fail with `WrongPasswordOrCorrupt`.
- **KDF bounds before allocation.** `KdfParams::validate` enforces the allowlist
  (`log_n` 15..=18, `r=8`, `p=1`, `dklen=32`) and checked `128*N*r <= 256 MiB`,
  `N*r*p <= 2^22` before `scrypt::Params::new`. Ceilings match the wallet fix
  (#49), so the loaders cannot drift. Both edges pinned by tests.
- **Parse-before-KDF ordering.** `read_encrypted_producer_key` caps the password,
  does the bounded owner-only read, parses (purpose/version/lengths), then derives.
  Unknown fields, wrong lengths and trailing content fail before any KDF work.
- **Filesystem rules.** `O_NOFOLLOW`, fd-based `metadata()` (avoids path TOCTOU),
  regular-file check, owner-only mode (`mode & 0o077 == 0`), ownership check
  (`/proc/self` on Linux, documented parent-dir fallback elsewhere), bounded read
  (`limit+1`). Non-Unix returns `UnsupportedPlatform`, no silent bypass. Symlink
  test present.
- **No-clobber publication.** `0600` `create_new` tempfile, write+`sync_all`,
  atomic `hard_link` (fails on existing dest), temp removal, directory sync.
  Authenticated decrypt read-back compares against the source secret before
  success. Existing-file test present.
- **Canonical secret + pubkey binding.** `decode_secret_key` canonical nonzero
  decode; derived public key must equal the authenticated header key
  (`PublicKeyMismatch`).
- **Credentials.** One shared convention (`load_credential_file`): strip one
  trailing LF/CRLF, refuse embedded CR/LF/NUL, non-empty, 4 KiB cap after strip,
  owner-only regular non-symlink file. Creation floor 12 bytes; unlock applies no
  creation policy. No password-valued CLI argument for producer commands; the
  `password: String` args belong to the pre-existing wallet/transfer commands.
  Non-echo `rpassword` prompts (`/dev/tty`), creation prompts twice and must match.
  Passwords held in `Zeroizing`; no secret in errors/logs (error variants carry no
  secret material).
- **CLI flow.** `create` refuses beside a live legacy key and refuses an existing
  dest; `migrate` checks genesis validator membership before writing and preserves
  the legacy source on success and failure; `unlock`/`inspect` print the public key
  only, never the secret.
- **Zeroization.** Plaintext, derived key, JSON buffer and read-back comparisons
  use `Zeroizing`. Documented limitation: scrypt work buffers and `PrivateKey`
  clones outside the crate are not covered (at-rest protection only).

## Findings

### R-59-01 (Low) — duplicate-field rejection claimed but not isolated/enforced

The contract and ADR say "unknown/duplicate JSON fields fail", and
`malformed_json_is_refused_before_the_kdf` includes a "duplicate field" case.
That case writes `{"purpose":...,"purpose":...}` with all other required fields
missing, so it fails on missing fields, not on the duplicate. `serde_json` with
`deny_unknown_fields` accepts duplicate *known* fields (last-wins); it does not
reject them.

Impact: none on confidentiality/integrity — the AAD binds the same parsed values
used for decryption and the tag must verify, so a duplicate cannot bypass
authentication without the password. This is a format-strictness/test gap, not a
crypto bypass.

Recommendation: add a test that takes a complete valid keystore and adds a
duplicate field (e.g. a second `kdf`), then either enforce duplicate rejection
(custom deserializer) or soften the doc/ADR claim to "unknown fields fail;
duplicate known fields are last-wins and remain tag-authenticated". Confidence:
likely (serde_json last-wins); confirm with the isolating test.

### R-59-02 (Info) — AAD length framing

`associated_data` appends `public_key` and `salt` without length prefixes. Safe
today because `parse`/write enforce exact lengths (48/32) before the AAD is built,
so the framing is unambiguous. Length-prefixing these two fields would harden the
framing against any future change to field lengths. Optional.

### R-59-03 (Info) — non-Linux ownership fallback

On non-Linux Unix, ownership is inferred from the containing directory's owner
(weaker than `/proc/self`), documented as such; the owner-only mode check remains
the primary control. Acceptable for the supported platforms.

## Scope note (not a defect of this PR)

Encrypted node start-up is not wired yet: Track A still owes
`Node::bootstrap_with_producer_key` and the legacy loader fixes under ADR-011.
Until then `subhost node` reads the plaintext `producer.key`, and a migrated
directory keeps that legacy file. The threat model should treat the encrypted
producer key as partially landed (keystore crate + CLI present; node bootstrap
pending) rather than fully implemented.

## Evidence

- Source read in full: `crates/subhost-keystore/src/lib.rs`, producer/credential
  paths of `crates/subhost-cli/src/main.rs` (PR head `pr-59-review`).
- Contract: `docs/agents/questions/agreed/Q-018-producer-keystore-loader-contract.md`,
  `docs/agents/decisions/ADR-011-encrypted-producer-keystore.md`,
  `docs/operations/producer-keystore.md`.
- Gates: four-gate baseline on `fa87215` green
  (`2026-11-19-four-gate-baseline/fa87215/SUMMARY.md`); PR CI re-runs on its head.
---

## Re-review (head 5a4947e, 2026-11-19)

R-59-01 **resolved**. `parse` now first deserializes into a `StrictValue` whose
custom `Visitor` rejects duplicate object keys at every level (`visit_map` checks
`contains_key` before insert; nested values recurse), then converts to
`KeystoreFile`. Duplicate rejection happens before any KDF work.

Isolating test verified: `malformed_json_is_refused_before_the_kdf` builds a
complete valid keystore, injects a duplicate top-level `version` and a duplicate
nested `kdf` object, asserts `InvalidFormat` for each, and confirms the untouched
original still unlocks (so the refusal is the duplication, not fixture damage).

CLI tests added (10) covering the fail-closed flows: create/init share one
identity; init refuses a keystore without a credential file, a wrong credential,
and a loose credential file; init with a credential but no key refuses to
generate; migrate preserves source + identity, refuses a non-genesis key, refuses
without a source; unlock/inspect follow the credential convention; create refuses
a second identity beside a live key; creation password pair must match and meet
policy.

CI at re-review: Clippy/Format/Supply chain pass; Test pending.

**Verdict: approve** (security). R-59-01 closed; no blocking issues. Merge once
Test CI is green. R-59-02/R-59-03 remain informational, non-blocking.
