use clap::{Parser, Subcommand};
use std::path::PathBuf;
use subhost_core::Address;
use subhost_wallet::Wallet;

#[derive(Parser)]
#[command(name = "subhost")]
#[command(about = "Subhost Web3 CLI")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        chain_id: Option<u64>,
        
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        #[arg(long = "alloc", value_name = "ADDRESS=BALANCE")]
        allocations: Vec<String>,
    },
    
    Node {
        #[arg(long)]
        validator: bool,
        
        #[arg(short, long)]
        bootnodes: Vec<String>,
        
        #[arg(short, long, default_value = "0.0.0.0:30333")]
        listen: String,

        #[arg(short, long)]
        data_dir: Option<PathBuf>,
    },
    
    Wallet {
        #[command(subcommand)]
        cmd: WalletCommands,
    },
    
    Tx {
        #[command(subcommand)]
        cmd: TxCommands,
    },
    
    Query {
        #[command(subcommand)]
        cmd: QueryCommands,
    },
    
    Contract {
        #[command(subcommand)]
        cmd: ContractCommands,
    },
}

#[derive(Subcommand)]
enum WalletCommands {
    New {
        #[arg(short, long)]
        password: String,
        
        #[arg(short, long)]
        name: Option<String>,
    },
    
    List,
    
    Show {
        address: String,
    },
    
    Export {
        address: String,
        
        #[arg(short, long)]
        password: String,
    },
    
    Import {
        #[arg(long)]
        private_key: String,
        
        #[arg(short, long)]
        password: String,
    },
}

#[derive(Subcommand)]
enum TxCommands {
    Send {
        #[arg(short, long)]
        from: String,
        
        #[arg(short, long)]
        to: String,
        
        #[arg(short, long)]
        amount: u128,
        
        #[arg(short, long)]
        gas_price: Option<u128>,
        
        #[arg(short, long)]
        data: Option<String>,
    },
    
    Status {
        tx_hash: String,
    },
}

#[derive(Subcommand)]
enum QueryCommands {
    Balance {
        address: String,
    },
    
    Block {
        #[arg(long)]
        height: Option<u64>,
        
        #[arg(long)]
        hash: Option<String>,
    },
    
    Account {
        address: String,
    },
    
    Validators,
}

#[derive(Subcommand)]
enum ContractCommands {
    Deploy {
        #[arg(short, long)]
        from: String,
        
        #[arg(short, long)]
        bytecode: PathBuf,
        
        #[arg(short, long)]
        gas_limit: Option<u64>,
    },
    
    Call {
        contract: String,
        
        #[arg(short, long)]
        data: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }
    
    match cli.command {
        Commands::Init { chain_id, data_dir, allocations } => {
            init_genesis(chain_id, data_dir, allocations).await?;
        }
        
        Commands::Node { validator, bootnodes, listen, data_dir } => {
            run_node(validator, bootnodes, listen, data_dir).await?;
        }
        
        Commands::Wallet { cmd } => {
            handle_wallet(cmd).await?;
        }
        
        Commands::Tx { cmd } => {
            handle_transaction(cmd).await?;
        }
        
        Commands::Query { cmd } => {
            handle_query(cmd).await?;
        }
        
        Commands::Contract { cmd } => {
            handle_contract(cmd).await?;
        }
    }
    
    Ok(())
}

async fn init_genesis(
    chain_id: Option<u64>,
    data_dir: Option<PathBuf>,
    allocations: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = data_dir.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("home directory")
            .join(".subhost")
            .join("data")
    });
    
    std::fs::create_dir_all(&data_dir)?;
    
    let mut parsed_allocations = std::collections::HashMap::new();
    for allocation in allocations {
        let (address, balance) = allocation
            .split_once('=')
            .ok_or("allocation must use ADDRESS=BALANCE")?;
        let address = parse_address(address)?;
        let balance = balance.parse::<u128>()?;
        if parsed_allocations.insert(address, balance).is_some() {
            return Err("duplicate genesis allocation".into());
        }
    }

    let genesis = subhost_core::GenesisConfig {
        chain_id: chain_id.unwrap_or(1),
        initial_validators: vec![],
        allocations: parsed_allocations,
        block_time_ms: 1000,
        gas_limit: 30_000_000,
        genesis_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    };
    
    let genesis_path = data_dir.join("genesis.json");
    let genesis_json = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&genesis_path, genesis_json)?;

    tracing::info!("Genesis initialized at {}", genesis_path.display());

    // Core's GenesisConfig::validate() requires >= 1 validator. Writing an empty
    // validator set yields a genesis that validate()/load() will reject, so warn
    // honestly instead of claiming it is ready.
    if genesis.initial_validators.is_empty() {
        tracing::warn!(
            "Genesis has no initial validators and will be rejected by \
             GenesisConfig::validate(). Add validators before booting a real network."
        );
    }

    Ok(())
}

