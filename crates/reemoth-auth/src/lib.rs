//! Authentication for Reemoth sessions.
//!
//! - [`HostIdentity`]: the Ed25519 key that owns a host ID on the signaling server.
//! - [`PasswordSecret`]: the Argon2id-stretched password both sides feed to SPAKE2.
//! - [`HostPake`] / [`ClientPake`]: password-authenticated key exchange. The
//!   resulting [`SessionKeys`] confirm both sides know the password and MAC the
//!   SDP offer/answer, which carry the DTLS fingerprints. A signaling server
//!   that sees every message can therefore neither learn the password nor
//!   man-in-the-middle the media path.
//! - [`Lockout`]: host-side back-off after failed attempts.

mod identity;
mod lockout;
mod pake;
mod password;

pub use identity::HostIdentity;
pub use lockout::Lockout;
pub use pake::{ClientPake, HostPake, SessionKeys};
pub use password::PasswordSecret;

use base64::{Engine, engine::general_purpose::STANDARD as B64};

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Wrong password (or a tampered message; indistinguishable by design).
    BadPassword,
    Malformed(&'static str),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPassword => f.write_str("authentication failed: wrong password"),
            Self::Malformed(what) => write!(f, "malformed {what}"),
        }
    }
}

impl std::error::Error for AuthError {}

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str, what: &'static str) -> Result<Vec<u8>, AuthError> {
    B64.decode(s).map_err(|_| AuthError::Malformed(what))
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("OS randomness is available");
    buf
}
