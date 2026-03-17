//! Omnichain Node - Main entry point
//!
//! # Architecture
//! The node runs multiple subsystems concurrently:
//! - Networking (libp2p with Dandelion++ privacy)
//! - Consensus (DAG + HotStuff hybrid)
//! - Execution (Parallel EVM + WASM)
//! - State (Merkle Patricia Trie on RocksDB)
//! - ZK (Shielded transactions, MEV resistance)
//! - Governance (Quadratic voting)
//! - IBC (Cross-chain communication)

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, error};

/// CLI arguments
#[derive(Parser)]
#[command(name = "omnichain")]
#[command(about = "Omnichain - Next-generation Layer-1 blockchain")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    
    /// Data directory
    #[arg(short, long, default_value = "./data")]
    data_dir: String,
    
    /// Listen address
    #[arg(short, long, default_value = "/ip4/0.0.0.0/tcp/30333")]
    listen_addr: String,
    
    /// Bootstrap peers
    #[arg(long)]
    bootstrap: Vec<String>,
    
    /// Enable validator mode
    #[arg(long)]
    validator: bool,
    
    /// Chain ID
    #[arg(long, default_value = "1")]
    chain_id: u64,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the node
    Run,
    
    /// Initialize genesis state
    Init {
        /// Genesis file
        #[arg(short, long)]
        genesis: Option<String>,
    },
    
    /// Show node status
    Status,
}

/// Node runtime
struct NodeRuntime {
    /// Network manager
    network: Arc<omnichain_network::NetworkManager>,
    /// Consensus engine
    consensus: Arc<omnichain_consensus::ConsensusEngine>,
    /// Execution engine
    execution: Arc<omnichain_evm::ExecutionEngine>,
    /// State database
    state: Arc<RwLock<omnichain_state::StateDB>>,
    /// ZK system
    zk: Arc<omnichain_zk::ZKSystem>,
    /// Governance
    governance: Arc<omnichain_governance::Governance>,
    /// IBC module
    ibc: Arc<omnichain_ibc::IBCModule>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run => run_node(cli).await,
        Commands::Init { genesis } => init_genesis(cli, genesis).await,
        Commands::Status => show_status().await,
    }
}

async fn run_node(cli: Cli) -> anyhow::Result<()> {
    info!("Starting Omnichain node");
    info!("Chain ID: {}", cli.chain_id);
    info!("Data directory: {}", cli.data_dir);
    info!("Validator mode: {}", cli.validator);

    // Initialize state database
    let state_config = omnichain_state::StateConfig {
        data_dir: format!("{}/state", cli.data_dir),
        ..Default::default()
    };
    
    let state = Arc::new(RwLock::new(
        omnichain_state::StateDB::open(&state_config)?
    ));

    // Initialize network
    let network_config = omnichain_network::NetworkConfig {
        listen_addr: cli.listen_addr,
        bootstrap_peers: cli.bootstrap,
        max_peers: 50,
        enable_mdns: true,
        dandelion_enabled: true,
    };

    let (network_tx, network_rx) = mpsc::channel(1000);
    let (dag_tx, dag_rx) = mpsc::channel(1000);
    
    let (network_manager, _network_handle) = omnichain_network::NetworkManager::new(
        network_config,
        network_tx.clone(),
        dag_tx,
    ).await?;

    let network = Arc::new(network_manager);

    // Initialize consensus if validator
    let consensus = if cli.validator {
        // Generate validator keys
        let (sk, _pk) = omnichain_crypto::BLSScheme::keygen();
        let addr = omnichain_core::Address::from([1u8; 20]);
        
        let consensus_config = omnichain_consensus::ConsensusConfig::new(4);
        let (finalized_tx, _finalized_rx) = mpsc::channel(100);
        
        let consensus = Arc::new(omnichain_consensus::ConsensusEngine::new(
            consensus_config,
            sk,
            addr,
            finalized_tx,
            vec![addr], // Single validator for now
        ));
        
        // Spawn consensus task
        let consensus_clone = consensus.clone();
        tokio::spawn(async move {
            consensus_clone.run().await;
        });
        
        Some(consensus)
    } else {
        None
    };

    // Initialize execution engine
    let execution_config = omnichain_evm::ExecutionConfig::default();
    let execution = Arc::new(omnichain_evm::ExecutionEngine::new(
        execution_config,
        state.clone(),
    ));

    // Initialize ZK system
    let zk = Arc::new(omnichain_zk::ZKSystem);

    // Initialize governance
    let gov_config = omnichain_governance::GovernanceConfig::default();
    let governance = Arc::new(omnichain_governance::Governance::new(
        gov_config,
        state.clone(),
    ));

    // Initialize IBC
    let ibc_config = omnichain_ibc::IBCConfig::default();
    let ibc = Arc::new(omnichain_ibc::IBCModule::new(ibc_config));

    // Build runtime
    let _runtime = NodeRuntime {
        network,
        consensus: consensus.unwrap_or_else(|| {
            // Dummy consensus for non-validator
            let (sk, _pk) = omnichain_crypto::BLSScheme::keygen();
            let (finalized_tx, _finalized_rx) = mpsc::channel(100);
            Arc::new(omnichain_consensus::ConsensusEngine::new(
                omnichain_consensus::ConsensusConfig::new(1),
                sk,
                omnichain_core::Address::from([0u8; 20]),
                finalized_tx,
                vec![],
            ))
        }),
        execution,
        state,
        zk,
        governance,
        ibc,
    };

    info!("Node started successfully");
    info!("Press Ctrl+C to shutdown");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}

async fn init_genesis(cli: Cli, _genesis: Option<String>) -> anyhow::Result<()> {
    info!("Initializing genesis state");

    let state_config = omnichain_state::StateConfig {
        data_dir: format!("{}/state", cli.data_dir),
        ..Default::default()
    };

    let state = omnichain_state::StateDB::open(&state_config)?;

    // Create genesis accounts
    // In production: load from genesis file
    let genesis_accounts = vec![
        (omnichain_core::Address::from([0u8; 20]), 1_000_000_000u128),
    ];

    for (addr, balance) in genesis_accounts {
        let account = omnichain_state::Account {
            balance,
            nonce: 0,
            code_hash: None,
            storage_root: omnichain_core::Hash::ZERO,
            rent_epoch: 0,
        };
        state.set_account(addr, account)?;
        info!("Created genesis account: {} with balance {}", addr, balance);
    }

    // Commit genesis state
    let root = state.commit()?;
    info!("Genesis state root: {}", root);

    Ok(())
}

async fn show_status() -> anyhow::Result<()> {
    info!("Omnichain Node Status");
    info!("====================");
    
    // In production: query actual node state
    info!("Version: 0.1.0");
    info!("Status: Running");
    info!("Connections: 0");
    info!("Pending transactions: 0");
    info!("Latest block: 0");
    
    Ok(())
}
