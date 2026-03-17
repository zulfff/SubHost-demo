# SUB Tokenomics Design

## Overview

The SUB token is the native utility and governance token of the Subhost Web3 protocol. It serves multiple critical functions within the ecosystem while maintaining long-term value accrual through carefully designed economic mechanisms.

## Token Parameters

| Parameter | Value |
|-----------|-------|
| **Token Name** | Subhost Token |
| **Symbol** | SUB |
| **Total Supply** | 1,000,000,000 SUB (1 Billion) |
| **Decimals** | 18 |
| **Initial Circulating Supply** | 150,000,000 SUB (15%) |
| **Maximum Supply** | Fixed at 1 Billion |

## Token Allocation

### Distribution Breakdown

```
Community & Ecosystem:     400,000,000 SUB (40%)
├── Developer Grants:      150,000,000 SUB
├── Bug Bounties:           50,000,000 SUB
├── Marketing:             100,000,000 SUB
└── Community Rewards:     100,000,000 SUB

Team & Advisors:           200,000,000 SUB (20%)
├── Core Team:             150,000,000 SUB
├── Advisors:               50,000,000 SUB

Staking Rewards:           150,000,000 SUB (15%)
├── Validator Rewards:     100,000,000 SUB
├── Delegator Rewards:      50,000,000 SUB

Private Sale:              100,000,000 SUB (10%)
├── Seed Round:             40,000,000 SUB
├── Series A:               60,000,000 SUB

Public Sale (IDO):          50,000,000 SUB (5%)

Treasury Reserve:          100,000,000 SUB (10%)
├── Protocol Development:   60,000,000 SUB
├── Emergency Reserve:      40,000,000 SUB
```

### Vesting Schedules

| Category | Cliff | Vesting Period | Release Schedule |
|----------|-------|----------------|------------------|
| Team | 12 months | 48 months | Quarterly, linear |
| Advisors | 6 months | 24 months | Quarterly, linear |
| Private Sale | 6 months | 24 months | Monthly, linear |
| Community | 0 months | 48 months | Milestone-based |
| Staking Rewards | 0 months | 60 months | Per-block |

## Economic Model

### Inflation Schedule

The protocol features a controlled inflation mechanism to incentivize network security:

| Year | Annual Inflation | Total New Tokens |
|------|------------------|------------------|
| 1 | 5.00% | 50,000,000 |
| 2 | 4.50% | 47,250,000 |
| 3 | 4.00% | 42,000,000 |
| 4 | 3.50% | 36,750,000 |
| 5 | 3.00% | 31,500,000 |
| 6+ | 2.00% | Decaying to 0% over 20 years |

### Inflation Allocation

```
Staking Rewards:     70% (Validators & Delegators)
Treasury:           20% (Protocol Development)
Burn Mechanism:     10% (Deflationary pressure)
```

## Token Utility

### 1. Network Security (Staking)

- **Minimum Stake**: 10,000 SUB for validators
- **Delegation**: No minimum for delegators
- **Unbonding Period**: 21 days
- **Slashing**: Up to 100% for double-signing

**Staking Rewards Formula:**
```
Annual Reward = (Your Stake / Total Staked) × Annual Issuance × (1 - Validator Commission)
```

**Expected APY Range:** 12% - 18% (varies based on total staked amount)

### 2. Transaction Fees

| Operation | Base Fee (SUB) | Priority Fee |
|-----------|---------------|--------------|
| Transfer | 0.0001 | Variable |
| Contract Call | 0.001 | Variable |
| Contract Deploy | 0.01 | Variable |
| IBC Transfer | 0.0005 | Variable |
| Staking | 0.0001 | Fixed |

**Fee Burn:** 50% of all transaction fees are burned (deflationary mechanism)

### 3. Governance

**Voting Power:** 1 SUB = 1 vote (quadratic voting planned for V2)

**Governance Rights:**
- Protocol parameter changes
- Treasury spending
- Upgrade proposals
- Validator set changes

**Proposal Requirements:**
- Minimum deposit: 100,000 SUB
- Voting period: 14 days
- Quorum: 40% of staked supply
- Threshold: 50% + 1 majority

### 4. IBC Bridge Fees

Cross-chain transfers require SUB as bridge fees:
- Source chain: Native token
- Bridge fee: Paid in SUB (0.1% of transfer value, min 1 SUB)
- Destination: Receive wrapped assets

## Value Accrual Mechanisms

### 1. Fee Burn

50% of all fees are permanently burned, creating deflationary pressure:

