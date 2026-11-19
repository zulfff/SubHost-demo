# SubHost-demo security finding: block gas cap bypass makes the node fail to restart

- Target: `SubHost-demo`
- Commit: `49f955b29607b21f379801ad03a05231fbedd0ce` (branch `test/gossip-propagation`)
- Date: 2026-11-18
- Finding ID: `SUBHOST-2026-001`
- Severity: High
- CVSS v4 base: `8.7` (`CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:H/SC:N/SI:N/SA:N`)
- Status: Fixed. Originally confirmed with two passing proof-of-concept tests
  on `49f955b`; re-verified 2026-11-19 against main `982d3e6`, where both
  attack cases are refused with `RpcError::BlockRejected` (see
  Re-verification below).

## Summary

`RpcState::accept_block` re-validates a peer block but never checks that the
sum of the transactions' gas limits is within the block gas limit. A peer can
publish a block whose header claims `gas_limit = 30_000_000` while its
transactions consume more than that. The node accepts the block, applies it, and
persists it.

On the next restart, the ledger loader rejects the same block. The node cannot
start until an operator deletes or repairs the ledger. One peer can therefore
affect every node that accepts the block.

## Affected code

- `crates/subhost-rpc/src/lib.rs:444` defines `RpcState::accept_block`.
- `crates/subhost-rpc/src/lib.rs:456` checks only that the header field equals
  `BLOCK_GAS_LIMIT`. It does not compare the transaction sum to that limit.
- `crates/subhost-rpc/src/lib.rs:468-488` accumulates `gas_used` with no ceiling
  check.
- `crates/subhost-rpc/src/lib.rs:490` compares `gas_used` to
  `block.header.gas_used`, so a matching oversized figure passes.
- `crates/subhost-storage/src/lib.rs:285` rejects the persisted block on load:
  `block.header.gas_used > block.header.gas_limit`.
- `crates/subhost-storage/src/lib.rs:701-704` already tests that an overspent
  block is rejected on load, which confirms the invariant is intended.
- `crates/subhost-rpc/src/lib.rs:326-329` enforces the cap on locally produced
  blocks, which shows the intended producer rule.
- `crates/subhost-node/src/lib.rs:322-328` feeds inbound `BlockProposal`
  messages into `accept_block`.
- `crates/subhost-cli/src/main.rs:86-88` enables that path with `--p2p-listen`.

`SECURITY.md:95-99` states that `accept_block` re-validates the block gas limit
before any state change. `SECURITY.md:81-85` states that blocks are bounded by
the 30M block gas limit. The code does not enforce either statement on the peer
path.

## Root cause

The producer applies the cap while selecting transactions:

```rust
// crates/subhost-rpc/src/lib.rs:326-329
let Some(next_gas) = gas_used.checked_add(tx.gas_limit) else { break };
if next_gas > BLOCK_GAS_LIMIT {
    break;
}
```

The peer path repeats the addition but drops the comparison:

```rust
// crates/subhost-rpc/src/lib.rs:479-490
gas_used = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
...
if gas_used != block.header.gas_used
```

A block with `gas_used = 40_000_000` and `gas_limit = 30_000_000` satisfies
every remaining check: parent, height, chain ID, signatures, transaction
uniqueness, transaction root, receipt root, state transition, and state root.

## Attack path

1. The node is started with `--p2p-listen`, which enables block gossip.
2. An attacker connects as a libp2p peer and learns the current tip from
   published blocks.
3. The attacker publishes a `NetworkMessage::BlockProposal` for height
   `tip + 1` with a valid parent hash.
4. The block carries one or more signed transactions whose gas limits sum to
   more than 30M. The header sets `gas_limit = BLOCK_GAS_LIMIT` and
   `gas_used` to the true oversized sum.
5. `accept_block` applies the transactions, matches the commitments, persists
   the block, and advances the tip.
6. On restart, `validate_chain` sees `gas_used > gas_limit` and refuses to load
   the ledger. The node exits with a storage error.

The second PoC removes the balance requirement. `accept_block` does not enforce
the mempool's minimum gas price, so a self-transfer with `gas_price = 0` and
`value = 0` costs nothing. The only state change is the sender's nonce. The
attacker can compute the resulting state root from the `eth_getProof` response
for their own account, which includes `leafIndex`, the leaf, and the sibling
path.

## Reproduction

The proof-of-concept test is in this folder at `poc/gas_limit_bypass.rs` and in
the repository at `crates/subhost-rpc/tests/gas_limit_bypass.rs`.

Command:

```sh
cargo test -p subhost-rpc --test gas_limit_bypass -- --nocapture
```

Observed output (`evidence/test-output.txt`):

