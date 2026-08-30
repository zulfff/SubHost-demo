//! Encrypted key storage.
//!
//! A wallet file holds an ed25519 secret seed encrypted with AES-256-GCM under a
//! scrypt-derived key. The KDF parameters are stored in the file so a future
//! hardening pass can raise them without invalidating existing wallets.
//!
//! The address is derived from the ed25519 *public* key, matching
//! [`subhost_core::Address::from_public_key`] and the RPC signature gate. Loading
//! re-derives the address and refuses a file whose stored address disagrees, so a
//! tampered wallet cannot redirect a signature to another account.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use subhost_core::Address;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Current wallet file format version.
pub const WALLET_VERSION: u32 = 1;
/// Refuse to read a wallet file larger than this.
pub const MAX_WALLET_FILE_BYTES: u64 = 1024 * 1024;
/// Minimum accepted password length in characters.
pub const MIN_PASSWORD_CHARS: usize = 8;

const SALT_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const SECRET_KEY_BYTES: usize = 32;
/// scrypt cost: N = 2^15, r = 8, p = 1 — the same work factor as Ethereum's
/// standard keystore, roughly 100 ms and 32 MiB per derivation.
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

/// An ed25519 secret seed, wiped on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PrivateKey(pub [u8; SECRET_KEY_BYTES]);

impl PrivateKey {
    /// The ed25519 verifying key for this seed.
    pub fn public_key(&self) -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&self.0).verifying_key().to_bytes()
    }

    /// The account address this key controls.
    pub fn address(&self) -> Address {
        Address::from_public_key(&self.public_key())
    }
}

/// Key-derivation parameters recorded alongside the ciphertext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub log_n: u8,
    pub r: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self { log_n: SCRYPT_LOG_N, r: SCRYPT_R, p: SCRYPT_P }
    }
}

impl KdfParams {
    /// Reject parameters that would be unusable or trivially cheap to brute force.
    fn validate(&self) -> Result<scrypt::Params, WalletError> {
        if self.log_n < 14 {
            return Err(WalletError::WeakKdfParameters);
        }
        scrypt::Params::new(self.log_n, self.r, self.p, SECRET_KEY_BYTES)
            .map_err(|_| WalletError::KeyDerivationFailed)
    }
}

/// The ciphertext bundle produced by encrypting a private key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedKey {
    ciphertext: Vec<u8>,
    salt: Vec<u8>,
    nonce: Vec<u8>,
    kdf: KdfParams,
}

/// An on-disk wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wallet {
    pub address: String,
    pub encrypted_key: Vec<u8>,
    pub salt: Vec<u8>,
    pub nonce: Vec<u8>,
    pub version: u32,
    /// Absent in the original format; defaults to the standard cost.
    #[serde(default)]
    pub kdf: KdfParams,
}

