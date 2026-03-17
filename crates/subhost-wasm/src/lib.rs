use serde::{Serialize, Deserialize};
use tracing::{info, debug, warn, error};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct subhostwasmConfig {
    pub enabled: bool,
    pub max_connections: usize,
}

impl Default for subhostwasmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
        }
    }
}

pub struct subhostwasmModule {
    config: subhostwasmConfig,
    metrics: Metrics,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

impl subhostwasmModule {
    pub fn new(config: subhostwasmConfig) -> Self {
        info!("Initializing subhostwasmModule");
        Self {
            config,
            metrics: Metrics::default(),
        }
    }
    
    pub fn process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.config.enabled {
            return Ok(());
        }
        self.metrics.requests += 1;
        debug!("Processing request in subhost-wasm");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum subhostwasmError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Processing error: {0}")]
    Processing(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = subhostwasmConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = subhostwasmModule::new(subhostwasmConfig::default());
        assert!(module.config.enabled);
    }
}
