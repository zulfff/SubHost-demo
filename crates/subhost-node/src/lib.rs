//! Node bootstrap: genesis, ledger, JSON-RPC, and metrics in one place.
//!
//! This is the single place that decides how a running node is assembled, so the
//! CLI, tests, and container entrypoint cannot drift apart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use subhost_core::{ChainId, GenesisConfig};
use subhost_metrics::{Metrics, MetricsConfig};
use subhost_rpc::{RpcServer, RpcServerConfig, RpcState};
use subhost_storage::LedgerStore;
use tracing::{info, warn};

/// Genesis file name inside the data directory.
pub const GENESIS_FILE_NAME: &str = "genesis.json";

/// Everything needed to start a node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Directory holding `genesis.json` and the ledger file.
    pub data_dir: PathBuf,
    /// JSON-RPC listen address.
    pub rpc_addr: SocketAddr,
    /// Maximum concurrent JSON-RPC connections.
    pub max_rpc_connections: u32,
    /// Optional Prometheus exporter address.
    pub metrics_addr: Option<SocketAddr>,
    /// Whether this node intends to validate, which requires a validator set.
    pub validator: bool,
    /// Chain ID used when the data directory has no genesis file.
    pub fallback_chain_id: ChainId,
}

impl NodeConfig {
    /// A loopback-only configuration rooted at `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            rpc_addr: SocketAddr::from(([127, 0, 0, 1], 8545)),
            max_rpc_connections: 1_000,
            metrics_addr: None,
            validator: false,
            fallback_chain_id: 1,
        }
    }

    pub fn genesis_path(&self) -> PathBuf {
        self.data_dir.join(GENESIS_FILE_NAME)
    }
}

/// A booted node: ledger restored, genesis applied, ready to serve.
pub struct Node {
    state: RpcState,
    metrics: Metrics,
    config: NodeConfig,
    genesis: Option<GenesisConfig>,
}

impl Node {
    /// Load genesis, restore or create the ledger, and apply allocations once.
    ///
    /// Genesis allocations are applied only when no ledger exists yet, so a
    /// restart can never re-mint balances over a live chain.
    pub fn bootstrap(config: NodeConfig) -> Result<Self, NodeError> {
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|source| NodeError::DataDir { path: config.data_dir.clone(), source })?;

        let genesis = load_genesis(&config.genesis_path())?;
        if config.validator {
            // Fail fast rather than starting a validator that can never form a quorum.
            if let Some(genesis) = &genesis {
                genesis.requires_validators()?;
            } else {
                return Err(NodeError::MissingGenesis(config.genesis_path()));
            }
        }

        let chain_id =
            genesis.as_ref().map_or(config.fallback_chain_id, |genesis| genesis.chain_id);
        if chain_id == 0 {
            return Err(NodeError::InvalidChainId);
        }

        let store = LedgerStore::open(chain_id, &config.data_dir)?;
        let fresh = !store.exists();
        let state = RpcState::with_store(store)?;

        if fresh {
            if let Some(genesis) = &genesis {
                if !genesis.allocations.is_empty() {
                    state.seed_accounts(
                        genesis.allocations.iter().map(|(address, balance)| (*address, *balance)),
                    )?;
                    info!(accounts = genesis.allocations.len(), "applied genesis allocations");
                }
            }
        } else {
            info!(height = state.block_height(), "restored existing ledger");
        }

        if genesis.is_none() {
            warn!(
                path = %config.genesis_path().display(),
                chain_id,
                "no genesis file found; starting with an empty allocation set"
            );
        }

        let metrics = Metrics::new()?;
        metrics.set_block_height(state.block_height());
        metrics.set_pending_transactions(state.pending_count());

        Ok(Self { state, metrics, config, genesis })
    }

    pub fn state(&self) -> &RpcState {
        &self.state
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn genesis(&self) -> Option<&GenesisConfig> {
        self.genesis.as_ref()
    }

    pub fn chain_id(&self) -> ChainId {
        self.state.chain_id()
    }

    /// Serve JSON-RPC, and the metrics exporter when configured, until either
    /// stops or the process receives a shutdown signal.
    pub async fn run(self) -> Result<(), NodeError> {
        let rpc_config = RpcServerConfig {
            listen_addr: self.config.rpc_addr,
            max_connections: self.config.max_rpc_connections,
        };
        info!(
            chain_id = self.chain_id(),
            height = self.state.block_height(),
            data_dir = %self.config.data_dir.display(),
            validator = self.config.validator,
            "starting node"
        );

        let metrics_task = self.config.metrics_addr.map(|addr| {
            let metrics = self.metrics.clone();
            tokio::spawn(async move { metrics.serve(MetricsConfig { listen_addr: addr }).await })
        });

        // Keep metrics in step with the ledger while the node serves.
        let observer = {
            let metrics = self.metrics.clone();
            let state = self.state.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    ticker.tick().await;
                    metrics.set_block_height(state.block_height());
                    metrics.set_pending_transactions(state.pending_count());
                }
            })
        };

        let rpc = RpcServer::new(self.state.clone());
        let result = tokio::select! {
            result = rpc.run(rpc_config) => result.map_err(NodeError::from),
            result = shutdown_signal() => result,
        };

        observer.abort();
        if let Some(task) = metrics_task {
            task.abort();
        }
        result
    }
}

