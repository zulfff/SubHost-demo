# ADR-011: Purpose-specific encrypted producer keystore

Status: Accepted · 2026-11-19

Contract thread: `docs/agents/questions/agreed/Q-018-producer-keystore-loader-contract.md`
(agreed 2026-11-19 with two Track B deltas). Implements the Track B slice of
S9.6; Track A lands the node bootstrap API and the legacy loader fixes under
this same ADR.

## Decision

A producer key is a validator's identity: whoever holds it can produce blocks.
S9.6 replaces the plaintext `producer.key` hex file with an encrypted keystore
owned by the new Track B crate `subhost-keystore`.

### Format

Purpose-specific JSON, version 1, purpose string `subhost-producer-keystore/v1`.
This is deliberately **not ERC-2335**: it uses AES-256-GCM, the repository's
canonical little-endian scalar encoding and a Subhost purpose string, and makes
no interoperability claim with Ethereum BLS keystores. Fields:

- `purpose`, `version` — format identity; unknown values fail before any KDF.
- `public_key` — 48-byte compressed G1 encoding of the derived public key.
- `kdf` — scrypt parameters (`log_n`, `r`, `p`, `dklen`), strict allowlist below.
- `salt` — 32 bytes, OS CSPRNG, regenerated on every write.
- `nonce` — 12 bytes, OS CSPRNG, regenerated on every write; this is the AEAD
  nonce, so changing it breaks the tag.
- `ciphertext` — 48 bytes: the canonical 32-byte secret
  (`BLSScheme::encode_secret_key`) plus the 16-byte AES-GCM tag.

Unknown/duplicate JSON fields, wrong lengths and trailing content fail before
KDF execution. Duplicate rejection is explicit: `serde_json`'s derived
`Deserialize` keeps the last value of a repeated field, so the file is first
parsed into a strict value that rejects duplicate object keys at every level,
then converted into the typed record (R-59-01). The file is capped at 16 KiB.
No chain/genesis binding and no proof of possession live in the file: genesis
membership is a bootstrap check, the public key is authenticated, and genesis
independently supplies the PoP for the active signing suite.

Purpose, version, public key, KDF parameters and salt are authenticated as
AES-GCM associated data using an explicitly specified, domain-separated binary
framing (fixed-width integers and lengths, not incidental JSON serialization).
Downgrading the KDF, swapping the public key or touching the salt breaks the
tag rather than slipping past a cheap check.

### KDF bounds

