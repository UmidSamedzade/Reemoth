//! Reemoth signaling Worker.
//!
//! `GET /v1/host/{id}?v=N&role=host|client` with `Upgrade: websocket` is
//! routed to the [`HostRoom`] Durable Object named after the host ID. Rooms
//! accept sockets through the Hibernation API, so idle hosts cost nothing
//! between messages. See `reemoth_proto::signal` for the message flow.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::{Signature, VerifyingKey};
use reemoth_proto::signal::{
    ErrorCode, HostId, IceServer, PROTOCOL_VERSION, Role, SessionId, SignalMsg, host_auth_message,
};
use serde::{Deserialize, Serialize};
use worker::*;

const ROOM_BINDING: &str = "HOST_ROOMS";
const RATE_LIMIT_BINDING: &str = "CONNECT_LIMITER";
const MINT_LIMIT_BINDING: &str = "TURN_MINT_LIMITER";
const MAX_ROOM_SOCKETS: usize = 8;
/// Unauthenticated host sockets a room holds at once, and for how long.
const MAX_PENDING_HOSTS: usize = 2;
const PENDING_HOST_TTL_MS: u64 = 10_000;
/// TURN credential mints a room may make per day; beyond that clients get
/// STUN only. IDs are free to claim, so this bounds relay abuse per room and
/// the per-IP limiter bounds it across rooms.
const MAX_MINTS_PER_DAY: usize = 30;
/// Client sessions a room accepts per minute; bounds online password guessing.
const MAX_CONNECTS_PER_MINUTE: usize = 20;
const HOST_TAG: &str = "host";
const STUN_URL: &str = "stun:stun.cloudflare.com:3478";
/// Lifetime of minted TURN credentials. A relayed session loses its relay
/// when they expire (re-minting mid-session is future work).
const TURN_TTL_SECS: u32 = 4 * 3600;

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
            if query(&req, "v").and_then(|v| v.parse().ok()) != Some(PROTOCOL_VERSION) {
                return Response::error(
                    format!("unsupported version, server speaks v{PROTOCOL_VERSION}"),
                    400,
                );
            }
            if query(&req, "role")
                .as_deref()
                .and_then(Role::parse)
                .is_none()
            {
                return Response::error("role must be host or client", 400);
            }
            if !within_ip_rate_limit(&req, &env).await {
                return Response::error("too many requests", 429);
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

fn query(req: &Request, key: &str) -> Option<String> {
    req.url()
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Per-IP limit across all rooms. Skipped when the binding is not configured.
async fn within_ip_rate_limit(req: &Request, env: &Env) -> bool {
    let Ok(limiter) = env.rate_limiter(RATE_LIMIT_BINDING) else {
        return true;
    };
    let ip = req
        .headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .unwrap_or_default();
    match limiter.limit(ip).await {
        Ok(outcome) => outcome.success,
        Err(e) => {
            console_warn!("rate limiter failed: {e}");
            true
        }
    }
}

/// Per-socket state, kept in the socket attachment so it survives hibernation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Peer {
    /// Host socket that has not answered its challenge yet.
    PendingHost {
        id: u32,
        nonce: String,
        since: u64,
        ip: String,
    },
    Host {
        ip: String,
    },
    /// A host socket superseded or expired, about to close.
    Replaced,
    Client {
        session: SessionId,
        authorized: bool,
    },
}

/// One room per host ID.
#[durable_object]
pub struct HostRoom {
    state: State,
    env: Env,
}

impl DurableObject for HostRoom {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let now = Date::now().as_millis();
        let pending = self.evict_stale_pending(now);
        let established = self.state.get_websockets().len().saturating_sub(pending);
        if established >= MAX_ROOM_SOCKETS {
            return Response::error("room full", 503);
        }
        // The Worker validated both before routing here.
        let id = req
            .path()
            .rsplit('/')
            .next()
            .and_then(HostId::parse)
            .ok_or("missing host id")?;
        let role = query(&req, "role")
            .as_deref()
            .and_then(Role::parse)
            .ok_or("missing role")?;

        let pair = WebSocketPair::new()?;
        let ws = &pair.server;
        match role {
            Role::Host if pending >= MAX_PENDING_HOSTS => {
                return Response::error("too many pending host connections", 503);
            }
            Role::Host => {
                let nonce = B64.encode(random_bytes::<32>());
                self.state.accept_websocket_with_tags(ws, &[HOST_TAG]);
                ws.serialize_attachment(Peer::PendingHost {
                    id: id.get(),
                    nonce: nonce.clone(),
                    since: now,
                    ip: client_ip(&req),
                })?;
                send(ws, &SignalMsg::Challenge { nonce })?;
            }
            Role::Client => {
                // Attachments go through JS numbers, so stay within 53 bits.
                let session = u64::from_le_bytes(random_bytes::<8>()) & ((1 << 53) - 1);
                self.state
                    .accept_websocket_with_tags(ws, &[&session_tag(session)]);
                ws.serialize_attachment(Peer::Client {
                    session,
                    authorized: false,
                })?;
                // Check the host first so probing random IDs writes no storage.
                match self.host() {
                    None => reject(ws, ErrorCode::HostOffline, "host is offline"),
                    Some(_) if !self.allow_connect().await? => {
                        reject(ws, ErrorCode::RateLimited, "too many connection attempts")
                    }
                    Some(host) => send(&host, &SignalMsg::Incoming { session })?,
                }
            }
        }
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let WebSocketIncomingMessage::String(frame) = message else {
            reject(
                &ws,
                ErrorCode::BadRequest,
                "binary frames are not supported",
            );
            return Ok(());
        };
        let msg = match SignalMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(e @ reemoth_proto::signal::DecodeError::TooLarge(_)) => {
                reject(&ws, ErrorCode::FrameTooLarge, &e.to_string());
                return Ok(());
            }
            Err(e) => {
                reject(&ws, ErrorCode::BadRequest, &e.to_string());
                return Ok(());
            }
        };
        if let SignalMsg::Ping { nonce } = msg {
            return send(&ws, &SignalMsg::Pong { nonce });
        }
        match (attachment(&ws), msg) {
            (
                Some(Peer::PendingHost { id, nonce, ip, .. }),
                SignalMsg::HostAuth {
                    public_key,
                    signature,
                },
            ) => {
                self.register_host(&ws, id, &nonce, &public_key, &signature, ip)
                    .await
            }
            (Some(Peer::Host { .. }), SignalMsg::Relay { session, payload }) => {
                match self.client(session) {
                    Some(client) => send(&client, &SignalMsg::Relay { session, payload }),
                    None => send(&ws, &SignalMsg::SessionClosed { session }),
                }
            }
            (Some(Peer::Host { .. }), SignalMsg::Authorize { session }) => {
                self.authorize(&ws, session).await
            }
            (Some(Peer::Host { .. }), SignalMsg::SessionClosed { session }) => {
                if let Some(client) = self.client(session) {
                    let _ = client.close(Some(1000), Some("session closed by host"));
                }
                Ok(())
            }
            (Some(Peer::Client { session, .. }), SignalMsg::Relay { payload, .. }) => {
                match self.host() {
                    Some(host) => send(&host, &SignalMsg::Relay { session, payload }),
                    None => {
                        reject(&ws, ErrorCode::HostOffline, "host went offline");
                        Ok(())
                    }
                }
            }
            (Some(Peer::Client { .. }), SignalMsg::SessionClosed { .. }) => {
                let _ = ws.close(Some(1000), Some("session closed"));
                Ok(())
            }
            (_, other) => {
                reject(&ws, ErrorCode::BadRequest, &format!("unexpected {other:?}"));
                Ok(())
            }
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        code: usize,
        reason: String,
        _clean: bool,
    ) -> Result<()> {
        match attachment(&ws) {
            Some(Peer::Client { session, .. }) => {
                if let Some(host) = self.host() {
                    let _ = send(&host, &SignalMsg::SessionClosed { session });
                }
            }
            Some(Peer::Host { .. }) => {
                for client in self.clients() {
                    let _ = client.close(Some(1001), Some("host went offline"));
                }
            }
            _ => {}
        }
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

impl HostRoom {
    /// The registered (authenticated) host socket, if any.
    fn host(&self) -> Option<WebSocket> {
        self.state
            .get_websockets_with_tag(HOST_TAG)
            .into_iter()
            .find(|ws| matches!(attachment(ws), Some(Peer::Host { .. })))
    }

    /// Closes unauthenticated host sockets past their deadline; returns how
    /// many pending ones remain.
    fn evict_stale_pending(&self, now: u64) -> usize {
        let mut pending = 0;
        for ws in self.state.get_websockets_with_tag(HOST_TAG) {
            if let Some(Peer::PendingHost { since, .. }) = attachment(&ws) {
                if now.saturating_sub(since) > PENDING_HOST_TTL_MS {
                    let _ = ws.serialize_attachment(Peer::Replaced);
                    reject(&ws, ErrorCode::AuthFailed, "challenge not answered in time");
                } else {
                    pending += 1;
                }
            }
        }
        pending
    }

    fn client(&self, session: SessionId) -> Option<WebSocket> {
        self.state
            .get_websockets_with_tag(&session_tag(session))
            .into_iter()
            .next()
    }

    fn clients(&self) -> Vec<WebSocket> {
        self.state
            .get_websockets()
            .into_iter()
            .filter(|ws| matches!(attachment(ws), Some(Peer::Client { .. })))
            .collect()
    }

    async fn register_host(
        &self,
        ws: &WebSocket,
        id: u32,
        nonce: &str,
        public_key: &str,
        signature: &str,
        ip: String,
    ) -> Result<()> {
        let id = HostId::new(id).ok_or("bad id in attachment")?;
        let nonce = B64.decode(nonce).map_err(|e| e.to_string())?;
        let Some((key_bytes, key)) = parse_key(public_key) else {
            reject(ws, ErrorCode::AuthFailed, "malformed public key");
            return Ok(());
        };
        let verified = B64
            .decode(signature)
            .ok()
            .and_then(|s| Signature::from_slice(&s).ok())
            .is_some_and(|sig| {
                key.verify_strict(&host_auth_message(id, &nonce), &sig)
                    .is_ok()
            });
        if !verified {
            reject(ws, ErrorCode::AuthFailed, "challenge signature invalid");
            return Ok(());
        }

        // First key to register an ID owns it.
        let storage = self.state.storage();
        match storage.get::<Vec<u8>>("host_key").await? {
            Some(owner) if owner != key_bytes => {
                reject(ws, ErrorCode::IdTaken, "this ID belongs to another host");
                return Ok(());
            }
            Some(_) => {}
            None => storage.put("host_key", key_bytes).await?,
        }

        // A new registration replaces any previous host connection.
        if let Some(old) = self.host() {
            // Mark it first so its close event does not tear down our clients.
            old.serialize_attachment(Peer::Replaced)?;
            let _ = old.close(Some(4000), Some("replaced by a newer host connection"));
        }
        ws.serialize_attachment(Peer::Host { ip })?;
        send(ws, &SignalMsg::Registered)
    }

    async fn authorize(&self, host: &WebSocket, session: SessionId) -> Result<()> {
        let Some(client) = self.client(session) else {
            return send(host, &SignalMsg::SessionClosed { session });
        };
        // Mint TURN credentials at most once per session.
        if let Some(Peer::Client {
            authorized: true, ..
        }) = attachment(&client)
        {
            return Ok(());
        }
        client.serialize_attachment(Peer::Client {
            session,
            authorized: true,
        })?;
        let host_ip = match attachment(host) {
            Some(Peer::Host { ip }) => ip,
            _ => String::new(),
        };
        let servers = if self.allow_mint(&host_ip).await? {
            ice_servers(&self.env).await
        } else {
            console_warn!("TURN mint budget exhausted; sending STUN only");
            vec![stun_server()]
        };
        let msg = SignalMsg::IceServers { session, servers };
        send(host, &msg)?;
        send(&client, &msg)
    }

    /// Per-host-IP rate limit plus a daily per-room budget for TURN mints.
    async fn allow_mint(&self, host_ip: &str) -> Result<bool> {
        if let Ok(limiter) = self.env.rate_limiter(MINT_LIMIT_BINDING)
            && !limiter.limit(host_ip.to_owned()).await?.success
        {
            return Ok(false);
        }
        let now = Date::now().as_millis();
        let storage = self.state.storage();
        let mut recent: Vec<u64> = storage.get("mints").await?.unwrap_or_default();
        recent.retain(|t| now.saturating_sub(*t) < 86_400_000);
        if recent.len() >= MAX_MINTS_PER_DAY {
            return Ok(false);
        }
        recent.push(now);
        storage.put("mints", recent).await?;
        Ok(true)
    }

    /// Sliding one-minute window of client connects, persisted in storage.
    async fn allow_connect(&self) -> Result<bool> {
        let now = Date::now().as_millis();
        let storage = self.state.storage();
        let mut recent: Vec<u64> = storage.get("connects").await?.unwrap_or_default();
        recent.retain(|t| now.saturating_sub(*t) < 60_000);
        if recent.len() >= MAX_CONNECTS_PER_MINUTE {
            return Ok(false);
        }
        recent.push(now);
        storage.put("connects", recent).await?;
        Ok(true)
    }
}

fn parse_key(public_key: &str) -> Option<(Vec<u8>, VerifyingKey)> {
    let bytes = B64.decode(public_key).ok()?;
    let key = VerifyingKey::from_bytes(bytes.as_slice().try_into().ok()?).ok()?;
    Some((bytes, key))
}

fn client_ip(req: &Request) -> String {
    req.headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn stun_server() -> IceServer {
    IceServer {
        urls: vec![STUN_URL.into()],
        username: None,
        credential: None,
    }
}

fn session_tag(session: SessionId) -> String {
    format!("s{session}")
}

fn attachment(ws: &WebSocket) -> Option<Peer> {
    ws.deserialize_attachment().ok().flatten()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("crypto.getRandomValues is available");
    buf
}

/// STUN always; TURN too when `TURN_KEY_ID` and `TURN_KEY_API_TOKEN` are set.
async fn ice_servers(env: &Env) -> Vec<IceServer> {
    let stun = stun_server;
    let (Ok(key_id), Ok(token)) = (env.secret("TURN_KEY_ID"), env.secret("TURN_KEY_API_TOKEN"))
    else {
        return vec![stun()];
    };
    match mint_turn(&key_id.to_string(), &token.to_string()).await {
        Ok(servers) if !servers.is_empty() => servers,
        Ok(_) => vec![stun()],
        Err(e) => {
            console_error!("minting TURN credentials failed: {e}");
            vec![stun()]
        }
    }
}

/// Cloudflare Realtime TURN credential API.
async fn mint_turn(key_id: &str, token: &str) -> Result<Vec<IceServer>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Servers {
        Many(Vec<IceServer>),
        One(IceServer),
    }
    #[derive(Deserialize)]
    struct Body {
        #[serde(rename = "iceServers")]
        ice_servers: Servers,
    }

    let url = format!(
        "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
    );
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {token}"))?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(format!(r#"{{"ttl":{TURN_TTL_SECS}}}"#).into()));
    let mut resp = Fetch::Request(Request::new_with_init(&url, &init)?)
        .send()
        .await?;
    if !(200..300).contains(&resp.status_code()) {
        return Err(format!("TURN API returned {}", resp.status_code()).into());
    }
    Ok(match resp.json::<Body>().await?.ice_servers {
        Servers::Many(v) => v,
        Servers::One(s) => vec![s],
    })
}

fn send(ws: &WebSocket, msg: &SignalMsg) -> Result<()> {
    ws.send_with_str(msg.encode())
}

/// Sends an error and closes. Best effort: the socket may already be gone.
fn reject(ws: &WebSocket, code: ErrorCode, message: &str) {
    let _ = send(
        ws,
        &SignalMsg::Error {
            code,
            message: message.into(),
        },
    );
    let _ = ws.close(Some(1008), Some("rejected"));
}
