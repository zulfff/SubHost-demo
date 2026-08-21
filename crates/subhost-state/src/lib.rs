use serde::{Deserialize, Serialize};
use subhost_core::{Address, Amount, ChainId, Nonce, Transaction, TransactionType};
use std::collections::HashMap;

/// An account's mutable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub nonce: Nonce,
    pub balance: Amount,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: 0,
        }
    }
}

impl Account {
    pub fn new(balance: Amount) -> Self {
        Self { nonce: 0, balance }
    }
}

/// World state as a simple address -> account map.
///
/// Previously this crate was an empty template that kept a requests counter and
/// never held any state, so RPC `eth_getBalance` could only return a fabricated
/// `0x0`. This is a real (if not yet disk-persisted) state store.
#[derive(Debug)]
pub struct State {
    accounts: HashMap<Address, Account>,
    chain_id: ChainId,
}

impl State {
    pub fn new() -> Self {
        Self::with_chain_id(1)
    }

    pub fn with_chain_id(chain_id: ChainId) -> Self {
        Self {
            accounts: HashMap::new(),
            chain_id,
        }
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    /// Seed an account with an initial balance (used at genesis / by the faucet).
    pub fn add_account(&mut self, address: Address, balance: Amount) -> Result<(), StateError> {
        let entry = self.accounts.entry(address).or_default();
        entry.balance = entry
            .balance
            .checked_add(balance)
            .ok_or(StateError::BalanceOverflow { address })?;
        Ok(())
    }

    pub fn set_balance(&mut self, address: Address, balance: Amount) {
        self.accounts.entry(address).or_default().balance = balance;
    }

    pub fn balance(&self, address: &Address) -> Amount {
        self.accounts.get(address).map(|a| a.balance).unwrap_or(0)
    }

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.accounts.get(address)
    }

    pub fn nonce(&self, address: &Address) -> Nonce {
        self.accounts.get(address).map(|a| a.nonce).unwrap_or(0)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Fund `to` debiting `from`. Returns an error if `from` has insufficient funds.
    pub fn apply_transfer(&mut self, from: Address, to: Address, value: Amount) -> Result<(), StateError> {
        let from_balance = self.balance(&from);
        if from_balance < value {
            return Err(StateError::InsufficientBalance { address: from, have: from_balance, want: value });
        }
        if from == to {
            return Ok(());
        }
        let to_balance = self.balance(&to);
        let credited = to_balance.checked_add(value).ok_or(StateError::BalanceOverflow { address: to })?;
        self.accounts.entry(from).or_default().balance = from_balance - value;
        self.accounts.entry(to).or_default().balance = credited;
        Ok(())
    }

    /// Execute a single core transaction. Currently only `Transfer` is fully
    /// interpreted; other tx types are explicitly unsupported (not silently
    /// coerced) until their execution engines exist.
    pub fn apply_transaction(&mut self, tx: &Transaction) -> Result<(), StateError> {
        if tx.chain_id != self.chain_id {
            return Err(StateError::InvalidTransaction(format!(
                "transaction chain ID {} does not match state chain ID {}",
                tx.chain_id, self.chain_id
            )));
        }

        // Reject stale / reordered nonces.
        let expected_nonce = self.nonce(&tx.from);
        if tx.nonce != expected_nonce {
            return Err(StateError::InvalidNonce {
                address: tx.from,
                expected: expected_nonce,
                got: tx.nonce,
            });
        }
        let next_nonce = tx
            .nonce
            .checked_add(1)
            .ok_or_else(|| StateError::InvalidTransaction("transaction nonce overflow".to_string()))?;

        let fee = tx
            .gas_price
            .checked_mul(tx.gas_limit as u128)
            .ok_or_else(|| StateError::InvalidTransaction("transaction fee overflows balance type".to_string()))?;

        match tx.tx_type {
            TransactionType::Transfer => {
                let to = tx.to.ok_or(StateError::InvalidTransaction(
                    "transfer requires a destination".to_string(),
                ))?;
                let total = tx
                    .value
                    .checked_add(fee)
                    .ok_or_else(|| StateError::InvalidTransaction("transaction cost overflows balance type".to_string()))?;
                self.ensure_balance(&tx.from, total)?;
                self.apply_transfer(tx.from, to, tx.value)?;
                self.debit_balance(tx.from, fee)?;
            }
            TransactionType::ContractCall | TransactionType::ContractCreation => {
                // Require enough balance for the gas fee only; bytecode execution is
                // handled by the EVM/WASM engines (not wired here yet).
                self.ensure_balance(&tx.from, fee)?;
                self.debit_balance(tx.from, fee)?;
            }
            _ => {
                return Err(StateError::UnsupportedTransactionType(tx.tx_type));
            }
        }

        // Bump the sender nonce so the tx cannot be replayed.
        self.accounts.entry(tx.from).or_default().nonce = next_nonce;
        Ok(())
    }

    fn ensure_balance(&self, address: &Address, amount: Amount) -> Result<(), StateError> {
        let have = self.balance(address);
        if have < amount {
            return Err(StateError::InsufficientBalance { address: *address, have, want: amount });
        }
        Ok(())
    }

    fn debit_balance(&mut self, address: Address, amount: Amount) -> Result<(), StateError> {
        let balance = self.balance(&address);
        if balance < amount {
            return Err(StateError::InsufficientBalance { address, have: balance, want: amount });
        }
        self.accounts.entry(address).or_default().balance = balance - amount;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("insufficient balance for {address}: have {have}, want {want}")]
    InsufficientBalance { address: Address, have: Amount, want: Amount },

    #[error("invalid nonce for {address}: expected {expected}, got {got}")]
    InvalidNonce { address: Address, expected: Nonce, got: Nonce },

    #[error("invalid transaction: {0}")]
    InvalidTransaction(String),

    #[error("transaction type {0:?} is not supported by this state backend")]
    UnsupportedTransactionType(TransactionType),

    #[error("balance overflow for {address}")]
    BalanceOverflow { address: Address },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn balance_and_seeding() {
        let mut state = State::new();
        state.add_account(addr(1), 1000).unwrap();
        assert_eq!(state.balance(&addr(1)), 1000);
        assert_eq!(state.balance(&addr(2)), 0);
    }

    #[test]
    fn transfer_moves_funds() {
        let mut state = State::new();
        state.add_account(addr(1), 1000).unwrap();
        state.add_account(addr(2), 0).unwrap();
        state.apply_transfer(addr(1), addr(2), 300).unwrap();
        assert_eq!(state.balance(&addr(1)), 700);
        assert_eq!(state.balance(&addr(2)), 300);
    }

    #[test]
    fn transfer_rejects_overdraft() {
        let mut state = State::new();
        state.add_account(addr(1), 10).unwrap();
        assert!(state.apply_transfer(addr(1), addr(2), 11).is_err());
    }

    #[test]
    fn transaction_enforces_nonce_and_bumps_it() {
        let mut state = State::new();
        // Enough to cover value (100) + fee (gas_price 1 * gas_limit 21_000).
        state.add_account(addr(1), 1_000_000).unwrap();

        let tx = Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from: addr(1),
            to: Some(addr(2)),
            value: 100,
            gas_price: 1,
            gas_limit: 21_000,
            data: vec![],
            chain_id: 1,
            signature: subhost_core::TransactionSignature { r: [0; 32], s: [0; 32], v: 27 },
        };
        state.apply_transaction(&tx).unwrap();
        assert_eq!(state.balance(&addr(2)), 100);
        assert_eq!(state.balance(&addr(1)), 978_900);
        assert_eq!(state.nonce(&addr(1)), 1);

        // Replay with the same nonce must fail (nonce bumped to 1).
        assert!(state.apply_transaction(&tx).is_err());
    }

