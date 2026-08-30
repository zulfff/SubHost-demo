//! Rate-limited testnet faucet.
//!
//! The faucet holds one funded key and signs a real transfer for each request,
//! submitting it to a node over JSON-RPC. It never fabricates a transaction hash:
//! a request either yields the hash of a transaction the node accepted, or an
//! error explaining why it did not.
//!
//! # Security scope
//!
//! - The HTTP surface is unauthenticated by design and rate limited per address.
//!   Expose it behind a proxy that adds IP-level limits and TLS.
//! - The signing key is loaded from an encrypted wallet file; the password comes
//!   from the environment and is never logged or returned in a response.
//! - `drip_amount` is a hard per-request cap, and the cooldown key is the
//!   lowercased address, so letter-case variants cannot bypass the limit.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subhost_core::{Address, Amount, Transaction, TransactionSignature, TransactionType};
use subhost_wallet::Wallet;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Largest accepted request body.
pub const MAX_REQUEST_BYTES: usize = 1024;
/// Largest accepted RPC response.
const MAX_RPC_RESPONSE_BYTES: usize = 64 * 1024;
/// How often expired cooldown entries are swept.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
/// Gas limit used for a faucet transfer.
const TRANSFER_GAS_LIMIT: u64 = 21_000;
/// Environment variable holding the faucet wallet password.
pub const PASSWORD_ENV: &str = "SUBHOST_FAUCET_PASSWORD";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaucetConfig {
    pub listen_addr: SocketAddr,
    pub rpc_url: String,
    /// Encrypted wallet file that funds the drips.
    pub wallet_path: PathBuf,
    pub drip_amount: Amount,
    pub cooldown_secs: u64,
}

impl Default for FaucetConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            rpc_url: "http://127.0.0.1:8545".to_string(),
            wallet_path: PathBuf::from("faucet-wallet.json"),
            drip_amount: 1_000_000_000_000_000_000,
            cooldown_secs: 86_400,
        }
    }
}

impl FaucetConfig {
    fn validate(&self) -> Result<(), FaucetError> {
        if self.drip_amount == 0 {
            return Err(FaucetError::Config("drip_amount must be > 0".into()));
        }
        if self.cooldown_secs == 0 {
            return Err(FaucetError::Config("cooldown_secs must be > 0".into()));
        }
        if !self.rpc_url.starts_with("http://") && !self.rpc_url.starts_with("https://") {
            return Err(FaucetError::Config("rpc_url must be an http(s) URL".into()));
        }
        Ok(())
    }
}

/// Per-address cooldown ledger.
#[derive(Clone, Default)]
pub struct CooldownLedger {
    entries: Arc<DashMap<String, Instant>>,
    cooldown: Duration,
}

impl CooldownLedger {
    pub fn new(cooldown: Duration) -> Self {
        Self { entries: Arc::new(DashMap::new()), cooldown }
    }

    /// Atomically claim the slot for `address`, or refuse if it is still cooling
    /// down. Check-then-set in two steps would let concurrent requests both pass.
    pub fn try_claim(&self, address: &str) -> Result<(), Duration> {
        let now = Instant::now();
        match self.entries.entry(address.to_ascii_lowercase()) {
            Entry::Occupied(mut entry) => {
                let elapsed = entry.get().elapsed();
                if elapsed < self.cooldown {
                    return Err(self.cooldown - elapsed);
                }
                entry.insert(now);
                Ok(())
            }
            Entry::Vacant(entry) => {
                entry.insert(now);
                Ok(())
            }
        }
    }

    /// Release a slot after a failed drip so the caller may retry immediately.
    pub fn release(&self, address: &str) {
        self.entries.remove(&address.to_ascii_lowercase());
    }

    /// Drop entries whose cooldown has long expired.
    pub fn sweep(&self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, claimed| claimed.elapsed() < self.cooldown * 2);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The signing key and address that fund drips.
#[derive(Clone)]
pub struct FaucetSigner {
    signing_key: Arc<SigningKey>,
    address: Address,
}

impl FaucetSigner {
    /// Load and decrypt the faucet wallet.
    pub fn load(path: &Path, password: &str) -> Result<Self, FaucetError> {
        let (_, private_key) = Wallet::load(path, password)?;
        let signing_key = SigningKey::from_bytes(&private_key.0);
        let address = Address::from_public_key(&signing_key.verifying_key().to_bytes());
        Ok(Self { signing_key: Arc::new(signing_key), address })
    }

