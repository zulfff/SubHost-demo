# Security policy

## Supported versions

This repository is a pre-production codebase. Security fixes land on the default
branch. There is no supported release branch, and no version is intended for a
production network.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability.

Report it privately through either channel:

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

**JSON-RPC authentication is opt-in, and there is no TLS anywhere.** By default
the JSON-RPC server, the metrics exporter, and the faucet serve any caller that can
reach them, and all of them bind loopback. `--rpc-auth-token-file` adds a bearer
token to JSON-RPC and `--rpc-rate-limit` adds a per-connection limit, but both are
off unless configured, and the metrics endpoint has no authentication at all.
jsonrpsee 0.24 has no server-side TLS, so the supported deployment is a
TLS-terminating reverse proxy in front of a loopback bind. `--rpc-allow-public`
without a token is refused by `subhost-rpc::start`. WebSocket tokens are checked at
the upgrade handshake, not per message, and the rate limit is per connection
because the peer address is not exposed to the middleware.

**The explorer signs with a local key.** `POST /api/transfer` decrypts a wallet
using a password taken from the request body. The explorer refuses to bind to a
non-loopback address for that reason. Do not expose it.

**No consensus in the node.** Block production still comes from whichever node
receives the transaction, even though `subhost-consensus` contains a tested
in-process `2f + 1` driver. There is no fork choice, no reorg handling, no block
sync, and no leader rotation. A produced block is signed by the configured producer
and is only accepted by a node that has the same genesis validator set.

**The producer key is an unencrypted file.** `<data_dir>/producer.key` holds one
hex-encoded 32-byte BLS scalar with mode `0600`, and the node refuses to load it if
group or others can read it. It is not the wallet keystore and it is not encrypted,
so anyone who copies the file can produce blocks as that validator. That is
acceptable for a testnet and not for production; `ROADMAP.md` S9.6 replaces it with
an encrypted keystore. Back the file up as carefully as the identity it represents.

**Block gossip is best-effort and unauthenticated beyond libp2p.** With
`--p2p-listen`, a node publishes blocks it sealed and applies peer blocks that
extend its tip. There is no peer scoring, no rate limiting per peer, and no way to
request a missed block: a node that was offline cannot catch up. Two producers at
the same height fork permanently.

**BLS hash-to-curve is non-standard.** `subhost-crypto` uses a domain-separated,
length-prefixed hash-and-multiply onto the prime-order G2 subgroup, not an RFC 9380
suite. It is deterministic and domain separated, which is sufficient for internal
use, but it will not interoperate with another BLS implementation.

**Aggregate BLS verification requires proofs of possession.** Verifying an
aggregate over a shared message is vulnerable to the rogue-key attack unless every
key is registered with a verified proof of possession.
`ValidatorRegistry::register` enforces that; a caller assembling keys by hand must
do the same.

**The IBC light client trusts one key.** `subhost-ibc` enforces channel binding,
sequencing, timeouts, and replay rejection locally, and
`receive_packet_with_proof` additionally requires an ed25519-signed counterparty
header with strictly increasing height and time plus a Merkle inclusion proof for
the exact packet key and value. There is no validator-set rotation, no multisig or
Tendermint quorum certificate, and no non-membership proof. The plain
`receive_packet` path verifies nothing and trusts the relayer completely, so use it
only for local testing. If the trusted key is compromised, the bridge is
compromised.

**The state tree proves membership only.** `state_root` is a Merkle root over
address-sorted account leaves with the chain ID mixed into each leaf. It detects
divergence and supports inclusion proofs (`eth_getProof`), but a plain Merkle tree
cannot prove that an account is absent. An unknown address returns an error rather
than an empty proof.

**Blocks may hold many transactions.** One submission seals one block containing
every pending transaction that is currently executable, bounded by the 30M block
gas limit, with senders visited in address order. The ledger validator enforces
per-sender nonce contiguity, per-block signature validity, transaction uniqueness,
gas accounting, and the receipt index/cumulative-gas commitments on load.

## Security properties the code enforces

A regression in any of these is a genuine vulnerability and worth reporting.

- `eth_sendTransaction` requires an ed25519 signature over the unsigned
  transaction encoding, and the supplied public key must hash to the `from`
  address. The key travels with the transaction, so the binding is checked again
  at execution and on every ledger replay. The node holds no user keys.
- `RpcState::accept_block` re-validates a peer block before any state change:
  parent, height, chain ID, block gas limit, every transaction signature,
  transaction uniqueness, transaction and receipt Merkle commitments, the state
  transition, and the resulting state root. Receipts are derived locally and never
  trusted from the wire. A rejected block leaves state, receipts, and the pool
  untouched.
- A node with a genesis validator set also requires exactly one valid BLS signature
  from that block's registered producer over the block signing payload, and rejects
  an unknown producer, a missing or extra signature, a signer that differs from
  `header.validator`, and a payload that no longer matches the header. The check runs
  before the state transition.
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
- The consensus driver commits only on a `2f + 1` quorum for one proposal, counts
  each validator once per view, and refuses a proposal that does not extend the
  committed tip by exactly one height.
- Merkle leaves, internal nodes, and padding use separate domain tags, so a leaf
  digest cannot be reinterpreted as an internal node and `[a, b, c]` cannot share
  a root with `[a, b, c, c]`.
- Slashing actually deducts stake and ejects a fully slashed validator; the
  evidence is recorded before any mutation.
- The faucet rate limits per lowercased address, so letter case cannot bypass the
  cooldown, and releases the slot when a drip fails.
- The RPC, CLI, faucet, and explorer all bound request and response body sizes.

## Audits

No third-party audit of this codebase has been completed. Any auditor name or
target date you may find in the history was a proposal, not evidence of an
engagement.