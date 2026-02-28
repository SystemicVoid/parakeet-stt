use anyhow::{Context, Result};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::ClientConfig;
use crate::protocol::{decode_server_message, ClientMessage, DecodedServerMessage, ServerMessage};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type WsWrite = SplitSink<WsStream, Message>;
pub type WsRead = SplitStream<WsStream>;

pub struct WsClient {
    stream: WsStream,
}

impl WsClient {
    pub async fn connect(config: &ClientConfig) -> Result<Self> {
        let request = config.build_request()?;
        let stream = timeout(config.connect_timeout, connect_async(request))
            .await
            .context("WebSocket connection timed out")??
            .0;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let payload = serde_json::to_string(message).context("failed to serialize message")?;
        self.stream
            .send(Message::Text(payload))
            .await
            .context("failed to send message")
    }

    pub async fn next_message(&mut self) -> Result<Option<ServerMessage>> {
        while let Some(msg) = self.stream.next().await {
            match msg {
                Ok(Message::Text(txt)) => {
                    match decode_server_message(&txt).context("failed to decode server message")? {
                        DecodedServerMessage::Known(parsed) => return Ok(Some(*parsed)),
                        DecodedServerMessage::UnknownType { .. } => continue,
                    }
                }
                Ok(Message::Binary(_)) => {
                    continue;
                }
                Ok(Message::Ping(payload)) => {
                    self.stream
                        .send(Message::Pong(payload))
                        .await
                        .context("failed to reply pong")?;
                }
                Ok(Message::Close(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
                _ => {}
            }
        }
        Ok(None)
    }

    pub fn into_split(self) -> (WsWrite, WsRead) {
        self.stream.split()
    }

    #[allow(dead_code)]
    pub async fn close(mut self) -> Result<()> {
        self.stream
            .close(None)
            .await
            .context("failed to perform websocket close")
    }
}
