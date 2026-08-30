//! Subhost command line interface.
//!
//! Every command either performs a real operation or does not exist. Commands
//! whose backend is unimplemented were removed rather than left printing a
//! placeholder line.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use subhost_core::{
    Address, Amount, ChainId, GenesisConfig, Transaction, TransactionSignature, TransactionType,
    ValidatorInfo,
};
use subhost_node::{Node, NodeConfig, GENESIS_FILE_NAME};
use subhost_telemetry::{TelemetryConfig, Verbosity};
use subhost_wallet::Wallet;

/// Largest wallet file the CLI will parse while scanning a directory.
const MAX_WALLET_SCAN_BYTES: u64 = 1024 * 1024;
/// Largest RPC response the CLI will buffer.
const MAX_RPC_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Gas limit used for a CLI transfer.
const TRANSFER_GAS_LIMIT: u64 = 21_000;
const DEFAULT_RPC_URL: &str = "http://127.0.0.1:8545";

#[derive(Parser)]
#[command(name = "subhost", about = "Subhost Web3 CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Increase log verbosity.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Only log warnings and errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Write a genesis file into a data directory.
    Init {
        #[arg(long, default_value_t = 1)]
        chain_id: ChainId,

        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Initial balance, repeatable: `--alloc ADDRESS=BALANCE`.
        #[arg(long = "alloc", value_name = "ADDRESS=BALANCE")]
        allocations: Vec<String>,

        /// Initial validator, repeatable: `--validator ADDRESS=PUBKEY_HEX:POWER`.
        #[arg(long = "validator", value_name = "ADDRESS=PUBKEY_HEX:POWER")]
        validators: Vec<String>,

        /// Overwrite an existing genesis file.
        #[arg(long)]
        force: bool,
    },

    /// Run a node: restore the ledger and serve JSON-RPC.
    Node {
        /// Require a genesis validator set and refuse to start without one.
        #[arg(long)]
        validator: bool,

        #[arg(short, long, default_value = DEFAULT_RPC_URL.trim_start_matches("http://"))]
        listen: String,

        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Also serve Prometheus metrics on this address.
        #[arg(long)]
        metrics_addr: Option<SocketAddr>,

        #[arg(long, default_value_t = 1000)]
        max_connections: u32,
    },

    /// Manage local wallets.
    Wallet {
        #[command(subcommand)]
        cmd: WalletCommands,
    },

    /// Build, sign, and submit transactions.
    Tx {
        #[command(subcommand)]
        cmd: TxCommands,
    },

    /// Read chain state over JSON-RPC.
    Query {
        #[command(subcommand)]
        cmd: QueryCommands,
    },
}

#[derive(Subcommand)]
enum WalletCommands {
    /// Create a new wallet.
    New {
        #[arg(short, long)]
        password: String,

        #[arg(short, long)]
        name: Option<String>,
    },

    /// List wallets in the wallet directory.
    List,

    /// Show a wallet's stored metadata.
    Show { address: String },

    /// Print a wallet's private key. Handle the output carefully.
    Export {
        address: String,

        #[arg(short, long)]
        password: String,
    },

    /// Import a hex-encoded 32-byte private key.
    Import {
        #[arg(long)]
        private_key: String,

        #[arg(short, long)]
        password: String,

        #[arg(short, long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum TxCommands {
    /// Sign and submit a transfer.
    Send {
        #[arg(short, long)]
        from: String,

        #[arg(short, long)]
        to: String,

        #[arg(short, long)]
        amount: Amount,

        #[arg(short, long)]
        password: String,

        #[arg(short, long, default_value_t = 1)]
        gas_price: u128,

        /// Sender nonce. Queried from the node when omitted.
        #[arg(long)]
        nonce: Option<u64>,

        /// Chain ID. Queried from the node when omitted.
        #[arg(long)]
        chain_id: Option<ChainId>,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },

    /// Fetch a transaction receipt.
    Status {
        tx_hash: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },
}

#[derive(Subcommand)]
enum QueryCommands {
    /// Account balance.
    Balance {
        address: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },

    /// Next expected nonce for an account.
    Nonce {
        address: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },

    /// A block by height, or the latest block.
    Block {
        /// Block height. Defaults to the chain tip.
        #[arg(long)]
        height: Option<u64>,

        /// Include full transaction objects.
        #[arg(long)]
        full: bool,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },

