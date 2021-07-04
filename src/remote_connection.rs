use crate::channel::{Channel, ChannelPacketData};
use crate::error::RenetError;
use crate::packet::{AckData, Connection, HeartBeat, Normal, Packet};
use crate::protocol::SecurityService;
use crate::reassembly_fragment::{build_fragments, FragmentConfig, ReassemblyFragment};
use crate::sequence_buffer::SequenceBuffer;
use crate::Timer;

use log::{debug, error};

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub type ClientId = u64;

#[derive(Debug, Clone)]
struct SentPacket {
    time: Instant,
    ack: bool,
    size_bytes: usize,
}

impl SentPacket {
    fn new(time: Instant, size_bytes: usize) -> Self {
        Self {
            time,
            size_bytes,
            ack: false,
        }
    }
}

#[derive(Debug, Clone)]
struct ReceivedPacket {
    time: Instant,
    size_bytes: usize,
}

impl ReceivedPacket {
    fn new(time: Instant, size_bytes: usize) -> Self {
        Self { time, size_bytes }
    }
}

#[derive(Debug)]
pub struct NetworkInfo {
    pub rtt: f64,
    pub sent_bandwidth_kbps: f64,
    pub received_bandwidth_kbps: f64,
    pub packet_loss: f64,
}

impl Default for NetworkInfo {
    fn default() -> Self {
        Self {
            rtt: 0.,
            sent_bandwidth_kbps: 0.,
            received_bandwidth_kbps: 0.,
            packet_loss: 0.,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub max_packet_size: usize,
    pub sent_packets_buffer_size: usize,
    pub received_packets_buffer_size: usize,
    pub measure_smoothing_factor: f64,
    pub timeout_duration: Duration,
    pub heartbeat_time: Duration,
    pub fragment_config: FragmentConfig,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            max_packet_size: 16 * 1024,
            sent_packets_buffer_size: 256,
            received_packets_buffer_size: 256,
            measure_smoothing_factor: 0.05,
            timeout_duration: Duration::from_secs(5),
            heartbeat_time: Duration::from_millis(100),
            fragment_config: FragmentConfig::default(),
        }
    }
}

pub struct RemoteConnection<S> {
    sequence: u16,
    addr: SocketAddr,
    channels: HashMap<u8, Box<dyn Channel>>,
    security_service: S,
    heartbeat_timer: Timer,
    timeout_timer: Timer,
    config: ConnectionConfig,
    reassembly_buffer: SequenceBuffer<ReassemblyFragment>,
    sent_buffer: SequenceBuffer<SentPacket>,
    received_buffer: SequenceBuffer<ReceivedPacket>,
    acks: Vec<u16>,
    network_info: NetworkInfo,
}

impl<S: SecurityService> RemoteConnection<S> {
    pub fn new(server_addr: SocketAddr, config: ConnectionConfig, security_service: S) -> Self {
        let timeout_timer = Timer::new(config.timeout_duration);
        let heartbeat_timer = Timer::new(config.heartbeat_time);
        let reassembly_buffer =
            SequenceBuffer::with_capacity(config.fragment_config.reassembly_buffer_size);
        let sent_buffer = SequenceBuffer::with_capacity(config.sent_packets_buffer_size);
        let received_buffer = SequenceBuffer::with_capacity(config.received_packets_buffer_size);

        Self {
            channels: HashMap::new(),
            addr: server_addr,
            security_service,
            timeout_timer,
            heartbeat_timer,
            sequence: 0,
            reassembly_buffer,
            sent_buffer,
            received_buffer,
            config,
            acks: vec![],
            network_info: NetworkInfo::default(),
        }
    }

    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    pub fn add_channel(&mut self, channel_id: u8, channel: Box<dyn Channel>) {
        self.channels.insert(channel_id, channel);
    }

    pub fn has_timed_out(&mut self) -> bool {
        self.timeout_timer.is_finished()
    }

    pub fn send_message(&mut self, channel_id: u8, message: Box<[u8]>) {
        let channel = self
            .channels
            .get_mut(&channel_id)
            .expect("Sending message to invalid channel");
        channel.send_message(message);
    }

    // TODO: Make into_bytes for packets
    pub fn build_heartbeat_packet(&self) -> Result<Vec<u8>, RenetError> {
        let (ack, ack_bits) = self.received_buffer.ack_bits();
        let packet = Packet::Heartbeat(HeartBeat {
            ack_data: AckData { ack, ack_bits },
        });

        let packet = bincode::serialize(&packet).map_err(|_| RenetError::SerializationFailed)?;
        Ok(packet)
    }

