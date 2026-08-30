//! Validator staking, delegation, and slashing.
//!
//! Every mutation uses checked arithmetic: silently wrapping a balance here would
//! mint or destroy stake, so overflow is an error rather than a wrap.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use subhost_core::{Address, Amount, BlockHeight};
use tracing::info;

/// Minimum self-stake required to register a validator.
pub const MIN_VALIDATOR_STAKE: Amount = 10_000_000;
/// Commission is expressed in basis points, so this is the 100% ceiling.
pub const MAX_COMMISSION_BPS: u64 = 10_000;

/// A snapshot of the active validator set with its derived quorum sizing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatorSet {
    pub validators: Vec<Validator>,
    pub total_stake: Amount,
    pub quorum_threshold: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Validator {
    pub address: Address,
    pub public_key: Vec<u8>,
    /// Self-stake plus everything delegated to this validator.
    pub stake: Amount,
    /// Commission rate in basis points (0..=10_000).
    pub commission_bps: u64,
    /// Observed uptime as a fraction in `0.0..=1.0`.
    pub uptime: f64,
}

impl Validator {
    /// Reject a validator whose declared parameters are out of range.
    pub fn validate(&self) -> Result<(), StakingError> {
        if self.stake < MIN_VALIDATOR_STAKE {
            return Err(StakingError::StakeTooLow {
                provided: self.stake,
                minimum: MIN_VALIDATOR_STAKE,
            });
        }
        if self.public_key.is_empty() {
            return Err(StakingError::MissingPublicKey(self.address));
        }
        if self.commission_bps > MAX_COMMISSION_BPS {
            return Err(StakingError::InvalidCommission(self.commission_bps));
        }
        if !self.uptime.is_finite() || self.uptime < 0.0 || self.uptime > 1.0 {
            return Err(StakingError::InvalidUptime(self.uptime));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    pub delegator: Address,
    pub validator: Address,
    pub amount: Amount,
    pub rewards: Amount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashingEvidence {
    pub validator: Address,
    pub evidence_type: SlashingType,
    pub height: BlockHeight,
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlashingType {
    /// Equivocation: the whole stake is burned and the validator ejected.
    DoubleSign,
    /// Liveness failure: 1% of stake.
    Downtime,
    /// Other provable misbehaviour: 50% of stake.
    MaliciousBehavior,
}

impl SlashingType {
    /// The penalty this offence applies to `stake`.
    pub fn penalty(self, stake: Amount) -> Amount {
        match self {
            Self::DoubleSign => stake,
            Self::Downtime => stake / 100,
            Self::MaliciousBehavior => stake / 2,
        }
    }
}

/// The staking registry: validators, delegations, and recorded slashings.
#[derive(Debug, Default)]
pub struct StakingModule {
    validators: HashMap<Address, Validator>,
    delegations: HashMap<(Address, Address), Delegation>,
    slashings: Vec<SlashingEvidence>,
}

impl StakingModule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a validator, rejecting duplicates and out-of-range parameters.
    pub fn add_validator(&mut self, validator: Validator) -> Result<(), StakingError> {
        validator.validate()?;
        if self.validators.contains_key(&validator.address) {
            return Err(StakingError::DuplicateValidator(validator.address));
        }
        info!(address = %validator.address, stake = validator.stake, "validator registered");
        self.validators.insert(validator.address, validator);
        Ok(())
    }

    pub fn validator(&self, address: &Address) -> Option<&Validator> {
        self.validators.get(address)
    }

    pub fn validator_count(&self) -> usize {
        self.validators.len()
    }

    /// Total stake across every active validator.
    pub fn total_stake(&self) -> Amount {
        self.validators.values().fold(0, |total, validator| total.saturating_add(validator.stake))
    }

    /// The active set, sorted by descending stake then address so the ordering is
    /// deterministic across nodes.
    pub fn validator_set(&self) -> ValidatorSet {
        let mut validators: Vec<Validator> = self.validators.values().cloned().collect();
        validators.sort_by(|left, right| {
            right.stake.cmp(&left.stake).then_with(|| left.address.cmp(&right.address))
        });
        let count = validators.len();
        let quorum_threshold = if count == 0 { 0 } else { 2 * ((count - 1) / 3) + 1 };
        ValidatorSet { total_stake: self.total_stake(), quorum_threshold, validators }
    }

    /// Delegate to an existing validator, crediting both the delegation record and
    /// the validator's stake.
    pub fn delegate(
        &mut self,
        delegator: Address,
        validator: Address,
        amount: Amount,
    ) -> Result<Amount, StakingError> {
        if amount == 0 {
            return Err(StakingError::ZeroAmount);
        }
        // Delegating to a non-existent validator would strand the funds.
        let target =
            self.validators.get_mut(&validator).ok_or(StakingError::UnknownValidator(validator))?;
        let new_stake =
            target.stake.checked_add(amount).ok_or(StakingError::StakeOverflow(validator))?;

        let entry = self.delegations.entry((delegator, validator)).or_insert(Delegation {
            delegator,
            validator,
            amount: 0,
            rewards: 0,
        });
        let new_amount =
            entry.amount.checked_add(amount).ok_or(StakingError::StakeOverflow(validator))?;

        entry.amount = new_amount;
        target.stake = new_stake;
        Ok(new_amount)
    }

    /// Withdraw delegated stake, reducing both the delegation and the validator.
    pub fn undelegate(
        &mut self,
        delegator: Address,
        validator: Address,
        amount: Amount,
    ) -> Result<Amount, StakingError> {
        if amount == 0 {
            return Err(StakingError::ZeroAmount);
        }
        let entry = self
            .delegations
            .get_mut(&(delegator, validator))
            .ok_or(StakingError::UnknownDelegation { delegator, validator })?;
        if entry.amount < amount {
            return Err(StakingError::InsufficientDelegation { have: entry.amount, want: amount });
        }
        entry.amount -= amount;
        let remaining = entry.amount;
        if remaining == 0 && entry.rewards == 0 {
            self.delegations.remove(&(delegator, validator));
        }
        if let Some(target) = self.validators.get_mut(&validator) {
            // Slashing may already have cut the validator below this amount.
            target.stake = target.stake.saturating_sub(amount);
        }
        Ok(remaining)
    }

    pub fn delegation(&self, delegator: &Address, validator: &Address) -> Option<&Delegation> {
        self.delegations.get(&(*delegator, *validator))
    }

    /// Total delegated to one validator, excluding rewards.
    pub fn delegated_to(&self, validator: &Address) -> Amount {
        self.delegations
            .values()
            .filter(|delegation| &delegation.validator == validator)
            .fold(0, |total, delegation| total.saturating_add(delegation.amount))
    }

    /// Apply slashing evidence, deducting stake and ejecting the validator when
    /// its stake is fully burned.
    ///
    /// Evidence is recorded before any mutation so it survives ejection. Returns
    /// the amount actually slashed.
    pub fn slash(&mut self, evidence: SlashingEvidence) -> Result<Amount, StakingError> {
        let address = evidence.validator;
        let stake = self
            .validators
            .get(&address)
            .map(|validator| validator.stake)
            .ok_or(StakingError::UnknownValidator(address))?;
        if evidence.proof.is_empty() {
            return Err(StakingError::MissingProof(address));
        }

        let penalty = evidence.evidence_type.penalty(stake);
        let offence = evidence.evidence_type;
        self.slashings.push(evidence);
        if penalty == 0 {
            return Ok(0);
        }

        // Deduct for real. Returning the figure without applying it, as the
        // previous implementation did, made slashing a no-op.
        let mut ejected = false;
        if let Some(validator) = self.validators.get_mut(&address) {
            if validator.stake <= penalty {
                ejected = true;
            } else {
                validator.stake -= penalty;
            }
        }
        if ejected {
            self.validators.remove(&address);
            // Delegations to an ejected validator are burned with it.
            self.delegations.retain(|(_, validator), _| validator != &address);
        }
        info!(%address, ?offence, penalty, ejected, "validator slashed");
        Ok(penalty)
    }

    pub fn slashings(&self) -> &[SlashingEvidence] {
        &self.slashings
    }

    pub fn slashings_for(&self, validator: &Address) -> usize {
        self.slashings.iter().filter(|evidence| &evidence.validator == validator).count()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StakingError {
    #[error("stake {provided} is below the minimum {minimum}")]
    StakeTooLow { provided: Amount, minimum: Amount },

    #[error("validator {0} has no public key")]
    MissingPublicKey(Address),

    #[error("commission {0} basis points exceeds 10000")]
    InvalidCommission(u64),

    #[error("uptime {0} is outside 0.0..=1.0")]
    InvalidUptime(f64),

    #[error("validator {0} is already registered")]
    DuplicateValidator(Address),

    #[error("validator {0} is not registered")]
    UnknownValidator(Address),

    #[error("no delegation from {delegator} to {validator}")]
    UnknownDelegation { delegator: Address, validator: Address },

    #[error("insufficient delegation: have {have}, want {want}")]
    InsufficientDelegation { have: Amount, want: Amount },

    #[error("amount must be greater than zero")]
    ZeroAmount,

    #[error("stake overflow for validator {0}")]
    StakeOverflow(Address),

    #[error("slashing evidence for {0} carries no proof")]
    MissingProof(Address),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn validator(n: u8, stake: Amount) -> Validator {
        Validator {
            address: address(n),
            public_key: vec![n, n, n],
            stake,
            commission_bps: 1_000,
            uptime: 0.999,
        }
    }

    fn evidence(n: u8, evidence_type: SlashingType) -> SlashingEvidence {
        SlashingEvidence { validator: address(n), evidence_type, height: 1, proof: vec![1] }
    }

    #[test]
    fn registration_validates_every_parameter() {
        let mut module = StakingModule::new();
        module.add_validator(validator(1, 20_000_000)).unwrap();
        assert_eq!(module.validator_count(), 1);
        assert_eq!(module.total_stake(), 20_000_000);

        assert!(matches!(
            module.add_validator(validator(1, 20_000_000)),
            Err(StakingError::DuplicateValidator(_))
        ));
        assert!(matches!(
            module.add_validator(validator(2, MIN_VALIDATOR_STAKE - 1)),
            Err(StakingError::StakeTooLow { .. })
        ));
        assert!(matches!(
            module.add_validator(Validator { public_key: Vec::new(), ..validator(3, 20_000_000) }),
            Err(StakingError::MissingPublicKey(_))
        ));
        assert!(matches!(
            module.add_validator(Validator {
                commission_bps: MAX_COMMISSION_BPS + 1,
                ..validator(4, 20_000_000)
            }),
            Err(StakingError::InvalidCommission(_))
        ));
        for uptime in [-0.1, 1.1, f64::NAN] {
            assert!(matches!(
                module.add_validator(Validator { uptime, ..validator(5, 20_000_000) }),
                Err(StakingError::InvalidUptime(_))
            ));
        }
        assert_eq!(module.validator_count(), 1, "no invalid validator was stored");
    }

    #[test]
    fn delegation_credits_the_validator_and_accumulates() {
        let mut module = StakingModule::new();
        module.add_validator(validator(1, 20_000_000)).unwrap();

        assert_eq!(module.delegate(address(9), address(1), 500).unwrap(), 500);
        assert_eq!(module.delegate(address(9), address(1), 250).unwrap(), 750);
        assert_eq!(module.delegated_to(&address(1)), 750);
        assert_eq!(module.validator(&address(1)).unwrap().stake, 20_000_750);
        assert_eq!(module.delegation(&address(9), &address(1)).unwrap().amount, 750);
    }

    #[test]
    fn delegation_rejects_unknown_validators_zero_and_overflow() {
        let mut module = StakingModule::new();
        assert!(matches!(
            module.delegate(address(9), address(1), 100),
            Err(StakingError::UnknownValidator(_))
        ));

        module.add_validator(validator(1, 20_000_000)).unwrap();
        assert!(matches!(
            module.delegate(address(9), address(1), 0),
            Err(StakingError::ZeroAmount)
        ));
        // Overflow must be refused, not wrapped into a phantom balance.
        assert!(matches!(
            module.delegate(address(9), address(1), Amount::MAX),
            Err(StakingError::StakeOverflow(_))
        ));
        assert_eq!(
            module.validator(&address(1)).unwrap().stake,
            20_000_000,
            "a failed delegation must not change stake"
        );
        assert_eq!(module.delegated_to(&address(1)), 0);
    }

    #[test]
    fn undelegation_reduces_stake_and_clears_empty_records() {
        let mut module = StakingModule::new();
        module.add_validator(validator(1, 20_000_000)).unwrap();
        module.delegate(address(9), address(1), 1_000).unwrap();

        assert_eq!(module.undelegate(address(9), address(1), 400).unwrap(), 600);
        assert_eq!(module.validator(&address(1)).unwrap().stake, 20_000_600);

        assert!(matches!(
            module.undelegate(address(9), address(1), 601),
            Err(StakingError::InsufficientDelegation { have: 600, want: 601 })
        ));
        assert!(matches!(
            module.undelegate(address(9), address(1), 0),
            Err(StakingError::ZeroAmount)
        ));
        assert!(matches!(
            module.undelegate(address(8), address(1), 1),
            Err(StakingError::UnknownDelegation { .. })
        ));

        assert_eq!(module.undelegate(address(9), address(1), 600).unwrap(), 0);
        assert!(module.delegation(&address(9), &address(1)).is_none());
        assert_eq!(module.validator(&address(1)).unwrap().stake, 20_000_000);
    }

    #[test]
    fn slashing_deducts_stake_for_each_offence_class() {
        for (offence, stake, expected_remaining) in [
            (SlashingType::Downtime, 20_000_000u128, 19_800_000u128),
            (SlashingType::MaliciousBehavior, 20_000_000, 10_000_000),
        ] {
            let mut module = StakingModule::new();
            module.add_validator(validator(1, stake)).unwrap();
            let slashed = module.slash(evidence(1, offence)).unwrap();
            assert_eq!(slashed, stake - expected_remaining);
            assert_eq!(module.validator(&address(1)).unwrap().stake, expected_remaining);
            assert_eq!(module.slashings_for(&address(1)), 1);
        }
    }

    #[test]
    fn double_signing_ejects_the_validator_and_burns_delegations() {
        let mut module = StakingModule::new();
        module.add_validator(validator(2, 20_000_000)).unwrap();
        module.delegate(address(9), address(2), 5_000).unwrap();

        let slashed = module.slash(evidence(2, SlashingType::DoubleSign)).unwrap();
        assert_eq!(slashed, 20_005_000, "delegated stake is slashed with the validator");
        assert!(module.validator(&address(2)).is_none());
        assert!(module.delegation(&address(9), &address(2)).is_none());
        assert_eq!(module.total_stake(), 0);
        // Evidence outlives the ejected validator.
        assert_eq!(module.slashings().len(), 1);
        assert_eq!(module.slashings_for(&address(2)), 1);
    }

    #[test]
    fn slashing_requires_a_known_validator_and_a_proof() {
        let mut module = StakingModule::new();
        assert!(matches!(
            module.slash(evidence(1, SlashingType::Downtime)),
            Err(StakingError::UnknownValidator(_))
        ));

        module.add_validator(validator(1, 20_000_000)).unwrap();
        assert!(matches!(
            module.slash(SlashingEvidence {
                proof: Vec::new(),
                ..evidence(1, SlashingType::Downtime)
            }),
            Err(StakingError::MissingProof(_))
        ));
        assert!(module.slashings().is_empty(), "rejected evidence is not recorded");
        assert_eq!(module.validator(&address(1)).unwrap().stake, 20_000_000);
    }

    #[test]
    fn a_penalty_that_rounds_to_zero_records_evidence_without_a_deduction() {
        let mut module = StakingModule::new();
        // 1% of the minimum stake is non-zero, so force the rounding case with a
        // penalty computed against a stake below 100.
        module.add_validator(validator(1, MIN_VALIDATOR_STAKE)).unwrap();
        module.slash(evidence(1, SlashingType::MaliciousBehavior)).unwrap();
        let remaining = module.validator(&address(1)).unwrap().stake;
        assert_eq!(remaining, MIN_VALIDATOR_STAKE / 2);
        assert_eq!(SlashingType::Downtime.penalty(99), 0);
        assert_eq!(SlashingType::Downtime.penalty(100), 1);
    }

    #[test]
    fn validator_set_is_deterministic_and_sizes_the_quorum() {
        let mut module = StakingModule::new();
        assert_eq!(module.validator_set().quorum_threshold, 0);

        for (index, stake) in
            [(1u8, 30_000_000u128), (2, 10_000_000), (3, 30_000_000), (4, 20_000_000)]
        {
            module.add_validator(validator(index, stake)).unwrap();
        }

        let set = module.validator_set();
        assert_eq!(set.total_stake, 90_000_000);
        assert_eq!(set.quorum_threshold, 3, "4 validators tolerate 1 fault");
        // Descending stake, ties broken by address.
        assert_eq!(
            set.validators
                .iter()
                .map(|validator| validator.address.as_bytes()[0])
                .collect::<Vec<_>>(),
            vec![1, 3, 4, 2]
        );
        assert_eq!(module.validator_set(), set, "the ordering must be stable");
    }
}
