//! Account world state and the transaction execution rules applied to it.
//!
//! The store is in-memory and deterministic; durability is provided by
//! `subhost-storage`, which snapshots this state alongside the block history.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use subhost_core::{Address, Amount, ChainId, Nonce, Transaction, TransactionType};

/// An account's mutable state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub nonce: Nonce,
    pub balance: Amount,
}

impl Account {
    pub fn new(balance: Amount) -> Self {
        Self { nonce: 0, balance }
    }
}

/// World state as an address -> account map bound to a single chain ID.
#[derive(Debug, Clone)]
pub struct State {
    accounts: HashMap<Address, Account>,
    chain_id: ChainId,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// A fresh state on chain 1. Prefer [`Self::with_chain_id`] outside tests.
    pub fn new() -> Self {
        Self::with_chain_id(1)
    }

    pub fn with_chain_id(chain_id: ChainId) -> Self {
        Self { accounts: HashMap::new(), chain_id }
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    /// Credit an account, used at genesis and by the faucet.
    pub fn credit(&mut self, address: Address, amount: Amount) -> Result<(), StateError> {
        let entry = self.accounts.entry(address).or_default();
        entry.balance =
            entry.balance.checked_add(amount).ok_or(StateError::BalanceOverflow { address })?;
        Ok(())
    }

    /// Overwrite an account balance. Genesis allocation is the only caller that
    /// legitimately needs to *set* rather than credit.
    pub fn set_balance(&mut self, address: Address, balance: Amount) {
        self.accounts.entry(address).or_default().balance = balance;
    }

    pub fn balance(&self, address: &Address) -> Amount {
        self.accounts.get(address).map_or(0, |account| account.balance)
    }

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.accounts.get(address)
    }

