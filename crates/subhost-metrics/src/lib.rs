use serde::{Serialize, Deserialize};
use tracing::{info, debug};
use std::net::SocketAddr;
use std::sync::Arc;
use prometheus::{Registry, Counter, Gauge, Histogram, Encoder, TextEncoder};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubhostmetricsConfig {
    pub enabled: bool,
    pub max_connections: usize,
    pub listen_addr: String,
}

impl Default for SubhostmetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 1000,
            listen_addr: "127.0.0.1:9090".to_string(),
        }
    }
}

pub struct SubhostmetricsModule {
    config: SubhostmetricsConfig,
    registry: Arc<Registry>,
    requests: Counter,
    errors: Counter,
    latency: Histogram,
    connected_peers: Gauge,
    block_height: Gauge,
}

impl SubhostmetricsModule {
    pub fn new(config: SubhostmetricsConfig) -> Self {
        info!("Initializing SubhostmetricsModule");
        
        let registry = Arc::new(Registry::new());
        
        let requests = Counter::new(
            "subhost_requests_total",
            "Total number of requests"
        ).unwrap();
        registry.register(Box::new(requests.clone())).unwrap();
        
        let errors = Counter::new(
            "subhost_errors_total",
            "Total number of errors"
        ).unwrap();
        registry.register(Box::new(errors.clone())).unwrap();
        
        let latency = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "subhost_request_duration_seconds",
                "Request duration in seconds"
            ).buckets(prometheus::exponential_buckets(0.001, 2.0, 15).unwrap())
        ).unwrap();
        registry.register(Box::new(latency.clone())).unwrap();
        
        let connected_peers = Gauge::new(
            "subhost_connected_peers",
            "Number of connected peers"
        ).unwrap();
        registry.register(Box::new(connected_peers.clone())).unwrap();
        
        let block_height = Gauge::new(
            "subhost_block_height",
            "Current block height"
        ).unwrap();
        registry.register(Box::new(block_height.clone())).unwrap();
        
        Self {
            config,
            registry,
            requests,
            errors,
            latency,
            connected_peers,
            block_height,
        }
    }
    
    pub fn process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.config.enabled {
            return Ok(());
        }
        self.requests.inc();
        debug!("Processing request in subhost-metrics");
        Ok(())
    }
    
    pub fn record_request(&self, duration_secs: f64) {
        self.requests.inc();
        self.latency.observe(duration_secs);
    }
    
    pub fn record_error(&self) {
        self.errors.inc();
    }
    
    pub fn set_connected_peers(&self, count: i64) {
        self.connected_peers.set(count as f64);
    }
    
    pub fn set_block_height(&self, height: i64) {
        self.block_height.set(height as f64);
    }
    
    pub fn gather_metrics(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }
    
    pub async fn run_exporter(&self, addr: SocketAddr) -> anyhow::Result<()> {
        let registry = self.registry.clone();
        
        let app = axum::Router::new()
            .route("/metrics", axum::routing::get(move || {
                let reg = registry.clone();
                async move {
                    let metric_families = reg.gather();
                    let encoder = TextEncoder::new();
                    let mut buffer = vec![];
                    encoder.encode(&metric_families, &mut buffer).unwrap();
                    axum::response::Response::builder()
                        .header("Content-Type", encoder.format_type())
                        .body(axum::body::Body::from(buffer))
                        .unwrap()
                }
            }));
        
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
    
    pub async fn start(&self) -> anyhow::Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let addr: SocketAddr = self.config.listen_addr.parse()?;
        self.run_exporter(addr).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubhostmetricsError {
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
        let config = SubhostmetricsConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_connections, 1000);
    }
    
    #[test]
    fn test_module_creation() {
        let module = SubhostmetricsModule::new(SubhostmetricsConfig::default());
        assert!(module.config.enabled);
    }
}
