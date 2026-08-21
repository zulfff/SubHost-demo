use subhost_core::{Address, Hash, Nonce, Transaction};
use std::collections::{BTreeMap, HashMap};
use tracing::debug;

/// Runtime limits for the transaction pool.
///
/// Previously this crate was an empty Config/Module/Error template that kept a
/// counter; it never actually held or ordered transactions.
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Hard cap on the total number of pending transactions.
    pub max_pending: usize,
    /// Max pending transactions a single sender may keep.
    pub max_per_sender: usize,
    /// Transactions below this gas price (in wei) are rejected outright.
    pub min_gas_price: u128,
    /// Maximum calldata/bytecode size accepted for one pending transaction.
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

/// A correctness-first pending transaction pool.
///
/// Rules enforced:
/// - One transaction per (sender, nonce): submitting a different tx for the same
///   nonce only replaces the existing one if it pays a strictly higher gas price.
/// - Per-sender nonce budget and a global capacity cap, evicting the lowest-priority
///   tx (lowest gas price) when the pool is full.
/// - Transactions handing back the pool order transactions so a proposer can pick
///   highest gas price first while preserving nonce order per sender.
#[derive(Default)]
pub struct Mempool {
    config: MempoolConfig,
    txs: HashMap<Hash, Transaction>,
    /// sender -> (nonce -> tx hash). Keeps the per-sender nonce view cheap.
    by_sender: HashMap<Address, BTreeMap<Nonce, Hash>>,
}

impl Mempool {
    pub fn new(config: MempoolConfig) -> Self {
        Self {
            config,
            txs: HashMap::new(),
            by_sender: HashMap::new(),
        }
    }

    fn tx_hash(tx: &Transaction) -> Hash {
        let encoded = bincode::serialize(tx).expect("transaction serialization cannot fail");
        Hash::from_data(&encoded)
    }

    pub fn add(&mut self, tx: Transaction) -> Result<Hash, MempoolError> {
        if tx.data.len() > self.config.max_tx_data_bytes {
            return Err(MempoolError::DataTooLarge {
                provided: tx.data.len(),
                max: self.config.max_tx_data_bytes,
            });
        }
        let hash = Self::tx_hash(&tx);

        // Idempotent: an identical tx already in the pool.
        if self.txs.contains_key(&hash) {
            return Ok(hash);
        }

        if tx.gas_limit == 0 {
            return Err(MempoolError::InvalidTransaction(
                "gas limit must be > 0".to_string(),
            ));
        }
        if tx.gas_price < self.config.min_gas_price {
            return Err(MempoolError::GasPriceTooLow {
                provided: tx.gas_price,
                min: self.config.min_gas_price,
            });
        }

        let sender = tx.from;
        let nonce = tx.nonce;

        // (1) If a tx already occupies (sender, nonce), only a strictly higher
        // gas price may replace it. Peek at it without holding a borrow across mutation.
        let existing = self
            .by_sender
            .get(&sender)
            .and_then(|m| m.get(&nonce))
            .copied();
        if let Some(existing_hash) = existing {
            let existing_price = self.txs.get(&existing_hash).map(|t| t.gas_price).unwrap_or(0);
            if tx.gas_price <= existing_price {
                // Keep the existing, higher-or-equal priority tx.
                return Ok(existing_hash);
            }
            // New tx outbids: drop the old tx for this (sender, nonce).
            self.txs.remove(&existing_hash);
            if let Some(m) = self.by_sender.get_mut(&sender) {
                m.remove(&nonce);
            }
        }

        // (2) Per-sender budget: only enforced for fresh nonces.
        let sender_len = self.by_sender.get(&sender).map(|m| m.len()).unwrap_or(0);
        if !self.by_sender.get(&sender).map_or(false, |m| m.contains_key(&nonce))
            && sender_len >= self.config.max_per_sender
        {
            return Err(MempoolError::SenderQueueFull(sender));
        }

        // (3) Global capacity: evict the globally lowest-priority tx before inserting.
        if self.txs.len() >= self.config.max_pending {
            self.evict_lowest_priority()?;
        }

        // (4) Insert.
        self.by_sender.entry(sender).or_default().insert(nonce, hash);
        self.txs.insert(hash, tx);
        debug!("Mempool add: sender={} nonce={} pool_size={}", sender, nonce, self.txs.len());

        Ok(hash)
    }

    /// Called by the block producer once a tx is included on-chain (or expires).
    pub fn remove(&mut self, tx_hash: &Hash) -> bool {
        let Some(tx) = self.txs.remove(tx_hash) else {
            return false;
        };
        if let Some(map) = self.by_sender.get_mut(&tx.from) {
            map.remove(&tx.nonce);
            if map.is_empty() {
                self.by_sender.remove(&tx.from);
            }
        }
        true
    }

