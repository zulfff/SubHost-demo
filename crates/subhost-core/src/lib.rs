use serde::{Deserialize, Serialize};
use std::fmt;

pub const HASH_SIZE: usize = 32;
pub const ADDRESS_SIZE: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hash([u8; HASH_SIZE]);

impl Hash {
    pub const fn new(bytes: [u8; HASH_SIZE]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_SIZE] {
        &self.0
    }

    pub fn from_data(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self::new(*hash.as_bytes())
    }

    pub const ZERO: Self = Self([0u8; HASH_SIZE]);
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address([u8; ADDRESS_SIZE]);

impl Address {
    pub const fn new(bytes: [u8; ADDRESS_SIZE]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ADDRESS_SIZE] {
        &self.0
    }

    pub fn from_public_key(pk: &[u8]) -> Self {
        let hash = blake3::hash(pk);
        let mut addr = [0u8; ADDRESS_SIZE];
        addr.copy_from_slice(&hash.as_bytes()[12..32]);
        Self::new(addr)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

pub type BlockHeight = u64;
pub type Timestamp = u64;
pub type Nonce = u64;
pub type Amount = u128;
pub type Gas = u64;
pub type ChainId = u64;

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
    pub fn hash(&self) -> Hash {
        let encoded = bincode::serialize(self).expect("serialization should not fail");
        Hash::from_data(&encoded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionType {
    Transfer,
    ContractCreation,
    ContractCall,
    Stake,
    Unstake,
    GovernanceVote,
    CrossChain,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorSignature {
    pub validator: Address,
    pub signature: Vec<u8>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<Hash>,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Invalid hash length: expected {HASH_SIZE}, got {0}")]
    InvalidHashLength(usize),
    
    #[error("Invalid address length: expected {ADDRESS_SIZE}, got {0}")]
    InvalidAddressLength(usize),
    
    #[error("Serialization failed: {0}")]
    Serialization(String),
    
    #[error("Deserialization failed: {0}")]
    Deserialization(String),
}

pub mod hex {
    use std::fmt;

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let mut result = String::with_capacity(bytes.as_ref().len() * 2);
        for byte in bytes.as_ref() {
            result.push_str(&format!("{:02x}", byte));
        }
        result
    }
    
    pub fn decode(s: &str) -> Result<Vec<u8>, HexError> {
        let s = s.trim_start_matches("0x");
        if !s.is_ascii() {
            return Err(HexError::InvalidHex);
        }
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
    
    #[derive(Debug)]
    pub enum HexError {
        OddLength,
        InvalidHex,
    }
    
    impl fmt::Display for HexError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                HexError::OddLength => write!(f, "Odd length hex string"),
                HexError::InvalidHex => write!(f, "Invalid hex character"),
            }
        }
    }
    
    impl std::error::Error for HexError {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decode_rejects_non_ascii_without_panicking() {
        assert!(super::hex::decode("aéb").is_err());
    }

    #[test]
    fn genesis_allocations_round_trip_full_u128_range() {
        let mut genesis = GenesisConfig::default();
        genesis
            .allocations
            .insert(Address::new([1; ADDRESS_SIZE]), u128::MAX);
        let encoded = serde_json::to_string(&genesis).unwrap();
        let decoded: GenesisConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded.allocations.get(&Address::new([1; ADDRESS_SIZE])),
            Some(&u128::MAX)
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisConfig {
    pub chain_id: u64,
    pub initial_validators: Vec<ValidatorInfo>,
    #[serde(with = "genesis_allocations")]
    pub allocations: std::collections::HashMap<Address, u128>,
    pub block_time_ms: u64,
    pub gas_limit: u64,
    pub genesis_time: u64,
}

mod genesis_allocations {
    use super::{Address, ADDRESS_SIZE};
    use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;

    #[derive(Serialize, Deserialize)]
    struct Allocation {
        address: String,
        balance: String,
    }

    pub fn serialize<S>(allocations: &HashMap<Address, u128>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut values: Vec<_> = allocations
            .iter()
            .map(|(address, balance)| Allocation {
                address: address.to_string(),
                balance: balance.to_string(),
            })
            .collect();
        values.sort_by(|left, right| left.address.cmp(&right.address));
        values.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<Address, u128>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<Allocation>::deserialize(deserializer)?;
        let mut allocations = HashMap::with_capacity(values.len());
        for value in values {
            let raw = value
                .address
                .strip_prefix("0x")
                .ok_or_else(|| D::Error::custom("allocation address must start with 0x"))?;
            let bytes = hex::decode(raw).map_err(D::Error::custom)?;
            let bytes: [u8; ADDRESS_SIZE] = bytes
                .try_into()
                .map_err(|_| D::Error::custom("allocation address must be 20 bytes"))?;
            let address = Address::new(bytes);
            let balance = value.balance.parse::<u128>().map_err(D::Error::custom)?;
            if allocations.insert(address, balance).is_some() {
                return Err(D::Error::custom("duplicate genesis allocation"));
            }
        }
        Ok(allocations)
    }
}

impl Default for GenesisConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            initial_validators: vec![],
            allocations: std::collections::HashMap::new(),
            block_time_ms: 1000,
            gas_limit: 30_000_000,
            genesis_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub address: Address,
    pub public_key: Vec<u8>,
    pub power: u64,
}

impl GenesisConfig {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.chain_id == 0 {
            return Err(CoreError::Serialization("Chain ID cannot be 0".to_string()));
        }
        if self.block_time_ms == 0 {
            return Err(CoreError::Serialization("Block time must be > 0".to_string()));
        }
        if self.initial_validators.is_empty() {
            return Err(CoreError::Serialization("At least one validator required".to_string()));
        }
        Ok(())
    }
    
    pub fn load(path: &std::path::Path) -> Result<Self, CoreError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;
        let config: GenesisConfig = serde_json::from_str(&content)
            .map_err(|e| CoreError::Deserialization(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
    
    pub fn save(&self, path: &std::path::Path) -> Result<(), CoreError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;
        std::fs::write(path, json)
            .map_err(|e| CoreError::Serialization(e.to_string()))
    }
}
