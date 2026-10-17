//! The request a caller sends: a report, an interval, filters. Nothing here
//! is trusted yet; [`crate::validate`] turns it into something that can run.

use serde::{Deserialize, Serialize};

/// A dimension events can be grouped or filtered by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
// Flattened into a filter, so unknown fields cannot be denied here.
#[serde(tag = "dimension", rename_all = "snake_case")]
pub enum Dimension {
    Country,
    Platform,
    Os,
    Browser,
    DeviceClass,
    AppVersion,
    Screen,
    Event,
    /// A text-like property that an event declares.
    Property {
        event: String,
        name: String,
    },
}

/// Keep what has one of the values on the dimension.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Filter {
    #[serde(flatten)]
    pub dimension: Dimension,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Hour,
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    /// Events stored.
    Events,
    /// Distinct devices.
    Users,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDimension {
    Country,
    Platform,
    AppVersion,
    EntryScreen,
    ExitScreen,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Report {
    /// Users, events and sessions with the change against the period just
    /// before.
    Overview,
    /// Events or devices over time, optionally split by a dimension.
    Series {
        measure: Measure,
        event: Option<String>,
        granularity: Granularity,
        breakdown: Option<Dimension>,
    },
    /// The most frequent values of a dimension.
    Top {
        dimension: Dimension,
        measure: Measure,
        event: Option<String>,
        limit: Option<u32>,
    },
    /// Views and taps per screen.
    Screens { limit: Option<u32> },
    /// Taps per element, on one screen or all.
    Elements {
        screen: Option<String>,
        limit: Option<u32>,
    },
    /// Session aggregates, optionally split by a dimension.
    Sessions {
        breakdown: Option<SessionDimension>,
        limit: Option<u32>,
    },
}

/// A report over `[from_ms, to_ms)`, in milliseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default)]
    pub filters: Vec<Filter>,
    pub report: Report,
}
