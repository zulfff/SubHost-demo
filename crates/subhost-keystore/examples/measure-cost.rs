//! Local hardware measurement of the default scrypt cost.
//!
//! Uses disposable fixture material generated in a temporary directory, never
//! a live identity. Adopted from the parked `s9/encrypted-producer-keystore`
//! session's staged example (Q-018 coordination); answers the "measure on this
//! machine" note in the agreed contract. The measured numbers inform policy;
//! they are not a guarantee for other hardware.
//!
//! Run with: `cargo run -p subhost-keystore --example measure-cost`

use std::time::Instant;
use subhost_crypto::BLSScheme;
use subhost_keystore::{
    read_encrypted_producer_key, write_encrypted_producer_key_new, DEFAULT_LOG_N, KDF_P, KDF_R,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fixture.keystore.json");
    let (secret, _) = BLSScheme::keygen();
    let password = b"measurement-fixture-password";

    let start = Instant::now();
    write_encrypted_producer_key_new(&path, &secret, password)?;
    let write_seconds = start.elapsed().as_secs_f64();

    let start = Instant::now();
    let _unlocked = read_encrypted_producer_key(&path, password)?;
    let unlock_seconds = start.elapsed().as_secs_f64();

    let n = 1u64 << DEFAULT_LOG_N;
    println!(
        "Default scrypt N={n} r={KDF_R} p={KDF_P}; ROMix working set {} MiB",
        128 * n * u64::from(KDF_R) / (1024 * 1024)
    );
    println!("Write including authenticated read-back (two derivations): {write_seconds:.3} s");
    println!("One unlock (one derivation): {unlock_seconds:.3} s");
    Ok(())
}
