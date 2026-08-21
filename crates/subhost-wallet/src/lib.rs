use serde::{Serialize, Deserialize};
use std::path::Path;
use subhost_core::Address;
use zeroize::{Zeroize, ZeroizeOnDrop};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("Invalid password")]
    InvalidPassword,
    
    #[error("Key derivation failed")]
    KeyDerivationFailed,
    
    #[error("Encryption failed")]
    EncryptionFailed,
    
    #[error("Decryption failed")]
    DecryptionFailed,
    
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    
    #[error("Invalid private key")]
    InvalidPrivateKey,

    #[error("Wallet address does not match the decrypted private key")]
    AddressMismatch,
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PrivateKey(pub [u8; 32]);

impl PrivateKey {
    /// The ed25519 public (verifying) key for this 32-byte secret seed. The wallet
    /// address is derived from this key, matching `subhost_core::Address::from_public_key`
    /// and the `eth_sendTransaction` signature gate in `subhost-rpc`.
    pub fn public_key(&self) -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&self.0)
            .verifying_key()
            .to_bytes()
    }
}

#[derive(Serialize, Deserialize)]
pub struct Wallet {
    pub address: String,
    pub encrypted_key: Vec<u8>,
    pub salt: Vec<u8>,
    pub nonce: Vec<u8>,
    pub version: u32,
}

impl Wallet {
    pub fn new(password: &str) -> Result<Self, WalletError> {
        let mut rng = rand::thread_rng();
        let mut private_key = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rng, &mut private_key);
        
        let address = derive_address(&private_key);
        let (encrypted_key, salt, nonce) = encrypt_private_key(&private_key, password)?;
        
        private_key.zeroize();
        
        Ok(Self {
            address: address.to_string(),
            encrypted_key,
            salt,
            nonce,
            version: 1,
        })
    }
    
    pub fn from_private_key(private_key_hex: &str, password: &str) -> Result<Self, WalletError> {
        let private_key_hex = private_key_hex.trim_start_matches("0x");
        let bytes = subhost_core::hex::decode(private_key_hex)
            .map_err(|_| WalletError::InvalidPrivateKey)?;
        
        if bytes.len() != 32 {
            return Err(WalletError::InvalidPrivateKey);
        }
        
        let mut private_key = [0u8; 32];
        private_key.copy_from_slice(&bytes);
        
        let address = derive_address(&private_key);
        let (encrypted_key, salt, nonce) = encrypt_private_key(&private_key, password)?;
        
        private_key.zeroize();
        
        Ok(Self {
            address: address.to_string(),
            encrypted_key,
            salt,
            nonce,
            version: 1,
        })
    }
    
    pub fn save(&self, path: &Path) -> Result<(), WalletError> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
    
    pub fn load(path: &Path, password: &str) -> Result<(Self, PrivateKey), WalletError> {
        let json = std::fs::read_to_string(path)?;
        let wallet: Wallet = serde_json::from_str(&json)?;
        let private_key = decrypt_private_key(
            &wallet.encrypted_key,
            &wallet.salt,
            &wallet.nonce,
            password,
        )?;
        if wallet.address != derive_address(&private_key.0).to_string() {
            return Err(WalletError::AddressMismatch);
        }
        Ok((wallet, private_key))
    }
    
    pub fn address(&self) -> &str {
        &self.address
    }
}

fn derive_address(private_key: &[u8; 32]) -> Address {
    // Derive the address from the ed25519 PUBLIC key, not from the raw secret.
    // This keeps the wallet consistent with subhost_core::Address::from_public_key,
    // which the RPC uses to bind `from` to the provided public key.
    let signing_key = ed25519_dalek::SigningKey::from_bytes(private_key);
    Address::from_public_key(signing_key.verifying_key().as_bytes())
}

fn encrypt_private_key(
    private_key: &[u8; 32],
    password: &str,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), WalletError> {
    let mut salt = [0u8; 32];
    let mut rng = rand::thread_rng();
    rand::RngCore::fill_bytes(&mut rng, &mut salt);
    
    let mut key = [0u8; 32];
    scrypt::scrypt(
        password.as_bytes(),
        &salt,
        &scrypt::Params::new(15, 8, 1, 32).map_err(|_| WalletError::KeyDerivationFailed)?,
        &mut key,
    ).map_err(|_| WalletError::KeyDerivationFailed)?;
    
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| WalletError::EncryptionFailed)?;
    
    let mut nonce_bytes = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rng, &mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    
    let encrypted = cipher
        .encrypt(nonce, private_key.as_slice())
        .map_err(|_| WalletError::EncryptionFailed)?;
    
    key.zeroize();
    
    Ok((encrypted, salt.to_vec(), nonce_bytes.to_vec()))
}

fn decrypt_private_key(
    encrypted: &[u8],
    salt: &[u8],
    nonce: &[u8],
    password: &str,
) -> Result<PrivateKey, WalletError> {
    if salt.len() != 32 || nonce.len() != 12 {
        return Err(WalletError::DecryptionFailed);
    }

    let mut key = [0u8; 32];
    scrypt::scrypt(
        password.as_bytes(),
        salt,
        &scrypt::Params::new(15, 8, 1, 32).map_err(|_| WalletError::KeyDerivationFailed)?,
        &mut key,
    ).map_err(|_| WalletError::KeyDerivationFailed)?;
    
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| WalletError::DecryptionFailed)?;
    
    let nonce = Nonce::from_slice(nonce);
    let mut decrypted = cipher
        .decrypt(nonce, encrypted)
        .map_err(|_| WalletError::DecryptionFailed)?;
    
    key.zeroize();
    
    if decrypted.len() != 32 {
        decrypted.zeroize();
        return Err(WalletError::DecryptionFailed);
    }
    
    let mut private_key = [0u8; 32];
    private_key.copy_from_slice(&decrypted);
    // The plaintext buffer still holds the raw private key; wipe it so the secret
    // does not linger on the heap longer than necessary.
    decrypted.zeroize();
    
    Ok(PrivateKey(private_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_nonce_without_panicking() {
        assert!(matches!(
            decrypt_private_key(&[], &[0u8; 32], &[0u8; 11], "password"),
            Err(WalletError::DecryptionFailed)
        ));
    }

    #[test]
    fn rejects_wallet_address_tampering() {
        let mut wallet = Wallet::new("password").unwrap();
        wallet.address = Address::new([9u8; 20]).to_string();
        let path = tempfile::NamedTempFile::new().unwrap();
        wallet.save(path.path()).unwrap();
        assert!(matches!(
            Wallet::load(path.path(), "password"),
            Err(WalletError::AddressMismatch)
        ));
    }

    #[test]
    fn address_is_derived_from_public_key() {
        // The RPC `eth_sendTransaction` gate binds `from` to the ed25519 public
        // key; the wallet must derive the same address or real accounts can never
        // authenticate. Lock that invariant here.
        let pk = PrivateKey([42u8; 32]);
        let addr = derive_address(&pk.0);
        assert_eq!(addr, Address::from_public_key(&pk.public_key()));
    }
}
