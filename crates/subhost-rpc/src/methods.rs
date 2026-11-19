//! JSON-RPC surface: parameter parsing, response shaping, and method routing.

use crate::observability::RpcMetricsLayer;
use crate::security::{
    check_bind_policy, BindPolicyError, RpcGuardLayer, RpcSecurityConfig, SecurityError,
};
use crate::state::{RpcError, RpcState};
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};
use jsonrpsee::server::middleware::rpc::RpcServiceBuilder;
use jsonrpsee::server::{BatchRequestConfig, RpcModule, Server, ServerHandle};
use std::net::SocketAddr;
use subhost_core::{
    Address, Amount, Block, ChainId, Gas, Hash, Receipt, Transaction, TransactionSignature,
    TransactionType,
};
use subhost_metrics::Metrics;
use tracing::info;

/// Largest JSON-RPC request body accepted.
const MAX_REQUEST_BODY_BYTES: u32 = 256 * 1024;
/// Largest JSON-RPC response body produced.
const MAX_RESPONSE_BODY_BYTES: u32 = 1024 * 1024;
/// Largest batch accepted, so one request cannot fan out without bound.
const MAX_BATCH_REQUESTS: u32 = 10;
/// Bound fee-history response size and the work spent sorting rewards.
const MAX_FEE_HISTORY_BLOCKS: u64 = 1024;
const MAX_REWARD_PERCENTILES: usize = 100;
/// Default gas limit applied when a caller omits one.
pub(crate) const DEFAULT_TX_GAS_LIMIT: Gas = 21_000;

/// Verify that `signature` was produced by `public_key` over `tx`, and that the
/// key really controls `tx.from`.
fn verify_transaction_signature(
    tx: &Transaction,
    public_key: &[u8; 32],
    signature: &[u8; 64],
) -> Result<(), RpcError> {
    let verifying_key =
        VerifyingKey::from_bytes(public_key).map_err(|_| RpcError::InvalidSignature)?;
    // Bind the key to the claimed sender before spending anything.
    if Address::from_public_key(public_key) != tx.from {
        return Err(RpcError::InvalidSignature);
    }
    verifying_key
        .verify(&tx.signing_payload(), &Ed25519Signature::from_bytes(signature))
        .map_err(|_| RpcError::InvalidSignature)
}

fn parse_address(value: &str) -> Result<Address, RpcError> {
    if value.len() != 42 {
        return Err(RpcError::InvalidAddress);
    }
    Address::from_hex(value).map_err(|_| RpcError::InvalidAddress)
}

fn parse_tx_hash(value: &str) -> Result<Hash, RpcError> {
    if value.len() != 66 {
        return Err(RpcError::InvalidTxHash);
    }
    Hash::from_hex(value).map_err(|_| RpcError::InvalidTxHash)
}

/// Strict Ethereum QUANTITY parsing: `0x`-prefixed, non-empty, no leading zeros.
fn hex_digits(value: &serde_json::Value) -> Result<&str, RpcError> {
    let serde_json::Value::String(text) = value else {
        return Err(RpcError::InvalidParams);
    };
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RpcError::InvalidParams);
    }
    Ok(digits)
}

fn hex_quantity(value: Option<&serde_json::Value>, default: u64) -> Result<u64, RpcError> {
    let Some(value) = value else { return Ok(default) };
    u64::from_str_radix(hex_digits(value)?, 16).map_err(|_| RpcError::InvalidParams)
}

fn hex_amount(value: Option<&serde_json::Value>, default: Amount) -> Result<Amount, RpcError> {
    let Some(value) = value else { return Ok(default) };
    Amount::from_str_radix(hex_digits(value)?, 16).map_err(|_| RpcError::InvalidParams)
}

/// Parse a `0x`-prefixed fixed-width byte string (public keys, signatures).
fn fixed_hex<const N: usize>(value: Option<&serde_json::Value>) -> Result<[u8; N], RpcError> {
    let text = value.and_then(serde_json::Value::as_str).ok_or(RpcError::InvalidSignature)?;
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidSignature)?;
    let bytes = hex::decode(digits).map_err(|_| RpcError::InvalidSignature)?;
    bytes.as_slice().try_into().map_err(|_| RpcError::InvalidSignature)
}

/// Optional `0x`-prefixed calldata.
fn hex_data(value: Option<&serde_json::Value>) -> Result<Vec<u8>, RpcError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let text = value.as_str().ok_or(RpcError::InvalidParams)?;
    let digits = text.strip_prefix("0x").ok_or(RpcError::InvalidParams)?;
    hex::decode(digits).map_err(|_| RpcError::InvalidParams)
}

/// Resolve a block tag (`latest`, `earliest`, `pending`, or a hex height) to a
/// concrete height. `Ok(None)` means "no such block", which maps to JSON `null`.
///
/// `base` is the last pruned height and `head` is the newest height. On a pruned
/// node `earliest` is the oldest block it can actually serve, not height 1.
fn resolve_block_height(tag: &str, base: u64, head: u64) -> Result<Option<u64>, RpcError> {
    // Validate even on an empty ledger: malformed quantities are errors, not
    // missing blocks. Reuse the strict QUANTITY parser used by transactions.
    let explicit_height = match tag {
        "latest" | "pending" | "safe" | "finalized" | "earliest" => None,
        value => Some(hex_quantity(Some(&serde_json::Value::String(value.into())), 0)?),
    };
    let Some(earliest) = base.checked_add(1) else {
        return Ok(None);
    };
    if head < earliest {
        return Ok(None);
    }
    match tag {
        "latest" | "pending" | "safe" | "finalized" => Ok(Some(head)),
        "earliest" => Ok(Some(earliest)),
        _ => {
            let height = explicit_height.ok_or(RpcError::InvalidParams)?;
            Ok((height >= earliest && height <= head).then_some(height))
        }
    }
}

/// Positional block lookup parameters, with the legacy optional hydration flag.
fn block_lookup_params(params: jsonrpsee::types::Params<'_>) -> Result<(String, bool), RpcError> {
    let values: Vec<serde_json::Value> = params.parse().map_err(|_| RpcError::InvalidParams)?;
    if !(1..=2).contains(&values.len()) {
        return Err(RpcError::InvalidParams);
    }
    let key = values[0].as_str().ok_or(RpcError::InvalidParams)?.to_owned();
    let full = match values.get(1) {
        Some(value) => value.as_bool().ok_or(RpcError::InvalidParams)?,
        None => false,
    };
    Ok((key, full))
}

fn no_params(params: jsonrpsee::types::Params<'_>) -> Result<(), RpcError> {
    if params.len_bytes() == 0 {
        return Ok(());
    }
    let values: Vec<serde_json::Value> = params.parse().map_err(|_| RpcError::InvalidParams)?;
    if values.is_empty() {
        Ok(())
    } else {
        Err(RpcError::InvalidParams)
    }
}