    /// Chain ID and current height.
    Chain {
        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    subhost_telemetry::init_or_warn(TelemetryConfig::from_env(Verbosity::from_flags(
        cli.verbose,
        cli.quiet,
    )));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;

    runtime.block_on(async move {
        match cli.command {
            Commands::Init { chain_id, data_dir, allocations, validators, force } => {
                init_genesis(chain_id, data_dir, allocations, validators, force)
            }
            Commands::Node { validator, listen, data_dir, metrics_addr, max_connections } => {
                run_node(validator, listen, data_dir, metrics_addr, max_connections).await
            }
            Commands::Wallet { cmd } => handle_wallet(cmd),
            Commands::Tx { cmd } => handle_transaction(cmd).await,
            Commands::Query { cmd } => handle_query(cmd).await,
        }
    })
}

/// `~/.subhost` unless `SUBHOST_HOME` overrides it.
fn subhost_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("SUBHOST_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var("HOME").context("neither SUBHOST_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".subhost"))
}

fn default_data_dir() -> Result<PathBuf> {
    Ok(subhost_home()?.join("data"))
}

fn wallet_dir() -> Result<PathBuf> {
    let dir = subhost_home()?.join("wallets");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create the wallet directory {}", dir.display()))?;
    Ok(dir)
}

fn init_genesis(
    chain_id: ChainId,
    data_dir: Option<PathBuf>,
    allocations: Vec<String>,
    validators: Vec<String>,
    force: bool,
) -> Result<()> {
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create {}", data_dir.display()))?;

    let genesis_path = data_dir.join(GENESIS_FILE_NAME);
    // Overwriting genesis on a live data directory would orphan the ledger.
    if genesis_path.exists() && !force {
        bail!("{} already exists; pass --force to overwrite it", genesis_path.display());
    }

    let mut genesis = GenesisConfig { chain_id, ..Default::default() };
    for allocation in &allocations {
        let (address, balance) = parse_allocation(allocation)?;
        if genesis.allocations.insert(address, balance).is_some() {
            bail!("duplicate genesis allocation for {address}");
        }
    }
    for validator in &validators {
        genesis.initial_validators.push(parse_validator(validator)?);
    }

    genesis
        .save(&genesis_path)
        .with_context(|| format!("cannot write {}", genesis_path.display()))?;

    println!("Genesis written to {}", genesis_path.display());
    println!("  chain ID:    {chain_id}");
    println!("  allocations: {}", genesis.allocations.len());
    println!("  validators:  {}", genesis.initial_validators.len());
    if genesis.initial_validators.is_empty() {
        // Say this plainly instead of implying the genesis is production ready.
        println!(
            "\nNote: no validators are configured. This genesis works for a single-node\n\
             chain (`subhost node`), but `subhost node --validator` will refuse to start."
        );
    }
    Ok(())
}

/// `ADDRESS=BALANCE`
fn parse_allocation(value: &str) -> Result<(Address, Amount)> {
    let (address, balance) = value
        .split_once('=')
        .with_context(|| format!("allocation {value:?} must use ADDRESS=BALANCE"))?;
    let address = Address::from_hex(address.trim())
        .with_context(|| format!("invalid allocation address in {value:?}"))?;
    let balance = balance
        .trim()
        .parse::<Amount>()
        .with_context(|| format!("invalid allocation balance in {value:?}"))?;
    Ok((address, balance))
}

/// `ADDRESS=PUBKEY_HEX:POWER`
fn parse_validator(value: &str) -> Result<ValidatorInfo> {
    let (address, rest) = value
        .split_once('=')
        .with_context(|| format!("validator {value:?} must use ADDRESS=PUBKEY_HEX:POWER"))?;
    let (public_key, power) = rest
        .rsplit_once(':')
        .with_context(|| format!("validator {value:?} must use ADDRESS=PUBKEY_HEX:POWER"))?;
    let address = Address::from_hex(address.trim())
        .with_context(|| format!("invalid validator address in {value:?}"))?;
    let public_key = hex::decode(public_key.trim().trim_start_matches("0x"))
        .with_context(|| format!("invalid validator public key in {value:?}"))?;
    if public_key.is_empty() {
        bail!("validator {value:?} has an empty public key");
    }
    let power = power
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid validator power in {value:?}"))?;
    Ok(ValidatorInfo { address, public_key, power })
}

