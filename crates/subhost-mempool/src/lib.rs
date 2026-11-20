//! Pending transaction pool.
//!
//! The pool is intentionally correctness-first rather than throughput-first:
//! every rule below exists to stop a caller from parking conflicting or
//! unbounded work in memory.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use subhost_core::{Address, Hash, Nonce, Transaction};
use subhost_metrics::{MempoolEvent, Metrics, SenderDepthBucket};
use tracing::debug;

mod persistence;
pub use persistence::{
    MempoolPersistenceError, MempoolStore, MAX_MEMPOOL_BYTES, MAX_MEMPOOL_TRANSACTIONS,
    MAX_MEMPOOL_TRANSACTION_BYTES, MEMPOOL_FILE_NAME,
};

/// Runtime limits for the transaction pool.
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Hard cap on the total number of pending transactions.
    pub max_pending: usize,
    /// Maximum pending transactions a single sender may keep.
    pub max_per_sender: usize,
    /// Transactions below this gas price are rejected outright.
    pub min_gas_price: u128,
    /// Maximum calldata size accepted for one pending transaction. The
    /// default is deliberately below the 128 KiB protocol upper bound: at the
    /// Q-037 floor rate (16 gas/byte) a max-size transaction must cost real
    /// gas, and one transaction cannot dominate a block's calldata (L0).
    pub max_tx_data_bytes: usize,
    /// Aggregate encoded-byte budget for the live pool, enforced with
    /// persistence on or off. Count and per-sender caps alone do not bound
    /// memory: `max_pending * max_tx_data_bytes` is over a GiB at the defaults.
    pub max_pending_bytes: usize,
    /// How long a pending transaction may sit in the pool before the expiry
    /// sweep drops it (S6.3, Q-039). Measured from admission into the live
    /// pool; restore resets the clock because the snapshot stores transactions,
    /// not timestamps. Must be non-zero.
    pub tx_ttl: Duration,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_pending: 10_000,
            max_per_sender: 64,
            min_gas_price: 1,
            max_tx_data_bytes: 16 * 1024,
            // Half the snapshot file cap, so a pool that fits the runtime budget
            // still fits `mempool.bin` with its framing overhead.
            max_pending_bytes: 32 * 1024 * 1024,
            tx_ttl: Duration::from_secs(30 * 60),
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
        if self.max_pending_bytes == 0 {
            return Err(MempoolError::InvalidConfig("max_pending_bytes must be > 0".into()));
        }
        if self.tx_ttl.is_zero() {
            return Err(MempoolError::InvalidConfig("tx_ttl must be > 0".into()));
        }
        Ok(())
    }
}

/// Encoded size of one pending transaction, measured the same way the snapshot
/// writer measures it. Deterministic per transaction, so insert and removal
/// account the same number and the running total cannot drift. An unencodable
/// transaction saturates instead of counting as zero: it is then refused by the
/// byte budget rather than silently admitted for free.
fn encoded_bytes(tx: &Transaction) -> usize {
    bincode::serialized_size(tx)
        .ok()
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or(usize::MAX)
}

