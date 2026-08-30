//! Pending transaction pool.
//!
//! The pool is intentionally correctness-first rather than throughput-first:
//! every rule below exists to stop a caller from parking conflicting or
//! unbounded work in memory.

use std::collections::{BTreeMap, HashMap};
use subhost_core::{Address, Hash, Nonce, Transaction};
use tracing::debug;

/// Runtime limits for the transaction pool.
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Hard cap on the total number of pending transactions.
    pub max_pending: usize,
    /// Maximum pending transactions a single sender may keep.
    pub max_per_sender: usize,
    /// Transactions below this gas price are rejected outright.
    pub min_gas_price: u128,
    /// Maximum calldata size accepted for one pending transaction.
    pub max_tx_data_bytes: usize,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_pending: 10_000,
            max_per_sender: 64,
            min_gas_price: 1,
            max_tx_data_bytes: 128 * 1024,
        }
    }
}

impl MempoolConfig {
    /// Reject a configuration that would make the pool unusable or unbounded.
    ///
    /// A `max_per_sender` above `max_pending` is allowed: the global cap simply
    /// binds first, which is exactly what a small pool wants.
    pub fn validate(&self) -> Result<(), MempoolError> {
        if self.max_pending == 0 {
            return Err(MempoolError::InvalidConfig("max_pending must be > 0".into()));
        }
        if self.max_per_sender == 0 {
            return Err(MempoolError::InvalidConfig("max_per_sender must be > 0".into()));
        }
        Ok(())
    }
}

/// The outcome of accepting a transaction into the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// Hash of the transaction that now occupies the (sender, nonce) slot.
    pub hash: Hash,
    /// Whether `hash` refers to the transaction just submitted. `false` means an
    /// equal-or-better transaction already held the slot and was kept.
    pub accepted: bool,
    /// A transaction removed to make room, either replaced or capacity-evicted.
    /// Callers that may fail after admission use this to restore the pool.
    pub displaced: Option<Transaction>,
}

/// A correctness-first pending transaction pool.
///
/// Rules enforced:
/// - One transaction per `(sender, nonce)`; a resubmission for the same nonce
///   only wins with a strictly higher gas price.
/// - A per-sender nonce budget and a global capacity cap, evicting the
///   lowest-gas-price transaction when full.
/// - Deterministic proposal ordering: highest gas price first, then sender, then
///   nonce.
#[derive(Debug)]
pub struct Mempool {
    config: MempoolConfig,
    txs: HashMap<Hash, Transaction>,
    /// sender -> (nonce -> tx hash), keeping the per-sender nonce view cheap.
    by_sender: HashMap<Address, BTreeMap<Nonce, Hash>>,
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(MempoolConfig::default())
    }
}

impl Mempool {
    /// Build a pool. An invalid config is clamped to a usable one; use
    /// [`Mempool::try_new`] to surface the error instead.
    pub fn new(config: MempoolConfig) -> Self {
        match Self::try_new(config) {
            Ok(pool) => pool,
            Err(_) => Self::try_new(MempoolConfig::default())
                .expect("the default mempool configuration is valid"),
        }
    }

    pub fn try_new(config: MempoolConfig) -> Result<Self, MempoolError> {
        config.validate()?;
        Ok(Self { config, txs: HashMap::new(), by_sender: HashMap::new() })
    }

    pub fn config(&self) -> &MempoolConfig {
        &self.config
    }

    /// Insert a transaction, returning the hash that owns the nonce slot.
    pub fn add(&mut self, tx: Transaction) -> Result<Hash, MempoolError> {
        Ok(self.admit(tx)?.hash)
    }

