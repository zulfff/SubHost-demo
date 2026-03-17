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
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PrivateKey(pub [u8; 32]);

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
        Ok((wallet, private_key))
    }
    
    pub fn address(&self) -> &str {
        &self.address
    }
}

fn derive_address(private_key: &[u8; 32]) -> Address {
    let hash = blake3::hash(private_key);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash.as_bytes()[12..32]);
    Address::new(addr)
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
    let decrypted = cipher
        .decrypt(nonce, encrypted)
        .map_err(|_| WalletError::DecryptionFailed)?;
    
    key.zeroize();
    
    if decrypted.len() != 32 {
        return Err(WalletError::DecryptionFailed);
    }
    
    let mut private_key = [0u8; 32];
    private_key.copy_from_slice(&decrypted);
    
    Ok(PrivateKey(private_key))
}