/// Legacy fees have no base-fee component; effective tips equal gas prices.
/// Gas consumption currently equals the transaction gas limit (see receipts).
fn block_rewards(block: &Block, percentiles: &[f64]) -> Vec<String> {
    let mut transactions = block.transactions.iter().collect::<Vec<_>>();
    transactions.sort_by_key(|tx| tx.gas_price);
    let Some(first) = transactions.first() else {
        return vec!["0x0".into(); percentiles.len()];
    };
    let mut index = 0;
    let mut consumed = first.gas_limit;
    percentiles
        .iter()
        .map(|percentile| {
            let threshold = (block.header.gas_used as f64 * percentile / 100.0) as Gas;
            while consumed < threshold && index + 1 < transactions.len() {
                index += 1;
                consumed += transactions[index].gas_limit;
            }
            format!("0x{:x}", transactions[index].gas_price)
        })
        .collect()
}

fn fee_history(
    params: jsonrpsee::types::Params<'_>,
    state: &RpcState,
) -> Result<serde_json::Value, RpcError> {
    let values: Vec<serde_json::Value> = params.parse().map_err(|_| RpcError::InvalidParams)?;
    if !(2..=3).contains(&values.len()) {
        return Err(RpcError::InvalidParams);
    }
    let count = hex_quantity(Some(&values[0]), 0)?;
    if !(1..=MAX_FEE_HISTORY_BLOCKS).contains(&count) {
        return Err(RpcError::InvalidParams);
    }
    let newest = values[1].as_str().ok_or(RpcError::InvalidParams)?;
    let percentiles = match values.get(2) {
        Some(value) => {
            let array = value.as_array().ok_or(RpcError::InvalidParams)?;
            if array.len() > MAX_REWARD_PERCENTILES {
                return Err(RpcError::InvalidParams);
            }
            array
                .iter()
                .map(|value| value.as_f64().ok_or(RpcError::InvalidParams))
                .collect::<Result<Vec<_>, _>>()?
        }
        None => Vec::new(),
    };
    if percentiles.iter().any(|p| !p.is_finite() || !(0.0..=100.0).contains(p))
        || percentiles.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(RpcError::InvalidParams);
    }

    // Hold a single block-index read guard so concurrent commits cannot produce
    // arrays from different tips. Only retained blocks are available after prune.
    let blocks = crate::state::read(&state.blocks);
    let head = blocks.last().map_or(state.base_height(), |block| block.header.height);
    let end = resolve_block_height(newest, state.base_height(), head)?;
    if blocks.is_empty() && newest.starts_with("0x") && hex_quantity(Some(&values[1]), 0)? != 0 {
        return Err(RpcError::InvalidParams);
    }
    let retained = end.map_or(&[][..], |end| {
        let start = end.saturating_sub(count - 1).max(state.base_height().saturating_add(1));
        let first = blocks.partition_point(|block| block.header.height < start);
        let last = blocks.partition_point(|block| block.header.height <= end);
        &blocks[first..last]
    });
    if end.is_none() && !blocks.is_empty() {
        return Err(RpcError::InvalidParams);
    }
    let mut result = serde_json::json!({
        "oldestBlock": format!("0x{:x}", retained.first().map_or(0, |b| b.header.height)),
        "baseFeePerGas": vec!["0x0"; if retained.is_empty() { 0 } else { retained.len() + 1 }],
        "gasUsedRatio": retained.iter().map(|block| {
            block.header.gas_used as f64 / block.header.gas_limit as f64
        }).collect::<Vec<_>>(),
    });
    if !percentiles.is_empty() {
        result["reward"] = serde_json::json!(retained
            .iter()
            .map(|block| block_rewards(block, &percentiles))
            .collect::<Vec<_>>());
    }
    Ok(result)
}

fn receipt_json(receipt: &Receipt) -> serde_json::Value {
    serde_json::json!({
        "transactionHash": receipt.tx_hash.to_string(),
        "blockHash": receipt.block_hash.to_string(),
        "blockNumber": format!("0x{:x}", receipt.block_height),
        "transactionIndex": format!("0x{:x}", receipt.transaction_index),
        "cumulativeGasUsed": format!("0x{:x}", receipt.cumulative_gas_used),
        "gasUsed": format!("0x{:x}", receipt.gas_used),
        "status": format!("0x{:x}", receipt.status as u8),
        "contractAddress": receipt.contract_address.map(|address| address.to_string()),
        "logs": receipt.logs.iter().map(|log| serde_json::json!({
            "address": log.address.to_string(),
            "topics": log.topics.iter().map(Hash::to_string).collect::<Vec<_>>(),
            "data": format!("0x{}", hex::encode(&log.data)),
        })).collect::<Vec<_>>(),
    })
}

fn transaction_json(tx: &Transaction, block: &Block, index: usize) -> serde_json::Value {
    serde_json::json!({
        "hash": tx.hash().to_string(),
        "blockHash": block.hash().to_string(),
        "blockNumber": format!("0x{:x}", block.header.height),
        "transactionIndex": format!("0x{index:x}"),
        "from": tx.from.to_string(),
        "to": tx.to.map(|address| address.to_string()),
        "value": format!("0x{:x}", tx.value),
        "nonce": format!("0x{:x}", tx.nonce),
        "gas": format!("0x{:x}", tx.gas_limit),
        "gasPrice": format!("0x{:x}", tx.gas_price),
        "chainId": format!("0x{:x}", tx.chain_id),
        "input": format!("0x{}", hex::encode(&tx.data)),
        "type": tx.tx_type.to_string(),
    })
}

fn block_json(block: &Block, full_transactions: bool) -> serde_json::Value {
    let transactions = if full_transactions {
        block
            .transactions
            .iter()
            .enumerate()
            .map(|(index, tx)| transaction_json(tx, block, index))
            .collect::<Vec<_>>()
    } else {
        block
            .transactions
            .iter()
            .map(|tx| serde_json::Value::String(tx.hash().to_string()))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "number": format!("0x{:x}", block.header.height),
        "hash": block.hash().to_string(),
        "parentHash": block.header.parent_hash.to_string(),
        "stateRoot": block.header.state_root.to_string(),
        "transactionsRoot": block.header.tx_root.to_string(),
        "receiptsRoot": block.header.receipt_root.to_string(),
        "miner": block.header.validator.to_string(),
        "timestamp": format!("0x{:x}", block.header.timestamp),
        "gasLimit": format!("0x{:x}", block.header.gas_limit),
        "gasUsed": format!("0x{:x}", block.header.gas_used),
        "extraData": format!("0x{}", hex::encode(&block.header.extra_data)),
        "transactions": transactions,
    })
}