    /// Load using the password from [`PASSWORD_ENV`].
    pub fn from_env(path: &Path) -> Result<Self, FaucetError> {
        let password =
            std::env::var(PASSWORD_ENV).map_err(|_| FaucetError::MissingPassword(PASSWORD_ENV))?;
        Self::load(path, &password)
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Sign a transaction over its unsigned encoding.
    pub fn sign(&self, tx: &Transaction) -> TransactionSignature {
        TransactionSignature::from_ed25519(&self.signing_key.sign(&tx.signing_payload()).to_bytes())
    }
}

#[derive(Debug, Deserialize)]
pub struct FaucetRequest {
    pub address: String,
}

#[derive(Debug, Serialize)]
pub struct FaucetResponse {
    pub tx_hash: String,
    pub address: String,
    pub amount: String,
}

#[derive(Debug, Serialize)]
pub struct FaucetStatus {
    pub faucet_address: String,
    pub balance: String,
    pub drip_amount: String,
    pub cooldown_secs: u64,
    pub chain_id: String,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

/// Shared faucet state.
#[derive(Clone)]
pub struct FaucetState {
    config: Arc<FaucetConfig>,
    signer: FaucetSigner,
    cooldown: CooldownLedger,
    client: reqwest::Client,
    /// Serializes nonce read and submit so two requests cannot reuse a nonce.
    submit_lock: Arc<Mutex<()>>,
}

impl FaucetState {
    pub fn new(config: FaucetConfig, signer: FaucetSigner) -> Result<Self, FaucetError> {
        config.validate()?;
        let cooldown = CooldownLedger::new(Duration::from_secs(config.cooldown_secs));
        Ok(Self {
            config: Arc::new(config),
            signer,
            cooldown,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                // The faucet talks to one configured node; never follow redirects.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| FaucetError::Config(error.to_string()))?,
            submit_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn config(&self) -> &FaucetConfig {
        &self.config
    }

    pub fn faucet_address(&self) -> Address {
        self.signer.address()
    }

    pub fn cooldown_ledger(&self) -> &CooldownLedger {
        &self.cooldown
    }

    /// Fund `address`, returning the hash of the transaction the node accepted.
    pub async fn drip(&self, address: &str) -> Result<FaucetResponse, FaucetError> {
        let recipient = parse_address(address)?;
        if recipient == self.signer.address() {
            return Err(FaucetError::SelfFunding);
        }

        // Claim the cooldown slot before doing any work, releasing it if the drip
        // fails so a node outage does not lock a caller out for a day.
        if let Err(remaining) = self.cooldown.try_claim(address) {
            return Err(FaucetError::Cooldown { remaining_secs: remaining.as_secs().max(1) });
        }

        match self.submit_transfer(recipient).await {
            Ok(tx_hash) => {
                info!(%recipient, amount = self.config.drip_amount, "faucet drip sent");
                Ok(FaucetResponse {
                    tx_hash,
                    address: recipient.to_string(),
                    amount: self.config.drip_amount.to_string(),
                })
            }
            Err(error) => {
                self.cooldown.release(address);
                Err(error)
            }
        }
    }

    async fn submit_transfer(&self, recipient: Address) -> Result<String, FaucetError> {
        let _guard = self.submit_lock.lock().await;
        let chain_id = self.chain_id().await?;
        let nonce = self.nonce().await?;

        let unsigned = Transaction {
            tx_type: TransactionType::Transfer,
            nonce,
            from: self.signer.address(),
            to: Some(recipient),
            value: self.config.drip_amount,
            gas_price: 1,
            gas_limit: TRANSFER_GAS_LIMIT,
            data: Vec::new(),
            chain_id,
            signature: TransactionSignature::EMPTY,
        };
        let signature = self.signer.sign(&unsigned);

        let result = self
            .rpc_call(
                "eth_sendTransaction",
                serde_json::json!([{
                    "from": unsigned.from.to_string(),
                    "to": recipient.to_string(),
                    "value": format!("0x{:x}", unsigned.value),
                    "nonce": format!("0x{nonce:x}"),
                    "gasPrice": format!("0x{:x}", unsigned.gas_price),
                    "gas": format!("0x{:x}", unsigned.gas_limit),
                    "chainId": format!("0x{chain_id:x}"),
                    "data": "0x",
                    "publicKey": format!("0x{}", hex::encode(self.signer.public_key())),
                    "signature": format!("0x{}", hex::encode(signature.to_ed25519())),
                }]),
            )
            .await?;

        result
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| FaucetError::Rpc("node did not return a transaction hash".into()))
    }

    /// Faucet balance and chain metadata, for the status endpoint.
    pub async fn status(&self) -> Result<FaucetStatus, FaucetError> {
        let chain_id = self.chain_id().await?;
        let balance = self
            .rpc_call(
                "eth_getBalance",
                serde_json::json!([self.signer.address().to_string(), "latest"]),
            )
            .await?;
        Ok(FaucetStatus {
            faucet_address: self.signer.address().to_string(),
            balance: balance.as_str().unwrap_or("0x0").to_string(),
            drip_amount: self.config.drip_amount.to_string(),
            cooldown_secs: self.config.cooldown_secs,
            chain_id: format!("0x{chain_id:x}"),
        })
    }

    async fn chain_id(&self) -> Result<u64, FaucetError> {
        let value = self.rpc_call("eth_chainId", serde_json::json!([])).await?;
        parse_quantity(&value).ok_or_else(|| FaucetError::Rpc("invalid chain ID".into()))
    }

    async fn nonce(&self) -> Result<u64, FaucetError> {
        let value = self
            .rpc_call(
                "eth_getTransactionCount",
                serde_json::json!([self.signer.address().to_string(), "latest"]),
            )
            .await?;
        parse_quantity(&value).ok_or_else(|| FaucetError::Rpc("invalid nonce".into()))
    }

    async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, FaucetError> {
        let response = self
            .client
            .post(&self.config.rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": 1,
            }))
            .send()
            .await
            .map_err(|error| FaucetError::Rpc(format!("{method} request failed: {error}")))?;
        if !response.status().is_success() {
            return Err(FaucetError::Rpc(format!("{method} returned HTTP {}", response.status())));
        }

        // Bound the response so a hostile or broken node cannot exhaust memory.
        let body = read_bounded(response, MAX_RPC_RESPONSE_BYTES).await?;
        let body: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
            FaucetError::Rpc(format!("{method} returned invalid JSON: {error}"))
        })?;
        if let Some(error) = body.get("error") {
            let message =
                error.get("message").and_then(serde_json::Value::as_str).unwrap_or("unknown error");
            return Err(FaucetError::Rpc(format!("{method} failed: {message}")));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| FaucetError::Rpc(format!("{method} response has no result")))
    }

