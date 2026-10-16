//! The ingest path as plain functions, with no HTTP server, no spool and no
//! clock of its own: admission of a write key ([`Cache`]), request limits
//! ([`RateLimiter`]) and the pipeline from a request body to accepted events
//! and drop counts ([`process`]). The HTTP handlers call these and the
//! batcher stores the result; keeping them apart means every rule here is
//! tested without a socket or a wall clock.

mod batcher;
mod cache;
mod dedup;
mod drops;
mod enrich;
mod limit;
mod pipeline;
mod sink;

pub use batcher::{Batcher, BatcherConfig, BatcherStats, Destination};
pub use cache::{Admission, Cache, Refusal};
pub use dedup::Deduper;
pub use drops::DropAggregator;
pub use enrich::{CountryLookup, Enrichment, MaxMind, UserAgent, parse_user_agent};
pub use limit::{RateLimiter, RetryAfter};
pub use pipeline::{
    Accepted, Context, DropCount, Limits, Processed, Reason, UNDECLARED_EVENT, UNPARSED_EVENT,
    process, process_with,
};
pub use sink::{CHUNK_BYTES, Enqueued, INVALID_ROW, enqueue, enqueue_drop_rows, enqueue_identity};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod batch_tests;

#[cfg(test)]
mod enrich_tests;
