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
use subhost_keystore::{
    check_creation_password, is_keystore_contents, load_credential_file, read_bounded_owner_only,
    read_encrypted_producer_key, read_public_key, write_encrypted_producer_key_new, DEFAULT_LOG_N,
    KEYSTORE_FILE_NAME, MAX_KEYSTORE_FILE_BYTES,
};
use subhost_node::{
    read_producer_key, write_producer_key, Node, NodeConfig, P2pConfig, GENESIS_FILE_NAME,
    PRODUCER_KEY_FILE_NAME,
};
use subhost_storage::{LedgerStore, MigrationOutcome, PruneOutcome};
use subhost_telemetry::{TelemetryConfig, Verbosity};
use subhost_wallet::Wallet;
use zeroize::Zeroizing;

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
        /// An encrypted `producer.keystore.json` is unlocked instead when
        /// `--producer-credential-file` is supplied.
        #[arg(long)]
        producer_key_file: Option<PathBuf>,

        /// Owner-only credential file unlocking an encrypted producer keystore.
        /// Required when the producer key file is a keystore; never a password
        /// argument. With no key file present, init refuses rather than
        /// generating one: create it first with `subhost producer create`.
        #[arg(long)]
        producer_credential_file: Option<PathBuf>,

        /// Overwrite an existing genesis file.
        #[arg(long)]
        force: bool,
    },

    /// Run a node: restore the ledger and serve JSON-RPC.
    Node {
        /// Require a genesis validator set and refuse to start without one.
        #[arg(long)]
        validator: bool,

        /// Explicit unsigned single-writer demo mode; requires no genesis validators.
        #[arg(long, conflicts_with = "validator")]
        single_writer: bool,

        /// Base consensus view timeout in milliseconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        view_timeout_ms: Option<u64>,

        /// Producer key file used to sign blocks. Defaults to
        /// `<data-dir>/producer.key`.
        #[arg(long, requires = "validator")]
        producer_key_file: Option<PathBuf>,

        /// Owner-only credential file unlocking the encrypted producer
        /// keystore for node start-up (ADR-011). With this flag the node
        /// unlocks `<data-dir>/producer.keystore.json` (or
        /// `--producer-key-file`) and boots with the unlocked key; it never
        /// falls back to plaintext and never generates a key. Requires
        /// `--validator`.
        #[arg(long, requires = "validator")]
        producer_credential_file: Option<PathBuf>,

        /// Persist accepted-but-unmined transactions to `<data-dir>/mempool.bin`
        /// and restore them on start-up (Q-016/Q-038). Opt-in: unset keeps the
        /// pool volatile. A corrupt or foreign snapshot refuses boot; recovery
        /// is moving the snapshot aside and restarting (see
        /// docs/operations/mempool-persistence.md).
        #[arg(long)]
        mempool_persist: bool,

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

        /// Automatic ledger retention in blocks (Q-036). Unset = off.
        /// Accepted now; the prune trigger lands with the S12.3 retention
        /// slice, so until then this has no effect on a running node and
        /// `subhost db prune` remains the only prune path. The newest
        /// snapshot is the hard floor: the real bound is snapshot cadence + N.
        /// Values below 4096 warn at boot once the trigger lands (degraded
        /// sync-serving role).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        retention_blocks: Option<u64>,
    },

    /// Manage local wallets.
    Wallet {
        #[command(subcommand)]
        cmd: WalletCommands,
    },

    /// Manage the encrypted BLS producer keystore (validator identity).
    Producer {
        #[command(subcommand)]
        cmd: ProducerCommands,
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
enum ProducerCommands {
    /// Generate a new producer key into an encrypted keystore.
    ///
    /// Refuses to replace an existing file. To encrypt an existing plaintext
    /// `producer.key`, use `producer migrate` instead. The password is read
    /// twice without echo, or taken from an owner-only `--credential-file`;
    /// there is no password argument.
    Create {
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Keystore destination. Defaults to `<data-dir>/producer.keystore.json`.
        #[arg(long)]
        keystore_file: Option<PathBuf>,

        /// Owner-only file holding the password (one trailing LF/CRLF stripped).
        #[arg(long)]
        credential_file: Option<PathBuf>,
    },

    /// Print a keystore's public key without unlocking it.
    ///
    /// Reads the authenticated header only; the tag is not verified until an
    /// unlock, so this cannot distinguish a valid keystore from a corrupt one.
    Inspect {
        /// Keystore file to inspect.
        path: PathBuf,
    },

    /// Verify a password unlocks the keystore; prints the public key only.
    Unlock {
        /// Keystore file to unlock.
        path: PathBuf,

        /// Owner-only file holding the password (one trailing LF/CRLF stripped).
        #[arg(long)]
        credential_file: Option<PathBuf>,
    },

    /// Encrypt an existing plaintext `producer.key` into a new keystore.
    ///
    /// The source file is preserved on success and on every failure; removing
    /// it later is an explicit operator action. The new file is decrypted and
    /// compared against the source before success is reported. When a genesis
    /// exists, the key must be one of its validators.
    Migrate {
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Legacy plaintext key file. Defaults to `<data-dir>/producer.key`.
        #[arg(long)]
        producer_key_file: Option<PathBuf>,

        /// Keystore destination. Defaults to `<data-dir>/producer.keystore.json`.
        #[arg(long)]
        keystore_file: Option<PathBuf>,

        /// Owner-only file holding the new password (one trailing LF/CRLF stripped).
        #[arg(long)]
        credential_file: Option<PathBuf>,
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
                producer_credential_file,
                force,
            } => init_genesis(
                chain_id,
                data_dir,
                allocations,
                validators,
                producer_key_file,
                producer_credential_file,
                force,
            ),
            Commands::Node {
                validator,
                single_writer,
                view_timeout_ms,
                producer_key_file,
                producer_credential_file,
                mempool_persist,
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
                retention_blocks,
            } => {
                run_node(
                    validator,
                    single_writer,
                    view_timeout_ms,
                    producer_key_file,
                    producer_credential_file,
                    mempool_persist,
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
                    retention_blocks,
                )
                .await
            }
            Commands::Wallet { cmd } => handle_wallet(cmd),
            Commands::Producer { cmd } => handle_producer(cmd),
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
    producer_credential_file: Option<PathBuf>,
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
    genesis.initial_validators = resolve_validators(
        specs,
        &data_dir,
        producer_key_file,
        producer_credential_file.as_deref(),
    )?;

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
    producer_credential_file: Option<&Path>,
) -> Result<Vec<ValidatorInfo>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    // Resolve the producer key path fail-closed. With a credential, the
    // default target is the keystore, never the legacy file. Without one, a
    // present keystore must not be shadowed by generating a new plaintext key.
    let key_path = match producer_key_file {
        Some(path) => path,
        None if producer_credential_file.is_some() => data_dir.join(KEYSTORE_FILE_NAME),
        None => {
            let keystore = data_dir.join(KEYSTORE_FILE_NAME);
            if keystore.exists() {
                bail!(
                    "{} is an encrypted producer keystore; pass --producer-credential-file to unlock it",
                    keystore.display()
                );
            }
            data_dir.join(PRODUCER_KEY_FILE_NAME)
        }
    };
    let mut secret = None;
    if specs.iter().any(|spec| spec.proof_of_possession.is_none()) {
        secret = Some(load_or_create_producer_key(&key_path, producer_credential_file)?);
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
///
/// An existing file is routed by content: a keystore is unlocked with the
/// supplied credential file and never falls back to plaintext; a legacy hex
/// file keeps the original loader. With a credential file but no key file,
/// init refuses instead of generating: encrypted keys are created explicitly
/// with `subhost producer create`.
fn load_or_create_producer_key(path: &Path, credential_file: Option<&Path>) -> Result<PrivateKey> {
    if path.exists() {
        let bytes = read_bounded_owner_only(path, MAX_KEYSTORE_FILE_BYTES)
            .with_context(|| format!("cannot read the producer key file {}", path.display()))?;
        if is_keystore_contents(&bytes) {
            let Some(credential_path) = credential_file else {
                bail!(
                    "{} is an encrypted producer keystore; pass --producer-credential-file to unlock it",
                    path.display()
                );
            };
            let password = load_credential_file(credential_path).with_context(|| {
                format!("cannot read the producer credential file {}", credential_path.display())
            })?;
            return read_encrypted_producer_key(path, password.as_slice()).with_context(|| {
                format!("cannot unlock the producer keystore {}", path.display())
            });
        }
        return read_producer_key(path)
            .with_context(|| format!("cannot use the producer key in {}", path.display()));
    }
    if let Some(credential_path) = credential_file {
        bail!(
            "{} does not exist; create it first with `subhost producer create` \
             (a credential file was supplied: {})",
            path.display(),
            credential_path.display()
        );
    }
    let (secret, public_key) = BLSScheme::keygen();
    write_producer_key(path, &secret)
        .with_context(|| format!("cannot write the producer key to {}", path.display()))?;
    println!("Generated producer key {}", path.display());
    println!("  public key: 0x{}", hex::encode(BLSScheme::encode_public_key(&public_key)));
    println!("  mode 0600. Keep it: it is this validator's identity.");
    Ok(secret)
}

fn handle_producer(cmd: ProducerCommands) -> Result<()> {
    match cmd {
        ProducerCommands::Create { data_dir, keystore_file, credential_file } => {
            producer_create(data_dir, keystore_file, credential_file.as_deref())
        }
        ProducerCommands::Inspect { path } => producer_inspect(&path),
        ProducerCommands::Unlock { path, credential_file } => {
            producer_unlock(&path, credential_file.as_deref())
        }
        ProducerCommands::Migrate {
            data_dir,
            producer_key_file,
            keystore_file,
            credential_file,
        } => {
            producer_migrate(data_dir, producer_key_file, keystore_file, credential_file.as_deref())
        }
    }
}

/// Generate a new identity into an encrypted keystore; never replace a file.
fn producer_create(
    data_dir: Option<PathBuf>,
    keystore_file: Option<PathBuf>,
    credential_file: Option<&Path>,
) -> Result<()> {
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let dest = match keystore_file {
        Some(path) => path,
        None => {
            // Creating a second identity next to a live plaintext key is almost
            // always a mistake: the operator meant to encrypt the existing one.
            let legacy = data_dir.join(PRODUCER_KEY_FILE_NAME);
            if legacy.exists() {
                bail!(
                    "{} already holds a plaintext producer key; encrypt it with \
                     `subhost producer migrate` instead of creating a new identity",
                    legacy.display()
                );
            }
            data_dir.join(KEYSTORE_FILE_NAME)
        }
    };
    if dest.exists() {
        bail!("{} already exists; producer keystores are never replaced", dest.display());
    }
    let password = obtain_creation_password(credential_file)?;
    let parent = dest.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let (secret, public_key) = BLSScheme::keygen();
    write_encrypted_producer_key_new(&dest, &secret, password.as_slice())
        .with_context(|| format!("cannot write the encrypted keystore {}", dest.display()))?;
    println!("Encrypted producer keystore written to {}", dest.display());
    println!("  public key: 0x{}", hex::encode(BLSScheme::encode_public_key(&public_key)));
    println!("  scrypt log_n={DEFAULT_LOG_N}, r=8, p=1; AES-256-GCM.");
    println!("  Keep the password: losing it loses this validator's identity.");
    Ok(())
}

fn producer_inspect(path: &Path) -> Result<()> {
    let public_key = read_public_key(path)
        .with_context(|| format!("cannot inspect the keystore {}", path.display()))?;
    println!("public key: 0x{}", hex::encode(&public_key));
    println!("(header only; the tag is verified by `subhost producer unlock`)");
    Ok(())
}

fn producer_unlock(path: &Path, credential_file: Option<&Path>) -> Result<()> {
    let password = obtain_unlock_password(credential_file)?;
    let secret = read_encrypted_producer_key(path, password.as_slice())
        .with_context(|| format!("cannot unlock the keystore {}", path.display()))?;
    println!("Unlocked {}", path.display());
    println!(
        "  public key: 0x{}",
        hex::encode(BLSScheme::encode_public_key(&BLSScheme::public_key(&secret)))
    );
    Ok(())
}

/// Encrypt the legacy plaintext key into a separate keystore, preserving the
/// source on success and on every failure.
fn producer_migrate(
    data_dir: Option<PathBuf>,
    producer_key_file: Option<PathBuf>,
    keystore_file: Option<PathBuf>,
    credential_file: Option<&Path>,
) -> Result<()> {
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let source = producer_key_file.unwrap_or_else(|| data_dir.join(PRODUCER_KEY_FILE_NAME));
    if !source.exists() {
        bail!("no legacy producer key at {}; nothing to migrate", source.display());
    }
    let dest = keystore_file.unwrap_or_else(|| data_dir.join(KEYSTORE_FILE_NAME));
    if dest.exists() {
        bail!("{} already exists; migration never replaces a keystore", dest.display());
    }
    let secret = read_producer_key(&source)
        .with_context(|| format!("cannot read the legacy producer key {}", source.display()))?;
    let public_key = BLSScheme::encode_public_key(&BLSScheme::public_key(&secret));

    // When a genesis is present, the migrated identity must be one of its
    // validators. Refuse before writing anything, so a wrong data directory
    // cannot produce a keystore for an unrelated chain.
    let genesis_path = data_dir.join(GENESIS_FILE_NAME);
    if genesis_path.exists() {
        let genesis = GenesisConfig::load(&genesis_path)
            .with_context(|| format!("cannot read {}", genesis_path.display()))?;
        if !genesis.initial_validators.is_empty()
            && !genesis
                .initial_validators
                .iter()
                .any(|validator| validator.public_key == public_key)
        {
            bail!(
                "the key in {} is not a validator in {}; refusing to migrate the wrong identity",
                source.display(),
                genesis_path.display()
            );
        }
    }

    let password = obtain_creation_password(credential_file)?;
    // The writer publishes with no-clobber semantics and verifies the new file
    // by decrypting it back to the same secret before returning.
    write_encrypted_producer_key_new(&dest, &secret, password.as_slice())
        .with_context(|| format!("cannot write the encrypted keystore {}", dest.display()))?;
    println!("Migrated {} -> {}", source.display(), dest.display());
    println!("  public key: 0x{}", hex::encode(&public_key));
    println!("  The legacy file is preserved. Remove it explicitly when ready;");
    println!("  deletion cannot promise physical erasure on SSD/COW/backup systems.");
    Ok(())
}

/// Creation credentials: an owner-only file, or two matching non-echo prompts.
fn obtain_creation_password(credential_file: Option<&Path>) -> Result<Zeroizing<Vec<u8>>> {
    if let Some(path) = credential_file {
        let password = load_credential_file(path)
            .with_context(|| format!("cannot read the credential file {}", path.display()))?;
        check_creation_password(password.as_slice())
            .with_context(|| format!("credential file {} rejected", path.display()))?;
        return Ok(password);
    }
    let first = prompt_password_bytes("New producer keystore password: ")?;
    let second = prompt_password_bytes("Confirm password: ")?;
    creation_password_from_pair(first.as_slice(), second.as_slice())
}

/// Unlock credentials: an owner-only file, or one non-echo prompt. No creation
/// policy applies here: an existing valid file must stay decryptable.
fn obtain_unlock_password(credential_file: Option<&Path>) -> Result<Zeroizing<Vec<u8>>> {
    if let Some(path) = credential_file {
        return load_credential_file(path)
            .with_context(|| format!("cannot read the credential file {}", path.display()));
    }
    prompt_password_bytes("Producer keystore password: ")
}

/// Interactive input is UTF-8 exactly as the terminal reader returns it: no
/// trimming and no Unicode normalization. The credential-file path is the
/// exact-byte-string path for scripts and services.
fn prompt_password_bytes(prompt: &str) -> Result<Zeroizing<Vec<u8>>> {
    let password = rpassword::prompt_password(prompt).context(
        "cannot read a password from the terminal; pass --credential-file for non-interactive use",
    )?;
    Ok(Zeroizing::new(password.into_bytes()))
}

/// The two creation entries must match exactly and pass the creation policy.
fn creation_password_from_pair(first: &[u8], second: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if first != second {
        bail!("the two password entries do not match");
    }
    check_creation_password(first)?;
    Ok(Zeroizing::new(first.to_vec()))
}

#[allow(clippy::too_many_arguments)]
async fn run_node(
    validator: bool,
    single_writer: bool,
    view_timeout_ms: Option<u64>,
    producer_key_file: Option<PathBuf>,
    producer_credential_file: Option<PathBuf>,
    mempool_persist: bool,
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
    retention_blocks: Option<u64>,
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

    // The node owns defaults, including any later policy/timeout fields.
    let mut config = NodeConfig::new(data_dir);
    config.rpc_addr = rpc_addr;
    config.max_rpc_connections = max_connections;
    config.rpc_security = rpc_security;
    config.metrics_addr = metrics_addr;
    config.validator = validator;
    config.single_writer = single_writer;
    if let Some(timeout) = view_timeout_ms {
        config.view_timeout_ms = timeout;
    }
    config.producer_key_file = producer_key_file;
    // Q-036: accepted now, inert until the node-side prune trigger lands.
    config.retention_blocks = retention_blocks;
    // Q-038: durability is an explicit operator choice; the node attaches the
    // store and restores the snapshot only when this is set (boot policy A).
    config.mempool_persist = mempool_persist;
    config.network = network;
    validate_node_policy(&config)?;
    // ADR-011: with a credential, boot from the unlocked keystore key; the
    // configured key file is never read by the node when a key is supplied.
    // Without one, the legacy loader path is unchanged.
    let node = match producer_credential_file {
        Some(credential_path) => {
            let key = unlock_producer_keystore(
                &config.data_dir,
                config.producer_key_file.as_deref(),
                &credential_path,
            )?;
            Node::bootstrap_with_producer_key(config, key)?
        }
        None => Node::bootstrap(config)?,
    };
    node.run().await?;
    Ok(())
}

/// Unlock the encrypted producer keystore for node start-up (ADR-011).
///
/// Fail-closed: the keystore must exist and unlock with the owner-only
/// credential file; there is no plaintext fallback and no key generation.
/// The default target is `<data-dir>/producer.keystore.json`, overridden by
/// `--producer-key-file`.
fn unlock_producer_keystore(
    data_dir: &Path,
    key_file: Option<&Path>,
    credential_path: &Path,
) -> Result<PrivateKey> {
    let keystore = match key_file {
        Some(path) => path.to_path_buf(),
        None => data_dir.join(KEYSTORE_FILE_NAME),
    };
    if !keystore.exists() {
        bail!(
            "{} does not exist; create it with `subhost producer create` or encrypt the legacy key with `subhost producer migrate`",
            keystore.display()
        );
    }
    let password = load_credential_file(credential_path).with_context(|| {
        format!("cannot read the producer credential file {}", credential_path.display())
    })?;
    read_encrypted_producer_key(&keystore, password.as_slice())
        .with_context(|| format!("cannot unlock the producer keystore {}", keystore.display()))
}

/// Reject ambiguous CLI modes before the node touches its ledger or journal.
fn validate_node_policy(config: &NodeConfig) -> Result<()> {
    if config.view_timeout_ms == 0 {
        bail!("--view-timeout-ms must be positive");
    }
    let path = config.genesis_path();
    let has_validators = if path.is_file() {
        !GenesisConfig::load(&path)?.initial_validators.is_empty()
    } else {
        false
    };
    if config.single_writer && (config.validator || has_validators) {
        bail!("--single-writer is incompatible with validator mode or a genesis validator set");
    }
    if config.validator && !has_validators {
        bail!("--validator requires a genesis validator set");
    }
    if !config.single_writer && !has_validators {
        bail!("an unsigned demo chain requires explicit --single-writer");
    }
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
                // `prune` (keep-segments) still refuses with an error instead of
                // this outcome; the arm keeps the match exhaustive for Q-036.
                PruneOutcome::RefusedBySnapshot { snapshot_height, requested_height } => {
                    bail!(
                        "cannot prune {}: the newest snapshot is at block {snapshot_height}, \
                         which a prune to {requested_height} would drop",
                        data_dir.display()
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
    fn node_policy_flags_reject_conflicts_and_nonpositive_timeouts() {
        assert!(Cli::try_parse_from(["subhost", "node", "--validator", "--single-writer"]).is_err());
        for timeout in ["0", "-1"] {
            assert!(Cli::try_parse_from(["subhost", "node", "--view-timeout-ms", timeout]).is_err());
        }
        let cli = Cli::try_parse_from([
            "subhost",
            "node",
            "--single-writer",
            "--view-timeout-ms",
            "5000",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Node { single_writer: true, view_timeout_ms: Some(5000), .. }
        ));
    }

    #[test]
    fn retention_blocks_flag_parses_positive_values_and_defaults_off() {
        // Q-036 Q5: unset = off (None), any n >= 1 accepted, 0/negative refused.
        let cli = Cli::try_parse_from(["subhost", "node", "--single-writer"]).unwrap();
        assert!(matches!(cli.command, Commands::Node { retention_blocks: None, .. }));

        let cli = Cli::try_parse_from([
            "subhost",
            "node",
            "--single-writer",
            "--retention-blocks",
            "50000",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Node { retention_blocks: Some(50000), .. }));

        // Aggressive lean values are allowed (Q-036 Q7); the boot warning below
        // 4096 belongs to the node trigger slice, not the parser.
        let cli =
            Cli::try_parse_from(["subhost", "node", "--single-writer", "--retention-blocks", "1"])
                .unwrap();
        assert!(matches!(cli.command, Commands::Node { retention_blocks: Some(1), .. }));

        for bad in ["0", "-1", "not-a-number"] {
            assert!(Cli::try_parse_from([
                "subhost",
                "node",
                "--single-writer",
                "--retention-blocks",
                bad,
            ])
            .is_err());
        }
    }

    #[test]
    fn mempool_persist_flag_defaults_off_and_is_opt_in() {
        let cli = Cli::try_parse_from(["subhost", "node"]).unwrap();
        assert!(matches!(cli.command, Commands::Node { mempool_persist: false, .. }));
        let cli = Cli::try_parse_from(["subhost", "node", "--mempool-persist"]).unwrap();
        assert!(matches!(cli.command, Commands::Node { mempool_persist: true, .. }));
        // Durability composes with validator and demo modes alike.
        let cli =
            Cli::try_parse_from(["subhost", "node", "--validator", "--mempool-persist"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Node { validator: true, mempool_persist: true, .. }
        ));
    }

    #[test]
    fn unsigned_mode_requires_explicit_selection_before_directory_creation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("not-created");
        let mut config = NodeConfig::new(&path);
        assert!(validate_node_policy(&config).is_err());
        assert!(!path.exists());
        config.single_writer = true;
        assert!(validate_node_policy(&config).is_ok());
        assert!(!path.exists());
        config.validator = true;
        assert!(validate_node_policy(&config).is_err());
        config.validator = false;
        config.view_timeout_ms = 0;
        assert!(validate_node_policy(&config).is_err());
    }

    #[test]
    fn validator_genesis_cannot_select_unsigned_mode() {
        let root = tempfile::tempdir().unwrap();
        let (secret, public) = BLSScheme::keygen();
        let mut genesis = GenesisConfig::default();
        genesis.initial_validators.push(ValidatorInfo {
            address: Address::new([1; 20]),
            public_key: BLSScheme::encode_public_key(&public),
            power: 1,
            proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                &secret,
            )),
        });
        genesis.save(&root.path().join("genesis.json")).unwrap();
        let mut config = NodeConfig::new(root.path());
        assert!(validate_node_policy(&config).is_ok());
        config.single_writer = true;
        assert!(validate_node_policy(&config).is_err());
        assert!(!root.path().join("ledger").exists());
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
            &[
                "subhost",
                "node",
                "--single-writer",
                "--listen",
                "127.0.0.1:8545",
                "--data-dir",
                "./data",
            ],
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
        assert!(init_genesis(7, Some(data_dir.clone()), Vec::new(), Vec::new(), None, None, false)
            .is_err());
        init_genesis(
            9,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
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

    /// Owner-only credential file with the shared trailing-newline convention.
    fn write_credential(dir: &Path, name: &str, password: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, password).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    const KEYSTORE_PASSWORD: &[u8] = b"correct horse battery staple\n";

    #[test]
    fn producer_create_and_init_share_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_create(Some(data_dir.clone()), None, Some(&credential)).unwrap();

        let keystore = data_dir.join(KEYSTORE_FILE_NAME);
        assert!(keystore.exists());
        assert!(
            !data_dir.join(PRODUCER_KEY_FILE_NAME).exists(),
            "create must not leave a plaintext key behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&keystore).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            Some(credential.clone()),
            false,
        )
        .unwrap();

        // The genesis identity, the keystore header and the unlocked secret
        // must all be the same key, and the stored proof must verify.
        let genesis = GenesisConfig::load(&data_dir.join(GENESIS_FILE_NAME)).unwrap();
        let validator = &genesis.initial_validators[0];
        assert_eq!(validator.public_key, read_public_key(&keystore).unwrap());
        let password = load_credential_file(&credential).unwrap();
        let secret = read_encrypted_producer_key(&keystore, password.as_slice()).unwrap();
        assert_eq!(
            validator.public_key,
            BLSScheme::encode_public_key(&BLSScheme::public_key(&secret))
        );
        let public = BLSScheme::decode_public_key(&validator.public_key).unwrap();
        let proof = BLSScheme::decode_signature(&validator.proof_of_possession).unwrap();
        assert!(BLSScheme::verify_possession(&public, &proof));
    }

    #[test]
    fn init_refuses_a_keystore_without_a_credential_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_create(Some(data_dir.clone()), None, Some(&credential)).unwrap();

        let result = init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            None,
            false,
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("--producer-credential-file"), "error was: {error}");
        assert!(
            !data_dir.join(GENESIS_FILE_NAME).exists(),
            "a refused init must not write a genesis"
        );
    }

    #[test]
    fn init_refuses_a_wrong_credential_and_a_loose_credential_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_create(Some(data_dir.clone()), None, Some(&credential)).unwrap();

        let wrong = write_credential(dir.path(), "wrong", b"another password entirely\n");
        let result = init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            Some(wrong),
            false,
        );
        assert!(result.is_err());
        assert!(!data_dir.join(GENESIS_FILE_NAME).exists());

        // A group-readable credential file is refused before any KDF work.
        let loose = dir.path().join("loose");
        std::fs::write(&loose, KEYSTORE_PASSWORD).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let result = init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            Some(loose),
            false,
        );
        assert!(result.is_err());
        assert!(!data_dir.join(GENESIS_FILE_NAME).exists());
    }

    #[test]
    fn init_with_a_credential_but_no_key_refuses_to_generate() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);

        let result = init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            Some(credential),
            false,
        );
        assert!(result.is_err());
        assert!(!data_dir.join(PRODUCER_KEY_FILE_NAME).exists());
        assert!(!data_dir.join(KEYSTORE_FILE_NAME).exists());
        assert!(!data_dir.join(GENESIS_FILE_NAME).exists());
    }

    #[test]
    fn migrate_preserves_the_source_and_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            None,
            false,
        )
        .unwrap();
        let source = data_dir.join(PRODUCER_KEY_FILE_NAME);
        let source_bytes = std::fs::read(&source).unwrap();
        let genesis_bytes = std::fs::read(data_dir.join(GENESIS_FILE_NAME)).unwrap();

        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_migrate(Some(data_dir.clone()), None, None, Some(&credential)).unwrap();

        let keystore = data_dir.join(KEYSTORE_FILE_NAME);
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes, "the source must survive");
        assert_eq!(
            std::fs::read(data_dir.join(GENESIS_FILE_NAME)).unwrap(),
            genesis_bytes,
            "migration must not touch the genesis"
        );
        let password = load_credential_file(&credential).unwrap();
        let migrated = read_encrypted_producer_key(&keystore, password.as_slice()).unwrap();
        assert_eq!(
            BLSScheme::encode_secret_key(&migrated),
            BLSScheme::encode_secret_key(&read_producer_key(&source).unwrap())
        );

        // A second migration must not replace the keystore or the source.
        let keystore_bytes = std::fs::read(&keystore).unwrap();
        assert!(producer_migrate(Some(data_dir.clone()), None, None, Some(&credential)).is_err());
        assert_eq!(std::fs::read(&keystore).unwrap(), keystore_bytes);
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);

        // With both files present, init without a credential must refuse
        // rather than silently keep using the legacy identity.
        assert!(init_genesis(
            9,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            None,
            true,
        )
        .is_err());
        assert_eq!(
            std::fs::read(data_dir.join(GENESIS_FILE_NAME)).unwrap(),
            genesis_bytes,
            "the refused init must not rewrite the genesis"
        );
    }

    #[test]
    fn migrate_refuses_a_key_that_is_not_in_the_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let (genesis_secret, genesis_public) = BLSScheme::keygen();
        let public_hex = hex::encode(BLSScheme::encode_public_key(&genesis_public));
        let proof_hex = hex::encode(BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
            &genesis_secret,
        )));
        init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec![format!(
                "0x2222222222222222222222222222222222222222=0x{public_hex}:10:0x{proof_hex}"
            )],
            None,
            None,
            false,
        )
        .unwrap();

        // A different local key: migrating it would encrypt the wrong identity.
        let (other_secret, _) = BLSScheme::keygen();
        let source = data_dir.join(PRODUCER_KEY_FILE_NAME);
        write_producer_key(&source, &other_secret).unwrap();
        let source_bytes = std::fs::read(&source).unwrap();

        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        assert!(producer_migrate(Some(data_dir.clone()), None, None, Some(&credential)).is_err());
        assert!(!data_dir.join(KEYSTORE_FILE_NAME).exists());
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
    }

    #[test]
    fn migrate_without_a_source_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        assert!(producer_migrate(Some(dir.path().to_path_buf()), None, None, Some(&credential))
            .is_err());
        assert!(!dir.path().join(KEYSTORE_FILE_NAME).exists());
    }

    #[test]
    fn unlock_and_inspect_follow_the_credential_convention() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_create(Some(data_dir.clone()), None, Some(&credential)).unwrap();
        let keystore = data_dir.join(KEYSTORE_FILE_NAME);

        producer_inspect(&keystore).unwrap();
        producer_unlock(&keystore, Some(&credential)).unwrap();

        let wrong = write_credential(dir.path(), "wrong", b"another password entirely\n");
        assert!(producer_unlock(&keystore, Some(&wrong)).is_err());

        // A legacy hex file is not a keystore: inspect refuses it.
        let legacy = dir.path().join("legacy.key");
        let (secret, _) = BLSScheme::keygen();
        write_producer_key(&legacy, &secret).unwrap();
        assert!(producer_inspect(&legacy).is_err());
    }

    #[test]
    fn create_refuses_a_second_identity_next_to_a_live_key() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        init_genesis(
            7,
            Some(data_dir.clone()),
            Vec::new(),
            vec!["0x2222222222222222222222222222222222222222:10".to_string()],
            None,
            None,
            false,
        )
        .unwrap();
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);

        // Default destination: the legacy key must be migrated, not shadowed.
        let error = producer_create(Some(data_dir.clone()), None, Some(&credential))
            .unwrap_err()
            .to_string();
        assert!(error.contains("migrate"), "error was: {error}");
        assert!(!data_dir.join(KEYSTORE_FILE_NAME).exists());

        // An explicit destination is allowed, but never replaced.
        let explicit = dir.path().join("other.keystore.json");
        producer_create(Some(data_dir.clone()), Some(explicit.clone()), Some(&credential)).unwrap();
        let bytes = std::fs::read(&explicit).unwrap();
        assert!(producer_create(Some(data_dir.clone()), Some(explicit.clone()), Some(&credential))
            .is_err());
        assert_eq!(std::fs::read(&explicit).unwrap(), bytes);
    }

    #[test]
    fn creation_password_pair_must_match_and_meet_the_policy() {
        assert!(creation_password_from_pair(b"twelve bytes", b"twelve bytez").is_err());
        assert!(creation_password_from_pair(b"eleven byte", b"eleven byte").is_err());
        let over_cap = vec![b'x'; subhost_keystore::MAX_PASSWORD_BYTES + 1];
        assert!(creation_password_from_pair(&over_cap, &over_cap).is_err());
        let password = creation_password_from_pair(b"  spaced pass  ", b"  spaced pass  ").unwrap();
        assert_eq!(password.as_slice(), b"  spaced pass  ");
    }

    #[test]
    fn node_credential_flag_requires_validator_and_parses() {
        assert!(Cli::try_parse_from([
            "subhost",
            "node",
            "--producer-credential-file",
            "/tmp/cred",
        ])
        .is_err());
        let cli = Cli::try_parse_from([
            "subhost",
            "node",
            "--validator",
            "--producer-credential-file",
            "/tmp/cred",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Node { producer_credential_file: Some(_), .. }));
    }

    #[test]
    fn node_unlock_uses_the_default_keystore_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let credential = write_credential(dir.path(), "credential", KEYSTORE_PASSWORD);
        producer_create(Some(data_dir.clone()), None, Some(&credential)).unwrap();
        let keystore = data_dir.join(KEYSTORE_FILE_NAME);

        // The unlocked key is the keystore identity, via the default path and
        // via an explicit --producer-key-file override.
        for key_file in [None, Some(keystore.as_path())] {
            let key = unlock_producer_keystore(&data_dir, key_file, &credential).unwrap();
            assert_eq!(
                BLSScheme::encode_public_key(&BLSScheme::public_key(&key)),
                read_public_key(&keystore).unwrap()
            );
        }

        // Wrong credential: fail closed, no plaintext fallback.
        let wrong = write_credential(dir.path(), "wrong", b"another password entirely\n");
        assert!(unlock_producer_keystore(&data_dir, None, &wrong).is_err());

        // A group-readable credential file is refused.
        let loose = dir.path().join("loose");
        std::fs::write(&loose, KEYSTORE_PASSWORD).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(unlock_producer_keystore(&data_dir, None, &loose).is_err());

        // Missing keystore: refuse and never generate a key.
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        // PrivateKey deliberately has no Debug; .err() avoids unwrap_err's bound.
        let error = unlock_producer_keystore(&empty, None, &credential)
            .err()
            .expect("missing keystore must refuse start-up")
            .to_string();
        assert!(error.contains("producer create"), "error was: {error}");
        assert!(!empty.join(KEYSTORE_FILE_NAME).exists());
        assert!(!empty.join(PRODUCER_KEY_FILE_NAME).exists());

        // A legacy hex file at the keystore path is not a keystore.
        let legacy_dir = dir.path().join("legacy");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let (secret, _) = BLSScheme::keygen();
        write_producer_key(&legacy_dir.join(KEYSTORE_FILE_NAME), &secret).unwrap();
        assert!(unlock_producer_keystore(&legacy_dir, None, &credential).is_err());
    }
}