/// Compare fee density — gas price per encoded byte — by cross-multiplication
/// so no division precision is lost. Products are checked u128: an overflowing
/// side is the denser one, and if both overflow the raw gas prices decide.
/// Deterministic for any pair of transactions (L0, Q-037 interim C).
fn fee_density_cmp(
    left: &Transaction,
    left_bytes: u128,
    right: &Transaction,
    right_bytes: u128,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left.gas_price.checked_mul(right_bytes), right.gas_price.checked_mul(left_bytes)) {
        (Some(l), Some(r)) => l.cmp(&r),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (None, None) => left.gas_price.cmp(&right.gas_price),
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
///   only wins with a strictly higher fee density (gas price per encoded byte).
/// - A per-sender nonce budget and a global capacity cap, evicting the
///   lowest-fee-density transaction when full.
/// - Deterministic proposal ordering: highest gas price first, then sender, then
///   nonce.
#[derive(Debug, Clone)]
pub struct Mempool {
    config: MempoolConfig,
    txs: HashMap<Hash, Transaction>,
    /// sender -> (nonce -> tx hash), keeping the per-sender nonce view cheap.
    by_sender: HashMap<Address, BTreeMap<Nonce, Hash>>,
    /// Lifecycle events buffered since the last flush. The submit path admits
    /// into a cloned candidate pool and drops the clone when a later step
    /// fails, so events must travel with the pool and only reach the registry
    /// once the candidate is committed; see [`Mempool::flush_events`].
    events: Vec<MempoolEvent>,
    /// Sum of the encoded sizes of the pending transactions, kept in step with
    /// `txs` so the byte budget never needs a full re-measure.
    pending_bytes: usize,
    /// Admission instant per pending hash, the clock the expiry sweep reads
    /// (S6.3). Kept in step with `txs` at every insert and removal site.
    admitted_at: HashMap<Hash, Instant>,
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
        Ok(Self {
            config,
            txs: HashMap::new(),
            by_sender: HashMap::new(),
            events: Vec::new(),
            pending_bytes: 0,
            admitted_at: HashMap::new(),
        })
    }

    pub fn config(&self) -> &MempoolConfig {
        &self.config
    }

    /// Encoded bytes currently held by the live pool.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Insert a transaction, returning the hash that owns the nonce slot.
    pub fn add(&mut self, tx: Transaction) -> Result<Hash, MempoolError> {
        Ok(self.admit(tx)?.hash)
    }

    /// Insert a transaction and report what it displaced, so a caller whose
    /// downstream commit fails can put the pool back exactly as it was.
    pub fn admit(&mut self, tx: Transaction) -> Result<Admission, MempoolError> {
        self.admit_inner(tx, true)
    }

    /// Restore-path admission: every cap and validity rule applies, but the
    /// local gas-price floor does not.
    ///
    /// Q-017 requires an entry that is merely underpriced against the current
    /// base fee to stay parked across a restart rather than be deleted, so a
    /// fee-floor change must not silently shrink a restored pool. Count,
    /// per-sender, calldata and byte-budget limits still bind, and a transaction
    /// that cannot fit is dropped deterministically by the caller.
    pub fn admit_restored(&mut self, tx: Transaction) -> Result<Admission, MempoolError> {
        self.admit_inner(tx, false)
    }

    fn admit_inner(
        &mut self,
        tx: Transaction,
        enforce_floor: bool,
    ) -> Result<Admission, MempoolError> {
        // Expiry is hygiene independent of the admission outcome: dead entries
        // go first so capacity and byte budgets never displace a live
        // transaction to make room for them (S6.3, Q-039).
        self.expire_before(Instant::now());
        if tx.data.len() > self.config.max_tx_data_bytes {
            return Err(MempoolError::DataTooLarge {
                provided: tx.data.len(),
                max: self.config.max_tx_data_bytes,
            });
        }
        let hash = tx.hash();
        let tx_bytes = encoded_bytes(&tx);
        // Refuse before mutating anything: a transaction that cannot fit an empty
        // pool must leave the pool exactly as it was.
        if tx_bytes > self.config.max_pending_bytes {
            return Err(MempoolError::PendingBytesExceeded {
                provided: tx_bytes,
                max: self.config.max_pending_bytes,
            });
        }

        // Idempotent: the identical transaction is already pending.
        if self.txs.contains_key(&hash) {
            return Ok(Admission { hash, accepted: true, displaced: None });
        }

        if tx.gas_limit == 0 {
            return Err(MempoolError::InvalidTransaction("gas limit must be > 0".into()));
        }
        if enforce_floor && tx.gas_price < self.config.min_gas_price {
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
            // Replace-by-fee ranks by fee density, not raw price: a higher
            // price attached to far more bytes is not a better offer (L0).
            let wins = self.txs.get(&existing_hash).is_none_or(|existing| {
                fee_density_cmp(&tx, tx_bytes as u128, existing, encoded_bytes(existing) as u128)
                    == std::cmp::Ordering::Greater
            });
            if !wins {
                return Ok(Admission { hash: existing_hash, accepted: false, displaced: None });
            }
            displaced = self.txs.remove(&existing_hash);
            self.admitted_at.remove(&existing_hash);
            if let Some(replaced) = &displaced {
                self.pending_bytes = self.pending_bytes.saturating_sub(encoded_bytes(replaced));
                self.events.push(MempoolEvent::Replaced);
            }
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
            if evicted.is_some() {
                self.events.push(MempoolEvent::Evicted);
            }
            if displaced.is_none() {
                displaced = evicted;
            }
        }

        // (4) Aggregate byte budget: same deterministic victim order as the count
        // cap. The upfront check guarantees an empty pool fits this transaction,
        // so the loop always terminates without evicting the newcomer.
        while self.pending_bytes.saturating_add(tx_bytes) > self.config.max_pending_bytes {
            let evicted = self.evict_lowest_priority()?;
            if evicted.is_none() {
                break;
            }
            self.events.push(MempoolEvent::Evicted);
            if displaced.is_none() {
                displaced = evicted;
            }
        }

        self.by_sender.entry(sender).or_default().insert(nonce, hash);
        self.txs.insert(hash, tx);
        self.admitted_at.insert(hash, Instant::now());
        self.pending_bytes = self.pending_bytes.saturating_add(tx_bytes);
        self.events.push(MempoolEvent::Admitted);
        debug!(%sender, nonce, pool_size = self.txs.len(), "mempool admitted transaction");

        Ok(Admission { hash, accepted: true, displaced })
    }

    /// Drop every entry admitted at or before `now - tx_ttl`, buffering one
    /// `Expired` event per drop (S6.3, Q-039). The clock is an argument so
    /// callers — and tests — control time; the admission paths sweep with
    /// `Instant::now()` so a full pool never evicts a live transaction to make
    /// room for one that is already dead. Returns the number dropped. If `now`
    /// predates one full TTL, nothing can be expired yet.
    pub fn expire_before(&mut self, now: Instant) -> usize {
        let Some(cutoff) = now.checked_sub(self.config.tx_ttl) else {
            return 0;
        };
        let mut expired: Vec<Hash> = self
            .admitted_at
            .iter()
            .filter(|(_, admitted)| **admitted <= cutoff)
            .map(|(hash, _)| *hash)
            .collect();
        // Deterministic drop order, independent of hash-map iteration.
        expired.sort();
        let mut dropped = 0;
        for hash in expired {
            if self.remove(&hash) {
                self.events.push(MempoolEvent::Expired);
                dropped += 1;
            }
        }
        if dropped > 0 {
            debug!(dropped, pool_size = self.txs.len(), "mempool expired transactions");
        }
        dropped
    }

    /// Drain buffered lifecycle events into the registry.
    ///
    /// Call this only on the committed pool, after a candidate was swapped in:
    /// events buffered in a dropped candidate vanish with it, so a rejected
    /// submit never counts. Draining is idempotent and safe to call from any
    /// request that observes a successful commit; the pool write lock keeps
    /// concurrent drains from counting an event twice.
    pub fn flush_events(&mut self, metrics: &Metrics) {
        for event in self.events.drain(..) {
            metrics.record_mempool_event(event);
        }
        // Pool depth per sender-depth bucket (S6.3, Q-039): the gauge value is
        // the number of pending transactions whose sender sits in that bucket,
        // so the four series sum to the pool size. Committed pool only, exactly
        // like the counters above.
        let mut depths = [0u64; SenderDepthBucket::ALL.len()];
        for nonces in self.by_sender.values() {
            let depth = nonces.len();
            let bucket = SenderDepthBucket::for_depth(depth);
            depths[bucket.index()] += depth as u64;
        }
        for (bucket, value) in SenderDepthBucket::ALL.iter().zip(depths) {
            metrics.set_mempool_sender_depth(*bucket, value);
        }
    }

    /// Discard buffered lifecycle events without recording them.
    ///
    /// The no-registry counterpart of [`Self::flush_events`]: a pool whose
    /// owner has no metrics must still drain on every flush, so the buffer
    /// cannot grow unbounded and a registry attached later never publishes
    /// admissions that predate it.
    pub fn drain_events(&mut self) {
        self.events.clear();
    }

    /// Drop a transaction once it is mined or expired.
    pub fn remove(&mut self, tx_hash: &Hash) -> bool {
        let Some(tx) = self.txs.remove(tx_hash) else {
            return false;
        };
        self.admitted_at.remove(tx_hash);
        self.pending_bytes = self.pending_bytes.saturating_sub(encoded_bytes(&tx));
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
        let stale: Vec<Hash> = match self.by_sender.get(sender) {
            Some(nonces) => nonces.range(..next_nonce).map(|(_, hash)| *hash).collect(),
            None => return 0,
        };
        // `remove` also drops the sender entry when its last nonce goes, and is
        // the single place the byte total is decremented.
        for hash in &stale {
            self.remove(hash);
        }
        stale.len()
    }

    fn evict_lowest_priority(&mut self) -> Result<Option<Transaction>, MempoolError> {
        // Lowest fee density first; among equals drop the highest nonce so the
        // remaining per-sender sequence stays contiguous from the bottom, then
        // the hash for determinism. Sizes are measured once per candidate, the
        // same way the byte budget measures them (L0, Q-037 interim C).
        let victim = self
            .txs
            .iter()
            .map(|(hash, tx)| (hash, tx, encoded_bytes(tx) as u128))
            .min_by(|(left_hash, left, left_bytes), (right_hash, right, right_bytes)| {
                fee_density_cmp(left, *left_bytes, right, *right_bytes)
                    .then_with(|| right.nonce.cmp(&left.nonce))
                    .then_with(|| left_hash.cmp(right_hash))
            })
            .map(|(hash, _, _)| *hash);

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

    #[error("transaction needs {provided} encoded bytes, above the pool budget of {max}")]
    PendingBytesExceeded { provided: usize, max: usize },

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
            public_key: [0u8; subhost_core::PUBLIC_KEY_SIZE],
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

    #[test]
    fn lifecycle_events_buffer_until_flush_and_drain_once() {
        let metrics = Metrics::new().unwrap();
        let mut pool = Mempool::default();
        pool.add(tx(1, 0, 10)).unwrap();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 0"));
        pool.flush_events(&metrics);
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
    }

    #[test]
    fn drain_without_a_registry_discards_and_never_publishes_history() {
        let mut pool = Mempool::default();
        pool.add(tx(1, 0, 10)).unwrap();
        pool.add(tx(2, 0, 10)).unwrap();
        assert_eq!(pool.events.len(), 2);
        pool.drain_events();
        assert!(pool.events.is_empty(), "the buffer must not grow without a registry");
        pool.drain_events();

        // A registry attached later sees only admissions that follow it.
        let metrics = Metrics::new().unwrap();
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 0"));
        pool.add(tx(3, 0, 10)).unwrap();
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
    }

    #[test]
    fn replacement_and_eviction_buffer_typed_events() {
        let metrics = Metrics::new().unwrap();
        let mut pool = Mempool::new(MempoolConfig { max_pending: 2, ..Default::default() });
        pool.add(tx(1, 0, 10)).unwrap();
        pool.add(tx(2, 0, 5)).unwrap();
        // Replace-by-fee on the occupied (sender, nonce) slot.
        pool.admit(tx(1, 0, 20)).unwrap();
        // Full pool: the cheapest transaction (sender 2) is capacity-evicted.
        let admission = pool.admit(tx(3, 0, 30)).unwrap();
        assert_eq!(admission.displaced.unwrap().gas_price, 5);
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 4"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"replaced\"} 1"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"evicted\"} 1"));
    }

    #[test]
    fn dropped_candidate_events_never_reach_the_registry() {
        let metrics = Metrics::new().unwrap();
        let mut live = Mempool::default();
        let mut candidate = live.clone();
        candidate.add(tx(1, 0, 10)).unwrap();
        // A rejected submit discards its candidate clone, events included.
        drop(candidate);
        live.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 0"));

        // The committed path publishes exactly its own events.
        let mut candidate = live.clone();
        candidate.add(tx(2, 0, 10)).unwrap();
        live = candidate;
        live.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
    }

    #[test]
    fn idempotent_and_rejected_admissions_buffer_nothing() {
        let metrics = Metrics::new().unwrap();
        let mut pool = Mempool::default();
        let first = tx(1, 0, 10);
        pool.admit(first.clone()).unwrap();
        pool.admit(first).unwrap();
        assert!(!pool.admit(tx(1, 0, 5)).unwrap().accepted);
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"replaced\"} 0"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"evicted\"} 0"));
    }

    /// Independent re-measure of the live pool, to prove the running total never
    /// drifts from the entries actually held.
    fn measured(pool: &Mempool) -> usize {
        pool.pending().iter().map(encoded_bytes).sum()
    }

    #[test]
    fn byte_budget_evicts_the_cheapest_and_reports_it() {
        let metrics = Metrics::new().unwrap();
        let one = usize::try_from(bincode::serialized_size(&tx(1, 0, 1)).unwrap()).unwrap();
        let mut pool =
            Mempool::new(MempoolConfig { max_pending_bytes: one * 2 + 1, ..Default::default() });
        pool.add(tx(1, 0, 10)).unwrap();
        pool.add(tx(2, 0, 5)).unwrap();
        assert_eq!(pool.pending_bytes(), one * 2);

        // Three entries cannot fit: the cheapest goes, exactly as the count cap.
        let admission = pool.admit(tx(3, 0, 30)).unwrap();
        assert_eq!(admission.displaced.unwrap().gas_price, 5);
        assert_eq!(pool.len(), 2);
        assert!(pool.pending_bytes() <= one * 2 + 1);
        assert_eq!(pool.pending_bytes(), measured(&pool));

        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 3"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"evicted\"} 1"));
    }

    #[test]
    fn a_transaction_over_the_budget_is_refused_without_mutating_the_pool() {
        let mut pool =
            Mempool::new(MempoolConfig { max_pending_bytes: 4096, ..Default::default() });
        let kept = pool.add(tx(1, 0, 10)).unwrap();
        let before = pool.pending_bytes();

        let mut huge = tx(2, 0, 10);
        huge.data = vec![0; 8192];
        let error = pool.admit(huge).unwrap_err();
        assert!(matches!(error, MempoolError::PendingBytesExceeded { .. }), "{error:?}");
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.pending_bytes(), before);
        assert!(pool.contains(&kept));
        assert_eq!(pool.pending_bytes(), measured(&pool));
    }

    #[test]
    fn replacement_removal_and_prune_keep_the_byte_total_exact() {
        let mut pool = Mempool::default();
        pool.add(tx(1, 0, 10)).unwrap();
        pool.add(tx(1, 1, 10)).unwrap();
        pool.add(tx(2, 0, 10)).unwrap();
        assert_eq!(pool.pending_bytes(), measured(&pool));

        // Price high enough that fee density still wins with the extra bytes.
        let mut replacement = tx(1, 0, 200);
        replacement.data = vec![0; 1024];
        assert!(pool.admit(replacement).unwrap().accepted);
        assert_eq!(pool.pending_bytes(), measured(&pool));

        assert_eq!(pool.prune_below_nonce(&Address::new([1; 20]), 1), 1);
        assert_eq!(pool.pending_bytes(), measured(&pool));

        let mined = pool.get_by_sender_nonce(&Address::new([2; 20]), 0).unwrap().0;
        assert!(pool.remove(&mined));
        assert_eq!(pool.pending_bytes(), measured(&pool));
        assert!(!pool.remove(&mined));
        assert_eq!(pool.pending_bytes(), measured(&pool));
    }

    #[test]
    fn the_byte_budget_binds_with_no_persistence_configured() {
        // No store, no snapshot: the runtime budget is the only memory bound.
        let mut pool = Mempool::new(MempoolConfig {
            max_per_sender: 1_000,
            max_tx_data_bytes: 128 * 1024,
            ..Default::default()
        });
        let mut template = tx(1, 0, 10);
        template.data = vec![0; 128 * 1024];
        for nonce in 0..400 {
            let mut next = template.clone();
            next.nonce = nonce;
            pool.add(next).unwrap();
            assert!(pool.pending_bytes() <= pool.config().max_pending_bytes);
        }
        assert!(pool.len() < 400, "the budget must evict before 50 MiB accumulates");
        assert_eq!(pool.pending_bytes(), measured(&pool));
    }

    #[test]
    fn a_zero_byte_budget_is_an_invalid_config() {
        assert!(matches!(
            Mempool::try_new(MempoolConfig { max_pending_bytes: 0, ..Default::default() }),
            Err(MempoolError::InvalidConfig(_))
        ));
    }
    // --- Q-037 L0: tighter cap and fee-density ranking ---

    fn sized(sender: u8, nonce: u64, gas_price: u128, data_bytes: usize) -> Transaction {
        let mut transaction = tx(sender, nonce, gas_price);
        transaction.data = vec![0; data_bytes];
        transaction
    }

    #[test]
    fn the_default_calldata_cap_is_sixteen_kibibytes() {
        assert_eq!(MempoolConfig::default().max_tx_data_bytes, 16 * 1024);
        let mut pool = Mempool::default();
        assert!(matches!(
            pool.admit(sized(1, 0, 10, 16 * 1024 + 1)),
            Err(MempoolError::DataTooLarge { provided, max }) if provided == 16 * 1024 + 1 && max == 16 * 1024
        ));
        pool.admit(sized(1, 0, 10, 16 * 1024)).unwrap();
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn replacement_ranks_by_fee_density_not_raw_price() {
        let mut pool = Mempool::default();
        let original = tx(1, 0, 100);
        pool.add(original.clone()).unwrap();

        // A higher raw price spread over far more bytes is a worse offer.
        let bloated = sized(1, 0, 150, 8192);
        let rejected = pool.admit(bloated).unwrap();
        assert!(!rejected.accepted);
        assert_eq!(rejected.hash, original.hash());

        // A much higher price with modest bytes wins.
        let better = sized(1, 0, 10_000, 1024);
        let accepted = pool.admit(better.clone()).unwrap();
        assert!(accepted.accepted);
        assert_eq!(accepted.displaced.unwrap().hash(), original.hash());
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn capacity_eviction_drops_the_lowest_fee_density_first() {
        let mut pool = Mempool::new(MempoolConfig { max_pending: 2, ..Default::default() });
        let dense = tx(1, 0, 10);
        let bloated = sized(2, 0, 20, 8192);
        pool.add(dense.clone()).unwrap();
        pool.add(bloated.clone()).unwrap();

        // The newcomer has the lowest raw price, but the bloated entry has the
        // lowest fee density, so it is the victim.
        let admission = pool.admit(tx(3, 0, 5)).unwrap();
        assert_eq!(admission.displaced.unwrap().hash(), bloated.hash());
        assert!(pool.contains(&dense.hash()));
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn byte_budget_eviction_drops_the_lowest_fee_density_first() {
        // Composition with the S6.1b runtime byte budget (nara's L0 ask): the
        // budget loop reuses evict_lowest_priority, so it inherits fee density.
        let dense = tx(1, 0, 10);
        let bloated = sized(2, 0, 20, 8192);
        let one = usize::try_from(bincode::serialized_size(&dense).unwrap()).unwrap();
        let bloat = usize::try_from(bincode::serialized_size(&bloated).unwrap()).unwrap();
        let mut pool = Mempool::new(MempoolConfig {
            max_pending_bytes: one + bloat + one / 2,
            ..Default::default()
        });
        pool.add(dense.clone()).unwrap();
        pool.add(bloated.clone()).unwrap();

        let admission = pool.admit(tx(3, 0, 5)).unwrap();
        assert_eq!(admission.displaced.unwrap().hash(), bloated.hash());
        assert!(pool.contains(&dense.hash()));
        assert!(pool.pending_bytes() <= pool.config().max_pending_bytes);
    }

    #[test]
    fn an_overflowing_fee_density_ranks_highest() {
        let mut pool = Mempool::new(MempoolConfig { max_pending: 2, ..Default::default() });
        // A price whose density product overflows u128 while total_cost still
        // fits (gas_limit 1), so admission accepts it but ranking must not
        // panic or wrap: the overflowing side is the denser one.
        let mut saturating = tx(1, 0, u128::MAX - 1_000);
        saturating.gas_limit = 1;
        let normal = tx(2, 0, 10);
        pool.add(saturating.clone()).unwrap();
        pool.add(normal.clone()).unwrap();

        let admission = pool.admit(tx(3, 0, 5)).unwrap();
        assert_eq!(admission.displaced.unwrap().hash(), normal.hash());
        assert!(pool.contains(&saturating.hash()));

        // Both products overflow: the raw gas prices decide, deterministically.
        let mut lower = tx(1, 0, u128::MAX - 2_000);
        lower.gas_limit = 1;
        let rejected = pool.admit(lower).unwrap();
        assert!(!rejected.accepted);
        assert_eq!(rejected.hash, saturating.hash());
    }

    #[test]
    fn restore_admission_drops_entries_over_the_tighter_cap() {
        // nara's L0 ask: the Q-034 restore filter is just a cap, so entries
        // persisted under the old 128 KiB default are cap-dropped on restore.
        let mut pool = Mempool::default();
        assert!(matches!(
            pool.admit_restored(sized(1, 0, 10, 32 * 1024)),
            Err(MempoolError::DataTooLarge { .. })
        ));
        pool.admit_restored(sized(1, 0, 10, 16 * 1024)).unwrap();
        assert_eq!(pool.len(), 1);
    }
    // --- S6.3 (Q-039): TTL expiry and sender-depth gauge ---

    #[test]
    fn the_default_ttl_is_thirty_minutes_and_zero_is_refused() {
        assert_eq!(MempoolConfig::default().tx_ttl, Duration::from_secs(30 * 60));
        let zero = MempoolConfig { tx_ttl: Duration::ZERO, ..Default::default() };
        assert!(matches!(Mempool::try_new(zero), Err(MempoolError::InvalidConfig(_))));
    }

    #[test]
    fn expire_before_drops_only_entries_past_the_ttl() {
        let mut pool = Mempool::default();
        let ttl = pool.config().tx_ttl;
        let before = Instant::now();
        let hash = pool.add(tx(1, 0, 10)).unwrap();
        let after = Instant::now();

        // One nanosecond short of the TTL: nothing is expired yet.
        assert_eq!(pool.expire_before(before + ttl - Duration::from_nanos(1)), 0);
        assert!(pool.contains(&hash));

        // A full TTL past the admission window: the entry goes, and every
        // index and accounting view is cleaned with it.
        assert_eq!(pool.expire_before(after + ttl), 1);
        assert!(!pool.contains(&hash));
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.sender_count(), 0);
        assert_eq!(pool.pending_bytes(), 0);
        // Idempotent: a second sweep drops nothing.
        assert_eq!(pool.expire_before(after + ttl), 0);
    }

    #[test]
    fn a_now_before_one_full_ttl_expires_nothing() {
        let mut pool = Mempool::default();
        pool.add(tx(1, 0, 10)).unwrap();
        // checked_sub underflow path: the process just started.
        assert_eq!(pool.expire_before(Instant::now()), 0);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn expiry_buffers_events_that_only_a_committed_flush_counts() {
        let metrics = Metrics::new().unwrap();
        let mut pool = Mempool::default();
        let ttl = pool.config().tx_ttl;
        pool.add(tx(1, 0, 10)).unwrap();
        pool.add(tx(2, 0, 10)).unwrap();
        let now = Instant::now();
        assert_eq!(pool.expire_before(now + ttl), 2);

        // A dropped candidate takes its buffered events with it (Q-026).
        let mut candidate = pool.clone();
        candidate.drain_events();
        candidate.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"expired\"} 0"));

        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"expired\"} 2"));
    }

    #[test]
    fn expiry_frees_capacity_before_eviction_touches_live_entries() {
        let mut pool = Mempool::new(MempoolConfig { max_pending: 1, ..Default::default() });
        let ttl = pool.config().tx_ttl;
        let old = tx(1, 0, 10);
        pool.add(old.clone()).unwrap();
        assert_eq!(pool.expire_before(Instant::now() + ttl), 1);

        // The slot is free: admitting a newcomer evicts nothing live.
        let admission = pool.admit(tx(2, 0, 5)).unwrap();
        assert!(admission.accepted);
        assert!(admission.displaced.is_none());
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn replacement_removal_and_pruning_keep_the_expiry_clock_in_step() {
        let mut pool = Mempool::default();
        let ttl = pool.config().tx_ttl;
        let original = tx(1, 0, 10);
        pool.add(original.clone()).unwrap();
        let replacement = tx(1, 0, 10_000);
        pool.admit(replacement.clone()).unwrap();
        pool.add(tx(1, 1, 10)).unwrap();
        pool.add(tx(1, 2, 10)).unwrap();

        // Prune the two lowest nonces, then expire: exactly the survivor goes,
        // no ghost timestamps from the replaced or pruned entries.
        assert_eq!(pool.prune_below_nonce(&original.from, 2), 2);
        assert_eq!(pool.expire_before(Instant::now() + ttl), 1);
        assert_eq!(pool.len(), 0);

        // remove() also clears the clock.
        pool.add(tx(2, 0, 10)).unwrap();
        assert!(pool.remove(&tx(2, 0, 10).hash()));
        assert_eq!(pool.expire_before(Instant::now() + ttl), 0);
    }

    #[test]
    fn restore_resets_the_expiry_clock() {
        let mut pool = Mempool::default();
        let ttl = pool.config().tx_ttl;
        pool.admit_restored(tx(1, 0, 10)).unwrap();
        // A freshly restored entry is not already expired (Q-039 point 3).
        assert_eq!(pool.expire_before(Instant::now()), 0);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.expire_before(Instant::now() + ttl), 1);
    }

    #[test]
    fn flush_publishes_sender_depth_buckets_that_sum_to_the_pool() {
        // tania's boundary matrix: senders at exactly 1, 4, 5, 16 and 17.
        let metrics = Metrics::new().unwrap();
        let mut pool = Mempool::default();
        for (sender, depth) in [(1u8, 1usize), (2, 4), (3, 5), (4, 16), (5, 17)] {
            for nonce in 0..depth {
                pool.add(tx(sender, nonce as u64, 10)).unwrap();
            }
        }
        assert_eq!(pool.len(), 43);
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"1\"} 1"));
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"2-4\"} 4"));
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"5-16\"} 21"));
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"17+\"} 17"));

        // The gauge is set, not accumulated: a later flush after removals
        // republishes the smaller depths, and the sum invariant holds. The
        // 17-deep sender drops to depth 1 and moves buckets.
        pool.prune_below_nonce(&Address::new([5u8; 20]), 16);
        pool.flush_events(&metrics);
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"17+\"} 0"));
        assert!(rendered.contains("subhost_mempool_sender_depth{bucket=\"1\"} 2"));
        assert_eq!(pool.len(), 27);
    }
}