    pub fn get(&self, tx_hash: &Hash) -> Option<&Transaction> {
        self.txs.get(tx_hash)
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    /// Transactions ordered for proposal: highest gas price first, breaking ties by
    /// (sender, nonce). Consumers should additionally enforce nonce-continuity per
    /// sender based on the state root's account nonces before assembling a block.
    pub fn pending(&self) -> Vec<Transaction> {
        let mut txs: Vec<Transaction> = self.txs.values().cloned().collect();
        txs.sort_by(|a, b| {
            b.gas_price
                .cmp(&a.gas_price)
                .then_with(|| a.from.as_bytes().cmp(b.from.as_bytes()))
                .then_with(|| a.nonce.cmp(&b.nonce))
        });
        txs
    }

    /// Highest `nonce` currently pending for `sender` (None if empty). Useful to
    /// reject txs with an unexpected nonce without a state backend.
    pub fn next_nonce_for(&self, sender: &Address) -> Option<Nonce> {
        self.by_sender
            .get(sender)
            .and_then(|m| m.keys().next_back().copied())
            .and_then(|max| max.checked_add(1))
    }

    fn evict_lowest_priority(&mut self) -> Result<(), MempoolError> {
        // Lowest gas price; tie-break to keep the chain clean (drop the highest
        // nonce among the cheapest) so a proposer can keep continuity.
        let victim = self
            .txs
            .iter()
            .min_by(|(_, a), (_, b)| {
                a.gas_price
                    .cmp(&b.gas_price)
                    .then_with(|| b.nonce.cmp(&a.nonce))
            })
            .map(|(h, _)| *h);

        match victim {
            Some(hash) => {
                self.remove(&hash);
                Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_core::TransactionType;

    fn tx(sender: u8, nonce: u64, gas_price: u128) -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::new([sender; 20]),
            to: Some(Address::new([0u8; 20])),
            value: 1,
            gas_price,
            gas_limit: 21_000,
            data: vec![],
            chain_id: 1,
            signature: subhost_core::TransactionSignature {
                r: [0u8; 32],
                s: [0u8; 32],
                v: 27,
            },
        }
    }

    #[test]
    fn add_and_retrieve() {
        let mut pool = Mempool::new(MempoolConfig::default());
        let tx = tx(1, 0, 100);
        let hash = pool.add(tx.clone()).unwrap();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(&hash).unwrap().nonce, 0);
    }

    #[test]
    fn duplicate_nonce_keeps_higher_gas_price() {
        let mut pool = Mempool::new(MempoolConfig::default());
        let low = tx(1, 0, 10);
        let high = tx(1, 0, 50);
        let _ = pool.add(low.clone()).unwrap();
        let high_hash = pool.add(high.clone()).unwrap();
        assert_eq!(high_hash, pool.add(high.clone()).unwrap());
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(&high_hash).unwrap().gas_price, 50);
        // Re-adding the lower one must NOT downgrade.
        pool.add(low.clone()).unwrap();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(&high_hash).unwrap().gas_price, 50);
    }

    #[test]
    fn capacity_evicts_lowest_priority() {
        let mut pool = Mempool::new(MempoolConfig {
            max_pending: 3,
            ..Default::default()
        });
        pool.add(tx(1, 0, 1)).unwrap();
        pool.add(tx(1, 1, 1)).unwrap();
        pool.add(tx(2, 0, 1)).unwrap();
        // Pool is full with 3. Adding a high-priority tx evicts one of the gas=1 txs.
        pool.add(tx(3, 0, 100)).unwrap();
        assert_eq!(pool.len(), 3);
        // The high-priority tx must survive eviction.
        assert!(pool.pending().iter().any(|t| t.gas_price == 100));
    }

    #[test]
    fn rejects_zero_gas_limit_and_low_gas_price() {
        let mut pool = Mempool::new(MempoolConfig::default());
        let mut bad = tx(1, 0, 5);
        bad.gas_limit = 0;
        assert!(pool.add(bad).is_err());
        assert!(pool.add(tx(1, 0, 0)).is_err());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn per_sender_queue_capped() {
        let mut pool = Mempool::new(MempoolConfig {
            max_per_sender: 2,
            max_pending: 100,
            min_gas_price: 1,
            max_tx_data_bytes: 128 * 1024,
        });
        pool.add(tx(1, 0, 1)).unwrap();
        pool.add(tx(1, 1, 1)).unwrap();
        assert!(pool.add(tx(1, 2, 1)).is_err()); // sender queue full
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn remove_frees_slots() {
        let mut pool = Mempool::new(MempoolConfig::default());
        let tx = tx(1, 0, 10);
        let hash = pool.add(tx).unwrap();
        assert!(pool.remove(&hash));
        assert!(!pool.remove(&hash));
        assert!(pool.is_empty());
    }

    #[test]
    fn rejects_oversized_transaction_data() {
        let mut pool = Mempool::new(MempoolConfig {
            max_tx_data_bytes: 4,
            ..Default::default()
        });
        let mut transaction = tx(1, 0, 10);
        transaction.data = vec![0u8; 5];
        assert!(matches!(
            pool.add(transaction),
            Err(MempoolError::DataTooLarge { provided: 5, max: 4 })
        ));
        assert!(pool.is_empty());
    }
}
