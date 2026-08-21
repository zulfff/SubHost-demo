use subhost_core::{Address, BlockHeight, Amount};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorSet {
    pub validators: Vec<Validator>,
    pub total_stake: Amount,
    pub quorum_threshold: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Validator {
    pub address: Address,
    pub public_key: Vec<u8>,
    pub stake: Amount,
    pub commission: u64,
    pub uptime: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    pub delegator: Address,
    pub validator: Address,
    pub amount: Amount,
    pub rewards: Amount,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashingEvidence {
    pub validator: Address,
    pub evidence_type: SlashingType,
    pub height: BlockHeight,
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SlashingType {
    DoubleSign,
    Downtime,
    MaliciousBehavior,
}

pub struct StakingModule {
    validators: HashMap<Address, Validator>,
    delegations: HashMap<(Address, Address), Delegation>,
    slashings: Vec<SlashingEvidence>,
}

impl StakingModule {
    pub fn new() -> Self {
        Self {
            validators: HashMap::new(),
            delegations: HashMap::new(),
            slashings: Vec::new(),
        }
    }
    
    pub fn add_validator(&mut self, validator: Validator) -> Result<(), String> {
        if validator.stake < 10_000_000 {
            return Err("Minimum stake is 10M SUB".to_string());
        }
        self.validators.insert(validator.address, validator);
        Ok(())
    }
    
    pub fn delegate(&mut self, delegator: Address, validator: Address, amount: Amount) -> Result<(), String> {
        let key = (delegator, validator);
        let delegation = self.delegations.entry(key).or_insert(Delegation {
            delegator,
            validator,
            amount: 0,
            rewards: 0,
        });
        delegation.amount += amount;
        Ok(())
    }
    
    pub fn slash(&mut self, evidence: SlashingEvidence) -> Amount {
        let addr = evidence.validator;
        // Compute the intended slash without keeping a borrow across mutation.
        let slash_amount = self
            .validators
            .get(&addr)
            .map(|v| match evidence.evidence_type {
                SlashingType::DoubleSign => v.stake,
                SlashingType::Downtime => v.stake / 100,
                SlashingType::MaliciousBehavior => v.stake / 2,
            })
            .unwrap_or(0);

        if slash_amount == 0 {
            return 0;
        }

        // Record the evidence FIRST (before any potential removal) so it is never lost.
        self.slashings.push(evidence);

        // Apply the penalty for real: deduct stake, removing the validator entirely
        // if its remaining stake is burned away. Previously the amount was merely
        // returned and never deducted, so slashing had no effect at all.
        if let Some(v) = self.validators.get_mut(&addr) {
            if v.stake <= slash_amount {
                self.validators.remove(&addr);
            } else {
                v.stake -= slash_amount;
            }
        }

        slash_amount
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_validator_stake() {
        let mut module = StakingModule::new();
        let validator = Validator {
            address: Address::new([1u8; 20]),
            public_key: vec![1, 2, 3],
            stake: 20_000_000,
            commission: 10,
            uptime: 99.9,
        };
        assert!(module.add_validator(validator).is_ok());
    }

    #[test]
    fn test_slash_deducts_stake() {
        let mut module = StakingModule::new();
        let validator = Validator {
            address: Address::new([1u8; 20]),
            public_key: vec![1, 2, 3],
            stake: 20_000_000,
            commission: 10,
            uptime: 99.9,
        };
        module.add_validator(validator).unwrap();

        // Malicious behavior slashes half the stake.
        let slashed = module.slash(SlashingEvidence {
            validator: Address::new([1u8; 20]),
            evidence_type: SlashingType::MaliciousBehavior,
            height: 1,
            proof: vec![1],
        });
        assert_eq!(slashed, 10_000_000);
        assert_eq!(module.validators[&Address::new([1u8; 20])].stake, 10_000_000);
    }

    #[test]
    fn test_full_slash_removes_validator() {
        let mut module = StakingModule::new();
        module.add_validator(Validator {
            address: Address::new([2u8; 20]),
            public_key: vec![1],
            stake: 20_000_000,
            commission: 10,
            uptime: 99.9,
        }).unwrap();

        // Double-signing slashes the ENTIRE stake -> validator removal.
        let slashed = module.slash(SlashingEvidence {
            validator: Address::new([2u8; 20]),
            evidence_type: SlashingType::DoubleSign,
            height: 1,
            proof: vec![1],
        });
        assert_eq!(slashed, 20_000_000);
        assert!(module.validators.get(&Address::new([2u8; 20])).is_none());
    }
}
