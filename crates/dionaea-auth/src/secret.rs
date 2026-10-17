//! Session and read-token secrets: 256 random bits, shown once, stored only
//! as a SHA-256.
//!
//! A hash without a salt or a work factor is enough here because the secret
//! is random and long; there is nothing to guess. (A password is not, and is
//! hashed with argon2id in [`crate::password`].)

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::AuthError;

pub const SESSION_PREFIX: &str = "ds_";
pub const READ_TOKEN_PREFIX: &str = "rt_";
/// 32 random bytes in lowercase base32 without padding.
const BODY_CHARS: usize = 52;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A secret and its hash. The secret is not stored anywhere; give it to the
/// person once.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    pub value: String,
    pub hash: String,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("value", &"<hidden>")
            .field("hash", &self.hash)
            .finish()
    }
}

pub fn generate(prefix: &str) -> Result<Secret, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Random(e.to_string()))?;
    let mut value = String::with_capacity(prefix.len() + BODY_CHARS);
    value.push_str(prefix);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            value.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        value.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    let hash = hash(&value);
    Ok(Secret { value, hash })
}

/// The SHA-256 of a secret, in lowercase hex.
pub fn hash(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether `text` has the shape of a secret with this prefix. Callers check
/// before looking anything up, so garbage costs no query.
pub fn has_shape(prefix: &str, text: &str) -> bool {
    text.strip_prefix(prefix).is_some_and(|body| {
        body.len() == BODY_CHARS
            && body
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_has_its_prefix_and_shape_and_is_never_repeated() {
        let a = generate(SESSION_PREFIX).expect("secret");
        let b = generate(SESSION_PREFIX).expect("secret");
        assert!(has_shape(SESSION_PREFIX, &a.value));
        assert!(!has_shape(READ_TOKEN_PREFIX, &a.value));
        assert_ne!(a.value, b.value);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_secret() {
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let secret = generate(READ_TOKEN_PREFIX).expect("secret");
        assert_eq!(secret.hash, hash(&secret.value));
        assert_eq!(secret.hash.len(), 64);
    }

    #[test]
    fn the_debug_form_hides_the_secret() {
        let secret = generate(SESSION_PREFIX).expect("secret");
        assert!(!format!("{secret:?}").contains(&secret.value));
    }

    #[test]
    fn every_character_of_the_body_comes_from_the_alphabet() {
        // The body is 256 bits in 52 characters of 5 bits: the last carries 1.
        for _ in 0..200 {
            let secret = generate(SESSION_PREFIX).expect("secret");
            assert!(has_shape(SESSION_PREFIX, &secret.value), "{}", secret.value);
        }
    }

    #[test]
    fn garbage_has_no_shape() {
        for text in [
            "",
            "ds_",
            "ds_short",
            &format!("ds_{}", "A".repeat(52)),
            &format!("ds_{}", "1".repeat(52)),
            &format!("ds_{}", "a".repeat(53)),
            &format!("xx_{}", "a".repeat(52)),
        ] {
            assert!(!has_shape(SESSION_PREFIX, text), "{text:?}");
        }
        assert!(has_shape(SESSION_PREFIX, &format!("ds_{}", "a".repeat(52))));
    }
}