async fn run_node(
    validator: bool,
    listen: String,
    data_dir: Option<PathBuf>,
    metrics_addr: Option<SocketAddr>,
    max_connections: u32,
) -> Result<()> {
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let rpc_addr: SocketAddr =
        listen.parse().with_context(|| format!("invalid listen address {listen:?}"))?;

    let node = Node::bootstrap(NodeConfig {
        data_dir,
        rpc_addr,
        max_rpc_connections: max_connections,
        metrics_addr,
        validator,
        fallback_chain_id: 1,
    })?;
    node.run().await?;
    Ok(())
}

fn handle_wallet(cmd: WalletCommands) -> Result<()> {
    let dir = wallet_dir()?;
    match cmd {
        WalletCommands::New { password, name } => {
            let wallet = Wallet::new(&password)?;
            let path = wallet_path(&dir, name.as_deref(), wallet.address())?;
            wallet.save(&path)?;
            println!("Created wallet {}", wallet.address());
            println!("  file: {}", path.display());
        }

        WalletCommands::Import { private_key, password, name } => {
            let wallet = Wallet::from_private_key(&private_key, &password)?;
            let path = wallet_path(&dir, name.as_deref(), wallet.address())?;
            wallet.save(&path)?;
            println!("Imported wallet {}", wallet.address());
            println!("  file: {}", path.display());
        }

        WalletCommands::List => {
            let wallets = list_wallets(&dir)?;
            if wallets.is_empty() {
                println!("No wallets in {}", dir.display());
            }
            for (path, wallet) in wallets {
                let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("wallet");
                println!("{name}\t{}", wallet.address());
            }
        }

        WalletCommands::Show { address } => {
            let address = parse_address(&address)?;
            let (path, wallet) = find_wallet(&dir, &address)?;
            println!("address: {}", wallet.address());
            println!("file:    {}", path.display());
            println!("version: {}", wallet.version);
            println!(
                "kdf:     scrypt(log_n={}, r={}, p={})",
                wallet.kdf.log_n, wallet.kdf.r, wallet.kdf.p
            );
        }

        WalletCommands::Export { address, password } => {
            let address = parse_address(&address)?;
            let (path, _) = find_wallet(&dir, &address)?;
            let (_, private_key) = Wallet::load(&path, &password)?;
            // Warn on stderr so a piped stdout stays machine-readable.
            eprintln!("WARNING: this prints a private key in plain text.");
            println!("0x{}", hex::encode(private_key.0));
        }
    }
    Ok(())
}

/// Choose a wallet file path, refusing to overwrite an existing wallet.
fn wallet_path(dir: &Path, name: Option<&str>, address: &str) -> Result<PathBuf> {
    let stem = match name {
        Some(name) => {
            // A name becomes a file name, so reject path separators outright.
            if name.is_empty()
                || name.contains(std::path::MAIN_SEPARATOR)
                || name.contains('/')
                || name.contains('\\')
                || name == "."
                || name == ".."
            {
                bail!("wallet name {name:?} is not a valid file name");
            }
            name.to_string()
        }
        None => address.trim_start_matches("0x").chars().take(8).collect(),
    };
    let path = dir.join(format!("{stem}.json"));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    Ok(path)
}

fn list_wallets(dir: &Path) -> Result<Vec<(PathBuf, Wallet)>> {
    let mut wallets = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("cannot read the wallet directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if std::fs::metadata(&path)?.len() > MAX_WALLET_SCAN_BYTES {
            continue;
        }
        // Skip unrelated JSON files rather than aborting the whole listing.
        if let Ok(wallet) = Wallet::read(&path) {
            wallets.push((path, wallet));
        }
    }
    wallets.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(wallets)
}

fn find_wallet(dir: &Path, address: &Address) -> Result<(PathBuf, Wallet)> {
    list_wallets(dir)?
        .into_iter()
        .find(|(_, wallet)| wallet.matches(address))
        .with_context(|| format!("no wallet for {address} in {}", dir.display()))
}

