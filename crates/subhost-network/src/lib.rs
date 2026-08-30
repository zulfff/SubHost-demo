//! libp2p gossip transport.
//!
//! Scope: this is a working publish/subscribe transport — messages submitted to
//! the outbound channel are published to the gossip topic, and messages received
//! from peers are decoded and forwarded to the application. It does not implement
//! Dandelion++ relay, peer scoring, or transaction validation; the message
//! variants that name those flows are wire types, not evidence of the behaviour.
//!
//! The swarm runs on the tokio transport, matching the rest of the workspace, so
//! there is no second async runtime in the process.

use libp2p::futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identity::Keypair,
    kad::{self, store::MemoryStore},
    mdns, noise, ping,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Gossip topic carrying application messages.
pub const TOPIC: &str = "subhost-txs";
/// Largest gossip payload accepted, matching the RPC request bound.
pub const MAX_TRANSMIT_BYTES: usize = 256 * 1024;
/// Capacity of the inbound and outbound channels.
const CHANNEL_CAPACITY: usize = 1_000;

#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// Multiaddr to listen on.
    pub listen_addr: String,
    /// Peers dialled at startup.
    pub bootstrap_peers: Vec<String>,
    /// Cap on established connections in each direction.
    pub max_peers: usize,
    /// Enable local-network mDNS discovery.
    pub enable_mdns: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_addr: "/ip4/0.0.0.0/tcp/0".to_string(),
            bootstrap_peers: Vec::new(),
            max_peers: 50,
            enable_mdns: true,
        }
    }
}

impl NetworkConfig {
    /// Parse and validate every address up front so a typo fails at startup
    /// rather than silently leaving the node unreachable.
    pub fn parsed_addresses(&self) -> Result<(Multiaddr, Vec<Multiaddr>), NetworkError> {
        if self.max_peers == 0 {
            return Err(NetworkError::Config("max_peers must be > 0".into()));
        }
        let listen = self
            .listen_addr
            .parse::<Multiaddr>()
            .map_err(|error| NetworkError::Config(format!("invalid listen address: {error}")))?;
        let mut bootstrap = Vec::with_capacity(self.bootstrap_peers.len());
        for peer in &self.bootstrap_peers {
            bootstrap.push(peer.parse::<Multiaddr>().map_err(|error| {
                NetworkError::Config(format!("invalid bootstrap peer {peer}: {error}"))
            })?);
        }
        Ok((listen, bootstrap))
    }
}

#[derive(NetworkBehaviour)]
pub struct SubhostBehavior {
    pub gossipsub: gossipsub::Behaviour,
    pub kademlia: kad::Behaviour<MemoryStore>,
    pub mdns: Toggle<mdns::tokio::Behaviour>,
    pub ping: ping::Behaviour,
    pub connection_limits: libp2p::connection_limits::Behaviour,
}

/// Application-level messages carried over the gossip topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkMessage {
    /// A transaction relayed along a Dandelion++ stem path. The `ttl` bound is
    /// enforced on receipt; stem *routing* itself is not implemented.
    TransactionStem { data: Vec<u8>, ttl: u8 },
    /// A transaction broadcast to the whole topic.
    TransactionFluff { tx_hash: [u8; 32], data: Vec<u8> },
    /// A serialized DAG vertex.
    DagVertex { data: Vec<u8> },
    /// A serialized block proposal.
    BlockProposal { data: Vec<u8> },
    /// A vote for a block hash.
    Vote { block_hash: [u8; 32], signature: Vec<u8> },
}

