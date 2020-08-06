#![feature(async_closure)]

use crate::SocksError::ProtocolError;
use futures::{FutureExt, SinkExt, StreamExt};
use nym_hs::*;
use rand::Rng;
use structopt::StructOpt;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_tungstenite::connect_async;
use tungstenite::Message;

#[derive(StructOpt)]
struct Options {
    websocket: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let connections: Connections = Default::default();
    let (out_sender, out_receiver) = tokio::sync::mpsc::channel(16);

    tokio::spawn(nym_client(connections.clone(), out_receiver));

    TcpListener::bind("127.0.0.1:9090")
        .await
        .unwrap()
        .incoming()
        .for_each(|socket| {
            tokio::spawn(handle_connection(
                socket.unwrap(),
                connections.clone(),
                out_sender.clone(),
            ))
            .map(|_| ())
        })
        .await;
}

async fn handle_connection(
    mut socket: TcpStream,
    connections: Connections,
    out: Sender<Packet>,
) -> Result<(), SocksError> {
    let id: ConnectionId = rand::thread_rng().gen();
    let (in_sender, incoming) = tokio::sync::mpsc::channel(16);
    connections.write().await.insert(id, in_sender);

    authenticate(&mut socket).await?;
    let req = receive_request(&mut socket).await?;
    println!("{:?}", req);

    // parse <nym_id>.nym TLDs
    let mut fqdn = req.fqdn.split('.');
    let peer = fqdn.next().ok_or(SocksError::UnsupportedDestination)?;
    let gateway = fqdn.next().ok_or(SocksError::UnsupportedDestination)?;
    if "nym" != fqdn.next().ok_or(SocksError::UnsupportedDestination)? {
        return Err(SocksError::UnsupportedDestination);
    }
    // assert_eq!("nym", fqdn.next().unwrap());
    // assert_eq!(None, fqdn.next());

    let mut recipient = Identity::default();
    recipient
        .client
        .copy_from_slice(&bs58::decode(peer).into_vec().unwrap());
    recipient
        .gateway
        .copy_from_slice(&bs58::decode(gateway).into_vec().unwrap());

    reliable_transport(
        recipient,
        id,
        socket,
        out,
        incoming,
        Some(Packet {
            recipient,
            stream: id,
            payload: Payload::Establish {
                sender: Default::default(),
            },
        }),
    )
    .await
}

async fn nym_client(connections: Connections, mut outgoing: Receiver<Packet>) {
    let options: Options = Options::from_args();

    let (mut ws, _) = connect_async(&options.websocket)
        .await
        .expect("Couldn't connect to nym websocket");

    ws.send(Message::text("{\"type\": \"selfAddress\"}"))
        .await
        .unwrap();

    let addr_answer = serde_json::from_str::<serde_json::Value>(&message_to_string(
        ws.next().await.unwrap().unwrap(),
    ))
    .unwrap();
    let addr = addr_answer.get("address").unwrap().as_str().unwrap();
    let mut addr_parts = addr.split('@');
    let node = bs58::decode(&addr_parts.next().unwrap())
        .into_vec()
        .unwrap();
    let gateway = bs58::decode(&addr_parts.next().unwrap())
        .into_vec()
        .unwrap();

    let mut us = Identity::default();
    us.client.copy_from_slice(&node);
    us.gateway.copy_from_slice(&gateway);

    loop {
        select! {
            Some(packet) = outgoing.next() => {
                let mut packet = packet;

                if let Payload::Establish { ref mut sender } = packet.payload {
                    *sender = us;
                }

                let bytes = bincode::serialize(&packet).unwrap();
                ws.send(Message::Binary(bytes)).await.unwrap();
                // receive send ack
                ws.next().await.unwrap().unwrap();
            },
            Some(Ok(message)) = ws.next() => {
                let (stream, payload) = message_to_stream_payload(message);
                let mut conn_lock = connections.write()
                    .await;
                let mut was_err = false;
                if let Some(ref mut client) = conn_lock.get_mut(&stream) {
                    was_err = client.send(payload)
                        .await
                        .is_err();
                }
                if was_err {
                    conn_lock.remove(&stream);
                }
            }
        }
    }
}

#[derive(Debug)]
struct SocksRequest {
    fqdn: String,
    port: u16,
}

async fn authenticate(socket: &mut TcpStream) -> Result<(), SocksError> {
    if socket.read_u8().await? != 5 {
        return Err(ProtocolError("Wrong version"));
    }

    let methods_len = socket.read_u8().await?;
    let mut methods = Vec::with_capacity(methods_len as usize);
    for _ in 0..methods_len {
        methods.push(socket.read_u8().await?);
    }

    if !methods.contains(&0) {
        return Err(ProtocolError("NO AUTHENTICATION REQUIRED not supported"));
    }

    // respond with version 5
    socket.write_u8(5).await?;
    // choose no authentication
    socket.write_u8(0).await?;

    Ok(())
}

async fn receive_request(socket: &mut TcpStream) -> Result<SocksRequest, SocksError> {
    if socket.read_u8().await? != 5 {
        return Err(ProtocolError("Wrong version"));
    }

    if socket.read_u8().await? != 1 {
        return Err(ProtocolError("Only connect requests are supported"));
    }

    if socket.read_u8().await? != 0 {
        return Err(ProtocolError("RSV!=0"));
    }

    if socket.read_u8().await? != 3 {
        return Err(ProtocolError("Only fqdns are supported"));
    }

    let len = socket.read_u8().await?;
    let mut addr_bytes = vec![0u8; len as usize];
    socket.read_exact(&mut addr_bytes).await?;
    let addr =
        String::from_utf8(addr_bytes).map_err(|_| ProtocolError("Invalid unicode as fqdn"))?;

    let port =
        tokio_byteorder::AsyncReadBytesExt::read_u16::<tokio_byteorder::BigEndian>(socket).await?;
    // Our response is kinda bs except that it says it was successful (which might actually be the case)
    socket.write_all(&[5, 0, 0, 1, 10, 0, 0, 1]).await?;
    //tokio_byteorder::AsyncWriteBytesExt::write_u16::<tokio_byteorder::BigEndian>(socket, port)
    //    .await?;
    Ok(SocksRequest { fqdn: addr, port })
}
