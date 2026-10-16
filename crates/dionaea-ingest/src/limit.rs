//! A token-bucket rate limiter with a bounded number of buckets and an
//! injected clock. One instance per kind of key (write key, client address).
//! The address is only ever a map key here: it is neither stored elsewhere
//! nor logged.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};

/// How long to wait before trying again, in whole seconds (at least 1), for a
/// `Retry-After` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub u64);

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at_ms: u64,
}

/// Allows `rate_per_sec` requests per second on average per key, with bursts
/// of up to `burst`.
#[derive(Debug)]
pub struct RateLimiter<K> {
    rate_per_ms: f64,
    burst: f64,
    max_keys: usize,
    buckets: Mutex<HashMap<K, Bucket>>,
}

impl<K: Hash + Eq> RateLimiter<K> {
    /// `max_keys` bounds memory: when it is reached, buckets that have fully
    /// refilled are dropped first (they cannot be told apart from new ones),
    /// then an arbitrary one.
    ///
    /// # Panics
    ///
    /// If `rate_per_sec` is not positive, `burst` is below 1 or `max_keys` is 0.
    pub fn new(rate_per_sec: f64, burst: f64, max_keys: usize) -> Self {
        assert!(rate_per_sec > 0.0 && burst >= 1.0 && max_keys > 0);
        Self {
            rate_per_ms: rate_per_sec / 1000.0,
            burst,
            max_keys,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `key` at time `now_ms`, or says when to retry.
    pub fn check(&self, key: K, now_ms: u64) -> Result<(), RetryAfter> {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if !buckets.contains_key(&key) && buckets.len() >= self.max_keys {
            self.make_room(&mut buckets, now_ms);
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            at_ms: now_ms,
        });
        // A clock that steps back refills nothing rather than panicking.
        let elapsed = now_ms.saturating_sub(bucket.at_ms);
        bucket.tokens = (bucket.tokens + elapsed as f64 * self.rate_per_ms).min(self.burst);
        bucket.at_ms = bucket.at_ms.max(now_ms);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait_ms = (1.0 - bucket.tokens) / self.rate_per_ms;
            Err(RetryAfter((wait_ms / 1000.0).ceil().max(1.0) as u64))
        }
    }

    fn make_room(&self, buckets: &mut HashMap<K, Bucket>, now_ms: u64) {
        buckets.retain(|_, b| {
            let refilled = b.tokens + now_ms.saturating_sub(b.at_ms) as f64 * self.rate_per_ms;
            refilled < self.burst
        });
        if buckets.len() >= self.max_keys {
            let mut evict = true;
            buckets.retain(|_, _| !std::mem::take(&mut evict));
        }
    }

    /// How many keys are tracked.
    pub fn len(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
