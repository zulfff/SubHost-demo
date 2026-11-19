//! Core consensus-independent types shared by every Subhost crate.
//!
//! Everything here is deliberately transport- and storage-agnostic: hashes,
//! addresses, the block/transaction/receipt shapes, and the genesis document.

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub mod fee;
pub mod merkle;

pub use fee::{
    settle_block_fees, BlockFeeSettlement, FeeBreakdown, FeeContext, FeeError, FeePolicy,
    FEE_CHANGE_DENOMINATOR, FEE_ELASTICITY_MULTIPLIER, FEE_HEADER_VERSION,
};

/// Length of a [`struct@Hash`] in bytes (BLAKE3-256).
pub const HASH_SIZE: usize = 32;
/// Length of an [`Address`] in bytes.
pub const ADDRESS_SIZE: usize = 20;
/// Length of an ed25519 public key carried by a signed transaction.
pub const PUBLIC_KEY_SIZE: usize = 32;
/// Per-block gas ceiling enforced by the block producer and the ledger validator.
pub const BLOCK_GAS_LIMIT: Gas = 30_000_000;
/// Default target block interval used when a genesis file omits it.
pub const DEFAULT_BLOCK_TIME_MS: u64 = 1_000;
/// Domain tag mixed into every block signing payload.
pub const BLOCK_SIGNING_DOMAIN: &[u8] = b"subhost-block-v1";

/// A 32-byte BLAKE3 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash([u8; HASH_SIZE]);

impl Hash {
    /// The all-zero hash, used as the genesis parent.
    pub const ZERO: Self = Self([0u8; HASH_SIZE]);

    pub const fn new(bytes: [u8; HASH_SIZE]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_SIZE] {
        &self.0
    }

    /// Hash arbitrary bytes with BLAKE3.
    pub fn from_data(data: &[u8]) -> Self {
        Self::new(*blake3::hash(data).as_bytes())
    }

    /// Parse a `0x`-prefixed (or bare) 32-byte hex digest.
    pub fn from_hex(value: &str) -> Result<Self, CoreError> {
        let raw = value.strip_prefix("0x").unwrap_or(value);
        let bytes = hex::decode(raw).map_err(|error| CoreError::InvalidHex(error.to_string()))?;
        let bytes: [u8; HASH_SIZE] =
            bytes.as_slice().try_into().map_err(|_| CoreError::InvalidHashLength(bytes.len()))?;
        Ok(Self::new(bytes))
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

/// A 20-byte account address derived from an ed25519 public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Address([u8; ADDRESS_SIZE]);

impl Address {
    /// The all-zero address, used as the local-producer validator placeholder.
    pub const ZERO: Self = Self([0u8; ADDRESS_SIZE]);

    pub const fn new(bytes: [u8; ADDRESS_SIZE]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ADDRESS_SIZE] {
        &self.0
    }

    /// Derive an address as the low 20 bytes of `blake3(public_key)`.
    ///
    /// `subhost-wallet` and the RPC signature gate both depend on this exact
    /// derivation; changing it invalidates every existing wallet file.
    pub fn from_public_key(public_key: &[u8]) -> Self {
        let hash = blake3::hash(public_key);
        let mut address = [0u8; ADDRESS_SIZE];
        address.copy_from_slice(&hash.as_bytes()[HASH_SIZE - ADDRESS_SIZE..]);
        Self::new(address)
    }

    /// Parse a `0x`-prefixed (or bare) 20-byte hex address.
    pub fn from_hex(value: &str) -> Result<Self, CoreError> {
        let raw = value.strip_prefix("0x").unwrap_or(value);
        let bytes = hex::decode(raw).map_err(|error| CoreError::InvalidHex(error.to_string()))?;
        let bytes: [u8; ADDRESS_SIZE] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| CoreError::InvalidAddressLength(bytes.len()))?;
        Ok(Self::new(bytes))
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        &self.0
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

/// Canonical block header. The header hash is the block hash.
///
/// Version 1 keeps the exact legacy 12-field binary layout. Version 2
/// (fee-enabled chains, Q-017) appends `base_fee_per_gas` as a 13th field;
/// the field is part of the hash and therefore of every signature and QC
/// commitment over the block. A version 1 header always carries
/// `base_fee_per_gas == 0` in memory and never encodes the field.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Recorded only by version >= 2 headers; zero on legacy headers.
    pub base_fee_per_gas: Amount,
}

impl BlockHeader {
    pub fn hash(&self) -> Hash {
        Hash::from_data(&encode_canonical(self))
    }
}

/// Binary layout: a fixed tuple, 12 elements for version 1 and 13 for
/// version 2. bincode's tuple access is length-based with no completeness
/// check, so the decoder may stop after 12 elements for a legacy header and
/// leave the rest of the stream to the next record. JSON uses named fields
/// instead, with the base fee as a decimal string (u128-safe).
impl Serialize for BlockHeader {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.version < FEE_HEADER_VERSION && self.base_fee_per_gas != 0 {
            return Err(serde::ser::Error::custom("a legacy header cannot carry a base fee"));
        }
        if serializer.is_human_readable() {
            if self.version >= FEE_HEADER_VERSION {
                HeaderJsonV2::from(self).serialize(serializer)
            } else {
                HeaderJsonV1::from(self).serialize(serializer)
            }
        } else {
            let fields = if self.version >= FEE_HEADER_VERSION { 13 } else { 12 };
            let mut tuple = serializer.serialize_tuple(fields)?;
            tuple.serialize_element(&self.version)?;
            tuple.serialize_element(&self.chain_id)?;
            tuple.serialize_element(&self.height)?;
            tuple.serialize_element(&self.timestamp)?;
            tuple.serialize_element(&self.parent_hash)?;
            tuple.serialize_element(&self.state_root)?;
            tuple.serialize_element(&self.tx_root)?;
            tuple.serialize_element(&self.receipt_root)?;
            tuple.serialize_element(&self.validator)?;
            tuple.serialize_element(&self.gas_used)?;
            tuple.serialize_element(&self.gas_limit)?;
            tuple.serialize_element(&self.extra_data)?;
            if self.version >= FEE_HEADER_VERSION {
                tuple.serialize_element(&self.base_fee_per_gas)?;
            }
            tuple.end()
        }
    }
}

impl<'de> Deserialize<'de> for BlockHeader {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let json = HeaderJson::deserialize(deserializer)?;
            return json.into_header();
        }
        deserializer.deserialize_tuple(13, HeaderVisitor)
    }
}

