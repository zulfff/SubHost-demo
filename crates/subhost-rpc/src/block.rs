//! Block selection, sealing, persistence, and acceptance.

#[cfg(test)]
#[path = "proposal_budget_tests.rs"]
mod proposal_budget_tests;

use crate::state::{lock, read, write, BlockEvent, RpcError, RpcState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use subhost_consensus::{ConsensusConfig, HotStuff, QuorumCertificate, ValidatorRegistry};
use subhost_core::{
    encode_canonical, settle_block_fees, tx_root_of, Address, Amount, Block, BlockHeader,
    FeeContext, Gas, Hash, Receipt, ReceiptStatus, Transaction, ValidatorSignature,
    FEE_HEADER_VERSION,
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

/// A block built against a candidate state, before any commit.
struct BuiltCandidate {
    block: Block,
    /// State after the block's transactions were applied.
    state: State,
}

/// Settle one block's fees against the state it produced.
///
/// Every executed transaction debited its full `gas_price * gas_limit`, and
/// that total must equal the settlement split into burned base fee and
/// producer tip (`S6.2` consumes `tx.gas_limit` per transaction, as committed
/// in receipts). The tip is credited once, after execution; `None` burns it
/// in the explicit validatorless demo mode.
fn settle_fees(
    context: &FeeContext,
    state: &mut State,
    transactions: &[Transaction],
    debited: Amount,
    producer: Option<Address>,
) -> Result<(), String> {
    let settlement = settle_block_fees(context, transactions.iter().map(|tx| (tx, tx.gas_limit)))
        .map_err(|error| format!("fee settlement failed: {error}"))?;
    if settlement.total != debited {
        return Err(format!(
            "execution debited {debited} but the block settles {}",
            settlement.total
        ));
    }
    if settlement.burned.checked_add(settlement.priority) != Some(settlement.total) {
        return Err("fee settlement does not conserve value".into());
    }
    state
        .credit_block_priority(producer, settlement.priority)
        .map_err(|error| format!("priority credit failed: {error}"))
}

impl RpcState {
    /// Header version this chain produces and accepts.
    fn header_version(&self) -> u32 {
        if self.fee_policy.is_some() {
            FEE_HEADER_VERSION
        } else {
            1
        }
    }

    /// Fee parameters for the block after `parent`; `None` on a legacy chain.
    fn fee_context(&self, parent: Option<&Block>) -> Result<Option<FeeContext>, String> {
        let Some(policy) = &self.fee_policy else {
            return Ok(None);
        };
        let context = match parent {
            None => policy.initial_context(self.block_gas_limit),
            Some(parent) => policy.next_context(&parent.header, self.block_gas_limit),
        }
        .map_err(|error| format!("cannot derive the next base fee: {error}"))?;
        Ok(Some(context))
    }

    /// Reject an admission bid that cannot pay the next block's base fee.
    ///
    /// Local admission policy only: consensus validity is enforced per block by
    /// [`Self::validate_candidate`], and pool entries admitted before a fee rise
    /// stay parked rather than being deleted.
    fn check_next_base_fee(&self, tx: &Transaction) -> Result<(), RpcError> {
        let Some(context) =
            self.fee_context(read(&self.blocks).last()).map_err(|_| RpcError::Internal)?
        else {
            return Ok(());
        };
        if tx.gas_price < context.base_fee_per_gas {
            return Err(RpcError::TransactionRejected(format!(
                "gas price {} is below the next block's {} base fee",
                tx.gas_price, context.base_fee_per_gas
            )));
        }
        Ok(())
    }
    /// Admit an executable transaction to a copy of the pool.
    ///
    /// With validators, admission leaves the transaction pending for a certified
    /// block and returns its hash before a receipt exists. Demo chains seal
    /// synchronously and replace the live pool only after persistence succeeds.
    pub fn submit_transaction(&self, tx: Transaction) -> Result<Hash, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let requested_hash = tx.hash();

        // Reject a bid below the next block's base fee before it touches the
        // pool. This is an admission policy, not a consensus rule: entries
        // admitted earlier stay parked when the fee rises.
        self.check_next_base_fee(&tx)?;

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

        // With a validator set configured, this node does not commit on the RPC
        // thread: the producer proposes the block and it commits only after a
        // quorum. Dry-run the build so an unexecutable transaction is still
        // rejected here, then leave it for the producer.
        if self.validators.is_some() {
            let executable = self.build_candidate(&candidate_pool)?.is_some_and(|candidate| {
                candidate.block.transactions.iter().any(|tx| tx.hash() == requested_hash)
            });
            if !executable {
                return Err(RpcError::TransactionRejected(
                    "transaction is not executable against the current state".into(),
                ));
            }
            *write(&self.mempool) = candidate_pool;
            return Ok(requested_hash);
        }

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
        if self.validators.is_some() {
            return Err(RpcError::TransactionRejected(
                "direct inclusion is disabled while a validator set is configured; the producer will include the transaction".into(),
            ));
        }
        let mut candidate_pool = read(&self.mempool).clone();
        if !candidate_pool.contains(tx_hash) {
            return Err(RpcError::InvalidTxHash);
        }
        // A held entry may have become underpriced since admission. Refuse
        // before sealing rather than committing an empty block and then
        // reporting a missing receipt.
        if let Some(context) =
            self.fee_context(read(&self.blocks).last()).map_err(|_| RpcError::Internal)?
        {
            if let Some(tx) = candidate_pool.pending().iter().find(|tx| tx.hash() == *tx_hash) {
                if tx.gas_price < context.base_fee_per_gas {
                    return Err(RpcError::TransactionRejected(format!(
                        "gas price {} is below the {} base fee; the transaction stays pending",
                        tx.gas_price, context.base_fee_per_gas
                    )));
                }
            }
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
    /// nonce. On a fee-enabled chain the first transaction paying below the
    /// next base fee parks that sender instead of skipping its nonce; the
    /// transaction stays pending for a later block. [`State::apply_transaction`]
    /// validates before mutating, so a reject leaves `state` untouched.
    fn select_transactions(
        &self,
        pool: &Mempool,
        state: &mut State,
        fee: Option<&FeeContext>,
    ) -> Vec<Transaction> {
        let mut senders: Vec<Address> = pool.pending().iter().map(|tx| tx.from).collect();
        senders.sort_unstable();
        senders.dedup();

        let mut selected = Vec::new();
        let mut gas_used: Gas = 0;
        // Fixed-width header/signature framing is below this conservative
        // reserve. Transaction bodies are measured using the actual codec.
        let transaction_budget = self.proposal_bytes_limit.map(|limit| limit.saturating_sub(512));
        let mut selected_bytes = 0usize;
        for sender in senders {
            for tx in pool.ready_for(&sender, state.nonce(&sender)) {
                if fee.is_some_and(|fee| tx.gas_price < fee.base_fee_per_gas) {
                    break;
                }
                let Some(next_gas) = gas_used.checked_add(tx.gas_limit) else { break };
                if next_gas > self.block_gas_limit {
                    break;
                }
                let Some(next_bytes) = selected_bytes.checked_add(encode_canonical(&tx).len())
                else {
                    break;
                };
                if transaction_budget.is_some_and(|budget| next_bytes > budget) {
                    break;
                }
                if state.apply_transaction(&tx).is_err() {
                    break;
                }
                gas_used = next_gas;
                selected_bytes = next_bytes;
                selected.push(tx);
            }
        }
        selected
    }

    /// The next block this producer would seal, without committing it.
    ///
    /// The block is built and signed against a snapshot of the current state,
    /// but nothing is persisted, the mempool is untouched, and no event is
    /// emitted. A consensus producer hands this block to the driver and commits
    /// it through [`RpcState::commit_qc`] only after a quorum certifies it.
    ///
    /// Returns `None` only when a legacy chain has no executable pending
    /// transaction. A fee-enabled chain returns an empty block so idle blocks
    /// still move the base fee.
    pub fn build_block(&self) -> Result<Option<Block>, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let pool = read(&self.mempool).clone();
        Ok(self.build_candidate(&pool)?.map(|candidate| candidate.block))
    }

    /// Build the next block from `pool` and the state it would produce.
    ///
    /// The caller must hold the commit lock. `seal_block` uses the returned
    /// state to persist the commit; `build_block` throws it away because a
    /// proposal is not a commit.
    fn build_candidate(&self, pool: &Mempool) -> Result<Option<BuiltCandidate>, RpcError> {
        let mut candidate_state = read(&self.state).clone();
        let previous_block = read(&self.blocks).last().cloned();
        let fee_context =
            self.fee_context(previous_block.as_ref()).map_err(|_| RpcError::Internal)?;
        let transactions =
            self.select_transactions(pool, &mut candidate_state, fee_context.as_ref());
        // A fee-enabled chain seals empty certified blocks at the block
        // cadence so an idle or underpriced chain can still lower its base
        // fee; a legacy chain keeps skipping empty selections.
        if transactions.is_empty() && fee_context.is_none() {
            return Ok(None);
        }

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

        let validator = self.authority.as_ref().map_or(Address::ZERO, |a| a.producer);
        // Settlement runs before the state root is taken, so the recorded root
        // commits to the producer's tip. `Address::ZERO` marks the explicit
        // validatorless demo mode, which burns the tip instead of crediting it.
        let producer = (validator != Address::ZERO).then_some(validator);
        let base_fee_per_gas = match &fee_context {
            Some(context) => {
                let debited = transactions.iter().try_fold(0u128, |sum, tx| {
                    tx.fee().and_then(|fee| sum.checked_add(fee)).ok_or(RpcError::Internal)
                })?;
                settle_fees(context, &mut candidate_state, &transactions, debited, producer)
                    .map_err(|_| RpcError::Internal)?;
                context.base_fee_per_gas
            }
            None => 0,
        };

        let block = {
            let mut block = Block {
                header: BlockHeader {
                    version: self.header_version(),
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
                    gas_limit: self.block_gas_limit,
                    extra_data: Vec::new(),
                    base_fee_per_gas,
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

        Ok(Some(BuiltCandidate { block, state: candidate_state }))
    }

    /// Seal every executable pending transaction into one block.
    ///
    /// On success the pool has the mined transactions removed and every other
    /// in-memory view has been swapped; on error nothing is published. The caller
    /// installs the mutated pool only after this returns `Ok`.
    fn seal_block(&self, pool: &mut Mempool) -> Result<HashMap<Hash, Receipt>, RpcError> {
        let Some(BuiltCandidate { block, state: candidate_state }) = self.build_candidate(pool)?
        else {
            return Ok(HashMap::new());
        };

        let block_hash = block.hash();
        let height = block.header.height;
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

    /// Check that a peer block is signed by a registered validator.
    ///
    /// A node without a validator registry keeps the pre-S1 behaviour and accepts
    /// unsigned blocks. With one, the block must name a registered validator and
    /// carry exactly one signature from that validator over
    /// [`Block::signing_payload`]. Anything else is
    /// [`RpcError::UnauthorizedProducer`].
    fn verify_block_authority(&self, block: &Block) -> Result<(), RpcError> {
        let Some(validators) = &self.validators else {
            return Ok(());
        };
        let Some(public_key) = validators.public_key(&block.header.validator) else {
            return Err(RpcError::UnauthorizedProducer);
        };
        let [signature] = block.signatures.as_slice() else {
            return Err(RpcError::UnauthorizedProducer);
        };
        if signature.validator != block.header.validator {
            return Err(RpcError::UnauthorizedProducer);
        }
        let decoded = BLSScheme::decode_signature(&signature.signature)
            .ok_or(RpcError::UnauthorizedProducer)?;
        if !BLSScheme::verify(public_key, &block.signing_payload(), &decoded) {
            return Err(RpcError::UnauthorizedProducer);
        }
        Ok(())
    }

    /// Accept a block produced elsewhere (a peer or the consensus driver).
    ///
    /// The block is fully re-validated before any state change: parent, height,
    /// chain, gas accounting, producer authority, every transaction signature,
    /// both commitments, the state transition, and the resulting state root.
    /// Receipts are derived locally and never taken from the wire.
    pub fn accept_block(&self, block: Block) -> Result<Vec<Receipt>, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        self.verify_block_authority(&block)?;
        if self.validators.is_some() {
            return Err(RpcError::BlockRejected(
                "validator blocks require a quorum certificate".into(),
            ));
        }
        self.accept_validated(block)
    }

    /// Validate a proposal against the current tip without writing any state.
    pub fn validate_proposal(&self, block: &Block) -> Result<(), RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        self.validate_candidate(block).map(|_| ())
    }

    /// Persist a block only after its quorum certificate and state transition verify.
    pub fn commit_qc(
        &self,
        block: Block,
        qc: &QuorumCertificate,
    ) -> Result<Vec<Receipt>, RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        self.validate_final_qc(&block, qc)?;
        let receipts = self.accept_validated(block)?;
        self.committed_view.store(qc.view, Ordering::SeqCst);
        self.verified_view.store(true, Ordering::SeqCst);
        Ok(receipts)
    }

    /// Independently verify a certified next block without writing the ledger,
    /// changing the pool or releasing any state. Archive consumers use this
    /// exact commit boundary before recording a certified item.
    pub fn validate_certified_block(
        &self,
        block: &Block,
        qc: &QuorumCertificate,
    ) -> Result<(), RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        self.validate_final_qc(block, qc)?;
        self.validate_candidate(block).map(|_| ())
    }

    /// Recover verified view metadata from the current ledger tip's final QC.
    /// The certificate must match the actual retained tip and genesis namespace.
    pub fn restore_committed_qc(&self, qc: &QuorumCertificate) -> Result<(), RpcError> {
        let _commit_guard = lock(&self.commit_lock);
        let block = read(&self.blocks).last().cloned().ok_or_else(|| {
            RpcError::BlockRejected("genesis has no committed certificate".into())
        })?;
        if qc.block_hash != block.hash()
            || qc.view < block.header.height
            || qc.protocol_context != self.consensus_context
            || self.committed_view().is_some_and(|view| qc.view < view)
        {
            return Err(RpcError::BlockRejected(
                "tip certificate does not match restored history".into(),
            ));
        }
        self.verify_block_authority(&block)?;
        self.verify_final_qc(qc)?;
        self.committed_view.store(qc.view, Ordering::SeqCst);
        self.verified_view.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn validate_final_qc(&self, block: &Block, qc: &QuorumCertificate) -> Result<(), RpcError> {
        let committed_view = self.committed_view().ok_or_else(|| {
            RpcError::BlockRejected(
                "verified tip certificate is required before extending restored validator history"
                    .into(),
            )
        })?;
        if qc.block_hash != block.hash()
            || qc.view < block.header.height
            || qc.view <= committed_view
            || qc.protocol_context != self.consensus_context
        {
            return Err(RpcError::BlockRejected(
                "final certificate does not match the block, genesis or newer view".into(),
            ));
        }
        self.verify_final_qc(qc)
    }

    fn verify_final_qc(&self, qc: &QuorumCertificate) -> Result<(), RpcError> {
        let validators = self.validators.as_ref().ok_or_else(|| {
            RpcError::BlockRejected("a quorum commit requires a validator registry".into())
        })?;
        let mut config = ConsensusConfig::new(validators.len())
            .map_err(|error| RpcError::BlockRejected(error.to_string()))?;
        config.protocol_context = self.consensus_context;
        HotStuff::new(config, (**validators).clone())
            .validate_qc(qc)
            .map_err(|error| RpcError::BlockRejected(error.to_string()))
    }

    /// Validate before persistence; caller holds the commit lock.
    fn validate_candidate(&self, block: &Block) -> Result<State, RpcError> {
        if self.proposal_bytes_limit.is_some_and(|limit| encode_canonical(block).len() > limit) {
            return Err(RpcError::BlockRejected(
                "canonical block exceeds proposal/recovery budget".into(),
            ));
        }
        let previous = read(&self.blocks).last().cloned();
        let expected_height = match previous.as_ref() {
            Some(previous) => previous.header.height.checked_add(1).ok_or(RpcError::Internal)?,
            None => 1,
        };
        let expected_parent = previous.as_ref().map_or(Hash::ZERO, Block::hash);
        let fee_context = self
            .fee_context(previous.as_ref())
            .map_err(|error| RpcError::BlockRejected(format!("block fee: {error}")))?;
        let expected_base_fee = fee_context.as_ref().map_or(0, |context| context.base_fee_per_gas);

        if block.header.height != expected_height
            || block.header.parent_hash != expected_parent
            || block.header.chain_id != self.chain_id
            || block.header.version != self.header_version()
            || block.header.gas_limit != self.block_gas_limit
            || (self.fee_policy.is_none() && block.transactions.is_empty())
        {
            return Err(RpcError::BlockRejected(format!(
                "block does not extend this chain at height {expected_height}"
            )));
        }
        if block.header.base_fee_per_gas != expected_base_fee {
            return Err(RpcError::BlockRejected(format!(
                "block {} records base fee {} but the chain derives {expected_base_fee}",
                block.header.height, block.header.base_fee_per_gas
            )));
        }

        // Identity before execution: an unauthorized block must not reach the
        // state transition at all.
        self.verify_block_authority(block)?;

        let mut candidate_state = read(&self.state).clone();
        let mut seen = HashSet::with_capacity(block.transactions.len());
        let mut hashes = Vec::with_capacity(block.transactions.len());
        let mut commitments = Vec::with_capacity(block.transactions.len());
        let mut gas_used: Gas = 0;
        let mut debited: Amount = 0;
        for (index, tx) in block.transactions.iter().enumerate() {
            if tx.chain_id != self.chain_id || !tx.verify_signature() || !seen.insert(tx.hash()) {
                return Err(RpcError::BlockRejected(format!(
                    "invalid transaction {index} in block {}",
                    block.header.height
                )));
            }
            if let Some(context) = &fee_context {
                if tx.gas_price < context.base_fee_per_gas {
                    return Err(RpcError::BlockRejected(format!(
                        "transaction {index} in block {} pays {} below the {} base fee",
                        block.header.height, tx.gas_price, context.base_fee_per_gas
                    )));
                }
            }
            let next_gas = gas_used.checked_add(tx.gas_limit).ok_or(RpcError::Internal)?;
            if next_gas > block.header.gas_limit {
                return Err(RpcError::BlockRejected(format!(
                    "block {} uses more than its {} gas limit",
                    block.header.height, block.header.gas_limit
                )));
            }
            let fee = candidate_state
                .apply_transaction(tx)
                .map_err(|error| RpcError::BlockRejected(error.to_string()))?;
            debited = debited.checked_add(fee).ok_or(RpcError::Internal)?;
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

        if let Some(context) = &fee_context {
            // The recorded producer earns this block's tip; `Address::ZERO` is
            // the explicit validatorless mode that burns it.
            let producer =
                (block.header.validator != Address::ZERO).then_some(block.header.validator);
            settle_fees(context, &mut candidate_state, &block.transactions, debited, producer)
                .map_err(RpcError::BlockRejected)?;
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

        Ok(candidate_state)
    }

    /// Apply a validated block under the caller's commit lock.
    fn accept_validated(&self, block: Block) -> Result<Vec<Receipt>, RpcError> {
        let candidate_state = self.validate_candidate(&block)?;
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
    use ed25519_dalek::{Signer, SigningKey};
    use subhost_core::{
        Amount, FeePolicy, Nonce, TransactionSignature, TransactionType, BLOCK_GAS_LIMIT,
        FEE_CHANGE_DENOMINATOR, FEE_ELASTICITY_MULTIPLIER,
    };

    /// A one-validator authority for `secret`, with a fixed producer address.
    fn authority(secret: &PrivateKey) -> (BlockAuthority, Address) {
        let producer = Address::new([9; 20]);
        let public = BLSScheme::public_key(secret);
        let mut registry = ValidatorRegistry::new();
        registry.register(producer, public, &BLSScheme::proof_of_possession(secret)).unwrap();
        let authority = BlockAuthority::new(registry, producer, secret.clone()).unwrap();
        (authority, producer)
    }

    fn registry(entries: &[(Address, &PrivateKey)]) -> ValidatorRegistry {
        let mut registry = ValidatorRegistry::new();
        for (address, secret) in entries {
            registry
                .register(
                    *address,
                    BLSScheme::public_key(secret),
                    &BLSScheme::proof_of_possession(secret),
                )
                .unwrap();
        }
        registry
    }

    fn certificate(block: &Block, signers: &[(Address, &PrivateKey)]) -> QuorumCertificate {
        let view = block.header.height;
        let block_hash = block.hash();
        let payload = QuorumCertificate::payload_for(view, &block_hash);
        QuorumCertificate {
            protocol_context: Hash::ZERO,
            view,
            block_hash,
            signatures: signers
                .iter()
                .map(|(address, secret)| {
                    (*address, BLSScheme::encode_signature(&BLSScheme::sign(secret, &payload)))
                })
                .collect(),
        }
    }

    #[test]
    fn authority_signs_the_block_it_produces() {
        let tx_key = signing_key(7);
        let (bls_secret, _) = BLSScheme::keygen();
        let (authority, producer) = authority(&bls_secret);
        let state = funded_state(&tx_key, 1_000_000).with_authority(authority);

        state.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        let block = state.build_block().unwrap().unwrap();
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

    /// A producer and a follower that both know the two-validator registry.
    fn signed_block_and_follower() -> (Block, QuorumCertificate, RpcState) {
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        let (alice_secret, _) = BLSScheme::keygen();
        let (bob_secret, _) = BLSScheme::keygen();
        let alice = Address::new([9; 20]);
        let bob = Address::new([10; 20]);
        let registry = registry(&[(alice, &alice_secret), (bob, &bob_secret)]);

        let producer = funded_state(&tx_key, 1_000_000).with_authority(
            BlockAuthority::new(registry.clone(), alice, alice_secret.clone()).unwrap(),
        );
        producer.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        let block = producer.build_block().unwrap().unwrap();
        let qc = certificate(&block, &[(alice, &alice_secret), (bob, &bob_secret)]);

        let follower = RpcState::in_memory(1)
            .unwrap()
            .with_authority(BlockAuthority::new(registry, bob, bob_secret).unwrap());
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        (block, qc, follower)
    }

    #[test]
    fn admission_and_proposal_do_not_publish_state_or_bypass_quorum() {
        let tx_key = signing_key(7);
        let (secret, _) = BLSScheme::keygen();
        let (authority, _) = authority(&secret);
        let state = funded_state(&tx_key, 1_000_000).with_authority(authority);
        let root = state.state_root();
        let mut events = state.subscribe_blocks();
        let hash = state.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        let block = state.build_block().unwrap().unwrap();
        assert_eq!(state.block_height(), 0);
        assert_eq!(state.state_root(), root);
        assert!(state.receipt(&hash).is_none());
        assert_eq!(state.pending_count(), 1);
        assert!(events.try_recv().is_err());
        assert!(state.include_transaction(&hash).is_err());
        assert!(state.accept_block(block).is_err());
        assert_eq!(state.block_height(), 0);
    }

    #[test]
    fn invalid_certificates_leave_balances_receipts_and_height_unchanged() {
        let (block, qc, follower) = signed_block_and_follower();
        let mut insufficient = qc.clone();
        insufficient.signatures.pop();
        let mut duplicate = qc.clone();
        duplicate.signatures[1] = duplicate.signatures[0].clone();
        let mut wrong_hash = qc.clone();
        wrong_hash.block_hash = Hash::ZERO;
        let mut wrong_view = qc.clone();
        wrong_view.view += 1;
        let mut malformed = qc.clone();
        malformed.signatures[0].1 = vec![0xff; 96];
        let root = follower.state_root();
        for bad in [insufficient, duplicate, wrong_hash, wrong_view, malformed] {
            assert!(follower.commit_qc(block.clone(), &bad).is_err());
            assert_eq!(follower.block_height(), 0);
            assert_eq!(follower.state_root(), root);
            assert!(follower.receipt(&block.transactions[0].hash()).is_none());
        }
        follower.commit_qc(block, &qc).unwrap();
        assert_eq!(follower.block_height(), 1);
    }

    #[test]
    fn a_block_with_a_valid_producer_signature_is_accepted() {
        let (block, qc, follower) = signed_block_and_follower();
        follower.commit_qc(block, &qc).unwrap();
        assert_eq!(follower.block_height(), 1);
        assert_eq!(follower.block_count(), 1);
    }

    #[test]
    fn verification_needs_only_a_registry_not_a_signing_key() {
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        let (alice_secret, _) = BLSScheme::keygen();
        let alice = Address::new([9; 20]);
        let registry = registry(&[(alice, &alice_secret)]);

        let producer = funded_state(&tx_key, 1_000_000).with_authority(
            BlockAuthority::new(registry.clone(), alice, alice_secret.clone()).unwrap(),
        );
        producer.submit_transaction(signed_transfer(&tx_key, 0, 10)).unwrap();
        let block = producer.build_block().unwrap().unwrap();
        let qc = certificate(&block, &[(alice, &alice_secret)]);

        let follower = RpcState::in_memory(1).unwrap().with_validators(registry.clone());
        follower.seed_accounts([(sender, 1_000_000)]).unwrap();
        follower.commit_qc(block.clone(), &qc).unwrap();
        assert_eq!(follower.block_height(), 1);
        assert_eq!(follower.balance(&Address::new([2; 20])), 10);

        // The same registry on a fresh state still rejects an unsigned block.
        let strict = RpcState::in_memory(1).unwrap().with_validators(registry);
        strict.seed_accounts([(sender, 1_000_000)]).unwrap();
        let mut unsigned = block;
        unsigned.signatures.clear();
        assert!(matches!(strict.accept_block(unsigned), Err(RpcError::UnauthorizedProducer)));
    }

    #[test]
    fn a_block_without_a_valid_producer_signature_is_rejected() {
        let (block, _, follower) = signed_block_and_follower();

        let mut missing = block.clone();
        missing.signatures.clear();

        let mut unknown = block.clone();
        unknown.header.validator = Address::new([11; 20]);

        let mut wrong_signer = block.clone();
        wrong_signer.signatures[0].validator = Address::new([10; 20]);

        let mut tampered_header = block.clone();
        tampered_header.header.timestamp ^= 1;

        let mut malformed = block.clone();
        malformed.signatures[0].signature = vec![0xff; subhost_crypto::SIGNATURE_BYTES];

        let mut extra = block.clone();
        extra.signatures.push(extra.signatures[0].clone());

        for candidate in [missing, unknown, wrong_signer, tampered_header, malformed, extra] {
            assert!(matches!(
                follower.accept_block(candidate),
                Err(RpcError::UnauthorizedProducer)
            ));
            assert_eq!(follower.block_height(), 0, "a rejected block must not advance the tip");
        }
        assert_eq!(follower.block_count(), 0);
        assert_eq!(follower.pending_count(), 0);
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

    fn fee_policy() -> FeePolicy {
        FeePolicy {
            initial_base_fee_per_gas: 8,
            min_base_fee_per_gas: 1,
            max_base_fee_per_gas: Amount::MAX / u128::from(BLOCK_GAS_LIMIT),
            elasticity_multiplier: FEE_ELASTICITY_MULTIPLIER,
            change_denominator: FEE_CHANGE_DENOMINATOR,
        }
    }

    fn fee_state(key: &SigningKey, balance: Amount) -> RpcState {
        let state = RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap();
        let address = Address::from_public_key(&key.verifying_key().to_bytes());
        state.seed_accounts([(address, balance)]).unwrap();
        state
    }

    /// A signed transfer with an explicit bid, so it can be priced against the
    /// recorded base fee.
    fn priced_transfer(
        key: &SigningKey,
        nonce: Nonce,
        value: Amount,
        gas_price: Amount,
    ) -> Transaction {
        let public_key = key.verifying_key().to_bytes();
        let unsigned = Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: Address::from_public_key(&public_key),
            to: Some(Address::new([2; 20])),
            value,
            gas_price,
            gas_limit: DEFAULT_TX_GAS_LIMIT,
            data: Vec::new(),
            chain_id: 1,
            public_key,
            signature: TransactionSignature::EMPTY,
        };
        let signature = key.sign(&unsigned.signing_payload()).to_bytes();
        Transaction { signature: TransactionSignature::from_ed25519(&signature), ..unsigned }
    }

    #[test]
    fn fee_enabled_chains_seal_empty_blocks_and_lower_the_base_fee() {
        let state = RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap();
        let first = state.build_block().unwrap().expect("a v2 chain seals empty blocks");
        assert_eq!(first.header.version, FEE_HEADER_VERSION);
        assert_eq!(first.header.base_fee_per_gas, 8);
        assert!(first.transactions.is_empty());
        assert_eq!(first.header.gas_used, 0);
        state.validate_proposal(&first).unwrap();
        state.accept_block(first.clone()).unwrap();

        // Empty blocks sit below target, so the base fee falls by one.
        let second = state.build_block().unwrap().unwrap();
        assert_eq!(second.header.parent_hash, first.hash());
        assert_eq!(second.header.base_fee_per_gas, 7);
        state.accept_block(second.clone()).unwrap();
        assert_eq!(state.build_block().unwrap().unwrap().header.base_fee_per_gas, 6);
        // Every empty block leaves the state root untouched.
        assert_eq!(second.header.state_root, first.header.state_root);
    }

    #[test]
    fn fee_enabled_block_credits_the_producer_tip_once() {
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        let (alice_secret, _) = BLSScheme::keygen();
        let alice = Address::new([9; 20]);
        let registry = registry(&[(alice, &alice_secret)]);
        let producer =
            RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap().with_authority(
                BlockAuthority::new(registry, alice, alice_secret.clone()).unwrap(),
            );
        producer.seed_accounts([(sender, 1_000_000)]).unwrap();

        producer.submit_transaction(priced_transfer(&tx_key, 0, 10, 10)).unwrap();
        let block = producer.build_block().unwrap().unwrap();
        assert_eq!(block.header.base_fee_per_gas, 8);
        assert_eq!(block.header.gas_used, DEFAULT_TX_GAS_LIMIT);
        assert_eq!(producer.balance(&alice), 0, "the tip is not credited at build time");

        let qc = certificate(&block, &[(alice, &alice_secret)]);
        producer.commit_qc(block.clone(), &qc).unwrap();

        let gas = Amount::from(DEFAULT_TX_GAS_LIMIT);
        let tip = (10 - 8) * gas;
        assert_eq!(producer.balance(&alice), tip, "exactly the priority fee, once");
        assert_eq!(
            producer.balance(&sender),
            1_000_000 - 10 - 10 * gas,
            "the sender paid the full fee and the value"
        );
        // Value is conserved except for the burned base fee.
        assert_eq!(
            producer.balance(&alice)
                + producer.balance(&sender)
                + producer.balance(&Address::new([2; 20])),
            1_000_000 - 8 * gas,
            "only the base fee leaves the system"
        );
    }

    #[test]
    fn underpriced_transactions_are_rejected_at_admission_and_park_when_held() {
        let key = signing_key(7);
        let state = fee_state(&key, 1_000_000);
        // Admission refuses a bid below the next block's base fee.
        assert!(matches!(
            state.submit_transaction(priced_transfer(&key, 0, 10, 7)),
            Err(RpcError::TransactionRejected(_))
        ));
        assert_eq!(state.block_height(), 0);

        // A pool entry admitted before a fee rise is parked, not skipped, and
        // the chain still seals the empty block that lowers the fee back.
        state.mempool.write().unwrap().add(priced_transfer(&key, 0, 10, 7)).unwrap();
        let first = state.build_block().unwrap().unwrap();
        assert!(first.transactions.is_empty());
        state.accept_block(first).unwrap();

        let second = state.build_block().unwrap().unwrap();
        assert_eq!(second.header.base_fee_per_gas, 7);
        assert_eq!(second.transactions.len(), 1);
        assert_eq!(second.transactions[0].gas_price, 7);
    }

    #[test]
    fn fee_enabled_blocks_reject_a_wrong_base_fee_version_or_root() {
        let state = RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap();
        let block = state.build_block().unwrap().unwrap();
        let root = state.state_root();

        let mut wrong_fee = block.clone();
        wrong_fee.header.base_fee_per_gas = 1;
        assert!(matches!(state.accept_block(wrong_fee), Err(RpcError::BlockRejected(_))));

        // A legacy-version header cannot appear on a fee-enabled chain.
        let mut wrong_version = block.clone();
        wrong_version.header.version = 1;
        wrong_version.header.base_fee_per_gas = 0;
        assert!(matches!(state.accept_block(wrong_version), Err(RpcError::BlockRejected(_))));

        let mut wrong_root = block;
        wrong_root.header.state_root = Hash::ZERO;
        assert!(matches!(state.accept_block(wrong_root), Err(RpcError::BlockRejected(_))));

        assert_eq!(state.block_height(), 0);
        assert_eq!(state.state_root(), root);
    }

    #[test]
    fn validatorless_fee_blocks_burn_the_tip() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = fee_state(&key, 1_000_000);
        // Demo chains seal synchronously; the sealed block burned the tip.
        state.submit_transaction(priced_transfer(&key, 0, 10, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        let sealed = read(&state.blocks).last().cloned().unwrap();
        assert_eq!(sealed.header.validator, Address::ZERO);
        assert_eq!(sealed.header.base_fee_per_gas, 8);
        assert_eq!(state.balance(&Address::ZERO), 0, "no reward at the zero address");
        assert_eq!(
            state.balance(&sender),
            1_000_000 - 10 - 10 * Amount::from(DEFAULT_TX_GAS_LIMIT)
        );

        // An idle block keeps the zero address unrewarded too.
        let idle = state.build_block().unwrap().unwrap();
        assert!(idle.transactions.is_empty());
        state.accept_block(idle).unwrap();
        assert_eq!(state.balance(&Address::ZERO), 0);
    }

    #[test]
    fn fee_enabled_empty_blocks_commit_through_quorum_on_both_sides() {
        let (alice_secret, _) = BLSScheme::keygen();
        let alice = Address::new([9; 20]);
        let registry = registry(&[(alice, &alice_secret)]);
        let producer =
            RpcState::in_memory(1).unwrap().with_fee_policy(fee_policy()).unwrap().with_authority(
                BlockAuthority::new(registry.clone(), alice, alice_secret.clone()).unwrap(),
            );
        let empty = producer.build_block().unwrap().expect("an idle v2 chain still proposes");
        assert!(empty.transactions.is_empty());
        assert_eq!(empty.header.gas_used, 0);
        let qc = certificate(&empty, &[(alice, &alice_secret)]);
        producer.commit_qc(empty.clone(), &qc).unwrap();
        assert_eq!(producer.build_block().unwrap().unwrap().header.base_fee_per_gas, 7);

        // A follower without a signing key validates and commits the same
        // empty certified block, then derives the same next base fee.
        let follower = RpcState::in_memory(1)
            .unwrap()
            .with_fee_policy(fee_policy())
            .unwrap()
            .with_validators(registry);
        follower.commit_qc(empty, &qc).unwrap();
        assert_eq!(follower.block_height(), 1);
        assert_eq!(follower.build_block().unwrap().unwrap().header.base_fee_per_gas, 7);
    }

    #[test]
    fn legacy_chains_keep_skipping_empty_blocks_and_refuse_v2_headers() {
        let key = signing_key(7);
        let state = funded_state(&key, 1_000_000);
        assert!(state.build_block().unwrap().is_none(), "legacy chains skip empty selections");

        // Demo chains seal synchronously; the committed block is a v1 header
        // with no fee.
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        assert_eq!(state.block_height(), 1);
        let sealed = read(&state.blocks).last().cloned().unwrap();
        assert_eq!(sealed.header.version, 1);
        assert_eq!(sealed.header.base_fee_per_gas, 0);

        // The same block relabelled as v2 is refused by a legacy chain.
        let mut v2 = sealed;
        v2.header.version = FEE_HEADER_VERSION;
        let stranger = RpcState::in_memory(1).unwrap();
        assert!(matches!(stranger.accept_block(v2), Err(RpcError::BlockRejected(_))));
    }

    #[test]
    fn fee_policy_refuses_a_legacy_ledger_and_reopens_a_fee_ledger() {
        let key = signing_key(7);
        let address = Address::from_public_key(&key.verifying_key().to_bytes());

        // A legacy ledger must not be reinterpreted under a fee policy.
        let legacy_dir = tempfile::tempdir().unwrap();
        {
            let state = RpcState::with_data_dir(1, legacy_dir.path()).unwrap();
            state.seed_accounts([(address, 1_000_000)]).unwrap();
            state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
            assert_eq!(state.block_height(), 1);
        }
        let reopened = RpcState::with_data_dir(1, legacy_dir.path()).unwrap();
        assert!(matches!(reopened.with_fee_policy(fee_policy()), Err(RpcError::BlockRejected(_))));

        // A fee-enabled ledger restarts under the same policy. A durable store
        // needs its height-0 anchor persisted first (Q-030); an empty seed is
        // exactly what the node does for a fee-enabled genesis with no
        // allocations.
        let fee_dir = tempfile::tempdir().unwrap();
        {
            let state = RpcState::with_data_dir(1, fee_dir.path())
                .unwrap()
                .with_fee_policy(fee_policy())
                .unwrap();
            state.seed_accounts([]).unwrap();
            let block = state.build_block().unwrap().unwrap();
            state.accept_block(block).unwrap();
        }
        let reopened = RpcState::with_data_dir(1, fee_dir.path())
            .unwrap()
            .with_fee_policy(fee_policy())
            .expect("a v2 ledger is extendable");
        assert_eq!(reopened.block_height(), 1);
    }

    #[test]
    fn durable_fee_enabled_chain_persists_and_restores_a_real_tip() {
        let dir = tempfile::tempdir().unwrap();
        let tx_key = signing_key(7);
        let sender = Address::from_public_key(&tx_key.verifying_key().to_bytes());
        let (alice_secret, _) = BLSScheme::keygen();
        let alice = Address::new([9; 20]);
        let registry = registry(&[(alice, &alice_secret)]);
        let producer = RpcState::with_data_dir(1, dir.path())
            .unwrap()
            .with_fee_policy(fee_policy())
            .unwrap()
            .with_authority(
                BlockAuthority::new(registry.clone(), alice, alice_secret.clone()).unwrap(),
            );
        producer.seed_accounts([(sender, 2_000_000)]).unwrap();

        // Two non-empty v2 blocks, each paying a real tip. The second persist
        // extends the verified head, so it takes the re-executing suffix path
        // that refuses an unsettled replay.
        let mut expected_fee = 8;
        for nonce in 0..2 {
            producer.submit_transaction(priced_transfer(&tx_key, nonce, 10, 10)).unwrap();
            let block = producer.build_block().unwrap().unwrap();
            assert_eq!(block.header.version, FEE_HEADER_VERSION);
            assert_eq!(block.header.base_fee_per_gas, expected_fee);
            let qc = certificate(&block, &[(alice, &alice_secret)]);
            producer.commit_qc(block, &qc).unwrap();
            expected_fee -= 1;
        }
        let gas = Amount::from(DEFAULT_TX_GAS_LIMIT);
        // Tips are (10-8) and (10-7) per gas unit: the base fee fell between them.
        assert_eq!(producer.balance(&alice), 5 * gas);
        assert_eq!(producer.block_height(), 2);

        // Restart: the recorded policy replays and settles the same history, so
        // the credited tip survives and the next fee derives from the restored
        // tip.
        let reopened = RpcState::with_data_dir(1, dir.path())
            .unwrap()
            .with_fee_policy(fee_policy())
            .unwrap()
            .with_validators(registry);
        assert_eq!(reopened.block_height(), 2);
        assert_eq!(reopened.balance(&alice), 5 * gas);
        assert_eq!(reopened.balance(&sender), 2_000_000 - 20 - 20 * gas);
        assert_eq!(reopened.build_block().unwrap().unwrap().header.base_fee_per_gas, 6);
    }
}
