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
use dashmap::DashMap;
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
    
    pub fn can_request(&self, address: &str) -> bool {
        if let Some(last_request) = self.requests.get(address) {
            if last_request.elapsed() < self.cooldown {
                return false;
            }
        }
        true
    }
    
    pub fn record_request(&self, address: &str) {
        self.requests.insert(address.to_string(), Instant::now());
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
    let address = req.address.trim().to_lowercase();
    
    if !is_valid_address(&address) {
        return Ok(AxumJson(FaucetResponse {
            success: false,
            tx_hash: None,
            error: Some("Invalid address format".to_string()),
            amount: "0".to_string(),
        }));
    }
    
    if !state.can_request(&address) {
        return Ok(AxumJson(FaucetResponse {
            success: false,
            tx_hash: None,
            error: Some("Cooldown period not elapsed".to_string()),
            amount: "0".to_string(),
        }));
    }
    
    state.record_request(&address);
    
    let tx_hash = format!("0x{}", subhost_core::hex::encode(blake3::hash(address.as_bytes()).as_bytes()));
    
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
