use libp2p::{
    gossipsub::{self, MessageAuthenticity, ValidationMode, IdentTopic},
    identity::Keypair,
    kad::{self, store::MemoryStore},
    mdns,
    noise, ping,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm, SwarmBuilder,
};
use libp2p::futures::StreamExt;
use serde::{Serialize, Deserialize};
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
    pub mdns: Toggle<mdns::async_io::Behaviour>,
    pub ping: ping::Behaviour,
    pub connection_limits: libp2p::connection_limits::Behaviour,
}

pub struct NetworkManager {
    swarm: Swarm<SubhostBehavior>,
    config: NetworkConfig,
    connected_peers: HashSet<PeerId>,
    tx_topic: IdentTopic,
    message_rx: mpsc::Receiver<NetworkMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
            .max_transmit_size(256 * 1024)
            .build()
            .map_err(|e| NetworkError::Config(e.to_string()))?;

        let tx_topic = IdentTopic::new("subhost-txs");
        let mut gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(id_keys.clone()),
            gossipsub_config,
        ).map_err(|e| NetworkError::Libp2p(e.to_string()))?;
        gossipsub
            .subscribe(&tx_topic)
            .map_err(|e| NetworkError::Libp2p(e.to_string()))?;

        let store = MemoryStore::new(local_peer_id);
        let kademlia = kad::Behaviour::with_config(local_peer_id, store, kad::Config::default());
        let mdns = if config.enable_mdns {
            Some(mdns::async_io::Behaviour::new(
                mdns::Config::default(),
                local_peer_id,
            )?)
        } else {
            None
        };
        let ping = ping::Behaviour::default();
        let max_peers = u32::try_from(config.max_peers).unwrap_or(u32::MAX);
        let connection_limits = libp2p::connection_limits::Behaviour::new(
            libp2p::connection_limits::ConnectionLimits::default()
                .with_max_established(Some(max_peers))
                .with_max_established_incoming(Some(max_peers))
                .with_max_established_outgoing(Some(max_peers)),
        );

        let behavior = SubhostBehavior {
            gossipsub,
            kademlia,
            mdns: mdns.into(),
            ping,
            connection_limits,
        };

        let swarm_result = SwarmBuilder::with_existing_identity(id_keys)
            .with_async_std()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
            .with_behaviour(|_| Ok(behavior));
        
        let swarm = match swarm_result {
            Ok(builder) => builder
                .with_swarm_config(|c: libp2p::swarm::Config| c.with_idle_connection_timeout(std::time::Duration::from_secs(60)))
                .build(),
            Err(e) => return Err(NetworkError::Libp2p(e.to_string())),
        };

        let (tx, rx) = mpsc::channel(1000);

        let manager = Self {
            swarm,
            config,
            connected_peers: HashSet::new(),
            tx_topic,
            message_rx: rx,
        };

        Ok((manager, tx))
    }

    pub async fn run(mut self) -> Result<(), NetworkError> {
        let addr = self
            .config
            .listen_addr
            .parse()
            .map_err(|e| NetworkError::Config(format!("invalid listen address: {e}")))?;
        self.swarm
            .listen_on(addr)
            .map_err(|e| NetworkError::Libp2p(e.to_string()))?;

        info!("Network started");

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => {
                    self.handle_event(event).await;
                }
                Some(msg) = self.message_rx.recv() => {
                    self.broadcast(msg);
                }
                else => break,
            }
        }

        Ok(())
    }

    /// Publish a locally-submitted network message to the gossip topic so peers
    /// can receive it. The old code created the channel and immediately dropped
    /// the receiver, so every `tx.send()` failed with a closed-channel error and
    /// the transport could never dispatch application messages.
    fn broadcast(&mut self, msg: NetworkMessage) {
        let bytes = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to serialize network message: {e}");
                return;
            }
        };
        let topic = self.tx_topic.clone();
        if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
            debug!("gossipsub publish failed: {e}");
        }
    }

    async fn handle_event(&mut self, event: SwarmEvent<<SubhostBehavior as NetworkBehaviour>::ToSwarm>) {
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

impl From<libp2p::noise::Error> for NetworkError {
    fn from(e: libp2p::noise::Error) -> Self {
        NetworkError::Libp2p(e.to_string())
    }
}

impl From<&str> for NetworkError {
    fn from(e: &str) -> Self {
        NetworkError::Libp2p(e.to_string())
    }
}
