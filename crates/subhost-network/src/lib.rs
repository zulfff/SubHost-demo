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
    swarm::{
        behaviour::toggle::Toggle, dial_opts::DialOpts, ConnectionId, NetworkBehaviour, SwarmEvent,
    },
    tcp, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Gossip topic carrying application messages.
pub const TOPIC: &str = "subhost-txs";
/// Largest gossip payload accepted, matching the RPC request bound.
pub const MAX_TRANSMIT_BYTES: usize = 4 * 1024 * 1024;
/// Application envelope limit, leaving room for signed gossipsub framing.
pub const MAX_WIRE_BYTES: usize = MAX_TRANSMIT_BYTES - 1024;

/// Hard cap on the certified items one `Blocks` response may carry, and on the
/// span one `GetBlocks` request may ask for.
pub const MAX_SYNC_ITEMS: usize = 128;
/// Capacity of the inbound and outbound channels.
const CHANNEL_CAPACITY: usize = 1_000;

/// One configured endpoint, including the attempt ID used to match dial errors.
struct BootstrapRetry {
    address: Multiaddr,
    attempt: Option<ConnectionId>,
    connected: Option<PeerId>,
    next_attempt: Instant,
    delay: Duration,
}

impl BootstrapRetry {
    fn new(address: Multiaddr) -> Self {
        Self {
            address,
            attempt: None,
            connected: None,
            next_attempt: Instant::now(),
            delay: Duration::from_secs(1),
        }
    }

