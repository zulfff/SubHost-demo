//! EIP-1559-inspired fee market (S6.2, Q-017).
//!
//! A genesis-bound [`FeePolicy`] drives an exact-integer base fee recorded in
//! version 2 block headers. Legacy (policy-less) chains keep the fixed
//! gas-price burn rules; nothing here applies to them. The settlement
//! function is pure so the block path and ledger replay compute identical
//! fee distributions from the same inputs.
//!
//! Deliberate deviations from EIP-1559, accepted in Q-017: the downward step
//! is at least 1 (so an idle chain converges to the floor), and an explicit
//! ceiling keeps aggregate base-fee charging representable in `u128`. A
//! one-unit change at low fees can exceed 12.5%; no percentage bound is
//! claimed.

use crate::{Amount, BlockHeader, Gas, Transaction};
use serde::{Deserialize, Serialize};

/// Elasticity pinned for this policy version: `target = block_gas_limit / 2`.
pub const FEE_ELASTICITY_MULTIPLIER: u64 = 2;
/// Base-fee change denominator pinned for this policy version.
pub const FEE_CHANGE_DENOMINATOR: u64 = 8;
/// Header version that first records `base_fee_per_gas`.
pub const FEE_HEADER_VERSION: u32 = 2;

/// Genesis-bound fee policy. Bound into the canonical consensus context so
/// different floors/ceilings cannot vote in one protocol instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeePolicy {
    /// Base fee of the first produced block.
    pub initial_base_fee_per_gas: Amount,
    /// Floor the base fee converges to when blocks stay below target.
    pub min_base_fee_per_gas: Amount,
    /// Ceiling that keeps `base_fee * block_gas_limit` representable.
    pub max_base_fee_per_gas: Amount,
    /// Must be [`FEE_ELASTICITY_MULTIPLIER`] for this policy version.
    pub elasticity_multiplier: u64,
    /// Must be [`FEE_CHANGE_DENOMINATOR`] for this policy version.
    pub change_denominator: u64,
}

impl FeePolicy {
    /// Structural validation against the chain's fixed block gas limit.
    ///
    /// Pins elasticity 2 / denominator 8 (a future policy version owns other
    /// values), requires an even positive limit, `1 <= min <= initial <= max`,
    /// and `max * block_gas_limit <= Amount::MAX`.
    pub fn validate(&self, block_gas_limit: Gas) -> Result<(), FeeError> {
        if self.elasticity_multiplier != FEE_ELASTICITY_MULTIPLIER {
            return Err(FeeError::UnsupportedElasticity(self.elasticity_multiplier));
        }
        if self.change_denominator != FEE_CHANGE_DENOMINATOR {
            return Err(FeeError::UnsupportedDenominator(self.change_denominator));
        }
        if block_gas_limit == 0 || !block_gas_limit.is_multiple_of(2) {
            return Err(FeeError::InvalidBlockGasLimit(block_gas_limit));
        }
        if self.min_base_fee_per_gas == 0
            || self.min_base_fee_per_gas > self.initial_base_fee_per_gas
            || self.initial_base_fee_per_gas > self.max_base_fee_per_gas
        {
            return Err(FeeError::InvalidFeeBounds);
        }
        self.max_base_fee_per_gas
            .checked_mul(u128::from(block_gas_limit))
            .ok_or(FeeError::FeeBoundOverflow)?;
        Ok(())
    }

    /// Fee context for the first produced block: the genesis initial fee.
    pub fn initial_context(&self, block_gas_limit: Gas) -> Result<FeeContext, FeeError> {
        self.validate(block_gas_limit)?;
        Ok(FeeContext { base_fee_per_gas: self.initial_base_fee_per_gas, block_gas_limit })
    }

