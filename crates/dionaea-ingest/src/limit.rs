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

#[derive(Debug)]
struct State<K> {
    rate_per_ms: f64,
    burst: f64,
    max_keys: usize,
    buckets: HashMap<K, Bucket>,
}

/// Allows `rate_per_sec` requests per second on average per key, with bursts
/// of up to `burst`. The rate, the burst and the number of keys can be changed
/// while it is in use ([`reconfigure`](Self::reconfigure)).
///
/// Bounded memory has a price: under a flood of distinct keys an arbitrary
/// bucket can be evicted, which resets that key to a full burst. Apply it to
/// keys that already passed admission, or to client addresses behind a
/// network-level limit, never to unchecked strings from the request.
#[derive(Debug)]
pub struct RateLimiter<K> {
    state: Mutex<State<K>>,
}

fn valid(rate_per_sec: f64, burst: f64, max_keys: usize) -> bool {
    rate_per_sec.is_finite()
        && rate_per_sec > 0.0
        && burst.is_finite()
        && burst >= 1.0
        && max_keys > 0
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
        assert!(valid(rate_per_sec, burst, max_keys));
        Self {
            state: Mutex::new(State {
                rate_per_ms: rate_per_sec / 1000.0,
                burst,
                max_keys,
                buckets: HashMap::new(),
            }),
        }
    }

    /// Changes the limits from now on. Buckets keep their tokens, capped by
    /// the new burst; if there are more keys than the new bound, arbitrary ones
    /// are forgotten at once. Values that [`new`](Self::new) would refuse are
    /// ignored.
    pub fn reconfigure(&self, rate_per_sec: f64, burst: f64, max_keys: usize) {
        if !valid(rate_per_sec, burst, max_keys) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.rate_per_ms = rate_per_sec / 1000.0;
        state.burst = burst;
        state.max_keys = max_keys;
        for bucket in state.buckets.values_mut() {
            bucket.tokens = bucket.tokens.min(burst);
        }
        let mut remaining = state.buckets.len().saturating_sub(max_keys);
        if remaining > 0 {
            state.buckets.retain(|_, _| {
                if remaining > 0 {
                    remaining -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Takes one token for `key` at time `now_ms`, or says when to retry.
    pub fn check(&self, key: K, now_ms: u64) -> Result<(), RetryAfter> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let State {
            rate_per_ms,
            burst,
            max_keys,
            buckets,
        } = &mut *state;
        if !buckets.contains_key(&key) && buckets.len() >= *max_keys {
            make_room(buckets, *max_keys, *rate_per_ms, *burst, now_ms);
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: *burst,
            at_ms: now_ms,
        });
        // A clock that steps back refills nothing rather than panicking.
        let elapsed = now_ms.saturating_sub(bucket.at_ms);
        bucket.tokens = (bucket.tokens + elapsed as f64 * *rate_per_ms).min(*burst);
        bucket.at_ms = bucket.at_ms.max(now_ms);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait_ms = (1.0 - bucket.tokens) / *rate_per_ms;
            Err(RetryAfter((wait_ms / 1000.0).ceil().max(1.0) as u64))
        }
    }

    /// How many keys are tracked.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn make_room<K: Hash + Eq>(
    buckets: &mut HashMap<K, Bucket>,
    max_keys: usize,
    rate_per_ms: f64,
    burst: f64,
    now_ms: u64,
) {
    buckets.retain(|_, b| {
        let refilled = b.tokens + now_ms.saturating_sub(b.at_ms) as f64 * rate_per_ms;
        refilled < burst
    });
    if buckets.len() >= max_keys {
        let mut evict = true;
        buckets.retain(|_, _| !std::mem::take(&mut evict));
    }
}
