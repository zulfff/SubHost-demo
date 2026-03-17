use axum::{
    extract::{Path, State},
    response::Html,
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use askama::Template;
use serde::Serialize;

#[derive(Clone)]
pub struct ExplorerState {
    pub rpc_url: String,
    pub chain_name: String,
}

impl ExplorerState {
    pub fn new(rpc_url: String, chain_name: String) -> Self {
        Self { rpc_url, chain_name }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    chain_name: String,
    block_height: u64,
}

#[derive(Template)]
#[template(path = "block.html")]
struct BlockTemplate {
    chain_name: String,
    block_height: u64,
    block_hash: String,
}

#[derive(Template)]
#[template(path = "tx.html")]
struct TxTemplate {
    chain_name: String,
    tx_hash: String,
    status: String,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountTemplate {
    chain_name: String,
    address: String,
    balance: String,
}

#[derive(Serialize)]
struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

pub struct ExplorerServer {
    state: Arc<RwLock<ExplorerState>>,
}

impl ExplorerServer {
    pub fn new(state: ExplorerState) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }
    
    pub async fn run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/", get(handle_index))
            .route("/block/:height", get(handle_block))
            .route("/tx/:hash", get(handle_tx))
            .route("/account/:address", get(handle_account))
            .route("/api/blocks", get(api_blocks))
            .route("/api/txs", get(api_transactions))
            .layer(tower_http::cors::CorsLayer::permissive())
            .with_state(self.state.clone());
        
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        
        Ok(())
    }
}

async fn handle_index(State(state): State<Arc<RwLock<ExplorerState>>>) -> Html<String> {
    let state = state.read().await;
    let template = IndexTemplate {
        chain_name: state.chain_name.clone(),
        block_height: 0,
    };
    Html(template.render().unwrap_or_default())
}

async fn handle_block(
    State(state): State<Arc<RwLock<ExplorerState>>>,
    Path(height): Path<u64>,
) -> Html<String> {
    let state = state.read().await;
    let template = BlockTemplate {
        chain_name: state.chain_name.clone(),
        block_height: height,
        block_hash: format!("0x{}", hex::encode(blake3::hash(&height.to_le_bytes()).as_bytes())),
    };
    Html(template.render().unwrap_or_default())
}

async fn handle_tx(
    State(state): State<Arc<RwLock<ExplorerState>>>,
    Path(hash): Path<String>,
) -> Html<String> {
    let state = state.read().await;
    let template = TxTemplate {
        chain_name: state.chain_name.clone(),
        tx_hash: hash,
        status: "Success".to_string(),
    };
    Html(template.render().unwrap_or_default())
}

async fn handle_account(
    State(state): State<Arc<RwLock<ExplorerState>>>,
    Path(address): Path<String>,
) -> Html<String> {
    let state = state.read().await;
    let template = AccountTemplate {
        chain_name: state.chain_name.clone(),
        address,
        balance: "0".to_string(),
    };
    Html(template.render().unwrap_or_default())
}

async fn api_blocks() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "blocks": []
    }))
}

async fn api_transactions() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "transactions": []
    }))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplorerConfig {
    pub enabled: bool,
    pub listen_addr: String,
    pub rpc_url: String,
    pub chain_name: String,
}

impl Default for ExplorerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_addr: "127.0.0.1:3000".to_string(),
            rpc_url: "http://localhost:8545".to_string(),
            chain_name: "SubHost".to_string(),
        }
    }
}

pub struct ExplorerModule {
    config: ExplorerConfig,
}

impl ExplorerModule {
    pub fn new(config: ExplorerConfig) -> Self {
        Self { config }
    }
    
    pub async fn start(&self) -> anyhow::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        
        let state = ExplorerState::new(self.config.rpc_url.clone(), self.config.chain_name.clone());
        let server = ExplorerServer::new(state);
        let addr: SocketAddr = self.config.listen_addr.parse()?;
        server.run(addr).await
    }
}
