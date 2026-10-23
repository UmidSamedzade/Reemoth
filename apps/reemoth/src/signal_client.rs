//! WebSocket connection to the signaling Worker.
//!
//! Reading and writing run on their own tasks so a host can relay many
//! sessions over one socket. The writer also sends a keepalive ping so NATs
//! and proxies do not drop an idle host connection.

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use reemoth_proto::signal::{HostId, PROTOCOL_VERSION, Role, SignalMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(25);
/// No frame (not even a keepalive pong) for this long means the socket is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct SignalConn {
    tx: mpsc::UnboundedSender<SignalMsg>,
    rx: mpsc::UnboundedReceiver<SignalMsg>,
}

/// Cloneable send half, for per-session tasks.
#[derive(Clone)]
pub struct SignalTx(mpsc::UnboundedSender<SignalMsg>);

impl SignalTx {
    pub fn send(&self, msg: SignalMsg) -> anyhow::Result<()> {
        self.0
            .send(msg)
            .map_err(|_| anyhow::anyhow!("signaling connection closed"))
    }
}

impl SignalConn {
    pub async fn connect(base_url: &str, id: HostId, role: Role) -> anyhow::Result<Self> {
        let url = format!(
            "{}/v1/host/{id}?v={PROTOCOL_VERSION}&role={}",
            ws_base(base_url),
            role.as_str()
        );
        let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&url))
            .await
            .with_context(|| format!("timed out connecting to {base_url}"))?
            .map_err(|e| match e {
                tungstenite::Error::Http(resp) => {
                    let body = resp
                        .body()
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    anyhow::anyhow!(
                        "signaling server refused ({}): {}",
                        resp.status(),
                        body.trim()
                    )
                }
                other => anyhow::Error::new(other).context(format!("connecting to {base_url}")),
            })?;
        let (mut sink, mut stream) = ws.split();

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // Times out when a NAT or sleep silently killed the TCP connection.
            while let Ok(Some(frame)) = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                let text = match frame {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                match SignalMsg::decode(&text) {
                    Ok(msg) => {
                        if in_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => tracing::warn!("bad signaling frame: {e}"),
                }
            }
        });

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMsg>();
        tokio::spawn(async move {
            let mut keepalive = tokio::time::interval(KEEPALIVE);
            keepalive.tick().await;
            loop {
                let msg = tokio::select! {
                    msg = out_rx.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                    _ = keepalive.tick() => SignalMsg::Ping { nonce: 0 },
                };
                if sink.send(Message::text(msg.encode())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        Ok(Self {
            tx: out_tx,
            rx: in_rx,
        })
    }

    pub fn sender(&self) -> SignalTx {
        SignalTx(self.tx.clone())
    }

    pub fn send(&self, msg: SignalMsg) -> anyhow::Result<()> {
        self.sender().send(msg)
    }

    /// Next message, skipping keepalive pongs. `None` once the socket is gone.
    pub async fn recv(&mut self) -> Option<SignalMsg> {
        loop {
            match self.rx.recv().await? {
                SignalMsg::Pong { nonce: 0 } => continue,
                msg => return Some(msg),
            }
        }
    }

    /// Like [`Self::recv`] but turns a closed socket or server error into an error.
    pub async fn expect(&mut self) -> anyhow::Result<SignalMsg> {
        match self.recv().await {
            Some(SignalMsg::Error { code, message }) => {
                bail!("signaling error ({code:?}): {message}")
            }
            Some(msg) => Ok(msg),
            None => bail!("signaling connection closed"),
        }
    }
}

/// Accepts the `https://` URL `wrangler deploy` prints as well as `wss://`.
fn ws_base(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if url.contains("://") {
        url.to_owned()
    } else {
        format!("wss://{url}")
    }
}

#[cfg(test)]
mod tests {
    use super::ws_base;

    #[test]
    fn normalizes_server_urls() {
        assert_eq!(ws_base("https://x.workers.dev/"), "wss://x.workers.dev");
        assert_eq!(ws_base("http://127.0.0.1:8787"), "ws://127.0.0.1:8787");
        assert_eq!(ws_base("wss://x.workers.dev"), "wss://x.workers.dev");
        assert_eq!(ws_base("x.workers.dev"), "wss://x.workers.dev");
    }
}