```text
running 2 tests
accepted zero-fee block: gas_used=30000001 gas_limit=30000000
test zero_fee_zero_balance_block_exceeds_the_cap ... ok
accepted block: gas_used=40000000 gas_limit=30000000
restart failed as predicted: ledger is invalid: invalid block sequence at height 1
test oversized_block_is_accepted_and_then_breaks_restart ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The tests cover two cases:

- Two 20M-gas transfers. `accept_block` returns two receipts, the tip becomes
  1, and the stored header has `gas_used = 40_000_000`.
- One 30,000,001-gas self-transfer with zero fee and zero balance. The account
  has no funds, the header has `gas_used = 30_000_001`, and the block is still
  accepted.

In both cases the test drops the state, reopens the same data directory, and
asserts that loading fails. The loader returns
`ledger is invalid: invalid block sequence at height 1`.

## Impact

- Availability: the node cannot restart after the block is persisted. Recovery
  requires manual ledger deletion or repair, which also discards the chain
  history in that file.
- Scope: every node that processes the gossiped block persists it. One peer can
  affect the whole gossip mesh.
- Integrity: the node stores a block that its own loader declares invalid. The
  accept path and the load path disagree on a consensus rule.
- No direct theft: the attacker can only include transactions they signed, and
  `apply_transaction` still enforces nonce and balance rules. The concrete
  impact is denial of service and ledger corruption.

## CVSS v4 justification

| Metric | Value | Reason |
| --- | --- | --- |
| AV | Network | The block arrives over libp2p gossip. |
| AC | Low | The block format is deterministic and no race or special setup is needed beyond a peer connection. |
| AT | None | No additional attack requirements. |
| PR | None | Any gossip peer can publish. The attack needs an account that exists in state, which the default faucet creates; the zero-fee variant needs no balance. |
| UI | None | No user action. |
| VC | None | No confidential data is exposed. |
| VI | None | No other account balance or nonce is changed. The attacker's own transactions are valid. |
| VA | High | The persisted ledger is rejected on restart, so the node stays down until manual repair. |
| SC | None | No downstream system is affected. |
| SI | None | No downstream integrity impact. |
| SA | None | No downstream availability impact. |

Base score: `8.7` (High).

## Fix

Reject the block when the accumulated gas exceeds the header limit. Add the
check after the transaction loop and before the commitment comparison:

```rust
if gas_used > block.header.gas_limit {
    return Err(RpcError::BlockRejected(format!(
        "block {} uses {} gas, above the {} limit",
        block.header.height, gas_used, block.header.gas_limit
    )));
}
```

An earlier per-transaction check would also work and would match the producer:

```rust
let next_gas = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
if next_gas > block.header.gas_limit {
    return Err(RpcError::BlockRejected(format!(
        "block {} exceeds its gas limit",
        block.header.height
    )));
}
```

The header check at line 456 already forces `gas_limit` to `BLOCK_GAS_LIMIT`, so
either form closes the gap.

Recommended regression test: build a block with transactions that sum above
`BLOCK_GAS_LIMIT`, call `accept_block`, and assert `Err(RpcError::BlockRejected)`
and `block_height() == 0`. Keep the existing storage test at
`crates/subhost-storage/src/lib.rs:701-704` so the loader remains covered.

## Scope and false-positive control

Reviewed and not reported:

- Unauthenticated JSON-RPC, metrics, and faucet surfaces: listed in
  `SECURITY.md:33-36` as known posture.
- Explorer local-key signing: listed in `SECURITY.md:38-40` as known posture.
- No consensus in the node, best-effort gossip, and permanent forks: listed in
  `SECURITY.md:42-52` as known posture.
- Non-standard BLS hash-to-curve, single-key IBC client, membership-only state
  proofs: listed in `SECURITY.md:54-79` as known posture.
- Faucet cooldown keyed on the caller-supplied address: the faucet is
  unauthenticated and per-address by design (`SECURITY.md:33-36`).
- `DAG::add_vertex` does not verify `DAGVertex.author` or its signature, and
  `support_for` counts self-declared authors. No node path drives the DAG
  (`subhost-node` handles only `BlockProposal`), and `SECURITY.md:42-46` states
  the node has no consensus. This is a latent library gap, not a reachable
  finding in this report.

The reported issue does not depend on any known-posture item. It breaks a
property that `SECURITY.md` says the code enforces, and the storage loader
already enforces it.

## Re-verification 2026-11-19 (main `982d3e6`)

Fix commit `cb062c0` "fix(rpc): reject peer blocks above the block gas limit"
(2026-11-18 03:14, ancestor of `982d3e6`). `accept_block` now checks the
accumulated transaction gas against `block.header.gas_limit` per transaction
(`crates/subhost-rpc/src/block.rs:491-495`, inside `accept_block`), before any
state change.

The unmodified PoC from this folder was run against a scratch worktree at
`982d3e6` (`crates/subhost-rpc/tests/gas_limit_bypass_attack.rs`, never
committed), next to the repository regression test
`crates/subhost-rpc/tests/gas_limit_bypass.rs`:

- Regression test: 3 passed (oversized block rejected, ledger still loads,
  block exactly at the limit accepted).
- Original attack PoC: both tests FAIL with
  `BlockRejected("block 1 uses more than its 30000000 gas limit")`, which is
  the expected inverted outcome: `accept_block` refuses the block before
  applying it, so the persisted-ledger restart failure no longer occurs.

Command: `cargo test -p subhost-rpc --test gas_limit_bypass` and
`cargo test -p subhost-rpc --test gas_limit_bypass_attack`.
Evidence: `evidence/reverify-982d3e6.txt`.

Remaining residual, not a re-open of this finding: `accept_block` still does
not enforce the mempool minimum gas price, so the zero-fee variant reaches the
gas check before being refused. It is refused, which is what this finding
requires; transaction pricing is tracked separately as `SUBHOST-2026-003`.

## Delivery Gate

- PASS: Affected function and exact lines identified.
- PASS: Root cause reproduced on commit `49f955b`.
- PASS: Two independent PoC cases pass.
- PASS: Restart failure observed from the persisted ledger.
- PASS: CVSS v4 vector and score computed from the stated metrics.
- PASS: Impact is availability and ledger integrity, with no unsupported theft
  claim.
- PASS: No known-posture item used as a finding.
- PASS: No em dashes and no fabricated data.
- FAIL: None.

## Artifacts

- `poc/gas_limit_bypass.rs`: copy of the proof-of-concept test.
- `evidence/test-output.txt`: captured test run.
- `evidence/reverify-982d3e6.txt`: 2026-11-19 re-verification on main
  `982d3e6` (regression test passes, original PoC refused).