    #[test]
    fn transfer_rejects_recipient_balance_overflow() {
        let mut state = State::new();
        state.add_account(addr(1), 10).unwrap();
        state.set_balance(addr(2), Amount::MAX);
        assert!(matches!(
            state.apply_transfer(addr(1), addr(2), 1),
            Err(StateError::BalanceOverflow { address }) if address == addr(2)
        ));
        assert_eq!(state.balance(&addr(1)), 10);
    }

    #[test]
    fn max_nonce_transaction_is_rejected_without_replay_window() {
        let mut state = State::new();
        state.add_account(addr(1), 1_000).unwrap();
        state.accounts.get_mut(&addr(1)).unwrap().nonce = Nonce::MAX;
        let tx = Transaction {
            tx_type: TransactionType::ContractCall,
            nonce: Nonce::MAX,
            from: addr(1),
            to: Some(addr(2)),
            value: 0,
            gas_price: 1,
            gas_limit: 1,
            data: vec![],
            chain_id: 1,
            signature: subhost_core::TransactionSignature { r: [0; 32], s: [0; 32], v: 27 },
        };
        assert!(matches!(
            state.apply_transaction(&tx),
            Err(StateError::InvalidTransaction(message)) if message.contains("nonce overflow")
        ));
        assert_eq!(state.nonce(&addr(1)), Nonce::MAX);
        assert_eq!(state.balance(&addr(1)), 1_000);
    }

    #[test]
    fn rejects_foreign_chain_transaction() {
        let mut state = State::with_chain_id(7);
        state.add_account(addr(1), 1_000_000).unwrap();
        let tx = Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from: addr(1),
            to: Some(addr(2)),
            value: 1,
            gas_price: 1,
            gas_limit: 21_000,
            data: vec![],
            chain_id: 1,
            signature: subhost_core::TransactionSignature { r: [0; 32], s: [0; 32], v: 0 },
        };
        assert!(matches!(
            state.apply_transaction(&tx),
            Err(StateError::InvalidTransaction(message)) if message.contains("chain ID")
        ));
    }

    #[test]
    fn rejects_account_balance_overflow() {
        let mut state = State::new();
        state.set_balance(addr(1), Amount::MAX);
        assert!(matches!(
            state.add_account(addr(1), 1),
            Err(StateError::BalanceOverflow { address }) if address == addr(1)
        ));
    }
}
