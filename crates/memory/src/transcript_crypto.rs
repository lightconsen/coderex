//! Encryption key derivation for transcript encryption.
//!
//! Uses HKDF to derive per-session keys from a master key,
//! enabling session-level key isolation for `aes-gcm` encryption.

use aes_gcm::{Aes256Gcm, KeyInit};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;

/// Errors from key derivation.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// Invalid master key hex string.
    #[error("invalid master key: {0}")]
    InvalidKey(String),
}

/// Derive a per-session AES-256-GCM cipher from a master key.
///
/// The master key should be a hex-encoded 32-byte (64 hex char) key.
/// Each session gets a unique key via HKDF(master_key, session_id).
pub fn derive_cipher(
    master_key_hex: &str,
    session_id: &str,
) -> Result<Aes256Gcm, CryptoError> {
    let master_key = hex::decode(master_key_hex)
        .map_err(|e| CryptoError::InvalidKey(e.to_string()))?;
    if master_key.len() != 32 {
        return Err(CryptoError::InvalidKey(
            format!("expected 32-byte key, got {} bytes", master_key.len()),
        ));
    }

    let hkdf = Hkdf::<Sha256>::new(None, &master_key);
    let mut okm = [0u8; 32];
    hkdf.expand(session_id.as_bytes(), &mut okm)
        .map_err(|e| CryptoError::InvalidKey(format!("hkdf expand failed: {e}")))?;

    Ok(Aes256Gcm::new(&okm.into()))
}

/// Generate a random 256-bit master key as hex string.
#[must_use]
pub fn generate_master_key() -> String {
    use rand_core::{OsRng, RngCore};
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    hex::encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_cipher_returns_valid_cipher() {
        let key = generate_master_key();
        let cipher = derive_cipher(&key, "test_session").unwrap();
        let _ = cipher;
    }

    #[test]
    fn derive_cipher_same_key_different_session() {
        let key = "a".repeat(64); // 32 bytes of 0xaa
        let c1 = derive_cipher(&key, "session_a").unwrap();
        let c2 = derive_cipher(&key, "session_b").unwrap();
        let _ = (c1, c2);
    }

    #[test]
    fn invalid_hex_returns_error() {
        assert!(derive_cipher("not-hex", "s1").is_err());
    }

    #[test]
    fn wrong_length_returns_error() {
        assert!(derive_cipher("aabb", "s1").is_err()); // 2 bytes, not 32
    }
}
