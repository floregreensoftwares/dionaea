//! A bounded memory of recent event ids, so an event an SDK sends again after
//! a lost response is dropped instead of stored twice.
//!
//! An id is remembered only after its event is durable ([`Deduper::remember`]),
//! never before: remembering an id whose request then failed with `503` would
//! make the retry look like a duplicate and lose the event.
//!
//! The memory is per node and per process. It catches the retry that matters
//! (the same SDK, the same node, within the window); a retry that reaches
//! another replica or arrives after a restart is stored again, and the
//! deduplication of the insert itself only covers a batch re-read from the
//! spool.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};

#[derive(Debug, Default)]
struct Window {
    ids: HashSet<u128>,
    order: VecDeque<(u128, u64)>,
}

/// Remembers up to `capacity` ids for `window_ms` milliseconds each.
#[derive(Debug)]
pub struct Deduper {
    capacity: usize,
    window_ms: u64,
    state: Mutex<Window>,
}

impl Deduper {
    /// `capacity` 0 remembers nothing, which turns the check off.
    pub fn new(capacity: usize, window_ms: u64) -> Self {
        Self {
            capacity,
            window_ms,
            state: Mutex::new(Window::default()),
        }
    }

    /// Whether `id` was remembered and has not expired at `now_ms`.
    pub fn contains(&self, id: u128, now_ms: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.expire(&mut state, now_ms);
        state.ids.contains(&id)
    }

    /// Remembers `ids` as seen at `now_ms`. When full, the oldest are forgotten.
    pub fn remember(&self, ids: impl IntoIterator<Item = u128>, now_ms: u64) {
        if self.capacity == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.expire(&mut state, now_ms);
        for id in ids {
            if state.ids.insert(id) {
                state.order.push_back((id, now_ms));
            }
        }
        while state.ids.len() > self.capacity {
            if let Some((oldest, _)) = state.order.pop_front() {
                state.ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn expire(&self, state: &mut Window, now_ms: u64) {
        while let Some(&(id, at)) = state.order.front() {
            if now_ms.saturating_sub(at) < self.window_ms {
                break;
            }
            state.order.pop_front();
            state.ids.remove(&id);
        }
    }

    /// How many ids are remembered.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ids
            .len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
