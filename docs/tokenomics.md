# Tokenomics

The SUB token powers the Subhost network. This doc explains how it works, how tokens get created, and what you can do with them.

## Token Basics

| Parameter | Value |
|-----------|-------|
| Total Supply | 1,000,000,000 SUB |
| Initial Circulation | 150,000,000 SUB (15%) |
| Inflation | 5% annually, decaying |
| Staking Rewards | 70% of inflation |
| Treasury | 20% of inflation |
| Burn Mechanism | 50% of fees burned |

## Token Distribution

### Initial Allocation

- **Community & Ecosystem**: 40% (400M SUB) - Airdrops, grants, partnerships
- **Team & Advisors**: 20% (200M SUB) - 4-year vesting with 1-year cliff
- **Investors**: 15% (150M SUB) - Private sale participants
- **Treasury**: 15% (150M SUB) - Protocol development and operations
- **Staking Reserve**: 10% (100M SUB) - Bootstrap initial staking rewards

### Vesting Schedule

Team and investor tokens vest over 4 years. This means they can't dump everything at once and crash the price. Community tokens have shorter lockups or none at all.

## How Inflation Works

Every year, new SUB tokens get created at 5% of the total supply. But this rate slowly drops over time. The idea is to bootstrap the network early when we need to incentivize validators and users, then taper off as the ecosystem matures.

### Where Inflation Goes

Most of it (70%) goes to validators and stakers as rewards for securing the network. The remaining 30% funds the treasury for ongoing development.

## Staking Mechanics

You can stake your SUB tokens to help secure the network and earn rewards. Here's how it works:

- **Minimum Stake**: 1,000 SUB
- **Unbonding Period**: 14 days (your tokens are locked when you unstake)
- **Reward Distribution**: Every block (roughly every second)
- **Slashing**: You can lose part of your stake if your validator misbehaves

### Validator Economics

Running a validator requires technical expertise and capital. Validators need to:

- Maintain high uptime (99.9%+)
- Stay synced with the network
- Vote on proposals
- Not get slashed for double-signing or downtime

In return, validators earn commission on delegator rewards, typically 5-10%.

## Transaction Fees

Every transaction costs a small fee paid in SUB. The fee depends on:

- **Base Fee**: Fixed cost for including the transaction
- **Compute Units**: How much processing power the tx needs
- **Storage**: If the tx writes data to state

### Fee Burn

Half of all transaction fees get burned (destroyed forever). This creates deflationary pressure that offsets some of the inflation. As network usage grows, more fees get burned, potentially making SUB deflationary overall.

## Governance

SUB holders can vote on protocol changes. Your voting power equals your staked balance. Things you can vote on:

- Protocol upgrades
- Parameter changes (fees, inflation rate, etc.)
- Treasury spending
- Validator set changes

### Proposal Process

1. Anyone can submit a proposal (costs 100 SUB, refunded if passed)
2. Community discusses for 2 weeks
3. Voting period lasts 1 week
4. If 50%+ of staked tokens vote and majority approves, it passes

## Economic Attacks and Defenses

### Nothing at Stake

In pure PoS, validators could theoretically validate multiple conflicting chains without cost. We prevent this with:

- Slashing for double-signing
- Inactivity leaks for offline validators
- Economic finality via bonding periods

### Long Range Attacks

An attacker could buy old private keys and rewrite history. We defend against this with:

- Weak subjectivity (social consensus on checkpoints)
- Validator set rotation
- Unbonding period makes attacks expensive

### Sybil Resistance

Creating fake identities is cheap. We require stake to participate, making Sybil attacks economically prohibitive.

## Token Utility

### Current Uses

- Pay for transaction fees
- Stake to earn rewards
- Vote on governance
- Collateral for DeFi apps

### Future Uses

- Pay for storage on the network
- Tip validators for priority inclusion
- Access premium features
- Cross-chain transfers via IBC

## Questions?

This is a living document. If something is unclear or missing, open an issue or ask in the community channels.