```
Daily Burn = (Daily Transaction Fees + Bridge Fees) × 0.5

Annual Burn Projection (Year 1): ~5,000,000 SUB
```

### 2. MEV Capture

Protocol-level MEV extraction:
- 80% distributed to stakers
- 20% to treasury

Estimated MEV yield: +2-4% APY for stakers

### 3. State Rent

Smart contracts must pay rent for state storage:
- Base rent: 1 SUB per KB per year
- Unpaid rent: State moved to archival after 1 year
- Recovery fee: 10 SUB per KB

### 4. Fee Market

Dynamic fee adjustment based on network demand:
- Base fee adjusts every block
- Priority fee auction for inclusion
- Surge pricing during congestion

## Treasury Management

### Revenue Sources

1. **Inflation Allocation:** 20% of new issuance
2. **Transaction Fees:** 50% of fees (after burn)
3. **MEV Revenue:** 20% of extracted MEV
4. **Slashing:** 50% of slashed tokens

### Expenditure Categories

| Category | Allocation | Purpose |
|----------|------------|---------|
| R&D | 40% | Core protocol development |
| Grants | 30% | Ecosystem expansion |
| Security | 15% | Audits & bug bounties |
| Operations | 15% | Legal, marketing, admin |

### Streaming Payments

Treasury supports streaming payments for continuous funding:
- Salary streams for core contributors
- Grant streams for long-term projects
- Cancelable by governance vote

## Liquidity Incentives

### DEX Liquidity Mining

**SUB/ETH Pool:**
- Monthly allocation: 2,000,000 SUB
- Duration: 24 months
- Bonus for 30+ day lockups

**SUB/USDC Pool:**
- Monthly allocation: 1,500,000 SUB
- Duration: 24 months
- Stable pair with reduced IL risk

### Cross-Chain Liquidity

Incentives for IBC-enabled DEXs:
- Osmosis: 500,000 SUB/month
- Cosmos Hub: 300,000 SUB/month

## Risk Mitigation

### Inflation Control

- **Hard Cap:** Maximum 5% annual inflation
- **Deflationary Floor:** If burn > issuance, inflation reduces to 0%
- **Emergency Brake:** Governance can pause inflation in extreme scenarios

### Supply Shock Protection

- **Unlock Scheduling:** Staggered team/advisor unlocks prevent dumps
- **Liquidity Locks:** DEX liquidity locked for 12 months minimum
- **Buyback Reserve:** 5% of treasury allocated for market stabilization

### Governance Attack Prevention

- **Quadratic Voting:** Planned implementation to prevent whale control
- **Time-Locked Upgrades:** 48-hour delay on all critical parameter changes
- **Veto Council:** Emergency 5-of-9 multi-sig for malicious proposals

## Economic Projections

### Year 1 Projections

| Metric | Conservative | Base Case | Optimistic |
|--------|--------------|-----------|------------|
| Daily Transactions | 100,000 | 500,000 | 2,000,000 |
| Daily Fees (SUB) | 10,000 | 50,000 | 200,000 |
| Daily Burn (SUB) | 5,000 | 25,000 | 100,000 |
| Annual Burn Rate | 0.5% | 2.5% | 10% |
| Staking Ratio | 50% | 65% | 80% |
| Circulating Supply | 250M | 200M | 150M |

### Long-term Sustainability

The tokenomics model is designed to achieve equilibrium within 5 years:

- **Break-even Point:** When transaction fees cover validator rewards
- **Deflationary Phase:** Expected to begin in Year 3-4
- **Target Staking Ratio:** 65% (optimal security/liquidity balance)

## Comparison with Competitors

| Protocol | Inflation | Burn | Max Supply | Staking APY |
|----------|-----------|------|------------|-------------|
| **Subhost** | 5% → 0% | 50% fees | Fixed | 12-18% |
| Ethereum | 0.5-2% | 100% base fee | Unbounded | 3-5% |
| Solana | 8% | 50% fees | Unbounded | 6-8% |
| Cosmos | 7-20% | None | Unbounded | 10-15% |
| Polkadot | 10% | None | Unbounded | 14-16% |

## Conclusion

The SUB tokenomics model balances short-term incentivization with long-term sustainability through:

1. **Controlled inflation** that decays over time
2. **Aggressive burn mechanism** creating deflationary pressure
3. **Multiple value accrual streams** (fees, MEV, rent)
4. **Strong staking incentives** ensuring network security
5. **Governance utility** aligning holder interests with protocol success

The model is designed to transition from inflationary to deflationary as network adoption increases, ensuring SUB becomes increasingly scarce and valuable over time.
