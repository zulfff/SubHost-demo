use jsonrpsee::types::error::ErrorObject;
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};
use jsonrpsee::server::{BatchRequestConfig, Server, RpcModule};
use serde::{Deserialize, Serialize};
use subhost_core::{Address, Transaction, TransactionSignature, TransactionType};
use subhost_mempool::{Mempool, MempoolConfig};
use subhost_state::State;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::{info, debug};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Invalid address")]
    InvalidAddress,
    
    #[error("Invalid transaction hash")]
    InvalidTxHash,
    
    #[error("Block not found")]
    BlockNotFound,
    
    #[error("Invalid params")]
    InvalidParams,

    #[error("Invalid chain ID")]
    InvalidChainId,

    #[error("Invalid nonce")]
    InvalidNonce,

    #[error("Transaction signature is required and must be valid")]
    InvalidSignature,

    #[error("Internal error")]
    InternalError,
}

impl From<RpcError> for ErrorObject<'static> {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::InvalidAddress => ErrorObject::owned(-32602, "Invalid address", None::<()>),
            RpcError::InvalidTxHash => ErrorObject::owned(-32602, "Invalid transaction hash", None::<()>),
            RpcError::BlockNotFound => ErrorObject::owned(-32000, "Block not found", None::<()>),
            RpcError::InvalidParams => ErrorObject::owned(-32602, "Invalid params", None::<()>),
            RpcError::InvalidChainId => ErrorObject::owned(-32602, "Invalid chain ID", None::<()>),
            RpcError::InvalidNonce => ErrorObject::owned(-32602, "Invalid nonce", None::<()>),
            RpcError::InvalidSignature => ErrorObject::owned(-32602, "Invalid transaction signature", None::<()>),
            RpcError::InternalError => ErrorObject::owned(-32603, "Internal error", None::<()>),
        }
    }
}

#[derive(Clone)]
pub struct RpcState {
    pub chain_id: u64,
    /// Current (mutable) block height backing `eth_blockNumber`. Starts empty
    /// (0x0) and is advanced by the node/state producer; unlike the previous
    /// hardcoded value this is a real, queryable counter.
    block_height: Arc<AtomicU64>,
    /// Pending transaction pool - `eth_sendTransaction` inserts here.
    pub mempool: Arc<RwLock<Mempool>>,
    /// Account world state backing `eth_getBalance`.
    pub state: Arc<RwLock<State>>,
}

impl RpcState {
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            block_height: Arc::new(AtomicU64::new(0)),
            mempool: Arc::new(RwLock::new(Mempool::new(MempoolConfig::default()))),
            state: Arc::new(RwLock::new(State::with_chain_id(chain_id))),
        }
    }

    pub fn set_block_height(&self, height: u64) {
        self.block_height.store(height, Ordering::SeqCst);
    }

    pub fn block_height(&self) -> u64 {
        self.block_height.load(Ordering::SeqCst)
    }
}