struct HeaderVisitor;

impl<'de> Visitor<'de> for HeaderVisitor {
    type Value = BlockHeader;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a version 1 or 2 block header tuple")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        fn next<'de, A: SeqAccess<'de>, T: Deserialize<'de>>(seq: &mut A) -> Result<T, A::Error> {
            seq.next_element()?.ok_or_else(|| de::Error::custom("truncated block header"))
        }
        let version: u32 = next(&mut seq)?;
        if version != 1 && version != FEE_HEADER_VERSION {
            return Err(de::Error::custom(format!("unsupported header version {version}")));
        }
        let header = BlockHeader {
            version,
            chain_id: next(&mut seq)?,
            height: next(&mut seq)?,
            timestamp: next(&mut seq)?,
            parent_hash: next(&mut seq)?,
            state_root: next(&mut seq)?,
            tx_root: next(&mut seq)?,
            receipt_root: next(&mut seq)?,
            validator: next(&mut seq)?,
            gas_used: next(&mut seq)?,
            gas_limit: next(&mut seq)?,
            extra_data: next(&mut seq)?,
            base_fee_per_gas: 0,
        };
        if version >= FEE_HEADER_VERSION {
            let base_fee: Amount = seq
                .next_element()?
                .ok_or_else(|| de::Error::custom("version 2 header is missing base_fee_per_gas"))?;
            return Ok(BlockHeader { base_fee_per_gas: base_fee, ..header });
        }
        Ok(header)
    }
}

/// Named-field JSON shape of a legacy header: the exact pre-fee field set.
#[derive(Serialize, Deserialize)]
struct HeaderJsonV1 {
    version: u32,
    chain_id: ChainId,
    height: BlockHeight,
    timestamp: Timestamp,
    parent_hash: Hash,
    state_root: Hash,
    tx_root: Hash,
    receipt_root: Hash,
    validator: Address,
    gas_used: Gas,
    gas_limit: Gas,
    extra_data: Vec<u8>,
}

/// Named-field JSON shape of a fee-enabled header. The base fee is a decimal
/// string so `u128` survives the round trip, like genesis allocations.
#[derive(Serialize, Deserialize)]
struct HeaderJsonV2 {
    version: u32,
    chain_id: ChainId,
    height: BlockHeight,
    timestamp: Timestamp,
    parent_hash: Hash,
    state_root: Hash,
    tx_root: Hash,
    receipt_root: Hash,
    validator: Address,
    gas_used: Gas,
    gas_limit: Gas,
    extra_data: Vec<u8>,
    #[serde(with = "fee_amount_decimal")]
    base_fee_per_gas: Amount,
}

/// Try the fee-enabled shape first; a legacy document lacks the base-fee
/// field and falls back to the 12-field shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum HeaderJson {
    V2(HeaderJsonV2),
    V1(HeaderJsonV1),
}

impl HeaderJson {
    fn into_header<D: de::Error>(self) -> Result<BlockHeader, D> {
        match self {
            Self::V2(json) => {
                if json.version < FEE_HEADER_VERSION {
                    return Err(de::Error::custom("a legacy header cannot carry a base fee"));
                }
                Ok(BlockHeader {
                    version: json.version,
                    chain_id: json.chain_id,
                    height: json.height,
                    timestamp: json.timestamp,
                    parent_hash: json.parent_hash,
                    state_root: json.state_root,
                    tx_root: json.tx_root,
                    receipt_root: json.receipt_root,
                    validator: json.validator,
                    gas_used: json.gas_used,
                    gas_limit: json.gas_limit,
                    extra_data: json.extra_data,
                    base_fee_per_gas: json.base_fee_per_gas,
                })
            }
            Self::V1(json) => Ok(BlockHeader {
                version: json.version,
                chain_id: json.chain_id,
                height: json.height,
                timestamp: json.timestamp,
                parent_hash: json.parent_hash,
                state_root: json.state_root,
                tx_root: json.tx_root,
                receipt_root: json.receipt_root,
                validator: json.validator,
                gas_used: json.gas_used,
                gas_limit: json.gas_limit,
                extra_data: json.extra_data,
                base_fee_per_gas: 0,
            }),
        }
    }
}