/// Read `genesis.json` when present, validating it before returning it.
fn load_genesis(path: &Path) -> Result<Option<GenesisConfig>, NodeError> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(GenesisConfig::load(path)?))
}

/// Resolve when the process is asked to terminate.
async fn shutdown_signal() -> Result<(), NodeError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).map_err(NodeError::ShutdownSignal)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(NodeError::ShutdownSignal)?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(NodeError::ShutdownSignal)?;
    }
    info!("shutdown signal received; stopping node");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("cannot create data directory {path}: {source}")]
    DataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("a validator node requires a genesis file at {0}")]
    MissingGenesis(PathBuf),

    #[error("chain ID cannot be zero")]
    InvalidChainId,

    #[error(transparent)]
    Genesis(#[from] subhost_core::CoreError),

    #[error(transparent)]
    Storage(#[from] subhost_storage::StorageError),

    #[error(transparent)]
    Rpc(#[from] subhost_rpc::RpcError),

    #[error(transparent)]
    RpcServe(#[from] subhost_rpc::RpcServeError),

    #[error(transparent)]
    Metrics(#[from] subhost_metrics::MetricsError),

    #[error("cannot install shutdown signal handler: {0}")]
    ShutdownSignal(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_core::{Address, ValidatorInfo};

    fn genesis_with(chain_id: ChainId, allocation: Option<(Address, u128)>) -> GenesisConfig {
        let mut genesis = GenesisConfig { chain_id, ..Default::default() };
        if let Some((address, balance)) = allocation {
            genesis.allocations.insert(address, balance);
        }
        genesis
    }

    #[test]
    fn bootstrap_creates_the_data_directory_and_defaults_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("nested").join("data");
        let node = Node::bootstrap(NodeConfig::new(&data_dir)).unwrap();

        assert!(data_dir.is_dir(), "the data directory must be created");
        assert_eq!(node.chain_id(), 1);
        assert_eq!(node.state().block_height(), 0);
        assert!(node.genesis().is_none());
    }

    #[test]
    fn bootstrap_applies_genesis_allocations_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let address = Address::new([1; 20]);
        genesis_with(7, Some((address, 500))).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();

        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        assert_eq!(node.chain_id(), 7, "genesis chain ID must win");
        assert_eq!(node.state().balance(&address), 500);
        drop(node);

        // A restart must not re-apply allocations over the restored ledger.
        let restarted = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        assert_eq!(restarted.state().balance(&address), 500);
        assert!(restarted.state().has_persisted_state());
    }

    #[test]
    fn bootstrap_rejects_an_invalid_genesis_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(GENESIS_FILE_NAME), "{ not json").unwrap();
        assert!(matches!(Node::bootstrap(NodeConfig::new(dir.path())), Err(NodeError::Genesis(_))));
    }

    #[test]
    fn validator_mode_requires_a_genesis_validator_set() {
        let dir = tempfile::tempdir().unwrap();
        let config = NodeConfig { validator: true, ..NodeConfig::new(dir.path()) };

        // No genesis at all.
        assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::MissingGenesis(_))));

        // Genesis with an empty validator set.
        genesis_with(1, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        assert!(matches!(Node::bootstrap(config.clone()), Err(NodeError::Genesis(_))));

        // Genesis with a real validator boots.
        let mut genesis = genesis_with(1, None);
        genesis.initial_validators = vec![ValidatorInfo {
            address: Address::new([9; 20]),
            public_key: vec![1, 2, 3],
            power: 100,
        }];
        genesis.save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let node = Node::bootstrap(config).unwrap();
        assert_eq!(node.genesis().unwrap().total_voting_power(), 100);
    }

    #[test]
    fn a_zero_fallback_chain_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Node::bootstrap(NodeConfig { fallback_chain_id: 0, ..NodeConfig::new(dir.path()) }),
            Err(NodeError::InvalidChainId)
        ));
    }

    #[test]
    fn a_ledger_from_another_chain_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        genesis_with(7, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        node.state().seed_accounts([(Address::new([1; 20]), 1)]).unwrap();
        drop(node);

        // Rewrite genesis to a different chain over the same ledger.
        genesis_with(8, None).save(&dir.path().join(GENESIS_FILE_NAME)).unwrap();
        assert!(matches!(Node::bootstrap(NodeConfig::new(dir.path())), Err(NodeError::Storage(_))));
    }

    #[test]
    fn metrics_start_in_step_with_the_restored_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::bootstrap(NodeConfig::new(dir.path())).unwrap();
        let rendered = String::from_utf8(node.metrics().encode().unwrap()).unwrap();
        assert!(rendered.contains("subhost_block_height 0"));
        assert!(rendered.contains("subhost_pending_transactions 0"));
    }

    #[test]
    fn config_exposes_the_genesis_path_under_the_data_dir() {
        let config = NodeConfig::new("/tmp/subhost-node-test");
        assert_eq!(
            config.genesis_path(),
            PathBuf::from("/tmp/subhost-node-test").join(GENESIS_FILE_NAME)
        );
        assert!(config.metrics_addr.is_none());
        assert!(!config.validator);
    }
}
