//! Drop counts of requests that kept no event, held in memory and written to the
//! spool together every few seconds.
//!
//! A request whose every event was dropped (a crawler, a stale schema, a
//! retried batch) has nothing worth an fsync: only counts. Writing one spool
//! record per such request would let a flood of them cost as much as real
//! traffic. They are merged per site, event name, reason and day instead.
//! A crash loses the counts of the last interval, never an event.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use dionaea_store::DropRow;

use crate::pipeline::DropCount;

const DAY_MS: u64 = 86_400_000;

type Key = (u32, String, String, u64);

/// Counts waiting to be written, bounded in the number of distinct keys.
#[derive(Debug)]
pub struct DropAggregator {
    max_keys: usize,
    counts: Mutex<BTreeMap<Key, u64>>,
}

impl DropAggregator {
    /// Holds at most `max_keys` distinct (site, event, reason, day) counts;
    /// past that, [`add`](Self::add) reports that a flush is due.
    pub fn new(max_keys: usize) -> Self {
        Self {
            max_keys,
            counts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Adds the drops of one request. Returns whether the table is full and
    /// should be flushed now.
    pub fn add(&self, site_id: u32, drops: &[DropCount], at_ms: u64) -> bool {
        let day = at_ms / DAY_MS;
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        for drop in drops {
            let key = (
                site_id,
                drop.event_name.clone(),
                drop.reason.as_str().to_owned(),
                day,
            );
            *counts.entry(key).or_default() += drop.count;
        }
        counts.len() >= self.max_keys
    }

    /// Takes everything held, as rows for the store.
    pub fn take(&self) -> Vec<DropRow> {
        let taken =
            std::mem::take(&mut *self.counts.lock().unwrap_or_else(PoisonError::into_inner));
        taken
            .into_iter()
            .map(|((site_id, event_name, reason, day), count)| DropRow {
                site_id,
                event_name,
                reason,
                at_ms: day * DAY_MS,
                count,
            })
            .collect()
    }

    /// Puts back rows that could not be written, so they are tried again.
    pub fn restore(&self, rows: Vec<DropRow>) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        for row in rows {
            *counts
                .entry((row.site_id, row.event_name, row.reason, row.at_ms / DAY_MS))
                .or_default() += row.count;
        }
    }

    /// How many distinct counts are held.
    pub fn len(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
