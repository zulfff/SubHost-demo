# Encrypted producer keystore

S9.6 (ADR-011) adds an encrypted keystore for the BLS producer key, the
validator's identity. The crate is `subhost-keystore`; the CLI surface is
`subhost producer …` plus `subhost init --producer-credential-file`.

**Status:** complete. Creation, inspection, unlock, migration and genesis init
consume the keystore, and **encrypted node start-up is available**:
`subhost node --validator --producer-credential-file FILE` unlocks
`<data-dir>/producer.keystore.json` (or `--producer-key-file`) and boots via
`Node::bootstrap_with_producer_key` with the same membership and
proof-of-possession checks as a file-loaded key. The legacy `producer.key`
loader uses the keystore crate's hardened owner-only reader (no symlinks,
owner-only mode, effective-uid ownership, bounded read, explicit non-Unix
refusal). Without the credential flag, `subhost node` keeps reading the
plaintext `producer.key`, a migrated data directory still starts from that
preserved legacy file until the operator switches to the encrypted path.

## Commands

```sh
# New identity, encrypted from birth (default <data-dir>/producer.keystore.json).
# Prompts twice without echo; refuses to replace any existing file.
subhost producer create --data-dir ~/.subhost/data

# Non-interactive creation/unlock: an owner-only credential file.
subhost producer create --data-dir DIR --credential-file /run/secrets/producer.pass

# Public key only; does not verify the tag (header read).
subhost producer inspect DIR/producer.keystore.json

# Verify a password unlocks the file; prints the public key, never the secret.
subhost producer unlock DIR/producer.keystore.json --credential-file FILE

# Encrypt an existing plaintext producer.key into a separate keystore.
subhost producer migrate --data-dir DIR --credential-file FILE

# Genesis init consuming the keystore (proofs of possession are signed with it).
subhost init --data-dir DIR --validator ADDRESS:10 \
    --producer-credential-file /run/secrets/producer.pass

# Encrypted node start-up: unlock the keystore and boot with the key.
subhost node --validator --data-dir DIR \
    --producer-credential-file /run/secrets/producer.pass
```

There is **no password argument**. Passwords come from non-echo terminal
prompts (`/dev/tty`; creation asks twice and the entries must match) or from an
owner-only credential file. Without a terminal, use `--credential-file`.

With `--producer-credential-file`, `init` targets
`<data-dir>/producer.keystore.json` by default (override with
`--producer-key-file`). Without a credential, `init` refuses when a keystore
is present rather than shadowing it with a newly generated plaintext key.

## Credential file convention

One shared convention everywhere the CLI reads a credential file:

- exactly one trailing `LF` or `CRLF` is stripped;
- embedded `CR`, `LF` or `NUL` are refused;
- every other byte is preserved, including leading/trailing spaces;
- the result must be non-empty and at most 4 KiB;
- the file itself must be a regular, owner-only (`0600`), non-symlink file
  owned by the running user, the same rules as the keystore.

Create it without leaving the password in shell history, e.g.:

```sh
install -m 600 /dev/null /run/secrets/producer.pass
# write the password into the file with an editor or a secret manager;
# a single trailing newline is fine and will be stripped.
```

Creation requires at least 12 bytes. That is a length floor, not an entropy
guarantee: use a real passphrase or a generated secret. Unlock applies no
creation policy: an existing valid file must stay decryptable.

## Migrating an existing validator

```sh
subhost producer migrate --data-dir ~/.subhost/data --credential-file FILE
```

What migration does, in order:

1. Reads the legacy `producer.key` (canonical scalar, owner-only mode).
2. Refuses if the destination keystore already exists, it never replaces one.
3. If a genesis exists, refuses unless the key is one of its validators.
4. Writes the encrypted keystore with no-clobber publication, file and
   directory sync, and an authenticated decrypt read-back comparing against
   the source secret.
5. Reports success only after the read-back matches.

The plaintext source is **preserved on success and on every failure**. Remove
it explicitly when you are ready (`shred`/secure erase where the storage
supports it); ordinary deletion cannot promise physical erasure on
SSD/COW/backup systems. Until Track A's bootstrap loader lands, keep the legacy
file if the node still needs to start from it.

If a write reports a post-publication failure (directory sync or read-back),
the destination **may already exist and be complete**. Inspect it
(`subhost producer inspect` / `unlock`) before retrying; no-clobber publication
refuses a blind recreation, which is safe but intentional.

`subhost producer create` refuses to run next to a live `producer.key` at the
default destination: creating a second identity beside the first is almost
always a mistake: migrate instead. An explicit `--keystore-file` destination
is allowed for genuinely separate identities.

## File format and bounds

JSON v1, purpose `subhost-producer-keystore/v1` (a Subhost format, **not**
ERC-2335). scrypt-derived AES-256-GCM over the canonical 32-byte BLS scalar;
purpose, version, public key, KDF parameters and salt are authenticated as
domain-separated binary associated data. Unknown fields, duplicate fields
(at any nesting level) and trailing content are refused before any KDF work.
Accepted scrypt costs: `log_n`
15..=18, `r = 8`, `p = 1`, `dklen = 32`; default `log_n = 17` (~128 MiB,
roughly half a second per derivation). Checked estimates cap the KDF working
set at 256 MiB before any allocation, the same ceiling as the wallet fix
(SUBHOST-2026-002). Files are capped at 16 KiB, passwords at 4 KiB.

Every input is opened with `O_NOFOLLOW` and checked for regular-file type,
owner-only mode and ownership; reads are bounded. Non-Unix platforms are
refused explicitly (`UnsupportedPlatform`) rather than silently skipping
permission checks.

To measure the default cost on your own hardware (disposable fixture key,
never a live identity):

```sh
cargo run --release -p subhost-keystore --example measure-cost
```

On the development machine this is ≈ 0.5 s per unlock and ≈ 0.9 s per write
(the write pays two derivations because of the authenticated read-back).

## Security scope

- Encryption protects the key **at rest**. A running node holds the unlocked
  key in memory; this feature does not change that.
- Owned password/plaintext/derived-key buffers are zeroized. Residual
  exposure remains in scrypt's internal work buffers, the AES-GCM key schedule
  and cloneable BLS scalar copies outside the crate (ADR-011 documents this).
- Wrong password, tampered header, unknown version/purpose, bad scalar,
  public-key mismatch, loose permissions, symlinks and missing credentials all
  fail closed. There is no plaintext fallback for an encrypted file.
- Losing the password loses the validator identity: there is no recovery path
  and no escrow. Back up the credential through your secret manager, not into
  the repository or shell history.

## Tests

`cargo test -p subhost-keystore -p subhost-cli` covers: round-trip and default
cost; wrong password; header tamper (public key, salt, KDF, nonce, ciphertext);
version/purpose refusal; malformed/unknown/duplicate/trailing JSON; file and
password bounds; symlink refusal; no-clobber publication; KDF allowlist edges;
the credential convention; creation policy; and the CLI identity chain
(create → init → genesis PoP), fail-closed init paths, migration preservation,
genesis membership and read-back.