use ed25519_dalek::{Signer, SigningKey};
use reemoth_proto::signal::{HostId, host_auth_message};

use crate::{AuthError, b64, random_bytes as random, unb64};

/// The Ed25519 key that owns a host ID on the signaling server.
pub struct HostIdentity {
    key: SigningKey,
}

impl HostIdentity {
    pub fn generate() -> Self {
        Self {
            key: SigningKey::from_bytes(&random::<32>()),
        }
    }

    pub fn from_base64(secret: &str) -> Result<Self, AuthError> {
        let bytes: [u8; 32] = unb64(secret, "host key")?
            .try_into()
            .map_err(|_| AuthError::Malformed("host key"))?;
        Ok(Self {
            key: SigningKey::from_bytes(&bytes),
        })
    }

    pub fn secret_base64(&self) -> String {
        b64(self.key.as_bytes())
    }

    pub fn public_base64(&self) -> String {
        b64(self.key.verifying_key().as_bytes())
    }

    /// Answers a signaling `Challenge`; returns the base64 signature.
    pub fn sign_challenge(&self, id: HostId, nonce_b64: &str) -> Result<String, AuthError> {
        let nonce = unb64(nonce_b64, "challenge nonce")?;
        Ok(b64(&self
            .key
            .sign(&host_auth_message(id, &nonce))
            .to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, VerifyingKey};

    #[test]
    fn signature_verifies_against_auth_message() {
        let identity = HostIdentity::generate();
        let restored = HostIdentity::from_base64(&identity.secret_base64()).unwrap();
        assert_eq!(restored.public_base64(), identity.public_base64());

        let id = HostId::new(123_456_789).unwrap();
        let nonce = [7u8; 32];
        let sig = restored.sign_challenge(id, &b64(&nonce)).unwrap();

        let key: [u8; 32] = unb64(&identity.public_base64(), "k")
            .unwrap()
            .try_into()
            .unwrap();
        let key = VerifyingKey::from_bytes(&key).unwrap();
        let sig = Signature::from_slice(&unb64(&sig, "s").unwrap()).unwrap();
        key.verify_strict(&host_auth_message(id, &nonce), &sig)
            .unwrap();
        assert!(
            key.verify_strict(&host_auth_message(id, &[0u8; 32]), &sig)
                .is_err()
        );
    }
}
