use serde::{Serialize, Deserialize};
use tracing::{info, debug};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostrpcConfig {
    pub enabled: bool,
    pub max_connections: usize,
}

impl Default for SubhostrpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
        }
    }
}

pub struct SubhostrpcModule {
    config: SubhostrpcConfig,
    metrics: Metrics,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

impl SubhostrpcModule {
    pub fn new(config: SubhostrpcConfig) -> Self {
        info!("Initializing SubhostrpcModule");
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
        debug!("Processing request in subhost-rpc");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostrpcError {
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
        let config = SubhostrpcConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = SubhostrpcModule::new(SubhostrpcConfig::default());
        assert!(module.config.enabled);
    }
}
