use serde::{Serialize, Deserialize};
use tracing::{info, debug};
use std::collections::{HashMap, HashSet};
use subhost_core::Address;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub struct IbcModule {
    config: IbcConfig,
    channels: HashMap<u64, IbcChannel>,
    packets: HashMap<u64, IbcPacket>,
    next_sequence_send: u64,
    next_sequence_recv: u64,
    next_sequence_ack: u64,
    current_height: u64,
    received_packets: HashSet<(String, u64)>,
    acknowledged_packets: HashSet<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbcChannel {
    pub channel_id: String,
    pub port_id: String,
    pub state: ChannelState,
    pub ordering: ChannelOrdering,
    pub counterparty_channel_id: String,
    pub counterparty_port_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelState {
    Uninitialized,
    Init,
    TryOpen,
    Open,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelOrdering {
    Unordered,
    Ordered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbcPacket {
    pub sequence: u64,
    pub source_port: String,
    pub source_channel: String,
    pub destination_port: String,
    pub destination_channel: String,
    pub data: Vec<u8>,
    pub timeout_height: u64,
    pub timeout_timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbcTransfer {
    pub sender: Address,
    pub receiver: String,
    pub token: String,
    pub amount: u128,
    pub source_channel: String,
}

impl IbcModule {
    pub fn new(config: IbcConfig) -> Self {
        info!("Initializing IBC Module");
        Self {
            config,
            channels: HashMap::new(),
            packets: HashMap::new(),
            next_sequence_send: 1,
            next_sequence_recv: 1,
            next_sequence_ack: 1,
            current_height: 0,
            received_packets: HashSet::new(),
            acknowledged_packets: HashSet::new(),
        }
    }

    pub fn set_current_height(&mut self, height: u64) {
        self.current_height = height;
    }
    
    pub fn open_channel(
        &mut self,
        port_id: String,
        counterparty_port: String,
        counterparty_channel: String,
        ordering: ChannelOrdering,
    ) -> Result<String, IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        let channel_id = format!("channel-{}", self.channels.len());
        let channel = IbcChannel {
            channel_id: channel_id.clone(),
            port_id,
            state: ChannelState::Init,
            ordering,
            counterparty_channel_id: counterparty_channel,
            counterparty_port_id: counterparty_port,
        };
        
        self.channels.insert(self.channels.len() as u64, channel);
        info!("Opened IBC channel: {}", channel_id);
        
        Ok(channel_id)
    }
    
    pub fn send_transfer(&mut self, transfer: IbcTransfer) -> Result<u64, IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        let packet = IbcPacket {
            sequence: self.next_sequence_send,
            source_port: "transfer".to_string(),
            source_channel: transfer.source_channel.clone(),
            destination_port: "transfer".to_string(),
            destination_channel: self.get_counterparty_channel(&transfer.source_channel)?,
            data: serde_json::to_vec(&transfer)?,
            timeout_height: 1000,
            timeout_timestamp: chrono::Utc::now().timestamp().max(0) as u64 + 600,
        };

        let seq = self.next_sequence_send;
        if packet.data.len() > 128 * 1024 {
            return Err(IbcError::InvalidPacket);
        }
        self.packets.insert(seq, packet);
        self.next_sequence_send = self
            .next_sequence_send
            .checked_add(1)
            .ok_or(IbcError::SequenceOverflow)?;
        
        info!("Sent IBC transfer: seq={}, amount={} {}", seq, transfer.amount, transfer.token);
        
        Ok(seq)
    }
    
    pub fn receive_packet(&mut self, packet: IbcPacket) -> Result<(), IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        if packet.data.len() > 128 * 1024 || packet.sequence == 0 {
            return Err(IbcError::InvalidPacket);
        }
        let channel = self
            .channels
            .values()
            .find(|channel| channel.channel_id == packet.destination_channel)
            .ok_or(IbcError::ChannelNotFound)?;
        if !matches!(channel.state, ChannelState::Open | ChannelState::TryOpen) {
            return Err(IbcError::InvalidPacket);
        }
        if channel.counterparty_channel_id != packet.source_channel
            || channel.counterparty_port_id != packet.source_port
            || channel.port_id != packet.destination_port
        {
            return Err(IbcError::InvalidPacket);
        }
        if packet.timeout_height == 0 && packet.timeout_timestamp == 0 {
            return Err(IbcError::InvalidPacket);
        }
        if packet.timeout_height != 0 && packet.timeout_height <= self.current_height {
            return Err(IbcError::PacketTimedOut);
        }
        if packet.timeout_timestamp != 0
            && packet.timeout_timestamp <= chrono::Utc::now().timestamp().max(0) as u64
        {
            return Err(IbcError::PacketTimedOut);
        }
        if self
            .received_packets
            .contains(&(packet.source_channel.clone(), packet.sequence))
        {
            return Err(IbcError::Replay);
        }
        if matches!(channel.ordering, ChannelOrdering::Ordered)
            && packet.sequence != self.next_sequence_recv
        {
            return Err(IbcError::InvalidSequence {
                expected: self.next_sequence_recv,
                got: packet.sequence,
            });
        }

        let transfer: IbcTransfer = serde_json::from_slice(&packet.data)?;
        if transfer.source_channel != packet.source_channel {
            return Err(IbcError::InvalidPacket);
        }

        self.received_packets
            .insert((packet.source_channel.clone(), packet.sequence));

        info!("Received IBC transfer: {} {} from {}", transfer.amount, transfer.token, transfer.sender);

        self.packets.insert(packet.sequence, packet.clone());
        self.next_sequence_recv = self
            .next_sequence_recv
            .max(packet.sequence)
            .checked_add(1)
            .ok_or(IbcError::SequenceOverflow)?;
        
        Ok(())
    }
    
    pub fn acknowledge_packet(&mut self, sequence: u64) -> Result<(), IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        if !self.packets.contains_key(&sequence) {
            return Err(IbcError::InvalidPacket);
        }
        if !self.acknowledged_packets.insert(sequence) {
            return Err(IbcError::Replay);
        }
        info!("Acknowledged IBC packet: seq={sequence}");
        self.next_sequence_ack = self
            .next_sequence_ack
            .max(sequence)
            .checked_add(1)
            .ok_or(IbcError::SequenceOverflow)?;
        
        Ok(())
    }
    
    pub fn query_channel(&self, channel_id: &str) -> Option<&IbcChannel> {
        self.channels.values().find(|c| c.channel_id == channel_id)
    }
    
    pub fn query_packet(&self, sequence: u64) -> Option<&IbcPacket> {
        self.packets.get(&sequence)
    }
    
    fn get_counterparty_channel(&self, source_channel: &str) -> Result<String, IbcError> {
        self.channels
            .values()
            .find(|c| c.channel_id == source_channel)
            .map(|c| c.counterparty_channel_id.clone())
            .ok_or(IbcError::ChannelNotFound)
    }
    
    pub fn process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.config.enabled {
            return Ok(());
        }
        debug!("Processing IBC module");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IbcError {
    #[error("IBC is disabled")]
    Disabled,
    #[error("Channel not found")]
    ChannelNotFound,
    #[error("Invalid packet")]
    InvalidPacket,
    #[error("Packet timed out")]
    PacketTimedOut,
    #[error("Packet replay detected")]
    Replay,
    #[error("Invalid packet sequence: expected {expected}, got {got}")]
    InvalidSequence { expected: u64, got: u64 },
    #[error("Packet sequence overflow")]
    SequenceOverflow,
    #[error("Serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Processing error: {0}")]
    Processing(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = IbcConfig::default();
        assert!(config.enabled);
    }
    
    #[test]
    fn test_open_channel() {
        let config = IbcConfig::default();
        let mut module = IbcModule::new(config);
        let channel_id = module.open_channel(
            "transfer".to_string(),
            "transfer".to_string(),
            "channel-0".to_string(),
            ChannelOrdering::Unordered,
        ).unwrap();
        assert!(channel_id.starts_with("channel-"));
    }

    fn packet(sequence: u64, channel: &str) -> IbcPacket {
        IbcPacket {
            sequence,
            source_port: "transfer".to_string(),
            source_channel: "counterparty-0".to_string(),
            destination_port: "transfer".to_string(),
            destination_channel: channel.to_string(),
            data: serde_json::to_vec(&IbcTransfer {
                sender: Address::new([1; 20]),
                receiver: "receiver".to_string(),
                token: "SUB".to_string(),
                amount: 1,
                source_channel: "counterparty-0".to_string(),
            }).unwrap(),
            timeout_height: 100,
            timeout_timestamp: u64::MAX,
        }
    }

    #[test]
    fn receive_rejects_replay_without_poisoning_invalid_packet() {
        let mut module = IbcModule::new(IbcConfig::default());
        module.open_channel(
            "transfer".to_string(),
            "transfer".to_string(),
            "counterparty-0".to_string(),
            ChannelOrdering::Ordered,
        ).unwrap();
        module.channels.get_mut(&0).unwrap().state = ChannelState::Open;

        let mut invalid = packet(2, "channel-0");
        assert!(matches!(module.receive_packet(invalid.clone()), Err(IbcError::InvalidSequence { .. })));
        invalid.sequence = 1;
        module.receive_packet(invalid.clone()).unwrap();
        assert!(matches!(module.receive_packet(invalid), Err(IbcError::Replay)));
    }

    #[test]
    fn acknowledge_checks_existence_before_replay_marker() {
        let mut module = IbcModule::new(IbcConfig::default());
        assert!(matches!(module.acknowledge_packet(1), Err(IbcError::InvalidPacket)));
        module.open_channel(
            "transfer".to_string(),
            "transfer".to_string(),
            "counterparty-0".to_string(),
            ChannelOrdering::Unordered,
        ).unwrap();
        module.send_transfer(IbcTransfer {
            sender: Address::new([1; 20]),
            receiver: "receiver".to_string(),
            token: "SUB".to_string(),
            amount: 1,
            source_channel: "channel-0".to_string(),
        }).unwrap();
        module.acknowledge_packet(1).unwrap();
        assert!(matches!(module.acknowledge_packet(1), Err(IbcError::Replay)));
    }

    #[test]
    fn receive_rejects_wrong_counterparty_and_expired_packet() {
        let mut module = IbcModule::new(IbcConfig::default());
        module.open_channel(
            "transfer".to_string(),
            "transfer".to_string(),
            "counterparty-0".to_string(),
            ChannelOrdering::Unordered,
        ).unwrap();
        module.channels.get_mut(&0).unwrap().state = ChannelState::Open;

        let mut wrong_channel = packet(1, "channel-0");
        wrong_channel.source_channel = "attacker-channel".to_string();
        assert!(matches!(module.receive_packet(wrong_channel), Err(IbcError::InvalidPacket)));

        let expired = packet(1, "channel-0");
        module.set_current_height(100);
        assert!(matches!(module.receive_packet(expired), Err(IbcError::PacketTimedOut)));
    }
}
