//! Signaling protocol: JSON text frames over a WebSocket to the Worker.
//!
//! Every frame is one [`SignalMsg`], tagged by `"type"`.

use serde::{Deserialize, Serialize};

/// Protocol version, bumped on breaking changes. Sent as `?v=` on connect;
/// the server answers a mismatch with HTTP 400 before upgrading.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame either side accepts, in bytes.
pub const MAX_FRAME_LEN: usize = 16 * 1024;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    FrameTooLarge,
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
        assert_eq!(HostId::parse("12345678"), None);
        assert_eq!(HostId::parse("1234567890"), None);
        assert_eq!(HostId::parse("12345678a"), None);
        assert_eq!(HostId::new(1_000_000_000), None);
    }
}