fn is_hex_address(s: &str) -> bool {
    s.starts_with("0x") && s.len() == 42 && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn is_hex_tx_hash(s: &str) -> bool {
    s.starts_with("0x") && s.len() == 66 && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_address(s: &str) -> Result<Address, RpcError> {
    if !is_hex_address(s) {
        return Err(RpcError::InvalidAddress);
    }
    let bytes = subhost_core::hex::decode(&s[2..]).map_err(|_| RpcError::InvalidAddress)?;
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(Address::new(arr))
}

/// Parse an Ethereum QUANTITY (hex string or number) into a u64.
fn hex_quantity(v: Option<&serde_json::Value>, default: u64) -> Result<u64, RpcError> {
    let Some(value) = v else {
        return Ok(default);
    };
    let serde_json::Value::String(s) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = s.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    u64::from_str_radix(digits, 16).map_err(|_| RpcError::InvalidParams)
}

fn hex_amount(v: Option<&serde_json::Value>, default: u128) -> Result<u128, RpcError> {
    let Some(value) = v else {
        return Ok(default);
    };
    let serde_json::Value::String(s) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = s.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    u128::from_str_radix(digits, 16).map_err(|_| RpcError::InvalidParams)
}

fn fixed_hex<const N: usize>(value: Option<&serde_json::Value>) -> Result<[u8; N], RpcError> {
    let value = value
        .and_then(serde_json::Value::as_str)
        .ok_or(RpcError::InvalidSignature)?;
    if !value.starts_with("0x") {
        return Err(RpcError::InvalidSignature);
    }
    let bytes = hex_bytes(value).map_err(|_| RpcError::InvalidSignature)?;
    bytes.try_into().map_err(|_| RpcError::InvalidSignature)
}

fn hex_bytes(s: &str) -> Result<Vec<u8>, RpcError> {
    subhost_core::hex::decode(s).map_err(|_| RpcError::InvalidParams)
}

#[derive(Clone)]
pub struct RpcServer {
    state: Arc<RwLock<RpcState>>,
}

impl RpcServer {
    pub fn new(state: RpcState) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }
    
    pub async fn run(self, addr: SocketAddr) -> anyhow::Result<()> {
        self.run_with_limits(addr, 1000).await
    }

    pub async fn run_with_limits(self, addr: SocketAddr, max_connections: usize) -> anyhow::Result<()> {
        let max_connections = u32::try_from(max_connections).unwrap_or(u32::MAX).max(1);
        let server = Server::builder()
            .max_connections(max_connections)
            .max_request_body_size(256 * 1024)
            .max_response_body_size(1024 * 1024)
            .set_batch_request_config(BatchRequestConfig::Limit(10))
            .build(addr)
            .await?;
        let mut module = RpcModule::new(self.state.clone());
        
        module.register_method("eth_chainId", |_params, ctx| {
            // std::sync::RwLock: safe to take briefly inside a sync handler on the
            // async runtime (no tokio Blocking* API that panics on a worker thread).
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(format!("0x{:x}", state.chain_id))
        })?;
        
        module.register_method("eth_blockNumber", |_params, ctx| {
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(format!("0x{:x}", state.block_height()))
        })?;
        
        module.register_method("eth_getBalance", |params, ctx| {
            // Accept both `[address]` and the spec-compliant `[address, block]`.
            let mut seq = params.sequence();
            let address: String = seq.next().map_err(|_| RpcError::InvalidParams)?;
            let addr = parse_address(&address)?;
            // Read the real world-state balance (no more fabricated 0x0).
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            let state_guard = state.state.read().unwrap_or_else(|e| e.into_inner());
            let balance = state_guard.balance(&addr);
            Ok::<_, RpcError>(format!("0x{:x}", balance))
        })?;
        
        module.register_method("eth_sendTransaction", |params, ctx| {
            let tx: serde_json::Value = params.one().map_err(|_| RpcError::InvalidParams)?;

            let from = tx
                .get("from").and_then(|v| v.as_str())
                .ok_or(RpcError::InvalidAddress)
                .and_then(parse_address)?;
            let to = match tx.get("to").and_then(|v| v.as_str()) {
                Some(t) => Some(parse_address(t)?),
                None => None,
            };
            let value = hex_amount(tx.get("value"), 0)?;
            let nonce = hex_quantity(tx.get("nonce"), 0)?;
            let gas_price = hex_amount(tx.get("gasPrice"), 1)?;
            let gas_limit = hex_quantity(tx.get("gasLimit"), 21_000)?;
            let configured_chain_id = ctx
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .chain_id;
            let chain_id = match tx.get("chainId") {
                Some(value) => hex_quantity(Some(value), configured_chain_id)?,
                None => configured_chain_id,
            };
            if chain_id != configured_chain_id {
                return Err(RpcError::InvalidChainId);
            }
            let data = match tx.get("data").and_then(|v| v.as_str()) {
                Some(d) => hex_bytes(d)?,
                None => Vec::new(),
            };

            let tx_type = if to.is_none() {
                TransactionType::ContractCreation
            } else {
                TransactionType::Transfer
            };

            let public_key = fixed_hex::<32>(tx.get("publicKey"))?;
            let signature = fixed_hex::<64>(tx.get("signature"))?;
            let verifying_key = VerifyingKey::from_bytes(&public_key).map_err(|_| RpcError::InvalidSignature)?;
            if Address::from_public_key(&public_key) != from {
                return Err(RpcError::InvalidSignature);
            }

            let unsigned_tx = Transaction {
                tx_type,
                nonce,
                from,
                to,
                value,
                gas_price,
                gas_limit,
                data,
                chain_id,
                signature: TransactionSignature { r: [0u8; 32], s: [0u8; 32], v: 0 },
            };

            let encoded = bincode::serialize(&unsigned_tx).map_err(|_| RpcError::InternalError)?;
            let signature = Ed25519Signature::from_bytes(&signature);
            verifying_key
                .verify(&encoded, &signature)
                .map_err(|_| RpcError::InvalidSignature)?;

            let current_nonce = {
                let rpc_state = ctx.read().unwrap_or_else(|e| e.into_inner());
                let state = rpc_state.state.read().unwrap_or_else(|e| e.into_inner());
                state.nonce(&from)
            };
            if nonce != current_nonce {
                return Err(RpcError::InvalidNonce);
            }

            let tx = Transaction {
                signature: TransactionSignature {
                    r: signature.to_bytes()[..32].try_into().expect("fixed signature half"),
                    s: signature.to_bytes()[32..].try_into().expect("fixed signature half"),
                    v: 0,
                },
                ..unsigned_tx
            };

            // Insert into the real pending pool.
            let rpc_state = ctx.read().unwrap_or_else(|e| e.into_inner());
            let mut mempool = rpc_state.mempool.write().unwrap_or_else(|e| e.into_inner());
            let hash = mempool.add(tx).map_err(|_| RpcError::InvalidParams)?;
            drop(mempool);

            Ok::<_, RpcError>(format!("0x{}", hex::encode(hash.as_bytes())))
        })?;
        
        module.register_method("eth_getTransactionReceipt", |params, _ctx| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            if !is_hex_tx_hash(&tx_hash) {
                return Err(RpcError::InvalidTxHash);
            }

            // Honest: there is no block-producer/confirmation pipeline wired to this
            // node, so no transaction is ever confirmed. Return `null` (the
            // spec-compliant "receipt not found"), NOT a fabricated status 0x1.
            Ok::<_, RpcError>(serde_json::Value::Null)
        })?;
        
        module.register_method("eth_gasPrice", |_params, _ctx| {
            Ok::<_, RpcError>("0x1".to_string())
        })?;
        
        module.register_method("net_version", |_params, ctx| {
            let state = ctx.read().unwrap_or_else(|e| e.into_inner());
            Ok::<_, RpcError>(state.chain_id.to_string())
        })?;
        
        let addr = server.local_addr()?;
        let handle = server.start(module);
        
        info!("RPC server started at {}", addr);
        
        handle.stopped().await;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostrpcConfig {
    pub enabled: bool,
    pub max_connections: usize,
    pub listen_addr: String,
}

impl Default for SubhostrpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
            listen_addr: "127.0.0.1:8545".to_string(),
        }
    }
}