async fn handle_transaction(cmd: TxCommands) -> Result<()> {
    match cmd {
        TxCommands::Send { from, to, amount, password, gas_price, nonce, chain_id, rpc_url } => {
            if amount == 0 {
                bail!("amount must be greater than zero");
            }
            let from = parse_address(&from)?;
            let to = parse_address(&to)?;
            if from == to {
                bail!("sender and recipient must differ");
            }

            let (path, _) = find_wallet(&wallet_dir()?, &from)?;
            let (_, private_key) = Wallet::load(&path, &password)?;
            let signing_key = SigningKey::from_bytes(&private_key.0);
            let public_key = signing_key.verifying_key().to_bytes();

            // Fill in whatever the caller left out from the node itself, so a
            // stale hand-written nonce cannot silently produce a rejected tx.
            let chain_id = match chain_id {
                Some(chain_id) => chain_id,
                None => {
                    quantity(&rpc_call_value(&rpc_url, "eth_chainId", serde_json::json!([])).await?)
                        .context("node returned an invalid chain ID")?
                }
            };
            let nonce = match nonce {
                Some(nonce) => nonce,
                None => quantity(
                    &rpc_call_value(
                        &rpc_url,
                        "eth_getTransactionCount",
                        serde_json::json!([from.to_string(), "latest"]),
                    )
                    .await?,
                )
                .context("node returned an invalid nonce")?,
            };

            let unsigned = Transaction {
                tx_type: TransactionType::Transfer,
                nonce,
                from,
                to: Some(to),
                value: amount,
                gas_price,
                gas_limit: TRANSFER_GAS_LIMIT,
                data: Vec::new(),
                chain_id,
                signature: TransactionSignature::EMPTY,
            };
            let signature = signing_key.sign(&unsigned.signing_payload()).to_bytes();

            let hash: String = rpc_call(
                &rpc_url,
                "eth_sendTransaction",
                serde_json::json!([{
                    "from": from.to_string(),
                    "to": to.to_string(),
                    "value": format!("0x{amount:x}"),
                    "nonce": format!("0x{nonce:x}"),
                    "gasPrice": format!("0x{gas_price:x}"),
                    "gas": format!("0x{TRANSFER_GAS_LIMIT:x}"),
                    "chainId": format!("0x{chain_id:x}"),
                    "data": "0x",
                    "publicKey": format!("0x{}", hex::encode(public_key)),
                    "signature": format!("0x{}", hex::encode(signature)),
                }]),
            )
            .await?;
            println!("{hash}");
        }

        TxCommands::Status { tx_hash, rpc_url } => {
            if !is_tx_hash(&tx_hash) {
                bail!("transaction hash must be 0x followed by 64 hex characters");
            }
            let receipt =
                rpc_call_value(&rpc_url, "eth_getTransactionReceipt", serde_json::json!([tx_hash]))
                    .await?;
            if receipt.is_null() {
                println!("pending or unknown");
            } else {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            }
        }
    }
    Ok(())
}

async fn handle_query(cmd: QueryCommands) -> Result<()> {
    match cmd {
        QueryCommands::Balance { address, rpc_url } => {
            let address = parse_address(&address)?;
            let raw = rpc_call_value(
                &rpc_url,
                "eth_getBalance",
                serde_json::json!([address.to_string(), "latest"]),
            )
            .await?;
            let balance = amount(&raw).context("node returned an invalid balance")?;
            println!("{address}\t{balance}");
        }

        QueryCommands::Nonce { address, rpc_url } => {
            let address = parse_address(&address)?;
            let raw = rpc_call_value(
                &rpc_url,
                "eth_getTransactionCount",
                serde_json::json!([address.to_string(), "latest"]),
            )
            .await?;
            println!("{address}\t{}", quantity(&raw).context("node returned an invalid nonce")?);
        }

        QueryCommands::Block { height, full, rpc_url } => {
            let tag = match height {
                Some(height) => format!("0x{height:x}"),
                None => "latest".to_string(),
            };
            let block =
                rpc_call_value(&rpc_url, "eth_getBlockByNumber", serde_json::json!([tag, full]))
                    .await?;
            if block.is_null() {
                bail!("no such block");
            }
            println!("{}", serde_json::to_string_pretty(&block)?);
        }

        QueryCommands::Chain { rpc_url } => {
            let chain_id =
                quantity(&rpc_call_value(&rpc_url, "eth_chainId", serde_json::json!([])).await?)
                    .context("node returned an invalid chain ID")?;
            let height = quantity(
                &rpc_call_value(&rpc_url, "eth_blockNumber", serde_json::json!([])).await?,
            )
            .context("node returned an invalid height")?;
            println!("chain_id\t{chain_id}");
            println!("height\t{height}");
        }
    }
    Ok(())
}

fn parse_address(value: &str) -> Result<Address> {
    let trimmed = value.trim();
    if trimmed.len() != 42 || !trimmed.starts_with("0x") {
        bail!("address must be 0x followed by 40 hex characters");
    }
    Address::from_hex(trimmed).context("address is not valid hex")
}

