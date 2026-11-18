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
use subhost_crypto::{BLSScheme, PrivateKey};
use subhost_node::{
    read_producer_key, write_producer_key, Node, NodeConfig, P2pConfig, GENESIS_FILE_NAME,
    PRODUCER_KEY_FILE_NAME,
};
use subhost_storage::{LedgerStore, MigrationOutcome, PruneOutcome};
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

        /// Initial validator, repeatable. Forms:
        /// `ADDRESS:POWER` uses the producer key for the public key and proof,
        /// `ADDRESS=PUBKEY_HEX:POWER` requires the key to match,
        /// `ADDRESS=PUBKEY_HEX:POWER:POP_HEX` stores a proof signed elsewhere.
        #[arg(long = "validator", value_name = "ADDRESS[=PUBKEY_HEX]:POWER[:POP_HEX]")]
        validators: Vec<String>,

        /// Producer key file used for validator proofs.
        /// Defaults to `<data-dir>/producer.key`, generated when missing.
        #[arg(long)]
        producer_key_file: Option<PathBuf>,

        /// Overwrite an existing genesis file.
        #[arg(long)]
        force: bool,
    },

    /// Run a node: restore the ledger and serve JSON-RPC.
    Node {
        /// Require a genesis validator set and refuse to start without one.
        #[arg(long)]
        validator: bool,

        /// Producer key file used to sign blocks. Defaults to
        /// `<data-dir>/producer.key`.
        #[arg(long, requires = "validator")]
        producer_key_file: Option<PathBuf>,

        #[arg(short, long, default_value = DEFAULT_RPC_URL.trim_start_matches("http://"))]
        listen: String,

        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Also serve Prometheus metrics on this address.
        #[arg(long)]
        metrics_addr: Option<SocketAddr>,

        #[arg(long, default_value_t = 1000)]
        max_connections: u32,

        /// Multiaddr to listen on for peer gossip. Omit to run offline.
        #[arg(long)]
        p2p_listen: Option<String>,

        /// Bootstrap peer multiaddr, repeatable. Requires --p2p-listen.
        #[arg(long = "bootstrap-peer", value_name = "MULTIADDR", requires = "p2p_listen")]
        bootstrap_peers: Vec<String>,

        /// Disable local mDNS peer discovery.
        #[arg(long, requires = "p2p_listen")]
        no_mdns: bool,

        /// Maximum number of peer connections.
        #[arg(long, default_value_t = 50, requires = "p2p_listen")]
        max_peers: usize,

        /// File holding the bearer token every JSON-RPC caller must present.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,

        /// Requests per second allowed per JSON-RPC connection.
        #[arg(long)]
        rpc_rate_limit: Option<u32>,

        /// Permit a non-loopback bind. Requires --rpc-auth-token-file.
        #[arg(long, requires = "rpc_auth_token_file")]
        rpc_allow_public: bool,
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

    /// Inspect and maintain the on-disk ledger without starting a node.
    Db {
        #[command(subcommand)]
        cmd: DbCommands,
    },
}

#[derive(Subcommand)]
enum DbCommands {
    /// Migrate a version 2 `node-state.bin` to the segmented format.
    ///
    /// The legacy file is validated and kept as `node-state.bin.v2-backup`.
    /// Running this on an already-segmented ledger is a no-op.
    Migrate {
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        #[arg(long, default_value_t = 1)]
        chain_id: ChainId,
    },

    /// Drop the oldest ledger segments, keeping the newest `--keep-segments`.
    ///
    /// The newest snapshot stays the replay anchor, so the command refuses when
    /// the retained history would no longer contain the block at its height.
    /// Pruned blocks and receipts are gone: this node stops serving those heights.
    Prune {
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        #[arg(long, default_value_t = 1)]
        chain_id: ChainId,

        /// Number of newest segment files to keep.
        #[arg(long)]
        keep_segments: usize,
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

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
    },

