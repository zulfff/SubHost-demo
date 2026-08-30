//! IBC channel handshake, packet flow, and replay protection.
//!
//! Scope: this implements the *local* packet state machine — channel lifecycle,
//! sequence ordering, timeouts, and replay rejection. It does **not** verify light
//! client proofs or commitment proofs from the counterparty chain, so a packet is
//! trusted only as far as the relayer that delivered it. Do not treat this as a
//! trust-minimized bridge.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use subhost_core::{Address, Amount, BlockHeight, Timestamp};
use tracing::info;

/// Largest packet payload accepted.
pub const MAX_PACKET_BYTES: usize = 128 * 1024;
/// Default timeout window applied to an outbound packet, in blocks.
pub const DEFAULT_TIMEOUT_BLOCKS: BlockHeight = 1_000;
/// Default timeout window applied to an outbound packet, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: Timestamp = 600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IbcConfig {
    pub enabled: bool,
    pub chain_id: String,
    pub connection_id: String,
    pub client_id: String,
    pub counterparty_chain_id: String,
}

impl Default for IbcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            chain_id: "subhost-1".to_string(),
            connection_id: "connection-0".to_string(),
            client_id: "07-tendermint-0".to_string(),
            counterparty_chain_id: "cosmoshub-4".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IbcChannel {
    pub channel_id: String,
    pub port_id: String,
    pub state: ChannelState,
    pub ordering: ChannelOrdering,
    pub counterparty_channel_id: String,
    pub counterparty_port_id: String,
}

