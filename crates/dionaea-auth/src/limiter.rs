//! Limits failed logins so a password cannot be guessed by trying.
//!
//! Failures are counted per key (an e-mail address, a client address) in a
//! fixed window. The table is bounded: when it is full the oldest window is
//! dropped first, so an attacker cannot grow memory by inventing keys. The
//! cost of the bound is that someone who can send enough failures from
//! enough keys can make the limiter forget a victim; it is a speed bump
//! against guessing, not a replacement for a long password.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// Most keys remembered at once.
const MAX_KEYS: usize = 10_000;

#[derive(Debug)]
pub struct LoginLimiter {
    max_failures: u32,
    window_secs: i64,
    entries: Mutex<HashMap<String, Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started: i64,
    failures: u32,
}

impl LoginLimiter {
    /// At most `max_failures` failures per key in each `window_secs`.
    pub fn new(max_failures: u32, window_secs: i64) -> Self {
        Self {
            max_failures,
            window_secs,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Seconds to wait before `key` may try again, or `None` if it may now.
    pub fn blocked_for(&self, key: &str, now: i64) -> Option<i64> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let window = entries.get(key)?;
        let ends = window.started.saturating_add(self.window_secs);
        (window.failures >= self.max_failures && now < ends).then(|| ends - now)
    }

    pub fn record_failure(&self, key: &str, now: i64) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= MAX_KEYS && !entries.contains_key(key) {
            entries.retain(|_, w| now < w.started.saturating_add(self.window_secs));
            if entries.len() >= MAX_KEYS {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, w)| w.started)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    entries.remove(&oldest);
                }
            }
        }
        let window = entries.entry(key.to_owned()).or_insert(Window {
            started: now,
            failures: 0,
        });
        if now >= window.started.saturating_add(self.window_secs) {
            *window = Window {
                started: now,
                failures: 0,
            };
        }
        window.failures = window.failures.saturating_add(1);
    }

    /// Forgets a key after a successful login.
    pub fn clear(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_blocked_after_too_many_failures_until_the_window_ends() {
        let limiter = LoginLimiter::new(3, 60);
        for _ in 0..2 {
            limiter.record_failure("a", 100);
        }
        assert_eq!(limiter.blocked_for("a", 100), None);
        limiter.record_failure("a", 110);
        assert_eq!(limiter.blocked_for("a", 120), Some(40));
        assert_eq!(limiter.blocked_for("b", 120), None);
        assert_eq!(limiter.blocked_for("a", 160), None);
    }

    #[test]
    fn a_new_window_starts_counting_again() {
        let limiter = LoginLimiter::new(2, 60);
        limiter.record_failure("a", 0);
        limiter.record_failure("a", 1);
        assert!(limiter.blocked_for("a", 2).is_some());
        limiter.record_failure("a", 100);
        assert_eq!(limiter.blocked_for("a", 101), None);
    }

    #[test]
    fn a_success_clears_the_key() {
        let limiter = LoginLimiter::new(1, 60);
        limiter.record_failure("a", 0);
        assert!(limiter.blocked_for("a", 1).is_some());
        limiter.clear("a");
        assert_eq!(limiter.blocked_for("a", 1), None);
    }

    #[test]
    fn the_table_never_grows_past_its_bound() {
        let limiter = LoginLimiter::new(5, 1_000_000_000);
        for i in 0..(MAX_KEYS + 500) {
            limiter.record_failure(&format!("key{i}"), i64::try_from(i).expect("small"));
        }
        assert_eq!(limiter.len(), MAX_KEYS);
        // The oldest are the ones forgotten.
        assert_eq!(limiter.blocked_for("key0", 0), None);
        limiter.record_failure("key10499", 20_000);
        assert_eq!(limiter.len(), MAX_KEYS);
    }
}
