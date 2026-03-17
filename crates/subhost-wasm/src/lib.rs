use serde::{Serialize, Deserialize};
use tracing::{info, debug};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostwasmConfig {
    pub enabled: bool,
    pub max_connections: usize,
}

impl Default for SubhostwasmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
        }
    }
}

pub struct SubhostwasmModule {
    config: SubhostwasmConfig,
    metrics: Metrics,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

impl SubhostwasmModule {
    pub fn new(config: SubhostwasmConfig) -> Self {
        info!("Initializing SubhostwasmModule");
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
pub enum SubhostwasmError {
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
        let config = SubhostwasmConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = SubhostwasmModule::new(SubhostwasmConfig::default());
        assert!(module.config.enabled);
    }
}
