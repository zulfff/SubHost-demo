//! Shared helpers for the crate's unit tests.

use crate::methods::DEFAULT_TX_GAS_LIMIT;
use crate::state::RpcState;
use ed25519_dalek::{Signer, SigningKey};
use subhost_core::{Address, Amount, Nonce, Transaction, TransactionSignature, TransactionType};

pub(crate) fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(crate) fn signed_transfer(key: &SigningKey, nonce: Nonce, value: Amount) -> Transaction {
    let public_key = key.verifying_key().to_bytes();
    let unsigned = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
        to: Some(Address::new([2; 20])),
        value,
        gas_price: 1,
        gas_limit: DEFAULT_TX_GAS_LIMIT,
        data: Vec::new(),
        chain_id: 1,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    let signature = key.sign(&unsigned.signing_payload()).to_bytes();
    Transaction { signature: TransactionSignature::from_ed25519(&signature), ..unsigned }
}

pub(crate) fn funded_state(key: &SigningKey, balance: Amount) -> RpcState {
    let state = RpcState::in_memory(1).unwrap();
    let address = Address::from_public_key(&key.verifying_key().to_bytes());
    state.seed_accounts([(address, balance)]).unwrap();
    state
}