async fn run_node(
    validator: bool,
    bootnodes: Vec<String>,
    listen: String,
    data_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "Starting Subhost node (validator={}, bootnodes={:?}). Exposing JSON-RPC on {}",
        validator,
        bootnodes,
        listen
    );

    let data_dir = data_dir.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("home directory")
            .join(".subhost")
            .join("data")
    });
    let genesis = {
        let genesis_path = data_dir.join("genesis.json");
        if genesis_path.is_file() {
            let content = std::fs::read_to_string(genesis_path)?;
            Some(serde_json::from_str::<subhost_core::GenesisConfig>(&content)?)
        } else {
            None
        }
    };
    let chain_id = genesis.as_ref().map(|genesis| genesis.chain_id).unwrap_or(1);
    let addr: std::net::SocketAddr = listen.parse()?;
    let state = subhost_rpc::RpcState::with_data_dir(chain_id, &data_dir)?;
    if state.block_height() == 0 && !state.has_persisted_state() {
        if let Some(genesis) = &genesis {
            state.seed_accounts(
                genesis
                    .allocations
                    .iter()
                    .map(|(address, balance)| (*address, *balance)),
            )?;
        }
    }
    tracing::info!("Using chain {} and persistent data directory {}", chain_id, data_dir.display());
    let server = subhost_rpc::RpcServer::new(state);
    server.run(addr).await?;

    Ok(())
}

async fn handle_wallet(cmd: WalletCommands) -> Result<(), Box<dyn std::error::Error>> {
    let wallet_dir = dirs::home_dir()
        .expect("home directory")
        .join(".subhost")
        .join("wallets");
    
    std::fs::create_dir_all(&wallet_dir)?;
    
    match cmd {
        WalletCommands::New { password, name } => {
            let wallet = Wallet::new(&password)?;
            let name = name.unwrap_or_else(|| format!("wallet-{}", &wallet.address().to_string()[..8]));
            let path = wallet_dir.join(format!("{}.json", name));
            wallet.save(&path)?;
            tracing::info!("Created wallet: {} at {:?}", wallet.address(), path);
        }
        
        WalletCommands::List => {
            for entry in std::fs::read_dir(&wallet_dir)? {
                let entry = entry?;
                if entry.path().extension().map_or(false, |e| e == "json") {
                    let name = entry.file_name().to_string_lossy().to_string();
                    tracing::info!("Wallet: {}", name.replace(".json", ""));
                }
            }
        }
        
        WalletCommands::Show { address } => {
            tracing::info!("Wallet: {}", address);
        }
        
        WalletCommands::Export { address, password } => {
            // Real implementation: find the wallet file by address, decrypt the
            // private key with the password, and print it hex-encoded.
            let found = std::fs::read_dir(&wallet_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |x| x == "json"))
                .find(|e| {
                    std::fs::read_to_string(e.path())
                        .ok()
                        .and_then(|c| serde_json::from_str::<subhost_wallet::Wallet>(&c).ok())
                        .map_or(false, |w| w.address() == address)
                });

            match found {
                Some(entry) => {
                    let (_, key) = subhost_wallet::Wallet::load(&entry.path(), &password)?;
                    tracing::info!("Private key for {}: 0x{}", address, hex::encode(key.0));
                }
                None => {
                    tracing::error!("No wallet found for address {} in {:?}", address, wallet_dir);
                    std::process::exit(1);
                }
            }
        }
        
        WalletCommands::Import { private_key, password } => {
            let wallet = Wallet::from_private_key(&private_key, &password)?;
            let path = wallet_dir.join(format!("{}.json", &wallet.address().to_string()[..8]));
            wallet.save(&path)?;
            tracing::info!("Imported wallet: {} at {:?}", wallet.address(), path);
        }
    }
    
    Ok(())
}