impl Wallet {
    /// Create a wallet around a freshly generated key.
    pub fn new(password: &str) -> Result<Self, WalletError> {
        let mut secret = [0u8; SECRET_KEY_BYTES];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut secret);
        let wallet = Self::from_secret(&secret, password);
        secret.zeroize();
        wallet
    }

    /// Import a hex-encoded 32-byte secret seed.
    pub fn from_private_key(private_key_hex: &str, password: &str) -> Result<Self, WalletError> {
        let trimmed = private_key_hex.trim().trim_start_matches("0x");
        let bytes = hex::decode(trimmed).map_err(|_| WalletError::InvalidPrivateKey)?;
        let mut secret: [u8; SECRET_KEY_BYTES] =
            bytes.as_slice().try_into().map_err(|_| WalletError::InvalidPrivateKey)?;
        let wallet = Self::from_secret(&secret, password);
        secret.zeroize();
        wallet
    }

    fn from_secret(secret: &[u8; SECRET_KEY_BYTES], password: &str) -> Result<Self, WalletError> {
        validate_password(password)?;
        let address = derive_address(secret);
        let encrypted = encrypt_private_key(secret, password, KdfParams::default())?;
        Ok(Self {
            address: address.to_string(),
            encrypted_key: encrypted.ciphertext,
            salt: encrypted.salt,
            nonce: encrypted.nonce,
            version: WALLET_VERSION,
            kdf: encrypted.kdf,
        })
    }

    /// Write the wallet atomically with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<(), WalletError> {
        if self.version != WALLET_VERSION {
            return Err(WalletError::UnsupportedVersion(self.version));
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;

        let json = serde_json::to_string_pretty(self)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        // Restrict the file before it holds key material.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(json.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|error| error.error)?;
        // fsync the directory so the rename survives a crash.
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Read a wallet file without decrypting it.
    pub fn read(path: &Path) -> Result<Self, WalletError> {
        if std::fs::metadata(path)?.len() > MAX_WALLET_FILE_BYTES {
            return Err(WalletError::FileTooLarge);
        }
        let wallet: Self = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if wallet.version != WALLET_VERSION {
            return Err(WalletError::UnsupportedVersion(wallet.version));
        }
        Ok(wallet)
    }

    /// Read and decrypt a wallet, verifying the stored address.
    pub fn load(path: &Path, password: &str) -> Result<(Self, PrivateKey), WalletError> {
        validate_password(password)?;
        let wallet = Self::read(path)?;
        let private_key = decrypt_private_key(
            &EncryptedKey {
                ciphertext: wallet.encrypted_key.clone(),
                salt: wallet.salt.clone(),
                nonce: wallet.nonce.clone(),
                kdf: wallet.kdf,
            },
            password,
        )?;
        // The decrypted key must actually control the advertised address.
        if wallet.address != derive_address(&private_key.0).to_string() {
            return Err(WalletError::AddressMismatch);
        }
        Ok((wallet, private_key))
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// The parsed address, or an error if the stored string is malformed.
    pub fn parsed_address(&self) -> Result<Address, WalletError> {
        Address::from_hex(&self.address).map_err(|_| WalletError::AddressMismatch)
    }

    /// Whether this wallet claims `address`, comparing case-insensitively.
    pub fn matches(&self, address: &Address) -> bool {
        self.address.eq_ignore_ascii_case(&address.to_string())
    }
}

fn validate_password(password: &str) -> Result<(), WalletError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(WalletError::WeakPassword);
    }
    Ok(())
}

fn derive_address(secret: &[u8; SECRET_KEY_BYTES]) -> Address {
    Address::from_public_key(
        ed25519_dalek::SigningKey::from_bytes(secret).verifying_key().as_bytes(),
    )
}

/// Derive the AES key from a password and salt, wiping intermediate material.
fn derive_encryption_key(
    password: &str,
    salt: &[u8],
    kdf: KdfParams,
) -> Result<[u8; 32], WalletError> {
    let params = kdf.validate()?;
    let mut key = [0u8; 32];
    scrypt::scrypt(password.as_bytes(), salt, &params, &mut key)
        .map_err(|_| WalletError::KeyDerivationFailed)?;
    Ok(key)
}

fn encrypt_private_key(
    secret: &[u8; SECRET_KEY_BYTES],
    password: &str,
    kdf: KdfParams,
) -> Result<EncryptedKey, WalletError> {
    let mut rng = rand::thread_rng();
    let mut salt = [0u8; SALT_BYTES];
    rand::RngCore::fill_bytes(&mut rng, &mut salt);
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    rand::RngCore::fill_bytes(&mut rng, &mut nonce_bytes);

    let mut key = derive_encryption_key(password, &salt, kdf)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| WalletError::EncryptionFailed)?;
    key.zeroize();

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), secret.as_slice())
        .map_err(|_| WalletError::EncryptionFailed)?;

    Ok(EncryptedKey { ciphertext, salt: salt.to_vec(), nonce: nonce_bytes.to_vec(), kdf })
}

