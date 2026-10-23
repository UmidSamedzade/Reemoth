//! End-to-end messages between client and host, carried in
//! [`SignalMsg::Relay`](crate::signal::SignalMsg::Relay) payloads.
//!
//! The server only sees these as opaque strings. Binary fields are base64.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMsg {
    /// Host → client: password salt and the host's SPAKE2 message.
    Hello { salt: String, pake: String },
    /// Client → host: the client's SPAKE2 message and key confirmation.
    Auth { pake: String, confirm: String },
    /// Host → client: host key confirmation.
    AuthOk { confirm: String },
    /// Host → client: wrong password or locked out.
    Denied {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    /// Client → host: SDP offer, MAC'd with the session key.
    Offer { sdp: String, mac: String },
    /// Host → client: SDP answer, MAC'd with the session key.
    Answer { sdp: String, mac: String },
}

impl PeerMsg {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("PeerMsg serializes")
    }

    pub fn decode(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Text messages on the `ctrl` data channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CtrlMsg {
    Ping {
        seq: u32,
    },
    Pong {
        seq: u32,
    },
    Chat {
        text: String,
    },
    /// Client announces a bulk transfer of `bytes` in binary messages.
    BulkStart {
        bytes: u64,
    },
    /// Host reports what it received since `BulkStart`.
    BulkReport {
        bytes: u64,
        micros: u64,
    },
}

impl CtrlMsg {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("CtrlMsg serializes")
    }

    pub fn decode(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let msgs = [
            PeerMsg::Hello {
                salt: "s".into(),
                pake: "p".into(),
            },
            PeerMsg::Denied {
                reason: "locked".into(),
                retry_after_secs: Some(30),
            },
            PeerMsg::Offer {
                sdp: "v=0".into(),
                mac: "m".into(),
            },
        ];
        for m in msgs {
            assert_eq!(PeerMsg::decode(&m.encode()).unwrap(), m);
        }
        let c = CtrlMsg::BulkReport {
            bytes: 1,
            micros: 2,
        };
        assert_eq!(CtrlMsg::decode(&c.encode()).unwrap(), c);
    }
}