impl From<&BlockHeader> for HeaderJsonV1 {
    fn from(header: &BlockHeader) -> Self {
        Self {
            version: header.version,
            chain_id: header.chain_id,
            height: header.height,
            timestamp: header.timestamp,
            parent_hash: header.parent_hash,
            state_root: header.state_root,
            tx_root: header.tx_root,
            receipt_root: header.receipt_root,
            validator: header.validator,
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
            extra_data: header.extra_data.clone(),
        }
    }
}

impl From<&BlockHeader> for HeaderJsonV2 {
    fn from(header: &BlockHeader) -> Self {
        Self {
            version: header.version,
            chain_id: header.chain_id,
            height: header.height,
            timestamp: header.timestamp,
            parent_hash: header.parent_hash,
            state_root: header.state_root,
            tx_root: header.tx_root,
            receipt_root: header.receipt_root,
            validator: header.validator,
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
            extra_data: header.extra_data.clone(),
            base_fee_per_gas: header.base_fee_per_gas,
        }
    }
}

/// Decimal-string codec for `Amount` in human-readable documents.
mod fee_amount_decimal {
    use super::Amount;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Amount,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Amount, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
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

impl fmt::Display for TransactionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Transfer => "transfer",
            Self::ContractCreation => "contractCreation",
            Self::ContractCall => "contractCall",
            Self::Stake => "stake",
            Self::Unstake => "unstake",
            Self::GovernanceVote => "governanceVote",
            Self::CrossChain => "crossChain",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// ed25519 public key that controls `from`.
    ///
    /// It is part of the signed payload, so a block or a peer can authenticate a
    /// transaction without trusting whoever delivered it.
    pub public_key: [u8; PUBLIC_KEY_SIZE],
    pub signature: TransactionSignature,
}

impl Transaction {
    /// Canonical transaction hash over the *signed* encoding.
    ///
    /// This is the identity used by the mempool, the receipt index, and the
    /// `eth_sendTransaction` return value, so it must stay in one place.
    pub fn hash(&self) -> Hash {
        Hash::from_data(&encode_canonical(self))
    }

    /// The same transaction with a cleared signature.
    ///
    /// Signing and verification both hash this form, so a signature commits to
    /// every field except itself.
    pub fn signing_payload(&self) -> Vec<u8> {
        let unsigned = Self { signature: TransactionSignature::EMPTY, ..self.clone() };
        encode_canonical(&unsigned)
    }

    /// Total cost that must be covered by the sender balance: `value + fee`.
    pub fn total_cost(&self) -> Option<Amount> {
        self.fee().and_then(|fee| self.value.checked_add(fee))
    }

    /// `gas_price * gas_limit`, or `None` on overflow.
    pub fn fee(&self) -> Option<Amount> {
        self.gas_price.checked_mul(u128::from(self.gas_limit))
    }

    /// Verify that `signature` was produced by the key that owns `from`.
    ///
    /// This is the one authentication rule every path uses: RPC admission,
    /// gossip relay, and block acceptance. The public key is bound to the
    /// address before the signature is checked, so a caller cannot claim an
    /// address it does not control.
    pub fn verify_signature(&self) -> bool {
        use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};

        if Address::from_public_key(&self.public_key) != self.from {
            return false;
        }
        let Ok(verifying_key) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        verifying_key
            .verify(
                &self.signing_payload(),
                &Ed25519Signature::from_bytes(&self.signature.to_ed25519()),
            )
            .is_ok()
    }
}

/// A 64-byte ed25519 signature split into Ethereum-style `r`/`s`/`v` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

impl TransactionSignature {
    /// The cleared signature used to build a signing payload.
    pub const EMPTY: Self = Self { r: [0u8; 32], s: [0u8; 32], v: 0 };

    /// Split a raw 64-byte ed25519 signature into `r`/`s`.
    pub fn from_ed25519(signature: &[u8; 64]) -> Self {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&signature[..32]);
        s.copy_from_slice(&signature[32..]);
        Self { r, s, v: 0 }
    }

    /// Recombine `r`/`s` into a raw 64-byte ed25519 signature.
    pub fn to_ed25519(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&self.r);
        bytes[32..].copy_from_slice(&self.s);
        bytes
    }
}

impl Default for TransactionSignature {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
    pub signatures: Vec<ValidatorSignature>,
}

impl Block {
    pub fn hash(&self) -> Hash {
        self.header.hash()
    }

    /// Bytes a validator signs to attest this block.
    ///
    /// Layout: `BLOCK_SIGNING_DOMAIN || chain_id || height || block_hash`, with
    /// both integers big-endian. The block hash commits to the header, so the
    /// payload covers every header field and the transaction/receipt roots
    /// without signing the signature list itself.
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(BLOCK_SIGNING_DOMAIN.len() + 8 + 8 + HASH_SIZE);
        payload.extend_from_slice(BLOCK_SIGNING_DOMAIN);
        payload.extend_from_slice(&self.header.chain_id.to_be_bytes());
        payload.extend_from_slice(&self.header.height.to_be_bytes());
        payload.extend_from_slice(self.hash().as_bytes());
        payload
    }

    /// Commitment over the contained transaction hashes.
    pub fn tx_root(&self) -> Hash {
        let hashes: Vec<[u8; HASH_SIZE]> =
            self.transactions.iter().map(|tx| *tx.hash().as_bytes()).collect();
        Hash::from_data(&encode_canonical(&hashes))
    }
}