impl NetworkMessage {
    /// Reject a structurally impossible message before it reaches the application.
    pub fn validate(&self) -> Result<(), NetworkError> {
        let payload_len = match self {
            Self::TransactionStem { data, ttl } => {
                if *ttl == 0 {
                    return Err(NetworkError::InvalidMessage("stem TTL is exhausted".into()));
                }
                data.len()
            }
            Self::TransactionFluff { data, .. } => data.len(),
            Self::DagVertex { data } | Self::BlockProposal { data } => data.len(),
            Self::Vote { signature, .. } => signature.len(),
        };
        if payload_len == 0 {
            return Err(NetworkError::InvalidMessage("empty payload".into()));
        }
        if payload_len > MAX_TRANSMIT_BYTES {
            return Err(NetworkError::InvalidMessage(format!(
                "payload of {payload_len} bytes exceeds the {MAX_TRANSMIT_BYTES} byte limit"
            )));
        }
        Ok(())
    }
}

/// Channels an application uses to talk to the swarm.
pub struct NetworkHandle {
    /// Messages to publish to peers.
    pub outbound: mpsc::Sender<NetworkMessage>,
    /// Messages received from peers.
    pub inbound: mpsc::Receiver<NetworkMessage>,
    pub local_peer_id: PeerId,
}

/// Owns the libp2p swarm and pumps messages in both directions.
pub struct NetworkManager {
    swarm: Swarm<SubhostBehavior>,
    config: NetworkConfig,
    connected_peers: HashSet<PeerId>,
    topic: IdentTopic,
    outbound_rx: mpsc::Receiver<NetworkMessage>,
    inbound_tx: mpsc::Sender<NetworkMessage>,
    local_peer_id: PeerId,
}

impl NetworkManager {
    /// Build the swarm and return it alongside the application's channels.
    pub fn new(config: NetworkConfig) -> Result<(Self, NetworkHandle), NetworkError> {
        // Validate configuration before allocating anything.
        config.parsed_addresses()?;

        let id_keys = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(id_keys.public());
        info!(%local_peer_id, "local libp2p identity");

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            // Strict mode requires every message to be signed by its author.
            .validation_mode(ValidationMode::Strict)
            .max_transmit_size(MAX_TRANSMIT_BYTES)
            .build()
            .map_err(|error| NetworkError::Config(error.to_string()))?;

        let topic = IdentTopic::new(TOPIC);
        let mut gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(id_keys.clone()),
            gossipsub_config,
        )
        .map_err(|error| NetworkError::Libp2p(error.to_string()))?;
        gossipsub.subscribe(&topic).map_err(|error| NetworkError::Libp2p(error.to_string()))?;