    /// Fee context for the block after `parent`, from the parent's utilisation.
    ///
    /// Rejects a pre-fee-market parent, a parent whose gas limit differs from
    /// the genesis-bound limit, invalid parent gas usage, and a parent base
    /// fee outside the policy bounds — never a defaulted or wrapped fee.
    ///
    /// Fee continuity across the calldata fork (Q-037): a version 3 block may
    /// derive its fee from a **version 2 parent** — the fork changes the
    /// validity rule, not the fee schedule. A version 1 parent is still
    /// refused.
    ///
    /// Exact floor arithmetic without a wide-integer dependency: with
    /// `target = limit / 2`, `b = parent base fee`, `delta = |gas_used -
    /// target| <= target`, `q = b / target`, `r = b % target`,
    /// `raw = (q * delta + (r * delta) / target) / 8` equals
    /// `floor(b * delta / (target * 8))`. Every intermediate fits `u128`
    /// (`r * delta < target^2 < 2^126`; the summed numerator is at most `b`),
    /// and checked arithmetic is still used throughout.
    pub fn next_context(
        &self,
        parent: &BlockHeader,
        block_gas_limit: Gas,
    ) -> Result<FeeContext, FeeError> {
        self.validate(block_gas_limit)?;
        if parent.version < FEE_HEADER_VERSION {
            return Err(FeeError::LegacyParent(parent.version));
        }
        if parent.gas_limit != block_gas_limit {
            return Err(FeeError::ParentGasLimitMismatch {
                expected: block_gas_limit,
                got: parent.gas_limit,
            });
        }
        if parent.gas_used > parent.gas_limit {
            return Err(FeeError::InvalidParentGas {
                got: parent.gas_used,
                limit: parent.gas_limit,
            });
        }
        let b = parent.base_fee_per_gas;
        if b < self.min_base_fee_per_gas || b > self.max_base_fee_per_gas {
            return Err(FeeError::ParentFeeOutOfBounds(b));
        }

        let target = u128::from(block_gas_limit / 2);
        let gas_used = u128::from(parent.gas_used);
        if gas_used == target {
            return Ok(FeeContext { base_fee_per_gas: b, block_gas_limit });
        }
        let delta = gas_used.abs_diff(target);
        let q = b / target;
        let r = b % target;
        let numerator = q
            .checked_mul(delta)
            .ok_or(FeeError::Overflow)?
            .checked_add(r.checked_mul(delta).ok_or(FeeError::Overflow)? / target)
            .ok_or(FeeError::Overflow)?;
        let raw = numerator / u128::from(FEE_CHANGE_DENOMINATOR);
        // Both directions move at least one unit; see the module docs.
        let step = raw.max(1);
        let base_fee_per_gas = if gas_used > target {
            // Compare against the headroom first so the sum cannot overflow.
            let headroom = self.max_base_fee_per_gas - b;
            if step >= headroom {
                self.max_base_fee_per_gas
            } else {
                b + step
            }
        } else {
            let above_floor = b - self.min_base_fee_per_gas;
            if step >= above_floor {
                self.min_base_fee_per_gas
            } else {
                b - step
            }
        };
        Ok(FeeContext { base_fee_per_gas, block_gas_limit })
    }
}

/// The fee parameters in force for one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeContext {
    pub base_fee_per_gas: Amount,
    pub block_gas_limit: Gas,
}

/// Fee split for one executed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeeBreakdown {
    /// `gas_price * gas_used`, debited from the sender.
    pub total: Amount,
    /// `base_fee_per_gas * gas_used`, burned.
    pub burned: Amount,
    /// `total - burned`, credited to the block validator at end of block.
    pub priority: Amount,
}

/// Aggregate fee distribution for one block. Conservation holds by
/// construction: `total = burned + priority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockFeeSettlement {
    pub total: Amount,
    pub burned: Amount,
    pub priority: Amount,
}

