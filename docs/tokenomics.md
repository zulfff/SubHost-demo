# Tokenomics

SUB is the proposed token for the Subhost network. This document describes the
design, not anything deployed.

> **Status: this is a design specification, not a description of what is deployed.**
> There is no live SUB token, and most of the mechanics below (staking rewards,
> inflation, fee burn, and governance voting) are **not implemented in the code**
> in this repository. Numbers are planning targets and may change. Anything that
> *is* partially implemented is called out inline.

## Token basics

| Parameter | Value (design target) |
|-----------|-------|
| Total Supply | 1,000,000,000 SUB |
| Initial Circulation | 150,000,000 SUB (15%) |
| Inflation | 5% annually, decaying |
| Staking Rewards | 70% of inflation |
| Treasury | 20% of inflation |
| Burn Mechanism | 50% of fees burned |

> The allocations below assign 100% of total supply. "Initial circulation" of 15%
> refers to the portion unlocked at genesis. The rest is vested or locked and
> enters circulation over time, which is why both figures can be consistent.

## Token distribution

### Initial allocation

The allocation splits the **total** 1B supply. It vests over time, so not all of it
circulates on day one:

- **Community & Ecosystem**: 40% (400M SUB) - Airdrops, grants, partnerships
- **Team & Advisors**: 20% (200M SUB) - 4-year vesting with 1-year cliff
- **Investors**: 15% (150M SUB) - Private sale participants
- **Treasury**: 15% (150M SUB) - Protocol development and operations
- **Staking Reserve**: 10% (100M SUB) - Bootstrap initial staking rewards

### Vesting schedule

The design proposes four-year vesting for team and investor allocations, with
shorter or no lockups for some community allocations. No vesting contract or live
token distribution is implemented here.

## How inflation works

The design target starts annual issuance at 5% of total supply and tapers it over
time. No issuance mechanism is implemented in this repository.

### Where inflation goes

The proposed split sends 70% of issuance to validators and stakers, and 30% to the
treasury. Neither the reward distribution nor the treasury distribution is
implemented.

## Staking mechanics

The design proposes the following staking parameters:

- **Minimum Validator Self-Stake**: 10,000,000 SUB, enforced as
  `MIN_VALIDATOR_STAKE` in `crates/subhost-consensus/src/staking.rs`
- **Maximum Commission**: 10,000 basis points, enforced by `Validator::validate`
- **Minimum Delegation**: 1,000 SUB (design target; only a non-zero amount is
  enforced)
- **Unbonding Period**: 14 days (design target; not enforced)
- **Reward Distribution**: Per block (design target; no issuance implemented)
- **Slashing**: implemented in memory. Double-signing burns the whole stake and
  ejects the validator along with its delegations, malicious behaviour takes 50%,
  and downtime takes 1%. Evidence must carry a proof and is recorded before any
  mutation.

> `StakingModule` implements registration, delegation, undelegation, the validator
> set with its quorum sizing, and slashing, all with checked arithmetic. It does
> **not** implement bonding periods, reward distribution, or unbonding. It holds
> no on-chain state either: nothing in this repository connects it to a running
> chain.

### Validator economics

The envisioned validator role would require operators to:

- Maintain high uptime (99.9%+)
- Stay synced with the network
- Vote on proposals
- Avoid slashing for double-signing or downtime

The 5-10% validator commission range is a planning assumption, not implemented
protocol behaviour.

## Transaction fees

The proposed fee model would charge SUB based on:

- **Base Fee**: Fixed cost for including the transaction
- **Compute Units**: How much processing power the transaction needs
- **Storage**: Whether the transaction writes data to state

### Fee burn

The design target burns half of transaction fees; that split is not implemented.
What ships today (S6.2, ADR-010) is a genesis-opt-in EIP-1559-style market: the
`base_fee * gas_used` portion burns and the remainder is credited to the block
producer as a priority tip (burned as well in the explicit validatorless demo
mode). Policy-less chains keep the legacy rule of burning the entire fee.
Neither mode issues new tokens.

## Governance

The governance design proposes stake-weighted voting for:

- Protocol upgrades
- Parameter changes (fees, inflation rate, etc.)
- Treasury spending
- Validator set changes

### Proposal process

1. Anyone can submit a proposal (proposed deposit: 100 SUB, refunded if passed)
2. Community discussion lasts two weeks
3. Voting lasts one week
4. A proposal passes if the proposed quorum and majority thresholds are met

The current governance scaffold does not enforce these timings, deposits, or
stake-weighted rules.

## Economic attacks and defenses

### Nothing at stake

In a future PoS network, validators could validate multiple conflicting chains
without cost. Planned mitigations include:

- Slashing for double-signing
- Inactivity leaks for offline validators
- Economic finality via bonding periods

### Long range attacks

An attacker could buy old private keys and rewrite history. Planned mitigations
include:

- Weak subjectivity (social consensus on checkpoints)
- Validator set rotation
- Unbonding period makes attacks expensive

### Sybil resistance

Creating fake identities is cheap. Requiring stake is the planned Sybil-resistance
mechanism, and no running network enforces it here.

## Token utility

### Proposed uses

- Pay transaction fees
- Stake for rewards and governance weight
- Serve as collateral in future applications
- Pay for storage on the network
- Tip validators for priority inclusion
- Access premium features
- Cross-chain transfers via IBC

## Questions?

This is a living document. If something is unclear or missing, open an issue or ask
in the community channels.