        let kademlia = kad::Behaviour::with_config(
            local_peer_id,
            MemoryStore::new(local_peer_id),
            kad::Config::default(),
        );
        let mdns = if config.enable_mdns {
            Some(mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?)
        } else {
            None
        };
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
            ping: ping::Behaviour::default(),
            connection_limits,
        };

        let swarm = SwarmBuilder::with_existing_identity(id_keys)
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
            .with_behaviour(|_| Ok(behavior))
            .map_err(|error| NetworkError::Libp2p(error.to_string()))?
            .with_swarm_config(|config: libp2p::swarm::Config| {
                config.with_idle_connection_timeout(std::time::Duration::from_secs(60))
            })
            .build();

        let (outbound_tx, outbound_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_CAPACITY);

        Ok((
            Self {
                swarm,
                config,
                connected_peers: HashSet::new(),
                topic,
                outbound_rx,
                inbound_tx,
                local_peer_id,
            },
            NetworkHandle { outbound: outbound_tx, inbound: inbound_rx, local_peer_id },
        ))
    }

    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub fn connected_peer_count(&self) -> usize {
        self.connected_peers.len()
    }

    /// Listen, dial the bootstrap set, then pump events until the outbound
    /// channel closes.
    pub async fn run(mut self) -> Result<(), NetworkError> {
        let (listen_addr, bootstrap) = self.config.parsed_addresses()?;
        self.swarm
            .listen_on(listen_addr)
            .map_err(|error| NetworkError::Libp2p(error.to_string()))?;

        for peer in bootstrap {
            match self.swarm.dial(peer.clone()) {
                Ok(()) => debug!(%peer, "dialling bootstrap peer"),
                // One unreachable bootstrap peer must not stop the node.
                Err(error) => warn!(%peer, %error, "cannot dial bootstrap peer"),
            }
        }
        info!("network started");

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.handle_event(event),
                message = self.outbound_rx.recv() => match message {
                    Some(message) => self.publish(message),
                    // The application dropped its handle: shut down cleanly.
                    None => break,
                },
            }
        }
        Ok(())
    }

    /// Publish one application message to the gossip topic.
    fn publish(&mut self, message: NetworkMessage) {
        if let Err(error) = message.validate() {
            warn!(%error, "refusing to publish an invalid message");
            return;
        }
        let bytes = match serde_json::to_vec(&message) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(%error, "cannot serialize an outbound message");
                return;
            }
        };
        let topic = self.topic.clone();
        if let Err(error) = self.swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
            // With no peers subscribed this is expected, so it is not an error.
            debug!(%error, "gossip publish failed");
        }
    }

    fn handle_event(&mut self, event: SwarmEvent<SubhostBehaviorEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(%address, "listening");
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                self.connected_peers.insert(peer_id);
                debug!(%peer_id, peers = self.connected_peers.len(), "peer connected");
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                self.connected_peers.remove(&peer_id);
                debug!(%peer_id, peers = self.connected_peers.len(), "peer disconnected");
            }
            SwarmEvent::Behaviour(SubhostBehaviorEvent::Gossipsub(gossipsub::Event::Message {
                message,
                ..
            })) => self.handle_gossip(&message.data),
            SwarmEvent::Behaviour(SubhostBehaviorEvent::Mdns(mdns::Event::Discovered(peers))) => {
                for (peer_id, address) in peers {
                    debug!(%peer_id, %address, "mDNS discovered peer");
                    self.swarm.behaviour_mut().kademlia.add_address(&peer_id, address);
                }
            }
            _ => {}
        }
    }

    /// Decode an inbound gossip payload and hand it to the application.
    ///
    /// Previously inbound messages were dropped, so the transport could publish
    /// but never deliver.
    fn handle_gossip(&mut self, data: &[u8]) {
        let message: NetworkMessage = match serde_json::from_slice(data) {
            Ok(message) => message,
            Err(error) => {
                debug!(%error, "discarding an undecodable gossip message");
                return;
            }
        };
        if let Err(error) = message.validate() {
            debug!(%error, "discarding an invalid gossip message");
            return;
        }
        // A full application queue must shed load, not stall the swarm.
        if let Err(error) = self.inbound_tx.try_send(message) {
            warn!(%error, "inbound queue is full; dropping a gossip message");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("libp2p error: {0}")]
    Libp2p(String),

    #[error("invalid message: {0}")]
    InvalidMessage(String),
}

impl From<std::io::Error> for NetworkError {
    fn from(error: std::io::Error) -> Self {
        Self::Libp2p(error.to_string())
    }
}

impl From<noise::Error> for NetworkError {
    fn from(error: noise::Error) -> Self {
        Self::Libp2p(error.to_string())
    }
}

