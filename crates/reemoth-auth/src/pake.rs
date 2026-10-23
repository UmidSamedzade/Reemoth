use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use reemoth_proto::signal::{HostId, SessionId};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::{AuthError, PasswordSecret};

type HmacSha256 = Hmac<Sha256>;

const CLIENT_IDENTITY: &[u8] = b"reemoth-client";

fn host_identity(id: HostId) -> Vec<u8> {
    format!("reemoth-host {id}").into_bytes()
}

/// Host side of SPAKE2 (role B).
pub struct HostPake {
    state: Spake2<Ed25519Group>,
    id: HostId,
    session: SessionId,
}

impl HostPake {
    /// Returns the state and the message to send in `PeerMsg::Hello`.
    pub fn start(secret: &PasswordSecret, id: HostId, session: SessionId) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(secret.as_bytes()),
            &Identity::new(CLIENT_IDENTITY),
            &Identity::new(&host_identity(id)),
        );
        (Self { state, id, session }, msg)
    }

    /// Consumes the client's `Auth` message and checks its key confirmation.
    pub fn finish(
        self,
        client_msg: &[u8],
        client_confirm: &[u8],
    ) -> Result<SessionKeys, AuthError> {
        let key = self
            .state
            .finish(client_msg)
            .map_err(|_| AuthError::Malformed("PAKE message"))?;
        let keys = SessionKeys::derive(&key, self.id, self.session);
        keys.verify(&keys.client_confirm, b"confirm", client_confirm)?;
        Ok(keys)
    }
}

/// Client side of SPAKE2 (role A).
pub struct ClientPake {
    state: Spake2<Ed25519Group>,
    id: HostId,
}

impl ClientPake {
    /// Returns the state and the message to send in `PeerMsg::Auth`.
    pub fn start(secret: &PasswordSecret, id: HostId) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(secret.as_bytes()),
            &Identity::new(CLIENT_IDENTITY),
            &Identity::new(&host_identity(id)),
        );
        (Self { state, id }, msg)
    }

    /// Consumes the host's `Hello` message. Returns the keys; send
    /// [`SessionKeys::client_confirmation`] with the `Auth` message.
    pub fn finish(self, host_msg: &[u8], session: SessionId) -> Result<SessionKeys, AuthError> {
        let key = self
            .state
            .finish(host_msg)
            .map_err(|_| AuthError::Malformed("PAKE message"))?;
        Ok(SessionKeys::derive(&key, self.id, session))
    }
}

/// Keys derived from the PAKE output, bound to the host ID and session.
pub struct SessionKeys {
    client_confirm: [u8; 32],
    host_confirm: [u8; 32],
    sdp: [u8; 32],
}

impl SessionKeys {
    fn derive(pake_key: &[u8], id: HostId, session: SessionId) -> Self {
        let hk = Hkdf::<Sha256>::new(None, pake_key);
        let expand = |label: &str| {
            let mut out = [0u8; 32];
            hk.expand(
                format!("reemoth v1 {label} {id} {session}").as_bytes(),
                &mut out,
            )
            .expect("32 bytes is a valid HKDF length");
            out
        };
        Self {
            client_confirm: expand("client-confirm"),
            host_confirm: expand("host-confirm"),
            sdp: expand("sdp"),
        }
    }

    pub fn client_confirmation(&self) -> Vec<u8> {
        mac(&self.client_confirm, b"confirm")
    }

    pub fn host_confirmation(&self) -> Vec<u8> {
        mac(&self.host_confirm, b"confirm")
    }

    pub fn verify_host_confirmation(&self, tag: &[u8]) -> Result<(), AuthError> {
        self.verify(&self.host_confirm, b"confirm", tag)
    }

    /// MAC over an SDP. `kind` is `"offer"` or `"answer"` so one cannot be
    /// replayed as the other.
    pub fn sdp_mac(&self, kind: &str, sdp: &str) -> Vec<u8> {
        mac(&self.sdp, &sdp_input(kind, sdp))
    }

    pub fn verify_sdp(&self, kind: &str, sdp: &str, tag: &[u8]) -> Result<(), AuthError> {
        self.verify(&self.sdp, &sdp_input(kind, sdp), tag)
    }

    fn verify(&self, key: &[u8; 32], data: &[u8], tag: &[u8]) -> Result<(), AuthError> {
        let mut m = HmacSha256::new_from_slice(key).expect("any key length works");
        m.update(data);
        m.verify_slice(tag).map_err(|_| AuthError::BadPassword)
    }
}

fn sdp_input(kind: &str, sdp: &str) -> Vec<u8> {
    let mut v = kind.as_bytes().to_vec();
    v.push(0);
    v.extend_from_slice(sdp.as_bytes());
    v
}

fn mac(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("any key length works");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(host_pw: &str, client_pw: &str) -> Result<(SessionKeys, SessionKeys), AuthError> {
        let salt = b"0123456789abcdef";
        let id = HostId::new(5).unwrap();
        let session = 77;
        let (host, hello) =
            HostPake::start(&PasswordSecret::derive(host_pw, salt).unwrap(), id, session);
        let (client, auth) =
            ClientPake::start(&PasswordSecret::derive(client_pw, salt).unwrap(), id);
        let client_keys = client.finish(&hello, session)?;
        let host_keys = host.finish(&auth, &client_keys.client_confirmation())?;
        client_keys.verify_host_confirmation(&host_keys.host_confirmation())?;
        Ok((host_keys, client_keys))
    }

    #[test]
    fn matching_passwords_agree() {
        let (host, client) = run("hunter22hunter", "hunter22hunter").unwrap();
        let mac = client.sdp_mac("offer", "v=0");
        host.verify_sdp("offer", "v=0", &mac).unwrap();
        assert_eq!(
            host.verify_sdp("answer", "v=0", &mac),
            Err(AuthError::BadPassword)
        );
        assert_eq!(
            host.verify_sdp("offer", "v=1", &mac),
            Err(AuthError::BadPassword)
        );
    }

    #[test]
    fn wrong_password_fails_at_host() {
        assert!(matches!(
            run("hunter22hunter", "hunter22hunteR"),
            Err(AuthError::BadPassword)
        ));
    }
}
