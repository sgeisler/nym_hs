#![feature(async_closure)]

use crate::SocksError::ProtocolError;
use futures::io::Error;
use futures::{FutureExt, StreamExt, TryStreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;

#[tokio::main]
async fn main() {
    TcpListener::bind("127.0.0.1:9090")
        .await
        .unwrap()
        .incoming()
        .for_each(|socket| tokio::spawn(handle_connection(socket.unwrap())).map(|_| ()))
        .await;
}

async fn handle_connection(mut socket: TcpStream) -> Result<(), SocksError> {
    authenticate(&mut socket).await?;
    let req = receive_request(&mut socket).await?;
    println!("{:?}", req);

    Ok(())
}

#[derive(Debug)]
struct SocksRequest {
    fqdn: String,
    port: u16,
}

#[derive(Debug)]
enum SocksError {
    IoError(tokio::io::Error),
    ProtocolError(&'static str),
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
    socket.write_all(&[5, 0, 0, 3]).await?;
    socket.write_u8(addr.len() as u8).await?;
    socket.write_all(addr.as_bytes()).await?;
    tokio_byteorder::AsyncWriteBytesExt::write_u16::<tokio_byteorder::BigEndian>(socket, port)
        .await?;

    Ok(SocksRequest { fqdn: addr, port })
}

impl From<tokio::io::Error> for SocksError {
    fn from(e: tokio::io::Error) -> Self {
        SocksError::IoError(e)
    }
}