impl From<gossipsub::SubscriptionError> for NetworkError {
    fn from(error: gossipsub::SubscriptionError) -> Self {
        Self::Libp2p(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let (listen, bootstrap) = NetworkConfig::default().parsed_addresses().unwrap();
        assert_eq!(listen.to_string(), "/ip4/0.0.0.0/tcp/0");
        assert!(bootstrap.is_empty());
    }

    #[test]
    fn invalid_addresses_and_peer_limits_are_rejected_up_front() {
        assert!(matches!(
            NetworkConfig { listen_addr: "not-a-multiaddr".into(), ..Default::default() }
                .parsed_addresses(),
            Err(NetworkError::Config(_))
        ));
        assert!(matches!(
            NetworkConfig {
                bootstrap_peers: vec!["/ip4/127.0.0.1/tcp/1".into(), "garbage".into()],
                ..Default::default()
            }
            .parsed_addresses(),
            Err(NetworkError::Config(_))
        ));
        assert!(matches!(
            NetworkConfig { max_peers: 0, ..Default::default() }.parsed_addresses(),
            Err(NetworkError::Config(_))
        ));
    }

    #[test]
    fn bootstrap_peers_are_parsed_in_order() {
        let config = NetworkConfig {
            bootstrap_peers: vec![
                "/ip4/10.0.0.1/tcp/30333".into(),
                "/dns4/validator-1/tcp/30333".into(),
            ],
            ..Default::default()
        };
        let (_, bootstrap) = config.parsed_addresses().unwrap();
        assert_eq!(bootstrap.len(), 2);
        assert_eq!(bootstrap[0].to_string(), "/ip4/10.0.0.1/tcp/30333");
    }

    #[test]
    fn message_validation_rejects_empty_oversized_and_exhausted_payloads() {
        assert!(NetworkMessage::DagVertex { data: vec![1] }.validate().is_ok());
        assert!(NetworkMessage::DagVertex { data: Vec::new() }.validate().is_err());
        assert!(NetworkMessage::BlockProposal { data: vec![0; MAX_TRANSMIT_BYTES + 1] }
            .validate()
            .is_err());

        assert!(NetworkMessage::TransactionStem { data: vec![1], ttl: 3 }.validate().is_ok());
        assert!(
            NetworkMessage::TransactionStem { data: vec![1], ttl: 0 }.validate().is_err(),
            "an exhausted TTL must not be relayed"
        );

        assert!(NetworkMessage::TransactionFluff { tx_hash: [0; 32], data: vec![1] }
            .validate()
            .is_ok());
        assert!(NetworkMessage::Vote { block_hash: [0; 32], signature: Vec::new() }
            .validate()
            .is_err());
    }

    #[test]
    fn messages_round_trip_through_the_wire_encoding() {
        let messages = [
            NetworkMessage::TransactionStem { data: vec![1, 2], ttl: 4 },
            NetworkMessage::TransactionFluff { tx_hash: [7; 32], data: vec![3] },
            NetworkMessage::DagVertex { data: vec![4] },
            NetworkMessage::BlockProposal { data: vec![5] },
            NetworkMessage::Vote { block_hash: [9; 32], signature: vec![6] },
        ];
        for message in messages {
            let encoded = serde_json::to_vec(&message).unwrap();
            assert!(encoded.len() <= MAX_TRANSMIT_BYTES);
            let decoded: NetworkMessage = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn manager_construction_yields_working_channels() {
        let (manager, handle) = NetworkManager::new(NetworkConfig {
            // mDNS binds a socket, so keep it off in tests.
            enable_mdns: false,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(manager.local_peer_id(), handle.local_peer_id);
        assert_eq!(manager.connected_peer_count(), 0);
        // The receiver is retained by the handle, so sending must not fail.
        handle
            .outbound
            .try_send(NetworkMessage::DagVertex { data: vec![1] })
            .expect("the outbound channel must stay open");
    }

    #[test]
    fn manager_construction_rejects_an_invalid_config() {
        assert!(matches!(
            NetworkManager::new(NetworkConfig {
                listen_addr: "nope".into(),
                enable_mdns: false,
                ..Default::default()
            }),
            Err(NetworkError::Config(_))
        ));
    }

    #[tokio::test]
    async fn inbound_gossip_reaches_the_application() {
        let (mut manager, mut handle) =
            NetworkManager::new(NetworkConfig { enable_mdns: false, ..Default::default() })
                .unwrap();

        let message = NetworkMessage::BlockProposal { data: vec![1, 2, 3] };
        manager.handle_gossip(&serde_json::to_vec(&message).unwrap());
        assert_eq!(handle.inbound.recv().await, Some(message));

        // Malformed and invalid payloads are dropped rather than forwarded.
        manager.handle_gossip(b"not json");
        manager.handle_gossip(
            &serde_json::to_vec(&NetworkMessage::DagVertex { data: Vec::new() }).unwrap(),
        );
        assert!(handle.inbound.try_recv().is_err());
    }
}