    pub fn process_payload(&mut self, payload: &[u8]) -> Result<(), RenetError> {
        self.timeout_timer.reset();
        let payload = self.security_service.ss_unwrap(payload)?;
        let packet = bincode::deserialize(&payload).map_err(|_| RenetError::SerializationFailed)?;
        let payload = match packet {
            Packet::Normal(Normal {
                sequence,
                ack_data,
                payload,
            }) => {
                let received_packet = ReceivedPacket::new(Instant::now(), payload.len());
                self.received_buffer.insert(sequence, received_packet);
                self.update_acket_packets(ack_data.ack, ack_data.ack_bits);
                Some(payload)
            }
            Packet::Fragment(fragment) => {
                if let Some(received_packet) = self.received_buffer.get_mut(fragment.sequence) {
                    received_packet.size_bytes += payload.len();
                } else {
                    let received_packet = ReceivedPacket::new(Instant::now(), payload.len());
                    self.received_buffer
                        .insert(fragment.sequence, received_packet);
                }

                self.update_acket_packets(fragment.ack_data.ack, fragment.ack_data.ack_bits);

                self.reassembly_buffer
                    .handle_fragment(fragment, &self.config.fragment_config)?
            }
            Packet::Heartbeat(HeartBeat { ack_data }) => {
                self.update_acket_packets(ack_data.ack, ack_data.ack_bits);
                None
            }
            Packet::Connection(Connection { error, .. }) => {
                if let Some(error) = error {
                    return Err(RenetError::ConnectionError(error));
                }
                None
            }
        };

        for ack in self.acks.drain(..) {
            for channel in self.channels.values_mut() {
                channel.process_ack(ack);
            }
        }

        let payload = match payload {
            Some(payload) => payload,
            None => return Ok(()),
        };

        // TODO: should Vec<ChannelPacketData> be inside packet instead of payload?
        let mut channel_packets = match bincode::deserialize::<Vec<ChannelPacketData>>(&payload) {
            Ok(x) => x,
            Err(e) => {
                error!("Failed to deserialize ChannelPacketData: {:?}", e);
                return Err(RenetError::SerializationFailed);
            }
        };

        for channel_packet_data in channel_packets.drain(..) {
            let channel = match self.channels.get_mut(&channel_packet_data.channel_id) {
                Some(c) => c,
                None => {
                    error!(
                        "Received channel packet with invalid id: {:?}",
                        channel_packet_data.channel_id
                    );
                    continue;
                }
            };
            channel.process_messages(channel_packet_data.messages);
        }

        Ok(())
    }

    pub fn send_payload(&mut self, payload: &[u8], socket: &UdpSocket) -> Result<(), RenetError> {
        let reliable_packets = self.generate_packets(payload)?;
        for reliable_packet in reliable_packets.iter() {
            let payload = self.security_service.ss_wrap(&reliable_packet).unwrap();
            socket.send_to(&payload, self.addr)?;
        }
        Ok(())
    }

    pub fn generate_packets(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, RenetError> {
        if payload.len() > self.config.max_packet_size {
            error!(
                "Packet to large to send, maximum is {} got {}.",
                self.config.max_packet_size,
                payload.len()
            );
            return Err(RenetError::MaximumPacketSizeExceeded);
        }

        let sequence = self.sequence;
        self.sequence += 1;

        let (ack, ack_bits) = self.received_buffer.ack_bits();
        // TODO: add header size
        let sent_packet = SentPacket::new(Instant::now(), payload.len());
        self.sent_buffer.insert(sequence, sent_packet);
        if payload.len() > self.config.fragment_config.fragment_above {
            // Fragment packet
            debug!("Sending fragmented packet {}.", sequence);
            Ok(build_fragments(
                payload,
                sequence,
                AckData { ack, ack_bits },
                &self.config.fragment_config,
            )?)
        } else {
            // Normal packet
            debug!("Sending normal packet {}.", sequence);
            let packet = Packet::Normal(Normal {
                payload: payload.to_vec(),
                sequence,
                ack_data: AckData { ack, ack_bits },
            });
            let packet =
                bincode::serialize(&packet).map_err(|_| RenetError::SerializationFailed)?;
            Ok(vec![packet])
        }
    }

    fn update_acket_packets(&mut self, ack: u16, ack_bits: u32) {
        let mut ack_bits = ack_bits;
        let now = Instant::now();
        for i in 0..32 {
            if ack_bits & 1 != 0 {
                let ack_sequence = ack.wrapping_sub(i);
                if let Some(ref mut sent_packet) = self.sent_buffer.get_mut(ack_sequence) {
                    if !sent_packet.ack {
                        debug!("Acked packet {}.", ack_sequence);
                        self.acks.push(ack_sequence);
                        sent_packet.ack = true;
                        let rtt = (now - sent_packet.time).as_secs_f64();
                        if self.network_info.rtt == 0.0 && rtt > 0.0
                            || f64::abs(self.network_info.rtt - rtt) < 0.00001
                        {
                            self.network_info.rtt = rtt;
                        } else {
                            self.network_info.rtt += (rtt - self.network_info.rtt)
                                * self.config.measure_smoothing_factor;
                        }
                    }
                }
            }
            ack_bits >>= 1;
        }
    }

    pub fn send_packets(&mut self, socket: &UdpSocket) -> Result<(), RenetError> {
        if let Some(payload) = self.get_packet()? {
            self.heartbeat_timer.reset();
            self.send_payload(&payload, socket).unwrap();
        } else if self.heartbeat_timer.is_finished() {
            self.heartbeat_timer.reset();
            let packet = self.build_heartbeat_packet().unwrap();
            let payload = self.security_service.ss_wrap(&packet).unwrap();
            socket.send_to(&payload, self.addr).unwrap();
        }
        Ok(())
    }

    pub fn get_packet(&mut self) -> Result<Option<Box<[u8]>>, RenetError> {
        let sequence = self.sequence;
        let mut channel_packets: Vec<ChannelPacketData> = vec![];
        for (channel_id, channel) in self.channels.iter_mut() {
            let messages =
                channel.get_messages_to_send(Some(self.config.max_packet_size as u32), sequence);
            if let Some(messages) = messages {
                debug!("Sending {} messages.", messages.len());
                let packet_data = ChannelPacketData::new(messages, *channel_id);
                channel_packets.push(packet_data);
            }
        }

        if channel_packets.is_empty() {
            return Ok(None);
        }

        let payload = match bincode::serialize(&channel_packets) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize Vec<ChannelPacketData>: {:?}", e);
                return Err(RenetError::SerializationFailed);
            }
        };

