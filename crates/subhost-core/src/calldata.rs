//! Intrinsic calldata gas as a validity floor (Q-037, SUBHOST-2026-003).
//!
//! A genesis-bound [`CalldataPolicy`] prices calldata through the gas axis:
//! a transaction carrying `B` bytes must declare `gas_limit >= gas_per_byte *
//! B`, or it is **invalid** — never clamped or topped up, the same discipline
//! as the underpriced refusal in Q-017. Consumption, debit, receipts and
//! settlement stay `gas_limit` (there is no execution engine yet), so the
//! sender pays for the whole declaration and a block's gas limit caps total
//! calldata. The `version` field is what lets S10 introduce real consumption
//! later without reinterpreting committed history.
//!
//! The rule is carried by version 3 block headers ([`CALLDATA_HEADER_VERSION`],
//! same 13-field layout as version 2): the header alone names the validity
//! rule, so the peer/certified-sync path can enforce it without a manifest.
//! Genesis and the storage manifest carry the numbers; replay refuses a
//! missing or foreign schedule on a priced history. Nothing here activates
//! until a genesis binds the policy and the node produces version 3 headers.
//!
//! [`CalldataPolicy::check_floor`] is pure so admission, selection and replay
//! enforce the identical rule independently (Q-037, dewi's condition); it also
//! refuses calldata above [`MAX_TX_DATA_BYTES`] even when the gas floor is met,
//! so the protocol cap is consensus validity on every path, not just the local
//! mempool cap (R-L2-02). The arithmetic lives only in this crate (Q-017's
//! no-duplicate-pricing rule).

use crate::fee::FeeError;
use crate::{Gas, Transaction};
use serde::{Deserialize, Serialize};

/// Rule version pinned for this policy version; unknown values are refused.
pub const CALLDATA_POLICY_VERSION: u32 = 1;
/// Header version that first enforces the calldata floor.
pub const CALLDATA_HEADER_VERSION: u32 = 3;
/// Protocol-level maximum calldata size of one transaction (128 KiB).
///
/// The mempool's `max_tx_data_bytes` defaults to this value and operators may
/// tighten it; policy validation uses this upper bound so any transaction the
/// protocol can carry stays includable under a validated rate.
pub const MAX_TX_DATA_BYTES: usize = 128 * 1024;

/// Genesis-bound calldata pricing. Absent means legacy unpriced calldata,
/// never an implicit upgrade; a priced chain starts from a fresh genesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalldataPolicy {
    /// Pinned to [`CALLDATA_POLICY_VERSION`]; unknown values are refused.
    pub version: u32,
    /// Gas charged per calldata byte, zero and nonzero alike.
    pub gas_per_byte: u64,
}

impl CalldataPolicy {
    /// Structural validation against the chain's fixed block gas limit.
    ///
    /// Fail-closed: the version must be [`CALLDATA_POLICY_VERSION`], the rate
    /// nonzero (a zero rate would reopen the residual — a chain that wants no
    /// calldata tightens the B-owned byte cap instead), and
    /// `gas_per_byte * MAX_TX_DATA_BYTES <= block_gas_limit` so a maximum-size
    /// transaction is still includable. The product is checked, never wrapped.
    pub fn validate(&self, block_gas_limit: Gas) -> Result<(), FeeError> {
        if self.version != CALLDATA_POLICY_VERSION {
            return Err(FeeError::UnsupportedCalldataVersion(self.version));
        }
        if self.gas_per_byte == 0 {
            return Err(FeeError::ZeroCalldataRate);
        }
        let max_intrinsic =
            self.gas_per_byte.checked_mul(MAX_TX_DATA_BYTES as u64).ok_or(FeeError::Overflow)?;
        if max_intrinsic > block_gas_limit {
            return Err(FeeError::CalldataRateTooHigh {
                gas_per_byte: self.gas_per_byte,
                block_gas_limit,
            });
        }
        Ok(())
    }

    /// Intrinsic gas of `data` under this rate, or `None` on overflow.
    pub fn intrinsic_gas(&self, data: &[u8]) -> Option<Gas> {
        self.gas_per_byte.checked_mul(data.len() as u64)
    }

