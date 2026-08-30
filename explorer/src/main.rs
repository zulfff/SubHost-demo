//! Read-only block explorer with a local demo signing endpoint.
//!
//! # Security scope
//!
//! The `/api/transfer` endpoint decrypts a local wallet with a password supplied
//! in the request body and signs on the caller's behalf. That is only acceptable
//! for a single-operator demo, so [`ExplorerServer::run`] refuses to bind to
//! anything but a loopback address, and the wallet directory must be pointed at
//! explicitly through the environment. Everything else is read-only proxying.

use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use subhost_core::{Address, Amount, ChainId, Transaction, TransactionSignature, TransactionType};
use subhost_telemetry::{TelemetryConfig, Verbosity};
use subhost_wallet::Wallet;
use tokio::sync::Mutex;
use tracing::info;

/// Blocks shown in the dashboard.
const MAX_BLOCKS: u64 = 12;
/// Largest accepted RPC response.
const MAX_RPC_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Largest accepted request body.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Largest wallet file parsed while scanning.
const MAX_WALLET_FILE_BYTES: u64 = 1024 * 1024;
/// Gas limit used for a demo transfer.
const TRANSFER_GAS_LIMIT: u64 = 21_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerConfig {
    pub listen_addr: SocketAddr,
    pub rpc_url: String,
    pub chain_name: String,
    /// Wallet directory backing the demo signing endpoint. When absent, the
    /// account and transfer endpoints report that they are unavailable.
    pub wallet_dir: Option<PathBuf>,
}

impl Default for ExplorerConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 3000)),
            rpc_url: "http://127.0.0.1:8545".to_string(),
            chain_name: "SubHost".to_string(),
            wallet_dir: None,
        }
    }
}

#[derive(Clone)]
pub struct ExplorerState {
    config: Arc<ExplorerConfig>,
    client: reqwest::Client,
    /// Serializes nonce read and submit so two transfers cannot reuse a nonce.
    transfer_lock: Arc<Mutex<()>>,
}

