//! Reemoth signaling Worker.
//!
//! `GET /v1/host/{id}?v=N` with `Upgrade: websocket` is routed to the
//! [`HostRoom`] Durable Object named after the host ID. Rooms accept sockets
//! through the Hibernation API, so idle hosts cost nothing between messages.

use reemoth_proto::signal::{ErrorCode, HostId, PROTOCOL_VERSION, SignalMsg};
use worker::*;

const ROOM_BINDING: &str = "HOST_ROOMS";
const MAX_ROOM_SOCKETS: usize = 8;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "healthz"] => Response::ok("ok"),
        ["", "v1", "host", raw_id] => {
            let Some(id) = HostId::parse(raw_id) else {
                return Response::error("invalid host id", 400);
            };
            if !is_websocket_upgrade(&req) {
                return Response::error("expected websocket upgrade", 426);
            }
            if requested_version(&req) != Some(PROTOCOL_VERSION) {
                return Response::error(
                    format!("unsupported version, server speaks v{PROTOCOL_VERSION}"),
                    400,
                );
            }
            let rooms = env.durable_object(ROOM_BINDING)?;
            let stub = rooms.id_from_name(&id.to_string())?.get_stub()?;
            stub.fetch_with_request(req).await
        }
        _ => Response::error("not found", 404),
    }
}

fn is_websocket_upgrade(req: &Request) -> bool {
    req.headers()
        .get("Upgrade")
        .ok()
        .flatten()
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

fn requested_version(req: &Request) -> Option<u32> {
    req.url()
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "v")
        .and_then(|(_, v)| v.parse().ok())
}

/// One room per host ID. Signaling state lives here from M1 on.
#[durable_object]
pub struct HostRoom {
    state: State,
}

impl DurableObject for HostRoom {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, _req: Request) -> Result<Response> {
        // A session needs the host plus one client; leave headroom for reconnects.
        if self.state.get_websockets().len() >= MAX_ROOM_SOCKETS {
            return Response::error("room full", 503);
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let WebSocketIncomingMessage::String(frame) = message else {
            return reject(
                &ws,
                ErrorCode::BadRequest,
                "binary frames are not supported".into(),
            );
        };
        match SignalMsg::decode(&frame) {
            Ok(SignalMsg::Ping { nonce }) => send(&ws, &SignalMsg::Pong { nonce }),
            Ok(other) => reject(&ws, ErrorCode::BadRequest, format!("unexpected {other:?}")),
            Err(e @ reemoth_proto::signal::DecodeError::TooLarge(_)) => {
                reject(&ws, ErrorCode::FrameTooLarge, e.to_string())
            }
            Err(e) => reject(&ws, ErrorCode::BadRequest, e.to_string()),
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        code: usize,
        reason: String,
        _clean: bool,
    ) -> Result<()> {
        // Complete the close handshake, echoing the code only if it is sendable.
        // Newer runtimes reply to the close themselves, so a failure here is expected.
        let code = match code {
            1000..=1003 | 1007..=1014 | 3000..=4999 => code as u16,
            _ => 1000,
        };
        let _ = ws.close(Some(code), Some(reason));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, error: Error) -> Result<()> {
        console_warn!("websocket error: {error}");
        Ok(())
    }
}

fn send(ws: &WebSocket, msg: &SignalMsg) -> Result<()> {
    ws.send_with_str(msg.encode())
}

fn reject(ws: &WebSocket, code: ErrorCode, message: String) -> Result<()> {
    // Close even if the error frame could not be sent.
    let _ = send(ws, &SignalMsg::Error { code, message });
    ws.close(Some(1008), Some("protocol error"))
}