    /// Enforce the validity floor on one transaction: its signed `gas_limit`
    /// must cover the intrinsic gas of its signed `data`, and the data must
    /// not exceed [`MAX_TX_DATA_BYTES`].
    ///
    /// The length refusal is part of the same pure check so every enforcing
    /// path (admission, selection, replay, peer-block acceptance) applies the
    /// protocol cap as a consensus rule, not just the local mempool cap: an
    /// oversized transaction that meets the gas floor is still invalid
    /// (R-L2-02). Under-intrinsic is a distinct refusal, never a clamp or
    /// top-up; overflow is invalid, never a wrap. The policy itself must
    /// already have passed [`Self::validate`] at bind time.
    pub fn check_floor(&self, transaction: &Transaction) -> Result<(), FeeError> {
        if transaction.data.len() > MAX_TX_DATA_BYTES {
            return Err(FeeError::CalldataTooLarge {
                len: transaction.data.len(),
                max: MAX_TX_DATA_BYTES,
            });
        }
        let required = self.intrinsic_gas(&transaction.data).ok_or(FeeError::Overflow)?;
        if transaction.gas_limit < required {
            return Err(FeeError::UnderIntrinsic { gas_limit: transaction.gas_limit, required });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Address, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT};

    fn policy() -> CalldataPolicy {
        CalldataPolicy { version: CALLDATA_POLICY_VERSION, gas_per_byte: 16 }
    }

    fn tx(data_len: usize, gas_limit: Gas) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from: Address::new([1; 20]),
            to: Some(Address::new([2; 20])),
            value: 0,
            gas_price: 1,
            gas_limit,
            data: vec![0xAB; data_len],
            chain_id: 1,
            public_key: [0; 32],
            signature: TransactionSignature::EMPTY,
        }
    }

    #[test]
    fn validate_pins_the_rule_version() {
        assert!(policy().validate(BLOCK_GAS_LIMIT).is_ok());
        for version in [0, 2, 99] {
            let bad = CalldataPolicy { version, ..policy() };
            assert_eq!(
                bad.validate(BLOCK_GAS_LIMIT),
                Err(FeeError::UnsupportedCalldataVersion(version))
            );
        }
    }

    #[test]
    fn validate_refuses_a_zero_rate() {
        let bad = CalldataPolicy { gas_per_byte: 0, ..policy() };
        assert_eq!(bad.validate(BLOCK_GAS_LIMIT), Err(FeeError::ZeroCalldataRate));
    }

    #[test]
    fn validate_bounds_the_rate_by_the_block_limit() {
        // The agreed flat rate: a maximum-size transaction costs ~7% of a block.
        assert_eq!(16 * MAX_TX_DATA_BYTES as u64, 2_097_152);
        assert!(policy().validate(2_097_152).is_ok(), "boundary equality is includable");
        assert!(policy().validate(2_097_151).is_err());

        // The largest includable rate at the default limit, and one past it.
        let max_rate = BLOCK_GAS_LIMIT / MAX_TX_DATA_BYTES as u64;
        let at_bound = CalldataPolicy { gas_per_byte: max_rate, ..policy() };
        assert!(at_bound.validate(BLOCK_GAS_LIMIT).is_ok());
        let past_bound = CalldataPolicy { gas_per_byte: max_rate + 1, ..policy() };
        assert_eq!(
            past_bound.validate(BLOCK_GAS_LIMIT),
            Err(FeeError::CalldataRateTooHigh {
                gas_per_byte: max_rate + 1,
                block_gas_limit: BLOCK_GAS_LIMIT
            })
        );

        // The product is checked, never wrapped.
        let overflowing = CalldataPolicy { gas_per_byte: u64::MAX, ..policy() };
        assert_eq!(overflowing.validate(BLOCK_GAS_LIMIT), Err(FeeError::Overflow));
    }

    #[test]
    fn check_floor_enforces_the_signed_declaration() {
        let policy = policy();
        let required = 16 * MAX_TX_DATA_BYTES as u64;
        assert!(policy.check_floor(&tx(MAX_TX_DATA_BYTES, required)).is_ok());
        assert!(policy.check_floor(&tx(MAX_TX_DATA_BYTES, required + 1)).is_ok());
        assert_eq!(
            policy.check_floor(&tx(MAX_TX_DATA_BYTES, required - 1)),
            Err(FeeError::UnderIntrinsic { gas_limit: required - 1, required })
        );
        // The cheap-bytes attack from the audit: 128 KiB at gas_limit 1.
        assert_eq!(
            policy.check_floor(&tx(MAX_TX_DATA_BYTES, 1)),
            Err(FeeError::UnderIntrinsic { gas_limit: 1, required })
        );
        // Empty calldata has no floor; gas-limit validity is a separate rule.
        assert!(policy.check_floor(&tx(0, 0)).is_ok());
    }

    #[test]
    fn check_floor_refuses_oversized_calldata_even_when_the_gas_floor_is_met() {
        let policy = policy();
        // The protocol maximum itself is valid at exactly its floor...
        assert!(policy.check_floor(&tx(MAX_TX_DATA_BYTES, 16 * MAX_TX_DATA_BYTES as u64)).is_ok());
        // ...but one byte past it is invalid even with the gas to pay for it:
        // the length rule is consensus validity, not the local mempool cap.
        let oversized = MAX_TX_DATA_BYTES + 1;
        assert_eq!(
            policy.check_floor(&tx(oversized, 16 * oversized as u64)),
            Err(FeeError::CalldataTooLarge { len: oversized, max: MAX_TX_DATA_BYTES })
        );
    }

    #[test]
    fn check_floor_overflow_is_invalid_never_wrapped() {
        // validate() refuses this rate at bind time; the pure check must still
        // fail closed if it is ever handed one.
        let bad = CalldataPolicy { gas_per_byte: u64::MAX, ..policy() };
        assert_eq!(bad.check_floor(&tx(2, Gas::MAX)), Err(FeeError::Overflow));
    }

    #[test]
    fn the_floor_reads_the_signed_data() {
        let policy = policy();
        let mut tx = tx(64, 16 * 64);
        assert!(policy.check_floor(&tx).is_ok());
        // One more byte breaks the floor...
        tx.data.push(0);
        assert!(policy.check_floor(&tx).is_err());
        // ...and the signature commits to the priced bytes, so they cannot be
        // shrunk post-admission without invalidating the sender binding.
        let payload = tx.signing_payload();
        tx.data.pop();
        assert_ne!(tx.signing_payload(), payload, "data is part of the signed payload");
    }

    #[test]
    fn policy_json_round_trips_and_denies_unknown_fields() {
        let json = serde_json::to_string(&policy()).unwrap();
        assert_eq!(serde_json::from_str::<CalldataPolicy>(&json).unwrap(), policy());
        let stray = json.replace('}', ",\"zero_byte_rate\":4}");
        assert!(serde_json::from_str::<CalldataPolicy>(&stray).is_err());
        assert!(serde_json::from_str::<CalldataPolicy>("{\"gas_per_byte\":16}").is_err());
    }
}