/// Build the transaction described by an `eth_sendTransaction` parameter object.
fn transaction_from_params(
    params: &serde_json::Value,
    configured_chain_id: ChainId,
    public_key: [u8; subhost_core::PUBLIC_KEY_SIZE],
) -> Result<Transaction, RpcError> {
    let from = params
        .get("from")
        .and_then(serde_json::Value::as_str)
        .ok_or(RpcError::InvalidAddress)
        .and_then(parse_address)?;
    let to = match params.get("to").and_then(serde_json::Value::as_str) {
        Some(value) => Some(parse_address(value)?),
        None => None,
    };
    let chain_id = hex_quantity(params.get("chainId"), configured_chain_id)?;
    if chain_id != configured_chain_id {
        return Err(RpcError::InvalidChainId);
    }

    // Accept `gas` (spec) and `gasLimit` (widely used) for the same field.
    let gas_limit = match params.get("gas").or_else(|| params.get("gasLimit")) {
        Some(value) => hex_quantity(Some(value), DEFAULT_TX_GAS_LIMIT)?,
        None => DEFAULT_TX_GAS_LIMIT,
    };

    // Only transfers execute today; refuse anything else up front instead of
    // admitting it and failing during execution.
    if to.is_none() {
        return Err(RpcError::UnsupportedTransactionType(TransactionType::ContractCreation));
    }

    Ok(Transaction {
        tx_type: TransactionType::Transfer,
        nonce: hex_quantity(params.get("nonce"), 0)?,
        from,
        to,
        value: hex_amount(params.get("value"), 0)?,
        gas_price: hex_amount(params.get("gasPrice"), 1)?,
        gas_limit,
        data: hex_data(params.get("data").or_else(|| params.get("input")))?,
        chain_id,
        public_key,
        signature: TransactionSignature::EMPTY,
    })
}

/// Tunables for the JSON-RPC listener.
#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    pub listen_addr: SocketAddr,
    pub max_connections: u32,
    /// Authentication, rate limit and public-bind policy.
    pub security: RpcSecurityConfig,
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 8545)),
            max_connections: 1_000,
            security: RpcSecurityConfig::default(),
        }
    }
}

/// The JSON-RPC server.
///
/// There is no TLS in the server itself: terminate TLS in a reverse proxy (see
/// `docs/deployment.md`). Authentication and the rate limit are HTTP middleware
/// and are configured through [`RpcServerConfig::security`].
#[derive(Clone)]
pub struct RpcServer {
    state: RpcState,
    metrics: Option<Metrics>,
}

impl RpcServer {
    pub fn new(state: RpcState) -> Self {
        Self { state, metrics: None }
    }

