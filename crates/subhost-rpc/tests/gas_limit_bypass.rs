//! Proof-of-concept for the `accept_block` block-gas-cap bypass.
//!
//! A peer-supplied block whose transactions sum to more than `BLOCK_GAS_LIMIT`
//! is accepted and persisted, then rejected by the node's own ledger validator
//! on the next restart.

use ed25519_dalek::{Signer, SigningKey};
use subhost_core::{
    tx_root_of, Address, Block, BlockHeader, Hash, Transaction, TransactionSignature,
    TransactionType, BLOCK_GAS_LIMIT,
};
use subhost_rpc::RpcState;
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

#[test]
fn oversized_block_is_accepted_and_then_breaks_restart() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let balance = 1_000_000_000_000_000_000u128;

    // Two individually valid transfers, each declaring 20M gas. Their sum is
    // 40M, which is above the 30M block ceiling the producer enforces.
    let tx0 = signed_transfer(&key, 0, 10, 20_000_000);
    let tx1 = signed_transfer(&key, 1, 10, 20_000_000);
    assert!(tx0.gas_limit + tx1.gas_limit > BLOCK_GAS_LIMIT);

    // Replicate the state transition so the block carries the state root the
    // follower will compute. This is what a block producer does.
    let mut expected = State::with_chain_id(CHAIN_ID);
    expected.set_balance(sender, balance);
    expected.apply_transaction(&tx0).unwrap();
    expected.apply_transaction(&tx1).unwrap();

    let transactions = vec![tx0, tx1];
    let mut gas_used = 0u64;
    let mut hashes = Vec::new();
    let mut commitments = Vec::new();
    for (index, tx) in transactions.iter().enumerate() {
        gas_used += tx.gas_limit;
        hashes.push(tx.hash());
        commitments.push(receipt_commitment(tx.hash(), 1, index as u32, tx.gas_limit, gas_used));
    }

    let block = Block {
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
    };
    assert!(block.header.gas_used > block.header.gas_limit);

    let dir = tempfile::tempdir().unwrap();
    let follower = RpcState::with_data_dir(CHAIN_ID, dir.path()).unwrap();
    follower.seed_accounts([(sender, balance)]).unwrap();

    // The oversized block passes every check in accept_block.
    let receipts = follower
        .accept_block(block.clone())
        .expect("accept_block must reject a block over the gas limit, but accepted it");
    assert_eq!(receipts.len(), 2);
    assert_eq!(follower.block_height(), 1);
    let committed = follower.block_at_height(1).unwrap();
    assert!(committed.header.gas_used > committed.header.gas_limit);
    println!(
        "accepted block: gas_used={} gas_limit={}",
        committed.header.gas_used, committed.header.gas_limit
    );

    drop(follower);
    let restart = RpcState::with_data_dir(CHAIN_ID, dir.path());
    match &restart {
        Ok(state) => panic!(
            "restart succeeded at height {}; expected the ledger validator to reject the block",
            state.block_height()
        ),
        Err(error) => println!("restart failed as predicted: {error}"),
    }
}
