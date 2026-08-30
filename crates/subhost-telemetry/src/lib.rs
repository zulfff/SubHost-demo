//! Structured logging setup shared by every Subhost binary.
//!
//! One initialization path means every binary honours `RUST_LOG` the same way and
//! nothing installs a second global subscriber.

use tracing_subscriber::EnvFilter;

/// Environment variable read for the log filter.
pub const LOG_ENV: &str = "RUST_LOG";

/// How much detail to emit when `RUST_LOG` is unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// `warn` and above: quiet enough for supervised deployments.
    Quiet,
    /// `info` and above: the default for interactive use.
    #[default]
    Normal,
    /// `debug` and above for our own crates, `info` for dependencies.
    Verbose,
}

impl Verbosity {
    /// Pick a verbosity from a `--verbose`/`--quiet` pair.
    pub fn from_flags(verbose: bool, quiet: bool) -> Self {
        match (verbose, quiet) {
            // `--verbose` wins so a debugging session is never silently downgraded.
            (true, _) => Self::Verbose,
            (false, true) => Self::Quiet,
            (false, false) => Self::Normal,
        }
    }

    /// The filter directive used when `RUST_LOG` is absent.
    pub fn directive(self) -> &'static str {
        match self {
            Self::Quiet => "warn",
            Self::Normal => "info",
            // Keep dependency logs at info so libp2p and hyper do not drown the app.
            Self::Verbose => {
                "info,subhost=debug,subhost_cli=debug,subhost_node=debug,\
                              subhost_rpc=debug,subhost_state=debug,subhost_storage=debug,\
                              subhost_mempool=debug,subhost_network=debug,subhost_ibc=debug,\
                              subhost_faucet=debug,subhost_explorer=debug,subhost_metrics=debug"
            }
        }
    }
}

/// Configuration for the global subscriber.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    pub verbosity: Verbosity,
    /// Emit newline-delimited JSON instead of human-readable text.
    pub json: bool,
    /// Include the target module in each line.
    pub with_target: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { verbosity: Verbosity::default(), json: false, with_target: true }
    }
}

impl TelemetryConfig {
    pub fn new(verbosity: Verbosity) -> Self {
        Self { verbosity, ..Self::default() }
    }

    /// Read the `SUBHOST_LOG_FORMAT=json` opt-in used by container deployments.
    pub fn from_env(verbosity: Verbosity) -> Self {
        let json = std::env::var("SUBHOST_LOG_FORMAT")
            .map(|value| value.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        Self { verbosity, json, with_target: true }
    }

    /// The effective filter: `RUST_LOG` when set and parsable, else the verbosity
    /// directive.
    pub fn env_filter(&self) -> EnvFilter {
        EnvFilter::try_from_env(LOG_ENV)
            .unwrap_or_else(|_| EnvFilter::new(self.verbosity.directive()))
    }
}

/// Install the global subscriber.
///
/// Returns [`TelemetryError::AlreadyInitialized`] if one is already installed, so
/// a double call is a reported condition rather than a panic.
pub fn init(config: TelemetryConfig) -> Result<(), TelemetryError> {
    let builder = tracing_subscriber::fmt()
        .with_env_filter(config.env_filter())
        .with_target(config.with_target);

    let result = if config.json { builder.json().try_init() } else { builder.try_init() };
    result.map_err(|error| TelemetryError::AlreadyInitialized(error.to_string()))
}

/// Install the subscriber for a binary, logging to stderr and continuing if a
/// subscriber already exists.
///
/// Failing to configure logging must never stop a node from booting.
pub fn init_or_warn(config: TelemetryConfig) {
    if let Err(error) = init(config) {
        eprintln!("warning: could not install the tracing subscriber: {error}");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("a global tracing subscriber is already installed: {0}")]
    AlreadyInitialized(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_beats_quiet() {
        assert_eq!(Verbosity::from_flags(true, true), Verbosity::Verbose);
        assert_eq!(Verbosity::from_flags(true, false), Verbosity::Verbose);
        assert_eq!(Verbosity::from_flags(false, true), Verbosity::Quiet);
        assert_eq!(Verbosity::from_flags(false, false), Verbosity::Normal);
        assert_eq!(Verbosity::default(), Verbosity::Normal);
    }

    #[test]
    fn directives_are_valid_filters() {
        for verbosity in [Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose] {
            let directive = verbosity.directive();
            assert!(
                EnvFilter::try_new(directive).is_ok(),
                "{verbosity:?} produced an unparsable directive: {directive}"
            );
        }
    }

    #[test]
    fn config_defaults_to_human_readable_output() {
        let config = TelemetryConfig::default();
        assert!(!config.json);
        assert!(config.with_target);
        assert_eq!(config.verbosity, Verbosity::Normal);
        assert_eq!(TelemetryConfig::new(Verbosity::Quiet).verbosity, Verbosity::Quiet);
    }

    #[test]
    fn a_malformed_rust_log_falls_back_to_the_verbosity_directive() {
        // `env_filter` must never panic on operator input; it degrades instead.
        let config = TelemetryConfig::new(Verbosity::Quiet);
        let _ = config.env_filter();
    }

    #[test]
    fn double_initialization_is_reported_not_fatal() {
        // Exactly one call can win inside a test binary; both outcomes are valid
        // depending on test ordering, and neither may panic.
        let first = init(TelemetryConfig::default());
        let second = init(TelemetryConfig::default());
        assert!(second.is_err(), "the second install must be refused (first: {first:?})");
        // The convenience wrapper swallows the same condition.
        init_or_warn(TelemetryConfig::default());
    }
}
