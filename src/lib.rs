use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::select;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::{debug, info, trace, warn};
use tungstenite::Message;

// how long to wait for acks before resending in ms
pub const TIMEOUT: u64 = 5000;

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
                let read = read.unwrap();
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
                egress.send(packet)
                    .await
                    .map_err(|_| "send_error")
                    .unwrap();
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
                                .await
                                .map_err(|_| "send_error")
                                .unwrap();
                        } else if last_in_msg_id + 1 == idx {
                            // accept data and send ACK
                            trace!("received data {:?} and sending ACK {}", data, idx);
                            socket.write_all(&data).await.unwrap();
                            last_in_msg_id = idx;
                            egress.send(Packet {
                                recipient: peer,
                                stream: stream_id,
                                payload: Payload::Ack {
                                    idx
                                }
                            })
                                .await
                                .map_err(|_| "send_error")
                                .unwrap();
                        } else {
                            panic!("invalid state");
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
                                .await
                                .map_err(|_| "send_error")
                                .unwrap();
                    },
                }
            },
            _ = resend_interval.tick() => {
                if let Some(packet) = unack_msg.clone() {
                    trace!("resending packet");
                    egress.send(packet)
                        .await
                        .map_err(|_| "send_error")
                        .unwrap();
                }
            }
        }
    }
}

pub fn message_to_string(msg: Message) -> String {
    match msg {
        Message::Text(command) => command,
        Message::Binary(bin) => String::from_utf8(bin).unwrap(),
        Message::Close(_) => {
            panic!("Connection closed");
        }
        msg => {
            panic!("Received unsupported message: {:?}", msg);
        }
    }
}

pub fn message_to_stream_payload(msg: Message) -> (ConnectionId, Payload) {
    let bytes = match msg {
        Message::Binary(bin) => bin,
        _ => panic!("Unexpected msg type"),
    };

    let mut id = ConnectionId::default();
    id.copy_from_slice(&bytes[0..32]);

    let payload = bincode::deserialize::<Payload>(&bytes[32..]).unwrap();

    (id, payload)
}

#[derive(Debug)]
pub enum SocksError {
    UnsupportedDestination,
    IoError(tokio::io::Error),
    ProtocolError(&'static str),
}

impl From<tokio::io::Error> for SocksError {
    fn from(e: tokio::io::Error) -> Self {
        SocksError::IoError(e)
    }
}
