//! Typed report queries.
//!
//! A caller sends a [`Query`] (a report, an interval, filters), never SQL.
//! [`validate`] checks it against the site's schema and the [`Limits`] and
//! returns a [`Plan`] or a typed [`QueryError`]; [`run`] executes a plan
//! through `dionaea-store`, which holds every statement. Validation is pure.
//!
//! Counts of users and sessions are exact. Buckets with no events have no row.

mod error;
mod model;
mod run;
#[cfg(test)]
mod tests;
mod validate;

pub use error::QueryError;
pub use model::{Dimension, Filter, Granularity, Measure, Query, Report, SessionDimension};
pub use run::{
    Answer, ElementUsage, Ranked, ScreenUsage, SeriesPoint, SessionRow, SessionStats, Trend, run,
};
pub use validate::{Limits, Plan, Site, is_time_zone_shape, validate};