async fn handle_transaction(cmd: TxCommands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        TxCommands::Send { from, to, amount, gas_price, data } => {
            let from_addr = parse_address(&from)?;
            let to_addr = parse_address(&to)?;
            let gas_price = gas_price.unwrap_or(1);
            let data = match data {
                Some(d) => subhost_core::hex::decode(d.trim_start_matches("0x"))?,
                None => Vec::new(),
            };

            let tx = subhost_core::Transaction {
                tx_type: subhost_core::TransactionType::Transfer,
                nonce: 0,
                from: from_addr,
                to: Some(to_addr),
                value: amount,
                gas_price,
                gas_limit: 21_000,
                data,
                chain_id: 1,
                signature: subhost_core::TransactionSignature { r: [0; 32], s: [0; 32], v: 0 },
            };

            // Compute the identical hash the RPC mempool would assign, so a
            // CLI-created tx can be looked up there.
            let mut pool = subhost_mempool::Mempool::default();
            let hash = pool.add(tx)?;
            tracing::info!(
                "Prepared transfer of {} from {} to {} (gas {}): {}",
                amount, from, to, gas_price, hash
            );
            tracing::info!("Note: broadcasting to a running node is not yet wired by the CLI.");
        }
        
        TxCommands::Status { tx_hash } => {
            tracing::info!("Checking status of {}", tx_hash);
        }
    }
    
    Ok(())
}

async fn handle_query(cmd: QueryCommands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        QueryCommands::Balance { address } => {
            let addr = parse_address(&address)?;
            // Don't fabricate a balance of 0: balances live in a running node's
            // RPC state, which the CLI isn't wired to yet.
            tracing::warn!(
                "Balance for {} requires a running node (JSON-RPC); the CLI does not query one yet.",
                addr
            );
        }
        
        QueryCommands::Block { height, hash } => {
            if let Some(h) = height {
                tracing::info!("Querying block at height: {}", h);
            } else if let Some(h) = hash {
                tracing::info!("Querying block with hash: {}", h);
            }
        }
        
        QueryCommands::Account { address } => {
            let addr = parse_address(&address)?;
            tracing::info!("Account info: {}", addr);
        }
        
        QueryCommands::Validators => {
            tracing::info!("Validator list");
        }
    }
    
    Ok(())
}

async fn handle_contract(cmd: ContractCommands) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ContractCommands::Deploy { from, bytecode, gas_limit } => {
            tracing::info!("Deploying contract from {} using {:?}", from, bytecode);
            let _ = gas_limit;
        }
        
        ContractCommands::Call { contract, data } => {
            tracing::info!("Calling contract {} with data {}", contract, data);
        }
    }
    
    Ok(())
}

fn parse_address(s: &str) -> Result<Address, Box<dyn std::error::Error>> {
    let s = s.trim_start_matches("0x");
    let bytes = subhost_core::hex::decode(s)?;
    if bytes.len() != 20 {
        return Err("Invalid address length".into());
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(Address::new(addr))
}

mod dirs {
    pub fn home_dir() -> Option<std::path::PathBuf> {
        std::env::var("HOME").ok().map(std::path::PathBuf::from)
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
        assert!(Cli::try_parse_from([
            "subhost",
            "init",
            "--chain-id",
            "1",
            "--data-dir",
            "./data",
            "--alloc",
            "0x1111111111111111111111111111111111111111=1000000",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "subhost",
            "query",
            "block",
            "--height",
            "12345",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "subhost",
            "wallet",
            "import",
            "--private-key",
            "00",
            "--password",
            "test",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "subhost",
            "node",
            "--listen",
            "127.0.0.1:8545",
            "--data-dir",
            "./data",
        ])
        .is_ok());
    }
}