    /// Insert a transaction and report what it displaced, so a caller whose
    /// downstream commit fails can put the pool back exactly as it was.
    pub fn admit(&mut self, tx: Transaction) -> Result<Admission, MempoolError> {
        if tx.data.len() > self.config.max_tx_data_bytes {
            return Err(MempoolError::DataTooLarge {
                provided: tx.data.len(),
                max: self.config.max_tx_data_bytes,
            });
        }
        let hash = tx.hash();

        // Idempotent: the identical transaction is already pending.
        if self.txs.contains_key(&hash) {
            return Ok(Admission { hash, accepted: true, displaced: None });
        }

        if tx.gas_limit == 0 {
            return Err(MempoolError::InvalidTransaction("gas limit must be > 0".into()));
        }
        if tx.gas_price < self.config.min_gas_price {
            return Err(MempoolError::GasPriceTooLow {
                provided: tx.gas_price,
                min: self.config.min_gas_price,
            });
        }
        // A transaction whose fee cannot be represented can never be executed.
        if tx.total_cost().is_none() {
            return Err(MempoolError::InvalidTransaction(
                "gas_price * gas_limit + value overflows".into(),
            ));
        }

        let sender = tx.from;
        let nonce = tx.nonce;
        let mut displaced = None;

        // (1) Replace-by-fee for an occupied (sender, nonce) slot.
        let existing = self.by_sender.get(&sender).and_then(|nonces| nonces.get(&nonce)).copied();
        if let Some(existing_hash) = existing {
            let existing_price =
                self.txs.get(&existing_hash).map_or(0, |existing| existing.gas_price);
            if tx.gas_price <= existing_price {
                return Ok(Admission { hash: existing_hash, accepted: false, displaced: None });
            }
            displaced = self.txs.remove(&existing_hash);
            if let Some(nonces) = self.by_sender.get_mut(&sender) {
                nonces.remove(&nonce);
            }
        }

        // (2) Per-sender budget, only charged for a fresh nonce.
        let sender_len = self.by_sender.get(&sender).map_or(0, BTreeMap::len);
        if displaced.is_none() && sender_len >= self.config.max_per_sender {
            return Err(MempoolError::SenderQueueFull(sender));
        }

        // (3) Global capacity: drop the cheapest transaction before inserting.
        if self.txs.len() >= self.config.max_pending {
            let evicted = self.evict_lowest_priority()?;
            if displaced.is_none() {
                displaced = evicted;
            }
        }

        self.by_sender.entry(sender).or_default().insert(nonce, hash);
        self.txs.insert(hash, tx);
        debug!(%sender, nonce, pool_size = self.txs.len(), "mempool admitted transaction");

        Ok(Admission { hash, accepted: true, displaced })
    }

    /// Drop a transaction once it is mined or expired.
    pub fn remove(&mut self, tx_hash: &Hash) -> bool {
        let Some(tx) = self.txs.remove(tx_hash) else {
            return false;
        };
        if let Some(nonces) = self.by_sender.get_mut(&tx.from) {
            nonces.remove(&tx.nonce);
            if nonces.is_empty() {
                self.by_sender.remove(&tx.from);
            }
        }
        true
    }

    pub fn get(&self, tx_hash: &Hash) -> Option<&Transaction> {
        self.txs.get(tx_hash)
    }

    pub fn contains(&self, tx_hash: &Hash) -> bool {
        self.txs.contains_key(tx_hash)
    }