    /// Record completed decoded calls in the supplied metrics registry.
    ///
    /// Pass the node's existing registry so the exporter observes these values.
    /// Labels use registered method names or `unknown`; malformed payloads,
    /// notifications and HTTP guard rejections bypass call instrumentation.
    /// Also enables committed mempool lifecycle events (S8.2).
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.state.set_metrics(metrics.clone());
        self.metrics = Some(metrics);
        self
    }

    pub fn state(&self) -> &RpcState {
        &self.state
    }

    /// Serve until the handle is stopped.
    pub async fn run(self, config: RpcServerConfig) -> Result<(), RpcServeError> {
        let (_, handle) = self.start(config).await?;
        handle.stopped().await;
        Ok(())
    }

    /// Bind and start serving, returning the bound address and stop handle.
    pub async fn start(
        self,
        config: RpcServerConfig,
    ) -> Result<(SocketAddr, ServerHandle), RpcServeError> {
        config.security.validate()?;
        check_bind_policy(config.listen_addr, &config.security)?;

        let module = self.build_module()?;
        let metrics_layer = self
            .metrics
            .clone()
            .map(|metrics| RpcMetricsLayer::new(metrics, module.method_names()));

        let server = Server::builder()
            .max_connections(config.max_connections.max(1))
            .max_request_body_size(MAX_REQUEST_BODY_BYTES)
            .max_response_body_size(MAX_RESPONSE_BODY_BYTES)
            .set_batch_request_config(BatchRequestConfig::Limit(MAX_BATCH_REQUESTS))
            .set_rpc_middleware(RpcServiceBuilder::new().option_layer(metrics_layer))
            .set_http_middleware(
                tower::ServiceBuilder::new().layer(RpcGuardLayer::new(&config.security)),
            )
            .build(config.listen_addr)
            .await
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;

        let addr = server
            .local_addr()
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;
        let handle = server.start(module);

        info!(
            %addr,
            chain_id = self.state.chain_id(),
            auth = config.security.auth_token.is_some(),
            rate_limit_per_sec = config.security.rate_limit_per_sec,
            "JSON-RPC server started"
        );
        Ok((addr, handle))
    }

    fn build_module(&self) -> Result<RpcModule<RpcState>, RpcServeError> {
        let mut module = RpcModule::new(self.state.clone());

        module.register_method("eth_chainId", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.chain_id()))
        })?;

        module.register_method("net_version", |_params, state, _extensions| {
            Ok::<_, RpcError>(state.chain_id().to_string())
        })?;

        module.register_method("eth_blockNumber", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.block_height()))
        })?;

        module.register_method("eth_gasPrice", |_params, state, _extensions| {
            Ok::<_, RpcError>(format!("0x{:x}", state.min_gas_price()))
        })?;

        module.register_method("eth_maxPriorityFeePerGas", |params, state, _extensions| {
            no_params(params)?;
            Ok::<_, RpcError>(format!("0x{:x}", state.min_gas_price()))
        })?;

        module.register_method("eth_syncing", |params, _state, _extensions| {
            no_params(params)?;
            // There is no block-sync worker yet. This says no sync is running;
            // it does not assert that this node knows the network's latest tip.
            Ok::<_, RpcError>(false)
        })?;

        module.register_method("eth_feeHistory", |params, state, _extensions| {
            fee_history(params, state)
        })?;

        for method in ["eth_getLogs", "eth_subscribe"] {
            module.register_method(method, |_params, _state, _extensions| {
                Err::<serde_json::Value, _>(jsonrpsee::types::ErrorObjectOwned::owned(
                    -32004,
                    "method unsupported: contract logs and subscriptions are not available",
                    None::<()>,
                ))
            })?;
        }

        module.register_method("eth_getBalance", |params, state, _extensions| {
            // Accept `[address]` as well as the spec's `[address, block]`.
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            Ok::<_, RpcError>(format!("0x{:x}", state.balance(&parse_address(&address)?)))
        })?;

        module.register_method("eth_getTransactionCount", |params, state, _extensions| {
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            Ok::<_, RpcError>(format!("0x{:x}", state.nonce(&parse_address(&address)?)))
        })?;

        // Subhost extension: a Merkle membership proof for an account at the
        // latest state root. `storageKeys` and a block tag are accepted for shape
        // compatibility, but this chain has no contract storage yet, so the
        // response contains no storage proof. Absence cannot be proven.
        module.register_method("eth_getProof", |params, state, _extensions| {
            let address: String = params.sequence().next().map_err(|_| RpcError::InvalidParams)?;
            let address = parse_address(&address)?;
            let proof = state.account_proof(&address).ok_or(RpcError::UnknownAccount)?;

            let mut account_proof = Vec::with_capacity(proof.proof.siblings.len() + 1);
            account_proof.push(proof.proof.leaf.to_string());
            account_proof.extend(proof.proof.siblings.iter().map(Hash::to_string));

            Ok::<_, RpcError>(serde_json::json!({
                "address": address.to_string(),
                "accountProof": account_proof,
                "balance": format!("0x{:x}", proof.account.balance),
                "nonce": format!("0x{:x}", proof.account.nonce),
                "stateRoot": proof.root.to_string(),
                "leafIndex": format!("0x{:x}", proof.proof.index),
                "storageProof": [],
            }))
        })?;

        module.register_method("eth_sendTransaction", |params, state, _extensions| {
            let request: serde_json::Value = params.one().map_err(|_| RpcError::InvalidParams)?;

            // Signature is mandatory: the node never signs on a caller's behalf.
            let public_key = fixed_hex::<32>(request.get("publicKey"))?;
            let signature = fixed_hex::<64>(request.get("signature"))?;
            let unsigned = transaction_from_params(&request, state.chain_id(), public_key)?;
            verify_transaction_signature(&unsigned, &public_key, &signature)?;

            let tx = Transaction {
                signature: TransactionSignature::from_ed25519(&signature),
                ..unsigned
            };
            let hash = state.submit_transaction(tx)?;
            // The candidate pool was committed inside submit_transaction; publish
            // its buffered lifecycle events. Rejected candidates dropped theirs.
            state.flush_mempool_events();
            Ok::<_, RpcError>(hash.to_string())
        })?;

        module.register_method("eth_getTransactionReceipt", |params, state, _extensions| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            Ok::<_, RpcError>(
                state
                    .receipt(&parse_tx_hash(&tx_hash)?)
                    .as_ref()
                    .map_or(serde_json::Value::Null, receipt_json),
            )
        })?;

        module.register_method("eth_getBlockByNumber", |params, state, _extensions| {
            let (tag, full_transactions) = block_lookup_params(params)?;
            let Some(height) =
                resolve_block_height(&tag, state.base_height(), state.head_height())?
            else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            Ok::<_, RpcError>(
                state
                    .block_at_height(height)
                    .map_or(serde_json::Value::Null, |block| block_json(&block, full_transactions)),
            )
        })?;

        module.register_method("eth_getBlockByHash", |params, state, _extensions| {
            let (hash, full_transactions) = block_lookup_params(params)?;
            let hash = parse_tx_hash(&hash)?;
            let blocks = crate::state::read(&state.blocks);
            Ok::<_, RpcError>(
                blocks
                    .iter()
                    .find(|block| block.hash() == hash)
                    .map_or(serde_json::Value::Null, |block| block_json(block, full_transactions)),
            )
        })?;

        module.register_method("eth_getTransactionByHash", |params, state, _extensions| {
            let tx_hash: String = params.one().map_err(|_| RpcError::InvalidTxHash)?;
            let tx_hash = parse_tx_hash(&tx_hash)?;
            let Some(receipt) = state.receipt(&tx_hash) else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            let Some(block) = state.block_at_height(receipt.block_height) else {
                return Ok::<_, RpcError>(serde_json::Value::Null);
            };
            let found = block
                .transactions
                .iter()
                .position(|tx| tx.hash() == tx_hash)
                .map(|index| transaction_json(&block.transactions[index], &block, index));
            Ok::<_, RpcError>(found.unwrap_or(serde_json::Value::Null))
        })?;

        Ok(module)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcServeError {
    #[error("cannot bind JSON-RPC server to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Security(#[from] SecurityError),

    #[error(transparent)]
    BindPolicy(#[from] BindPolicyError),

    #[error("cannot register JSON-RPC method: {0}")]
    Registration(#[from] jsonrpsee::core::RegisterMethodError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecretToken;
    use crate::test_support::{funded_state, signed_transfer, signing_key};
    use ed25519_dalek::Signer;
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn rpc_request(addr: SocketAddr, body: serde_json::Value) -> serde_json::Value {
        let payload = body.to_string();
        rpc_payload(addr, &payload).await
    }

    async fn rpc_payload(addr: SocketAddr, payload: &str) -> serde_json::Value {
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
    }

    async fn start_test_server(state: RpcState) -> (SocketAddr, ServerHandle) {
        RpcServer::new(state)
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
                security: RpcSecurityConfig::default(),
            })
            .await
            .unwrap()
    }

    async fn call_rpc(
        addr: SocketAddr,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
            }),
        )
        .await
    }

    #[tokio::test]
    async fn hash_lookup_matches_number_lookup_and_pruned_hashes_are_null() {
        let key = signing_key(11);
        let state = funded_state(&key, 1_000_000);
        state.submit_transaction(signed_transfer(&key, 0, 1)).unwrap();
        state.submit_transaction(signed_transfer(&key, 1, 1)).unwrap();
        let first_hash = state.block_at_height(1).unwrap().hash().to_string();
        let second_hash = state.block_at_height(2).unwrap().hash().to_string();
        state.blocks.write().unwrap().remove(0);
        state.base_height.store(1, Ordering::SeqCst);
        let (addr, handle) = start_test_server(state).await;
        for full in [false, true] {
            let by_number =
                call_rpc(addr, "eth_getBlockByNumber", serde_json::json!(["latest", full])).await;
            let by_hash =
                call_rpc(addr, "eth_getBlockByHash", serde_json::json!([second_hash, full])).await;
            assert_eq!(by_hash["result"], by_number["result"]);
            assert_eq!(by_hash["result"]["number"], "0x2");
        }
        for hash in [first_hash, Hash::ZERO.to_string()] {
            let response =
                call_rpc(addr, "eth_getBlockByHash", serde_json::json!([hash, false])).await;
            assert!(response.get("result").unwrap().is_null());
        }
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn fee_history_reports_retained_blocks_and_legacy_fees() {
        let key = signing_key(12);
        let state = funded_state(&key, 1_000_000);
        for nonce in 0..3 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        state.blocks.write().unwrap().remove(0);
        state.base_height.store(1, Ordering::SeqCst);
        let (addr, handle) = start_test_server(state).await;
        let response =
            call_rpc(addr, "eth_feeHistory", serde_json::json!(["0x400", "latest", [0, 50, 100]]))
                .await;
        let result = &response["result"];
        assert_eq!(result["oldestBlock"], "0x2");
        assert_eq!(result["baseFeePerGas"], serde_json::json!(["0x0", "0x0", "0x0"]));
        assert_eq!(
            result["reward"],
            serde_json::json!([["0x1", "0x1", "0x1"], ["0x1", "0x1", "0x1"]])
        );
        assert_eq!(result["gasUsedRatio"], serde_json::json!([0.0007, 0.0007]));
        let single = call_rpc(addr, "eth_feeHistory", serde_json::json!(["0x1", "0x2", []])).await;
        assert_eq!(single["result"]["oldestBlock"], "0x2");
        assert_eq!(single["result"]["baseFeePerGas"], serde_json::json!(["0x0", "0x0"]));
        assert!(single["result"].get("reward").is_none());
        assert_eq!(call_rpc(addr, "eth_syncing", serde_json::json!([])).await["result"], false);
        let price = call_rpc(addr, "eth_gasPrice", serde_json::json!([])).await;
        let tip = call_rpc(addr, "eth_maxPriorityFeePerGas", serde_json::json!([])).await;
        assert_eq!(tip["result"], price["result"]);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn fee_history_rewards_are_weighted_by_receipt_gas() {
        let keys = [signing_key(21), signing_key(22), signing_key(23)];
        let state = RpcState::in_memory(1).unwrap();
        state
            .seed_accounts(
                keys.iter().map(|key| {
                    (Address::from_public_key(&key.verifying_key().to_bytes()), 10_000_000)
                }),
            )
            .unwrap();
        for (key, price, gas) in
            [(&keys[0], 1, 21_000), (&keys[1], 2, 210_000), (&keys[2], 9, 21_000)]
        {
            let mut tx = signed_transfer(key, 0, 1);
            tx.gas_price = price;
            tx.gas_limit = gas;
            tx.signature =
                TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
            state.mempool.write().unwrap().add(tx).unwrap();
        }
        let last_tx = crate::state::read(&state.mempool).pending().last().unwrap().clone();
        state.include_transaction(&last_tx.hash()).unwrap();
        let (addr, handle) = start_test_server(state).await;
        let response = call_rpc(
            addr,
            "eth_feeHistory",
            serde_json::json!(["0x1", "latest", [0, 10, 50, 90, 100]]),
        )
        .await;
        assert_eq!(
            response["result"]["reward"],
            serde_json::json!([["0x1", "0x2", "0x2", "0x2", "0x9"]])
        );
        assert_eq!(response["result"]["gasUsedRatio"], serde_json::json!([0.0084]));
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn new_rpc_methods_reject_malformed_and_unbounded_parameters() {
        let state = funded_state(&signing_key(13), 1_000_000);
        state.submit_transaction(signed_transfer(&signing_key(13), 0, 1)).unwrap();
        let (addr, handle) = start_test_server(state).await;
        let hash = Hash::ZERO.to_string();
        let cases = [
            ("eth_getBlockByHash", serde_json::json!([])),
            ("eth_getBlockByHash", serde_json::json!(["0x1234", false])),
            ("eth_getBlockByHash", serde_json::json!([hash, "false"])),
            ("eth_getBlockByHash", serde_json::json!([hash, false, 1])),
            ("eth_getBlockByNumber", serde_json::json!(["latest", null])),
            ("eth_getBlockByNumber", serde_json::json!(["0x01", false])),
            ("eth_feeHistory", serde_json::json!(["0x0", "latest", []])),
            ("eth_feeHistory", serde_json::json!(["0x401", "latest", []])),
            ("eth_feeHistory", serde_json::json!(["0x01", "latest", []])),
            ("eth_feeHistory", serde_json::json!([1, "latest", []])),
            ("eth_feeHistory", serde_json::json!(["0x1", "0x2", []])),
            ("eth_feeHistory", serde_json::json!(["0x1", "0x0", []])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", [-1]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", [101]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", [50, 20]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", [50, 50]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", ["50"]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", vec![0; 101]])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", null])),
            ("eth_feeHistory", serde_json::json!(["0x1", "latest", [], 1])),
            ("eth_syncing", serde_json::json!([1])),
            ("eth_maxPriorityFeePerGas", serde_json::json!([1])),
        ];
        for (method, params) in cases {
            let response = call_rpc(addr, method, params.clone()).await;
            assert_eq!(response["error"]["code"], -32602, "{method}: {params}: {response}");
            assert!(response.get("result").is_none());
        }
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn json_rpc_error_codes_and_ids_conform_over_http() {
        let (addr, handle) = start_test_server(funded_state(&signing_key(14), 10)).await;
        for (payload, code) in [
            ("{", -32700),
            (r#"{"jsonrpc":"2.0","id":1}"#, -32600),
            (r#"{"jsonrpc":"2.0","id":1,"method":"missing","params":[]}"#, -32601),
            (r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":[]}"#, -32602),
        ] {
            let response = rpc_payload(addr, payload).await;
            assert_eq!(response["jsonrpc"], "2.0");
            assert_eq!(response["error"]["code"], code, "{response}");
            assert!(response.get("result").is_none());
        }
        for method in ["eth_getLogs", "eth_subscribe"] {
            let response = call_rpc(addr, method, serde_json::json!([])).await;
            assert_eq!(response["error"]["code"], -32004);
            assert_eq!(response["id"], 1);
        }
        let unknown =
            call_rpc(addr, "eth_getProof", serde_json::json!([Address::ZERO.to_string()])).await;
        assert_eq!(unknown["error"]["code"], -32000);
        let batch = rpc_request(
            addr,
            serde_json::json!([
                {"jsonrpc":"2.0","id":"sync","method":"eth_syncing","params":[]},
                {"jsonrpc":"2.0","method":"eth_syncing","params":[]},
                {"jsonrpc":"2.0","id":7,"method":"missing","params":[]}
            ]),
        )
        .await;
        let responses = batch.as_array().unwrap();
        assert_eq!(responses.len(), 2, "notifications have no response");
        assert!(responses.iter().any(|r| r["id"] == "sync" && r["result"] == false));
        assert!(responses.iter().any(|r| r["id"] == 7 && r["error"]["code"] == -32601));
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn empty_history_has_no_synthetic_genesis_block() {
        let (addr, handle) = start_test_server(RpcState::in_memory(1).unwrap()).await;
        let response =
            call_rpc(addr, "eth_feeHistory", serde_json::json!(["0x1", "latest", [50]])).await;
        assert_eq!(
            response["result"],
            serde_json::json!({
                "oldestBlock": "0x0", "baseFeePerGas": [], "gasUsedRatio": [], "reward": [],
            })
        );
        let invalid =
            call_rpc(addr, "eth_getBlockByNumber", serde_json::json!(["0x01", false])).await;
        assert_eq!(invalid["error"]["code"], -32602);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn persistence_error_is_internal_and_does_not_expose_local_paths() {
        let dir = tempfile::tempdir().unwrap();
        let key = signing_key(15);
        let state = RpcState::with_data_dir(1, dir.path()).unwrap();
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        state.seed_accounts([(sender, 100_000)]).unwrap();
        let manifest = dir.path().join("ledger/manifest.json");
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();
        let probe = state.clone();
        let (addr, handle) = start_test_server(state).await;
        let response =
            call_rpc(addr, "eth_sendTransaction", serde_json::json!([send_params(&key, 0)])).await;
        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(response["error"]["message"], "state persistence failed");
        assert!(!response.to_string().contains(dir.path().to_str().unwrap()));
        assert_eq!(probe.block_height(), 0);
        assert_eq!(probe.balance(&sender), 100_000);
        assert_eq!(probe.pending_count(), 0);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[test]
    fn quantity_parsing_is_strict() {
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x1f")), 0).unwrap(), 31);
        assert_eq!(hex_quantity(Some(&serde_json::json!("0x0")), 0).unwrap(), 0);
        assert_eq!(hex_quantity(None, 7).unwrap(), 7);
        assert!(hex_quantity(Some(&serde_json::json!("1f")), 0).is_err(), "missing 0x");
        assert!(hex_quantity(Some(&serde_json::json!("0x01")), 0).is_err(), "leading zero");
        assert!(hex_quantity(Some(&serde_json::json!("0x")), 0).is_err(), "empty");
        assert!(hex_quantity(Some(&serde_json::json!("0xzz")), 0).is_err(), "non-hex");
        assert!(hex_quantity(Some(&serde_json::json!(31)), 0).is_err(), "number");
        assert!(hex_amount(Some(&serde_json::json!("0x100000000000000000000")), 0).is_ok());
    }

    #[test]
    fn fixed_and_variable_hex_parsing_is_strict() {
        assert!(fixed_hex::<32>(Some(&serde_json::json!("00".repeat(32)))).is_err());
        assert!(
            fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(31))))).is_err()
        );
        assert!(fixed_hex::<32>(Some(&serde_json::json!(format!("0x{}", "00".repeat(32))))).is_ok());
        assert!(fixed_hex::<32>(None).is_err());

        assert_eq!(hex_data(None).unwrap(), Vec::<u8>::new());
        assert_eq!(hex_data(Some(&serde_json::json!("0x"))).unwrap(), Vec::<u8>::new());
        assert_eq!(hex_data(Some(&serde_json::json!("0xdead"))).unwrap(), vec![0xde, 0xad]);
        assert!(hex_data(Some(&serde_json::json!("dead"))).is_err());
        assert!(hex_data(Some(&serde_json::json!("0xodd"))).is_err());
    }

    #[test]
    fn address_and_hash_parsing_reject_wrong_widths() {
        assert!(parse_address("0x1234").is_err());
        assert!(parse_address(&format!("0x{}", "11".repeat(20))).is_ok());
        assert!(parse_tx_hash(&format!("0x{}", "11".repeat(20))).is_err());
        assert!(parse_tx_hash(&format!("0x{}", "11".repeat(32))).is_ok());
    }

    #[test]
    fn block_tag_resolution_covers_tags_heights_and_gaps() {
        assert_eq!(resolve_block_height("latest", 0, 0).unwrap(), None);
        assert_eq!(resolve_block_height("latest", 0, 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("pending", 0, 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("finalized", 0, 3).unwrap(), Some(3));
        assert_eq!(resolve_block_height("earliest", 0, 3).unwrap(), Some(1));
        assert_eq!(resolve_block_height("0x2", 0, 3).unwrap(), Some(2));
        assert_eq!(resolve_block_height("0x0", 0, 3).unwrap(), None, "genesis has no block");
        assert_eq!(resolve_block_height("0x9", 0, 3).unwrap(), None, "beyond the tip");
        assert_eq!(resolve_block_height("0x0", 4, 9).unwrap(), None, "below the pruned base");
        assert_eq!(resolve_block_height("0x4", 4, 9).unwrap(), None, "the base itself is pruned");
        assert_eq!(resolve_block_height("earliest", 4, 9).unwrap(), Some(5));
        assert!(resolve_block_height("2", 0, 3).is_err());
    }

    #[test]
    fn a_pruned_state_maps_heights_onto_the_retained_suffix() {
        let key = signing_key(7);
        let state = funded_state(&key, 1_000_000);
        for nonce in 0..3 {
            state.submit_transaction(signed_transfer(&key, nonce, 1)).unwrap();
        }
        assert_eq!(state.head_height(), 3);
        assert_eq!(state.head_height(), state.block_height());
        assert_eq!(state.block_at_height(1).unwrap().header.height, 1);

        // Simulate a load from a ledger whose first two blocks were pruned: the
        // blocks and the receipts of those heights are gone, the base is not.
        {
            let mut blocks = state.blocks.write().unwrap();
            blocks.remove(0);
            blocks.remove(0);
        }
        state.base_height.store(2, Ordering::SeqCst);

        assert_eq!(state.base_height(), 2);
        assert_eq!(state.block_count(), 1);
        assert_eq!(state.head_height(), 3);
        assert_eq!(state.head_height(), state.block_height());
        assert_eq!(state.block_at_height(1), None, "pruned");
        assert_eq!(state.block_at_height(2), None, "the base itself is pruned");
        assert_eq!(state.block_at_height(3).unwrap().header.height, 3);
        assert_eq!(state.block_at_height(4), None, "beyond the head");
    }

    #[test]
    fn signature_verification_binds_payload_and_sender() {
        let key = signing_key(7);
        let public_key = key.verifying_key().to_bytes();
        let tx = signed_transfer(&key, 0, 10);
        let signature = tx.signature.to_ed25519();
        assert!(verify_transaction_signature(&tx, &public_key, &signature).is_ok());

        // Any mutated field invalidates the signature.
        let mut tampered = tx.clone();
        tampered.value = 11;
        assert!(matches!(
            verify_transaction_signature(&tampered, &public_key, &signature),
            Err(RpcError::InvalidSignature)
        ));

        // A valid signature from another key cannot spend this account.
        let attacker = signing_key(9);
        let attacker_key = attacker.verifying_key().to_bytes();
        let attacker_signature = attacker.sign(&tx.signing_payload()).to_bytes();
        assert!(matches!(
            verify_transaction_signature(&tx, &attacker_key, &attacker_signature),
            Err(RpcError::InvalidSignature)
        ));
    }

    #[test]
    fn contract_creation_is_refused_before_it_reaches_the_pool() {
        let request = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "value": "0x1",
        });
        assert!(matches!(
            transaction_from_params(&request, 1, [0u8; 32]),
            Err(RpcError::UnsupportedTransactionType(TransactionType::ContractCreation))
        ));
    }

    #[test]
    fn transaction_params_reject_a_foreign_chain_id() {
        let request = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "to": Address::new([2; 20]).to_string(),
            "chainId": "0x9",
        });
        assert!(matches!(
            transaction_from_params(&request, 1, [0u8; 32]),
            Err(RpcError::InvalidChainId)
        ));
    }

    #[test]
    fn transaction_params_accept_gas_and_gas_limit_aliases() {
        let base = serde_json::json!({
            "from": Address::new([1; 20]).to_string(),
            "to": Address::new([2; 20]).to_string(),
        });
        assert_eq!(
            transaction_from_params(&base, 1, [0u8; 32]).unwrap().gas_limit,
            DEFAULT_TX_GAS_LIMIT
        );

        let mut with_gas = base.clone();
        with_gas["gas"] = serde_json::json!("0x7530");
        assert_eq!(transaction_from_params(&with_gas, 1, [0u8; 32]).unwrap().gas_limit, 30_000);

        let mut with_gas_limit = base;
        with_gas_limit["gasLimit"] = serde_json::json!("0x7530");
        assert_eq!(
            transaction_from_params(&with_gas_limit, 1, [0u8; 32]).unwrap().gas_limit,
            30_000
        );
    }

    #[tokio::test]
    async fn http_round_trip_covers_send_receipt_block_and_transaction() {
        let key = signing_key(7);
        let public_key = key.verifying_key().to_bytes();
        let sender = Address::from_public_key(&public_key);
        let state = funded_state(&key, 100_000);
        let (addr, handle) = start_test_server(state).await;

        let tx = signed_transfer(&key, 0, 10);
        let send = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                    "nonce": "0x0",
                    "gasPrice": "0x1",
                    "gas": "0x5208",
                    "chainId": "0x1",
                    "data": "0x",
                    "publicKey": format!("0x{}", hex::encode(public_key)),
                    "signature": format!("0x{}", hex::encode(tx.signature.to_ed25519())),
                }],
            }),
        )
        .await;
        let hash = send["result"].as_str().expect("a transaction hash").to_string();
        assert_eq!(hash, tx.hash().to_string());

        let receipt = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "eth_getTransactionReceipt", "params": [hash.clone()],
            }),
        )
        .await;
        assert_eq!(receipt["result"]["blockNumber"], "0x1");
        assert_eq!(receipt["result"]["status"], "0x1");

        let block = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3,
                "method": "eth_getBlockByNumber", "params": ["latest", true],
            }),
        )
        .await;
        assert_eq!(block["result"]["number"], "0x1");
        assert_eq!(block["result"]["transactions"][0]["hash"], hash.as_str());
        assert_eq!(block["result"]["transactions"][0]["from"], sender.to_string());

        let transaction = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 4,
                "method": "eth_getTransactionByHash", "params": [hash],
            }),
        )
        .await;
        assert_eq!(transaction["result"]["value"], "0xa");
        assert_eq!(transaction["result"]["type"], "transfer");

        let balance = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 5,
                "method": "eth_getBalance",
                "params": [Address::new([2; 20]).to_string(), "latest"],
            }),
        )
        .await;
        assert_eq!(balance["result"], "0xa");

        let nonce = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 6,
                "method": "eth_getTransactionCount",
                "params": [sender.to_string(), "latest"],
            }),
        )
        .await;
        assert_eq!(nonce["result"], "0x1");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[test]
    fn flush_without_a_registry_drains_and_publishes_no_history() {
        let key = signing_key(11);
        let mut state = funded_state(&key, 100_000);
        // No registry attached: committed submits must still drain the buffer,
        // so it cannot grow across the process lifetime.
        state.submit_transaction(signed_transfer(&key, 0, 10)).unwrap();
        state.flush_mempool_events();
        state.submit_transaction(signed_transfer(&key, 1, 10)).unwrap();
        state.flush_mempool_events();

        // A registry attached later counts only admissions that follow it;
        // the drained history above must not publish retroactively.
        let metrics = Metrics::new().unwrap();
        state.set_metrics(metrics.clone());
        state.submit_transaction(signed_transfer(&key, 2, 10)).unwrap();
        state.flush_mempool_events();
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
    }

    #[tokio::test]
    async fn mempool_events_count_only_committed_submits() {
        let key = signing_key(9);
        let state = funded_state(&key, 100_000);
        let metrics = Metrics::new().unwrap();
        let (addr, handle) = RpcServer::new(state)
            .with_metrics(metrics.clone())
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
                security: RpcSecurityConfig::default(),
            })
            .await
            .unwrap();

        async fn send(addr: SocketAddr, tx: &Transaction, id: u64) -> serde_json::Value {
            rpc_request(
                addr,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "eth_sendTransaction",
                    "params": [{
                        "from": tx.from.to_string(),
                        "to": tx.to.expect("transfer has a recipient").to_string(),
                        "value": format!("0x{:x}", tx.value),
                        "nonce": format!("0x{:x}", tx.nonce),
                        "gasPrice": format!("0x{:x}", tx.gas_price),
                        "gas": format!("0x{:x}", tx.gas_limit),
                        "chainId": format!("0x{:x}", tx.chain_id),
                        "data": "0x",
                        "publicKey": format!("0x{}", hex::encode(tx.public_key)),
                        "signature": format!("0x{}", hex::encode(tx.signature.to_ed25519())),
                    }],
                }),
            )
            .await
        }

        // A committed admission counts once.
        let good = signed_transfer(&key, 0, 10);
        let response = send(addr, &good, 1).await;
        assert_eq!(response["result"].as_str(), Some(good.hash().to_string().as_str()));
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));

        // Admitted by the pool but unexecutable against the state: the candidate
        // is dropped after seal, so no lifecycle event may reach the registry.
        let unexecutable = signed_transfer(&key, 1, 1_000_000);
        let response = send(addr, &unexecutable, 2).await;
        assert!(response["error"].is_object(), "over-balance transfer must be rejected");
        let rendered = String::from_utf8(metrics.encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"admitted\"} 1"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"replaced\"} 0"));
        assert!(rendered.contains("subhost_mempool_transactions_total{event=\"evicted\"} 0"));

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_get_proof_returns_a_verifiable_merkle_path() {
        let key = signing_key(7);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let state = funded_state(&key, 100_000);
        let (addr, handle) = start_test_server(state.clone()).await;

        let response = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "eth_getProof",
                "params": [sender.to_string(), [], "latest"],
            }),
        )
        .await;
        let result = &response["result"];

        assert_eq!(result["address"], sender.to_string());
        assert_eq!(result["balance"], "0x186a0");
        assert_eq!(result["nonce"], "0x0");
        assert_eq!(result["stateRoot"], state.state_root().to_string());

        // The first element is the leaf, the rest are the path from the bottom.
        let path = result["accountProof"].as_array().expect("an account proof");
        assert_eq!(path[0], state.account_proof(&sender).unwrap().proof.leaf.to_string());
        assert_eq!(path.len(), 1 + state.account_proof(&sender).unwrap().proof.siblings.len());

        let proof = state.account_proof(&sender).unwrap();
        assert!(proof.verify(&state.state_root()));

        // An account that has never been touched cannot be proven absent.
        let unknown = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "eth_getProof",
                "params": [Address::new([9; 20]).to_string(), [], "latest"],
            }),
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32000);

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_surface_reports_chain_metadata_and_missing_data_as_null() {
        let state = RpcState::in_memory(1337).unwrap();
        let (addr, handle) = start_test_server(state).await;

        let chain_id = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
        )
        .await;
        assert_eq!(chain_id["result"], "0x539");

        let net_version = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"net_version","params":[]}),
        )
        .await;
        assert_eq!(net_version["result"], "1337");

        let gas_price = rpc_request(
            addr,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"eth_gasPrice","params":[]}),
        )
        .await;
        assert_eq!(gas_price["result"], "0x1");

        let block = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"eth_getBlockByNumber","params":["latest", false],
            }),
        )
        .await;
        assert!(block["result"].is_null(), "an empty chain has no latest block");

        let receipt = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"eth_getTransactionReceipt",
                "params":[format!("0x{}", "11".repeat(32))],
            }),
        )
        .await;
        assert!(receipt["result"].is_null());

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn http_surface_rejects_unsigned_and_malformed_requests() {
        let key = signing_key(7);
        let state = funded_state(&key, 100_000);
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let (addr, handle) = start_test_server(state).await;

        // No signature at all.
        let unsigned = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                }],
            }),
        )
        .await;
        assert_eq!(unsigned["error"]["code"], -32602);

        // Well-formed signature from the wrong key.
        let attacker = signing_key(9);
        let victim_tx = signed_transfer(&key, 0, 10);
        let forged = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "eth_sendTransaction",
                "params": [{
                    "from": sender.to_string(),
                    "to": Address::new([2; 20]).to_string(),
                    "value": "0xa",
                    "nonce": "0x0",
                    "gasPrice": "0x1",
                    "gas": "0x5208",
                    "chainId": "0x1",
                    "publicKey": format!("0x{}", hex::encode(attacker.verifying_key().to_bytes())),
                    "signature": format!(
                        "0x{}",
                        hex::encode(attacker.sign(&victim_tx.signing_payload()).to_bytes())
                    ),
                }],
            }),
        )
        .await;
        assert_eq!(forged["error"]["code"], -32602);

        let bad_address = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":3,"method":"eth_getBalance","params":["0x1234","latest"],
            }),
        )
        .await;
        assert_eq!(bad_address["error"]["code"], -32602);

        let bad_hash = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":4,"method":"eth_getTransactionReceipt","params":["0xzz"],
            }),
        )
        .await;
        assert_eq!(bad_hash["error"]["code"], -32602);

        // Nothing above may have moved funds.
        let balance = rpc_request(
            addr,
            serde_json::json!({
                "jsonrpc":"2.0","id":5,"method":"eth_getBalance",
                "params":[Address::new([2; 20]).to_string(),"latest"],
            }),
        )
        .await;
        assert_eq!(balance["result"], "0x0");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    fn post_request(
        addr: SocketAddr,
        body: &serde_json::Value,
        authorization: Option<&str>,
        connection: &str,
    ) -> String {
        let payload = body.to_string();
        let auth = authorization
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{auth}\
             Content-Length: {}\r\nConnection: {connection}\r\n\r\n{payload}",
            payload.len()
        )
    }

    async fn raw_rpc(
        addr: SocketAddr,
        body: &serde_json::Value,
        authorization: Option<&str>,
    ) -> String {
        let request = post_request(addr, body, authorization, "close");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    /// Read one HTTP response with a `content-length` body, leaving the stream
    /// usable for the next request.
    async fn read_one_response(stream: &mut tokio::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the server closed before a complete response");
            buffer.extend_from_slice(&chunk[..read]);
            let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]);
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if buffer.len() >= head_end + 4 + length {
                return String::from_utf8(buffer).unwrap();
            }
        }
    }

    fn status_of(response: &str) -> u16 {
        response
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or_default()
    }

    fn body_of(response: &str) -> serde_json::Value {
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
    }

    fn send_params(key: &ed25519_dalek::SigningKey, nonce: u64) -> serde_json::Value {
        let public_key = key.verifying_key().to_bytes();
        let tx = signed_transfer(key, nonce, 10);
        serde_json::json!({
            "from": Address::from_public_key(&public_key).to_string(),
            "to": Address::new([2; 20]).to_string(),
            "value": "0xa",
            "nonce": format!("0x{nonce:x}"),
            "gasPrice": "0x1",
            "gas": "0x5208",
            "chainId": "0x1",
            "data": "0x",
            "publicKey": format!("0x{}", hex::encode(public_key)),
            "signature": format!("0x{}", hex::encode(tx.signature.to_ed25519())),
        })
    }

    #[tokio::test]
    async fn a_configured_token_is_required_on_every_request() {
        let token = SecretToken::new("0123456789abcdef").unwrap();
        let (addr, handle) = RpcServer::new(funded_state(&signing_key(3), 1_000))
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
                security: RpcSecurityConfig { auth_token: Some(token), ..Default::default() },
            })
            .await
            .unwrap();
        let chain_id = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": [],
        });

        let missing = raw_rpc(addr, &chain_id, None).await;
        assert_eq!(status_of(&missing), 401);
        assert!(missing.to_ascii_lowercase().contains("www-authenticate: bearer"));
        assert_eq!(body_of(&missing)["error"]["code"], -32000);

        let wrong = raw_rpc(addr, &chain_id, Some("fedcba9876543210")).await;
        assert_eq!(status_of(&wrong), 401);
        assert_eq!(body_of(&wrong)["error"]["code"], -32000);

        let accepted = raw_rpc(addr, &chain_id, Some("0123456789abcdef")).await;
        assert_eq!(status_of(&accepted), 200);
        assert_eq!(body_of(&accepted)["result"], "0x1");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn an_unauthenticated_caller_cannot_send_a_transaction() {
        let key = signing_key(5);
        let state = funded_state(&key, 100_000);
        let probe = state.clone();
        let (addr, handle) = RpcServer::new(state)
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
                security: RpcSecurityConfig {
                    auth_token: Some(SecretToken::new("0123456789abcdef").unwrap()),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let send = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_sendTransaction",
            "params": [send_params(&key, 0)],
        });

        let refused = raw_rpc(addr, &send, None).await;
        assert_eq!(status_of(&refused), 401);
        assert_eq!(probe.pending_count(), 0, "nothing may reach the pool");
        assert_eq!(probe.block_height(), 0, "nothing may be sealed");

        let accepted = raw_rpc(addr, &send, Some("0123456789abcdef")).await;
        assert_eq!(status_of(&accepted), 200);
        assert!(body_of(&accepted)["result"].is_string());
        assert_eq!(probe.block_height(), 1, "the authenticated call is accepted");

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn the_rate_limit_is_per_connection_and_returns_retry_after() {
        let (addr, handle) = RpcServer::new(funded_state(&signing_key(9), 1_000))
            .start(RpcServerConfig {
                listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                max_connections: 16,
                security: RpcSecurityConfig { rate_limit_per_sec: Some(1), ..Default::default() },
            })
            .await
            .unwrap();
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": [],
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(post_request(addr, &request, None, "keep-alive").as_bytes())
            .await
            .unwrap();
        let first = read_one_response(&mut stream).await;
        assert_eq!(status_of(&first), 200);

        stream.write_all(post_request(addr, &request, None, "close").as_bytes()).await.unwrap();
        let second = read_one_response(&mut stream).await;
        assert_eq!(status_of(&second), 429);
        assert!(second.to_ascii_lowercase().contains("retry-after: 1"));
        assert_eq!(body_of(&second)["error"]["code"], -32005);

        // A fresh connection has its own bucket; it is not a global lockout.
        let other = raw_rpc(addr, &request, None).await;
        assert_eq!(status_of(&other), 200);

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn an_exposed_bind_needs_opt_in_and_a_token() {
        let public =
            RpcServerConfig { listen_addr: "0.0.0.0:0".parse().unwrap(), ..Default::default() };
        let error =
            RpcServer::new(funded_state(&signing_key(1), 1)).start(public).await.unwrap_err();
        assert!(matches!(error, RpcServeError::BindPolicy(BindPolicyError::OptInRequired { .. })));

        let opt_in_only = RpcServerConfig {
            listen_addr: "0.0.0.0:0".parse().unwrap(),
            security: RpcSecurityConfig { allow_public: true, ..Default::default() },
            ..Default::default()
        };
        let error =
            RpcServer::new(funded_state(&signing_key(1), 1)).start(opt_in_only).await.unwrap_err();
        assert!(matches!(error, RpcServeError::BindPolicy(BindPolicyError::AuthRequired { .. })));

        let zero_rate = RpcServerConfig {
            security: RpcSecurityConfig { rate_limit_per_sec: Some(0), ..Default::default() },
            ..Default::default()
        };
        let error =
            RpcServer::new(funded_state(&signing_key(1), 1)).start(zero_rate).await.unwrap_err();
        assert!(matches!(error, RpcServeError::Security(SecurityError::ZeroRateLimit)));
    }
}
