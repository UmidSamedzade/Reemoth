//! Signaling protocol: JSON text frames over a WebSocket to the Worker.
//!
//! Every frame is one [`SignalMsg`], tagged by `"type"`. A host connects to
//! `/v1/host/{id}?v=N&role=host` and proves it owns the ID; clients connect
//! with `role=client` and get a session the server relays to the host.
//!
//! ```text
//! host                      server                     client
//!  |<-- challenge -----------|                            |
//!  |--- host_auth ---------->|                            |
//!  |<-- registered ----------|                            |
//!  |                         |<----------- (ws open) -----|
//!  |<-- incoming{s} ---------|                            |
//!  |--- relay{s, hello} ---->|--- relay{s, hello} ------->|
//!  |<-- relay{s, auth} ------|<-- relay{auth} ------------|
//!  |--- authorize{s} ------->|                            |
//!  |<-- ice_servers{s} ------|--- ice_servers{s} -------->|
//!  |--- relay{s, auth_ok} -->|--- relay{s, auth_ok} ----->|
//!  |<-- relay{s, offer} -----|<-- relay{offer} -----------|
//!  |--- relay{s, answer} --->|--- relay{s, answer} ------>|
//! ```
//!
//! Relay payloads are opaque to the server; see [`crate::peer`].

use serde::{Deserialize, Serialize};

/// Protocol version, bumped on breaking changes. Sent as `?v=` on connect;
/// the server answers a mismatch with HTTP 400 before upgrading.
pub const PROTOCOL_VERSION: u32 = 2;

/// Largest frame either side accepts, in bytes.
pub const MAX_FRAME_LEN: usize = 16 * 1024;

/// Server-assigned session ID, unique within one host's room.
pub type SessionId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMsg {
    /// Liveness check; the server answers with [`SignalMsg::Pong`] echoing `nonce`.
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Sent by the server before it closes the socket on a bad request.
    Error {
        code: ErrorCode,
        message: String,
    },
    /// Server → host on connect: sign this to prove you own the ID.
    Challenge {
        /// Base64, 32 random bytes.
        nonce: String,
    },
    /// Host → server: Ed25519 signature over [`host_auth_message`].
    HostAuth {
        public_key: String,
        signature: String,
    },
    /// Server → host: the ID is now bound to this socket.
    Registered,
    /// Server → host: a client opened `session`.
    Incoming {
        session: SessionId,
    },
    /// Opaque end-to-end payload. The server routes host → client by
    /// `session` and overwrites `session` on client → host frames.
    Relay {
        session: SessionId,
        payload: String,
    },
    /// Host → server: the client proved the password; release ICE servers.
    Authorize {
        session: SessionId,
    },
    /// Server → host and client after [`SignalMsg::Authorize`].
    IceServers {
        session: SessionId,
        servers: Vec<IceServer>,
    },
    /// Either side ends a session; the server tells the other side.
    SessionClosed {
        session: SessionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    FrameTooLarge,
    /// No host is registered for this ID right now.
    HostOffline,
    /// The ID belongs to a different host key.
    IdTaken,
    /// The host's challenge signature did not verify.
    AuthFailed,
    RateLimited,
    RoomFull,
}

/// `?role=` query value on connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Client => "client",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "host" => Some(Self::Host),
            "client" => Some(Self::Client),
            _ => None,
        }
    }
}

/// The bytes a host signs to answer a [`SignalMsg::Challenge`].
pub fn host_auth_message(id: HostId, nonce: &[u8]) -> Vec<u8> {
    let mut msg = format!("reemoth host-auth v1 {id} ").into_bytes();
    msg.extend_from_slice(nonce);
    msg
}

#[derive(Debug)]
pub enum DecodeError {
    TooLarge(usize),
    Json(serde_json::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(len) => write!(f, "frame of {len} bytes exceeds {MAX_FRAME_LEN}"),
            Self::Json(e) => write!(f, "invalid frame: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl SignalMsg {
    pub fn encode(&self) -> String {
        // Serializing these plain enums cannot fail.
        serde_json::to_string(self).expect("SignalMsg serializes")
    }

    pub fn decode(frame: &str) -> Result<Self, DecodeError> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(DecodeError::TooLarge(frame.len()));
        }
        serde_json::from_str(frame).map_err(DecodeError::Json)
    }
}

/// A host ID: nine decimal digits, shown to users as `123 456 789`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostId(u32);

impl HostId {
    pub const MAX: u32 = 999_999_999;

    pub fn new(raw: u32) -> Option<Self> {
        (raw <= Self::MAX).then_some(Self(raw))
    }

    /// Parses user input, ignoring spaces and dashes.
    pub fn parse(s: &str) -> Option<Self> {
        let digits: String = s.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
        if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().and_then(Self::new)
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// Human-friendly form: `123 456 789`.
    pub fn grouped(self) -> String {
        let s = self.to_string();
        format!("{} {} {}", &s[0..3], &s[3..6], &s[6..9])
    }
}

impl std::fmt::Display for HostId {
    /// Canonical form used in URLs: nine digits, zero-padded, no separators.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:09}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_wire_format() {
        let msg = SignalMsg::Ping { nonce: 7 };
        assert_eq!(msg.encode(), r#"{"type":"ping","nonce":7}"#);
        assert_eq!(SignalMsg::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn error_wire_format() {
        let msg = SignalMsg::Error {
            code: ErrorCode::FrameTooLarge,
            message: "too big".into(),
        };
        let json = msg.encode();
        assert!(json.contains(r#""code":"frame_too_large""#));
        assert_eq!(SignalMsg::decode(&json).unwrap(), msg);
    }

    #[test]
    fn relay_and_ice_servers_wire_format() {
        let msg = SignalMsg::Relay {
            session: 9,
            payload: "{}".into(),
        };
        assert_eq!(
            msg.encode(),
            r#"{"type":"relay","session":9,"payload":"{}"}"#
        );
        let msg = SignalMsg::IceServers {
            session: 1,
            servers: vec![IceServer {
                urls: vec!["stun:stun.cloudflare.com:3478".into()],
                username: None,
                credential: None,
            }],
        };
        assert!(!msg.encode().contains("username"));
        assert_eq!(SignalMsg::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn host_auth_message_binds_id_and_nonce() {
        let id = HostId::new(42).unwrap();
        let m = host_auth_message(id, &[1, 2]);
        assert!(m.starts_with(b"reemoth host-auth v1 000000042 "));
        assert!(m.ends_with(&[1, 2]));
    }

    #[test]
    fn decode_rejects_oversized_and_unknown() {
        let big = format!(
            r#"{{"type":"ping","nonce":1,"pad":"{}"}}"#,
            "x".repeat(MAX_FRAME_LEN)
        );
        assert!(matches!(
            SignalMsg::decode(&big),
            Err(DecodeError::TooLarge(_))
        ));
        assert!(matches!(
            SignalMsg::decode(r#"{"type":"nope"}"#),
            Err(DecodeError::Json(_))
        ));
    }

    #[test]
    fn host_id_parse_and_display() {
        let id = HostId::parse("012 345-678").unwrap();
        assert_eq!(id.get(), 12_345_678);
        assert_eq!(id.to_string(), "012345678");
        assert_eq!(id.grouped(), "012 345 678");
        assert_eq!(HostId::parse("12345678"), None);
        assert_eq!(HostId::parse("1234567890"), None);
        assert_eq!(HostId::parse("12345678a"), None);
        assert_eq!(HostId::new(1_000_000_000), None);
    }
}