/// Commitment over an ordered set of transaction hashes.
///
/// Kept as a free function so a producer can compute the root before it has
/// assembled the final [`Block`].
pub fn tx_root_of(tx_hashes: &[Hash]) -> Hash {
    let hashes: Vec<[u8; HASH_SIZE]> = tx_hashes.iter().map(|hash| *hash.as_bytes()).collect();
    Hash::from_data(&encode_canonical(&hashes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSignature {
    pub validator: Address,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub tx_hash: Hash,
    pub block_hash: Hash,
    pub block_height: BlockHeight,
    /// Position of the transaction inside its block.
    pub transaction_index: u32,
    pub gas_used: Gas,
    /// Gas used by this transaction and every transaction before it in the block.
    pub cumulative_gas_used: Gas,
    pub status: ReceiptStatus,
    pub logs: Vec<Log>,
    pub contract_address: Option<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptStatus {
    Success = 1,
    Failure = 0,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<Hash>,
    pub data: Vec<u8>,
}

/// Deterministic binary encoding used for every hash commitment in the protocol.
///
/// `bincode` cannot fail for the fixed-shape types in this crate (no maps, no
/// unbounded recursion), so the encoding is infallible by construction.
pub fn encode_canonical<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serialize(value).expect("canonical encoding of a core type cannot fail")
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid hash length: expected {HASH_SIZE}, got {0}")]
    InvalidHashLength(usize),

    #[error("invalid address length: expected {ADDRESS_SIZE}, got {0}")]
    InvalidAddressLength(usize),

    #[error("invalid hex encoding: {0}")]
    InvalidHex(String),

    #[error("invalid genesis configuration: {0}")]
    InvalidGenesis(String),

    #[error("cannot read genesis file {path}: {source}")]
    GenesisRead {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot write genesis file {path}: {source}")]
    GenesisWrite {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot decode genesis file: {0}")]
    GenesisDecode(#[from] serde_json::Error),
}

/// The genesis document: chain identity, validator set, and initial balances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisConfig {
    pub chain_id: ChainId,
    pub initial_validators: Vec<ValidatorInfo>,
    /// Address -> starting balance. Serialized as a sorted array of decimal
    /// strings so `u128` survives a JSON round trip and the file is byte-stable.
    #[serde(with = "genesis_allocations")]
    pub allocations: std::collections::HashMap<Address, Amount>,
    pub block_time_ms: u64,
    pub gas_limit: Gas,
    pub genesis_time: Timestamp,
    /// Fee market policy (S6.2, Q-017). Absent means legacy fixed-gas-price
    /// rules, never an implicit upgrade; fee-enabled chains start from a
    /// fresh genesis. Omitted from legacy documents so they stay byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_policy: Option<FeePolicy>,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            initial_validators: Vec::new(),
            allocations: std::collections::HashMap::new(),
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            gas_limit: BLOCK_GAS_LIMIT,
            genesis_time: unix_timestamp(),
            fee_policy: None,
        }
    }
}

impl GenesisConfig {
    /// Stable namespace for consensus signatures, journals, and certified history.
    ///
    /// Allocations are sorted explicitly because their in-memory `HashMap`
    /// iteration order is process-local and must never change a signature domain.
    pub fn consensus_context(&self) -> Hash {
        let mut allocations: Vec<_> =
            self.allocations.iter().map(|(address, balance)| (*address, *balance)).collect();
        allocations.sort_unstable();
        // Legacy policy absence keeps the exact pre-fee six-field tuple, so
        // existing bindings are byte-identical. A policy appends its canonical
        // encoding as a seventh element; the two layouts never coexist on one
        // chain because fee-enabled chains require a fresh genesis.
        match &self.fee_policy {
            None => Hash::from_data(&encode_canonical(&(
                self.chain_id,
                &self.initial_validators,
                allocations,
                self.block_time_ms,
                self.gas_limit,
                self.genesis_time,
            ))),
            Some(policy) => Hash::from_data(&encode_canonical(&(
                self.chain_id,
                &self.initial_validators,
                allocations,
                self.block_time_ms,
                self.gas_limit,
                self.genesis_time,
                encode_canonical(policy),
            ))),
        }
    }

    /// Structural validation applied on every load and before every save.
    ///
    /// Note this deliberately does *not* require a validator set: the
    /// single-node development flow produces blocks locally without one, and
    /// [`Self::requires_validators`] is the check a multi-node deployment uses.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.chain_id == 0 {
            return Err(CoreError::InvalidGenesis("chain ID cannot be 0".into()));
        }
        if self.block_time_ms == 0 {
            return Err(CoreError::InvalidGenesis("block time must be > 0".into()));
        }
        if self.gas_limit == 0 {
            return Err(CoreError::InvalidGenesis("gas limit must be > 0".into()));
        }
        if let Some(policy) = &self.fee_policy {
            policy
                .validate(self.gas_limit)
                .map_err(|error| CoreError::InvalidGenesis(format!("fee policy: {error}")))?;
        }
        let mut seen = std::collections::HashSet::with_capacity(self.initial_validators.len());
        for validator in &self.initial_validators {
            if validator.power == 0 {
                return Err(CoreError::InvalidGenesis(format!(
                    "validator {} has zero voting power",
                    validator.address
                )));
            }
            if validator.public_key.is_empty() {
                return Err(CoreError::InvalidGenesis(format!(
                    "validator {} has no public key",
                    validator.address
                )));
            }
            if !seen.insert(validator.address) {
                return Err(CoreError::InvalidGenesis(format!(
                    "duplicate validator {}",
                    validator.address
                )));
            }
        }
        Ok(())
    }

    /// Reject a genesis that cannot support byzantine-fault-tolerant consensus.
    ///
    /// A multi-node deployment must call this in addition to [`Self::validate`].
    pub fn requires_validators(&self) -> Result<(), CoreError> {
        if self.initial_validators.is_empty() {
            return Err(CoreError::InvalidGenesis(
                "a validator network requires at least one initial validator".into(),
            ));
        }
        Ok(())
    }

    /// Total voting power across the initial validator set.
    pub fn total_voting_power(&self) -> u128 {
        self.initial_validators.iter().map(|validator| u128::from(validator.power)).sum()
    }

    pub fn load(path: &std::path::Path) -> Result<Self, CoreError> {
        let content = std::fs::read_to_string(path)
            .map_err(|source| CoreError::GenesisRead { path: path.to_path_buf(), source })?;
        let config: Self = serde_json::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), CoreError> {
        self.validate()?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
            .map_err(|source| CoreError::GenesisWrite { path: path.to_path_buf(), source })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub address: Address,
    pub public_key: Vec<u8>,
    pub power: u64,
    /// Proof that the validator controls `public_key`.
    ///
    /// Empty means the genesis document did not supply one. The registry rejects
    /// an empty proof once validator registration is wired in; keeping the field
    /// serde-defaulted lets older genesis files load until then.
    #[serde(default)]
    pub proof_of_possession: Vec<u8>,
}

/// Seconds since the Unix epoch, saturating at 0 for clocks set before 1970.
pub fn unix_timestamp() -> Timestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

mod genesis_allocations {
    use super::{Address, Amount};
    use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;

    #[derive(Serialize, Deserialize)]
    struct Allocation {
        address: String,
        balance: String,
    }

    pub(crate) fn serialize<S>(
        allocations: &HashMap<Address, Amount>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
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

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<Address, Amount>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<Allocation>::deserialize(deserializer)?;
        let mut allocations = HashMap::with_capacity(values.len());
        for value in values {
            if !value.address.starts_with("0x") {
                return Err(D::Error::custom("allocation address must start with 0x"));
            }
            let address = Address::from_hex(&value.address).map_err(D::Error::custom)?;
            let balance = value.balance.parse::<Amount>().map_err(D::Error::custom)?;
            if allocations.insert(address, balance).is_some() {
                return Err(D::Error::custom("duplicate genesis allocation"));
            }
        }
        Ok(allocations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction() -> Transaction {
        Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 7,
            from: Address::new([1; ADDRESS_SIZE]),
            to: Some(Address::new([2; ADDRESS_SIZE])),
            value: 10,
            gas_price: 3,
            gas_limit: 21_000,
            data: Vec::new(),
            chain_id: 1,
            public_key: [0u8; PUBLIC_KEY_SIZE],
            signature: TransactionSignature::EMPTY,
        }
    }

    fn sample_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                chain_id: 1,
                height: 1,
                timestamp: 0,
                parent_hash: Hash::ZERO,
                state_root: Hash::ZERO,
                tx_root: Hash::ZERO,
                receipt_root: Hash::ZERO,
                validator: Address::ZERO,
                gas_used: 0,
                gas_limit: BLOCK_GAS_LIMIT,
                extra_data: Vec::new(),
                base_fee_per_gas: 0,
            },
            transactions: vec![transaction()],
            signatures: Vec::new(),
        }
    }

    #[test]
    fn hex_parsers_reject_malformed_input_without_panicking() {
        assert!(Address::from_hex("aéb").is_err());
        assert!(Address::from_hex("0x01").is_err());
        assert!(Hash::from_hex("0x0f").is_err());
        let address = Address::new([3; ADDRESS_SIZE]);
        assert_eq!(Address::from_hex(&address.to_string()).unwrap(), address);
        let hash = Hash::from_data(b"subhost");
        assert_eq!(Hash::from_hex(&hash.to_string()).unwrap(), hash);
    }

    #[test]
    fn address_derivation_uses_low_20_bytes_of_the_digest() {
        let public_key = [9u8; 32];
        let digest = blake3::hash(&public_key);
        let expected = &digest.as_bytes()[HASH_SIZE - ADDRESS_SIZE..];
        assert_eq!(Address::from_public_key(&public_key).as_bytes(), expected);
    }

    #[test]
    fn signing_payload_excludes_only_the_signature() {
        let mut tx = transaction();
        let payload = tx.signing_payload();
        tx.signature = TransactionSignature { r: [7; 32], s: [8; 32], v: 1 };
        assert_eq!(tx.signing_payload(), payload, "signature must not be signed over");

        tx.signature = TransactionSignature::EMPTY;
        tx.value += 1;
        assert_ne!(tx.signing_payload(), payload, "value must be signed over");
    }

    #[test]
    fn transaction_signature_binds_the_public_key_and_every_field() {
        use ed25519_dalek::{Signer, SigningKey};

        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public_key = key.verifying_key().to_bytes();
        let mut tx = transaction();
        tx.from = Address::from_public_key(&public_key);
        tx.public_key = public_key;
        let signature = key.sign(&tx.signing_payload()).to_bytes();
        tx.signature = TransactionSignature::from_ed25519(&signature);
        assert!(tx.verify_signature());

        let mut tampered = tx.clone();
        tampered.value += 1;
        assert!(!tampered.verify_signature(), "value is part of the payload");

        let mut wrong_key = tx.clone();
        wrong_key.public_key = [9u8; PUBLIC_KEY_SIZE];
        assert!(!wrong_key.verify_signature(), "the key must own the address");

        let mut wrong_sender = tx;
        wrong_sender.from = Address::new([3; ADDRESS_SIZE]);
        assert!(!wrong_sender.verify_signature());
    }

    #[test]
    fn signature_round_trips_through_r_and_s() {
        let mut raw = [0u8; 64];
        for (index, byte) in raw.iter_mut().enumerate() {
            *byte = index as u8;
        }
        assert_eq!(TransactionSignature::from_ed25519(&raw).to_ed25519(), raw);
    }

    #[test]
    fn transaction_cost_saturates_instead_of_overflowing() {
        let mut tx = transaction();
        assert_eq!(tx.fee(), Some(63_000));
        assert_eq!(tx.total_cost(), Some(63_010));

        tx.gas_price = u128::MAX;
        assert_eq!(tx.fee(), None);
        assert_eq!(tx.total_cost(), None);
    }

    #[test]
    fn tx_root_matches_the_free_function_and_is_order_sensitive() {
        let first = transaction();
        let mut second = transaction();
        second.nonce = 8;
        let block = Block {
            header: BlockHeader {
                version: 1,
                chain_id: 1,
                height: 1,
                timestamp: 0,
                parent_hash: Hash::ZERO,
                state_root: Hash::ZERO,
                tx_root: Hash::ZERO,
                receipt_root: Hash::ZERO,
                validator: Address::ZERO,
                gas_used: 0,
                gas_limit: BLOCK_GAS_LIMIT,
                extra_data: Vec::new(),
                base_fee_per_gas: 0,
            },
            transactions: vec![first.clone(), second.clone()],
            signatures: Vec::new(),
        };
        assert_eq!(block.tx_root(), tx_root_of(&[first.hash(), second.hash()]));
        assert_ne!(block.tx_root(), tx_root_of(&[second.hash(), first.hash()]));
    }

    #[test]
    fn block_signing_payload_is_stable_and_covers_the_header() {
        let block = sample_block();
        let payload = block.signing_payload();

        assert_eq!(payload, block.clone().signing_payload(), "payload must be deterministic");

        let mut resigned = block.clone();
        resigned.signatures = vec![ValidatorSignature {
            validator: Address::new([9; ADDRESS_SIZE]),
            signature: vec![1, 2, 3],
        }];
        assert_eq!(resigned.signing_payload(), payload, "signatures are not signed over");

        let mut other_height = block.clone();
        other_height.header.height += 1;
        assert_ne!(other_height.signing_payload(), payload);

        let mut other_chain = block.clone();
        other_chain.header.chain_id += 1;
        assert_ne!(other_chain.signing_payload(), payload);

        let mut other_header = block;
        other_header.header.timestamp += 1;
        assert_ne!(other_header.signing_payload(), payload, "the block hash covers the header");
    }

    #[test]
    fn block_signing_payload_is_domain_separated() {
        fn layout(domain: &[u8], block: &Block) -> Vec<u8> {
            let mut payload = Vec::with_capacity(domain.len() + 8 + 8 + HASH_SIZE);
            payload.extend_from_slice(domain);
            payload.extend_from_slice(&block.header.chain_id.to_be_bytes());
            payload.extend_from_slice(&block.header.height.to_be_bytes());
            payload.extend_from_slice(block.hash().as_bytes());
            payload
        }

        let block = sample_block();
        let payload = block.signing_payload();
        assert_eq!(payload, layout(BLOCK_SIGNING_DOMAIN, &block));
        assert_ne!(payload, layout(b"subhost-block-v2", &block));
        assert_ne!(payload, block.hash().as_bytes().to_vec(), "a bare hash is not a payload");
        assert_eq!(payload.len(), BLOCK_SIGNING_DOMAIN.len() + 8 + 8 + HASH_SIZE);
    }

    #[test]
    fn validator_proof_of_possession_is_optional_in_json() {
        let validator = ValidatorInfo {
            address: Address::new([4; ADDRESS_SIZE]),
            public_key: vec![1, 2, 3],
            power: 10,
            proof_of_possession: vec![7, 8, 9],
        };

        let encoded = serde_json::to_value(&validator).unwrap();
        let decoded: ValidatorInfo = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.proof_of_possession, vec![7, 8, 9]);

        let mut legacy = encoded;
        legacy.as_object_mut().unwrap().remove("proof_of_possession");
        let decoded: ValidatorInfo = serde_json::from_value(legacy).unwrap();
        assert!(decoded.proof_of_possession.is_empty(), "old genesis files must still load");
    }

    #[test]
    fn genesis_allocations_round_trip_full_u128_range() {
        let mut genesis = GenesisConfig::default();
        genesis.allocations.insert(Address::new([1; ADDRESS_SIZE]), Amount::MAX);
        let encoded = serde_json::to_string(&genesis).unwrap();
        let decoded: GenesisConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.allocations.get(&Address::new([1; ADDRESS_SIZE])), Some(&Amount::MAX));
    }

    #[test]
    fn genesis_allocation_serialization_is_deterministic() {
        let mut first = GenesisConfig::default();
        first.allocations.insert(Address::new([2; ADDRESS_SIZE]), 2);
        first.allocations.insert(Address::new([1; ADDRESS_SIZE]), 1);
        let mut second = GenesisConfig { genesis_time: first.genesis_time, ..Default::default() };
        second.allocations.insert(Address::new([1; ADDRESS_SIZE]), 1);
        second.allocations.insert(Address::new([2; ADDRESS_SIZE]), 2);
        assert_eq!(serde_json::to_string(&first).unwrap(), serde_json::to_string(&second).unwrap());
        assert_eq!(first.consensus_context(), second.consensus_context());
    }

    #[test]
    fn genesis_validation_rejects_structural_errors() {
        assert!(GenesisConfig { chain_id: 0, ..Default::default() }.validate().is_err());
        assert!(GenesisConfig { block_time_ms: 0, ..Default::default() }.validate().is_err());
        assert!(GenesisConfig { gas_limit: 0, ..Default::default() }.validate().is_err());

        // A validator-less genesis is valid for single-node use, but a validator
        // network must reject it.
        let genesis = GenesisConfig::default();
        assert!(genesis.validate().is_ok());
        assert!(genesis.requires_validators().is_err());
    }

    #[test]
    fn genesis_validation_rejects_bad_validator_entries() {
        let validator = ValidatorInfo {
            address: Address::new([4; ADDRESS_SIZE]),
            public_key: vec![1, 2, 3],
            power: 10,
            proof_of_possession: Vec::new(),
        };

        let mut genesis = GenesisConfig {
            initial_validators: vec![validator.clone(), validator.clone()],
            ..Default::default()
        };
        assert!(genesis.validate().is_err(), "duplicate validators");

        genesis.initial_validators = vec![ValidatorInfo { power: 0, ..validator.clone() }];
        assert!(genesis.validate().is_err(), "zero voting power");

        genesis.initial_validators =
            vec![ValidatorInfo { public_key: Vec::new(), ..validator.clone() }];
        assert!(genesis.validate().is_err(), "missing public key");

        genesis.initial_validators = vec![validator];
        assert!(genesis.validate().is_ok());
        assert!(genesis.requires_validators().is_ok());
        assert_eq!(genesis.total_voting_power(), 10);
    }

    #[test]
    fn genesis_save_load_round_trip_rejects_invalid_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("genesis.json");
        let mut genesis = GenesisConfig::default();
        genesis.allocations.insert(Address::new([5; ADDRESS_SIZE]), 42);
        genesis.save(&path).unwrap();
        assert_eq!(GenesisConfig::load(&path).unwrap(), genesis);

        genesis.chain_id = 0;
        assert!(genesis.save(&path).is_err(), "save must validate");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(GenesisConfig::load(&path).is_err());
        assert!(GenesisConfig::load(&dir.path().join("missing.json")).is_err());
    }

    /// The exact pre-fee binary layout, encoded independently of the header's
    /// own Serialize impl: a 12-element tuple in field order.
    fn legacy_tuple(header: &BlockHeader) -> Vec<u8> {
        bincode::serialize(&(
            header.version,
            header.chain_id,
            header.height,
            header.timestamp,
            header.parent_hash,
            header.state_root,
            header.tx_root,
            header.receipt_root,
            header.validator,
            header.gas_used,
            header.gas_limit,
            &header.extra_data,
        ))
        .unwrap()
    }

    #[test]
    fn legacy_header_bytes_are_unchanged() {
        let header = sample_block().header;
        assert_eq!(header.version, 1);
        assert_eq!(encode_canonical(&header), legacy_tuple(&header));
        let decoded: BlockHeader = bincode::deserialize(&legacy_tuple(&header)).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.base_fee_per_gas, 0);
    }

    #[test]
    fn legacy_header_stops_after_twelve_elements() {
        // A record following the header must decode intact: the version-1
        // decoder may not consume bytes beyond its 12 fields.
        let header = sample_block().header;
        let stream = bincode::serialize(&(&header, 0xdead_beef_u64)).unwrap();
        let (decoded, tail): (BlockHeader, u64) = bincode::deserialize(&stream).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(tail, 0xdead_beef);
    }

    #[test]
    fn v2_header_appends_the_base_fee_and_changes_the_hash() {
        let v1 = sample_block().header;
        let v2 = BlockHeader { version: 2, base_fee_per_gas: 8, ..v1.clone() };
        let mut expected = legacy_tuple(&v1);
        // Patch the version byte pair (bincode u32 LE) and append the fee.
        expected[0..4].copy_from_slice(&2u32.to_le_bytes());
        expected.extend_from_slice(&8u128.to_le_bytes());
        assert_eq!(encode_canonical(&v2), expected);
        assert_ne!(v1.hash(), v2.hash(), "the fee is part of the commitment");
        let decoded: BlockHeader = bincode::deserialize(&encode_canonical(&v2)).unwrap();
        assert_eq!(decoded, v2);
    }

    #[test]
    fn malformed_headers_are_refused() {
        let v1 = sample_block().header;
        // Version 2 with the fee field missing.
        let mut truncated = legacy_tuple(&v1);
        truncated[0..4].copy_from_slice(&2u32.to_le_bytes());
        assert!(bincode::deserialize::<BlockHeader>(&truncated).is_err());
        // Unknown version.
        let mut unknown = legacy_tuple(&v1);
        unknown[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(bincode::deserialize::<BlockHeader>(&unknown).is_err());
        // A legacy header cannot carry a fee in memory.
        let inconsistent = BlockHeader { base_fee_per_gas: 1, ..v1 };
        assert!(bincode::serialize(&inconsistent).is_err());
    }

    #[test]
    fn header_json_round_trips_both_versions() {
        let v1 = sample_block().header;
        let json = serde_json::to_string(&v1).unwrap();
        assert!(!json.contains("base_fee"), "legacy JSON keeps the exact field set");
        assert_eq!(serde_json::from_str::<BlockHeader>(&json).unwrap(), v1);

        let v2 = BlockHeader { version: 2, base_fee_per_gas: u128::MAX / 3, ..v1.clone() };
        let json = serde_json::to_string(&v2).unwrap();
        assert!(json.contains(&format!("\"{}\"", u128::MAX / 3)), "u128 as decimal string");
        assert_eq!(serde_json::from_str::<BlockHeader>(&json).unwrap(), v2);
        // A legacy document with a stray fee field is refused, not absorbed.
        let stray = json.replace("\"version\":2", "\"version\":1");
        assert!(serde_json::from_str::<BlockHeader>(&stray).is_err());
    }

    fn fee_policy() -> FeePolicy {
        FeePolicy {
            initial_base_fee_per_gas: 8,
            min_base_fee_per_gas: 1,
            max_base_fee_per_gas: Amount::MAX / u128::from(BLOCK_GAS_LIMIT),
            elasticity_multiplier: FEE_ELASTICITY_MULTIPLIER,
            change_denominator: FEE_CHANGE_DENOMINATOR,
        }
    }

    #[test]
    fn legacy_genesis_context_is_byte_identical() {
        let mut genesis = GenesisConfig { genesis_time: 1_700_000_000, ..Default::default() };
        genesis.allocations.insert(Address::new([5; ADDRESS_SIZE]), 42);
        assert!(genesis.fee_policy.is_none());
        let mut allocations: Vec<_> = genesis.allocations.iter().map(|(a, b)| (*a, *b)).collect();
        allocations.sort_unstable();
        let legacy = Hash::from_data(&encode_canonical(&(
            genesis.chain_id,
            &genesis.initial_validators,
            allocations,
            genesis.block_time_ms,
            genesis.gas_limit,
            genesis.genesis_time,
        )));
        assert_eq!(genesis.consensus_context(), legacy);

        // The policy binds into the context and changes it.
        genesis.fee_policy = Some(fee_policy());
        assert_ne!(genesis.consensus_context(), legacy);
        let mut other = fee_policy();
        other.min_base_fee_per_gas = 2;
        other.initial_base_fee_per_gas = 9;
        genesis.fee_policy = Some(other);
        assert_ne!(genesis.consensus_context(), legacy);
    }

    #[test]
    fn genesis_fee_policy_json_is_optional_and_validated() {
        let mut genesis = GenesisConfig { genesis_time: 1_700_000_000, ..Default::default() };
        let legacy_json = serde_json::to_string(&genesis).unwrap();
        assert!(!legacy_json.contains("fee_policy"), "legacy documents stay byte-stable");
        assert_eq!(serde_json::from_str::<GenesisConfig>(&legacy_json).unwrap(), genesis);

        genesis.fee_policy = Some(fee_policy());
        assert!(genesis.validate().is_ok());
        let json = serde_json::to_string(&genesis).unwrap();
        assert_eq!(serde_json::from_str::<GenesisConfig>(&json).unwrap(), genesis);

        let mut bad = fee_policy();
        bad.elasticity_multiplier = 4;
        genesis.fee_policy = Some(bad);
        assert!(genesis.validate().is_err());
        // Odd gas limit is refused through the policy too.
        genesis.fee_policy = Some(fee_policy());
        genesis.gas_limit = BLOCK_GAS_LIMIT + 1;
        assert!(genesis.validate().is_err());
    }
}
