use bech32::{CheckBase32, FromBase32, ToBase32};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::select;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::{debug, info, trace, warn};
use tungstenite::Message;

// how long to wait for acks before resending in ms
pub const TIMEOUT: u64 = 2000;

pub type ConnectionId = [u8; 32];
/// Maps connection id to a socket reachable through a channel
pub type Connections = Arc<RwLock<HashMap<ConnectionId, Sender<Payload>>>>;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Packet {
    pub recipient: Identity,
    pub stream: ConnectionId,
    pub payload: Payload,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum Payload {
    Establish { sender: Identity },
    Data { idx: usize, data: Vec<u8> },
    Ack { idx: usize },
}

#[derive(Debug, Deserialize, Serialize, Default, Copy, Clone)]
pub struct Identity {
    pub client: [u8; 32],
    pub gateway: [u8; 32],
}

impl Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&self.client);
        bytes[32..].copy_from_slice(&self.gateway);
        bech32::encode_to_fmt(f, "nym", &(&bytes[..]).to_base32()).unwrap()
    }
}

#[derive(Debug)]
pub enum IdentityParseError {
    WrongLenght(usize),
    InvalidBech32(bech32::Error),
    InvalidHRP(String),
}

impl FromStr for Identity {
    type Err = IdentityParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hrp, data) = bech32::decode(s).map_err(|e| IdentityParseError::InvalidBech32(e))?;
        let bytes = Vec::<u8>::from_base32(&data).unwrap();

        if hrp != "nym" {
            return Err(IdentityParseError::InvalidHRP(hrp));
        }

        if bytes.len() != 64 {
            return Err(IdentityParseError::WrongLenght(bytes.len()));
        }

        let mut identity = Identity::default();
        identity.client.copy_from_slice(&bytes[..32]);
        identity.gateway.copy_from_slice(&bytes[32..]);

        Ok(identity)
    }
}

pub async fn reliable_transport(
    peer: Identity,
    stream_id: ConnectionId,
    mut socket: TcpStream,
    mut egress: Sender<Packet>,
    mut ingress: Receiver<Payload>,
    init_pkg: Option<Packet>,
) -> Result<(), SocksError> {
    debug!("starting new stream {:?}, {:?}", peer, stream_id);

    let mut buffer = [0u8; 500];
    let mut last_out_msg_id = 0;
    let mut last_in_msg_id = 0;
    let mut unack_msg = init_pkg;
    let mut resend_interval = tokio::time::interval(Duration::from_millis(TIMEOUT));

    loop {
        select! {
            read = socket.read(&mut buffer), if unack_msg.is_none() => {
                let read = read?;
                trace!("received bytes from socket {:?}", &buffer[..read]);
                last_out_msg_id += 1;
                let packet = Packet {
                    recipient: peer,
                    stream: stream_id,
                    payload: Payload::Data {
                        idx: last_out_msg_id,
                        data: buffer[..read].into()
                    },
                };
                unack_msg = Some(packet.clone());
                egress.send(packet).await?;
            },
            Some(payload) = ingress.next() => {
                trace!("received payload from nym {:?}", payload);
                match payload {
                    Payload::Data {idx, data} => {
                        if last_in_msg_id == idx {
                            // resend lost ACK
                            trace!("resending lost ACK {}", idx);
                            egress.send(Packet {
                                recipient: peer,
                                stream: stream_id,
                                payload: Payload::Ack {
                                    idx
                                }
                            })
                                .await?;
                        } else if last_in_msg_id + 1 == idx {
                            // accept data and send ACK
                            trace!("received data {:?} and sending ACK {}", data, idx);
                            socket.write_all(&data).await?;
                            last_in_msg_id = idx;
                            egress.send(Packet {
                                recipient: peer,
                                stream: stream_id,
                                payload: Payload::Ack {
                                    idx
                                }
                            })
                                .await?;
                        } else {
                            warn!("Invalid state: unexpected ACK");
                        }
                    },
                    Payload::Ack { idx } => {
                        if idx == last_out_msg_id {
                            trace!("received ACK for {}", idx);
                            unack_msg = None;
                        } else {
                            warn!("Late ACK {}", idx);
                        }
                    },
                    Payload::Establish {..} => {
                        trace!("ACKing establish pkg");
                        egress.send(Packet {
                                recipient: peer,
                                stream: stream_id,
                                payload: Payload::Ack {
                                    idx: 0
                                }
                            })
                                .await?;
                    },
                }
            },
            _ = resend_interval.tick() => {
                if let Some(packet) = unack_msg.clone() {
                    trace!("resending packet");
                    egress.send(packet).await?;
                }
            }
        }
    }
}

pub fn message_to_string(msg: Message) -> Result<String, SocksError> {
    match msg {
        Message::Text(command) => Ok(command),
        Message::Binary(bin) => {
            String::from_utf8(bin).map_err(|_| SocksError::ProtocolError("invalid UTF8"))
        }
        _ => Err(SocksError::ProtocolError("unexpected message")),
    }
}

pub fn message_to_stream_payload(msg: Message) -> Result<(ConnectionId, Payload), SocksError> {
    let bytes = match msg {
        Message::Binary(bin) => bin,
        _ => return Err(SocksError::ProtocolError("unexpected message")),
    };

    let mut id = ConnectionId::default();
    id.copy_from_slice(&bytes[0..32]);

    let payload = bincode::deserialize::<Payload>(&bytes[32..])
        .map_err(|_| SocksError::ProtocolError("Decoding Error"))?;

    Ok((id, payload))
}

#[derive(Debug)]
pub enum SocksError {
    UnsupportedDestination,
    IoError(tokio::io::Error),
    ProtocolError(&'static str),
    ConnectionDropped,
}

impl From<tokio::io::Error> for SocksError {
    fn from(e: tokio::io::Error) -> Self {
        SocksError::IoError(e)
    }
}

impl<T> From<SendError<T>> for SocksError {
    fn from(e: SendError<T>) -> Self {
        SocksError::ConnectionDropped
    }
}
