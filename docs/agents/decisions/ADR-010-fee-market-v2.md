# ADR-010: Genesis-bound fee market and version-2 headers

Status: Accepted · 2026-11-19

Contract: Q-017 (agreed), Q-030 (agreed). Scope: S6.2 — core policy/codec, state
settlement, ledger policy binding and replay, node bootstrap binding and the RPC
block path. RPC consumer formatting (`eth_feeHistory`, `eth_gasPrice`, receipt
`effectiveGasPrice`) remains Track B work on the same contract.

## Staged landing

This decision lands in three PRs so that `main` never carries a fee-enabled
chain that its own storage cannot verify (Q-030):

1. **`s6/fee-market-api`** — the additive core/state API and the version-2
   header codec, plus mechanical `base_fee_per_gas: 0` test literals. Nothing
   enables a policy: no genesis wiring, no node bootstrap, no RPC binding, so
   this step produces only version-1 blocks.
2. **Track B storage** — manifest `fee_policy` (additive, manifest version stays
   2), `LedgerStore::with_fee_policy`, the fail-closed binding rules, and
   re-execution + settlement in *both* replay paths for v2 histories.
3. **`s6/fee-market-v2`** — genesis/node/RPC wiring, empty v2 blocks, admission
   policy, and the durable non-empty v2 tests. First step where a fee-enabled
   chain exists.

## Decision

The fee market is a **genesis property, not an upgrade**. `GenesisConfig` gains
`fee_policy: Option<FeePolicy>`. A policy is validated against the fixed block
gas limit (positive, even; elasticity 2; denominator 8; `1 <= min <= initial <=
max`; `max * gas_limit <= Amount::MAX`) and is bound into
`consensus_context()` as a seventh canonical tuple element. Policy absence keeps
the exact pre-fee six-element tuple, so existing genesis bindings are
byte-identical. Fee-enabled chains start from a fresh genesis; there is no
in-place activation.

`FeePolicy::{validate, initial_context, next_context}` carries no gas limit of
its own — the limit is always the chain's. The base-fee update is exact floor
arithmetic without wide integers: with `target = limit / 2`,
`delta = |gas_used - target|` and `b = parent base fee`,
`raw = (q * delta + (r * delta) / target) / 8` where `q = b / 8`, `r = b % 8`.
Both directions have a minimum step of 1 (an explicit deviation from EIP-1559's
zero downward delta) and the result clamps to `[min, max]`. `next_context`
refuses a version-1 parent, a parent gas-limit mismatch, invalid parent gas
usage and an out-of-bounds parent fee; nothing is defaulted or wrapped.

**Header version 2** appends `base_fee_per_gas` as a 13th field. Serialization is
custom: the binary form is a fixed 12-element tuple for version 1 and a
13-element tuple for version 2, so a legacy decoder stops after 12 elements and
leaves the rest of the stream intact. JSON uses named fields with the base fee
as a decimal string (`u128`-safe); a legacy document keeps the exact pre-fee
field set. Unknown versions and a version-1 header carrying a fee are refused.
The base fee is inside the header hash, therefore inside every signature and QC
commitment. `BLOCK_SIGNING_DOMAIN` is unchanged.

**Settlement is at the end of the block.** `Transaction::fee_breakdown(context,
gas_used)` splits `gas_price * gas_used` into burned `base_fee * gas_used` and
the remaining priority fee; `settle_block_fees` aggregates a block with checked
arithmetic and conservation by construction (`total = burned + priority`).
`State::apply_transaction` is unchanged and still debits the full fee;
`State::credit_block_priority(producer, amount)` credits the authenticated
producer once, after execution, checking overflow before mutating and creating
no account for a zero reward. A producer therefore cannot spend this block's
tip from a transaction earlier in the same block. Validatorless explicit demo
mode (`Address::ZERO`) burns the tip and creates no reward account. There is no
issuance.

For S6.2 the consumed gas of a transfer remains `tx.gas_limit`, as committed in
receipts; intrinsic-gas/refund execution is not claimed. A valid included
transaction must pay `gas_price >= block.base_fee_per_gas`, and both the
producer and the verifier check that the fees execution debited equal the
settlement total before the state root is taken.

**Empty blocks are version-conditional.** A legacy chain keeps skipping empty
selections and refusing empty blocks. A fee-enabled chain builds, proposes,
certifies and accepts empty blocks (zero gas, unchanged state root, canonical
empty roots) at the block cadence, so an idle or fully underpriced chain can
still lower its base fee. Only real commits move the fee; timeouts and
uncommitted proposals do not.

**Admission is local policy, not consensus.** `submit_transaction` rejects a bid
below the next block's base fee under the commit lock before the pool is
touched; `include_transaction` refuses a held entry that has become underpriced
rather than sealing an empty block and reporting a missing receipt. Entries
admitted before a fee rise stay parked: selection breaks at the first
underpriced transaction of a sender, never skipping its nonce, and acceptance
does not delete parked entries.

**Legacy data is refused, never rewritten.** `RpcState::with_fee_policy`
validates the policy and refuses a restored ledger containing any version-1
header, and a restored version-2 tip that the policy cannot extend. The caller
must use a fresh data directory.

## Consumer contract

- Track B replay (`subhost-storage`) recomputes parent fees, gas totals and fee
  distribution before accepting roots, using the same pure
  `subhost_core::settle_block_fees` and `State::credit_block_priority`; it must
  not credit tips retroactively to legacy blocks.
- RPC read methods expose the recorded base fee on fee-enabled blocks and keep
  Q-008's zero/absent semantics for legacy history; `eth_feeHistory` rewards are
  gas-weighted effective tips from recorded fees, never today's configuration.
- CLI genesis generation for fee-enabled devnets is a Track B follow-up; the
  node already binds any policy present in the genesis document.
- Header literals outside `subhost-core` gained `base_fee_per_gas: 0`
  mechanically (consensus/node/RPC/storage test fixtures) so the workspace
  compiles; those are the only Track B file touches in this change.

## Limitations

- The ledger binds the policy in the manifest (Q-030, option A): additive
  `fee_policy: Option<FeePolicy>`, manifest version stays 2 because
  `deny_unknown_fields` makes older builds refuse during parse, before the
  version check. The node supplies the genesis policy before the first persist;
  storage never reads genesis files, so offline tools verify v2 ledgers from the
  recorded policy alone.
- A genesis with a non-default `gas_limit` is already inconsistent before fees:
  `GenesisConfig::validate` checks the policy against `genesis.gas_limit`, but
  the node never reads that field and the block path hardcodes
  `BLOCK_GAS_LIMIT`. Replay uses the policy-bound context limit and refuses a
  mismatched header. Honouring the genesis limit end to end is a separate
  Track A follow-up, not part of this contract.

- Full-history replay verification is not in this slice; a restored fee-enabled
  ledger is checked for extendability, not re-executed. Track B's storage PR
  supplies the replay gate.
- The demo synchronous path can still seal a block and then report a failure
  for an unrelated reason (the seal-then-Err family). The underpriced include
  case is now refused up front; the remaining inconsistency is a recorded
  Track A follow-up in `ROADMAP.md`.
- A fee-enabled chain restarted with a *different* policy document is only
  caught by the extendability check and the genesis commitment, not by replay.