fn decrypt_private_key(
    encrypted: &EncryptedKey,
    password: &str,
) -> Result<PrivateKey, WalletError> {
    // Validate the framing before spending ~100 ms on the KDF.
    if encrypted.salt.len() != SALT_BYTES || encrypted.nonce.len() != NONCE_BYTES {
        return Err(WalletError::DecryptionFailed);
    }

    let mut key = derive_encryption_key(password, &encrypted.salt, encrypted.kdf)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| WalletError::DecryptionFailed)?;
    key.zeroize();

    let mut plaintext = cipher
        .decrypt(Nonce::from_slice(&encrypted.nonce), encrypted.ciphertext.as_slice())
        .map_err(|_| WalletError::DecryptionFailed)?;

    let secret: Result<[u8; SECRET_KEY_BYTES], _> = plaintext.as_slice().try_into();
    // The heap buffer still holds the raw key; wipe it either way.
    let result = match secret {
        Ok(secret) => Ok(PrivateKey(secret)),
        Err(_) => Err(WalletError::DecryptionFailed),
    };
    plaintext.zeroize();
    result
}

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("key derivation failed")]
    KeyDerivationFailed,

    #[error("scrypt parameters are below the minimum accepted cost")]
    WeakKdfParameters,

    #[error("encryption failed")]
    EncryptionFailed,

    #[error("decryption failed (wrong password or corrupt wallet)")]
    DecryptionFailed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("private key must be 32 bytes of hex")]
    InvalidPrivateKey,

    #[error("wallet address does not match the decrypted private key")]
    AddressMismatch,

    #[error("password must contain at least {MIN_PASSWORD_CHARS} characters")]
    WeakPassword,

    #[error("unsupported wallet version: {0}")]
    UnsupportedVersion(u32),

    #[error("wallet file exceeds the maximum size")]
    FileTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn round_trip_through_an_atomic_file() {
        let wallet = Wallet::new(PASSWORD).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        wallet.save(&path).unwrap();

        let (loaded, key) = Wallet::load(&path, PASSWORD).unwrap();
        assert_eq!(loaded, wallet);
        assert_eq!(loaded.address(), derive_address(&key.0).to_string());
        assert_eq!(key.address().to_string(), wallet.address);
        assert_eq!(loaded.version, WALLET_VERSION);
        assert_eq!(loaded.kdf, KdfParams::default());

        // No temporary files may survive the atomic write.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("wallet.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn saved_wallet_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        Wallet::new(PASSWORD).unwrap().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a wallet must not be group or world readable");
    }

    #[test]
    fn address_is_derived_from_the_public_key() {
        // The RPC gate binds `from` to the ed25519 public key, so the wallet must
        // derive the same address or no account can ever authenticate.
        let key = PrivateKey([42u8; 32]);
        assert_eq!(derive_address(&key.0), Address::from_public_key(&key.public_key()));
        assert_eq!(key.address(), derive_address(&key.0));
    }

    #[test]
    fn import_round_trips_a_known_key() {
        let secret = [7u8; 32];
        let wallet = Wallet::from_private_key(&hex::encode(secret), PASSWORD).unwrap();
        assert_eq!(wallet.address, derive_address(&secret).to_string());

        // The `0x` prefix and surrounding whitespace are accepted.
        let prefixed =
            Wallet::from_private_key(&format!("  0x{}  ", hex::encode(secret)), PASSWORD).unwrap();
        assert_eq!(prefixed.address, wallet.address);
    }

    #[test]
    fn import_rejects_malformed_keys() {
        for candidate in ["", "0x", "zz", &"11".repeat(31), &"11".repeat(33)] {
            assert!(
                matches!(
                    Wallet::from_private_key(candidate, PASSWORD),
                    Err(WalletError::InvalidPrivateKey)
                ),
                "{candidate:?} must be rejected"
            );
        }
    }

    #[test]
    fn weak_passwords_are_rejected_everywhere() {
        assert!(matches!(Wallet::new("short"), Err(WalletError::WeakPassword)));
        assert!(matches!(
            Wallet::from_private_key(&"11".repeat(32), "short"),
            Err(WalletError::WeakPassword)
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        Wallet::new(PASSWORD).unwrap().save(&path).unwrap();
        assert!(matches!(Wallet::load(&path, "short"), Err(WalletError::WeakPassword)));
    }

    #[test]
    fn a_wrong_password_fails_to_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        Wallet::new(PASSWORD).unwrap().save(&path).unwrap();
        assert!(matches!(
            Wallet::load(&path, "another password"),
            Err(WalletError::DecryptionFailed)
        ));
    }

    #[test]
    fn address_tampering_is_detected() {
        let mut wallet = Wallet::new(PASSWORD).unwrap();
        wallet.address = Address::new([9u8; 20]).to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        wallet.save(&path).unwrap();
        assert!(matches!(Wallet::load(&path, PASSWORD), Err(WalletError::AddressMismatch)));
    }

    #[test]
    fn ciphertext_tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        let mut wallet = Wallet::new(PASSWORD).unwrap();
        let last = wallet.encrypted_key.len() - 1;
        wallet.encrypted_key[last] ^= 0x01;
        wallet.save(&path).unwrap();
        assert!(matches!(Wallet::load(&path, PASSWORD), Err(WalletError::DecryptionFailed)));
    }

    #[test]
    fn malformed_salt_or_nonce_is_rejected_without_panicking() {
        let base = encrypt_private_key(&[1u8; 32], PASSWORD, KdfParams::default()).unwrap();
        for broken in [
            EncryptedKey { salt: vec![0; 31], ..base.clone() },
            EncryptedKey { nonce: vec![0; 11], ..base.clone() },
            EncryptedKey { nonce: Vec::new(), ..base.clone() },
        ] {
            assert!(matches!(
                decrypt_private_key(&broken, PASSWORD),
                Err(WalletError::DecryptionFailed)
            ));
        }
        assert!(decrypt_private_key(&base, PASSWORD).is_ok());
    }

    #[test]
    fn unsupported_versions_are_rejected_on_read_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        let mut wallet = Wallet::new(PASSWORD).unwrap();
        wallet.save(&path).unwrap();

        wallet.version = 99;
        assert!(matches!(wallet.save(&path), Err(WalletError::UnsupportedVersion(99))));

        // A file claiming another version must not be read.
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["version"] = serde_json::json!(2);
        std::fs::write(&path, raw.to_string()).unwrap();
        assert!(matches!(Wallet::read(&path), Err(WalletError::UnsupportedVersion(2))));
    }

    #[test]
    fn oversized_and_missing_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_WALLET_FILE_BYTES + 1).unwrap();
        assert!(matches!(Wallet::read(&path), Err(WalletError::FileTooLarge)));
        assert!(matches!(Wallet::read(&dir.path().join("missing.json")), Err(WalletError::Io(_))));
    }

    #[test]
    fn weak_kdf_parameters_are_refused() {
        let weak = KdfParams { log_n: 10, r: 8, p: 1 };
        assert!(matches!(
            encrypt_private_key(&[1u8; 32], PASSWORD, weak),
            Err(WalletError::WeakKdfParameters)
        ));
        assert!(KdfParams::default().validate().is_ok());
    }

    #[test]
    fn a_wallet_file_without_kdf_params_defaults_to_the_standard_cost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        Wallet::new(PASSWORD).unwrap().save(&path).unwrap();

        // Simulate the original format, which had no `kdf` field.
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw.as_object_mut().unwrap().remove("kdf");
        std::fs::write(&path, raw.to_string()).unwrap();

        let (wallet, _) = Wallet::load(&path, PASSWORD).unwrap();
        assert_eq!(wallet.kdf, KdfParams::default());
    }

    #[test]
    fn address_helpers_match_case_insensitively() {
        let wallet = Wallet::new(PASSWORD).unwrap();
        let address = wallet.parsed_address().unwrap();
        assert!(wallet.matches(&address));

        let mut upper = wallet.clone();
        upper.address = upper.address.to_uppercase();
        assert!(upper.matches(&address));

        let mut broken = wallet;
        broken.address = "not-an-address".to_string();
        assert!(matches!(broken.parsed_address(), Err(WalletError::AddressMismatch)));
    }

    #[test]
    fn two_wallets_never_share_a_salt_or_nonce() {
        let first = Wallet::new(PASSWORD).unwrap();
        let second = Wallet::new(PASSWORD).unwrap();
        assert_ne!(first.salt, second.salt);
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.address, second.address);
    }
}
