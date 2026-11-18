//! Persist verified block/QC history and automatically replay it over existing P2P.
#[path = "sync_relay/history.rs"]
mod history;

use anyhow::{bail, Context, Result};
use clap::Parser;
use history::{History, IngestError};
use std::path::PathBuf;
use std::time::Duration;
use subhost_core::GenesisConfig;
use subhost_network::{NetworkConfig, NetworkManager};
use subhost_telemetry::{TelemetryConfig, Verbosity};

#[derive(Parser)]
struct Options {
    #[arg(long)]
    genesis: PathBuf,
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    p2p_listen: String,
    #[arg(long = "bootstrap-peer", required = true)]
    bootstrap_peers: Vec<String>,
    #[arg(long, default_value_t = 50)]
    replay_interval_ms: u64,
    #[arg(long, default_value_t = 67_108_864)]
    max_history_bytes: u64,
}

async fn relay(
    mut history: History,
    config: NetworkConfig,
    interval: Duration,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let (manager, mut handle) = NetworkManager::new(config)?;
    let mut network = tokio::spawn(manager.run());
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cursor = 0;
    tokio::pin!(shutdown);
    tracing::info!(height = history.height(), state_root = %history.state_root(), "certified-history relay started");
    let result = loop {
        tokio::select! {
            message = handle.inbound.recv() => {
                let Some(message) = message else { break Err(anyhow::anyhow!("history relay network channel closed")); };
                match history.ingest(message) {
                    Ok(count) if count > 0 => tracing::info!(height = history.height(), count, "certified history advanced"),
                    Ok(_) => {},
                    Err(IngestError::Invalid(error)) => tracing::debug!(%error, "rejected invalid history message"),
                    Err(IngestError::Storage(error)) => break Err(error.context("certified history persistence failed")),
                }
            }
            _ = ticker.tick(), if history.len() > 0 => {
                cursor %= history.len();
                if let Some(message) = history.message(cursor) {
                    match handle.outbound.try_send(message.clone()) {
                        Ok(()) => cursor += 1,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {},
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break Err(anyhow::anyhow!("history relay stopped accepting messages")),
                    }
                }
            }
            result = &mut network => {
                break result.context("history relay network task failed")?.map_err(Into::into);
            }
            _ = &mut shutdown => break Ok(()),
        }
    };
    network.abort();
    if !network.is_finished() {
        let _ = network.await;
    }
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    if options.replay_interval_ms == 0 || options.max_history_bytes == 0 {
        bail!("replay interval and history byte limit must be positive");
    }
    subhost_telemetry::init(TelemetryConfig::from_env(Verbosity::Normal))?;
    let genesis = GenesisConfig::load(&options.genesis)?;
    let history = History::open(&options.data_dir, &genesis, options.max_history_bytes)?;
    relay(
        history,
        NetworkConfig {
            listen_addr: options.p2p_listen,
            bootstrap_peers: options.bootstrap_peers,
            enable_mdns: false,
            ..Default::default()
        },
        Duration::from_millis(options.replay_interval_ms),
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
    )
    .await
}

#[cfg(test)]
#[path = "sync_relay/tests.rs"]
mod tests;