    /// Fetch a transaction receipt.
    Status {
        tx_hash: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum QueryCommands {
    /// Account balance.
    Balance {
        address: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
    },

    /// Next expected nonce for an account.
    Nonce {
        address: String,

        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
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

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
    },

    /// Chain ID and current height.
    Chain {
        #[arg(long, default_value = DEFAULT_RPC_URL)]
        rpc_url: String,

        /// File holding the bearer token for an authenticated node.
        #[arg(long, env = "SUBHOST_RPC_AUTH_TOKEN_FILE")]
        rpc_auth_token_file: Option<PathBuf>,
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
            Commands::Init {
                chain_id,
                data_dir,
                allocations,
                validators,
                producer_key_file,
                force,
            } => {
                init_genesis(chain_id, data_dir, allocations, validators, producer_key_file, force)
            }
            Commands::Node {
                validator,
                producer_key_file,
                listen,
                data_dir,
                metrics_addr,
                max_connections,
                p2p_listen,
                bootstrap_peers,
                no_mdns,
                max_peers,
                rpc_auth_token_file,
                rpc_rate_limit,
                rpc_allow_public,
            } => {
                run_node(
                    validator,
                    producer_key_file,
                    listen,
                    data_dir,
                    metrics_addr,
                    max_connections,
                    p2p_listen,
                    bootstrap_peers,
                    no_mdns,
                    max_peers,
                    rpc_auth_token_file,
                    rpc_rate_limit,
                    rpc_allow_public,
                )
                .await
            }
            Commands::Wallet { cmd } => handle_wallet(cmd),
            Commands::Tx { cmd } => handle_transaction(cmd).await,
            Commands::Query { cmd } => handle_query(cmd).await,
            Commands::Db { cmd } => handle_db(cmd),
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
    producer_key_file: Option<PathBuf>,
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
    let specs =
        validators.iter().map(|value| parse_validator(value)).collect::<Result<Vec<_>>>()?;
    genesis.initial_validators = resolve_validators(specs, &data_dir, producer_key_file)?;

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

/// One `--validator` argument, before the producer key is resolved.
struct ValidatorSpec {
    address: Address,
    public_key: Option<Vec<u8>>,
    power: u64,
    proof_of_possession: Option<Vec<u8>>,
}

/// `ADDRESS[=PUBKEY_HEX]:POWER[:POP_HEX]`
fn parse_validator(value: &str) -> Result<ValidatorSpec> {
    let (address, public_key, power, proof_of_possession) = match value.split_once('=') {
        Some((address, rest)) => {
            let fields = rest.split(':').collect::<Vec<_>>();
            match fields.as_slice() {
                [public_key, power] => (address, Some(*public_key), *power, None),
                [public_key, power, proof] => (address, Some(*public_key), *power, Some(*proof)),
                _ => bail!("validator {value:?} must use ADDRESS[=PUBKEY_HEX]:POWER[:POP_HEX]"),
            }
        }
        None => {
            let (address, power) = value.rsplit_once(':').with_context(|| {
                format!("validator {value:?} must use ADDRESS[=PUBKEY_HEX]:POWER[:POP_HEX]")
            })?;
            (address, None, power, None)
        }
    };
    let address = Address::from_hex(address.trim())
        .with_context(|| format!("invalid validator address in {value:?}"))?;
    let public_key = match public_key.map(str::trim) {
        Some("") => bail!("validator {value:?} has an empty public key"),
        Some(text) => {
            let key = hex::decode(text.trim_start_matches("0x"))
                .with_context(|| format!("invalid validator public key in {value:?}"))?;
            if BLSScheme::decode_public_key(&key).is_none() {
                bail!("validator {value:?} public key is not a compressed BLS12-381 G1 point");
            }
            Some(key)
        }
        None => None,
    };
    let power = power
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid validator power in {value:?}"))?;
    if power == 0 {
        bail!("validator {value:?} has zero voting power");
    }
    let proof_of_possession = match proof_of_possession.map(str::trim) {
        Some(text) if !text.is_empty() => Some(
            hex::decode(text.trim_start_matches("0x"))
                .with_context(|| format!("invalid proof of possession in {value:?}"))?,
        ),
        Some(_) => bail!("validator {value:?} has an empty proof of possession"),
        None => None,
    };
    Ok(ValidatorSpec { address, public_key, power, proof_of_possession })
}

/// Turn the parsed entries into genesis validators, generating the proof of
/// possession from the producer key when one was not supplied.
fn resolve_validators(
    specs: Vec<ValidatorSpec>,
    data_dir: &Path,
    producer_key_file: Option<PathBuf>,
) -> Result<Vec<ValidatorInfo>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let key_path = producer_key_file.unwrap_or_else(|| data_dir.join(PRODUCER_KEY_FILE_NAME));
    let mut secret = None;
    if specs.iter().any(|spec| spec.proof_of_possession.is_none()) {
        secret = Some(load_or_create_producer_key(&key_path)?);
    }
    let derived =
        secret.as_ref().map(|secret| BLSScheme::encode_public_key(&BLSScheme::public_key(secret)));

    let mut validators = Vec::with_capacity(specs.len());
    for spec in specs {
        let public_key = match (&spec.public_key, &spec.proof_of_possession) {
            // A proof supplied from elsewhere is self-contained: this node's key
            // has nothing to do with it.
            (Some(public_key), Some(_)) => public_key.clone(),
            (None, Some(_)) => bail!(
                "validator {} needs a public key for a supplied proof of possession",
                spec.address
            ),
            (public_key, None) => match (public_key, &derived) {
                (Some(public_key), Some(derived)) if public_key != derived => bail!(
                    "validator {} public key does not match the producer key in {}",
                    spec.address,
                    key_path.display()
                ),
                (Some(public_key), _) => public_key.clone(),
                (None, Some(derived)) => derived.clone(),
                (None, None) => bail!(
                    "validator {} needs a proof of possession or a producer key",
                    spec.address
                ),
            },
        };
        let proof_of_possession = match spec.proof_of_possession {
            Some(proof) => {
                let Some(point) = BLSScheme::decode_public_key(&public_key) else {
                    bail!(
                        "validator {} public key is not a compressed BLS12-381 G1 point",
                        spec.address
                    );
                };
                let Some(signature) = BLSScheme::decode_signature(&proof) else {
                    bail!(
                        "validator {} proof of possession is not a compressed BLS12-381 G2 point",
                        spec.address
                    );
                };
                if !BLSScheme::verify_possession(&point, &signature) {
                    bail!(
                        "validator {} proof of possession does not verify against its public key",
                        spec.address
                    );
                }
                proof
            }
            None => {
                let Some(secret) = secret.as_ref() else {
                    bail!(
                        "validator {} needs a proof of possession or a producer key",
                        spec.address
                    );
                };
                BLSScheme::encode_signature(&BLSScheme::proof_of_possession(secret))
            }
        };
        validators.push(ValidatorInfo {
            address: spec.address,
            public_key,
            power: spec.power,
            proof_of_possession,
        });
    }
    Ok(validators)
}

/// Read the producer key, generating it when the file is absent.
///
/// Generation is only for the first `init` on a data directory: an existing file
/// is never replaced, because replacing it would orphan every block signed with
/// the old key.
fn load_or_create_producer_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        return read_producer_key(path)
            .with_context(|| format!("cannot use the producer key in {}", path.display()));
    }
    let (secret, public_key) = BLSScheme::keygen();
    write_producer_key(path, &secret)
        .with_context(|| format!("cannot write the producer key to {}", path.display()))?;
    println!("Generated producer key {}", path.display());
    println!("  public key: 0x{}", hex::encode(BLSScheme::encode_public_key(&public_key)));
    println!("  mode 0600. Keep it: it is this validator's identity.");
    Ok(secret)
}

#[allow(clippy::too_many_arguments)]
async fn run_node(
    validator: bool,
    producer_key_file: Option<PathBuf>,
    listen: String,
    data_dir: Option<PathBuf>,
    metrics_addr: Option<SocketAddr>,
    max_connections: u32,
    p2p_listen: Option<String>,
    bootstrap_peers: Vec<String>,
    no_mdns: bool,
    max_peers: usize,
    rpc_auth_token_file: Option<PathBuf>,
    rpc_rate_limit: Option<u32>,
    rpc_allow_public: bool,
) -> Result<()> {
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let rpc_addr: SocketAddr =
        listen.parse().with_context(|| format!("invalid listen address {listen:?}"))?;
    // clap already enforces that peer options require --p2p-listen.
    let network = p2p_listen.map(|listen_addr| P2pConfig {
        listen_addr,
        bootstrap_peers,
        max_peers,
        enable_mdns: !no_mdns,
    });

    let rpc_security = subhost_rpc::RpcSecurityConfig {
        auth_token: rpc_auth_token_file
            .as_deref()
            .map(subhost_rpc::SecretToken::from_file)
            .transpose()
            .with_context(|| "cannot load the RPC auth token")?,
        rate_limit_per_sec: rpc_rate_limit,
        allow_public: rpc_allow_public,
    };
    rpc_security.validate()?;

    // Start from node-owned defaults so additive configuration fields do not
    // break this consumer. Override only settings exposed by the current CLI.
    let mut config = NodeConfig::new(data_dir);
    config.rpc_addr = rpc_addr;
    config.max_rpc_connections = max_connections;
    config.rpc_security = rpc_security;
    config.metrics_addr = metrics_addr;
    config.validator = validator;
    config.producer_key_file = producer_key_file;
    config.fallback_chain_id = 1;
    config.network = network;

    let node = Node::bootstrap(config)?;
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
        TxCommands::Send {
            from,
            to,
            amount,
            password,
            gas_price,
            nonce,
            chain_id,
            rpc_url,
            rpc_auth_token_file,
        } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
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
                None => quantity(&rpc.call_value("eth_chainId", serde_json::json!([])).await?)
                    .context("node returned an invalid chain ID")?,
            };
            let nonce = match nonce {
                Some(nonce) => nonce,
                None => quantity(
                    &rpc.call_value(
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
                public_key,
                signature: TransactionSignature::EMPTY,
            };
            let signature = signing_key.sign(&unsigned.signing_payload()).to_bytes();

            let hash: String = rpc
                .call(
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

        TxCommands::Status { tx_hash, rpc_url, rpc_auth_token_file } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
            if !is_tx_hash(&tx_hash) {
                bail!("transaction hash must be 0x followed by 64 hex characters");
            }
            let receipt =
                rpc.call_value("eth_getTransactionReceipt", serde_json::json!([tx_hash])).await?;
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
        QueryCommands::Balance { address, rpc_url, rpc_auth_token_file } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
            let address = parse_address(&address)?;
            let raw = rpc
                .call_value("eth_getBalance", serde_json::json!([address.to_string(), "latest"]))
                .await?;
            let balance = amount(&raw).context("node returned an invalid balance")?;
            println!("{address}\t{balance}");
        }

        QueryCommands::Nonce { address, rpc_url, rpc_auth_token_file } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
            let address = parse_address(&address)?;
            let raw = rpc
                .call_value(
                    "eth_getTransactionCount",
                    serde_json::json!([address.to_string(), "latest"]),
                )
                .await?;
            println!("{address}\t{}", quantity(&raw).context("node returned an invalid nonce")?);
        }

        QueryCommands::Block { height, full, rpc_url, rpc_auth_token_file } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
            let tag = match height {
                Some(height) => format!("0x{height:x}"),
                None => "latest".to_string(),
            };
            let block =
                rpc.call_value("eth_getBlockByNumber", serde_json::json!([tag, full])).await?;
            if block.is_null() {
                bail!("no such block");
            }
            println!("{}", serde_json::to_string_pretty(&block)?);
        }

