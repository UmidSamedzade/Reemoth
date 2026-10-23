use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::Zeroize;

use crate::{AuthError, b64, random_bytes as random, unb64};

/// Argon2id(password, salt): what both sides feed to SPAKE2.
///
/// The host stores this instead of the plaintext password. It is still a
/// password equivalent for this host, so the host keeps it private.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordSecret([u8; 32]);

/// About 50 ms on a desktop CPU; runs once per connection attempt.
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;

pub const MIN_PASSWORD_LEN: usize = 8;
const SALT_LEN: usize = 16;

impl PasswordSecret {
    pub fn new_salt() -> [u8; SALT_LEN] {
        random()
    }

    /// The salt comes from the peer, so its length is checked, not trusted.
    pub fn derive(password: &str, salt: &[u8]) -> Result<Self, AuthError> {
        if salt.len() != SALT_LEN {
            return Err(AuthError::Malformed("password salt"));
        }
        let params = Params::new(MEMORY_KIB, ITERATIONS, 1, Some(32)).expect("valid params");
        let mut out = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(password.as_bytes(), salt, &mut out)
            .map_err(|_| AuthError::Malformed("password salt"))?;
        Ok(Self(out))
    }

    /// Rejects passwords too weak for an internet-facing host.
    pub fn check_strength(password: &str) -> Result<(), String> {
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            ));
        }
        if password.chars().all(|c| c.is_ascii_digit()) && password.len() < 12 {
            return Err("an all-digit password must be at least 12 digits".into());
        }
        Ok(())
    }

    pub fn to_base64(&self) -> String {
        b64(&self.0)
    }

    pub fn from_base64(s: &str) -> Result<Self, AuthError> {
        let bytes: [u8; 32] = unb64(s, "password secret")?
            .try_into()
            .map_err(|_| AuthError::Malformed("password secret"))?;
        Ok(Self(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PasswordSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for PasswordSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordSecret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic_per_salt() {
        let a = PasswordSecret::derive("correct horse", b"salt-salt-salt-1").unwrap();
        let b = PasswordSecret::derive("correct horse", b"salt-salt-salt-1").unwrap();
        let c = PasswordSecret::derive("correct horse", b"salt-salt-salt-2").unwrap();
        assert!(PasswordSecret::derive("correct horse", b"short").is_err());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(PasswordSecret::from_base64(&a.to_base64()).unwrap(), a);
    }

    #[test]
    fn strength() {
        assert!(PasswordSecret::check_strength("short").is_err());
        assert!(PasswordSecret::check_strength("12345678").is_err());
        assert!(PasswordSecret::check_strength("long enough").is_ok());
    }
}
