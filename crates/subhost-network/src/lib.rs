use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identity::Keypair,
    kad::{self, store::MemoryStore},
    mdns,
    noise, ping,
    swarm::{NetworkBehaviour, SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct NetworkConfig {
    pub listen_addr: String,
    pub bootstrap_peers: Vec<String>,
    pub max_peers: usize,
    pub enable_mdns: bool,
    pub dandelion_enabled: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_addr: "/ip4/0.0.0.0/tcp/0".to_string(),
            bootstrap_peers: vec![],
            max_peers: 50,
            enable_mdns: true,
            dandelion_enabled: true,
        }
    }
}

#[derive(NetworkBehaviour)]
pub struct SubhostBehavior {
    pub gossipsub: gossipsub::Behaviour,
    pub kademlia: kad::Behaviour<MemoryStore>,
    pub mdns: mdns::tokio::Behaviour,
    pub ping: ping::Behaviour,
}

pub struct NetworkManager {
    swarm: Swarm<SubhostBehavior>,
    config: NetworkConfig,
    connected_peers: HashSet<PeerId>,
}

#[derive(Clone, Debug)]
pub enum NetworkMessage {
    TransactionStem { data: Vec<u8>, ttl: u8 },
    TransactionFluff { tx_hash: [u8; 32], data: Vec<u8> },
    DAGVertex { data: Vec<u8> },
    BlockProposal { data: Vec<u8> },
    Vote { block_hash: [u8; 32], signature: Vec<u8> },
}

impl NetworkManager {
    pub async fn new(config: NetworkConfig) -> Result<(Self, mpsc::Sender<NetworkMessage>), NetworkError> {
        let id_keys = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(id_keys.public());
        
        info!("Local peer id: {:?}", local_peer_id);

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .validation_mode(ValidationMode::Strict)
            .build()
            .map_err(|e: Box<dyn std::error::Error + Send + Sync>| NetworkError::Config(e.to_string()))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(id_keys.clone()),
            gossipsub_config,
        )?;

        let store = MemoryStore::new(local_peer_id);
        let kademlia = kad::Behaviour::with_config(local_peer_id, store, kad::Config::default());
        let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
        let ping = ping::Behaviour::default();

        let behavior = SubhostBehavior {
            gossipsub,
            kademlia,
            mdns,
            ping,
        };

        let swarm = SwarmBuilder::with_existing_identity(id_keys)
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
            .with_quic()
            .with_behaviour(|_| Ok(behavior))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(60)))
            .build();

        let (tx, _rx) = mpsc::channel(1000);

        let manager = Self {
            swarm,
            config,
            connected_peers: HashSet::new(),
        };

        Ok((manager, tx))
    }

    pub async fn run(mut self) {
        let addr = self.config.listen_addr.parse().expect("valid multiaddr");
        self.swarm.listen_on(addr).expect("listen failed");

        info!("Network started");

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => {
                    self.handle_event(event).await;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: SwarmEvent<SubhostBehavior>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!("Listening on {:?}", address);
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                debug!("Connected to {:?}", peer_id);
                self.connected_peers.insert(peer_id);
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                debug!("Disconnected from {:?}", peer_id);
                self.connected_peers.remove(&peer_id);
            }
            _ => {}
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("Configuration error: {0}")]
    Config(String),
    
    #[error("Libp2p error: {0}")]
    Libp2p(String),
}

impl From<gossipsub::SubscriptionError> for NetworkError {
    fn from(e: gossipsub::SubscriptionError) -> Self {
        NetworkError::Libp2p(e.to_string())
    }
}

impl From<std::io::Error> for NetworkError {
    fn from(e: std::io::Error) -> Self {
        NetworkError::Libp2p(e.to_string())
    }
}
