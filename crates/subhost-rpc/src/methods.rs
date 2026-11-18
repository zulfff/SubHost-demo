//! JSON-RPC surface: parameter parsing, response shaping, and method routing.

use crate::state::{RpcError, RpcState};
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey};
use jsonrpsee::server::{BatchRequestConfig, RpcModule, Server, ServerHandle};
use std::net::SocketAddr;
use subhost_core::{
    Address, Amount, Block, ChainId, Gas, Hash, Receipt, Transaction, TransactionSignature,
    TransactionType,
};
use tracing::{info, warn};

/// Largest JSON-RPC request body accepted.
const MAX_REQUEST_BODY_BYTES: u32 = 256 * 1024;
/// Largest JSON-RPC response body produced.
const MAX_RESPONSE_BODY_BYTES: u32 = 1024 * 1024;
/// Largest batch accepted, so one request cannot fan out without bound.
const MAX_BATCH_REQUESTS: u32 = 10;
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
    let Some(earliest) = base.checked_add(1) else {
        return Ok(None);
    };
    if head < earliest {
        return Ok(None);
    }
    match tag {
        "latest" | "pending" | "safe" | "finalized" => Ok(Some(head)),
        "earliest" => Ok(Some(earliest)),
        value => {
            let height =
                u64::from_str_radix(value.strip_prefix("0x").ok_or(RpcError::InvalidParams)?, 16)
                    .map_err(|_| RpcError::InvalidParams)?;
            Ok((height >= earliest && height <= head).then_some(height))
        }
    }
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
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self { listen_addr: SocketAddr::from(([127, 0, 0, 1], 8545)), max_connections: 1_000 }
    }
}

/// The JSON-RPC server.
///
/// The server has no authentication or TLS of its own: expose it on loopback, or
/// behind a reverse proxy that terminates both.
#[derive(Clone)]
pub struct RpcServer {
    state: RpcState,
}

impl RpcServer {
    pub fn new(state: RpcState) -> Self {
        Self { state }
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
        let server = Server::builder()
            .max_connections(config.max_connections.max(1))
            .max_request_body_size(MAX_REQUEST_BODY_BYTES)
            .max_response_body_size(MAX_RESPONSE_BODY_BYTES)
            .set_batch_request_config(BatchRequestConfig::Limit(MAX_BATCH_REQUESTS))
            .build(config.listen_addr)
            .await
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;

        let addr = server
            .local_addr()
            .map_err(|source| RpcServeError::Bind { addr: config.listen_addr, source })?;
        let handle = server.start(self.build_module()?);

        if !addr.ip().is_loopback() {
            warn!(
                %addr,
                "JSON-RPC is bound to a non-loopback address without authentication; \
                 restrict access at the network layer"
            );
        }
        info!(%addr, chain_id = self.state.chain_id(), "JSON-RPC server started");
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
            Ok::<_, RpcError>(state.submit_transaction(tx)?.to_string())
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
            let mut sequence = params.sequence();
            let tag: String = sequence.next().map_err(|_| RpcError::InvalidParams)?;
            let full_transactions: bool =
                sequence.optional_next().unwrap_or(Some(false)).unwrap_or(false);
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

    #[error("cannot register JSON-RPC method: {0}")]
    Registration(#[from] jsonrpsee::core::RegisterMethodError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{funded_state, signed_transfer, signing_key};
    use ed25519_dalek::Signer;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn rpc_request(addr: SocketAddr, body: serde_json::Value) -> serde_json::Value {
        let payload = body.to_string();
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
            })
            .await
            .unwrap()
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
        let mut state = funded_state(&key, 1_000_000);
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
        state.base_height = 2;

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
}