/// Every fee failure is explicit; nothing wraps or substitutes a fee.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeeError {
    #[error("fee policy requires elasticity multiplier {FEE_ELASTICITY_MULTIPLIER}, got {0}")]
    UnsupportedElasticity(u64),

    #[error("fee policy requires change denominator {FEE_CHANGE_DENOMINATOR}, got {0}")]
    UnsupportedDenominator(u64),

    #[error("block gas limit must be positive and even, got {0}")]
    InvalidBlockGasLimit(Gas),

    #[error("fee bounds must satisfy 1 <= min <= initial <= max")]
    InvalidFeeBounds,

    #[error("max base fee times block gas limit overflows")]
    FeeBoundOverflow,

    #[error("parent header version {0} predates the fee market")]
    LegacyParent(u32),

    #[error("parent gas limit {got} does not match the genesis-bound limit {expected}")]
    ParentGasLimitMismatch { expected: Gas, got: Gas },

    #[error("parent gas used {got} exceeds its gas limit {limit}")]
    InvalidParentGas { got: Gas, limit: Gas },

    #[error("parent base fee {0} is outside the policy bounds")]
    ParentFeeOutOfBounds(Amount),

    #[error("fee arithmetic overflow")]
    Overflow,

    #[error("transaction gas price {gas_price} is below the block base fee {base_fee}")]
    Underpriced { gas_price: Amount, base_fee: Amount },

    #[error("gas used {got} exceeds the transaction gas limit {limit}")]
    InvalidGasUsed { got: Gas, limit: Gas },

    #[error("calldata policy version {0} is not supported")]
    UnsupportedCalldataVersion(u32),

    #[error("calldata gas per byte must be nonzero")]
    ZeroCalldataRate,

    #[error("calldata rate {gas_per_byte} times the maximum transaction size exceeds the block gas limit {block_gas_limit}")]
    CalldataRateTooHigh { gas_per_byte: u64, block_gas_limit: Gas },

    #[error("transaction gas limit {gas_limit} is below the intrinsic calldata gas {required}")]
    UnderIntrinsic { gas_limit: Gas, required: Gas },

    #[error("calldata size {len} exceeds the protocol maximum {max}")]
    CalldataTooLarge { len: usize, max: usize },
}

impl Transaction {
    /// Split this transaction's fee under `context` for `gas_used` units.
    ///
    /// The sender pays its signed `gas_price` (legacy envelope, per Q-017);
    /// the base-fee portion burns and the remainder is the priority tip.
    /// A gas price below the block base fee is rejected, never clamped.
    pub fn fee_breakdown(
        &self,
        context: &FeeContext,
        gas_used: Gas,
    ) -> Result<FeeBreakdown, FeeError> {
        if gas_used > self.gas_limit {
            return Err(FeeError::InvalidGasUsed { got: gas_used, limit: self.gas_limit });
        }
        if self.gas_price < context.base_fee_per_gas {
            return Err(FeeError::Underpriced {
                gas_price: self.gas_price,
                base_fee: context.base_fee_per_gas,
            });
        }
        let gas = u128::from(gas_used);
        let total = self.gas_price.checked_mul(gas).ok_or(FeeError::Overflow)?;
        let burned = context.base_fee_per_gas.checked_mul(gas).ok_or(FeeError::Overflow)?;
        Ok(FeeBreakdown { total, burned, priority: total - burned })
    }
}

