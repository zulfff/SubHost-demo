# Security Policy

## Supported versions

This repository is a pre-production codebase. Security fixes land on the default
branch; there is no supported release branch and no version is intended for a
production network.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability.

Report privately through either channel:

- GitHub Security Advisories: <https://github.com/zulfff/SubHost-demo/security/advisories>
- Email: `security@subhost.xyz`

Include the affected commit, the exact file and line, the impact, and the steps to
reproduce. Never include real private keys, credentials, or production data.

We aim to acknowledge a report within five business days and to coordinate
disclosure once a fix or mitigation is available. There is no bug bounty program.

## Scope

In scope: every package under `crates/` and `explorer/`.

## Known security posture

These are deliberate properties of the current code, not undisclosed
vulnerabilities. Reporting them as findings is not useful; changing them is.

**No authentication on any network surface.** The JSON-RPC server, the metrics
exporter, and the faucet all serve any caller that can reach them. Bind them to
loopback or place an authenticating reverse proxy in front. The node logs a
warning when it binds a non-loopback address.

**The explorer signs with a local key.** `POST /api/transfer` decrypts a wallet
using a password taken from the request body. The explorer refuses to bind to a
non-loopback address for that reason. Do not expose it.

**No consensus.** Block production is single-node. There is no fork choice, no
block propagation, and no validator attestation. A `Block` carries an empty
`signatures` vector and the zero address as its validator.

**BLS hash-to-curve is non-standard.** `subhost-crypto` uses a domain-separated,
length-prefixed hash-and-multiply onto the prime-order G2 subgroup, not an RFC 9380
suite. It is deterministic and domain separated, which is sufficient for internal
use, but it will not interoperate with another BLS implementation.

**Aggregate BLS verification requires proofs of possession.** Verifying an
aggregate over a shared message is vulnerable to the rogue-key attack unless every
key is registered with a verified proof of possession.
`ValidatorRegistry::register` enforces that; a caller assembling keys by hand must
do the same.

**IBC packets are not proof-verified.** `subhost-ibc` enforces channel binding,
sequencing, timeouts, and replay rejection on the local side only. It does not
verify light-client or commitment proofs, so a packet is trusted exactly as far as
the relayer that delivered it.

**The state root is not a Merkle root.** It is a BLAKE3 hash over the sorted
account list. It detects divergence but supports no inclusion proofs.

**One transaction per block.** The producer seals each accepted transaction into
its own block. The ledger validator enforces that invariant on load.

## Security properties that are enforced

Regressions in any of these are genuine vulnerabilities and worth reporting.

- `eth_sendTransaction` requires an ed25519 signature over the unsigned
  transaction encoding, and the supplied public key must hash to the `from`
  address. The node holds no user keys.
- A transaction is rejected unless the chain ID matches, the nonce is exactly the
  account's current nonce, and the balance covers `value + gas_price * gas_limit`.
- Every balance and nonce operation uses checked arithmetic; overflow is an error.
- The ledger is written to a temporary file, fsynced, atomically renamed, and the
  directory is fsynced. It is only trusted on load after a size check, magic and
  version check, BLAKE3 checksum, chain binding check, and a full replay of every
  block and receipt commitment against the restored state.
- In-memory state is swapped in only after the ledger write succeeds, so a failed
  persist cannot leave memory ahead of disk.
- Wallet files use scrypt (N = 2^15, r = 8, p = 1) with AES-256-GCM, are written
  atomically with `0600` permissions, and are rejected on load if the stored
  address does not match the decrypted key.
- Secret material is zeroized on drop and after use.
- X25519 shared secrets are contributory-checked; low-order and all-zero peer keys
  are rejected.
- Quorum certificates are accepted only when the required number of distinct,
  registered validators each contribute a signature that verifies over a payload
  binding both the view and the block hash.
- Slashing actually deducts stake and ejects a fully slashed validator; the
  evidence is recorded before any mutation.
- The faucet rate limits per lowercased address, so letter case cannot bypass the
  cooldown, and releases the slot when a drip fails.
- The RPC, CLI, faucet, and explorer all bound request and response body sizes.

## Audits

No third-party audit of this codebase has been completed. Any auditor name or
target date you may find in the history was a proposal, not evidence of an
engagement.
