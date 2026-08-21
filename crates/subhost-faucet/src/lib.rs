use axum::{
    extract::{State, Json},
    http::StatusCode,
    response::Json as AxumJson,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::{DashMap, mapref::entry::Entry};
use tokio::time;

#[derive(Clone)]
pub struct FaucetState {
    pub drip_amount: u128,
    pub cooldown: Duration,
    pub requests: Arc<DashMap<String, Instant>>,
}

impl FaucetState {
    pub fn new(drip_amount: u128, cooldown_secs: u64) -> Self {
        Self {
            drip_amount,
            cooldown: Duration::from_secs(cooldown_secs),
            requests: Arc::new(DashMap::new()),
        }
    }
    
    /// Atomically check and reserve the cooldown slot for one address.
    pub fn try_record_request(&self, address: &str) -> bool {
        let now = Instant::now();
        match self.requests.entry(address.to_string()) {
            Entry::Occupied(mut entry) => {
                if entry.get().elapsed() < self.cooldown {
                    false
                } else {
                    entry.insert(now);
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(now);
                true
            }
        }
    }
    
}

#[derive(Deserialize)]
pub struct FaucetRequest {
    pub address: String,
}

#[derive(Serialize)]
pub struct FaucetResponse {
    pub success: bool,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub amount: String,
}

pub struct FaucetServer {
    state: FaucetState,
}

impl FaucetServer {
    pub fn new(state: FaucetState) -> Self {
        Self { state }
    }
    
    pub async fn run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/drip", post(handle_drip))
            .route("/status", axum::routing::get(handle_status))
            .layer(tower_http::cors::CorsLayer::permissive())
            .layer(tower_http::limit::RequestBodyLimitLayer::new(1024))
            .with_state(self.state.clone());
        
        tokio::spawn(cleanup_old_requests(self.state.requests.clone()));
        
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        
        Ok(())
    }
}

async fn handle_drip(
    State(state): State<FaucetState>,
    Json(req): Json<FaucetRequest>,
) -> Result<AxumJson<FaucetResponse>, StatusCode> {
    // SECURITY: Preserve original case for proper address validation
    let address_original = req.address.trim().to_string();
    // Normalize to lowercase for validation + cooldown key: Ethereum addresses
    // are case-insensitive, so keying the rate limit on the raw(case-sensitive)
    // string would let a caller bypass the cooldown by flipping letter case.
    let address_norm = address_original.to_lowercase();

    if !is_valid_address(&address_norm) {
        return Ok(AxumJson(FaucetResponse {
            success: false,
            tx_hash: None,
            error: Some("Invalid address format".to_string()),
            amount: "0".to_string(),
        }));
    }
    
    if !state.try_record_request(&address_norm) {
        return Ok(AxumJson(FaucetResponse {
            success: false,
            tx_hash: None,
            error: Some("Cooldown period not elapsed".to_string()),
            amount: "0".to_string(),
        }));
    }
    
    // Generate deterministic tx hash from normalized address + time
    let mut hasher = blake3::Hasher::new();
    hasher.update(address_norm.as_bytes());
    hasher.update(&std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_be_bytes());
    let tx_hash = format!("0x{}", subhost_core::hex::encode(hasher.finalize().as_bytes()));
    
    Ok(AxumJson(FaucetResponse {
        success: true,
        tx_hash: Some(tx_hash),
        error: None,
        amount: state.drip_amount.to_string(),
    }))
}

async fn handle_status() -> AxumJson<serde_json::Value> {
    AxumJson(serde_json::json!({
        "status": "ok",
        "network": "testnet",
    }))
}

fn is_valid_address(address: &str) -> bool {
    address.starts_with("0x") && address.len() == 42 && address[2..].chars().all(|c| c.is_ascii_hexdigit())
}

async fn cleanup_old_requests(requests: Arc<DashMap<String, Instant>>) {
    let mut interval = time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        let now = Instant::now();
        requests.retain(|_, v| now.duration_since(*v) < Duration::from_secs(86400));
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FaucetConfig {
    pub enabled: bool,
    pub listen_addr: String,
    pub drip_amount: u128,
    pub cooldown_secs: u64,
}

impl Default for FaucetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_addr: "127.0.0.1:8080".to_string(),
            drip_amount: 1000000000000000000,
            cooldown_secs: 86400,
        }
    }
}

pub struct FaucetModule {
    config: FaucetConfig,
}

impl FaucetModule {
    pub fn new(config: FaucetConfig) -> Self {
        Self { config }
    }
    
    pub async fn start(&self) -> anyhow::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        
        let state = FaucetState::new(self.config.drip_amount, self.config.cooldown_secs);
        let server = FaucetServer::new(state);
        let addr: SocketAddr = self.config.listen_addr.parse()?;
        server.run(addr).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FaucetError {
    #[error("Rate limited")]
    RateLimited,
    #[error("Invalid address")]
    InvalidAddress,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_reservation_is_single_use() {
        let state = FaucetState::new(1, 60);
        assert!(state.try_record_request("0xabc"));
        assert!(!state.try_record_request("0xabc"));
    }
}