impl IbcChannel {
    /// Whether the channel may carry packets.
    pub fn is_open(&self) -> bool {
        matches!(self.state, ChannelState::Open)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelState {
    Init,
    TryOpen,
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelOrdering {
    Unordered,
    Ordered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IbcPacket {
    pub sequence: u64,
    pub source_port: String,
    pub source_channel: String,
    pub destination_port: String,
    pub destination_channel: String,
    pub data: Vec<u8>,
    /// Block height after which the packet is void. 0 disables the height check.
    pub timeout_height: BlockHeight,
    /// Unix second after which the packet is void. 0 disables the time check.
    pub timeout_timestamp: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IbcTransfer {
    pub sender: Address,
    pub receiver: String,
    pub token: String,
    pub amount: Amount,
    pub source_channel: String,
}

impl IbcTransfer {
    fn validate(&self) -> Result<(), IbcError> {
        if self.amount == 0 {
            return Err(IbcError::InvalidTransfer("amount must be > 0".into()));
        }
        if self.receiver.trim().is_empty() {
            return Err(IbcError::InvalidTransfer("receiver is required".into()));
        }
        if self.token.trim().is_empty() {
            return Err(IbcError::InvalidTransfer("token denomination is required".into()));
        }
        Ok(())
    }
}

/// The local IBC packet state machine.
#[derive(Debug)]
pub struct IbcModule {
    config: IbcConfig,
    channels: HashMap<String, IbcChannel>,
    /// Packets this chain sent, by sequence.
    sent_packets: HashMap<u64, IbcPacket>,
    next_sequence_send: u64,
    /// Per-channel next expected receive sequence, for ordered channels.
    next_sequence_recv: HashMap<String, u64>,
    current_height: BlockHeight,
    /// `(source_channel, sequence)` pairs already received.
    received_packets: HashSet<(String, u64)>,
    acknowledged_packets: HashSet<u64>,
}

impl IbcModule {
    pub fn new(config: IbcConfig) -> Self {
        info!(chain_id = %config.chain_id, "IBC module initialized");
        Self {
            config,
            channels: HashMap::new(),
            sent_packets: HashMap::new(),
            next_sequence_send: 1,
            next_sequence_recv: HashMap::new(),
            current_height: 0,
            received_packets: HashSet::new(),
            acknowledged_packets: HashSet::new(),
        }
    }

    pub fn config(&self) -> &IbcConfig {
        &self.config
    }

    /// Advance the local height used for timeout checks.
    pub fn set_current_height(&mut self, height: BlockHeight) {
        self.current_height = height;
    }

    pub fn current_height(&self) -> BlockHeight {
        self.current_height
    }

    /// Open a channel in `Init`. It must be confirmed before it carries packets.
    pub fn open_channel(
        &mut self,
        port_id: String,
        counterparty_port: String,
        counterparty_channel: String,
        ordering: ChannelOrdering,
    ) -> Result<String, IbcError> {
        self.ensure_enabled()?;
        if port_id.trim().is_empty() || counterparty_port.trim().is_empty() {
            return Err(IbcError::InvalidChannel("port IDs are required".into()));
        }
        if counterparty_channel.trim().is_empty() {
            return Err(IbcError::InvalidChannel("counterparty channel ID is required".into()));
        }

        let channel_id = format!("channel-{}", self.channels.len());
        self.channels.insert(
            channel_id.clone(),
            IbcChannel {
                channel_id: channel_id.clone(),
                port_id,
                state: ChannelState::Init,
                ordering,
                counterparty_channel_id: counterparty_channel,
                counterparty_port_id: counterparty_port,
            },
        );
        info!(%channel_id, "IBC channel opened in Init");
        Ok(channel_id)
    }

    /// Move a channel through the handshake. Only forward transitions are legal.
    pub fn set_channel_state(
        &mut self,
        channel_id: &str,
        state: ChannelState,
    ) -> Result<(), IbcError> {
        self.ensure_enabled()?;
        let channel = self
            .channels
            .get_mut(channel_id)
            .ok_or_else(|| IbcError::ChannelNotFound(channel_id.to_string()))?;
        let allowed = matches!(
            (channel.state, state),
            (ChannelState::Init, ChannelState::TryOpen | ChannelState::Open)
                | (ChannelState::TryOpen, ChannelState::Open)
                | (
                    ChannelState::Init | ChannelState::TryOpen | ChannelState::Open,
                    ChannelState::Closed
                )
        );
        if !allowed {
            return Err(IbcError::InvalidChannelTransition { from: channel.state, to: state });
        }
        channel.state = state;
        Ok(())
    }

    /// Send a fungible token transfer, returning its sequence number.
    pub fn send_transfer(&mut self, transfer: IbcTransfer) -> Result<u64, IbcError> {
        self.ensure_enabled()?;
        transfer.validate()?;

        let channel = self
            .channels
            .get(&transfer.source_channel)
            .ok_or_else(|| IbcError::ChannelNotFound(transfer.source_channel.clone()))?;
        // Refuse to queue a packet a half-open channel can never deliver.
        if !channel.is_open() {
            return Err(IbcError::ChannelNotOpen {
                channel_id: transfer.source_channel.clone(),
                state: channel.state,
            });
        }
        let destination_channel = channel.counterparty_channel_id.clone();
        let destination_port = channel.counterparty_port_id.clone();
        let source_port = channel.port_id.clone();

        let data = serde_json::to_vec(&transfer)?;
        if data.len() > MAX_PACKET_BYTES {
            return Err(IbcError::PacketTooLarge { provided: data.len(), max: MAX_PACKET_BYTES });
        }

        let sequence = self.next_sequence_send;
        let packet = IbcPacket {
            sequence,
            source_port,
            source_channel: transfer.source_channel.clone(),
            destination_port,
            destination_channel,
            data,
            timeout_height: self.current_height.saturating_add(DEFAULT_TIMEOUT_BLOCKS),
            timeout_timestamp: subhost_core::unix_timestamp()
                .saturating_add(DEFAULT_TIMEOUT_SECONDS),
        };

        self.next_sequence_send = sequence.checked_add(1).ok_or(IbcError::SequenceOverflow)?;
        self.sent_packets.insert(sequence, packet);
        info!(
            sequence,
            amount = transfer.amount,
            token = %transfer.token,
            "IBC transfer queued"
        );
        Ok(sequence)
    }

    /// Accept an inbound packet, enforcing channel binding, timeouts, ordering,
    /// and replay protection.
    pub fn receive_packet(&mut self, packet: IbcPacket) -> Result<IbcTransfer, IbcError> {
        self.ensure_enabled()?;
        if packet.sequence == 0 {
            return Err(IbcError::InvalidPacket("sequence must be >= 1".into()));
        }
        if packet.data.is_empty() {
            return Err(IbcError::InvalidPacket("payload is empty".into()));
        }
        if packet.data.len() > MAX_PACKET_BYTES {
            return Err(IbcError::PacketTooLarge {
                provided: packet.data.len(),
                max: MAX_PACKET_BYTES,
            });
        }

        let channel = self
            .channels
            .get(&packet.destination_channel)
            .ok_or_else(|| IbcError::ChannelNotFound(packet.destination_channel.clone()))?;
        if !matches!(channel.state, ChannelState::Open | ChannelState::TryOpen) {
            return Err(IbcError::ChannelNotOpen {
                channel_id: packet.destination_channel.clone(),
                state: channel.state,
            });
        }
        // The packet must arrive from the counterparty this channel was opened to.
        if channel.counterparty_channel_id != packet.source_channel
            || channel.counterparty_port_id != packet.source_port
            || channel.port_id != packet.destination_port
        {
            return Err(IbcError::InvalidPacket(
                "packet does not match the channel's counterparty binding".into(),
            ));
        }

        // At least one timeout must be set, or the packet never expires.
        if packet.timeout_height == 0 && packet.timeout_timestamp == 0 {
            return Err(IbcError::InvalidPacket("at least one timeout must be set".into()));
        }
        if packet.timeout_height != 0 && packet.timeout_height <= self.current_height {
            return Err(IbcError::PacketTimedOut);
        }
        if packet.timeout_timestamp != 0
            && packet.timeout_timestamp <= subhost_core::unix_timestamp()
        {
            return Err(IbcError::PacketTimedOut);
        }

        // Replay check before any mutation.
        let receipt_key = (packet.source_channel.clone(), packet.sequence);
        if self.received_packets.contains(&receipt_key) {
            return Err(IbcError::Replay);
        }

        let ordering = channel.ordering;
        let expected = self.expected_recv_sequence(&packet.destination_channel);
        if matches!(ordering, ChannelOrdering::Ordered) && packet.sequence != expected {
            return Err(IbcError::InvalidSequence { expected, got: packet.sequence });
        }

        let transfer: IbcTransfer = serde_json::from_slice(&packet.data)?;
        transfer.validate()?;
        // The payload must agree with the envelope it arrived in.
        if transfer.source_channel != packet.source_channel {
            return Err(IbcError::InvalidPacket(
                "transfer source channel does not match the packet".into(),
            ));
        }

        self.received_packets.insert(receipt_key);
        let next = self.next_sequence_recv.entry(packet.destination_channel.clone()).or_insert(1);
        *next = (*next).max(packet.sequence).checked_add(1).ok_or(IbcError::SequenceOverflow)?;

        info!(
            sequence = packet.sequence,
            amount = transfer.amount,
            token = %transfer.token,
            sender = %transfer.sender,
            "IBC transfer received"
        );
        Ok(transfer)
    }

    /// Acknowledge a packet this chain sent. Each sequence may be acked once.
    pub fn acknowledge_packet(&mut self, sequence: u64) -> Result<(), IbcError> {
        self.ensure_enabled()?;
        // Existence is checked before the replay marker so an unknown sequence
        // cannot poison a later legitimate acknowledgement.
        if !self.sent_packets.contains_key(&sequence) {
            return Err(IbcError::UnknownPacket(sequence));
        }
        if !self.acknowledged_packets.insert(sequence) {
            return Err(IbcError::Replay);
        }
        info!(sequence, "IBC packet acknowledged");
        Ok(())
    }

    /// Drop a sent packet whose timeout has passed, freeing its escrow.
    pub fn timeout_packet(&mut self, sequence: u64) -> Result<IbcPacket, IbcError> {
        self.ensure_enabled()?;
        let packet = self.sent_packets.get(&sequence).ok_or(IbcError::UnknownPacket(sequence))?;
        if self.acknowledged_packets.contains(&sequence) {
            return Err(IbcError::InvalidPacket("an acknowledged packet cannot time out".into()));
        }
        let height_expired =
            packet.timeout_height != 0 && packet.timeout_height <= self.current_height;
        let time_expired = packet.timeout_timestamp != 0
            && packet.timeout_timestamp <= subhost_core::unix_timestamp();
        if !height_expired && !time_expired {
            return Err(IbcError::PacketNotExpired);
        }
        // Safe to unwrap: existence was checked above and nothing removed it.
        Ok(self.sent_packets.remove(&sequence).expect("the packet was present in this scope"))
    }

    pub fn channel(&self, channel_id: &str) -> Option<&IbcChannel> {
        self.channels.get(channel_id)
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn sent_packet(&self, sequence: u64) -> Option<&IbcPacket> {
        self.sent_packets.get(&sequence)
    }

    pub fn is_acknowledged(&self, sequence: u64) -> bool {
        self.acknowledged_packets.contains(&sequence)
    }

    pub fn has_received(&self, source_channel: &str, sequence: u64) -> bool {
        self.received_packets.contains(&(source_channel.to_string(), sequence))
    }

    pub fn next_send_sequence(&self) -> u64 {
        self.next_sequence_send
    }

    /// Next sequence an ordered channel expects to receive.
    pub fn expected_recv_sequence(&self, channel_id: &str) -> u64 {
        self.next_sequence_recv.get(channel_id).copied().unwrap_or(1)
    }

    fn ensure_enabled(&self) -> Result<(), IbcError> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(IbcError::Disabled)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IbcError {
    #[error("IBC is disabled")]
    Disabled,

    #[error("channel {0} not found")]
    ChannelNotFound(String),

    #[error("channel {channel_id} is not open (state {state:?})")]
    ChannelNotOpen { channel_id: String, state: ChannelState },

    #[error("invalid channel transition from {from:?} to {to:?}")]
    InvalidChannelTransition { from: ChannelState, to: ChannelState },

    #[error("invalid channel: {0}")]
    InvalidChannel(String),

    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    #[error("invalid transfer: {0}")]
    InvalidTransfer(String),

    #[error("packet is {provided} bytes, above the {max} byte limit")]
    PacketTooLarge { provided: usize, max: usize },

    #[error("packet timed out")]
    PacketTimedOut,

    #[error("packet has not expired yet")]
    PacketNotExpired,

    #[error("packet replay detected")]
    Replay,

    #[error("unknown packet sequence: {0}")]
    UnknownPacket(u64),

    #[error("invalid packet sequence: expected {expected}, got {got}")]
    InvalidSequence { expected: u64, got: u64 },

    #[error("packet sequence overflow")]
    SequenceOverflow,

    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(source_channel: &str) -> IbcTransfer {
        IbcTransfer {
            sender: Address::new([1; 20]),
            receiver: "cosmos1receiver".to_string(),
            token: "SUB".to_string(),
            amount: 100,
            source_channel: source_channel.to_string(),
        }
    }

    /// A module with one open channel `channel-0` bound to `counterparty-0`.
    fn module_with_open_channel(ordering: ChannelOrdering) -> IbcModule {
        let mut module = IbcModule::new(IbcConfig::default());
        let channel_id = module
            .open_channel(
                "transfer".to_string(),
                "transfer".to_string(),
                "counterparty-0".to_string(),
                ordering,
            )
            .unwrap();
        module.set_channel_state(&channel_id, ChannelState::Open).unwrap();
        module
    }

    fn inbound(sequence: u64) -> IbcPacket {
        IbcPacket {
            sequence,
            source_port: "transfer".to_string(),
            source_channel: "counterparty-0".to_string(),
            destination_port: "transfer".to_string(),
            destination_channel: "channel-0".to_string(),
            data: serde_json::to_vec(&transfer("counterparty-0")).unwrap(),
            timeout_height: 1_000,
            timeout_timestamp: Timestamp::MAX,
        }
    }

    #[test]
    fn channel_opens_in_init_and_requires_confirmation() {
        let mut module = IbcModule::new(IbcConfig::default());
        let channel_id = module
            .open_channel(
                "transfer".to_string(),
                "transfer".to_string(),
                "counterparty-0".to_string(),
                ChannelOrdering::Unordered,
            )
            .unwrap();
        assert_eq!(channel_id, "channel-0");
        assert_eq!(module.channel(&channel_id).unwrap().state, ChannelState::Init);
        assert!(!module.channel(&channel_id).unwrap().is_open());
        assert_eq!(module.channel_count(), 1);

        // A half-open channel must not carry a transfer.
        assert!(matches!(
            module.send_transfer(transfer(&channel_id)),
            Err(IbcError::ChannelNotOpen { .. })
        ));

        module.set_channel_state(&channel_id, ChannelState::Open).unwrap();
        assert!(module.send_transfer(transfer(&channel_id)).is_ok());
    }

    #[test]
    fn channel_transitions_only_move_forward() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        assert!(matches!(
            module.set_channel_state("channel-0", ChannelState::Init),
            Err(IbcError::InvalidChannelTransition { .. })
        ));
        assert!(matches!(
            module.set_channel_state("channel-0", ChannelState::TryOpen),
            Err(IbcError::InvalidChannelTransition { .. })
        ));
        module.set_channel_state("channel-0", ChannelState::Closed).unwrap();
        assert!(matches!(
            module.set_channel_state("channel-0", ChannelState::Open),
            Err(IbcError::InvalidChannelTransition { .. })
        ));
        assert!(matches!(
            module.set_channel_state("channel-9", ChannelState::Open),
            Err(IbcError::ChannelNotFound(_))
        ));
    }

    #[test]
    fn open_channel_rejects_empty_identifiers() {
        let mut module = IbcModule::new(IbcConfig::default());
        for (port, counterparty_port, counterparty_channel) in [
            ("", "transfer", "counterparty-0"),
            ("transfer", "", "counterparty-0"),
            ("transfer", "transfer", "  "),
        ] {
            assert!(matches!(
                module.open_channel(
                    port.to_string(),
                    counterparty_port.to_string(),
                    counterparty_channel.to_string(),
                    ChannelOrdering::Unordered,
                ),
                Err(IbcError::InvalidChannel(_))
            ));
        }
        assert_eq!(module.channel_count(), 0);
    }

    #[test]
    fn send_transfer_assigns_sequences_and_sets_timeouts() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        module.set_current_height(50);

        assert_eq!(module.send_transfer(transfer("channel-0")).unwrap(), 1);
        assert_eq!(module.send_transfer(transfer("channel-0")).unwrap(), 2);
        assert_eq!(module.next_send_sequence(), 3);

        let packet = module.sent_packet(1).unwrap();
        assert_eq!(packet.destination_channel, "counterparty-0");
        assert_eq!(packet.timeout_height, 50 + DEFAULT_TIMEOUT_BLOCKS);
        assert!(packet.timeout_timestamp > subhost_core::unix_timestamp());
    }

    #[test]
    fn send_transfer_validates_the_payload_and_channel() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        assert!(matches!(
            module.send_transfer(transfer("channel-9")),
            Err(IbcError::ChannelNotFound(_))
        ));
        for broken in [
            IbcTransfer { amount: 0, ..transfer("channel-0") },
            IbcTransfer { receiver: "  ".to_string(), ..transfer("channel-0") },
            IbcTransfer { token: String::new(), ..transfer("channel-0") },
        ] {
            assert!(matches!(module.send_transfer(broken), Err(IbcError::InvalidTransfer(_))));
        }
        assert_eq!(module.next_send_sequence(), 1, "no sequence was consumed");
    }

    #[test]
    fn receive_accepts_a_valid_packet_and_rejects_the_replay() {
        let mut module = module_with_open_channel(ChannelOrdering::Ordered);
        let received = module.receive_packet(inbound(1)).unwrap();
        assert_eq!(received.amount, 100);
        assert!(module.has_received("counterparty-0", 1));
        assert_eq!(module.expected_recv_sequence("channel-0"), 2);

        assert!(matches!(module.receive_packet(inbound(1)), Err(IbcError::Replay)));
    }

    #[test]
    fn ordered_channels_reject_out_of_order_sequences() {
        let mut module = module_with_open_channel(ChannelOrdering::Ordered);
        assert!(matches!(
            module.receive_packet(inbound(2)),
            Err(IbcError::InvalidSequence { expected: 1, got: 2 })
        ));
        // The failed attempt must not have consumed the sequence.
        assert!(!module.has_received("counterparty-0", 2));
        module.receive_packet(inbound(1)).unwrap();
        module.receive_packet(inbound(2)).unwrap();
        assert_eq!(module.expected_recv_sequence("channel-0"), 3);
    }

    #[test]
    fn unordered_channels_accept_gaps() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        module.receive_packet(inbound(5)).unwrap();
        module.receive_packet(inbound(2)).unwrap();
        assert!(module.has_received("counterparty-0", 5));
        assert!(module.has_received("counterparty-0", 2));
        assert!(matches!(module.receive_packet(inbound(5)), Err(IbcError::Replay)));
    }

    #[test]
    fn receive_rejects_wrong_counterparty_bindings() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        for broken in [
            IbcPacket { source_channel: "attacker".to_string(), ..inbound(1) },
            IbcPacket { source_port: "attacker".to_string(), ..inbound(1) },
            IbcPacket { destination_port: "attacker".to_string(), ..inbound(1) },
        ] {
            assert!(matches!(module.receive_packet(broken), Err(IbcError::InvalidPacket(_))));
        }
        assert!(matches!(
            module.receive_packet(IbcPacket {
                destination_channel: "channel-9".to_string(),
                ..inbound(1)
            }),
            Err(IbcError::ChannelNotFound(_))
        ));
    }

    #[test]
    fn receive_rejects_expired_and_timeout_less_packets() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);

        module.set_current_height(1_000);
        assert!(matches!(module.receive_packet(inbound(1)), Err(IbcError::PacketTimedOut)));

        module.set_current_height(0);
        assert!(matches!(
            module.receive_packet(IbcPacket { timeout_timestamp: 1, ..inbound(1) }),
            Err(IbcError::PacketTimedOut)
        ));
        assert!(matches!(
            module.receive_packet(IbcPacket {
                timeout_height: 0,
                timeout_timestamp: 0,
                ..inbound(1)
            }),
            Err(IbcError::InvalidPacket(_))
        ));
    }

