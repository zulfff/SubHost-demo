use serde::{Serialize, Deserialize};
use tracing::{info, debug};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostnodeConfig {
    pub enabled: bool,
    pub max_connections: usize,
}

impl Default for SubhostnodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
        }
    }
}

pub struct SubhostnodeModule {
    config: SubhostnodeConfig,
    metrics: Metrics,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: u64,
    pub errors: u64,
    pub latency_ms: u64,
}

impl SubhostnodeModule {
    pub fn new(config: SubhostnodeConfig) -> Self {
        info!("Initializing SubhostnodeModule");
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
        debug!("Processing request in subhost-node");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostnodeError {
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
        let config = SubhostnodeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = SubhostnodeModule::new(SubhostnodeConfig::default());
        assert!(module.config.enabled);
    }
}
