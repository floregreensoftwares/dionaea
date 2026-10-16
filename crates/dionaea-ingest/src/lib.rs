//! The ingest path as plain functions, with no HTTP server, no spool and no
//! clock of its own: admission of a write key ([`Cache`]), request limits
//! ([`RateLimiter`]) and the pipeline from a request body to accepted events
//! and drop counts ([`process`]). The HTTP handlers call these and the
//! batcher stores the result; keeping them apart means every rule here is
//! tested without a socket or a wall clock.

mod cache;
mod limit;
mod pipeline;

pub use cache::{Admission, Cache, Refusal};
pub use limit::{RateLimiter, RetryAfter};
pub use pipeline::{
    Accepted, DropCount, Limits, Processed, Reason, UNDECLARED_EVENT, UNPARSED_EVENT, process,
};

#[cfg(test)]
mod tests;
