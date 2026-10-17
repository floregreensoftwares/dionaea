use std::fmt;

/// Why a query was refused or failed. Every refusal before ClickHouse is
/// involved is one of the first group; messages name declared things only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The interval is empty or reversed.
    EmptyRange,
    /// A time before 1970 or after 2100.
    TimeOutOfRange,
    /// The interval is longer than allowed for the granularity.
    RangeTooLarge { max_days: u32 },
    /// The event is neither built in nor declared by any active schema version.
    UndeclaredEvent(String),
    /// The event does not declare the property.
    UndeclaredProperty { event: String, name: String },
    /// The property is a number or its kind differs between schema versions,
    /// so it has no values to group or filter by.
    NotGroupable { event: String, name: String },
    /// The query names more than one event, so nothing can match.
    ConflictingEvents,
    /// More filters than allowed.
    TooManyFilters { max: usize },
    /// A filter lists no values, too many, or one that is empty or too long.
    InvalidFilterValues,
    /// A filter this report cannot apply (sessions have no OS, for example).
    FilterNotAvailable,
    /// A limit above the maximum, or zero.
    InvalidLimit { max: u32 },
    /// The breakdown has more distinct values than allowed.
    BreakdownTooHigh { max: u32 },
    /// The time zone name is not usable.
    InvalidTimeZone,
    /// The read took longer than the limit.
    TooSlow,
    /// The answer would have more rows than the limit.
    TooManyRows,
    /// ClickHouse could not be reached or refused the read.
    Unavailable(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRange => f.write_str("the interval is empty"),
            Self::TimeOutOfRange => f.write_str("times are between 1970 and 2100"),
            Self::RangeTooLarge { max_days } => {
                write!(f, "the interval is longer than {max_days} days")
            }
            Self::UndeclaredEvent(name) => write!(f, "event `{name}` is not declared"),
            Self::UndeclaredProperty { event, name } => {
                write!(f, "event `{event}` does not declare property `{name}`")
            }
            Self::NotGroupable { event, name } => write!(
                f,
                "property `{name}` of `{event}` is numeric or changed kind between schema versions"
            ),
            Self::ConflictingEvents => f.write_str("the query names more than one event"),
            Self::TooManyFilters { max } => write!(f, "at most {max} filters"),
            Self::InvalidFilterValues => {
                f.write_str("a filter lists between 1 and 100 values, each 1 to 256 bytes")
            }
            Self::FilterNotAvailable => f.write_str("this report cannot filter by that dimension"),
            Self::InvalidLimit { max } => write!(f, "the limit is between 1 and {max}"),
            Self::BreakdownTooHigh { max } => {
                write!(f, "the breakdown has more than {max} distinct values")
            }
            Self::InvalidTimeZone => f.write_str("the time zone is not usable"),
            Self::TooSlow => f.write_str("the query took too long"),
            Self::TooManyRows => f.write_str("the answer has too many rows"),
            Self::Unavailable(message) => write!(f, "the store is unavailable: {message}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl QueryError {
    /// A stable machine-readable name for the error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyRange => "empty_range",
            Self::TimeOutOfRange => "time_out_of_range",
            Self::RangeTooLarge { .. } => "range_too_large",
            Self::UndeclaredEvent(_) => "undeclared_event",
            Self::UndeclaredProperty { .. } => "undeclared_property",
            Self::NotGroupable { .. } => "not_groupable",
            Self::ConflictingEvents => "conflicting_events",
            Self::TooManyFilters { .. } => "too_many_filters",
            Self::InvalidFilterValues => "invalid_filter_values",
            Self::FilterNotAvailable => "filter_not_available",
            Self::InvalidLimit { .. } => "invalid_limit",
            Self::BreakdownTooHigh { .. } => "breakdown_too_high",
            Self::InvalidTimeZone => "invalid_time_zone",
            Self::TooSlow => "too_slow",
            Self::TooManyRows => "too_many_rows",
            Self::Unavailable(_) => "unavailable",
        }
    }

    /// Whether the caller asked for something it may not (as opposed to the
    /// server failing to answer).
    pub fn is_client_error(&self) -> bool {
        !matches!(self, Self::Unavailable(_))
    }
}