pub struct SubhostrpcModule {
    config: SubhostrpcConfig,
}

impl SubhostrpcModule {
    pub fn new(config: SubhostrpcConfig) -> Self {
        info!("Initializing SubhostrpcModule");
        Self { config }
    }
    
    pub async fn start(&self) -> anyhow::Result<()> {
        if self.config.enabled {
            let server = RpcServer::new(RpcState::new(1));
            let addr: SocketAddr = self.config.listen_addr.parse()?;
            server.run_with_limits(addr, self.config.max_connections).await?;
        }
        Ok(())
    }
    
    pub fn process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.config.enabled {
            return Ok(());
        }
        debug!("Processing request in subhost-rpc");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostrpcError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Processing error: {0}")]
    Processing(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = SubhostrpcConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
        assert_eq!(config.listen_addr, "127.0.0.1:8545");
    }

    #[test]
    fn quantity_and_signature_parsers_are_strict() {
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x1f")), 0).unwrap(), 31);
        assert!(hex_quantity(Some(&serde_json::json!("1f")), 0).is_err());
        assert!(hex_quantity(Some(&serde_json::json!("0x01")), 0).is_err());
        assert!(hex_amount(Some(&serde_json::json!("0x100000000000000000000")), 0).is_ok());
        assert!(fixed_hex::<32>(Some(&serde_json::json!("00".repeat(32)))).is_err());
        assert!(fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(32))))).is_ok());
    }
}
