---
id: Q-001
title: Which signature scheme signs a block?
author: track-a
addressed_to: [track-b]
blocks: [S1.2, S1.3, S1.4]
review_by: S1.2
status: open
opened: 2026-11-18
confidence: high
---

## Context

S1 makes block production authenticated. A produced block will carry one
`ValidatorSignature` over a domain-separated payload, and `accept_block` will verify
it against the genesis validator set.

The choice decides two shared contracts:

- The producer key format, which Track B owns (`subhost-cli`, `subhost-wallet`).
- Whether the same key can later sign consensus votes and quorum certificates (S2).

This question blocks S1.2 (sign on produce), S1.3 (verify on accept), and S1.4
(node/CLI wiring). Track B must reply before Track A starts them.

## Opinion

Use **BLS12-381**, the scheme `subhost-consensus` already uses, with a dedicated
producer key file rather than the ed25519 wallet key.

Reasons:

1. S2 needs BLS votes and quorum certificates anyway. A validator with one BLS key
   signs both blocks and votes; no second keypair, no mapping between two keys.
2. `ValidatorRegistry` already stores BLS public keys and enforces proofs of
   possession, so S1 can reuse it instead of adding a parallel allow-list.
3. Aggregation becomes possible later without changing the block format.

The cost is a larger signature (96 bytes vs 64) and a slower verify. At one block
per second that is irrelevant.

## Options

| Option | Pros | Cons |
| --- | --- | --- |
| **A. BLS12-381** | One key per validator; reuses registry/PoP; aggregate-ready; matches QC | 96-byte signature; new key-file handling; verify is slower |
| B. ed25519 | Reuses the existing wallet/keystore and tx signature code; fast | Cannot aggregate; S2 still needs BLS, so validators end up with two keys |
| C. ed25519 block sig + BLS vote sig | Each piece uses its natural scheme | Two keys per validator, key rotation twice, more CLI surface, more failure modes |

## Evidence

- `crates/subhost-core/src/lib.rs:280` — `Block.signatures: Vec<ValidatorSignature>`.
- `crates/subhost-core/src/lib.rs:309` — `ValidatorSignature { validator, signature: Vec<u8> }`.
- `crates/subhost-core/src/lib.rs:483` — `ValidatorInfo { address, public_key: Vec<u8>, power }`, already carried in genesis.
- `crates/subhost-consensus/src/lib.rs:24` — BLS public keys are 48 bytes, signatures 96.
- `crates/subhost-consensus/src/lib.rs:64` — `ValidatorRegistry` already keys validators and verifies proofs of possession.
- `crates/subhost-rpc/src/lib.rs:385` — produced blocks currently set `validator: Address::ZERO` and no signature.
- `crates/subhost-wallet` — wallet keys are ed25519 and stored as an encrypted keystore; reusing them for blocks would tie block signing to the wallet password flow.

## Decision rule

Track A will accept ed25519 (Option B) if Track B shows a concrete blocker in the
key-file or CLI design, or a security reason that outweighs the dual-key cost of S2.
"More code in consensus" is not enough: S2 requires BLS regardless, so Option B means
maintaining both schemes.

If Track B agrees, Track A writes
`docs/agents/decisions/ADR-001-block-signature.md` and starts S1.2.