        Ok(Some(payload.into_boxed_slice()))
    }

    pub fn receive_message(&mut self, channel_id: u8) -> Option<Box<[u8]>> {
        let channel = match self.channels.get_mut(&channel_id) {
            Some(c) => c,
            None => {
                error!(
                    "Tried to receive message from invalid channel {}.",
                    channel_id
                );
                return None;
            }
        };

        channel.receive_message()
    }

    pub fn update_network_info(&mut self) {
        self.update_sent_bandwidth();
        self.update_received_bandwidth();
    }

    fn update_sent_bandwidth(&mut self) {
        let sample_size = self.config.sent_packets_buffer_size / 4;
        let base_sequence = self.sent_buffer.sequence().wrapping_sub(sample_size as u16);

        let mut packets_dropped = 0;
        let mut bytes_sent = 0;
        let mut start_time = Instant::now();
        let mut end_time = Instant::now() - Duration::from_secs(100);
        for i in 0..sample_size {
            if let Some(sent_packet) = self.sent_buffer.get(base_sequence.wrapping_add(i as u16)) {
                if sent_packet.size_bytes == 0 {
                    // Only Default Packets have size 0
                    continue;
                }
                bytes_sent += sent_packet.size_bytes;
                if sent_packet.time < start_time {
                    start_time = sent_packet.time;
                }
                if sent_packet.time > end_time {
                    end_time = sent_packet.time;
                }
                if !sent_packet.ack {
                    packets_dropped += 1;
                }
            }
        }

        // Calculate packet loss
        let packet_loss = packets_dropped as f64 / sample_size as f64 * 100.0;
        if f64::abs(self.network_info.packet_loss - packet_loss) > 0.0001 {
            self.network_info.packet_loss += (packet_loss - self.network_info.packet_loss)
                * self.config.measure_smoothing_factor;
        } else {
            self.network_info.packet_loss = packet_loss;
        }

        // Calculate sent bandwidth
        if end_time <= start_time {
            return;
        }

        let sent_bandwidth_kbps =
            bytes_sent as f64 / (end_time - start_time).as_secs_f64() * 8.0 / 1000.0;
        if f64::abs(self.network_info.sent_bandwidth_kbps - sent_bandwidth_kbps) > 0.0001 {
            self.network_info.sent_bandwidth_kbps += (sent_bandwidth_kbps
                - self.network_info.sent_bandwidth_kbps)
                * self.config.measure_smoothing_factor;
        } else {
            self.network_info.sent_bandwidth_kbps = sent_bandwidth_kbps;
        }
    }

    pub fn network_info(&self) -> &NetworkInfo {
        &self.network_info
    }

    fn update_received_bandwidth(&mut self) {
        let sample_size = self.config.received_packets_buffer_size / 4;
        let base_sequence = self
            .received_buffer
            .sequence()
            .wrapping_sub(sample_size as u16)
            .wrapping_add(1);

        let mut bytes_received = 0;
        let mut start_time = Instant::now();
        let mut end_time = Instant::now() - Duration::from_secs(100);
        for i in 0..sample_size {
            if let Some(received_packet) = self
                .received_buffer
                .get_mut(base_sequence.wrapping_add(i as u16))
            {
                bytes_received += received_packet.size_bytes;
                if received_packet.time < start_time {
                    start_time = received_packet.time;
                }
                if received_packet.time > end_time {
                    end_time = received_packet.time;
                }
            }
        }

        if end_time <= start_time {
            return;
        }

        let received_bandwidth_kbps =
            bytes_received as f64 / (end_time - start_time).as_secs_f64() * 8.0 / 1000.0;
        if f64::abs(self.network_info.received_bandwidth_kbps - received_bandwidth_kbps) > 0.0001 {
            self.network_info.received_bandwidth_kbps += (received_bandwidth_kbps
                - self.network_info.received_bandwidth_kbps)
                * self.config.measure_smoothing_factor;
        } else {
            self.network_info.received_bandwidth_kbps = received_bandwidth_kbps;
        }
    }
}