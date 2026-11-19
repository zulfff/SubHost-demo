//! Emits the funded address and one signed `eth_sendTransaction` body so a real
//! node can be driven with curl.
//!
//! Usage: `emit <data_len> <nonce> <chain_id>`.

use ed25519_dalek::{Signer, SigningKey};
use subhost_core::{Address, Transaction, TransactionSignature, TransactionType};

fn main() {
    let data_len: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(131_072);
    let nonce: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let chain_id: u64 = std::env::args()
        .nth(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    let key = SigningKey::from_bytes(&[7u8; 32]);
    let public_key = key.verifying_key().to_bytes();
    let from = Address::from_public_key(&public_key);
    let to = Address::new([2u8; 20]);
    let data = vec![0xAAu8; data_len];

    let unsigned = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from,
        to: Some(to),
        value: 0,
        gas_price: 1,
        gas_limit: 1,
        data,
        chain_id,
        public_key,
        signature: TransactionSignature::EMPTY,
    };
    let signature = key.sign(&unsigned.signing_payload()).to_bytes();
    let tx = Transaction { signature: TransactionSignature::from_ed25519(&signature), ..unsigned };

    // Sanity: the same check the RPC edge runs.
    assert!(tx.verify_signature());

    println!("FROM={from}");
    println!("TO={to}");
    println!(
        "BODY={}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_sendTransaction",
            "params": [{
                "chainId": format!("0x{:x}", chain_id),
                "from": from.to_string(),
                "to": to.to_string(),
                "value": "0x0",
                "gasPrice": "0x1",
                "gas": "0x1",
                "nonce": format!("0x{:x}", nonce),
                "data": format!("0x{}", hex::encode(&tx.data)),
                "publicKey": format!("0x{}", hex::encode(public_key)),
                "signature": format!("0x{}", hex::encode(signature)),
            }]
        })
    );
}