fn is_tx_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn quantity(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => u64::from_str_radix(text.strip_prefix("0x")?, 16).ok(),
        serde_json::Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

fn amount(value: &serde_json::Value) -> Option<Amount> {
    match value {
        serde_json::Value::String(text) => {
            Amount::from_str_radix(text.strip_prefix("0x")?, 16).ok()
        }
        serde_json::Value::Number(number) => number.as_u128(),
        _ => None,
    }
}

async fn rpc_call<T: DeserializeOwned>(
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<T> {
    let value = rpc_call_value(rpc_url, method, params).await?;
    serde_json::from_value(value)
        .with_context(|| format!("RPC {method} returned an unexpected result shape"))
}

async fn rpc_call_value(
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // The CLI talks to one configured node; never chase a redirect.
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1,
        }))
        .send()
        .await
        .with_context(|| format!("cannot reach the node at {rpc_url}"))?
        .error_for_status()?;

    // Bound the body so a broken node cannot exhaust memory.
    if response.content_length().is_some_and(|length| length > MAX_RPC_RESPONSE_BYTES as u64) {
        bail!("RPC {method} response exceeds {MAX_RPC_RESPONSE_BYTES} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
            bail!("RPC {method} response exceeds {MAX_RPC_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }

    let body: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("RPC {method} returned invalid JSON"))?;
    if let Some(error) = body.get("error") {
        let message =
            error.get("message").and_then(serde_json::Value::as_str).unwrap_or("unknown error");
        bail!("RPC {method} failed: {message}");
    }
    body.get("result").cloned().with_context(|| format!("RPC {method} response has no result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_has_no_argument_collisions() {
        Cli::command().debug_assert();
    }

    #[test]
    fn documented_commands_parse() {
        let invocations: &[&[&str]] = &[
            &[
                "subhost",
                "init",
                "--chain-id",
                "1",
                "--data-dir",
                "./data",
                "--alloc",
                "0x1111111111111111111111111111111111111111=1000000",
            ],
            &["subhost", "node", "--listen", "127.0.0.1:8545", "--data-dir", "./data"],
            &["subhost", "node", "--validator", "--metrics-addr", "127.0.0.1:9090"],
            &["subhost", "wallet", "new", "--password", "password123"],
            &["subhost", "wallet", "list"],
            &["subhost", "wallet", "show", "0x1111111111111111111111111111111111111111"],
            &["subhost", "wallet", "import", "--private-key", "0x00", "--password", "password123"],
            &[
                "subhost",
                "tx",
                "send",
                "--from",
                "0x1111111111111111111111111111111111111111",
                "--to",
                "0x2222222222222222222222222222222222222222",
                "--amount",
                "1000",
                "--password",
                "password123",
            ],
            &["subhost", "tx", "status", &format!("0x{}", "11".repeat(32))],
            &["subhost", "query", "balance", "0x1111111111111111111111111111111111111111"],
            &["subhost", "query", "block", "--height", "12345", "--full"],
            &["subhost", "query", "chain"],
        ];
        for invocation in invocations {
            assert!(
                Cli::try_parse_from(invocation.iter().copied()).is_ok(),
                "{invocation:?} must parse"
            );
        }
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["subhost", "--verbose", "--quiet", "query", "chain"]).is_err());
    }

    #[test]
    fn allocation_parsing_accepts_valid_pairs_and_rejects_junk() {
        let (address, balance) =
            parse_allocation("0x1111111111111111111111111111111111111111=1000").unwrap();
        assert_eq!(address, Address::new([0x11; 20]));
        assert_eq!(balance, 1000);
        // Whitespace around either side is tolerated.
        assert!(parse_allocation(" 0x1111111111111111111111111111111111111111 = 5 ").is_ok());

        for bad in [
            "0x1111111111111111111111111111111111111111",
            "not-an-address=1",
            "0x1111111111111111111111111111111111111111=notanumber",
            "0x11=1",
            "=1",
        ] {
            assert!(parse_allocation(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn validator_parsing_accepts_valid_triples_and_rejects_junk() {
        let validator =
            parse_validator("0x2222222222222222222222222222222222222222=0xaabb:100").unwrap();
        assert_eq!(validator.address, Address::new([0x22; 20]));
        assert_eq!(validator.public_key, vec![0xaa, 0xbb]);
        assert_eq!(validator.power, 100);

        for bad in [
            "0x2222222222222222222222222222222222222222=0xaabb",
            "0x2222222222222222222222222222222222222222=:100",
            "0x2222222222222222222222222222222222222222=0xzz:100",
            "0x2222222222222222222222222222222222222222=0xaabb:notanumber",
            "nope=0xaabb:100",
        ] {
            assert!(parse_validator(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn address_parsing_is_strict() {
        assert!(parse_address("0x1111111111111111111111111111111111111111").is_ok());
        assert!(parse_address("  0x1111111111111111111111111111111111111111  ").is_ok());
        for bad in ["", "0x", "1111111111111111111111111111111111111111", "0xzz"] {
            assert!(parse_address(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn transaction_hash_validation_is_strict() {
        assert!(is_tx_hash(&format!("0x{}", "11".repeat(32))));
        assert!(!is_tx_hash(&"11".repeat(32)));
        assert!(!is_tx_hash(&format!("0x{}", "11".repeat(31))));
        assert!(!is_tx_hash(&format!("0x{}", "zz".repeat(32))));
    }

    #[test]
    fn quantity_and_amount_parsing_handle_both_json_shapes() {
        assert_eq!(quantity(&serde_json::json!("0x1f")), Some(31));
        assert_eq!(quantity(&serde_json::json!(31)), Some(31));
        assert_eq!(quantity(&serde_json::json!("1f")), None);
        assert_eq!(
            amount(&serde_json::json!("0xffffffffffffffffffffff")),
            Some(0xff_ffff_ffff_ffff_ffff_ffff)
        );
        assert_eq!(amount(&serde_json::json!(7)), Some(7));
        assert_eq!(amount(&serde_json::json!(null)), None);
    }

    #[test]
    fn wallet_path_rejects_traversal_and_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let address = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        // An omitted name derives a short stem from the address.
        let derived = wallet_path(dir.path(), None, address).unwrap();
        assert_eq!(derived.file_name().unwrap(), "abcdefab.json");

        assert_eq!(
            wallet_path(dir.path(), Some("alice"), address).unwrap().file_name().unwrap(),
            "alice.json"
        );

        for bad in ["", "..", ".", "../escape", "nested/name"] {
            assert!(
                wallet_path(dir.path(), Some(bad), address).is_err(),
                "{bad:?} must be rejected"
            );
        }

        // An existing file must not be silently overwritten.
        std::fs::write(dir.path().join("alice.json"), "{}").unwrap();
        assert!(wallet_path(dir.path(), Some("alice"), address).is_err());
    }

    #[test]
    fn wallet_listing_skips_unrelated_files_and_finds_by_address() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Wallet::new("password123").unwrap();
        wallet.save(&dir.path().join("alice.json")).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.path().join("other.json"), "{\"unrelated\":true}").unwrap();

        let wallets = list_wallets(dir.path()).unwrap();
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].1.address(), wallet.address());

        let address = wallet.parsed_address().unwrap();
        assert_eq!(find_wallet(dir.path(), &address).unwrap().1.address(), wallet.address());
        assert!(find_wallet(dir.path(), &Address::new([9; 20])).is_err());
    }

    #[test]
    fn init_writes_a_loadable_genesis_and_refuses_to_clobber_it() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        init_genesis(
            7,
            Some(data_dir.clone()),
            vec!["0x1111111111111111111111111111111111111111=1000".to_string()],
            vec!["0x2222222222222222222222222222222222222222=0xaabb:10".to_string()],
            false,
        )
        .unwrap();

        let genesis = GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap();
        assert_eq!(genesis.chain_id, 7);
        assert_eq!(genesis.allocations.get(&Address::new([0x11; 20])), Some(&1000));
        assert_eq!(genesis.initial_validators.len(), 1);
        assert!(genesis.requires_validators().is_ok());

        // A second run must not silently replace the file.
        assert!(init_genesis(7, Some(data_dir.clone()), Vec::new(), Vec::new(), false).is_err());
        assert!(init_genesis(9, Some(data_dir.clone()), Vec::new(), Vec::new(), true).is_ok());
        assert_eq!(GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap().chain_id, 9);
    }

    #[test]
    fn init_rejects_duplicate_allocations() {
        let dir = tempfile::tempdir().unwrap();
        assert!(init_genesis(
            1,
            Some(dir.path().to_path_buf()),
            vec![
                "0x1111111111111111111111111111111111111111=1".to_string(),
                "0x1111111111111111111111111111111111111111=2".to_string(),
            ],
            Vec::new(),
            false,
        )
        .is_err());
    }
}