Accepted scrypt parameters: `log_n` 15..=18, `r = 8`, `p = 1`, `dklen = 32`;
default `log_n = 17` (128 MiB ROMix working set). Before `scrypt::Params::new`
and before any work buffer is allocated, checked estimates enforce
`128 * N * r <= 256 MiB` (inclusive: `log_n = 18, r = 8` sits exactly on the
ceiling and is accepted) and `N * r * p <= 2^22`. These are the same ceilings
as the wallet fix (SUBHOST-2026-002, PR #49), so the two loaders cannot drift
apart silently. The bounds constrain the KDF working set, not total process RSS
or guaranteed unlock latency. Measured on the development machine (release
profile, 2026-11-19, `cargo run -p subhost-keystore --example measure-cost`,
disposable fixture material): default `log_n = 17` unlock ≈ 0.51 s, write
including the authenticated read-back (two derivations) ≈ 0.93 s. The
default/cost range remain policy; other hardware will differ.

### Passwords and credentials

- Passwords are exact byte strings at the library boundary, capped at 4 KiB
  before any KDF work, on creation and on unlock. No trimming or normalization
  happens in the crate.
- Creation requires at least 12 bytes. This is a length floor only; no
  entropy-strength claim is made. Quality checks apply at creation, never as a
  restriction on decrypting an existing valid file.
- Interactive CLI input is UTF-8 exactly as the terminal reader returns it,
  without Unicode normalization. Creation prompts twice without echo
  (`rpassword`, reads `/dev/tty`); unlock prompts once.
- Service/script input uses an explicitly supplied owner-only credential file:
  exactly one trailing LF or CRLF is stripped, embedded CR/LF/NUL are refused,
  every other byte (including spaces) is preserved, and the cap is checked
  after stripping. This one convention applies wherever the CLI reads a
  credential file.
- There is no password-valued CLI argument (the wallet's plain `--password`
  pattern is deliberately not reused), and no password contents in NodeConfig,
  Debug output, errors or logs. Missing credentials fail before any validator
  activity and never create a key. Unlock is serial and local; there is no RPC
  password endpoint.

### Filesystem rules

- Reads open with `O_NOFOLLOW`, require a regular file, an owner-only mode
  (no group/other bits) and an ownership check, and use bounded reads even if
  metadata races. Ownership compares the file's uid against the process's
  effective uid via `rustix::process::geteuid` (a safe wrapper; the locked
  libc marks `geteuid` unsafe and this workspace forbids `unsafe`) on every
  Unix, so a root process loading another user's key is refused.
  Non-Unix platforms fail explicitly with `UnsupportedPlatform`; there is no
  silent permission bypass.
- Writes publish a `0600` sibling tempfile, sync it, publish with a no-clobber
  hard link (an existing destination is never replaced), remove the tempfile,
  and sync the directory. Cleanup never deletes a preexisting destination.
- Every write is verified by an authenticated decrypt read-back that must
  reproduce the written secret and derive the authenticated public key.
- **Durability uncertainty (as in ADR-009):** a directory-sync failure after
  the hard link, or a failed read-back, is reported as an error even though the
  complete destination may already be visible (`PublishedButUnsynced`,
  `ReadBackMismatch`). The errors say the file may exist and must be inspected
  before retrying; no-clobber publication refuses a blind recreation. This is
  documented durability uncertainty, not a successful write.

### Migration and compatibility

- Migration is an **explicit CLI command** (`subhost producer migrate`), not
  automatic at start-up. This records the scope change against ROADMAP S9.6
  ("migrate an existing key file at start-up"): automatic passwordless
  migration is impossible, and an explicit command with credentials and the
  same preservation rules is the agreed first step. A future startup migration
  option would need its own agreed decision.
- Migration loads the original canonical plaintext key, validates the selected
  genesis when one exists (the key must be a genesis validator), creates a
  separate encrypted destination, and decrypts/compares it before reporting
  success. The source is preserved on success and on every failure. Migration
  never regenerates a key and never overwrites an existing encrypted file.
- Removing the legacy file is an explicit later operation; ordinary deletion
  cannot promise physical erasure on SSD/COW/backup systems.
- Encrypted files have **no implicit plaintext fallback**. Plaintext remains
  only the explicit legacy/demo compatibility path; conversion does not make
  the legacy BLS signature suite safe. Unknown format, bad tag/wrong password,
  bad scalar, public-key mismatch and missing credentials all fail closed.

### Memory hygiene

Password, plaintext, derived-key, encoding and JSON buffers owned by the crate
are wrapped in `zeroize::Zeroizing` and wiped on success and error. Residual
exposure is documented, not hidden: scrypt's internal work buffers, the AES-GCM
key schedule and cloneable BLS `PrivateKey`/`Scalar` instances outside the
crate are not covered by these wrappers. Encryption protects the file at rest;
it does not protect a running node. Track A reviews the BLS key drop/clone
lifecycle with the bootstrap API.

## Consumer contract

- **Track A (landed):** `Node::bootstrap_with_producer_key(config, key)`
  accepts an already unlocked BLS key and still verifies canonical encoding,
  derived public key, unique genesis membership and corrected-suite genesis
  PoPs (the registry verifies PoPs before the key is resolved). Unlocking never
  bypasses validator configuration or journal recovery: it runs the same
  bootstrap path, and a supplied key wins over any configured file, which is
  never read. Supplying a key to a non-validator configuration is an error.
  Normal bootstrap remains usable by observers without key material.
- **Track A (landed): legacy loader hardening.** `read_producer_key` now reads
  through `subhost_keystore::read_bounded_owner_only`, so the legacy hex file
  gets the same rules as a keystore: `O_NOFOLLOW` on the opened handle, a
  regular file, owner-only mode, effective-uid ownership and a 256-byte bound.
  Symlinks are refused outright, non-Unix platforms fail explicitly, and the
  node's separate `stat`-then-open permission check is gone. Permission-bit
  refusals keep the existing `InsecureProducerKey` variant; the other refusals
  surface as `UnsafeProducerKey { path, reason }`.
- **CLI node start-up (landed):** `subhost node --validator
  --producer-credential-file FILE` unlocks the keystore (default
  `<data-dir>/producer.keystore.json`, override with `--producer-key-file`)
  and boots through `bootstrap_with_producer_key`. Fail-closed: a missing
  keystore, a wrong/loose credential or a non-keystore file refuses start-up
  before any validator activity; there is no plaintext fallback and no key
  generation. Without the flag, a migrated data directory still starts from
  the preserved legacy `producer.key`, read through the hardened loader.
- **CLI init (landed here):** `subhost init --producer-credential-file` routes
  an existing producer key file by content. A keystore is unlocked with the
  credential file and fails closed without one; a legacy hex file keeps the
  original loader. With a credential file but no key file, init refuses rather
  than generating: encrypted keys are created explicitly with
  `subhost producer create`. The init consumer reads the key file twice — once
  to route, once through the loader — and both reads now use the same hardened
  owner-only reader, so the symlink window between them is closed.

## Rejected alternatives

| Option | Why rejected |
| --- | --- |
| ERC-2335-compatible BLS keystore | AES-128-CTR/checksum modules, NFKD password rules and key-encoding conversion need an explicit interoperability implementation; no current consumer needs interchange. |
| External signer/HSM only | Useful later, but needs a new signing/timeout/recovery interface beyond this file-loader stage. |
| Reuse the wallet keystore format | Wallet files are ed25519 with a different purpose and a KDF validation that lacks an application resource ceiling; producer identity must not share that surface. |

## Evidence

- Q-018 agreed thread and replies (`docs/agents/replies/Q-018-track-b.md`,
  `Q-018-track-a.md`) — contract, deltas and acceptance, 2026-11-19.
- https://www.rfc-editor.org/rfc/rfc7914 — scrypt `128*r*N` ROMix storage and
  parameter constraints; basis for the checked ceilings.
- https://github.com/RustCrypto/AEADs/blob/aes-gcm-v0.11.0/aes-gcm/src/lib.rs —
  AES-GCM AAD semantics and unique-nonce requirement; the zeroize-gated code
  wipes an intermediate GHASH key, not the whole key schedule.
- https://github.com/RustCrypto/password-hashes/blob/scrypt-v0.11.0/scrypt/src/lib.rs —
  KDF allocates parameter-dependent work buffers; callers must bound cost.
- https://ercs.ethereum.org/ERCS/erc-2335 — why this format must not claim
  ERC-2335 compatibility.
- https://docs.rs/rpassword/7.5.5 — prompt reads `/dev/tty` (fails cleanly
  without a terminal), MSRV 1.85 within workspace 1.89.
- PR #49 (SUBHOST-2026-002) — the 256 MiB / 2^22 ceiling pattern reused here.