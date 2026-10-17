//! Passwords: a policy, and argon2id hashing.
//!
//! The default cost is the OWASP minimum for argon2id (19 MiB of memory, two
//! passes, one lane). A hash records its own cost, so raising the default
//! later leaves old hashes verifiable. Hashing takes tens of milliseconds and
//! all of that memory: callers on an async runtime run it on a blocking
//! thread.

use std::sync::{Arc, OnceLock};

use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::AuthError;

/// Shortest and longest password, in characters. The upper bound keeps one
/// login from costing more than hashing a short string.
pub const MIN_PASSWORD_CHARS: usize = 12;
pub const MAX_PASSWORD_CHARS: usize = 256;

const SALT_BYTES: usize = 16;

/// Checks a new password against the policy: long enough, not too long, and
/// more than whitespace.
pub fn check_policy(password: &str) -> Result<(), AuthError> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(AuthError::WeakPassword(format!(
            "at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(AuthError::WeakPassword(format!(
            "at most {MAX_PASSWORD_CHARS} characters"
        )));
    }
    if password.trim().is_empty() {
        return Err(AuthError::WeakPassword("not only whitespace".to_owned()));
    }
    Ok(())
}

fn hash_with(argon2: &Argon2<'_>, password: &str) -> Result<String, AuthError> {
    let mut salt = [0u8; SALT_BYTES];
    getrandom::fill(&mut salt).map_err(|e| AuthError::Random(e.to_string()))?;
    let hash = argon2
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(hash.to_string())
}

fn make_decoy(argon2: &Argon2<'_>) -> Result<String, AuthError> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| AuthError::Random(e.to_string()))?;
    hash_with(argon2, &format!("{random:?}"))
}

pub struct Hasher {
    argon2: Argon2<'static>,
    /// A hash of a random password nobody knows, verified against when the
    /// account does not exist so that a missing account takes as long as a
    /// wrong password. Made on a thread of its own when the hasher is, so
    /// starting a server does not wait for it; a login that needs it first
    /// waits for the thread.
    decoy: Arc<OnceLock<String>>,
}

impl std::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hasher").finish_non_exhaustive()
    }
}

impl Hasher {
    /// argon2id at the recommended minimum cost.
    pub fn new() -> Result<Self, AuthError> {
        Self::with_cost(
            Params::DEFAULT_M_COST,
            Params::DEFAULT_T_COST,
            Params::DEFAULT_P_COST,
        )
    }

    /// argon2id with an explicit cost: `memory_kib`, passes and lanes. For
    /// tests and benchmarks; a weaker cost than [`Hasher::new`] weakens every
    /// password hashed with it.
    pub fn with_cost(memory_kib: u32, passes: u32, lanes: u32) -> Result<Self, AuthError> {
        let params = Params::new(memory_kib, passes, lanes, None)
            .map_err(|e| AuthError::Hash(e.to_string()))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let hasher = Self {
            argon2,
            decoy: Arc::new(OnceLock::new()),
        };
        // Fails now, not on a login, if there is no randomness.
        getrandom::fill(&mut [0u8; 1]).map_err(|e| AuthError::Random(e.to_string()))?;
        let (argon2, decoy) = (hasher.argon2.clone(), Arc::clone(&hasher.decoy));
        let _ = std::thread::Builder::new()
            .name("dionaea-decoy".to_owned())
            .spawn(move || {
                let _ = decoy.get_or_init(|| make_decoy(&argon2).unwrap_or_default());
            });
        Ok(hasher)
    }

    /// The argon2id hash of a password, as a PHC string with a fresh salt.
    pub fn hash(&self, password: &str) -> Result<String, AuthError> {
        hash_with(&self.argon2, password)
    }

    /// Whether the password matches the hash. A malformed hash does not match.
    pub fn verify(&self, password: &str, hash: &str) -> bool {
        self.argon2
            .verify_password(password.as_bytes(), hash)
            .is_ok()
    }

    /// Spends the time of a verification on nothing, for an account that does
    /// not exist.
    pub fn verify_decoy(&self, password: &str) {
        let decoy = self
            .decoy
            .get_or_init(|| make_decoy(&self.argon2).unwrap_or_default());
        let _ = self.verify(password, decoy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> Hasher {
        Hasher::with_cost(8, 1, 1).expect("a valid cost")
    }

    #[test]
    fn a_password_verifies_against_its_own_hash_only() {
        let hasher = fast();
        let hash = hasher.hash("correct horse battery").expect("hash");
        assert!(hash.starts_with("$argon2id$v=19$"), "{hash}");
        assert!(hasher.verify("correct horse battery", &hash));
        assert!(!hasher.verify("correct horse batterz", &hash));
        assert!(!hasher.verify("", &hash));
    }

    #[test]
    fn every_hash_has_its_own_salt() {
        let hasher = fast();
        let a = hasher.hash("correct horse battery").expect("hash");
        let b = hasher.hash("correct horse battery").expect("hash");
        assert_ne!(a, b);
    }

    #[test]
    fn a_hash_made_at_another_cost_still_verifies() {
        let old = Hasher::with_cost(8, 1, 1).expect("cost");
        let hash = old.hash("correct horse battery").expect("hash");
        let newer = Hasher::with_cost(16, 2, 1).expect("cost");
        assert!(newer.verify("correct horse battery", &hash));
    }

    #[test]
    fn a_malformed_hash_never_matches() {
        let hasher = fast();
        for hash in [
            "",
            "plain",
            "$argon2id$",
            "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$",
        ] {
            assert!(!hasher.verify("correct horse battery", hash), "{hash:?}");
        }
    }

    #[test]
    fn the_default_cost_is_the_recommended_minimum() {
        let hasher = Hasher::new().expect("hasher");
        let hash = hasher.hash("correct horse battery").expect("hash");
        assert!(hash.contains("m=19456,t=2,p=1"), "{hash}");
    }

    #[test]
    fn the_policy_bounds_length_and_refuses_blank_passwords() {
        assert!(check_policy("correct horse").is_ok());
        assert!(check_policy("short").is_err());
        assert!(check_policy("           ").is_err());
        assert!(check_policy(&"x".repeat(MAX_PASSWORD_CHARS)).is_ok());
        assert!(check_policy(&"x".repeat(MAX_PASSWORD_CHARS + 1)).is_err());
        // Characters, not bytes.
        assert!(check_policy(&"\u{e9}".repeat(12)).is_ok());
    }

    #[test]
    fn bad_costs_are_refused() {
        assert!(Hasher::with_cost(1, 1, 1).is_err());
        assert!(Hasher::with_cost(8, 0, 1).is_err());
    }
}
