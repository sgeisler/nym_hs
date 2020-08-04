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

// how long to wait for acks before resending in ms
pub const TIMEOUT: u64 = 3000;

pub type ConnectionId = [u8; 32];
/// Maps connection id to a socket reachable through a channel
pub type Connections = Arc<RwLock<HashMap<ConnectionId, Sender<Payload>>>>;

#[derive(Deserialize, Serialize, Clone)]
pub struct Packet {
    pub recipient: Identity,
    pub stream: ConnectionId,
    pub payload: Payload,
}

#[derive(Deserialize, Serialize, Clone)]
pub enum Payload {
    Establish { sender: Identity },
    Data { idx: usize, data: Vec<u8> },
    Ack { idx: usize },
}

#[derive(Deserialize, Serialize, Default, Copy, Clone)]
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
) -> Result<(), SocksError> {
    let mut buffer = [0u8; 500];
    let mut last_out_msg_id = 0;
    let mut last_in_msg_id = 0;
    let mut unack_msg = Some(Packet {
        recipient: peer,
        stream: stream_id,
        payload: Payload::Establish {
            sender: Default::default(),
        },
    });
    let mut resend_interval = tokio::time::interval(Duration::from_millis(TIMEOUT));

    loop {
        select! {
            read = socket.read(&mut buffer), if unack_msg.is_none() => {
                last_out_msg_id += 1;
                let packet = Packet {
                    recipient: peer,
                    stream: stream_id,
                    payload: Payload::Data {
                        idx: last_out_msg_id,
                        data: buffer[..read.unwrap()].into()
                    },
                };
                unack_msg = Some(packet.clone());
                egress.send(packet)
                    .await
                    .map_err(|_| "send_error")
                    .unwrap();
            },
            payload = ingress.next() => {
                match payload.unwrap() {
                    Payload::Data {idx, data} => {
                        if last_in_msg_id == idx {
                            // resend lost ACK
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
                            unack_msg = None;
                        } else {
                            eprintln!("Late ACK {}", idx);
                        }
                    },
                    Payload::Establish {..} => panic!("Unexpected establish message"),
                }
            },
            _ = resend_interval.tick() => {
                if let Some(packet) = unack_msg.clone() {
                    egress.send(packet)
                        .await
                        .map_err(|_| "send_error")
                        .unwrap();
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum SocksError {
    IoError(tokio::io::Error),
    ProtocolError(&'static str),
}

impl From<tokio::io::Error> for SocksError {
    fn from(e: tokio::io::Error) -> Self {
        SocksError::IoError(e)
    }
}
