# SubHost-demo security finding: wallet files control scrypt cost, so a crafted wallet aborts or hangs the process

- Target: `SubHost-demo`
- Commit: `4be9457e18a37537270223cbdbf2e9ab94955fe8`
- Date: 2026-11-18
- Finding ID: `SUBHOST-2026-002`
- Severity: Medium
- CVSS v4 base: `6.7` (`CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:A/VC:N/VI:N/VA:H/SC:N/SI:N/SA:N`)
- CWE: CWE-400 (Uncontrolled Resource Consumption), CWE-770 (Allocation of Resources Without Limits or Throttling)
- Status: Fixed. Originally confirmed against the release CLI binary with two
  crafted wallet files and two controls on `4be9457`; re-verified 2026-11-19
  against main `fa87215` (PR #49 merged), where both attack cases exit 1 with
  `WeakKdfParameters` and no abort. See Re-verification below.

## Summary

A wallet file carries its own scrypt cost parameters. `KdfParams::validate` enforces
only `log_n >= 14`. It does not cap `log_n`, `r`, or `p`, and it does not bound the
memory or CPU the parameters imply.

`Wallet::load` reads those parameters from the file and passes them to
`scrypt::scrypt` before it checks the ciphertext or the stored address. A 337-byte
wallet file that sets `log_n = 30, r = 8, p = 1` asks scrypt for 1 TiB of memory.
The allocator cannot satisfy the request, and Rust's allocation error handler
aborts the process (SIGABRT, exit 134). A file that sets `log_n = 14, r = 1,
p = 1000000000` asks for 128 GB and also aborts. The same field can be tuned to
consume CPU for hours while staying under the memory limit.

The failure is an abort, not a `Result`. No caller can catch it. The wallet crate,
the CLI, the faucet, and the explorer have no code path that rejects the file
before the allocation.

This contradicts `SECURITY.md:109-111`, which lists a fixed scrypt cost as a
property the code enforces.

## Affected code

- `crates/subhost-wallet/src/lib.rs:70-76`: `KdfParams::validate` rejects only
  `log_n < 14`. The upper bounds pass through to `scrypt::Params::new`.
- `crates/subhost-wallet/src/lib.rs:221-231`: `derive_encryption_key` calls
  `scrypt::scrypt` with the file-supplied parameters.
- `crates/subhost-wallet/src/lib.rs:255-265`: `decrypt_private_key` checks the salt
  and nonce lengths, then derives the key. The AEAD check is after the KDF.
- `crates/subhost-wallet/src/lib.rs:173-188`: `Wallet::load` reads the file,
  decrypts, and only then compares the address. A wrong address cannot stop the
  KDF from running.
- `crates/subhost-wallet/src/lib.rs:161-171`: `Wallet::read` caps the file at 1 MiB.
  The cap limits the file, not the work the file requests.
- `crates/subhost-cli/src/main.rs:476` (`wallet export`) and
  `crates/subhost-cli/src/main.rs:551` (`tx send`) call `Wallet::load`.
- `crates/subhost-faucet/src/lib.rs:145`: `FaucetSigner::load` calls `Wallet::load`
  at startup.
- `explorer/src/main.rs:212`: `POST /api/transfer` calls `Wallet::load` on each
  request.
- `crates/subhost-wallet/src/lib.rs:481-489`: the existing test covers only the
  lower bound (`log_n = 10`). No test sets an upper bound.

## Root cause

The wallet format is self-describing. The cost parameters are read from the same
untrusted JSON as the ciphertext:

```rust
// crates/subhost-wallet/src/lib.rs:68-76
impl KdfParams {
    fn validate(&self) -> Result<scrypt::Params, WalletError> {
        if self.log_n < 14 {
            return Err(WalletError::WeakKdfParameters);
        }
        scrypt::Params::new(self.log_n, self.r, self.p, SECRET_KEY_BYTES)
            .map_err(|_| WalletError::KeyDerivationFailed)
    }
}
```

The comment on the type says the parameters are stored so a future hardening pass
can raise them without invalidating old wallets. The loader accepts that forward
compatibility without a ceiling.

`scrypt 0.11.0` accepts anything below its arithmetic limits:

- `log_n < 64` because `n` is shifted into a `usize`.
- `r > 0`, `p > 0`.
- `r * p < 2^30`.
- `128 * r * n` and `128 * r * p` must not overflow `usize`.

On x86_64 those checks permit `log_n = 30, r = 8`, which is 1 TiB of memory, and
`log_n = 14, r = 1, p = 1000000000`, which is 128 GB. Both parameter sets pass
`KdfParams::validate` and reach the allocator. The second set sits just under the
`r * p < 2^30` limit, so it is not an accident of a missing overflow check.

The abort comes from the global allocator. `scrypt::scrypt` returns
`Result<(), InvalidOutputLen>`, but a failed allocation never becomes an error
value: `handle_alloc_error` aborts. That is why the CLI prints the allocator
message instead of a wallet error.

## Attack path

1. The attacker builds a wallet JSON with a valid-looking frame: 48 ciphertext
   bytes, a 32-byte salt, a 12-byte nonce, `version: 1`, and the chosen `kdf`
   object. The ciphertext can be all zeros because the KDF runs first.
2. The attacker delivers the file: a shared "wallet backup", a fixture in a
   repository, a restored archive, or a path provisioned by a deployment
   configuration.
3. The file is placed in `$SUBHOST_HOME/wallets/`, or an operator points the
   faucet or explorer at it.
4. The victim runs `subhost wallet export <address> --password <password>` or
   `subhost tx send`, or starts the faucet, or submits `POST /api/transfer`.
5. `Wallet::load` reads the parameters and calls scrypt. The process aborts, or
   spends hours in the KDF while holding the request or startup path.

The password must be at least 8 characters, which every real use satisfies. The
attacker does not need the password: the crash happens before the password is
checked against the ciphertext.

## Reproduction

The reproducer is `poc/reproduce.sh`. It writes three wallet files and runs the
real CLI:

```sh
./security-audit/2026-11-18-wallet-kdf-unbounded/poc/reproduce.sh
```

Each malicious wallet is 337 bytes. Case 1 runs under `ulimit -v 1048576` so the
failure is deterministic on any machine. Case 2 is wrapped in `timeout 30` to
bound the run.

Observed output (`evidence/repro-transcript.txt`):

```text
=== case 1: log_n=30, r=8, p=1 (1 TiB) with a 1 GiB address-space limit ===
$ ... subhost wallet export 0x1111111111111111111111111111111111111111 --password password123
memory allocation of 1099511627776 bytes failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
[exit status: 134]

=== case 2: log_n=14, r=1, p=1000000000 (bounded by timeout 30) ===
$ ... subhost wallet export 0x2222222222222222222222222222222222222222 --password password123
memory allocation of 128000000000 bytes failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
[exit status: 134]

=== control 1: wallet show reads the parameters without running the KDF ===
address: 0x3333333333333333333333333333333333333333
file:    /tmp/.../control/wallets/evil.json
version: 1
kdf:     scrypt(log_n=15, r=8, p=1)
[exit status: 0]

=== control 2: default parameters reject the same junk ciphertext cleanly ===
Error: decryption failed (wrong password or corrupt wallet)
[exit status: 1]
```

The controls separate the parameter problem from a corrupt-file problem:

- `wallet show` uses `Wallet::read` and prints the file's `kdf` line. The wallet
  parser accepts the file either way, so the crash is not a parse failure.
- The same all-zero ciphertext with default parameters reaches the AEAD check and
  returns a normal error with exit status 1. The abort therefore comes from the
  KDF parameters, not from the junk ciphertext.

The abort is reproducible without the address-space limit on any machine with less
than 1 TiB of RAM, but the limit keeps the evidence machine-independent. On a host
with overcommit the 1 TiB mapping may succeed and the process is OOM-killed when
scrypt touches the pages instead of aborting at allocation.

## Impact

- Availability: the process aborts (exit 134) or hangs in the KDF. For the CLI
  that ends one command. For the faucet it prevents startup. For the explorer it
  aborts the process on the request that reaches the malicious wallet, which drops
  every other in-flight request with it.
- Recovery requires removing or repairing the wallet file. Nothing in the CLI or
  the service reports which parameter caused the abort, so an operator has to
  inspect the JSON by hand.
- A parameter set that fits in memory but not in time (`log_n` and `r` moderate,
  `p` large) burns CPU for hours. Repeating the request against the explorer
  keeps a core busy.
- No confidentiality or integrity loss. The KDF runs before decryption, so the
  attacker learns nothing and cannot forge a key. `wallet list` and `wallet show`
  do not trigger the bug because they never call `Wallet::load`.

## CVSS v4 justification

| Metric | Value | Reason |
| --- | --- | --- |
| AV | Local | The attacker supplies a local wallet file. There is no remote input path to `kdf`. |
| AC | Low | The file format is deterministic and the parameters are chosen by the attacker. |
| AT | None | No race, no special configuration, and no prior state is required. |
| PR | None | The attacker needs no account or privilege on the target. |
| UI | Active | The victim must place the file in the wallet directory and run a decrypting command, or start the service that loads it. |
| VC | None | No confidential data is exposed. |
| VI | None | No wallet or ledger data is modified. |
| VA | High | The process aborts or stops serving until the file is removed. |
| SC | None | The scoring does not claim impact on other components. Host memory pressure can affect them, but that is not asserted here. |
| SI | None | No downstream integrity impact. |
| SA | None | No downstream availability impact claimed. |

Base score: `6.7` (Medium). Treating the file delivery as an attack requirement
(`AT:P`) instead of user interaction lowers the score to `5.6`, still Medium.

## Fix

Reject parameters whose memory and work exceed fixed ceilings before calling
scrypt. Compute both with checked arithmetic and compare against constants:

```rust
/// Upper bounds for a file-supplied scrypt cost. The defaults need 32 MiB and
/// 2^18 work units; the ceiling leaves room to raise the cost later.
const MAX_KDF_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_KDF_WORK: u64 = 1 << 22;

impl KdfParams {
    fn validate(&self) -> Result<scrypt::Params, WalletError> {
        if self.log_n < 14 || self.r == 0 || self.p == 0 {
            return Err(WalletError::WeakKdfParameters);
        }
        let n = 1u64
            .checked_shl(self.log_n as u32)
            .ok_or(WalletError::WeakKdfParameters)?;
        let memory = 128u64
            .checked_mul(n)
            .and_then(|value| value.checked_mul(self.r as u64))
            .ok_or(WalletError::WeakKdfParameters)?;
        let work = n
            .checked_mul(self.r as u64)
            .and_then(|value| value.checked_mul(self.p as u64))
            .ok_or(WalletError::WeakKdfParameters)?;
        if memory > MAX_KDF_MEMORY_BYTES || work > MAX_KDF_WORK {
            return Err(WalletError::WeakKdfParameters);
        }
        scrypt::Params::new(self.log_n, self.r, self.p, SECRET_KEY_BYTES)
            .map_err(|_| WalletError::KeyDerivationFailed)
    }
}
```

The same bound must hold on the write path, because `encrypt_private_key` also
calls `derive_encryption_key`. The check above covers both.

Alternative, tighter policy: accept only the parameters the current release writes
and reject every other set. That breaks the stated intent to raise the cost
without invalidating old wallets, so a ceiling is the better fit.

Regression tests to add:

- `KdfParams { log_n: 30, r: 8, p: 1 }` returns `WeakKdfParameters` and does not
  allocate. Call `validate` directly so the test is fast.
- `KdfParams { log_n: 14, r: 1, p: 1_000_000_000 }` returns the same error.
- A `p`-only variant above the work ceiling returns the same error.
- The default parameters still validate, and an existing wallet still loads.

## Scope and false-positive control

Reviewed and not reported:

- The 1 MiB wallet file cap (`MAX_WALLET_FILE_BYTES`) is enforced by
  `Wallet::read` and by `list_wallets`. It is not the issue; a 337-byte file is
  enough.
- `wallet show` and `wallet list` are safe by construction. They call
  `Wallet::read`, which does not decrypt. This was checked against the real
  binary, not only by reading the code.
- The malformed-salt and malformed-nonce path returns `DecryptionFailed` before
  the KDF (`lib.rs:260-262`). Truncating the salt does not reach scrypt.
- A wrong password on a normal wallet returns a clean error. The reported issue
  needs crafted `kdf` values, not just a bad password.
- The ed25519 key, AES-GCM, address binding, atomic `0600` writes, and zeroization
  were reviewed in earlier passes and are not implicated here.
- `SECURITY.md:33-99` known-posture items do not cover KDF parameter bounds. The
  wallet bullet states a fixed cost as an enforced property, which the loader does
  not enforce. This finding is not a known-posture item.
- The prior audit `security-audit/2026-11-18-gas-cap-bypass/` covers a different
  bug (`SUBHOST-2026-001`) and is not duplicated here.

The reported issue is reachable from the shipped CLI on the stated commit. Both
malicious files were loaded through `Wallet::load` in the real binary.

## Re-verification 2026-11-19 (main `fa87215`, PR #49 merged)

The unmodified `poc/reproduce.sh` was run against a release CLI built from a
clean worktree at `fa87215` (`CARGO_PROFILE_RELEASE_DEBUG=0`):

- Binary sha256: `8b5ac1ea1ff62274a04d6fb67b983921adbb506ece2c375681cbccb6c9f4db6a`
- Case 1 (`log_n=30, r=8, p=1`, 1 TiB, 1 GiB address-space limit):
  `Error: scrypt parameters are outside the accepted cost bounds`, exit 1.
  Previously SIGABRT / exit 134.
- Case 2 (`log_n=14, r=1, p=1000000000`): same error, exit 1, well inside the
  30 s timeout. Previously abort or hang.
- Control 1 (`wallet show`, default cost): exit 0, prints the parameters
  without running the KDF, unchanged.
- Control 2 (`wallet export`, default cost, junk ciphertext): exit 1 with
  `decryption failed (wrong password or corrupt wallet)`, unchanged.
- `cargo test -p subhost-wallet`: 19 passed, 0 failed, including the three
  regression tests added by PR #49 (ceiling refusal before allocation, crafted
  file refused on load, ceiling/floor/default acceptance).

The bounds are enforced in `KdfParams::validate` before `scrypt` allocates, on
the single chokepoint `derive_encryption_key` used by both encrypt and decrypt.
Evidence: `evidence/reverify-fa87215.log`.

## Delivery Gate

- PASS: Affected function and exact lines identified.
- PASS: Root cause reproduced on commit `4be9457`.
- PASS: Two independent crafted parameter sets abort the real CLI.
- PASS: Two controls separate the parameter bug from a corrupt file.
- PASS: CVSS v4 vector and score computed from the stated metrics.
- PASS: Impact is availability only, with no unsupported theft claim.
- PASS: No known-posture item used as a finding.
- PASS: No em dashes and no fabricated data.
- PASS: The exploit needs no password knowledge, only a file load.
- FAIL: None.

## Artifacts

- `poc/reproduce.sh`: writes the wallet files and runs the CLI cases and controls.
- `evidence/repro-transcript.txt`: captured run on the release binary, including
  the binary hash and commit.
- `evidence/source-snippets.txt`: the validator, the call chain, and the
  `scrypt 0.11.0` parameter limits.
- `evidence/reverify-fa87215.log`: 2026-11-19 re-verification on main `fa87215`
  (release binary hash, both attack cases, both controls, wallet unit tests).
