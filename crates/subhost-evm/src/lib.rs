use serde::{Serialize, Deserialize};
use tracing::{info, debug};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostevmConfig {
    pub enabled: bool,
    pub max_connections: usize,
}

impl Default for SubhostevmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
        }
    }
}

pub struct SubhostevmModule {
    config: SubhostevmConfig,
    metrics: Metrics,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

impl SubhostevmModule {
    pub fn new(config: SubhostevmConfig) -> Self {
        info!("Initializing SubhostevmModule");
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
        debug!("Processing request in subhost-evm");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostevmError {
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
        let config = SubhostevmConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = SubhostevmModule::new(SubhostevmConfig::default());
        assert!(module.config.enabled);
    }
}