    pub fn nonce(&self, address: &Address) -> Nonce {
        self.accounts.get(address).map_or(0, |account| account.nonce)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Deterministic, address-sorted snapshot suitable for hashing and storage.
    pub fn snapshot(&self) -> StateSnapshot {
        let mut accounts: Vec<_> =
            self.accounts.iter().map(|(address, account)| (*address, account.clone())).collect();
        accounts.sort_by(|(left, _), (right, _)| left.cmp(right));
        StateSnapshot { chain_id: self.chain_id, accounts }
    }

    /// Rebuild a state from a snapshot, rejecting a snapshot that cannot have
    /// been produced by [`Self::snapshot`].
    pub fn from_snapshot(snapshot: StateSnapshot) -> Result<Self, StateError> {
        if snapshot.chain_id == 0 {
            return Err(StateError::InvalidSnapshot("chain ID cannot be zero".into()));
        }
        let mut accounts = HashMap::with_capacity(snapshot.accounts.len());
        for (address, account) in snapshot.accounts {
            if accounts.insert(address, account).is_some() {
                return Err(StateError::InvalidSnapshot(
                    "snapshot contains duplicate accounts".into(),
                ));
            }
        }
        Ok(Self { accounts, chain_id: snapshot.chain_id })
    }

    /// Root commitment over the account set, used as the block `state_root`.
    pub fn root(&self) -> subhost_core::Hash {
        subhost_core::Hash::from_data(&subhost_core::encode_canonical(&self.snapshot()))
    }

    /// Move `value` from `from` to `to`, rejecting overdraft and credit overflow.
    pub fn apply_transfer(
        &mut self,
        from: Address,
        to: Address,
        value: Amount,
    ) -> Result<(), StateError> {
        let from_balance = self.balance(&from);
        if from_balance < value {
            return Err(StateError::InsufficientBalance {
                address: from,
                have: from_balance,
                want: value,
            });
        }
        if from == to {
            // A self-transfer is a no-op rather than a double credit.
            return Ok(());
        }
        let credited = self
            .balance(&to)
            .checked_add(value)
            .ok_or(StateError::BalanceOverflow { address: to })?;
        self.accounts.entry(from).or_default().balance = from_balance - value;
        self.accounts.entry(to).or_default().balance = credited;
        Ok(())
    }

    /// Execute one transaction against the state.
    ///
    /// Only [`TransactionType::Transfer`] is interpreted; every other type is
    /// rejected explicitly rather than silently treated as a transfer, because
    /// no execution engine for them exists yet.
    ///
    /// The caller is responsible for signature verification. This function
    /// enforces chain binding, nonce ordering, and balance sufficiency, and
    /// leaves the state untouched on any error.
    pub fn apply_transaction(&mut self, tx: &Transaction) -> Result<Amount, StateError> {
        if tx.chain_id != self.chain_id {
            return Err(StateError::ChainMismatch { expected: self.chain_id, got: tx.chain_id });
        }
        if tx.gas_limit == 0 {
            return Err(StateError::InvalidTransaction("gas limit must be > 0".into()));
        }

        // Reject stale, reordered, and replayed nonces.
        let expected_nonce = self.nonce(&tx.from);
        if tx.nonce != expected_nonce {
            return Err(StateError::InvalidNonce {
                address: tx.from,
                expected: expected_nonce,
                got: tx.nonce,
            });
        }
        // Refuse the terminal nonce rather than wrapping it into a replay window.
        let next_nonce = tx
            .nonce
            .checked_add(1)
            .ok_or_else(|| StateError::InvalidTransaction("transaction nonce overflow".into()))?;

        let fee = tx
            .fee()
            .ok_or_else(|| StateError::InvalidTransaction("transaction fee overflows".into()))?;

        match tx.tx_type {
            TransactionType::Transfer => {
                let to = tx.to.ok_or_else(|| {
                    StateError::InvalidTransaction("transfer requires a destination".into())
                })?;
                let total = tx.total_cost().ok_or_else(|| {
                    StateError::InvalidTransaction("transaction cost overflows".into())
                })?;
                // Check value + fee up front so a transfer can never succeed and
                // then leave the fee unpayable.
                self.ensure_balance(&tx.from, total)?;
                self.apply_transfer(tx.from, to, tx.value)?;
                self.debit(tx.from, fee)?;
            }
            other => return Err(StateError::UnsupportedTransactionType(other)),
        }

        self.accounts.entry(tx.from).or_default().nonce = next_nonce;
        Ok(fee)
    }

    fn ensure_balance(&self, address: &Address, amount: Amount) -> Result<(), StateError> {
        let have = self.balance(address);
        if have < amount {
            return Err(StateError::InsufficientBalance { address: *address, have, want: amount });
        }
        Ok(())
    }

    fn debit(&mut self, address: Address, amount: Amount) -> Result<(), StateError> {
        let balance = self.balance(&address);
        if balance < amount {
            return Err(StateError::InsufficientBalance { address, have: balance, want: amount });
        }
        self.accounts.entry(address).or_default().balance = balance - amount;
        Ok(())
    }
}

/// A deterministic, sorted view of the whole account set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub chain_id: ChainId,
    pub accounts: Vec<(Address, Account)>,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("insufficient balance for {address}: have {have}, want {want}")]
    InsufficientBalance { address: Address, have: Amount, want: Amount },

    #[error("invalid nonce for {address}: expected {expected}, got {got}")]
    InvalidNonce { address: Address, expected: Nonce, got: Nonce },

    #[error("transaction chain ID {got} does not match state chain ID {expected}")]
    ChainMismatch { expected: ChainId, got: ChainId },

    #[error("invalid transaction: {0}")]
    InvalidTransaction(String),

    #[error("invalid state snapshot: {0}")]
    InvalidSnapshot(String),

    #[error("transaction type {0} is not supported by this state backend")]
    UnsupportedTransactionType(TransactionType),

    #[error("balance overflow for {address}")]
    BalanceOverflow { address: Address },
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_core::TransactionSignature;

    fn addr(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn transfer(nonce: Nonce, value: Amount) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: addr(1),
            to: Some(addr(2)),
            value,
            gas_price: 1,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature::EMPTY,
        }
    }

    #[test]
    fn credit_and_lookup() {
        let mut state = State::new();
        state.credit(addr(1), 1000).unwrap();
        state.credit(addr(1), 500).unwrap();
        assert_eq!(state.balance(&addr(1)), 1500);
        assert_eq!(state.balance(&addr(2)), 0);
        assert_eq!(state.nonce(&addr(2)), 0);
        assert_eq!(state.account_count(), 1);
        assert!(state.account(&addr(2)).is_none());
    }

    #[test]
    fn transfer_moves_funds_and_rejects_overdraft() {
        let mut state = State::new();
        state.credit(addr(1), 1000).unwrap();
        state.apply_transfer(addr(1), addr(2), 300).unwrap();
        assert_eq!(state.balance(&addr(1)), 700);
        assert_eq!(state.balance(&addr(2)), 300);
        assert!(state.apply_transfer(addr(1), addr(2), 701).is_err());
        assert_eq!(state.balance(&addr(1)), 700, "failed transfer must not mutate");
    }

    #[test]
    fn self_transfer_does_not_duplicate_funds() {
        let mut state = State::new();
        state.credit(addr(1), 100).unwrap();
        state.apply_transfer(addr(1), addr(1), 100).unwrap();
        assert_eq!(state.balance(&addr(1)), 100);
    }

    #[test]
    fn transaction_charges_fee_enforces_nonce_and_blocks_replay() {
        let mut state = State::new();
        state.credit(addr(1), 1_000_000).unwrap();
        let tx = transfer(0, 100);

        let fee = state.apply_transaction(&tx).unwrap();
        assert_eq!(fee, 21_000);
        assert_eq!(state.balance(&addr(2)), 100);
        assert_eq!(state.balance(&addr(1)), 1_000_000 - 100 - 21_000);
        assert_eq!(state.nonce(&addr(1)), 1);

        // Same nonce again must fail: the nonce has advanced.
        assert!(matches!(
            state.apply_transaction(&tx),
            Err(StateError::InvalidNonce { expected: 1, got: 0, .. })
        ));
    }

    #[test]
    fn transaction_rejects_value_it_cannot_afford_with_the_fee() {
        let mut state = State::new();
        // Enough for the value alone, but not value + fee.
        state.credit(addr(1), 21_050).unwrap();
        assert!(matches!(
            state.apply_transaction(&transfer(0, 100)),
            Err(StateError::InsufficientBalance { .. })
        ));
        assert_eq!(state.balance(&addr(1)), 21_050, "must not partially apply");
        assert_eq!(state.balance(&addr(2)), 0);
        assert_eq!(state.nonce(&addr(1)), 0);
    }

    #[test]
    fn transaction_rejects_foreign_chain_and_zero_gas() {
        let mut state = State::with_chain_id(7);
        state.credit(addr(1), 1_000_000).unwrap();
        assert!(matches!(
            state.apply_transaction(&transfer(0, 1)),
            Err(StateError::ChainMismatch { expected: 7, got: 1 })
        ));

        let mut state = State::new();
        state.credit(addr(1), 1_000_000).unwrap();
        let mut tx = transfer(0, 1);
        tx.gas_limit = 0;
        assert!(matches!(state.apply_transaction(&tx), Err(StateError::InvalidTransaction(_))));
    }

    #[test]
    fn transaction_rejects_unsupported_types_and_missing_destination() {
        let mut state = State::new();
        state.credit(addr(1), 1_000_000).unwrap();

        for tx_type in [
            TransactionType::ContractCall,
            TransactionType::ContractCreation,
            TransactionType::Stake,
            TransactionType::Unstake,
            TransactionType::GovernanceVote,
            TransactionType::CrossChain,
        ] {
            let mut tx = transfer(0, 1);
            tx.tx_type = tx_type;
            assert!(matches!(
                state.apply_transaction(&tx),
                Err(StateError::UnsupportedTransactionType(_))
            ));
        }

        let mut tx = transfer(0, 1);
        tx.to = None;
        assert!(matches!(state.apply_transaction(&tx), Err(StateError::InvalidTransaction(_))));
        assert_eq!(state.nonce(&addr(1)), 0);
    }

    #[test]
    fn transaction_rejects_terminal_nonce_and_fee_overflow() {
        let mut state = State::new();
        state.credit(addr(1), 1_000).unwrap();
        state.accounts.get_mut(&addr(1)).unwrap().nonce = Nonce::MAX;
        let mut tx = transfer(Nonce::MAX, 0);
        tx.gas_limit = 1;
        assert!(matches!(
            state.apply_transaction(&tx),
            Err(StateError::InvalidTransaction(message)) if message.contains("nonce overflow")
        ));
        assert_eq!(state.nonce(&addr(1)), Nonce::MAX);
        assert_eq!(state.balance(&addr(1)), 1_000);

        let mut state = State::new();
        state.credit(addr(1), 1_000).unwrap();
        let mut tx = transfer(0, 0);
        tx.gas_price = Amount::MAX;
        assert!(matches!(
            state.apply_transaction(&tx),
            Err(StateError::InvalidTransaction(message)) if message.contains("fee overflows")
        ));
    }

    #[test]
    fn overflow_is_rejected_on_credit_and_on_transfer() {
        let mut state = State::new();
        state.set_balance(addr(1), Amount::MAX);
        assert!(matches!(
            state.credit(addr(1), 1),
            Err(StateError::BalanceOverflow { address }) if address == addr(1)
        ));

        let mut state = State::new();
        state.credit(addr(1), 10).unwrap();
        state.set_balance(addr(2), Amount::MAX);
        assert!(matches!(
            state.apply_transfer(addr(1), addr(2), 1),
            Err(StateError::BalanceOverflow { address }) if address == addr(2)
        ));
        assert_eq!(state.balance(&addr(1)), 10);
    }

    #[test]
    fn snapshot_is_deterministic_and_round_trips() {
        let mut first = State::new();
        first.set_balance(addr(2), 20);
        first.set_balance(addr(1), 10);
        let mut second = State::new();
        second.set_balance(addr(1), 10);
        second.set_balance(addr(2), 20);
        assert_eq!(first.snapshot(), second.snapshot());
        assert_eq!(first.root(), second.root());
        assert_eq!(
            bincode::serialize(&first.snapshot()).unwrap(),
            bincode::serialize(&second.snapshot()).unwrap()
        );

        let restored = State::from_snapshot(first.snapshot()).unwrap();
        assert_eq!(restored.balance(&addr(1)), 10);
        assert_eq!(restored.chain_id(), first.chain_id());
        assert_eq!(restored.root(), first.root());
    }

    #[test]
    fn snapshot_restore_rejects_corrupt_input() {
        assert!(State::from_snapshot(StateSnapshot {
            chain_id: 1,
            accounts: vec![(addr(1), Account::new(10)), (addr(1), Account::new(20))],
        })
        .is_err());

        assert!(State::from_snapshot(StateSnapshot { chain_id: 0, accounts: Vec::new() }).is_err());
    }

    #[test]
    fn state_root_changes_with_every_balance_and_nonce_change() {
        let mut state = State::new();
        let empty = state.root();
        state.credit(addr(1), 1).unwrap();
        let credited = state.root();
        assert_ne!(empty, credited);
        state.accounts.get_mut(&addr(1)).unwrap().nonce = 1;
        assert_ne!(credited, state.root());
    }
}
