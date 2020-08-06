use futures::SinkExt;
use nym_hs::*;
use std::collections::HashMap;
use structopt::StructOpt;
use tokio;
use tokio::net::TcpStream;
use tokio::select;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio_tungstenite::connect_async;
use tungstenite::Message;

#[derive(StructOpt)]
struct Options {
    websocket: String,
    service: String,
}

#[tokio::main]
async fn main() {
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
    println!("Listening: {}", addr);
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

    let (out_send, mut out_rec) = tokio::sync::mpsc::channel(16);
    let mut connections = HashMap::<ConnectionId, Sender<Payload>>::new();

    loop {
        select! {
            Some(packet) = out_rec.next() => {
                let bytes = bincode::serialize(&packet).unwrap();
                ws.send(Message::Binary(bytes)).await.unwrap();
                // receive send ack
                ws.next().await.unwrap().unwrap();
            },
            Some(Ok(message)) = ws.next() => {
                let (stream, payload) = message_to_stream_payload(message);
                match payload {
                    Payload::Establish { sender } => {
                        let socket = TcpStream::connect(&options.service).await.unwrap();
                        let (in_send, in_recv) = tokio::sync::mpsc::channel(16);
                        connections.insert(stream, in_send);

                        tokio::spawn(reliable_transport(sender, stream, socket, out_send.clone(), in_recv));
                    },
                    other => {
                        if let Some(mut in_send) = connections.get_mut(&stream) {
                            in_send.send(other)
                                .await
                                .map_err(|_| "send_err")
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
}