    #[test]
    fn receive_rejects_malformed_payloads() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        assert!(matches!(
            module.receive_packet(IbcPacket { sequence: 0, ..inbound(1) }),
            Err(IbcError::InvalidPacket(_))
        ));
        assert!(matches!(
            module.receive_packet(IbcPacket { data: Vec::new(), ..inbound(1) }),
            Err(IbcError::InvalidPacket(_))
        ));
        assert!(matches!(
            module.receive_packet(IbcPacket { data: vec![0; MAX_PACKET_BYTES + 1], ..inbound(1) }),
            Err(IbcError::PacketTooLarge { .. })
        ));
        assert!(matches!(
            module.receive_packet(IbcPacket { data: b"not json".to_vec(), ..inbound(1) }),
            Err(IbcError::Serialization(_))
        ));
        // A payload whose channel disagrees with the envelope is refused.
        assert!(matches!(
            module.receive_packet(IbcPacket {
                data: serde_json::to_vec(&transfer("somewhere-else")).unwrap(),
                ..inbound(1)
            }),
            Err(IbcError::InvalidPacket(_))
        ));
        // A payload that fails transfer validation is refused.
        assert!(matches!(
            module.receive_packet(IbcPacket {
                data: serde_json::to_vec(&IbcTransfer { amount: 0, ..transfer("counterparty-0") })
                    .unwrap(),
                ..inbound(1)
            }),
            Err(IbcError::InvalidTransfer(_))
        ));
    }

    #[test]
    fn acknowledgement_checks_existence_before_the_replay_marker() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        assert!(matches!(module.acknowledge_packet(1), Err(IbcError::UnknownPacket(1))));

        let sequence = module.send_transfer(transfer("channel-0")).unwrap();
        module.acknowledge_packet(sequence).unwrap();
        assert!(module.is_acknowledged(sequence));
        assert!(matches!(module.acknowledge_packet(sequence), Err(IbcError::Replay)));
    }

    #[test]
    fn timeout_releases_only_expired_unacknowledged_packets() {
        let mut module = module_with_open_channel(ChannelOrdering::Unordered);
        let sequence = module.send_transfer(transfer("channel-0")).unwrap();

        assert!(matches!(module.timeout_packet(sequence), Err(IbcError::PacketNotExpired)));
        assert!(matches!(module.timeout_packet(99), Err(IbcError::UnknownPacket(99))));

        // An acknowledged packet cannot also time out.
        module.acknowledge_packet(sequence).unwrap();
        module.set_current_height(DEFAULT_TIMEOUT_BLOCKS + 1);
        assert!(matches!(module.timeout_packet(sequence), Err(IbcError::InvalidPacket(_))));

        let second = module.send_transfer(transfer("channel-0")).unwrap();
        module.set_current_height(module.current_height() + DEFAULT_TIMEOUT_BLOCKS + 1);
        let timed_out = module.timeout_packet(second).unwrap();
        assert_eq!(timed_out.sequence, second);
        assert!(module.sent_packet(second).is_none());
    }

    #[test]
    fn every_operation_is_refused_when_disabled() {
        let mut module = IbcModule::new(IbcConfig { enabled: false, ..Default::default() });
        assert!(matches!(
            module.open_channel(
                "transfer".to_string(),
                "transfer".to_string(),
                "counterparty-0".to_string(),
                ChannelOrdering::Unordered,
            ),
            Err(IbcError::Disabled)
        ));
        assert!(matches!(module.send_transfer(transfer("channel-0")), Err(IbcError::Disabled)));
        assert!(matches!(module.receive_packet(inbound(1)), Err(IbcError::Disabled)));
        assert!(matches!(module.acknowledge_packet(1), Err(IbcError::Disabled)));
        assert!(matches!(module.timeout_packet(1), Err(IbcError::Disabled)));
        assert!(matches!(
            module.set_channel_state("channel-0", ChannelState::Open),
            Err(IbcError::Disabled)
        ));
    }

    #[test]
    fn receive_sequences_are_tracked_per_channel() {
        let mut module = IbcModule::new(IbcConfig::default());
        for counterparty in ["counterparty-0", "counterparty-1"] {
            let channel = module
                .open_channel(
                    "transfer".to_string(),
                    "transfer".to_string(),
                    counterparty.to_string(),
                    ChannelOrdering::Ordered,
                )
                .unwrap();
            module.set_channel_state(&channel, ChannelState::Open).unwrap();
        }

        module.receive_packet(inbound(1)).unwrap();
        assert_eq!(module.expected_recv_sequence("channel-0"), 2);
        // The second channel still expects sequence 1: state is not shared.
        assert_eq!(module.expected_recv_sequence("channel-1"), 1);

        let second_channel = IbcPacket {
            source_channel: "counterparty-1".to_string(),
            destination_channel: "channel-1".to_string(),
            data: serde_json::to_vec(&transfer("counterparty-1")).unwrap(),
            ..inbound(1)
        };
        module.receive_packet(second_channel).unwrap();
        assert_eq!(module.expected_recv_sequence("channel-1"), 2);
    }
}
