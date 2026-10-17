//! Runs a validated [`Plan`] and shapes the rows into an [`Answer`].

use dionaea_store::report as store;
use dionaea_store::{Error, Store};
use serde::Serialize;
use serde_json::Value;

use crate::QueryError;
use crate::validate::{Kind, Limits, Plan};

/// ClickHouse: the read ran out of time.
const TIMEOUT_EXCEEDED: u32 = 159;
/// ClickHouse: the read would return more rows than allowed.
const TOO_MANY_ROWS: u32 = 158;
const TOO_MANY_ROWS_OR_BYTES: u32 = 396;
/// ClickHouse: a bad argument; with "time zone" in the text, a zone it does
/// not know.
const BAD_ARGUMENTS: u32 = 36;

/// A value and the same value in the period just before.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trend {
    pub value: f64,
    pub previous: f64,
}

/// Session figures of an interval. A bounce is a session with one event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStats {
    pub sessions: Trend,
    pub avg_screens: Trend,
    pub avg_seconds: Trend,
    pub bounces: Trend,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesPoint {
    /// The start of the bucket in the site's time zone.
    pub bucket: String,
    /// The breakdown value; absent without a breakdown.
    pub key: Option<String>,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ranked {
    pub key: String,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScreenUsage {
    pub screen: String,
    pub views: u64,
    pub viewers: u64,
    pub taps: u64,
    pub tappers: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ElementUsage {
    pub screen: String,
    pub element: String,
    pub taps: u64,
    pub tappers: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRow {
    /// The breakdown value; absent without a breakdown.
    pub key: Option<String>,
    pub sessions: u64,
    pub avg_screens: f64,
    pub avg_seconds: f64,
    pub bounces: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Overview {
        users: Trend,
        events: Trend,
        /// Absent when a filter has no meaning for sessions (a session has
        /// no operating system or screen property, for example).
        sessions: Option<SessionStats>,
    },
    Series {
        points: Vec<SeriesPoint>,
    },
    Top {
        rows: Vec<Ranked>,
    },
    Screens {
        rows: Vec<ScreenUsage>,
    },
    Elements {
        rows: Vec<ElementUsage>,
    },
    Sessions {
        rows: Vec<SessionRow>,
    },
}

fn read_error(error: &Error) -> QueryError {
    match error.server_code() {
        Some(TIMEOUT_EXCEEDED) => QueryError::TooSlow,
        Some(TOO_MANY_ROWS | TOO_MANY_ROWS_OR_BYTES) => QueryError::TooManyRows,
        Some(BAD_ARGUMENTS) if error.to_string().contains("time zone") => {
            QueryError::InvalidTimeZone
        }
        _ => QueryError::Unavailable(error.to_string()),
    }
}

fn text(row: &Value, name: &str) -> String {
    row.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A number column; an average over no rows is `null` and counts as zero.
fn number(row: &Value, name: &str) -> f64 {
    row.get(name).and_then(Value::as_f64).unwrap_or(0.0)
}

fn count(row: &Value, name: &str) -> u64 {
    row.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn trend(row: &Value, name: &str) -> Trend {
    Trend {
        value: number(row, name),
        previous: number(row, &format!("previous_{name}")),
    }
}

/// Runs the query. `Overview`, `Series` with a breakdown and nothing else
/// make more than one read.
pub fn run(store: &Store, plan: &Plan, limits: &Limits) -> Result<Answer, QueryError> {
    let read = limits.read();
    let select =
        |statement: store::Statement| store.select(&statement, &read).map_err(|e| read_error(&e));
    let scope = &plan.scope;
    match &plan.kind {
        Kind::Overview {
            filters,
            session_filters,
        } => {
            let rows = select(store::overview_events(scope, filters))?;
            let row = rows.first().cloned().unwrap_or(Value::Null);
            let sessions = match session_filters {
                None => None,
                Some(session_filters) => {
                    let borrowed: Vec<(store::SessionDim, &[String])> = session_filters
                        .iter()
                        .map(|(dim, values)| (*dim, values.as_slice()))
                        .collect();
                    let rows = select(store::overview_sessions(scope, &borrowed))?;
                    let row = rows.first().cloned().unwrap_or(Value::Null);
                    Some(SessionStats {
                        sessions: trend(&row, "sessions"),
                        avg_screens: trend(&row, "avg_screens"),
                        avg_seconds: trend(&row, "avg_seconds"),
                        bounces: trend(&row, "bounces"),
                    })
                }
            };
            Ok(Answer::Overview {
                users: trend(&row, "users"),
                events: trend(&row, "events"),
                sessions,
            })
        }
        Kind::Series {
            event,
            filters,
            measure,
            granularity,
            breakdown,
        } => {
            if let Some(dim) = breakdown {
                let rows = select(store::distinct(
                    scope,
                    event.as_deref(),
                    filters,
                    dim,
                    limits.max_breakdown,
                ))?;
                let distinct = rows.first().map_or(0, |row| count(row, "n"));
                if distinct > u64::from(limits.max_breakdown) {
                    return Err(QueryError::BreakdownTooHigh {
                        max: limits.max_breakdown,
                    });
                }
            }
            let rows = select(store::series(
                scope,
                event.as_deref(),
                filters,
                *measure,
                *granularity,
                breakdown.as_ref(),
            ))?;
            let points = rows
                .iter()
                .map(|row| SeriesPoint {
                    bucket: text(row, "bucket"),
                    key: breakdown.as_ref().map(|_| text(row, "key")),
                    value: number(row, "value"),
                })
                .collect();
            Ok(Answer::Series { points })
        }
        Kind::Top {
            event,
            filters,
            measure,
            dim,
            limit,
        } => {
            let rows = select(store::top(
                scope,
                event.as_deref(),
                filters,
                *measure,
                dim,
                *limit,
            ))?;
            Ok(Answer::Top {
                rows: rows
                    .iter()
                    .map(|row| Ranked {
                        key: text(row, "key"),
                        value: number(row, "value"),
                    })
                    .collect(),
            })
        }
        Kind::Screens { filters, limit } => {
            let rows = select(store::screen_usage(scope, filters, *limit))?;
            Ok(Answer::Screens {
                rows: rows
                    .iter()
                    .map(|row| ScreenUsage {
                        screen: text(row, "screen"),
                        views: count(row, "views"),
                        viewers: count(row, "viewers"),
                        taps: count(row, "taps"),
                        tappers: count(row, "tappers"),
                    })
                    .collect(),
            })
        }
        Kind::Elements {
            screen,
            filters,
            limit,
        } => {
            let rows = select(store::element_usage(
                scope,
                screen.as_deref(),
                filters,
                *limit,
            ))?;
            Ok(Answer::Elements {
                rows: rows
                    .iter()
                    .map(|row| ElementUsage {
                        screen: text(row, "screen"),
                        element: text(row, "element"),
                        taps: count(row, "taps"),
                        tappers: count(row, "tappers"),
                    })
                    .collect(),
            })
        }
        Kind::Sessions {
            filters,
            dim,
            limit,
        } => {
            let borrowed: Vec<(store::SessionDim, &[String])> = filters
                .iter()
                .map(|(dim, values)| (*dim, values.as_slice()))
                .collect();
            // Without a breakdown the whole interval is one group.
            let rows = match dim {
                Some(dim) => select(store::sessions_by(scope, &borrowed, *dim, *limit))?,
                None => select(store::sessions_total(scope, &borrowed))?,
            };
            Ok(Answer::Sessions {
                rows: rows
                    .iter()
                    .map(|row| SessionRow {
                        key: dim.map(|_| text(row, "key")),
                        sessions: count(row, "sessions"),
                        avg_screens: number(row, "avg_screens"),
                        avg_seconds: number(row, "avg_seconds"),
                        bounces: count(row, "bounces"),
                    })
                    .collect(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(message: &str) -> Error {
        Error::Server {
            status: 500,
            message: message.to_owned(),
        }
    }

    #[test]
    fn server_codes_become_typed_errors() {
        assert_eq!(
            read_error(&server("Code: 159. DB::Exception: Timeout exceeded")),
            QueryError::TooSlow
        );
        assert_eq!(
            read_error(&server("Code: 396. DB::Exception: Limit for result")),
            QueryError::TooManyRows
        );
        assert_eq!(
            read_error(&server("Code: 36. DB::Exception: Cannot load time zone X")),
            QueryError::InvalidTimeZone
        );
        assert!(matches!(
            read_error(&server("Code: 36. DB::Exception: something else")),
            QueryError::Unavailable(_)
        ));
        assert!(matches!(
            read_error(&Error::Io("down".to_owned())),
            QueryError::Unavailable(_)
        ));
    }
}
