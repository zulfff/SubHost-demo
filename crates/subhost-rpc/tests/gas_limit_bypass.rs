//! Regression tests for the block gas cap on the peer path.
//!
//! `SUBHOST-2026-001`: `accept_block` used to re-validate a peer block without
//! comparing the sum of its transaction gas limits to the header's `gas_limit`.
//! A peer could publish a block carrying more than `BLOCK_GAS_LIMIT` gas; the
//! node accepted and persisted it, and its own ledger validator then refused to
//! load that file on the next restart. The accept path now enforces the same
//! ceiling as the producer, so the invariant the loader checks cannot be broken
//! by a gossiped block.

use ed25519_dalek::{Signer, SigningKey};
use subhost_core::{
    tx_root_of, Address, Block, BlockHeader, Hash, Transaction, TransactionSignature,
    TransactionType, BLOCK_GAS_LIMIT,
};
use subhost_rpc::{RpcError, RpcState};
use subhost_state::State;
use subhost_storage::{receipt_commitment, receipts_root};

const CHAIN_ID: u64 = 1;

fn signed_transfer(key: &SigningKey, nonce: u64, value: u128, gas_limit: u64) -> Transaction {
    let public_key = key.verifying_key().to_bytes();
    let mut tx = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
        to: Some(Address::new([2; 20])),
        value,
        gas_price: 1,
        gas_limit,
        data: Vec::new(),
        chain_id: CHAIN_ID,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    tx.signature = TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
    tx
}

/// Build the block a producer would seal for these transactions, committing to
/// the state root after applying them.
fn build_block(transactions: Vec<Transaction>, sender: Address, balance: u128) -> Block {
    let mut expected = State::with_chain_id(CHAIN_ID);
    expected.set_balance(sender, balance);
    let mut gas_used = 0u64;
    let mut hashes = Vec::new();
    let mut commitments = Vec::new();
    for (index, tx) in transactions.iter().enumerate() {
        expected.apply_transaction(tx).unwrap();
        gas_used += tx.gas_limit;
        hashes.push(tx.hash());
        commitments.push(receipt_commitment(tx.hash(), 1, index as u32, tx.gas_limit, gas_used));
    }

    Block {
        header: BlockHeader {
            version: 1,
            chain_id: CHAIN_ID,
            height: 1,
            timestamp: 1,
            parent_hash: Hash::ZERO,
            state_root: expected.root(),
            tx_root: tx_root_of(&hashes),
            receipt_root: receipts_root(&commitments),
            validator: Address::ZERO,
            gas_used,
            gas_limit: BLOCK_GAS_LIMIT,
            extra_data: Vec::new(),
        },
        transactions,
        signatures: Vec::new(),
    }
}

#[test]
fn block_over_the_gas_limit_is_rejected_and_the_ledger_still_loads() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let balance = 1_000_000_000_000_000_000u128;

    // Two individually valid transfers, each declaring 20M gas. Their sum is
    // 40M, above the 30M ceiling the producer enforces.
    let transactions =
        vec![signed_transfer(&key, 0, 10, 20_000_000), signed_transfer(&key, 1, 10, 20_000_000)];
    let block = build_block(transactions, sender, balance);
    assert!(block.header.gas_used > block.header.gas_limit);

    let dir = tempfile::tempdir().unwrap();
    let follower = RpcState::with_data_dir(CHAIN_ID, dir.path()).unwrap();
    follower.seed_accounts([(sender, balance)]).unwrap();

    let error =
        follower.accept_block(block).expect_err("a block above the gas limit must be rejected");
    assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
    assert_eq!(follower.block_height(), 0);
    assert!(follower.block_at_height(1).is_none());

    // The rejected block was never persisted, so the node restarts cleanly.
    drop(follower);
    let restart = RpcState::with_data_dir(CHAIN_ID, dir.path())
        .expect("a rejected block must not corrupt the ledger");
    assert_eq!(restart.block_height(), 0);
}

/// A fresh key with zero balance can still push a block over the ceiling:
/// `accept_block` does not enforce the mempool's minimum gas price, so a
/// self-transfer with `gas_price = 0` costs nothing and changes only the nonce.
#[test]
fn zero_fee_zero_balance_block_over_the_limit_is_rejected() {
    let key = SigningKey::from_bytes(&[9; 32]);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());

    let public_key = key.verifying_key().to_bytes();
    let mut tx = Transaction {
        tx_type: TransactionType::Transfer,
        nonce: 0,
        from: sender,
        to: Some(sender),
        value: 0,
        gas_price: 0,
        gas_limit: BLOCK_GAS_LIMIT + 1,
        data: Vec::new(),
        chain_id: CHAIN_ID,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    tx.signature = TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());

    let block = build_block(vec![tx], sender, 0);
    assert!(block.header.gas_used > block.header.gas_limit);

    let dir = tempfile::tempdir().unwrap();
    let follower = RpcState::with_data_dir(CHAIN_ID, dir.path()).unwrap();

    let error = follower
        .accept_block(block)
        .expect_err("a zero-balance block above the gas limit must be rejected");
    assert!(matches!(error, RpcError::BlockRejected(_)), "{error:?}");
    assert_eq!(follower.block_height(), 0);
}

/// The ceiling is inclusive: a block that uses exactly the limit is valid, so
/// the new check cannot reject honest producer output.
#[test]
fn block_exactly_at_the_gas_limit_is_accepted() {
    let key = SigningKey::from_bytes(&[11; 32]);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let balance = 1_000_000_000_000_000_000u128;

    let transactions = vec![signed_transfer(&key, 0, 10, BLOCK_GAS_LIMIT)];
    let block = build_block(transactions, sender, balance);
    assert_eq!(block.header.gas_used, block.header.gas_limit);

    let dir = tempfile::tempdir().unwrap();
    let follower = RpcState::with_data_dir(CHAIN_ID, dir.path()).unwrap();
    follower.seed_accounts([(sender, balance)]).unwrap();

    let receipts = follower.accept_block(block).expect("a block at the gas limit must be accepted");
    assert_eq!(receipts.len(), 1);
    assert_eq!(follower.block_height(), 1);
}
