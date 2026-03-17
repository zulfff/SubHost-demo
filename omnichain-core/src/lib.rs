//! Core types and primitives for Omnichain
//! 
//! # Security Note
//! This module defines fundamental types used throughout the blockchain.
//! All hash types use fixed-size arrays to prevent length extension attacks.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Fixed-size hash type (32 bytes = 256 bits)
/// 
/// SECURITY: Using Blake3 for all hashing operations. Blake3 is faster than SHA3
/// and provides similar security guarantees for our use case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hash([u8; 32]);

impl Hash {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compute hash from arbitrary data using Blake3
    pub fn from_data(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self::new(*hash.as_bytes())
    }

    pub const ZERO: Self = Self([0u8; 32]);
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; 32]> for Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Self::new(bytes)
    }
}

/// Block height type
pub type BlockHeight = u64;

/// Timestamp in nanoseconds since Unix epoch
pub type Timestamp = u64;

/// Account address (20 bytes = 160 bits)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address([u8; 20]);

impl Address {
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Derive address from public key
    pub fn from_public_key(pk: &[u8]) -> Self {
        let hash = blake3::hash(pk);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash.as_bytes()[12..32]);
        Self::new(addr)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

/// Transaction nonce to prevent replay attacks
pub type Nonce = u64;

/// Amount type (wei equivalent)
pub type Amount = u128;

/// Gas limit type
pub type Gas = u64;

/// Chain ID for network isolation
pub type ChainId = u64;

/// Block header
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeader {
    pub version: u32,
    pub chain_id: ChainId,
    pub height: BlockHeight,
    pub timestamp: Timestamp,
    pub parent_hash: Hash,
    pub state_root: Hash,
    pub tx_root: Hash,
    pub receipt_root: Hash,
    pub validator: Address,
    pub gas_used: Gas,
    pub gas_limit: Gas,
    pub extra_data: Vec<u8>,
}

impl BlockHeader {
    /// Compute the hash of this header
    pub fn hash(&self) -> Hash {
        let encoded = bincode::serialize(self).expect("serialization should not fail");
        Hash::from_data(&encoded)
    }
}

/// Full block structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
    pub signatures: Vec<ValidatorSignature>,
}

impl Block {
    pub fn hash(&self) -> Hash {
        self.header.hash()
    }
}

/// Transaction types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionType {
    /// Standard transfer
    Transfer,
    /// Smart contract creation
    ContractCreation,
    /// Smart contract call
    ContractCall,
    /// Staking operation
    Stake,
    /// Unstaking operation
    Unstake,
    /// Governance vote
    GovernanceVote,
    /// Cross-chain message
    CrossChain,
}

/// Transaction structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub tx_type: TransactionType,
    pub nonce: Nonce,
    pub from: Address,
    pub to: Option<Address>,
    pub value: Amount,
    pub gas_price: u128,
    pub gas_limit: Gas,
    pub data: Vec<u8>,
    pub chain_id: ChainId,
    pub signature: TransactionSignature,
}

/// Transaction signature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

/// Validator signature for blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorSignature {
    pub validator: Address,
    pub signature: Vec<u8>,
}

/// Receipt for transaction execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub tx_hash: Hash,
    pub block_hash: Hash,
    pub block_height: BlockHeight,
    pub gas_used: Gas,
    pub status: ReceiptStatus,
    pub logs: Vec<Log>,
    pub contract_address: Option<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptStatus {
    Success = 1,
    Failure = 0,
}

/// Log entry from contract execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<Hash>,
    pub data: Vec<u8>,
}

/// Error types
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Invalid hash length: expected 32, got {0}")]
    InvalidHashLength(usize),
    
    #[error("Invalid address length: expected 20, got {0}")]
    InvalidAddressLength(usize),
    
    #[error("Serialization failed: {0}")]
    Serialization(String),
    
    #[error("Deserialization failed: {0}")]
    Deserialization(String),
}

/// Helper module for hex encoding/decoding
pub mod hex {
    /// Encode bytes to hex string
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let mut result = String::with_capacity(bytes.as_ref().len() * 2);
        for byte in bytes.as_ref() {
            result.push_str(&format!("{:02x}", byte));
        }
        result
    }
    
    /// Decode hex string to bytes
    pub fn decode(s: &str) -> Result<Vec<u8>, HexError> {
        let s = s.trim_start_matches("0x");
        if s.len() % 2 != 0 {
            return Err(HexError::OddLength);
        }
        
        let mut result = Vec::with_capacity(s.len() / 2);
        for i in (0..s.len()).step_by(2) {
            let byte = u8::from_str_radix(&s[i..i+2], 16)
                .map_err(|_| HexError::InvalidHex)?;
            result.push(byte);
        }
        Ok(result)
    }
    
    /// Hex decoding errors
    #[derive(Debug)]
    pub enum HexError {
        OddLength,
        InvalidHex,
    }
    
    impl std::fmt::Display for HexError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                HexError::OddLength => write!(f, "Odd length hex string"),
                HexError::InvalidHex => write!(f, "Invalid hex character"),
            }
        }
    }
    
    impl std::error::Error for HexError {}
}
