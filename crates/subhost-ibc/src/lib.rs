use serde::{Serialize, Deserialize};
use tracing::{info, debug};
use std::collections::HashMap;
use subhost_core::{Address, Hash};

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
        }
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
            timeout_timestamp: chrono::Utc::now().timestamp() as u64 + 600,
        };
        
        let seq = self.next_sequence_send;
        self.packets.insert(seq, packet);
        self.next_sequence_send += 1;
        
        info!("Sent IBC transfer: seq={}, amount={} {}", seq, transfer.amount, transfer.token);
        
        Ok(seq)
    }
    
    pub fn receive_packet(&mut self, packet: IbcPacket) -> Result<(), IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        let transfer: IbcTransfer = serde_json::from_slice(&packet.data)?;
        
        info!("Received IBC transfer: {} {} from {}", transfer.amount, transfer.token, transfer.sender);
        
        self.packets.insert(packet.sequence, packet.clone());
        self.next_sequence_recv = std::cmp::max(self.next_sequence_recv, packet.sequence + 1);
        
        Ok(())
    }
    
    pub fn acknowledge_packet(&mut self, sequence: u64) -> Result<(), IbcError> {
        if !self.config.enabled {
            return Err(IbcError::Disabled);
        }
        
        if let Some(packet) = self.packets.get(&sequence) {
            info!("Acknowledged IBC packet: seq={}", packet.sequence);
            self.next_sequence_ack = std::cmp::max(self.next_sequence_ack, sequence + 1);
        }
        
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
}