        QueryCommands::Chain { rpc_url, rpc_auth_token_file } => {
            let rpc = RpcClient::new(rpc_url, rpc_auth_token_file.as_deref())?;
            let chain_id = quantity(&rpc.call_value("eth_chainId", serde_json::json!([])).await?)
                .context("node returned an invalid chain ID")?;
            let height = quantity(&rpc.call_value("eth_blockNumber", serde_json::json!([])).await?)
                .context("node returned an invalid height")?;
            println!("chain_id\t{chain_id}");
            println!("height\t{height}");
        }
    }
    Ok(())
}

fn handle_db(cmd: DbCommands) -> Result<()> {
    match cmd {
        DbCommands::Migrate { data_dir, chain_id } => {
            let data_dir = match data_dir {
                Some(dir) => dir,
                None => default_data_dir()?,
            };
            let store = LedgerStore::open(chain_id, &data_dir)
                .with_context(|| format!("cannot open the ledger in {}", data_dir.display()))?;
            let outcome = store
                .migrate()
                .with_context(|| format!("cannot migrate the ledger in {}", data_dir.display()))?;
            match outcome {
                MigrationOutcome::AlreadySegmented => {
                    println!("{} is already segmented; nothing to do", data_dir.display());
                }
                MigrationOutcome::NothingToMigrate => {
                    println!(
                        "no version 2 {} in {}; nothing to do",
                        subhost_storage::LEDGER_FILE_NAME,
                        data_dir.display()
                    );
                }
                MigrationOutcome::Migrated { height } => {
                    println!(
                        "migrated {} to segments at height {height}; backup: {}",
                        data_dir.display(),
                        subhost_storage::LEDGER_BACKUP_FILE_NAME
                    );
                }
            }
        }
        DbCommands::Prune { data_dir, chain_id, keep_segments } => {
            let data_dir = match data_dir {
                Some(dir) => dir,
                None => default_data_dir()?,
            };
            let store = LedgerStore::open(chain_id, &data_dir)
                .with_context(|| format!("cannot open the ledger in {}", data_dir.display()))?;
            let outcome = store
                .prune(keep_segments)
                .with_context(|| format!("cannot prune the ledger in {}", data_dir.display()))?;
            match outcome {
                PruneOutcome::NothingToPrune { segments } => {
                    println!(
                        "{} keeps {segments} segment(s); nothing to prune",
                        data_dir.display()
                    );
                }
                PruneOutcome::Pruned { dropped_segments, retained_segments, base_height } => {
                    println!(
                        "pruned {}: dropped {dropped_segments} segment(s), kept {retained_segments}, \
                         history now starts at block {}",
                        data_dir.display(),
                        base_height + 1
                    );
                }
            }
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

/// A JSON-RPC client for one node, with an optional bearer token.
struct RpcClient {
    url: String,
    token: Option<subhost_rpc::SecretToken>,
}

impl RpcClient {
    fn new(url: String, token_file: Option<&Path>) -> Result<Self> {
        let token = token_file
            .map(subhost_rpc::SecretToken::from_file)
            .transpose()
            .with_context(|| "cannot load the RPC auth token")?;
        Ok(Self { url, token })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let value = self.call_value(method, params).await?;
        serde_json::from_value(value)
            .with_context(|| format!("RPC {method} returned an unexpected result shape"))
    }

    async fn call_value(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut request = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            // The CLI talks to one configured node; never chase a redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .post(&self.url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": 1,
            }));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose());
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("cannot reach the node at {}", self.url))?;

        // Say what the operator has to fix instead of a bare status code.
        match response.status() {
            reqwest::StatusCode::UNAUTHORIZED => bail!(
                "the node at {} requires authentication; pass --rpc-auth-token-file",
                self.url
            ),
            reqwest::StatusCode::TOO_MANY_REQUESTS => bail!(
                "the node at {} rate-limited this client; retry after {}s",
                self.url,
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("1")
            ),
            _ => {}
        }
        let mut response = response.error_for_status()?;

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
                "--validator",
                "0x2222222222222222222222222222222222222222:100",
                "--producer-key-file",
                "./data/producer.key",
            ],
            &["subhost", "node", "--listen", "127.0.0.1:8545", "--data-dir", "./data"],
            &["subhost", "node", "--validator", "--metrics-addr", "127.0.0.1:9090"],
            &["subhost", "node", "--validator", "--producer-key-file", "./data/validator.key"],
            &[
                "subhost",
                "node",
                "--p2p-listen",
                "/ip4/0.0.0.0/tcp/30333",
                "--bootstrap-peer",
                "/ip4/127.0.0.1/tcp/30334",
                "--no-mdns",
                "--max-peers",
                "8",
            ],
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
    fn peer_options_require_a_listen_address() {
        // Networking stays off by default.
        let parsed = Cli::try_parse_from(["subhost", "node"]).unwrap();
        match parsed.command {
            Commands::Node { p2p_listen, bootstrap_peers, no_mdns, .. } => {
                assert!(p2p_listen.is_none());
                assert!(bootstrap_peers.is_empty());
                assert!(!no_mdns);
            }
            _ => panic!("expected the node command"),
        }

        // A bootstrap peer without a listen address is a usage error, not a
        // silently ignored flag.
        assert!(Cli::try_parse_from([
            "subhost",
            "node",
            "--bootstrap-peer",
            "/ip4/127.0.0.1/tcp/30334",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["subhost", "node", "--no-mdns"]).is_err());
        assert!(Cli::try_parse_from(["subhost", "node", "--max-peers", "4"]).is_err());
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
    fn validator_parsing_accepts_the_three_forms_and_rejects_junk() {
        let address = "0x2222222222222222222222222222222222222222";
        let (secret, public) = BLSScheme::keygen();
        let public_hex = hex::encode(BLSScheme::encode_public_key(&public));
        let proof_hex =
            hex::encode(BLSScheme::encode_signature(&BLSScheme::proof_of_possession(&secret)));

        let derived = parse_validator(&format!("{address}:100")).unwrap();
        assert_eq!(derived.address, Address::new([0x22; 20]));
        assert_eq!(derived.power, 100);
        assert!(derived.public_key.is_none());
        assert!(derived.proof_of_possession.is_none());

        let explicit = parse_validator(&format!("{address}=0x{public_hex}:100")).unwrap();
        assert_eq!(explicit.public_key, Some(BLSScheme::encode_public_key(&public)));
        assert!(explicit.proof_of_possession.is_none());

        let with_proof =
            parse_validator(&format!("{address}=0x{public_hex}:100:0x{proof_hex}")).unwrap();
        assert_eq!(
            with_proof.proof_of_possession,
            Some(BLSScheme::encode_signature(&BLSScheme::proof_of_possession(&secret)))
        );

        for bad in [
            format!("{address}=0xaabb:100"),
            format!("{address}=:100"),
            format!("{address}=0xzz:100"),
            format!("{address}=0x{public_hex}:0"),
            format!("{address}=0x{public_hex}:notanumber"),
            format!("{address}=0x{public_hex}:100:0xzz"),
            format!("{address}=0x{public_hex}"),
            "nope:100".to_string(),
        ] {
            assert!(parse_validator(&bad).is_err(), "{bad:?} must be rejected");
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
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            false,
        )
        .unwrap();

        let genesis = GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap();
        assert_eq!(genesis.chain_id, 7);
        assert_eq!(genesis.allocations.get(&Address::new([0x11; 20])), Some(&1000));
        assert_eq!(genesis.initial_validators.len(), 1);
        assert!(genesis.requires_validators().is_ok());

        // The generated key is the validator identity: its public key and the
        // stored proof of possession must belong together.
        let key_path = data_dir.join(PRODUCER_KEY_FILE_NAME);
        let secret = read_producer_key(&key_path).unwrap();
        let validator = &genesis.initial_validators[0];
        assert_eq!(validator.address, Address::new([0x22; 20]));
        assert_eq!(
            validator.public_key,
            BLSScheme::encode_public_key(&BLSScheme::public_key(&secret))
        );
        let public = BLSScheme::decode_public_key(&validator.public_key).unwrap();
        let proof = BLSScheme::decode_signature(&validator.proof_of_possession).unwrap();
        assert!(BLSScheme::verify_possession(&public, &proof));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the producer key must not be group/other readable");
        }
        let key_contents = std::fs::read_to_string(&key_path).unwrap();

        // A second run must not silently replace the genesis file, and --force
        // must keep the key that already signs this validator.
        assert!(
            init_genesis(7, Some(data_dir.clone()), Vec::new(), Vec::new(), None, false).is_err()
        );
        init_genesis(
            9,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            true,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), key_contents);
        assert_eq!(GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap().chain_id, 9);
    }

    #[test]
    fn init_refuses_a_validator_key_that_does_not_match_the_producer_key() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        let (_, other) = BLSScheme::keygen();
        let other = hex::encode(BLSScheme::encode_public_key(&other));

        let result = init_genesis(
            1,
            Some(data_dir.clone()),
            Vec::new(),
            vec![format!("0x2222222222222222222222222222222222222222=0x{other}:10")],
            None,
            false,
        );

        assert!(result.is_err());
        assert!(
            !data_dir.join(GENESIS_FILE_NAME).exists(),
            "a refused genesis must not be written"
        );
    }

    #[test]
    fn init_accepts_a_proof_of_possession_signed_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        let (secret, public) = BLSScheme::keygen();
        let public = hex::encode(BLSScheme::encode_public_key(&public));
        let proof =
            hex::encode(BLSScheme::encode_signature(&BLSScheme::proof_of_possession(&secret)));

        init_genesis(
            1,
            Some(data_dir.clone()),
            Vec::new(),
            vec![format!("0x2222222222222222222222222222222222222222=0x{public}:10:0x{proof}")],
            None,
            false,
        )
        .unwrap();

        let genesis = GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap();
        assert_eq!(genesis.initial_validators.len(), 1);
        assert!(!data_dir.join(PRODUCER_KEY_FILE_NAME).exists(), "no local key was needed");
    }

    #[test]
    fn init_rejects_a_proof_that_does_not_match_the_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let (secret, _) = BLSScheme::keygen();
        let (_, other) = BLSScheme::keygen();
        let other = hex::encode(BLSScheme::encode_public_key(&other));
        let proof =
            hex::encode(BLSScheme::encode_signature(&BLSScheme::proof_of_possession(&secret)));

        assert!(init_genesis(
            1,
            Some(dir.path().to_path_buf()),
            Vec::new(),
            vec![format!("0x2222222222222222222222222222222222222222=0x{other}:10:0x{proof}")],
            None,
            false,
        )
        .is_err());
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
            None,
            false,
        )
        .is_err());
    }

    async fn serve_once(
        status: &'static str,
        body: String,
    ) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn the_rpc_client_sends_the_bearer_token() {
        let (addr, server) =
            serve_once("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#.to_string()).await;
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("rpc-token");
        std::fs::write(&token_path, "0123456789abcdef\n").unwrap();

        let client = RpcClient::new(format!("http://{addr}"), Some(&token_path)).unwrap();
        let value: String = client.call("eth_chainId", serde_json::json!([])).await.unwrap();
        assert_eq!(value, "0x1");

        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer 0123456789abcdef"));
    }

    #[tokio::test]
    async fn a_401_names_the_token_flag_instead_of_a_bare_status() {
        let (addr, server) = serve_once(
            "401 Unauthorized",
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"unauthorized"}}"#
                .to_string(),
        )
        .await;

        let client = RpcClient::new(format!("http://{addr}"), None).unwrap();
        let error = client.call_value("eth_chainId", serde_json::json!([])).await.unwrap_err();
        assert!(error.to_string().contains("--rpc-auth-token-file"));

        server.await.unwrap();
    }
}