/// Settle all executed transactions of one block against its fee context.
///
/// Pure and deterministic: the block producer, the peer-block verifier and
/// the ledger replay must all call this with the same `(transaction,
/// gas_used)` pairs so their state roots agree. Tips are aggregated here and
/// credited once at end of block — never spendable by an earlier transaction
/// in the same block. An empty block settles to zero.
pub fn settle_block_fees<'a>(
    context: &FeeContext,
    executed: impl IntoIterator<Item = (&'a Transaction, Gas)>,
) -> Result<BlockFeeSettlement, FeeError> {
    let mut settlement = BlockFeeSettlement::default();
    for (transaction, gas_used) in executed {
        let breakdown = transaction.fee_breakdown(context, gas_used)?;
        settlement.total =
            settlement.total.checked_add(breakdown.total).ok_or(FeeError::Overflow)?;
        settlement.burned =
            settlement.burned.checked_add(breakdown.burned).ok_or(FeeError::Overflow)?;
        settlement.priority =
            settlement.priority.checked_add(breakdown.priority).ok_or(FeeError::Overflow)?;
    }
    Ok(settlement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Address, Hash, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT};

    fn policy() -> FeePolicy {
        FeePolicy {
            initial_base_fee_per_gas: 8,
            min_base_fee_per_gas: 1,
            max_base_fee_per_gas: Amount::MAX / u128::from(BLOCK_GAS_LIMIT),
            elasticity_multiplier: FEE_ELASTICITY_MULTIPLIER,
            change_denominator: FEE_CHANGE_DENOMINATOR,
        }
    }

    fn parent(gas_used: Gas, base_fee: Amount) -> BlockHeader {
        BlockHeader {
            version: FEE_HEADER_VERSION,
            chain_id: 1,
            height: 1,
            timestamp: 1_700_000_000,
            parent_hash: Hash::ZERO,
            state_root: Hash::ZERO,
            tx_root: Hash::ZERO,
            receipt_root: Hash::ZERO,
            validator: Address::ZERO,
            gas_used,
            gas_limit: BLOCK_GAS_LIMIT,
            extra_data: Vec::new(),
            base_fee_per_gas: base_fee,
        }
    }

    fn tx(gas_price: Amount, gas_limit: Gas) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from: Address::new([1; 20]),
            to: Some(Address::new([2; 20])),
            value: 0,
            gas_price,
            gas_limit,
            data: Vec::new(),
            chain_id: 1,
            public_key: [0; 32],
            signature: TransactionSignature::EMPTY,
        }
    }

    #[test]
    fn validate_pins_the_policy_version() {
        let limit = BLOCK_GAS_LIMIT;
        assert!(policy().validate(limit).is_ok());
        let mut bad = policy();
        bad.elasticity_multiplier = 3;
        assert_eq!(bad.validate(limit), Err(FeeError::UnsupportedElasticity(3)));
        let mut bad = policy();
        bad.change_denominator = 16;
        assert_eq!(bad.validate(limit), Err(FeeError::UnsupportedDenominator(16)));
        assert_eq!(policy().validate(0), Err(FeeError::InvalidBlockGasLimit(0)));
        assert_eq!(policy().validate(3), Err(FeeError::InvalidBlockGasLimit(3)));
        let mut bad = policy();
        bad.min_base_fee_per_gas = 0;
        assert_eq!(bad.validate(limit), Err(FeeError::InvalidFeeBounds));
        let mut bad = policy();
        bad.initial_base_fee_per_gas = 0;
        assert_eq!(bad.validate(limit), Err(FeeError::InvalidFeeBounds));
        let mut bad = policy();
        bad.max_base_fee_per_gas = 7;
        assert_eq!(bad.validate(limit), Err(FeeError::InvalidFeeBounds));
        let mut bad = policy();
        bad.max_base_fee_per_gas = Amount::MAX;
        assert_eq!(bad.validate(limit), Err(FeeError::FeeBoundOverflow));
    }

    #[test]
    fn initial_context_uses_the_genesis_fee() {
        let context = policy().initial_context(BLOCK_GAS_LIMIT).unwrap();
        assert_eq!(context.base_fee_per_gas, 8);
        assert_eq!(context.block_gas_limit, BLOCK_GAS_LIMIT);
    }

    #[test]
    fn utilisation_examples_from_the_contract() {
        let policy = policy();
        let limit = BLOCK_GAS_LIMIT;
        let target = limit / 2;
        // Base 8: at target stays 8, full rises to 9, empty falls to 7.
        assert_eq!(policy.next_context(&parent(target, 8), limit).unwrap().base_fee_per_gas, 8);
        assert_eq!(policy.next_context(&parent(limit, 8), limit).unwrap().base_fee_per_gas, 9);
        assert_eq!(policy.next_context(&parent(0, 8), limit).unwrap().base_fee_per_gas, 7);
        // Base 2 on an empty block falls to 1; base 1 stays at the floor.
        assert_eq!(policy.next_context(&parent(0, 2), limit).unwrap().base_fee_per_gas, 1);
        assert_eq!(policy.next_context(&parent(0, 1), limit).unwrap().base_fee_per_gas, 1);
    }

    #[test]
    fn next_context_rejects_invalid_parents() {
        let policy = policy();
        let limit = BLOCK_GAS_LIMIT;
        let mut v1 = parent(0, 8);
        v1.version = 1;
        assert_eq!(policy.next_context(&v1, limit), Err(FeeError::LegacyParent(1)));
        let mut wrong_limit = parent(0, 8);
        wrong_limit.gas_limit = limit - 2;
        assert_eq!(
            policy.next_context(&wrong_limit, limit),
            Err(FeeError::ParentGasLimitMismatch { expected: limit, got: limit - 2 })
        );
        let mut over_gas = parent(0, 8);
        over_gas.gas_used = limit + 1;
        assert_eq!(
            policy.next_context(&over_gas, limit),
            Err(FeeError::InvalidParentGas { got: limit + 1, limit })
        );
        assert_eq!(
            policy.next_context(&parent(0, 0), limit),
            Err(FeeError::ParentFeeOutOfBounds(0))
        );
        assert_eq!(
            policy.next_context(&parent(0, policy.max_base_fee_per_gas + 1), limit),
            Err(FeeError::ParentFeeOutOfBounds(policy.max_base_fee_per_gas + 1))
        );
    }

    #[test]
    fn next_context_keeps_fee_continuity_across_the_calldata_fork() {
        let policy = policy();
        let limit = BLOCK_GAS_LIMIT;
        // The transition case dewi asked to pin: a v3 successor derives its
        // fee from a v2 parent (full block at base 8 rises to 9).
        let v2_parent = parent(limit, 8);
        assert_eq!(v2_parent.version, FEE_HEADER_VERSION);
        assert_eq!(policy.next_context(&v2_parent, limit).unwrap().base_fee_per_gas, 9);
        // And from a v3 parent once the fork is live.
        let v3_parent =
            BlockHeader { version: crate::CALLDATA_HEADER_VERSION, ..v2_parent.clone() };
        assert_eq!(policy.next_context(&v3_parent, limit).unwrap().base_fee_per_gas, 9);
        // A v1 parent stays refused on either side of the fork.
        let v1_parent = BlockHeader { version: 1, base_fee_per_gas: 0, ..v2_parent };
        assert_eq!(policy.next_context(&v1_parent, limit), Err(FeeError::LegacyParent(1)));
    }

    #[test]
    fn exact_floor_matches_the_mathematical_formula() {
        // Small limit so target^2 stays tiny and brute force is cheap.
        let limit: Gas = 100;
        let mut policy = policy();
        policy.max_base_fee_per_gas = 1_000_000;
        let target = u128::from(limit / 2);
        for b in [1u128, 2, 7, 8, 9, 63, 64, 65, 127, 999_999, 1_000_000] {
            for gas_used in 0..=limit {
                let delta = (u128::from(gas_used)).abs_diff(target);
                let expected_raw = b * delta / target / 8;
                let expected = if u128::from(gas_used) == target {
                    b
                } else if u128::from(gas_used) > target {
                    (b + expected_raw.max(1)).min(policy.max_base_fee_per_gas)
                } else {
                    b.saturating_sub(expected_raw.max(1)).max(policy.min_base_fee_per_gas)
                };
                let mut header = parent(gas_used, b);
                header.gas_limit = limit;
                let got = policy.next_context(&header, limit).unwrap();
                assert_eq!(got.base_fee_per_gas, expected, "b={b} gas_used={gas_used}");
            }
        }
    }

    #[test]
    fn ceiling_clamps_without_overflow() {
        let limit: Gas = 2;
        let mut policy = policy();
        policy.max_base_fee_per_gas = Amount::MAX / 2;
        policy.initial_base_fee_per_gas = policy.max_base_fee_per_gas;
        // Full block at the ceiling: the step must clamp, not wrap.
        let mut at_ceiling = parent(limit, policy.max_base_fee_per_gas);
        at_ceiling.gas_limit = limit;
        assert_eq!(
            policy.next_context(&at_ceiling, limit).unwrap().base_fee_per_gas,
            policy.max_base_fee_per_gas
        );
        // One below the ceiling with a step larger than the headroom.
        let mut near_ceiling = parent(limit, policy.max_base_fee_per_gas - 1);
        near_ceiling.gas_limit = limit;
        assert_eq!(
            policy.next_context(&near_ceiling, limit).unwrap().base_fee_per_gas,
            policy.max_base_fee_per_gas
        );
    }

    #[test]
    fn idle_chain_converges_to_the_floor() {
        let policy = policy();
        let limit = BLOCK_GAS_LIMIT;
        let mut header = parent(0, 8);
        let mut steps = 0;
        while header.base_fee_per_gas > policy.min_base_fee_per_gas {
            let context = policy.next_context(&header, limit).unwrap();
            header.base_fee_per_gas = context.base_fee_per_gas;
            steps += 1;
            assert!(steps < 64, "must converge quickly at the minimum-1 step");
        }
        assert_eq!(header.base_fee_per_gas, 1);
        // And stays there.
        assert_eq!(policy.next_context(&header, limit).unwrap().base_fee_per_gas, 1);
    }

    #[test]
    fn breakdown_splits_and_rejects_underpriced_transactions() {
        let context = FeeContext { base_fee_per_gas: 8, block_gas_limit: BLOCK_GAS_LIMIT };
        let transaction = tx(10, 21_000);
        let breakdown = transaction.fee_breakdown(&context, 21_000).unwrap();
        assert_eq!(breakdown.total, 210_000);
        assert_eq!(breakdown.burned, 168_000);
        assert_eq!(breakdown.priority, 42_000);
        assert_eq!(breakdown.total, transaction.fee().unwrap());
        assert_eq!(
            tx(7, 21_000).fee_breakdown(&context, 21_000),
            Err(FeeError::Underpriced { gas_price: 7, base_fee: 8 })
        );
        assert_eq!(
            transaction.fee_breakdown(&context, 21_001),
            Err(FeeError::InvalidGasUsed { got: 21_001, limit: 21_000 })
        );
        // Zero gas settles to zero even at a high base fee.
        let zero = transaction.fee_breakdown(&context, 0).unwrap();
        assert_eq!(zero, FeeBreakdown::default());
    }

    #[test]
    fn breakdown_overflow_is_an_error() {
        let context = FeeContext { base_fee_per_gas: 1, block_gas_limit: BLOCK_GAS_LIMIT };
        let transaction = tx(Amount::MAX, 2);
        assert_eq!(transaction.fee_breakdown(&context, 2), Err(FeeError::Overflow));
    }

    #[test]
    fn settlement_conserves_and_aggregates() {
        let context = FeeContext { base_fee_per_gas: 8, block_gas_limit: BLOCK_GAS_LIMIT };
        let a = tx(10, 21_000);
        let b = tx(9, 42_000);
        let settlement = settle_block_fees(&context, [(&a, 21_000), (&b, 42_000)]).unwrap();
        assert_eq!(settlement.total, 210_000 + 378_000);
        assert_eq!(settlement.burned, 8 * (21_000 + 42_000));
        assert_eq!(settlement.priority, settlement.total - settlement.burned);
        // Empty block: zero settlement, no fee movement.
        assert_eq!(settle_block_fees(&context, []).unwrap(), BlockFeeSettlement::default());
        // An underpriced member fails the whole settlement before any credit.
        let cheap = tx(1, 21_000);
        assert!(settle_block_fees(&context, [(&a, 21_000), (&cheap, 21_000)]).is_err());
    }

    #[test]
    fn settlement_aggregate_overflow_is_an_error() {
        let limit: Gas = 2;
        let context = FeeContext { base_fee_per_gas: 1, block_gas_limit: limit };
        let expensive = tx(Amount::MAX / 2, 2);
        assert_eq!(
            settle_block_fees(&context, [(&expensive, 2), (&expensive, 2)]),
            Err(FeeError::Overflow)
        );
    }
}