    fn failed(&mut self, now: Instant) {
        self.attempt = None;
        self.next_attempt = now + self.delay;
        self.delay = (self.delay * 2).min(Duration::from_secs(30));
    }
}

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
    /// A block proposal for one consensus view.
    ///
    /// `data` is the canonical block encoding; `view` lets a driver reject a
    /// proposal that is not the next view before it spends time decoding.
    BlockProposal { view: u64, data: Vec<u8> },
    /// One validator's vote for a block in a view.
    ///
    /// `validator` is the 20-byte address the signature belongs to. Without it a
    /// receiver cannot tell which registered key to verify against, and `view`
    /// binds the vote to one proposal.
    Vote { view: u64, validator: [u8; 20], block_hash: [u8; 32], signature: Vec<u8> },
    /// A committed block and its independently verifiable quorum certificate.
    BlockCommit { view: u64, data: Vec<u8>, signatures: Vec<([u8; 20], Vec<u8>)> },
    /// Canonical authenticated leader proposal, including timeout evidence.
    ConsensusProposal { data: Vec<u8> },
    /// Canonical phase-bound prepare or final-commit vote.
    ConsensusVote { data: Vec<u8> },
    /// Canonical prepare certificate and its authenticated block.
    PreparedBlock { data: Vec<u8> },
    /// Canonical signed target-view report with prepared-lock evidence.
    ViewChange { data: Vec<u8> },
    /// A complete authenticated target-view certificate for lagging peers.
    TimeoutCertificate { data: Vec<u8> },
    /// Ask a peer for a bounded ascending range of certified history.
    ///
    /// `from` and `to` are inclusive heights, both above the genesis. A peer
    /// answers with at most [`MAX_SYNC_ITEMS`] parent-linked items and may answer
    /// with a shorter prefix when its archive does not reach `to`.
    GetBlocks { from: u64, to: u64 },
    /// Certified history in ascending height order.
    ///
    /// Each item is a canonical `(block, QC)` pair. A receiver verifies and
    /// applies items in order and keeps only a validated prefix, so a truncated
    /// or invalid suffix can never move the tip past a missing certificate.
    Blocks { items: Vec<Vec<u8>> },
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
            Self::DagVertex { data }
            | Self::BlockProposal { data, .. }
            | Self::ConsensusProposal { data }
            | Self::ConsensusVote { data }
            | Self::PreparedBlock { data }
            | Self::ViewChange { data }
            | Self::TimeoutCertificate { data } => data.len(),
            Self::BlockCommit { data, signatures, .. } => {
                if data.is_empty() || signatures.is_empty() {
                    return Err(NetworkError::InvalidMessage("empty block or certificate".into()));
                }
                signatures.iter().try_fold(data.len(), |size, (_, signature)| {
                    if signature.len() != 96 {
                        return Err(NetworkError::InvalidMessage(
                            "malformed certificate signature".into(),
                        ));
                    }
                    size.checked_add(20 + signature.len()).ok_or_else(|| {
                        NetworkError::InvalidMessage("certificate size overflow".into())
                    })
                })?
            }
            Self::Vote { signature, .. } => signature.len(),
            Self::GetBlocks { from, to } => {
                if *from == 0 {
                    return Err(NetworkError::InvalidMessage(
                        "sync range must start at height 1 or later".into(),
                    ));
                }
                let span = to.checked_sub(*from).ok_or_else(|| {
                    NetworkError::InvalidMessage("sync range ends before it starts".into())
                })?;
                if span >= MAX_SYNC_ITEMS as u64 {
                    return Err(NetworkError::InvalidMessage(format!(
                        "sync range of {} heights exceeds the {MAX_SYNC_ITEMS} item limit",
                        span + 1
                    )));
                }
                // A range request carries only two integers; it needs no payload
                // or envelope budget check beyond the bounds above.
                return Ok(());
            }
            Self::Blocks { items } => {
                if items.is_empty() {
                    return Err(NetworkError::InvalidMessage("empty block range".into()));
                }
                if items.len() > MAX_SYNC_ITEMS {
                    return Err(NetworkError::InvalidMessage(format!(
                        "block range of {} items exceeds the {MAX_SYNC_ITEMS} item limit",
                        items.len()
                    )));
                }
                let mut total = 0usize;
                for item in items {
                    if item.is_empty() {
                        return Err(NetworkError::InvalidMessage(
                            "empty certified item in block range".into(),
                        ));
                    }
                    total = total.checked_add(item.len()).ok_or_else(|| {
                        NetworkError::InvalidMessage("block range size overflow".into())
                    })?;
                }
                total
            }
        };
        if payload_len == 0 {
            return Err(NetworkError::InvalidMessage("empty payload".into()));
        }
        if payload_len > MAX_WIRE_BYTES {
            return Err(NetworkError::InvalidMessage(format!(
                "payload of {payload_len} bytes exceeds the {MAX_TRANSMIT_BYTES} byte limit"
            )));
        }
        let wire_len = serde_json::to_vec(self)
            .map_err(|error| NetworkError::InvalidMessage(error.to_string()))?
            .len();
        if wire_len > MAX_WIRE_BYTES {
            return Err(NetworkError::InvalidMessage(
                "serialized envelope exceeds wire budget".into(),
            ));
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
    subscribed_peers: watch::Receiver<usize>,
}

impl NetworkHandle {
    /// Peers that have announced a subscription to the application topic.
    ///
    /// A transport connection alone is not enough to publish reliably: sending
    /// before the remote subscription arrives can consume gossipsub's duplicate
    /// cache without delivering the message.
    pub fn subscribed_peer_count(&self) -> usize {
        *self.subscribed_peers.borrow()
    }
}

/// Owns the libp2p swarm and pumps messages in both directions.
pub struct NetworkManager {
    swarm: Swarm<SubhostBehavior>,
    config: NetworkConfig,
    connected_peers: HashSet<PeerId>,
    subscribed_peers: HashSet<PeerId>,
    subscribed_peer_count: watch::Sender<usize>,
    topic: IdentTopic,
    outbound_rx: mpsc::Receiver<NetworkMessage>,
    inbound_tx: mpsc::Sender<NetworkMessage>,
    local_peer_id: PeerId,
    bootstrap: Vec<BootstrapRetry>,
}

