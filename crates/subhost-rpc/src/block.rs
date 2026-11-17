//! Block selection, sealing, persistence, and acceptance.

use crate::state::{lock, read, write, BlockEvent, RpcError, RpcState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use subhost_consensus::ValidatorRegistry;
use subhost_core::{
    tx_root_of, Address, Block, BlockHeader, Gas, Hash, Receipt, ReceiptStatus, Transaction,
    ValidatorSignature, BLOCK_GAS_LIMIT,
};
use subhost_crypto::{BLSScheme, PrivateKey};
use subhost_mempool::Mempool;
use subhost_state::State;
use subhost_storage::{receipt_commitment, receipts_root, LedgerSnapshot};

/// The validator identity a node signs blocks with.
///
/// Construction checks the key against the registry, so a node cannot start
/// signing as a validator it does not hold the key for, or with a key that does
/// not belong to the address it claims.
pub struct BlockAuthority {
    validators: ValidatorRegistry,
    producer: Address,
    signing_key: PrivateKey,
}

impl BlockAuthority {
    /// Bind a producer address and BLS secret to a validator registry.
    ///
    /// Fails with [`RpcError::UnauthorizedProducer`] when the producer is not
    /// registered or when the secret's public key is not the registered one.
    pub fn new(
        validators: ValidatorRegistry,
        producer: Address,
        signing_key: PrivateKey,
    ) -> Result<Self, RpcError> {
        let Some(registered) = validators.public_key(&producer) else {
            return Err(RpcError::UnauthorizedProducer);
        };
        if *registered != BLSScheme::public_key(&signing_key) {
            return Err(RpcError::UnauthorizedProducer);
        }
        Ok(Self { validators, producer, signing_key })
    }

    pub fn producer(&self) -> Address {
        self.producer
    }

    pub fn validators(&self) -> &ValidatorRegistry {
        &self.validators
    }
}

impl RpcState {
    /// Submit a transaction: admit it to a copy of the pool, then seal the next
    /// block with every pending transaction that is currently executable.
    ///
    /// The live pool is only replaced after the ledger write succeeds, so a
    /// failed commit leaves admission, replacement, and eviction entirely undone.
    pub fn submit_transaction(&self, tx: Transaction) -> Result<Hash, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let requested_hash = tx.hash();

        let mut candidate_pool = read(&self.mempool).clone();
        let admission = candidate_pool
            .admit(tx)
            .map_err(|error| RpcError::TransactionRejected(error.to_string()))?;
        if !admission.accepted {
            return Err(RpcError::TransactionRejected(
                "a transaction with this sender and nonce already has an equal or higher gas price"
                    .into(),
            ));
        }
        debug_assert_eq!(admission.hash, requested_hash);

        let receipts = self.seal_block(&mut candidate_pool)?;
        if !receipts.contains_key(&requested_hash) {
            return Err(RpcError::TransactionRejected(
                "transaction is not executable against the current state".into(),
            ));
        }

        *write(&self.mempool) = candidate_pool;
        Ok(requested_hash)
    }

    /// Execute one pending transaction and commit it with the rest of the next
    /// block's executable transactions.
    pub fn include_transaction(&self, tx_hash: &Hash) -> Result<Receipt, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let mut candidate_pool = read(&self.mempool).clone();
        if !candidate_pool.contains(tx_hash) {
            return Err(RpcError::InvalidTxHash);
        }
        let receipts = self.seal_block(&mut candidate_pool)?;
        let receipt = receipts.get(tx_hash).cloned().ok_or(RpcError::InvalidTxHash)?;
        *write(&self.mempool) = candidate_pool;
        Ok(receipt)
    }

    /// Transactions the producer may include, in deterministic order.
    ///
    /// Senders are visited in address order and each contributes the nonce
    /// continuous prefix that fits in the remaining block gas. Execution stops at
    /// the first rejected transaction for a sender, so a block can never skip a
    /// nonce. [`State::apply_transaction`] validates before mutating, so a reject
    /// leaves `state` untouched.
    fn select_transactions(&self, pool: &Mempool, state: &mut State) -> Vec<Transaction> {
        let mut senders: Vec<Address> = pool.pending().iter().map(|tx| tx.from).collect();
        senders.sort_unstable();
        senders.dedup();

        let mut selected = Vec::new();
        let mut gas_used: Gas = 0;
        for sender in senders {
            for tx in pool.ready_for(&sender, state.nonce(&sender)) {
                let Some(next_gas) = gas_used.checked_add(tx.gas_limit) else { break };
                if next_gas > BLOCK_GAS_LIMIT {
                    break;
                }
                if state.apply_transaction(&tx).is_err() {
                    break;
                }
                gas_used = next_gas;
                selected.push(tx);
            }
        }
        selected
    }

    /// Seal every executable pending transaction into one block.
    ///
    /// On success the pool has the mined transactions removed and every other
    /// in-memory view has been swapped; on error nothing is published. The caller
    /// installs the mutated pool only after this returns `Ok`.
    fn seal_block(&self, pool: &mut Mempool) -> Result<HashMap<Hash, Receipt>, RpcError> {
        let mut candidate_state = read(&self.state).clone();
        let transactions = self.select_transactions(pool, &mut candidate_state);
        if transactions.is_empty() {
            return Ok(HashMap::new());
        }

        let previous_block = read(&self.blocks).last().cloned();
        let height = match previous_block.as_ref() {
            Some(block) => block.header.height.checked_add(1).ok_or(RpcError::Internal)?,
            None => 1,
        };
        let parent_hash = previous_block.as_ref().map_or(Hash::ZERO, Block::hash);

        let mut hashes = Vec::with_capacity(transactions.len());
        let mut commitments = Vec::with_capacity(transactions.len());
        let mut gas_used: Gas = 0;
        for (index, tx) in transactions.iter().enumerate() {
            gas_used = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            hashes.push(tx.hash());
            commitments.push(receipt_commitment(
                tx.hash(),
                height,
                u32::try_from(index).map_err(|_| RpcError::Internal)?,
                tx.gas_limit,
                gas_used,
            ));
        }

        let block = {
            let validator = self.authority.as_ref().map_or(Address::ZERO, |a| a.producer);
            let mut block = Block {
                header: BlockHeader {
                    version: 1,
                    chain_id: self.chain_id,
                    height,
                    timestamp: subhost_core::unix_timestamp(),
                    parent_hash,
                    state_root: candidate_state.root(),
                    tx_root: tx_root_of(&hashes),
                    receipt_root: receipts_root(&commitments),
                    // A producer without a configured authority has no identity to
                    // attest with, which keeps single-node behaviour unchanged.
                    validator,
                    gas_used,
                    gas_limit: BLOCK_GAS_LIMIT,
                    extra_data: Vec::new(),
                },
                transactions,
                signatures: Vec::new(),
            };
            if let Some(authority) = &self.authority {
                // The payload commits to the block hash, so the header must be
                // final before it is signed.
                let signature = BLSScheme::sign(&authority.signing_key, &block.signing_payload());
                block.signatures.push(ValidatorSignature {
                    validator: authority.producer,
                    signature: BLSScheme::encode_signature(&signature),
                });
            }
            block
        };

        let block_hash = block.hash();
        let mut receipts = HashMap::with_capacity(block.transactions.len());
        let mut cumulative_gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used =
                cumulative_gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            receipts.insert(
                tx.hash(),
                Receipt {
                    tx_hash: tx.hash(),
                    block_hash,
                    block_height: height,
                    transaction_index: u32::try_from(index).map_err(|_| RpcError::Internal)?,
                    gas_used: tx.gas_limit,
                    cumulative_gas_used,
                    status: ReceiptStatus::Success,
                    logs: Vec::new(),
                    contract_address: None,
                },
            );
        }

        let mut next_blocks = read(&self.blocks).clone();
        next_blocks.push(block.clone());
        let mut next_receipts = read(&self.receipts).clone();
        for (hash, receipt) in &receipts {
            next_receipts.insert(*hash, receipt.clone());
        }

        // Durability before visibility: if this fails nothing is swapped in.
        self.persist(&candidate_state, &next_blocks, &next_receipts)?;

        for tx in &block.transactions {
            pool.remove(&tx.hash());
        }
        *write(&self.state) = candidate_state;
        *write(&self.blocks) = next_blocks;
        *write(&self.receipts) = next_receipts;
        self.block_height.store(height, Ordering::SeqCst);
        // A send fails only when no receiver is attached, which is normal.
        let _ = self.block_events.send(BlockEvent::Sealed(block));
        Ok(receipts)
    }

    /// Accept a block produced elsewhere (a peer or the consensus driver).
    ///
    /// The block is fully re-validated before any state change: parent, height,
    /// chain, gas accounting, every transaction signature, both commitments, the
    /// state transition, and the resulting state root. Receipts are derived
    /// locally and never taken from the wire. Block signatures are carried but
    /// not verified yet; S1.3 adds that.
    pub fn accept_block(&self, block: Block) -> Result<Vec<Receipt>, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let previous = read(&self.blocks).last().cloned();
        let expected_height = match previous.as_ref() {
            Some(previous) => previous.header.height.checked_add(1).ok_or(RpcError::Internal)?,
            None => 1,
        };
        let expected_parent = previous.as_ref().map_or(Hash::ZERO, Block::hash);

        if block.header.height != expected_height
            || block.header.parent_hash != expected_parent
            || block.header.chain_id != self.chain_id
            || block.header.gas_limit != BLOCK_GAS_LIMIT
            || block.transactions.is_empty()
        {
            return Err(RpcError::BlockRejected(format!(
                "block does not extend this chain at height {expected_height}"
            )));
        }

        let mut candidate_state = read(&self.state).clone();
        let mut seen = HashSet::with_capacity(block.transactions.len());
        let mut hashes = Vec::with_capacity(block.transactions.len());
        let mut commitments = Vec::with_capacity(block.transactions.len());
        let mut gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            if tx.chain_id != self.chain_id || !tx.verify_signature() || !seen.insert(tx.hash()) {
                return Err(RpcError::BlockRejected(format!(
                    "invalid transaction {index} in block {}",
                    block.header.height
                )));
            }
            let next_gas = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            if next_gas > block.header.gas_limit {
                return Err(RpcError::BlockRejected(format!(
                    "block {} uses more than its {} gas limit",
                    block.header.height, block.header.gas_limit
                )));
            }
            candidate_state
                .apply_transaction(tx)
                .map_err(|error| RpcError::BlockRejected(error.to_string()))?;
            gas_used = next_gas;
            hashes.push(tx.hash());
            commitments.push(receipt_commitment(
                tx.hash(),
                block.header.height,
                u32::try_from(index).map_err(|_| RpcError::Internal)?,
                tx.gas_limit,
                gas_used,
            ));
        }

        if gas_used != block.header.gas_used
            || block.header.tx_root != tx_root_of(&hashes)
            || block.header.receipt_root != receipts_root(&commitments)
            || candidate_state.root() != block.header.state_root
        {
            return Err(RpcError::BlockRejected(format!(
                "block {} commitments or state transition do not verify",
                block.header.height
            )));
        }

        let block_hash = block.hash();
        let mut receipts = Vec::with_capacity(block.transactions.len());
        let mut cumulative_gas_used: Gas = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            cumulative_gas_used =
                cumulative_gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            receipts.push(Receipt {
                tx_hash: tx.hash(),
                block_hash,
                block_height: block.header.height,
                transaction_index: u32::try_from(index).map_err(|_| RpcError::Internal)?,
                gas_used: tx.gas_limit,
                cumulative_gas_used,
                status: ReceiptStatus::Success,
                logs: Vec::new(),
                contract_address: None,
            });
        }

        let mut next_blocks = read(&self.blocks).clone();
        next_blocks.push(block.clone());
        let mut next_receipts = read(&self.receipts).clone();
        for receipt in &receipts {
            next_receipts.insert(receipt.tx_hash, receipt.clone());
        }

        self.persist(&candidate_state, &next_blocks, &next_receipts)?;

        {
            let mut pool = write(&self.mempool);
            for tx in &block.transactions {
                pool.remove(&tx.hash());
                pool.prune_below_nonce(&tx.from, candidate_state.nonce(&tx.from));
            }
        }
        *write(&self.state) = candidate_state;
        *write(&self.blocks) = next_blocks;
        *write(&self.receipts) = next_receipts;
        self.block_height.store(block.header.height, Ordering::SeqCst);
        let _ = self.block_events.send(BlockEvent::Accepted(block));
        Ok(receipts)
    }

    pub(crate) fn persist(
        &self,
        state: &State,
        blocks: &[Block],
        receipts: &HashMap<Hash, Receipt>,
    ) -> Result<(), RpcError> {
        // Sort receipts by height so the persisted file is byte-stable.
        let mut ordered: Vec<Receipt> = receipts.values().cloned().collect();
        ordered.sort_by_key(|receipt| receipt.block_height);
        self.store.persist(&LedgerSnapshot {
            chain_id: self.chain_id,
            state: state.snapshot(),
            blocks: blocks.to_vec(),
            receipts: ordered,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::methods::DEFAULT_TX_GAS_LIMIT;
    use crate::state::read;
    use crate::test_support::{funded_state, signed_transfer, signing_key};
    use subhost_core::Amount;

    /// A one-validator authority for `secret`, with a fixed producer address.
    fn authority(secret: &PrivateKey) -> (BlockAuthority, Address) {
        let producer = Address::new([9; 20]);
        let public = BLSScheme::public_key(secret);
        let mut registry = ValidatorRegistry::new();
        registry.register(producer, public, &BLSScheme::proof_of_possession(secret)).unwrap();
        let authority = BlockAuthority::new(registry, producer, secret.clone()).unwrap();
        (authority, producer)
    }

    #[test]
    fn authority_signs_the_block_it_produces() {
        let tx_key = signing_key(7);
        let (bls_secret, _) = BLSScheme::keygen();
        let (authority, producer) = authority(&bls_secret);
        let state = funded_state(&tx_key, 1_000_000).with_authority(authority);

        state.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();

        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.header.validator, producer);
        assert_eq!(block.signatures.len(), 1);
        let signature = &block.signatures[0];
        assert_eq!(signature.validator, producer);
        let decoded = BLSScheme::decode_signature(&signature.signature)
            .expect("a produced block must carry a decodable BLS signature");
        assert!(
            BLSScheme::verify(
                &BLSScheme::public_key(&bls_secret),
                &block.signing_payload(),
                &decoded
            ),
            "the signature must cover the block signing payload"
        );
    }

    #[test]
    fn no_authority_keeps_single_node_blocks_unsigned() {
        let tx_key = signing_key(7);
        let state = funded_state(&tx_key, 1_000_000);
        state.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();

        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.header.validator, Address::ZERO);
        assert!(block.signatures.is_empty());
    }

    #[test]
    fn authority_rejects_an_unknown_producer_or_a_foreign_key() {
        let (bls_secret, _) = BLSScheme::keygen();
        let (other_secret, _) = BLSScheme::keygen();
        let public = BLSScheme::public_key(&bls_secret);
        let proof = BLSScheme::proof_of_possession(&bls_secret);
        let producer = Address::new([9; 20]);
        let mut registry = ValidatorRegistry::new();
        registry.register(producer, public, &proof).unwrap();

        // The registry is valid, but it does not contain the claimed producer.
        let outsider = Address::new([10; 20]);
        assert!(matches!(
            BlockAuthority::new(registry.clone(), outsider, bls_secret.clone()),
            Err(RpcError::UnauthorizedProducer)
        ));

        // The producer is registered, but this secret is somebody else's.
        assert!(matches!(
            BlockAuthority::new(registry, producer, other_secret),
            Err(RpcError::UnauthorizedProducer)
        ));
    }

    #[test]
    fn producer_executes_transfer_and_records_receipt() {
        let key = signing_key(7);
        let state = funded_state(&key, 100_000);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());

        let hash = state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        assert_eq!(state.balance(&Address::new([2; 20])), 10);
        assert_eq!(state.nonce(&sender), 1);
        assert_eq!(state.pending_count(), 0, "mined transaction must leave the pool");

        let receipt = state.receipt(&hash).unwrap();
        assert_eq!(receipt.block_height, 1);
        assert_eq!(receipt.status, ReceiptStatus::Success);
        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.header.parent_hash, Hash::ZERO);
        assert_eq!(block.header.tx_root, tx_root_of(&[hash]));
    }

    #[test]
    fn consecutive_transfers_chain_blocks() {
        let key = signing_key(7);
        let state = funded_state(&key, 1_000_000);
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        state.submit_transaction(signed_transfer(&key, 1, 20)).unwrap();

        assert_eq!(state.block_height(), 2);
        let first = state.block_at_height(1).unwrap();
        let second = state.block_at_height(2).unwrap();
        assert_eq!(second.header.parent_hash, first.hash());
        assert_eq!(state.balance(&Address::new([2; 20])), 30);
    }

    #[tokio::test]
    async fn block_events_distinguish_sealed_from_accepted_blocks() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let producer = funded_state(&key, 1_000_000);
        let mut events = producer.subscribe_blocks();

        producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        match events.recv().await.unwrap() {
            BlockEvent::Sealed(block) => assert_eq!(block.header.height, 1),
            other => panic!("expected a sealed block, got {other:?}"),
        }

        let follower = RpcState::in_memory(1).unwrap();
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        let mut follower_events = follower.subscribe_blocks();
        follower.accept_block(producer.block_at_height(1).unwrap()).unwrap();
        match follower_events.recv().await.unwrap() {
            BlockEvent::Accepted(block) => assert_eq!(block.header.height, 1),
            other => panic!("expected an accepted block, got {other:?}"),
        }
    }

    #[test]
    fn one_block_carries_every_executable_transaction() {
        let alice = signing_key(7);
        let bob = signing_key(8);
        let state = RpcState::in_memory(1).unwrap();
        state
            .seed_accounts([
                (Address::from_public_key(&alice.verifying_key().to_bytes()), 100_000),
                (Address::from_public_key(&bob.verifying_key().to_bytes()), 100_000),
            ])
            .unwrap();

        // Park Alice's transaction without asking the producer for a block.
        let alice_tx = signed_transfer(&alice, 0, 10);
        state.mempool.write().unwrap().add(alice_tx.clone()).unwrap();

        let bob_tx = signed_transfer(&bob, 0, 20);
        let hash = state.submit_transaction(bob_tx.clone()).unwrap();
        assert_eq!(hash, bob_tx.hash());

        assert_eq!(state.block_height(), 1);
        let block = state.block_at_height(1).unwrap();
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(state.pending_count(), 0);
        assert_eq!(block.header.gas_used, 2 * DEFAULT_TX_GAS_LIMIT);

        let receipts: Vec<Receipt> = block
            .transactions
            .iter()
            .map(|tx| state.receipt(&tx.hash()).expect("every included transaction has a receipt"))
            .collect();
        let indexes: Vec<u32> = receipts.iter().map(|receipt| receipt.transaction_index).collect();
        assert_eq!(indexes, vec![0, 1]);
        assert_eq!(receipts[0].cumulative_gas_used, DEFAULT_TX_GAS_LIMIT);
        assert_eq!(receipts[1].cumulative_gas_used, 2 * DEFAULT_TX_GAS_LIMIT);
    }

    #[test]
    fn a_nonce_gap_blocks_later_transactions_of_the_same_sender() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = funded_state(&key, 1_000_000);

        // Nonce 1 cannot execute before nonce 0, so the submission is rejected
        // and leaves no trace in the pool.
        let gap = signed_transfer(&key, 1, 10);
        assert!(matches!(
            state.submit_transaction(gap.clone()),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.pending_count(), 0);

        // Once nonce 0 arrives, the contiguous prefix of both mines in order.
        state.mempool.write().unwrap().add(gap).unwrap();
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        assert_eq!(state.block_at_height(1).unwrap().transactions.len(), 2);
        assert_eq!(state.nonce(&sender), 2);
        assert_eq!(state.balance(&Address::new([2; 20])), 20);
    }

    #[test]
    fn accept_block_replays_a_peer_block_and_rejects_tampering() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let producer = funded_state(&key, 1_000_000);
        let hash = producer.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        let block = producer.block_at_height(1).unwrap();
        assert_eq!(block.transactions[0].hash(), hash);

        let follower = RpcState::in_memory(1).unwrap();
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        // The same transaction reached the follower through gossip; accepting the
        // block must prune it from the pool.
        follower.mempool.write().unwrap().add(block.transactions[0].clone()).unwrap();

        let receipts = follower.accept_block(block.clone()).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(follower.block_height(), 1);
        assert_eq!(follower.block_count(), 1);
        assert_eq!(follower.state_root(), block.header.state_root);
        assert_eq!(follower.balance(&Address::new([2; 20])), 10);
        assert_eq!(follower.nonce(&sender), 1);
        assert_eq!(follower.pending_count(), 0);

        // The same height cannot be replayed on top of itself.
        assert!(matches!(follower.accept_block(block.clone()), Err(RpcError::BlockRejected(_))));

        // A mutated transaction invalidates its signature (and every root).
        let mut tampered = block.clone();
        tampered.transactions[0].value += 1;
        let other = RpcState::in_memory(1).unwrap();
        other.seed_accounts([(sender, 1_000_000)]).unwrap();
        assert!(matches!(other.accept_block(tampered), Err(RpcError::BlockRejected(_))));
        assert_eq!(other.block_height(), 0);

        // Even a well-formed block fails when its declared state root is wrong.
        let mut bad_state_root = block;
        bad_state_root.header.state_root = Hash::ZERO;
        let third = RpcState::in_memory(1).unwrap();
        third.seed_accounts([(sender, 1_000_000)]).unwrap();
        assert!(matches!(third.accept_block(bad_state_root), Err(RpcError::BlockRejected(_))));
        assert_eq!(third.block_height(), 0);
    }

    #[test]
    fn rejects_insufficient_balance_without_mutating_state() {
        let key = signing_key(7);
        let state = RpcState::in_memory(1).unwrap();
        assert!(matches!(
            state.submit_transaction(signed_transfer(&key, 0, 10)),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.block_count(), 0);
        assert_eq!(state.pending_count(), 0, "rejected transaction must not linger");
    }

    #[test]
    fn rejects_gas_above_the_block_limit_without_commit() {
        let key = signing_key(7);
        let state = funded_state(&key, Amount::MAX / 2);
        let mut tx = signed_transfer(&key, 0, 10);
        tx.gas_limit = BLOCK_GAS_LIMIT + 1;
        assert!(matches!(state.submit_transaction(tx), Err(RpcError::TransactionRejected(_))));
        assert_eq!(state.block_height(), 0);
        assert!(state.pending_count() == 0);
    }

    #[test]
    fn failed_replacement_restores_the_previous_pending_transaction() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        // Unfunded: the replacement will fail during execution.
        let state = RpcState::in_memory(1).unwrap();
        let original = signed_transfer(&key, 0, 10);
        state.mempool.write().unwrap().add(original.clone()).unwrap();

        let mut replacement = original.clone();
        replacement.gas_price = 2;
        assert!(state.submit_transaction(replacement).is_err());

        let (_, restored) = read(&state.mempool)
            .get_by_sender_nonce(&sender, 0)
            .expect("the original transaction must be back in the pool");
        assert_eq!(restored.gas_price, original.gas_price);
    }
}