    pub fn get_by_sender_nonce(
        &self,
        sender: &Address,
        nonce: Nonce,
    ) -> Option<(Hash, Transaction)> {
        let hash = self.by_sender.get(sender)?.get(&nonce).copied()?;
        self.txs.get(&hash).cloned().map(|tx| (hash, tx))
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn sender_count(&self) -> usize {
        self.by_sender.len()
    }

    /// Transactions ordered for proposal: highest gas price first, ties broken by
    /// `(sender, nonce)` so the order is deterministic across nodes.
    ///
    /// A proposer must still enforce nonce continuity against account state.
    pub fn pending(&self) -> Vec<Transaction> {
        let mut txs: Vec<Transaction> = self.txs.values().cloned().collect();
        txs.sort_by(|left, right| {
            right
                .gas_price
                .cmp(&left.gas_price)
                .then_with(|| left.from.cmp(&right.from))
                .then_with(|| left.nonce.cmp(&right.nonce))
        });
        txs
    }

    /// Executable transactions for `sender`, starting at `next_nonce` and
    /// stopping at the first gap. This is the nonce-continuous prefix a proposer
    /// can include without stalling the account.
    pub fn ready_for(&self, sender: &Address, next_nonce: Nonce) -> Vec<Transaction> {
        let Some(nonces) = self.by_sender.get(sender) else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        let mut expected = next_nonce;
        for (nonce, hash) in nonces.range(next_nonce..) {
            if *nonce != expected {
                break;
            }
            match self.txs.get(hash) {
                Some(tx) => ready.push(tx.clone()),
                None => break,
            }
            match expected.checked_add(1) {
                Some(next) => expected = next,
                None => break,
            }
        }
        ready
    }

    /// The nonce a sender should use for its next submission, based on what is
    /// already pending. `None` when the sender has nothing queued.
    pub fn next_nonce_for(&self, sender: &Address) -> Option<Nonce> {
        self.by_sender
            .get(sender)?
            .keys()
            .next_back()
            .copied()
            .and_then(|highest| highest.checked_add(1))
    }

    /// Drop every pending transaction for `sender` below `next_nonce`, which is
    /// how a node prunes transactions that state has already executed.
    pub fn prune_below_nonce(&mut self, sender: &Address, next_nonce: Nonce) -> usize {
        let Some(nonces) = self.by_sender.get_mut(sender) else {
            return 0;
        };
        let stale: Vec<Hash> = nonces.range(..next_nonce).map(|(_, hash)| *hash).collect();
        for hash in &stale {
            self.txs.remove(hash);
        }
        nonces.retain(|nonce, _| *nonce >= next_nonce);
        if nonces.is_empty() {
            self.by_sender.remove(sender);
        }
        stale.len()
    }

    fn evict_lowest_priority(&mut self) -> Result<Option<Transaction>, MempoolError> {
        // Cheapest first; among equals drop the highest nonce so the remaining
        // per-sender sequence stays contiguous from the bottom.
        let victim = self
            .txs
            .iter()
            .min_by(|(left_hash, left), (right_hash, right)| {
                left.gas_price
                    .cmp(&right.gas_price)
                    .then_with(|| right.nonce.cmp(&left.nonce))
                    .then_with(|| left_hash.cmp(right_hash))
            })
            .map(|(hash, _)| *hash);

        match victim {
            Some(hash) => {
                let transaction = self.txs.get(&hash).cloned();
                self.remove(&hash);
                Ok(transaction)
            }
            None => Err(MempoolError::Full),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MempoolError {
    #[error("transaction pool is full")]
    Full,

    #[error("gas price {provided} is below the minimum {min}")]
    GasPriceTooLow { provided: u128, min: u128 },

    #[error("submitted transaction is invalid: {0}")]
    InvalidTransaction(String),

    #[error("sender queue is full: {0}")]
    SenderQueueFull(Address),

    #[error("transaction data is too large: {provided} bytes (max {max})")]
    DataTooLarge { provided: usize, max: usize },

    #[error("invalid mempool configuration: {0}")]
    InvalidConfig(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_core::{TransactionSignature, TransactionType};

    fn tx(sender: u8, nonce: u64, gas_price: u128) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::new([sender; 20]),
            to: Some(Address::new([0u8; 20])),
            value: 1,
            gas_price,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature::EMPTY,
        }
    }

    #[test]
    fn add_and_retrieve() {
        let mut pool = Mempool::default();
        let hash = pool.add(tx(1, 0, 100)).unwrap();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.sender_count(), 1);
        assert!(pool.contains(&hash));
        assert_eq!(pool.get(&hash).unwrap().nonce, 0);
        assert_eq!(pool.get_by_sender_nonce(&Address::new([1; 20]), 0).unwrap().0, hash);
    }

    #[test]
    fn identical_resubmission_is_idempotent() {
        let mut pool = Mempool::default();
        let first = pool.admit(tx(1, 0, 10)).unwrap();
        let second = pool.admit(tx(1, 0, 10)).unwrap();
        assert_eq!(first.hash, second.hash);
        assert!(second.accepted);
        assert!(second.displaced.is_none());
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn duplicate_nonce_keeps_higher_gas_price() {
        let mut pool = Mempool::default();
        let low = tx(1, 0, 10);
        let high = tx(1, 0, 50);
        pool.add(low.clone()).unwrap();

        let replacement = pool.admit(high.clone()).unwrap();
        assert!(replacement.accepted);
        assert_eq!(replacement.displaced.unwrap().gas_price, 10);
        assert_eq!(pool.len(), 1);

        // Re-adding the cheaper transaction must not downgrade the slot.
        let rejected = pool.admit(low).unwrap();
        assert!(!rejected.accepted);
        assert_eq!(rejected.hash, high.hash());
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(&high.hash()).unwrap().gas_price, 50);
    }

    #[test]
    fn replacement_does_not_consume_the_sender_budget() {
        let mut pool = Mempool::new(MempoolConfig { max_per_sender: 1, ..Default::default() });
        pool.add(tx(1, 0, 1)).unwrap();
        // Same nonce with a higher price replaces rather than exceeding the budget.
        assert!(pool.admit(tx(1, 0, 2)).unwrap().accepted);
        // A fresh nonce is over budget.
        assert!(matches!(pool.admit(tx(1, 1, 5)), Err(MempoolError::SenderQueueFull(_))));
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn capacity_evicts_lowest_priority_and_reports_it() {
        let mut pool = Mempool::new(MempoolConfig { max_pending: 3, ..Default::default() });
        pool.add(tx(1, 0, 1)).unwrap();
        pool.add(tx(1, 1, 1)).unwrap();
        pool.add(tx(2, 0, 1)).unwrap();

        let admission = pool.admit(tx(3, 0, 100)).unwrap();
        assert_eq!(pool.len(), 3);
        assert_eq!(admission.displaced.unwrap().gas_price, 1);
        assert!(pool.pending().iter().any(|tx| tx.gas_price == 100));
    }

    #[test]
    fn eviction_is_deterministic_for_equal_gas_prices() {
        let build = || {
            let mut pool = Mempool::new(MempoolConfig { max_pending: 2, ..Default::default() });
            pool.add(tx(1, 0, 1)).unwrap();
            pool.add(tx(1, 1, 1)).unwrap();
            pool.admit(tx(2, 0, 9)).unwrap().displaced.unwrap()
        };
        // Highest nonce among the cheapest is evicted, identically every run.
        assert_eq!(build().nonce, 1);
        assert_eq!(build().hash(), build().hash());
    }

    #[test]
    fn rejects_zero_gas_limit_low_gas_price_and_fee_overflow() {
        let mut pool = Mempool::default();
        let mut zero_gas = tx(1, 0, 5);
        zero_gas.gas_limit = 0;
        assert!(matches!(pool.add(zero_gas), Err(MempoolError::InvalidTransaction(_))));
        assert!(matches!(
            pool.add(tx(1, 0, 0)),
            Err(MempoolError::GasPriceTooLow { provided: 0, min: 1 })
        ));

        let mut overflow = tx(1, 0, u128::MAX);
        overflow.gas_limit = u64::MAX;
        assert!(matches!(pool.add(overflow), Err(MempoolError::InvalidTransaction(_))));
        assert!(pool.is_empty());
    }

    #[test]
    fn rejects_oversized_transaction_data() {
        let mut pool = Mempool::new(MempoolConfig { max_tx_data_bytes: 4, ..Default::default() });
        let mut oversized = tx(1, 0, 10);
        oversized.data = vec![0u8; 5];
        assert!(matches!(
            pool.add(oversized),
            Err(MempoolError::DataTooLarge { provided: 5, max: 4 })
        ));
        assert!(pool.is_empty());
    }

    #[test]
    fn remove_frees_slots_and_cleans_the_sender_index() {
        let mut pool = Mempool::default();
        let hash = pool.add(tx(1, 0, 10)).unwrap();
        assert!(pool.remove(&hash));
        assert!(!pool.remove(&hash));
        assert!(pool.is_empty());
        assert_eq!(pool.sender_count(), 0);
    }

    #[test]
    fn ready_for_stops_at_the_first_nonce_gap() {
        let mut pool = Mempool::default();
        let sender = Address::new([1; 20]);
        pool.add(tx(1, 0, 1)).unwrap();
        pool.add(tx(1, 1, 1)).unwrap();
        pool.add(tx(1, 3, 1)).unwrap();

        let ready = pool.ready_for(&sender, 0);
        assert_eq!(
            ready.iter().map(|tx| tx.nonce).collect::<Vec<_>>(),
            vec![0, 1],
            "nonce 3 is unreachable until 2 arrives"
        );
        assert_eq!(pool.ready_for(&sender, 3).len(), 1);
        assert!(pool.ready_for(&Address::new([9; 20]), 0).is_empty());
        assert_eq!(pool.next_nonce_for(&sender), Some(4));
        assert!(pool.next_nonce_for(&Address::new([9; 20])).is_none());
    }

    #[test]
    fn prune_below_nonce_drops_executed_transactions() {
        let mut pool = Mempool::default();
        let sender = Address::new([1; 20]);
        pool.add(tx(1, 0, 1)).unwrap();
        pool.add(tx(1, 1, 1)).unwrap();
        pool.add(tx(1, 2, 1)).unwrap();

        assert_eq!(pool.prune_below_nonce(&sender, 2), 2);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.ready_for(&sender, 2).len(), 1);

        assert_eq!(pool.prune_below_nonce(&sender, 3), 1);
        assert!(pool.is_empty());
        assert_eq!(pool.sender_count(), 0, "empty sender index must be cleaned");
        assert_eq!(pool.prune_below_nonce(&sender, 9), 0);
    }

    #[test]
    fn pending_order_is_deterministic() {
        let mut pool = Mempool::default();
        pool.add(tx(2, 0, 5)).unwrap();
        pool.add(tx(1, 1, 5)).unwrap();
        pool.add(tx(1, 0, 5)).unwrap();
        pool.add(tx(3, 0, 50)).unwrap();

        let ordered: Vec<_> = pool
            .pending()
            .into_iter()
            .map(|tx| (tx.from.as_bytes()[0], tx.nonce, tx.gas_price))
            .collect();
        assert_eq!(ordered, vec![(3, 0, 50), (1, 0, 5), (1, 1, 5), (2, 0, 5)]);
    }

    #[test]
    fn invalid_configuration_is_rejected_and_clamped() {
        assert!(matches!(
            Mempool::try_new(MempoolConfig { max_pending: 0, ..Default::default() }),
            Err(MempoolError::InvalidConfig(_))
        ));
        assert!(matches!(
            Mempool::try_new(MempoolConfig { max_per_sender: 0, ..Default::default() }),
            Err(MempoolError::InvalidConfig(_))
        ));
        // A per-sender budget above the global cap is legal: the global cap binds.
        assert!(Mempool::try_new(MempoolConfig {
            max_pending: 4,
            max_per_sender: 64,
            ..Default::default()
        })
        .is_ok());

        // `new` must never hand back an unusable pool.
        let clamped = Mempool::new(MempoolConfig { max_pending: 0, ..Default::default() });
        assert_eq!(clamped.config().max_pending, MempoolConfig::default().max_pending);
    }
}