impl NetworkManager {
    /// Build the swarm and return it alongside the application's channels.
    pub fn new(config: NetworkConfig) -> Result<(Self, NetworkHandle), NetworkError> {
        // Validate configuration before allocating anything.
        let (_, bootstrap) = config.parsed_addresses()?;
        let mut seen = HashSet::new();
        let bootstrap = bootstrap
            .into_iter()
            .filter(|address| seen.insert(address.clone()))
            .map(BootstrapRetry::new)
            .collect();

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
                config.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        let (outbound_tx, outbound_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (subscribed_peer_count, subscribed_peers) = watch::channel(0);

        Ok((
            Self {
                swarm,
                config,
                connected_peers: HashSet::new(),
                subscribed_peers: HashSet::new(),
                subscribed_peer_count,
                topic,
                outbound_rx,
                inbound_tx,
                local_peer_id,
                bootstrap,
            },
            NetworkHandle {
                outbound: outbound_tx,
                inbound: inbound_rx,
                local_peer_id,
                subscribed_peers,
            },
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
        let (listen_addr, _) = self.config.parsed_addresses()?;
        self.swarm
            .listen_on(listen_addr)
            .map_err(|error| NetworkError::Libp2p(error.to_string()))?;

        self.retry_bootstrap();
        info!("network started");
        let mut retry_tick = tokio::time::interval(Duration::from_millis(250));
        retry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.handle_event(event),
                _ = retry_tick.tick() => self.retry_bootstrap(),
                message = self.outbound_rx.recv() => match message {
                    Some(message) => self.publish(message),
                    // The application dropped its handle: shut down cleanly.
                    None => break,
                },
            }
        }
        Ok(())
    }

    fn retry_bootstrap(&mut self) {
        let now = Instant::now();
        for peer in &mut self.bootstrap {
            if peer.attempt.is_some() || peer.connected.is_some() || now < peer.next_attempt {
                continue;
            }
            let options = DialOpts::from(peer.address.clone());
            let connection = options.connection_id();
            match self.swarm.dial(options) {
                Ok(()) => {
                    peer.attempt = Some(connection);
                    debug!(address = %peer.address, "dialling bootstrap endpoint");
                }
                Err(error) => {
                    debug!(address = %peer.address, %error, "bootstrap dial deferred");
                    peer.failed(now);
                }
            }
        }
    }

    fn note_connection_closed(&mut self, peer_id: PeerId, remaining: u32) {
        if remaining != 0 {
            return;
        }
        self.connected_peers.remove(&peer_id);
        if self.subscribed_peers.remove(&peer_id) {
            self.subscribed_peer_count.send_replace(self.subscribed_peers.len());
        }
        for peer in &mut self.bootstrap {
            if peer.connected == Some(peer_id) {
                peer.connected = None;
                peer.attempt = None;
                peer.delay = Duration::from_secs(1);
                peer.next_attempt = Instant::now() + peer.delay;
            }
        }
    }

    fn note_subscribed(&mut self, peer_id: PeerId) {
        if self.subscribed_peers.insert(peer_id) {
            self.subscribed_peer_count.send_replace(self.subscribed_peers.len());
        }
    }

    fn note_unsubscribed(&mut self, peer_id: PeerId) {
        if self.subscribed_peers.remove(&peer_id) {
            self.subscribed_peer_count.send_replace(self.subscribed_peers.len());
        }
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
            SwarmEvent::ConnectionEstablished { peer_id, connection_id, .. } => {
                self.connected_peers.insert(peer_id);
                for peer in &mut self.bootstrap {
                    if peer.attempt == Some(connection_id) {
                        peer.attempt = None;
                        peer.connected = Some(peer_id);
                        peer.delay = Duration::from_secs(1);
                    }
                }
                debug!(%peer_id, peers = self.connected_peers.len(), "peer connected");
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                self.note_connection_closed(peer_id, num_established);
                debug!(%peer_id, peers = self.connected_peers.len(), "peer disconnected");
            }
            SwarmEvent::OutgoingConnectionError { connection_id, error, .. } => {
                for peer in &mut self.bootstrap {
                    if peer.attempt == Some(connection_id) {
                        debug!(address = %peer.address, %error, "bootstrap dial failed; will retry");
                        peer.failed(Instant::now());
                    }
                }
            }
            SwarmEvent::Behaviour(SubhostBehaviorEvent::Gossipsub(gossipsub::Event::Message {
                message,
                ..
            })) => self.handle_gossip(&message.data),
            SwarmEvent::Behaviour(SubhostBehaviorEvent::Gossipsub(
                gossipsub::Event::Subscribed { peer_id, topic },
            )) if topic == self.topic.hash() => {
                self.note_subscribed(peer_id);
            }
            SwarmEvent::Behaviour(SubhostBehaviorEvent::Gossipsub(
                gossipsub::Event::Unsubscribed { peer_id, topic },
            )) if topic == self.topic.hash() => {
                self.note_unsubscribed(peer_id);
            }
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
        if data.len() > MAX_WIRE_BYTES {
            debug!(bytes = data.len(), "discarding an oversized gossip envelope");
            return;
        }
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
    fn retry_backoff_is_capped_and_final_disconnect_resets_it() {
        let address = "/ip4/127.0.0.1/tcp/30333".parse().unwrap();
        let mut retry = BootstrapRetry::new(address);
        let now = Instant::now();
        retry.failed(now);
        assert_eq!(retry.next_attempt, now + Duration::from_secs(1));
        for _ in 0..10 {
            retry.failed(now);
        }
        assert_eq!(retry.delay, Duration::from_secs(30));
        let peer_id = PeerId::random();
        retry.connected = Some(peer_id);
        let (mut manager, _) =
            NetworkManager::new(NetworkConfig { enable_mdns: false, ..Default::default() })
                .unwrap();
        manager.bootstrap = vec![retry];
        manager.connected_peers.insert(peer_id);
        manager.note_connection_closed(peer_id, 1);
        assert_eq!(manager.connected_peer_count(), 1);
        assert_eq!(manager.bootstrap[0].connected, Some(peer_id));
        manager.note_connection_closed(peer_id, 0);
        assert_eq!(manager.connected_peer_count(), 0);
        assert!(manager.bootstrap[0].connected.is_none());
        assert_eq!(manager.bootstrap[0].delay, Duration::from_secs(1));
    }

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
    fn certificate_messages_require_a_bounded_nonempty_signature_list() {
        for (data, signatures) in [
            (Vec::new(), vec![([1; 20], vec![2; 96])]),
            (vec![1], Vec::new()),
            (vec![1], vec![([1; 20], vec![2; 95])]),
            (vec![1; MAX_TRANSMIT_BYTES], vec![([1; 20], vec![2; 96])]),
        ] {
            assert!(NetworkMessage::BlockCommit { view: 1, data, signatures }.validate().is_err());
        }
    }

    #[test]
    fn sync_range_requests_are_bounded() {
        assert!(NetworkMessage::GetBlocks { from: 1, to: 1 }.validate().is_ok());
        assert!(NetworkMessage::GetBlocks { from: 1, to: MAX_SYNC_ITEMS as u64 }
            .validate()
            .is_ok());
        assert!(
            NetworkMessage::GetBlocks { from: 0, to: 4 }.validate().is_err(),
            "genesis is never requested as catch-up history"
        );
        assert!(
            NetworkMessage::GetBlocks { from: 5, to: 4 }.validate().is_err(),
            "an inverted range must not be relayed"
        );
        assert!(
            NetworkMessage::GetBlocks { from: 1, to: MAX_SYNC_ITEMS as u64 + 1 }
                .validate()
                .is_err(),
            "a range wider than the item cap must not be relayed"
        );
    }

    #[test]
    fn block_ranges_are_bounded_and_never_empty() {
        assert!(NetworkMessage::Blocks { items: vec![vec![1]] }.validate().is_ok());
        assert!(NetworkMessage::Blocks { items: Vec::new() }.validate().is_err());
        assert!(
            NetworkMessage::Blocks { items: vec![Vec::new()] }.validate().is_err(),
            "an empty certified item must not be relayed"
        );
        assert!(
            NetworkMessage::Blocks { items: vec![vec![1]; MAX_SYNC_ITEMS + 1] }.validate().is_err(),
            "a response wider than the item cap must not be relayed"
        );

        for message in [
            NetworkMessage::GetBlocks { from: 7, to: 9 },
            NetworkMessage::Blocks { items: vec![vec![1, 2], vec![3]] },
        ] {
            let encoded = serde_json::to_vec(&message).unwrap();
            let decoded: NetworkMessage = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn message_validation_rejects_empty_oversized_and_exhausted_payloads() {
        assert!(NetworkMessage::DagVertex { data: vec![1] }.validate().is_ok());
        assert!(NetworkMessage::DagVertex { data: Vec::new() }.validate().is_err());
        assert!(NetworkMessage::BlockProposal { view: 1, data: vec![0; MAX_TRANSMIT_BYTES + 1] }
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
        assert!(NetworkMessage::Vote {
            view: 1,
            validator: [2; 20],
            block_hash: [0; 32],
            signature: Vec::new()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn messages_round_trip_through_the_wire_encoding() {
        let messages = [
            NetworkMessage::TransactionStem { data: vec![1, 2], ttl: 4 },
            NetworkMessage::TransactionFluff { tx_hash: [7; 32], data: vec![3] },
            NetworkMessage::DagVertex { data: vec![4] },
            NetworkMessage::BlockCommit {
                view: 4,
                data: vec![5],
                signatures: vec![([8; 20], vec![6; 96])],
            },
            NetworkMessage::BlockProposal { view: 4, data: vec![5] },
            NetworkMessage::Vote {
                view: 4,
                validator: [8; 20],
                block_hash: [9; 32],
                signature: vec![6],
            },
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
        let (mut manager, handle) = NetworkManager::new(NetworkConfig {
            // mDNS binds a socket, so keep it off in tests.
            enable_mdns: false,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(manager.local_peer_id(), handle.local_peer_id);
        assert_eq!(manager.connected_peer_count(), 0);
        assert_eq!(handle.subscribed_peer_count(), 0);
        let peer = PeerId::random();
        manager.note_subscribed(peer);
        manager.note_subscribed(peer);
        assert_eq!(handle.subscribed_peer_count(), 1);
        manager.note_unsubscribed(peer);
        assert_eq!(handle.subscribed_peer_count(), 0);
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

        let message = NetworkMessage::BlockProposal { view: 1, data: vec![1, 2, 3] };
        manager.handle_gossip(&serde_json::to_vec(&message).unwrap());
        assert_eq!(handle.inbound.recv().await, Some(message));

        // Malformed and invalid payloads are dropped rather than forwarded.
        manager.handle_gossip(b"not json");
        manager.handle_gossip(
            &serde_json::to_vec(&NetworkMessage::DagVertex { data: Vec::new() }).unwrap(),
        );
        assert!(handle.inbound.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connected_peers_report_their_topic_subscriptions() {
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let first_port = first.local_addr().unwrap().port();
        let second_port = second.local_addr().unwrap().port();
        drop((first, second));

        let (first_manager, first_handle) = NetworkManager::new(NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{first_port}"),
            bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{second_port}")],
            enable_mdns: false,
            ..Default::default()
        })
        .unwrap();
        let (second_manager, second_handle) = NetworkManager::new(NetworkConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{second_port}"),
            bootstrap_peers: vec![format!("/ip4/127.0.0.1/tcp/{first_port}")],
            enable_mdns: false,
            ..Default::default()
        })
        .unwrap();
        let first_task = tokio::spawn(first_manager.run());
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(("127.0.0.1", first_port)).await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first peer must start listening");
        let second_task = tokio::spawn(second_manager.run());

        tokio::time::timeout(Duration::from_secs(10), async {
            while first_handle.subscribed_peer_count() == 0
                || second_handle.subscribed_peer_count() == 0
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("connected peers must announce the application topic subscription");

        drop((first_handle, second_handle));
        assert!(first_task.await.unwrap().is_ok());
        assert!(second_task.await.unwrap().is_ok());
    }
}
