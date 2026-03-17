use clap::{Parser, Subcommand};
use std::path::PathBuf;
use subhost_core::{Address, Hash};
use subhost_wallet::{Wallet, WalletError};

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
        #[arg(short, long)]
        chain_id: Option<u64>,
        
        #[arg(short, long)]
        data_dir: Option<PathBuf>,
    },
    
    Node {
        #[arg(short, long)]
        validator: bool,
        
        #[arg(short, long)]
        bootnodes: Vec<String>,
        
        #[arg(short, long, default_value = "0.0.0.0:30333")]
        listen: String,
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
        #[arg(short, long)]
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
        #[arg(short, long)]
        height: Option<u64>,
        
        #[arg(short, long)]
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
        Commands::Init { chain_id, data_dir } => {
            init_genesis(chain_id, data_dir).await?;
        }
        
        Commands::Node { validator, bootnodes, listen } => {
            run_node(validator, bootnodes, listen).await?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = data_dir.unwrap_or_else(|| {
        dirs::home_dir()
            .expect("home directory")
            .join(".subhost")
            .join("data")
    });
    
    std::fs::create_dir_all(&data_dir)?;
    
    let genesis = subhost_core::GenesisConfig {
        chain_id: chain_id.unwrap_or(1),
        initial_validators: vec![],
        allocations: std::collections::HashMap::new(),
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
    
    Ok(())
}

async fn run_node(
    _validator: bool,
    _bootnodes: Vec<String>,
    _listen: String,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Starting Subhost node...");
    
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
            tracing::info!("Exporting wallet: {}", address);
            let _ = password;
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
            let _to = parse_address(&to)?;
            tracing::info!("Sending {} from {} to {}", amount, from, to);
            let _ = gas_price;
            let _ = data;
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
            tracing::info!("Balance of {}: 0", addr);
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