impl ExplorerState {
    pub fn new(config: ExplorerConfig) -> Result<Self, ExplorerError> {
        if !config.rpc_url.starts_with("http://") && !config.rpc_url.starts_with("https://") {
            return Err(ExplorerError::Config("rpc_url must be an http(s) URL".into()));
        }
        Ok(Self {
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| ExplorerError::Config(error.to_string()))?,
            transfer_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn config(&self) -> &ExplorerConfig {
        &self.config
    }

    fn wallet_dir(&self) -> Result<&Path, ExplorerError> {
        self.config.wallet_dir.as_deref().ok_or(ExplorerError::WalletsUnavailable)
    }

    /// Chain tip, recent blocks, and their transactions.
    pub async fn snapshot(&self) -> Result<Snapshot, ExplorerError> {
        let height = self.quantity("eth_blockNumber", json!([])).await?;
        let first = height.saturating_sub(MAX_BLOCKS.saturating_sub(1)).max(1);
        let mut blocks = Vec::new();
        let mut transactions = Vec::new();

        // Walk newest first so the dashboard shows the tip immediately.
        for number in (first..=height).rev() {
            if height == 0 {
                break;
            }
            let block = self
                .rpc_call("eth_getBlockByNumber", json!([format!("0x{number:x}"), true]))
                .await?;
            let Some(object) = block.as_object() else {
                continue;
            };

            let block_hash = string_field(object, "hash").unwrap_or_default();
            let block_number = object.get("number").and_then(quantity).unwrap_or(number);
            let block_transactions =
                object.get("transactions").and_then(Value::as_array).cloned().unwrap_or_default();

            blocks.push(BlockView {
                number: block_number,
                hash: block_hash.clone(),
                parent_hash: string_field(object, "parentHash").unwrap_or_default(),
                timestamp: object.get("timestamp").and_then(quantity).unwrap_or_default(),
                gas_used: string_field(object, "gasUsed").unwrap_or_default(),
                transaction_count: block_transactions.len(),
            });

            for tx in &block_transactions {
                let Some(tx) = tx.as_object() else { continue };
                let Some(hash) = string_field(tx, "hash") else {
                    continue;
                };
                transactions.push(TransactionView {
                    hash,
                    block_number,
                    block_hash: block_hash.clone(),
                    from: string_field(tx, "from").unwrap_or_else(|| "-".to_string()),
                    to: string_field(tx, "to"),
                    value: tx.get("value").and_then(amount).unwrap_or_default().to_string(),
                    nonce: tx.get("nonce").and_then(quantity).unwrap_or_default(),
                    kind: string_field(tx, "type").unwrap_or_else(|| "transfer".to_string()),
                });
            }
        }

        Ok(Snapshot {
            chain_name: self.config.chain_name.clone(),
            rpc_url: self.config.rpc_url.clone(),
            chain_id: format!("0x{:x}", self.chain_id().await?),
            height,
            latest_hash: blocks.first().map(|block| block.hash.clone()).unwrap_or_default(),
            blocks,
            transactions,
            updated_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Demo accounts and their balances.
    pub async fn accounts(&self) -> Result<Vec<AccountView>, ExplorerError> {
        let dir = self.wallet_dir()?;
        let mut accounts = Vec::new();
        for (path, wallet) in list_wallets(dir)? {
            let address = wallet.address().to_string();
            let balance =
                self.rpc_call("eth_getBalance", json!([address.clone(), "latest"])).await?;
            accounts.push(AccountView {
                label: path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("wallet")
                    .to_string(),
                address,
                balance: amount(&balance).unwrap_or_default().to_string(),
            });
        }
        accounts.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(accounts)
    }

    /// One transaction receipt.
    pub async fn receipt(&self, tx_hash: &str) -> Result<Value, ExplorerError> {
        if !is_tx_hash(tx_hash) {
            return Err(ExplorerError::InvalidRequest(
                "transaction hash must be 0x followed by 64 hex characters".into(),
            ));
        }
        self.rpc_call("eth_getTransactionReceipt", json!([tx_hash])).await
    }

    /// Sign and submit a demo transfer using a local wallet.
    pub async fn transfer(
        &self,
        request: TransferRequest,
    ) -> Result<TransferResponse, ExplorerError> {
        let dir = self.wallet_dir()?;
        let from = parse_address(&request.from)?;
        let to = parse_address(&request.to)?;
        if from == to {
            return Err(ExplorerError::InvalidRequest("sender and recipient must differ".into()));
        }
        let amount = request.amount.trim().parse::<Amount>().map_err(|_| {
            ExplorerError::InvalidRequest("amount must be a positive integer".into())
        })?;
        if amount == 0 {
            return Err(ExplorerError::InvalidRequest("amount must be greater than zero".into()));
        }

        let (path, _) = list_wallets(dir)?
            .into_iter()
            .find(|(_, wallet)| wallet.matches(&from))
            .ok_or(ExplorerError::WalletNotFound(from))?;
        let (_, private_key) =
            Wallet::load(&path, &request.password).map_err(|_| ExplorerError::WalletLocked)?;
        let signing_key = SigningKey::from_bytes(&private_key.0);

        // Hold the lock across the nonce read and the submit.
        let _guard = self.transfer_lock.lock().await;
        let chain_id = self.chain_id().await?;
        let nonce =
            self.quantity("eth_getTransactionCount", json!([from.to_string(), "latest"])).await?;

        let unsigned = Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from,
            to: Some(to),
            value: amount,
            gas_price: 1,
            gas_limit: TRANSFER_GAS_LIMIT,
            data: Vec::new(),
            chain_id,
            signature: TransactionSignature::EMPTY,
        };
        let signature = signing_key.sign(&unsigned.signing_payload()).to_bytes();

        let result = self
            .rpc_call(
                "eth_sendTransaction",
                json!([{
                    "from": from.to_string(),
                    "to": to.to_string(),
                    "value": format!("0x{amount:x}"),
                    "nonce": format!("0x{nonce:x}"),
                    "gasPrice": "0x1",
                    "gas": format!("0x{TRANSFER_GAS_LIMIT:x}"),
                    "chainId": format!("0x{chain_id:x}"),
                    "data": "0x",
                    "publicKey": format!("0x{}", hex::encode(signing_key.verifying_key().to_bytes())),
                    "signature": format!("0x{}", hex::encode(signature)),
                }]),
            )
            .await?;

        Ok(TransferResponse {
            hash: result
                .as_str()
                .ok_or_else(|| ExplorerError::Rpc("node returned an invalid hash".into()))?
                .to_string(),
            from: from.to_string(),
            to: to.to_string(),
            amount: amount.to_string(),
            nonce,
        })
    }

    async fn chain_id(&self) -> Result<ChainId, ExplorerError> {
        self.quantity("eth_chainId", json!([])).await
    }

    async fn quantity(&self, method: &str, params: Value) -> Result<u64, ExplorerError> {
        let value = self.rpc_call(method, params).await?;
        quantity(&value)
            .ok_or_else(|| ExplorerError::Rpc(format!("{method} returned invalid data")))
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value, ExplorerError> {
        let mut response = self
            .client
            .post(&self.config.rpc_url)
            .json(&json!({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}))
            .send()
            .await
            .map_err(|error| ExplorerError::Rpc(format!("{method} request failed: {error}")))?;
        if !response.status().is_success() {
            return Err(ExplorerError::Rpc(format!(
                "{method} returned HTTP {}",
                response.status()
            )));
        }

        // Bound the body so a broken node cannot exhaust memory.
        if response.content_length().is_some_and(|length| length > MAX_RPC_RESPONSE_BYTES as u64) {
            return Err(ExplorerError::Rpc("response is too large".into()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| ExplorerError::Rpc(format!("response read failed: {error}")))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
                return Err(ExplorerError::Rpc("response is too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }

        let body: Value = serde_json::from_slice(&bytes).map_err(|error| {
            ExplorerError::Rpc(format!("{method} returned invalid JSON: {error}"))
        })?;
        if let Some(error) = body.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(ExplorerError::Rpc(format!("{method} failed: {message}")));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| ExplorerError::Rpc(format!("{method} response has no result")))
    }
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub chain_name: String,
    pub rpc_url: String,
    pub chain_id: String,
    pub height: u64,
    pub latest_hash: String,
    pub blocks: Vec<BlockView>,
    pub transactions: Vec<TransactionView>,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct BlockView {
    pub number: u64,
    pub hash: String,
    pub parent_hash: String,
    pub timestamp: u64,
    pub gas_used: String,
    pub transaction_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransactionView {
    pub hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub from: String,
    pub to: Option<String>,
    pub value: String,
    pub nonce: u64,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub label: String,
    pub address: String,
    pub balance: String,
}

#[derive(Debug, Deserialize)]
pub struct TransferRequest {
    pub from: String,
    pub to: String,
    pub amount: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct TransferResponse {
    pub hash: String,
    pub from: String,
    pub to: String,
    pub amount: String,
    pub nonce: u64,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub struct ExplorerServer {
    state: ExplorerState,
}

impl ExplorerServer {
    pub fn new(state: ExplorerState) -> Self {
        Self { state }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(handle_index))
            .route("/health", get(|| async { "ok" }))
            .route("/api/snapshot", get(api_snapshot))
            .route("/api/accounts", get(api_accounts))
            .route("/api/transfer", post(api_transfer))
            .route("/api/tx/{hash}", get(api_transaction))
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            .with_state(self.state.clone())
    }

    /// Serve on a loopback address.
    ///
    /// Binding elsewhere is refused: `/api/transfer` accepts a wallet password and
    /// signs with a local key, which must never be reachable off-host.
    pub async fn run(self) -> Result<(), ExplorerError> {
        let addr = self.state.config.listen_addr;
        if !addr.ip().is_loopback() {
            return Err(ExplorerError::Config(
                "the demo signing explorer may only listen on a loopback address".into(),
            ));
        }
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|source| ExplorerError::Bind { addr, source })?;
        info!(%addr, rpc = %self.state.config.rpc_url, "explorer listening");
        axum::serve(listener, self.router()).await.map_err(ExplorerError::Serve)
    }
}

async fn handle_index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn api_snapshot(
    State(state): State<ExplorerState>,
) -> Result<Json<Snapshot>, (StatusCode, Json<ApiError>)> {
    state.snapshot().await.map(Json).map_err(api_error)
}

async fn api_accounts(
    State(state): State<ExplorerState>,
) -> Result<Json<Vec<AccountView>>, (StatusCode, Json<ApiError>)> {
    state.accounts().await.map(Json).map_err(api_error)
}

async fn api_transfer(
    State(state): State<ExplorerState>,
    Json(request): Json<TransferRequest>,
) -> Result<Json<TransferResponse>, (StatusCode, Json<ApiError>)> {
    state.transfer(request).await.map(Json).map_err(api_error)
}

async fn api_transaction(
    State(state): State<ExplorerState>,
    AxumPath(hash): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    state.receipt(&hash).await.map(Json).map_err(api_error)
}

/// Map an explorer error onto an HTTP status.
fn api_error(error: ExplorerError) -> (StatusCode, Json<ApiError>) {
    let status = match &error {
        ExplorerError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        ExplorerError::WalletNotFound(_) => StatusCode::NOT_FOUND,
        ExplorerError::WalletLocked => StatusCode::UNAUTHORIZED,
        ExplorerError::WalletsUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ExplorerError::Rpc(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(ApiError { error: error.to_string() }))
}

/// Read every wallet in `dir`, skipping unrelated and oversized files.
fn list_wallets(dir: &Path) -> Result<Vec<(PathBuf, Wallet)>, ExplorerError> {
    let mut wallets = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|source| ExplorerError::Io { path: dir.to_path_buf(), source })?;
    for entry in entries {
        let path =
            entry.map_err(|source| ExplorerError::Io { path: dir.to_path_buf(), source })?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|source| ExplorerError::Io { path: path.clone(), source })?;
        if metadata.len() > MAX_WALLET_FILE_BYTES {
            continue;
        }
        if let Ok(wallet) = Wallet::read(&path) {
            wallets.push((path, wallet));
        }
    }
    wallets.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(wallets)
}

fn parse_address(value: &str) -> Result<Address, ExplorerError> {
    let trimmed = value.trim();
    if trimmed.len() != 42 || !trimmed.starts_with("0x") {
        return Err(ExplorerError::InvalidRequest(
            "address must be 0x followed by 40 hex characters".into(),
        ));
    }
    Address::from_hex(trimmed)
        .map_err(|_| ExplorerError::InvalidRequest("address is not valid hex".into()))
}

fn is_tx_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(ToString::to_string)
}

fn quantity(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => u64::from_str_radix(text.strip_prefix("0x")?, 16).ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

fn amount(value: &Value) -> Option<Amount> {
    match value {
        Value::String(text) => Amount::from_str_radix(text.strip_prefix("0x")?, 16).ok(),
        Value::Number(number) => number.as_u128(),
        _ => None,
    }
}

/// Resolve the demo wallet directory from the environment.
///
/// `SUBHOST_TEST_HOME` wins; otherwise the two-user setup script's environment
/// file is consulted. Returning `None` disables the wallet endpoints rather than
/// guessing at a directory.
pub fn wallet_dir_from_env() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("SUBHOST_TEST_HOME") {
        return Some(PathBuf::from(home).join(".subhost").join("wallets"));
    }
    let env_path = std::env::var("SUBHOST_TWO_USERS_ENV")
        .unwrap_or_else(|_| "/tmp/subhost-two-users-current.env".to_string());
    let content = std::fs::read_to_string(env_path).ok()?;
    let home = content.lines().find_map(|line| {
        let value = line.strip_prefix("export SUBHOST_TEST_HOME=")?;
        Some(value.trim().trim_matches(['\'', '"']).to_string())
    })?;
    Some(PathBuf::from(home).join(".subhost").join("wallets"))
}

#[derive(Debug, thiserror::Error)]
pub enum ExplorerError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("no wallet found for {0}")]
    WalletNotFound(Address),

    #[error("wrong wallet password")]
    WalletLocked,

    #[error("no demo wallet directory is configured; set SUBHOST_TEST_HOME")]
    WalletsUnavailable,

    #[error("node RPC error: {0}")]
    Rpc(String),

    #[error("invalid explorer configuration: {0}")]
    Config(String),

    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot bind the explorer to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("explorer stopped: {0}")]
    Serve(#[source] std::io::Error),
}

fn main() -> anyhow::Result<()> {
    subhost_telemetry::init_or_warn(TelemetryConfig::from_env(Verbosity::Normal));

    let defaults = ExplorerConfig::default();
    let config = ExplorerConfig {
        listen_addr: match std::env::var("EXPLORER_LISTEN_ADDR") {
            Ok(value) => value
                .parse()
                .map_err(|error| anyhow::anyhow!("EXPLORER_LISTEN_ADDR is invalid: {error}"))?,
            Err(_) => defaults.listen_addr,
        },
        rpc_url: std::env::var("SUBHOST_RPC_URL").unwrap_or(defaults.rpc_url),
        chain_name: std::env::var("SUBHOST_CHAIN_NAME").unwrap_or(defaults.chain_name),
        wallet_dir: wallet_dir_from_env(),
    };

    let server = ExplorerServer::new(ExplorerState::new(config)?);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(server.run())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(config: ExplorerConfig) -> ExplorerState {
        ExplorerState::new(config).unwrap()
    }

    #[test]
    fn configuration_requires_an_http_rpc_url() {
        assert!(ExplorerState::new(ExplorerConfig::default()).is_ok());
        assert!(matches!(
            ExplorerState::new(ExplorerConfig {
                rpc_url: "ftp://node".into(),
                ..Default::default()
            }),
            Err(ExplorerError::Config(_))
        ));
    }

    #[test]
    fn address_and_hash_validation_are_strict() {
        assert!(parse_address("0x1111111111111111111111111111111111111111").is_ok());
        assert!(parse_address("  0x1111111111111111111111111111111111111111 ").is_ok());
        for bad in ["", "0x", "0xzz", "1111111111111111111111111111111111111111"] {
            assert!(parse_address(bad).is_err(), "{bad:?} must be rejected");
        }

        assert!(is_tx_hash(&format!("0x{}", "11".repeat(32))));
        assert!(!is_tx_hash(&format!("0x{}", "11".repeat(31))));
        assert!(!is_tx_hash(&"11".repeat(32)));
    }

    #[test]
    fn json_field_helpers_handle_both_shapes_and_junk() {
        assert_eq!(quantity(&json!("0x1f")), Some(31));
        assert_eq!(quantity(&json!(31)), Some(31));
        assert_eq!(quantity(&json!("1f")), None);
        assert_eq!(amount(&json!("0xa")), Some(10));
        assert_eq!(amount(&json!(10)), Some(10));
        assert_eq!(amount(&json!(null)), None);

        let object = json!({"hash": "0xabc", "number": 7});
        let object = object.as_object().unwrap();
        assert_eq!(string_field(object, "hash").as_deref(), Some("0xabc"));
        assert_eq!(string_field(object, "number"), None, "a number is not a string");
        assert_eq!(string_field(object, "missing"), None);
    }

    #[tokio::test]
    async fn wallet_endpoints_report_unavailable_without_a_configured_directory() {
        let state = state(ExplorerConfig::default());
        assert!(matches!(state.accounts().await, Err(ExplorerError::WalletsUnavailable)));
        assert!(matches!(
            state
                .transfer(TransferRequest {
                    from: format!("0x{}", "11".repeat(20)),
                    to: format!("0x{}", "22".repeat(20)),
                    amount: "1".into(),
                    password: "password123".into(),
                })
                .await,
            Err(ExplorerError::WalletsUnavailable)
        ));
    }

    #[tokio::test]
    async fn transfer_validates_input_before_touching_wallets_or_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(ExplorerConfig {
            wallet_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        let sender = format!("0x{}", "11".repeat(20));

        for (from, to, amount) in [
            ("bad", "0x2222222222222222222222222222222222222222", "1"),
            (sender.as_str(), "bad", "1"),
            (sender.as_str(), sender.as_str(), "1"),
            (sender.as_str(), "0x2222222222222222222222222222222222222222", "0"),
            (sender.as_str(), "0x2222222222222222222222222222222222222222", "abc"),
        ] {
            assert!(
                matches!(
                    state
                        .transfer(TransferRequest {
                            from: from.to_string(),
                            to: to.to_string(),
                            amount: amount.to_string(),
                            password: "password123".into(),
                        })
                        .await,
                    Err(ExplorerError::InvalidRequest(_))
                ),
                "({from}, {to}, {amount}) must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn transfer_reports_a_missing_wallet_and_a_wrong_password() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Wallet::new("password123").unwrap();
        wallet.save(&dir.path().join("alice.json")).unwrap();
        let state = state(ExplorerConfig {
            wallet_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });

        assert!(matches!(
            state
                .transfer(TransferRequest {
                    from: format!("0x{}", "99".repeat(20)),
                    to: format!("0x{}", "22".repeat(20)),
                    amount: "1".into(),
                    password: "password123".into(),
                })
                .await,
            Err(ExplorerError::WalletNotFound(_))
        ));

        assert!(matches!(
            state
                .transfer(TransferRequest {
                    from: wallet.address().to_string(),
                    to: format!("0x{}", "22".repeat(20)),
                    amount: "1".into(),
                    password: "wrong password".into(),
                })
                .await,
            Err(ExplorerError::WalletLocked)
        ));
    }

    #[tokio::test]
    async fn receipt_rejects_a_malformed_hash_before_calling_the_node() {
        let state = state(ExplorerConfig {
            // Nothing listens here, so reaching the node would error.
            rpc_url: "http://127.0.0.1:1".into(),
            ..Default::default()
        });
        assert!(matches!(state.receipt("0x1234").await, Err(ExplorerError::InvalidRequest(_))));
        // A well-formed hash does reach the node, and the node is down.
        assert!(matches!(
            state.receipt(&format!("0x{}", "11".repeat(32))).await,
            Err(ExplorerError::Rpc(_))
        ));
    }

    #[test]
    fn wallet_listing_skips_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Wallet::new("password123").unwrap();
        wallet.save(&dir.path().join("alice.json")).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::write(dir.path().join("bogus.json"), "{}").unwrap();

        let wallets = list_wallets(dir.path()).unwrap();
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].1.address(), wallet.address());
        assert!(list_wallets(&dir.path().join("missing")).is_err());
    }

    #[tokio::test]
    async fn a_non_loopback_bind_is_refused() {
        let server = ExplorerServer::new(state(ExplorerConfig {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            ..Default::default()
        }));
        assert!(matches!(server.run().await, Err(ExplorerError::Config(_))));
    }

    #[test]
    fn error_statuses_distinguish_client_and_upstream_failures() {
        assert_eq!(api_error(ExplorerError::InvalidRequest("x".into())).0, StatusCode::BAD_REQUEST);
        assert_eq!(
            api_error(ExplorerError::WalletNotFound(Address::ZERO)).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(api_error(ExplorerError::WalletLocked).0, StatusCode::UNAUTHORIZED);
        assert_eq!(api_error(ExplorerError::WalletsUnavailable).0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api_error(ExplorerError::Rpc("down".into())).0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            api_error(ExplorerError::Config("bad".into())).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn health_endpoint_answers_without_a_node() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = ExplorerServer::new(state(ExplorerConfig::default()));
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = server.router();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("ok"));

        task.abort();
    }

    #[test]
    fn wallet_dir_from_env_prefers_the_explicit_home() {
        std::env::set_var("SUBHOST_TEST_HOME", "/tmp/subhost-explorer-test");
        assert_eq!(
            wallet_dir_from_env(),
            Some(PathBuf::from("/tmp/subhost-explorer-test/.subhost/wallets"))
        );
        std::env::remove_var("SUBHOST_TEST_HOME");

        // With no home and no env file, the wallet endpoints stay disabled.
        std::env::set_var("SUBHOST_TWO_USERS_ENV", "/tmp/subhost-does-not-exist.env");
        assert_eq!(wallet_dir_from_env(), None);
        std::env::remove_var("SUBHOST_TWO_USERS_ENV");
    }
}
