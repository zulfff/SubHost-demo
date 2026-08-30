# Tokenomics

The SUB token is the design for the Subhost network.

> **Status: this is a design specification, not a description of what is deployed.**
> There is no live SUB token, and most of the mechanics below (staking rewards,
> inflation, fee burn, and governance voting) are **not implemented in the code**
> in this repository. Numbers are planning targets and may change. Anything that
> *is* partially implemented is called out inline.

## Token Basics

| Parameter | Value (design target) |
|-----------|-------|
| Total Supply | 1,000,000,000 SUB |
| Initial Circulation | 150,000,000 SUB (15%) |
| Inflation | 5% annually, decaying |
| Staking Rewards | 70% of inflation |
| Treasury | 20% of inflation |
| Burn Mechanism | 50% of fees burned |

> The allocations below assign 100% of total supply; "initial circulation" of 15%
> refers to the portion unlocked at genesis. The rest is vested/locked and enters
> circulation over time, which is why both figures can be consistent.

## Token Distribution

### Initial Allocation

Allocation splits the **total** 1B supply (vested over time, not all circulating day one):

- **Community & Ecosystem**: 40% (400M SUB) - Airdrops, grants, partnerships
- **Team & Advisors**: 20% (200M SUB) - 4-year vesting with 1-year cliff
- **Investors**: 15% (150M SUB) - Private sale participants
- **Treasury**: 15% (150M SUB) - Protocol development and operations
- **Staking Reserve**: 10% (100M SUB) - Bootstrap initial staking rewards

### Vesting Schedule

The design proposes four-year vesting for team and investor allocations, with
shorter or no lockups for some community allocations. No vesting contract or
live token distribution is implemented here.

## How Inflation Works

The design target starts annual issuance at 5% of total supply and tapers it over
time. No issuance mechanism is implemented in this repository.

### Where Inflation Goes

The proposed split sends 70% of issuance to validators/stakers and 30% to the
treasury. Reward and treasury distribution are not implemented.

## Staking Mechanics

The design proposes the following staking parameters:

- **Minimum Validator Self-Stake**: 10,000,000 SUB — enforced as
  `MIN_VALIDATOR_STAKE` in `crates/subhost-consensus/src/staking.rs`
- **Maximum Commission**: 10,000 basis points — enforced by `Validator::validate`
- **Minimum Delegation**: 1,000 SUB (design target; only a non-zero amount is enforced)
- **Unbonding Period**: 14 days (design target; not enforced)
- **Reward Distribution**: Per block (design target; no issuance implemented)
- **Slashing**: implemented in memory. Double-signing burns the whole stake and
  ejects the validator along with its delegations, malicious behaviour takes 50%,
  and downtime takes 1%. Evidence must carry a proof and is recorded before any
  mutation.

> `StakingModule` implements registration, delegation, undelegation, the validator
> set with its quorum sizing, and slashing, all with checked arithmetic. It does
> **not** implement bonding periods, reward distribution, or unbonding, and it
> holds no on-chain state — nothing in this repository connects it to a running
> chain.

### Validator Economics

The envisioned validator role would require operators to:

- Maintain high uptime (99.9%+)
- Stay synced with the network
- Vote on proposals
- Not get slashed for double-signing or downtime

The 5-10% validator commission range is a planning assumption, not implemented
protocol behavior.

## Transaction Fees

The proposed fee model would charge SUB based on:

- **Base Fee**: Fixed cost for including the transaction
- **Compute Units**: How much processing power the tx needs
- **Storage**: If the tx writes data to state

### Fee Burn

The design target burns half of transaction fees. No fee-burn accounting is
implemented.

## Governance

The governance design proposes stake-weighted voting for:

- Protocol upgrades
- Parameter changes (fees, inflation rate, etc.)
- Treasury spending
- Validator set changes

### Proposal Process

1. Anyone can submit a proposal (proposed deposit: 100 SUB, refunded if passed)
2. Community discussion lasts two weeks
3. Voting lasts one week
4. A proposal passes if the proposed quorum and majority thresholds are met

These timings, deposits, and stake-weighted rules are not enforced by the current
governance scaffold.

## Economic Attacks and Defenses

### Nothing at Stake

In a future PoS network, validators could validate multiple conflicting chains
without cost. Planned mitigations include:

- Slashing for double-signing
- Inactivity leaks for offline validators
- Economic finality via bonding periods

### Long Range Attacks

An attacker could buy old private keys and rewrite history. Planned mitigations
include:

- Weak subjectivity (social consensus on checkpoints)
- Validator set rotation
- Unbonding period makes attacks expensive

### Sybil Resistance

Creating fake identities is cheap. Requiring stake is the planned Sybil-resistance
mechanism; it is not enforced by a running network here.

## Token Utility

### Proposed Uses

- Pay transaction fees
- Stake for rewards and governance weight
- Serve as collateral in future applications
- Pay for storage on the network
- Tip validators for priority inclusion
- Access premium features
- Cross-chain transfers via IBC

## Questions?

This is a living document. If something is unclear or missing, open an issue or ask in the community channels.
