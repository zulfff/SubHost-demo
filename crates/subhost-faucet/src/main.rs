//! Faucet binary entrypoint.
//!
//! Configuration comes from the environment so the same image works in a
//! container and on a workstation. The wallet password is read from
//! `SUBHOST_FAUCET_PASSWORD` and never appears in a log line or a response.

use std::path::PathBuf;
use subhost_faucet::{FaucetConfig, FaucetSigner, FaucetState};
use subhost_telemetry::{TelemetryConfig, Verbosity};

fn main() -> anyhow::Result<()> {
    subhost_telemetry::init_or_warn(TelemetryConfig::from_env(Verbosity::Normal));

    let config = config_from_env()?;
    let signer = FaucetSigner::from_env(&config.wallet_path)?;
    let state = FaucetState::new(config, signer)?;

    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(state.serve())?;
    Ok(())
}

/// Build the faucet configuration from the environment, failing on a malformed
/// value rather than silently falling back to a default.
fn config_from_env() -> anyhow::Result<FaucetConfig> {
    let defaults = FaucetConfig::default();
    Ok(FaucetConfig {
        listen_addr: parse_env("FAUCET_LISTEN_ADDR", defaults.listen_addr)?,
        rpc_url: std::env::var("SUBHOST_RPC_URL").unwrap_or(defaults.rpc_url),
        wallet_path: std::env::var("FAUCET_WALLET_PATH")
            .map(PathBuf::from)
            .unwrap_or(defaults.wallet_path),
        drip_amount: parse_env("FAUCET_DRIP_AMOUNT", defaults.drip_amount)?,
        cooldown_secs: parse_env("FAUCET_COOLDOWN_SECS", defaults.cooldown_secs)?,
    })
}

fn parse_env<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(value) => value.parse().map_err(|error| anyhow::anyhow!("{key} is invalid: {error}")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn parse_env_falls_back_and_rejects_malformed_values() {
        let key = "SUBHOST_FAUCET_TEST_PORT";
        std::env::remove_var(key);
        assert_eq!(parse_env::<u64>(key, 7).unwrap(), 7);

        std::env::set_var(key, "42");
        assert_eq!(parse_env::<u64>(key, 7).unwrap(), 42);

        std::env::set_var(key, "not-a-number");
        assert!(parse_env::<u64>(key, 7).is_err());
        std::env::remove_var(key);
    }

    #[test]
    fn config_from_env_uses_defaults_when_unset() {
        for key in [
            "FAUCET_LISTEN_ADDR",
            "SUBHOST_RPC_URL",
            "FAUCET_WALLET_PATH",
            "FAUCET_DRIP_AMOUNT",
            "FAUCET_COOLDOWN_SECS",
        ] {
            std::env::remove_var(key);
        }
        assert_eq!(config_from_env().unwrap(), FaucetConfig::default());

        std::env::set_var("FAUCET_LISTEN_ADDR", "127.0.0.1:9999");
        assert_eq!(
            config_from_env().unwrap().listen_addr,
            SocketAddr::from(([127, 0, 0, 1], 9999))
        );
        std::env::set_var("FAUCET_LISTEN_ADDR", "nonsense");
        assert!(config_from_env().is_err());
        std::env::remove_var("FAUCET_LISTEN_ADDR");
    }
}
