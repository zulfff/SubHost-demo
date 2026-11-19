# ADR-015: Intrinsic calldata gas as a validity floor (version-3 headers)

Status: Accepted · 2026-11-20

Contract: Q-037 (agreed). Closes the High residual SUBHOST-2026-003 (unpriced
calldata) once the ladder lands and nara re-verifies. Scope: the calldata
pricing ladder — L2 core policy/genesis binding/v3 codec (this change), L3
Track B manifest binding and replay dispatch, L4 activation wiring, L5 docs.

## Staged landing

Nothing activates until a genesis binds the policy and the node produces
version-3 headers, so `main` never carries a priced chain its storage cannot
verify (the Q-030 discipline):

1. **L0/L1 (merged)** — interim byte-cap refusal (dewi/yoga) and the Q-034
   commit-path wiring (raka, #80): admission persistence, seal/accept
   retirement and the `restore_pending()` path. Under the floor model the
   consumed-gas call-site swaps are unnecessary (yoga's Q5 audit: settlement,
   selection accumulator, receipts and the storage sites all stay
   `gas_limit`); L3/L4 insert the floor into those paths.
2. **L2 (this change, `s6/calldata-floor-api`)** — `CalldataPolicy`, genesis
   field/validation/context binding, the version-3 header codec (accept-only)
   and the fee-continuity rule. Purely additive: no caller edits, no v3
   production, no admission enforcement yet.
3. **L3 (Track B, dewi)** — manifest `calldata_policy` binding mirroring the
   genesis layout, and replay validity dispatch from the header version.
4. **L4 (Track A, bima)** — genesis/node admission and selection wiring; the
   first step where a priced chain exists and version-3 headers are produced.
5. **L5 (Track B, yoga)** — CLI genesis flag and operator docs.

## Decision

Calldata is priced through the **gas axis as a validity floor**, not a new
fee outcome. A genesis-bound `CalldataPolicy { version, gas_per_byte }`
requires every transaction carrying `B` bytes to declare
`gas_limit >= gas_per_byte * B`. Under-intrinsic is a distinct refusal
(`FeeError::UnderIntrinsic`) — never clamped, topped up or defaulted, the same
discipline as the underpriced refusal in Q-017. Consumption, debit, receipts
and settlement stay `gas_limit` (there is no execution engine yet), so the
sender pays for the whole declaration and a block's gas limit caps total
calldata. The policy `version` lets S10 introduce real consumption later
without reinterpreting committed history.

`CalldataPolicy::check_floor(tx)` is **pure** so admission, selection and
replay enforce the identical rule independently (dewi's condition); the
arithmetic lives only in `subhost-core` (Q-017's no-duplicate-pricing rule).
It reads the signed `data` and signed `gas_limit`; `data` is part of the
signing payload, so priced bytes cannot be shrunk post-admission without
invalidating the sender binding. The same check refuses calldata above
`MAX_TX_DATA_BYTES` even when the gas floor is met (`FeeError::CalldataTooLarge`,
R-L2-02): the protocol cap is consensus validity on every enforcing path, not
just the local mempool cap, so an oversized transaction cannot ride a peer
block. Overflow is invalid, never wrapped (`checked_mul` throughout).

Policy validation is fail-closed: the version is pinned to
`CALLDATA_POLICY_VERSION = 1`, the rate must be nonzero (a zero rate would
reopen the residual — a chain that wants no calldata tightens the B-owned byte
cap instead), and `gas_per_byte * MAX_TX_DATA_BYTES <= genesis gas_limit` so a
maximum-size transaction stays includable. `MAX_TX_DATA_BYTES = 128 KiB` is a
core protocol constant; the mempool default matches it and operators may only
tighten. The agreed devnet schedule is flat 16 gas/byte (a maximum-size
transaction costs ~7% of the default block). Flat pricing — no zero/nonzero
byte distinction — is a deliberate simplification; the policy version is the
upgrade path.

**Calldata is a genesis property, not an upgrade.** `GenesisConfig` gains
`calldata_policy: Option<CalldataPolicy>`, omitted from legacy documents so
they stay byte-stable. `consensus_context()` keeps the legacy six-element and
the fee-only seven-element tuples **byte-identical**; a priced chain appends
the Option-encoded fee policy and the calldata policy as seventh and eighth
canonical elements. A different schedule is a different protocol instance.
Calldata without a fee policy is allowed (independent policies, confirmed in
Q-037): v3 validity rule with legacy settlement. Priced chains start from a
fresh genesis; there is no in-place activation.

**Header version 3** keeps the exact version-2 13-field layout and adds no
field: the version word alone discriminates the validity rule, so replay and
the peer/certified-sync path can dispatch from the header without a manifest.
The version is inside the header hash, therefore inside every signature and
QC commitment. The codec accepts versions {1, 2, 3} in binary and JSON and
refuses everything else on **both** serialize and deserialize; a fee-era
(2 or 3) JSON document without `base_fee_per_gas` is refused, not defaulted.
Nothing produces version 3 until L4.

**Fee continuity across the fork.** `FeePolicy::next_context` accepts a
version-2 parent for a version-3 successor — the fork changes the validity
rule, not the fee schedule. A version-1 parent stays refused on either side
of the fork. Pinned by test and rustdoc.

`FeeError` gains five additive variants: `UnsupportedCalldataVersion`,
`ZeroCalldataRate`, `CalldataRateTooHigh`, `UnderIntrinsic`,
`CalldataTooLarge`. No exhaustive
matches exist outside core, so no mechanical caller edits are needed.

## Consumer contract

- **L3 (dewi)** mirrors the genesis context layout exactly in the manifest
  binding, refuses a priced history with a missing or foreign schedule, and
  dispatches the floor from the header version in both replay paths.
  Per-policy mismatch checks stay independent (calldata-without-fee is a
  valid combination).
- **L4 (bima)** wires admission and selection to `check_floor` through the
  L1 call sites and starts producing version-3 headers from a bound genesis.
- **L1 (raka, merged #80)** wired the Q-034 pending-store commit paths
  (admission, seal, accept, restore). Under the floor model no consumed-gas
  swaps are needed; L3 and L4 insert `check_floor` into the replay and
  admission/selection/restore paths.
- **L5 (yoga)** adds the CLI genesis flag and operator docs; the activation
  gate (nara) is L2–L4 merged, regressions green, S9.6 merged (done), and her
  re-verification of SUBHOST-2026-003 as Fixed.

## Limitations

- L2 is accept-only: no producer emits version 3 and no admission path
  enforces the floor until L4. The residual stays open until the activation
  gate above is satisfied.
- The floor bounds inclusion, not execution: consumed gas remains the full
  `gas_limit` declaration, so a sender over-declaring pays for the difference.
  Real consumption/refunds are S10 work behind the policy version.
- A priced chain restarted with a different policy document is caught by the
  genesis commitment and (after L3) the manifest binding, not by L2 alone.