    /// The faucet HTTP router.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/drip", post(handle_drip))
            .route("/status", get(handle_status))
            .route("/health", get(|| async { "ok" }))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
            // A public faucet is meant to be called from any origin.
            .layer(tower_http::cors::CorsLayer::permissive())
            .with_state(self.clone())
    }

    /// Serve until the process exits, sweeping the cooldown ledger periodically.
    pub async fn serve(self) -> Result<(), FaucetError> {
        let listener = tokio::net::TcpListener::bind(self.config.listen_addr)
            .await
            .map_err(|source| FaucetError::Bind { addr: self.config.listen_addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| FaucetError::Bind { addr: self.config.listen_addr, source })?;
        info!(
            %addr,
            faucet = %self.signer.address(),
            rpc = %self.config.rpc_url,
            "faucet listening"
        );

        let sweeper = {
            let cooldown = self.cooldown.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
                loop {
                    ticker.tick().await;
                    let removed = cooldown.sweep();
                    if removed > 0 {
                        info!(removed, "swept expired faucet cooldown entries");
                    }
                }
            })
        };

        let result = axum::serve(listener, self.router()).await.map_err(FaucetError::Serve);
        sweeper.abort();
        result
    }
}

/// Read a response body, refusing anything above `limit`.
async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, FaucetError> {
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(FaucetError::Rpc("response is too large".into()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| FaucetError::Rpc(format!("response read failed: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(FaucetError::Rpc("response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn handle_drip(
    State(state): State<FaucetState>,
    Json(request): Json<FaucetRequest>,
) -> Result<Json<FaucetResponse>, (StatusCode, Json<ApiError>)> {
    state.drip(request.address.trim()).await.map(Json).map_err(api_error)
}

async fn handle_status(
    State(state): State<FaucetState>,
) -> Result<Json<FaucetStatus>, (StatusCode, Json<ApiError>)> {
    state.status().await.map(Json).map_err(api_error)
}

/// Map a faucet error onto an HTTP status, without leaking internals.
fn api_error(error: FaucetError) -> (StatusCode, Json<ApiError>) {
    let status = match &error {
        FaucetError::InvalidAddress(_) | FaucetError::SelfFunding => StatusCode::BAD_REQUEST,
        FaucetError::Cooldown { .. } => StatusCode::TOO_MANY_REQUESTS,
        FaucetError::Rpc(_) => StatusCode::BAD_GATEWAY,
        _ => {
            warn!(%error, "faucet request failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let message = match &error {
        FaucetError::Wallet(_) | FaucetError::MissingPassword(_) | FaucetError::Config(_) => {
            // Never surface key or configuration detail to an anonymous caller.
            "faucet is misconfigured".to_string()
        }
        other => other.to_string(),
    };
    (status, Json(ApiError { error: message }))
}

fn parse_address(value: &str) -> Result<Address, FaucetError> {
    let trimmed = value.trim();
    if trimmed.len() != 42 || !trimmed.starts_with("0x") {
        return Err(FaucetError::InvalidAddress(
            "address must be 0x followed by 40 hex characters".into(),
        ));
    }
    Address::from_hex(trimmed)
        .map_err(|_| FaucetError::InvalidAddress("address is not valid hex".into()))
}

fn parse_quantity(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => u64::from_str_radix(text.strip_prefix("0x")?, 16).ok(),
        serde_json::Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FaucetError {
    #[error("invalid address: {0}")]
    InvalidAddress(String),

    #[error("the faucet cannot fund itself")]
    SelfFunding,

    #[error("rate limited: try again in {remaining_secs} seconds")]
    Cooldown { remaining_secs: u64 },

    #[error("node RPC error: {0}")]
    Rpc(String),

    #[error("invalid faucet configuration: {0}")]
    Config(String),

    #[error("{0} is not set")]
    MissingPassword(&'static str),

    #[error("cannot load the faucet wallet: {0}")]
    Wallet(#[from] subhost_wallet::WalletError),

    #[error("cannot bind the faucet to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("faucet stopped: {0}")]
    Serve(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "faucet password";

    fn wallet_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("faucet-wallet.json");
        Wallet::new(PASSWORD).unwrap().save(&path).unwrap();
        (dir, path)
    }

    fn state_with(config: FaucetConfig, path: &Path) -> FaucetState {
        FaucetState::new(config, FaucetSigner::load(path, PASSWORD).unwrap()).unwrap()
    }

    #[test]
    fn cooldown_claim_is_single_use_and_case_insensitive() {
        let ledger = CooldownLedger::new(Duration::from_secs(60));
        let address = format!("0x{}", "ab".repeat(20));
        assert!(ledger.try_claim(&address).is_ok());

        // The same address in a different case must not get a second slot.
        assert!(ledger.try_claim(&address.to_uppercase()).is_err());
        assert_eq!(ledger.len(), 1);

        // A different address is unaffected.
        assert!(ledger.try_claim(&format!("0x{}", "cd".repeat(20))).is_ok());
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn cooldown_reports_the_remaining_wait() {
        let ledger = CooldownLedger::new(Duration::from_secs(600));
        ledger.try_claim("0xabc").unwrap();
        let remaining = ledger.try_claim("0xabc").unwrap_err();
        assert!(remaining.as_secs() > 500 && remaining.as_secs() <= 600);
    }

    #[test]
    fn cooldown_release_allows_an_immediate_retry() {
        let ledger = CooldownLedger::new(Duration::from_secs(600));
        ledger.try_claim("0xABC").unwrap();
        // Release must be case-insensitive too.
        ledger.release("0xabc");
        assert!(ledger.is_empty());
        assert!(ledger.try_claim("0xabc").is_ok());
    }

    #[test]
    fn cooldown_sweep_keeps_live_entries() {
        let ledger = CooldownLedger::new(Duration::from_secs(600));
        ledger.try_claim("0xabc").unwrap();
        assert_eq!(ledger.sweep(), 0, "a fresh entry must survive");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn address_parsing_is_strict() {
        assert!(parse_address(&format!("0x{}", "11".repeat(20))).is_ok());
        assert!(parse_address(&format!("  0x{}  ", "11".repeat(20))).is_ok());
        for bad in ["", "0x", "11".repeat(20).as_str(), "0xzz", &format!("0x{}", "11".repeat(19))] {
            assert!(parse_address(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn quantity_parsing_handles_both_json_shapes() {
        assert_eq!(parse_quantity(&serde_json::json!("0x1f")), Some(31));
        assert_eq!(parse_quantity(&serde_json::json!(31)), Some(31));
        assert_eq!(parse_quantity(&serde_json::json!("1f")), None);
        assert_eq!(parse_quantity(&serde_json::json!(true)), None);
    }

    #[test]
    fn signer_derives_the_wallet_address_and_signs_verifiably() {
        let (_dir, path) = wallet_file();
        let wallet = Wallet::read(&path).unwrap();
        let signer = FaucetSigner::load(&path, PASSWORD).unwrap();
        assert_eq!(signer.address().to_string(), wallet.address);

        let tx = Transaction {
            tx_type: TransactionType::Transfer,
            nonce: 0,
            from: signer.address(),
            to: Some(Address::new([2; 20])),
            value: 1,
            gas_price: 1,
            gas_limit: TRANSFER_GAS_LIMIT,
            data: Vec::new(),
            chain_id: 1,
            signature: TransactionSignature::EMPTY,
        };
        let signature = signer.sign(&tx);
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&signer.public_key()).unwrap();
        assert!(ed25519_dalek::Verifier::verify(
            &verifying_key,
            &tx.signing_payload(),
            &ed25519_dalek::Signature::from_bytes(&signature.to_ed25519()),
        )
        .is_ok());
    }

    #[test]
    fn signer_rejects_a_wrong_password_and_a_missing_env_password() {
        let (_dir, path) = wallet_file();
        assert!(matches!(
            FaucetSigner::load(&path, "not the password"),
            Err(FaucetError::Wallet(_))
        ));
        // The env var is not set in this test process.
        std::env::remove_var(PASSWORD_ENV);
        assert!(matches!(FaucetSigner::from_env(&path), Err(FaucetError::MissingPassword(_))));
    }

    #[test]
    fn invalid_configuration_is_refused() {
        let (_dir, path) = wallet_file();
        let signer = FaucetSigner::load(&path, PASSWORD).unwrap();
        for broken in [
            FaucetConfig { drip_amount: 0, ..Default::default() },
            FaucetConfig { cooldown_secs: 0, ..Default::default() },
            FaucetConfig { rpc_url: "ftp://node".into(), ..Default::default() },
        ] {
            assert!(matches!(
                FaucetState::new(broken, signer.clone()),
                Err(FaucetError::Config(_))
            ));
        }
        assert!(FaucetState::new(FaucetConfig::default(), signer).is_ok());
    }

    #[tokio::test]
    async fn drip_rejects_bad_addresses_before_touching_the_network() {
        let (_dir, path) = wallet_file();
        let state = state_with(FaucetConfig::default(), &path);

        // An unreachable RPC URL would error; these must fail earlier.
        assert!(matches!(state.drip("not-an-address").await, Err(FaucetError::InvalidAddress(_))));
        assert!(matches!(
            state.drip(&state.faucet_address().to_string()).await,
            Err(FaucetError::SelfFunding)
        ));
        assert!(
            state.cooldown_ledger().is_empty(),
            "a rejected request must not consume a cooldown slot"
        );
    }

    #[tokio::test]
    async fn a_failed_drip_releases_the_cooldown_slot() {
        let (_dir, path) = wallet_file();
        // Port 1 has nothing listening, so the RPC call fails.
        let state = state_with(
            FaucetConfig { rpc_url: "http://127.0.0.1:1".to_string(), ..Default::default() },
            &path,
        );

        let recipient = format!("0x{}", "22".repeat(20));
        assert!(matches!(state.drip(&recipient).await, Err(FaucetError::Rpc(_))));
        assert!(state.cooldown_ledger().is_empty(), "a node failure must not lock the caller out");
    }

    #[tokio::test]
    async fn cooldown_is_enforced_across_requests() {
        let (_dir, path) = wallet_file();
        let state = state_with(FaucetConfig::default(), &path);
        let recipient = format!("0x{}", "33".repeat(20));

        // Claim the slot directly, then confirm a drip is refused without any
        // network call.
        state.cooldown_ledger().try_claim(&recipient).unwrap();
        assert!(matches!(state.drip(&recipient).await, Err(FaucetError::Cooldown { .. })));
    }

    #[test]
    fn error_responses_map_to_sensible_statuses_and_hide_internals() {
        let (status, body) = api_error(FaucetError::InvalidAddress("bad".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0.error.contains("bad"));

        let (status, _) = api_error(FaucetError::SelfFunding);
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = api_error(FaucetError::Cooldown { remaining_secs: 42 });
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.0.error.contains("42"));

        let (status, _) = api_error(FaucetError::Rpc("node down".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        // Wallet and config failures must not describe the faucet's internals.
        let (status, body) = api_error(FaucetError::MissingPassword(PASSWORD_ENV));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0.error, "faucet is misconfigured");
        let (_, body) = api_error(FaucetError::Config("secret detail".into()));
        assert!(!body.0.error.contains("secret detail"));
    }

    #[tokio::test]
    async fn health_endpoint_answers_without_a_node() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_dir, path) = wallet_file();
        let state = state_with(FaucetConfig::default(), &path);
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, state.router()).await.unwrap();
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

        server.abort();
    }
}
