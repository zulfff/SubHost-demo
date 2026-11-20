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
    /// Export tracing spans over OTLP HTTP/protobuf to this collector endpoint,
    /// including the signal path (`/v1/traces`). Unset or empty keeps export off.
    #[arg(long, env = "SUBHOST_OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,
}

/// Normalize a configured OTLP endpoint: trimmed, and an empty value means
/// export stays off. The endpoint is used verbatim by the exporter, so it must
/// include the signal path (`/v1/traces`).
fn normalize_otlp_endpoint(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// Install relay telemetry. With an OTLP endpoint configured, spans export
/// through the provider and the returned shutdown guard must be retained for
/// the process lifetime (drop flushes). Construction or install failure is
/// reported and falls back to plain logging, never fatal to boot (Q-035).
/// Must be called inside a tokio runtime: the OTLP batch processor needs one.
fn init_telemetry(
    otlp_endpoint: Option<String>,
    verbosity: Verbosity,
) -> Option<subhost_telemetry::otlp::OtlpGuard> {
    match normalize_otlp_endpoint(otlp_endpoint) {
        Some(endpoint) => {
            let otlp = subhost_telemetry::otlp::OtlpConfig {
                endpoint,
                ..subhost_telemetry::otlp::OtlpConfig::default()
            };
            match subhost_telemetry::otlp::init_with_otlp(
                TelemetryConfig::from_env(verbosity),
                otlp,
            ) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    eprintln!("warning: OTLP export disabled: {error}");
                    subhost_telemetry::init_or_warn(TelemetryConfig::from_env(verbosity));
                    None
                }
            }
        }
        None => {
            subhost_telemetry::init_or_warn(TelemetryConfig::from_env(verbosity));
            None
        }
    }
}

struct AbortNetworkOnDrop(tokio::task::AbortHandle);

impl Drop for AbortNetworkOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn relay(
    mut history: History,
    config: NetworkConfig,
    interval: Duration,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let (manager, mut handle) = NetworkManager::new(config)?;
    let mut network = tokio::spawn(manager.run());
    let _network_cleanup = AbortNetworkOnDrop(network.abort_handle());
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
                if handle.subscribed_peer_count() == 0 {
                    continue;
                }
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
    // Retained for the process lifetime; dropping it flushes pending spans.
    // Installed inside the runtime: the OTLP batch processor needs a tokio
    // context (Q-035).
    let _otlp_guard = init_telemetry(options.otlp_endpoint, Verbosity::Normal);
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
