use anyhow::{bail, Context, Result};
use clap::{command, Parser};
use futures::{SinkExt, StreamExt};
use log::{debug, info, warn};
use serde_derive::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::broadcast;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_name = "ADDRESS")]
    server: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileMessage {
    filename: String,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum MessageType {
    ChatMessage(ChatMessage),
    FileMessage(FileMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InboundSocketMessage {
    destination_name: Option<String>,
    content: MessageType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutboundSocketMessage {
    destination_name: Option<String>,
    sender_name: String,
    content: MessageType,
}

#[derive(Debug, Clone, Deserialize)]
struct IdentifyMessage {
    name: String,
}

pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    let args = Args::parse();

    let (tx, _rx) = broadcast::channel::<OutboundSocketMessage>(100);
    let tx = Arc::new(Mutex::new(tx));

    let listener = TcpListener::bind(args.server).await.unwrap();
    info!("WebSocket server is running on ws://{}", args.server);

    while let Ok((stream, _)) = listener.accept().await {
        let tx = Arc::clone(&tx);
        tokio::spawn(handle_connection(stream, tx));
    }

    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    tx: Arc<Mutex<broadcast::Sender<OutboundSocketMessage>>>,
) -> Result<()> {
    let ws_stream = accept_async(stream)
        .await
        .expect("Error during WebSocket handshake");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let identity: IdentifyMessage = {
        if let Message::Text(text) = ws_receiver
            .next()
            .await
            .context("Missing identify message")??
        {
            serde_json::from_str(&text)?
        } else {
            bail!("Invalid identify message!");
        }
    };

    let name = identity.name.clone().to_lowercase();

    debug!("New connection from {}", name);

    let mut rx = tx.lock().await.subscribe();

    tokio::spawn(async move {
        while let Some(message) = ws_receiver.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let inbound_msg: InboundSocketMessage = serde_json::from_str(&text)?;
                    let outbound_msg = OutboundSocketMessage {
                        destination_name: inbound_msg.destination_name,
                        sender_name: name.clone(),
                        content: inbound_msg.content,
                    };

                    tx.lock().await.send(outbound_msg)?;
                }
                Ok(Message::Close(_)) => {
                    break;
                }
                Err(e) => {
                    warn!("Error receiving message: {:?}", e);
                    break;
                }
                _ => {}
            }
        }

        Result::<()>::Ok(())
    });

    while let Ok(message) = rx.recv().await {
        // if a destination name is set and does not match, do not forward the message
        if let Some(destination_name) = &message.destination_name {
            if *destination_name.to_lowercase() != identity.name.to_lowercase() {
                continue;
            }
        }

        ws_sender
            .send(Message::Text(serde_json::to_string(&message)?))
            .await?;
    }

    Ok(())
}
