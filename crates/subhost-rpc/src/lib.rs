use jsonrpsee::types::error::ErrorObject;
use jsonrpsee::server::{Server, RpcModule};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, debug};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Invalid address")]
    InvalidAddress,
    
    #[error("Invalid transaction hash")]
    InvalidTxHash,
    
    #[error("Block not found")]
    BlockNotFound,
    
    #[error("Internal error")]
    InternalError,
}

impl From<RpcError> for ErrorObject<'static> {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::InvalidAddress => ErrorObject::owned(-32602, "Invalid address", None::<()>),
            RpcError::InvalidTxHash => ErrorObject::owned(-32602, "Invalid transaction hash", None::<()>),
            RpcError::BlockNotFound => ErrorObject::owned(-32000, "Block not found", None::<()>),
            RpcError::InternalError => ErrorObject::owned(-32603, "Internal error", None::<()>),
        }
    }
}

#[derive(Clone)]
pub struct RpcState {
    pub chain_id: u64,
}

impl RpcState {
    pub fn new(chain_id: u64) -> Self {
        Self { chain_id }
    }
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
        let server = Server::builder().build(addr).await?;
        let mut module = RpcModule::new(self.state.clone());
        
        module.register_method("eth_chainId", |_params, ctx| {
            let chain_id = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    ctx.read().await.chain_id
                })
            });
            Ok::<_, RpcError>(format!("0x{:x}", chain_id))
        })?;
        
        module.register_method("eth_blockNumber", |_params, _ctx| {
            Ok::<_, RpcError>("0x0".to_string())
        })?;
        
        module.register_method("eth_getBalance", |params, _ctx| {
            let address: String = params.one().map_err(|e| RpcError::InvalidAddress)?;
            if !address.starts_with("0x") || address.len() != 42 {
                return Err(RpcError::InvalidAddress);
            }
            Ok::<_, RpcError>("0x0".to_string())
        })?;
        
        module.register_method("eth_sendTransaction", |params, ctx| {
            let tx: serde_json::Value = params.one().map_err(|_| RpcError::InvalidAddress)?;
            let _from = tx.get("from").and_then(|v| v.as_str()).ok_or(RpcError::InvalidAddress)?;
            let _to = tx.get("to").and_then(|v| v.as_str()).ok_or(RpcError::InvalidAddress)?;
            let _chain_id = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    ctx.read().await.chain_id
                })
            });
            let tx_hash = format!("0x{}", hex::encode(blake3::hash(b"tx").as_bytes()));
            Ok::<_, RpcError>(tx_hash)
        })?;
        
        module.register_method("eth_getTransactionReceipt", |params, _ctx| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            if !tx_hash.starts_with("0x") || tx_hash.len() != 66 {
                return Err(RpcError::InvalidTxHash);
            }
            let receipt = serde_json::json!({
                "transactionHash": tx_hash,
                "status": "0x1",
                "blockNumber": "0x0",
            });
            Ok::<_, RpcError>(receipt)
        })?;
        
        module.register_method("eth_gasPrice", |_params, _ctx| {
            Ok::<_, RpcError>("0x1".to_string())
        })?;
        
        module.register_method("net_version", |_params, ctx| {
            let chain_id = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    ctx.read().await.chain_id
                })
            });
            Ok::<_, RpcError>(chain_id.to_string())
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
            server.run(addr).await?;
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
}
