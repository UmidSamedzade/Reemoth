//! WebSocket client for the signaling Worker.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use reemoth_proto::signal::{HostId, PROTOCOL_VERSION, SignalMsg};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

pub struct SignalClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_nonce: u64,
}

impl SignalClient {
    pub async fn connect(base_url: &str, id: HostId) -> anyhow::Result<Self> {
        let url = format!(
            "{}/v1/host/{id}?v={PROTOCOL_VERSION}",
            base_url.trim_end_matches('/')
        );
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(&url),
        )
        .await
        .with_context(|| format!("timed out connecting to {url}"))?
        .with_context(|| format!("connecting to {url}"))?;
        Ok(Self { ws, next_nonce: 1 })
    }

    pub async fn send(&mut self, msg: &SignalMsg) -> anyhow::Result<()> {
        self.ws.send(Message::text(msg.encode())).await?;
        Ok(())
    }

    /// Next signaling message, skipping transport-level frames.
    pub async fn recv(&mut self) -> anyhow::Result<SignalMsg> {
        while let Some(frame) = self.ws.next().await {
            match frame? {
                Message::Text(text) => return Ok(SignalMsg::decode(&text)?),
                Message::Close(reason) => bail!("server closed connection: {reason:?}"),
                _ => {}
            }
        }
        bail!("connection ended")
    }

    pub async fn ping(&mut self) -> anyhow::Result<Duration> {
        let nonce = self.next_nonce;
        self.next_nonce += 1;
        let start = Instant::now();
        self.send(&SignalMsg::Ping { nonce }).await?;
        match self.recv().await? {
            SignalMsg::Pong { nonce: n } if n == nonce => Ok(start.elapsed()),
            SignalMsg::Error { code, message } => bail!("server error {code:?}: {message}"),
            other => bail!("expected pong {nonce}, got {other:?}"),
        }
    }

    pub async fn close(mut self) -> anyhow::Result<()> {
        self.ws.close(None).await?;
        Ok(())
    }
}
