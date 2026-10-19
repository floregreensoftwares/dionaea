//! The SQL of the reports.
//!
//! Callers describe a report with the plain types of this module and get a
//! [`Statement`] back: the text of a `SELECT` and its parameters. Nothing
//! that came from a caller is ever written into the text; names, values,
//! times and the time zone travel as ClickHouse query parameters
//! (`{p3:String}`), and the only fragments written into a statement are
//! constants of this crate. [`Store::select`] runs a statement with the
//! limits of a read: a time limit, a row limit that fails rather than
//! truncates, and no quoted 64-bit integers.
//!
//! Every report counts users and sessions exactly (`uniqExact`), never by an
//! estimate.

use std::fmt::Write as _;

use serde_json::Value;

use crate::{Error, Store, http};

/// Most values one filter may list.
pub const MAX_FILTER_VALUES: usize = 100;
/// The most one read may return, in bytes of JSON. With the row limit it
/// bounds memory; an answer past it fails with [`Error::AnswerTooLarge`].
pub const MAX_ANSWER_BYTES: usize = 16 << 20;
/// Longest time zone name, in bytes.
pub const MAX_TIME_ZONE_LEN: usize = 64;

/// A column events can be grouped or filtered by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dim {
    Country,
    Platform,
    Os,
    Browser,
    DeviceClass,
    AppVersion,
    /// The screen the SDK reports with every event.
    Screen,
    EventName,
    /// A declared text-like property of one event. Using it restricts the
    /// report to that event.
    Prop {
        event: String,
        name: String,
    },
}

/// A dimension of the session summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionDim {
    Country,
    Platform,
    AppVersion,
    EntryScreen,
    ExitScreen,
}

/// Events (or sessions) are kept when the dimension has one of the values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub dim: Dim,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Granularity {
    Hour,
    Day,
    /// Weeks start on Monday.
    Week,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Measure {
    /// Events stored.
    Events,
    /// Distinct devices.
    Users,
}

/// The part of the data a report reads: one site, a half-open interval of
/// instants, and the zone days are cut in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub site_id: u32,
    /// Milliseconds since the Unix epoch, inclusive.
    pub from_ms: i64,
    /// Milliseconds since the Unix epoch, exclusive.
    pub to_ms: i64,
    pub time_zone: String,
}

/// What a read may cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_execution_secs: u32,
    pub max_rows: u64,
}

/// A statement and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    /// Parameter name (without the `param_` prefix) and value.
    pub params: Vec<(String, String)>,
}

#[derive(Default)]
pub(crate) struct Builder {
    params: Vec<(String, String)>,
}

impl Builder {
    pub(crate) fn param(&mut self, kind: &str, value: impl ToString) -> String {
        let name = format!("p{}", self.params.len());
        self.params.push((name.clone(), value.to_string()));
        format!("{{{name}:{kind}}}")
    }

    pub(crate) fn finish(self, sql: String) -> Statement {
        Statement {
            sql,
            params: self.params,
        }
    }

    pub(crate) fn time(&mut self, ms: i64) -> String {
        let p = self.param("Int64", ms);
        format!("fromUnixTimestamp64Milli({p}, 'UTC')")
    }

    /// `site_id = .. AND ts in the scope` for the events table.
    fn events_scope(&mut self, scope: &Scope) -> String {
        let site = self.param("UInt32", scope.site_id);
        let from = self.time(scope.from_ms);
        let to = self.time(scope.to_ms);
        format!("site_id = {site} AND ts >= {from} AND ts < {to}")
    }

    /// The expression of a dimension, and the condition that makes it
    /// meaningful (a property only exists on its own event).
    fn dim(&mut self, dim: &Dim) -> (String, Option<String>) {
        let column = |name: &str| (name.to_owned(), None);
        match dim {
            Dim::Country => column("country"),
            Dim::Platform => column("platform"),
            Dim::Os => column("os"),
            Dim::Browser => column("browser"),
            Dim::DeviceClass => column("device_class"),
            Dim::AppVersion => column("app_version"),
            Dim::Screen => column("screen"),
            Dim::EventName => column("name"),
            Dim::Prop { event, name } => {
                let event = self.param("String", event);
                let name = self.param("String", name);
                (
                    format!("props_str[{name}]"),
                    Some(format!("name = {event}")),
                )
            }
        }
    }

    /// The `AND ...` conditions for filters, as one string (possibly empty).
    fn events_filters(&mut self, filters: &[Filter]) -> String {
        let mut out = String::new();
        for filter in filters {
            let (expr, condition) = self.dim(&filter.dim);
            let values: Vec<String> = filter
                .values
                .iter()
                .map(|value| self.param("String", value))
                .collect();
            if let Some(condition) = condition {
                let _ = write!(out, " AND {condition}");
            }
            let _ = write!(out, " AND {expr} IN ({})", values.join(", "));
        }
        out
    }

    fn session_expr(dim: SessionDim) -> &'static str {
        match dim {
            SessionDim::Country => "s_country",
            SessionDim::Platform => "s_platform",
            SessionDim::AppVersion => "s_app_version",
            SessionDim::EntryScreen => "s_entry",
            SessionDim::ExitScreen => "s_exit",
        }
    }
}

fn bucket_expr(granularity: Granularity, zone: &str) -> String {
    match granularity {
        Granularity::Hour => format!("toString(toStartOfHour(toTimeZone(ts, {zone})))"),
        Granularity::Day => format!("toString(toDate(toTimeZone(ts, {zone})))"),
        Granularity::Week => format!("toString(toStartOfWeek(toTimeZone(ts, {zone}), 1))"),
        Granularity::Month => format!("toString(toStartOfMonth(toTimeZone(ts, {zone})))"),
    }
}

fn measure_expr(measure: Measure) -> &'static str {
    match measure {
        Measure::Events => "count()",
        Measure::Users => "uniqExact(anon_id)",
    }
}

fn event_condition(b: &mut Builder, event: Option<&str>) -> String {
    match event {
        Some(event) => format!(" AND name = {}", b.param("String", event)),
        None => String::new(),
    }
}

/// Events (or devices) per time bucket, optionally split by a dimension.
/// Columns: `bucket`, `key`, `value`. A bucket with nothing in it has no row.
pub fn series(
    scope: &Scope,
    event: Option<&str>,
    filters: &[Filter],
    measure: Measure,
    granularity: Granularity,
    breakdown: Option<&Dim>,
) -> Statement {
    let mut b = Builder::default();
    let zone = b.param("String", &scope.time_zone);
    let bucket = bucket_expr(granularity, &zone);
    let (key, key_condition) = match breakdown {
        Some(dim) => b.dim(dim),
        None => ("''".to_owned(), None),
    };
    let scope_sql = b.events_scope(scope);
    let event_sql = event_condition(&mut b, event);
    let filter_sql = b.events_filters(filters);
    let key_sql = key_condition.map_or_else(String::new, |c| format!(" AND {c}"));
    let sql = format!(
        "SELECT {bucket} AS bucket, {key} AS key, {value} AS value FROM events \
         WHERE {scope_sql}{event_sql}{key_sql}{filter_sql} \
         GROUP BY bucket, key ORDER BY bucket, key",
        value = measure_expr(measure),
    );
    b.finish(sql)
}

/// The most frequent values of a dimension. Columns: `key`, `value`.
pub fn top(
    scope: &Scope,
    event: Option<&str>,
    filters: &[Filter],
    measure: Measure,
    dim: &Dim,
    limit: u32,
) -> Statement {
    let mut b = Builder::default();
    let (key, key_condition) = b.dim(dim);
    let scope_sql = b.events_scope(scope);
    let event_sql = event_condition(&mut b, event);
    let filter_sql = b.events_filters(filters);
    let key_sql = key_condition.map_or_else(String::new, |c| format!(" AND {c}"));
    let limit = b.param("UInt32", limit);
    let sql = format!(
        "SELECT {key} AS key, {value} AS value FROM events \
         WHERE {scope_sql}{event_sql}{key_sql}{filter_sql} \
         GROUP BY key ORDER BY value DESC, key ASC LIMIT {limit}",
        value = measure_expr(measure),
    );
    b.finish(sql)
}

/// How many distinct values a dimension has in the scope, counted up to
/// `cap + 1` so that the cost is bounded. Column: `n`.
pub fn distinct(
    scope: &Scope,
    event: Option<&str>,
    filters: &[Filter],
    dim: &Dim,
    cap: u32,
) -> Statement {
    let mut b = Builder::default();
    let (key, key_condition) = b.dim(dim);
    let scope_sql = b.events_scope(scope);
    let event_sql = event_condition(&mut b, event);
    let filter_sql = b.events_filters(filters);
    let key_sql = key_condition.map_or_else(String::new, |c| format!(" AND {c}"));
    let limit = b.param("UInt64", u64::from(cap) + 1);
    let sql = format!(
        "SELECT count() AS n FROM (SELECT DISTINCT {key} AS key FROM events \
         WHERE {scope_sql}{event_sql}{key_sql}{filter_sql} LIMIT {limit})"
    );
    b.finish(sql)
}

/// Users and events in the scope and in the period of the same length just
/// before it. Columns: `users`, `events`, `previous_users`, `previous_events`.
pub fn overview_events(scope: &Scope, filters: &[Filter]) -> Statement {
    let mut b = Builder::default();
    let site = b.param("UInt32", scope.site_id);
    let length = scope.to_ms.saturating_sub(scope.from_ms);
    let previous_from = b.time(scope.from_ms.saturating_sub(length));
    let from = b.time(scope.from_ms);
    let to = b.time(scope.to_ms);
    let filter_sql = b.events_filters(filters);
    let current = format!("ts >= {from} AND ts < {to}");
    let previous = format!("ts >= {previous_from} AND ts < {from}");
    let sql = format!(
        "SELECT uniqExactIf(anon_id, {current}) AS users, countIf({current}) AS events, \
         uniqExactIf(anon_id, {previous}) AS previous_users, countIf({previous}) AS previous_events \
         FROM events WHERE site_id = {site} AND ts >= {previous_from} AND ts < {to}{filter_sql}"
    );
    b.finish(sql)
}

/// The session summaries as one row per session, started in `[from, to)`.
/// Sessions are read with `GROUP BY` because one session may be stored in
/// several parts.
fn sessions_cte(
    b: &mut Builder,
    site_id: u32,
    from_ms: i64,
    to_ms: i64,
    filters: &[(SessionDim, &[String])],
) -> String {
    let site = b.param("UInt32", site_id);
    let from = b.time(from_ms);
    let to = b.time(to_ms);
    // A session whose first part was stored more than a day before the
    // interval starts is not found whole; sessions that long do not occur
    // with the SDK's inactivity rule.
    let started_from = b.param("Int64", from_ms.saturating_sub(86_400_000));
    let started_to = b.param("Int64", to_ms);
    let mut having = format!("first_ts >= {from} AND first_ts < {to}");
    for (dim, values) in filters {
        let list: Vec<String> = values.iter().map(|v| b.param("String", v)).collect();
        let _ = write!(
            having,
            " AND {} IN ({})",
            Builder::session_expr(*dim),
            list.join(", ")
        );
    }
    format!(
        "SELECT session_id, anon_id, min(first_ts) AS first_ts, max(last_ts) AS last_ts, \
         sum(events) AS ev, uniqMerge(screens) AS screens, \
         argMinMerge(entry_screen) AS s_entry, argMaxMerge(exit_screen) AS s_exit, \
         argMinMerge(platform) AS s_platform, argMinMerge(app_version) AS s_app_version, \
         argMinMerge(country) AS s_country \
         FROM sessions WHERE site_id = {site} \
         AND started >= toDate(fromUnixTimestamp64Milli({started_from}, 'UTC')) \
         AND started <= toDate(fromUnixTimestamp64Milli({started_to}, 'UTC')) \
         GROUP BY session_id, anon_id HAVING {having}"
    )
}

const SESSION_SECONDS: &str =
    "(toUnixTimestamp64Milli(last_ts) - toUnixTimestamp64Milli(first_ts)) / 1000";

/// Sessions in the scope and in the period just before it. Columns:
/// `sessions`, `avg_screens`, `avg_seconds`, `bounces` and the same four
/// prefixed `previous_`. A bounce is a session with one event.
pub fn overview_sessions(scope: &Scope, filters: &[(SessionDim, &[String])]) -> Statement {
    let mut b = Builder::default();
    let length = scope.to_ms.saturating_sub(scope.from_ms);
    let previous_from = scope.from_ms.saturating_sub(length);
    // The same shape twice, over the two intervals: the sessions table is
    // read once per interval so each keeps its own start window.
    let current = sessions_cte(&mut b, scope.site_id, scope.from_ms, scope.to_ms, filters);
    let previous = sessions_cte(&mut b, scope.site_id, previous_from, scope.from_ms, filters);
    let columns = |prefix: &str| {
        format!(
            "count() AS {prefix}sessions, avg(screens) AS {prefix}avg_screens, \
             avg({SESSION_SECONDS}) AS {prefix}avg_seconds, countIf(ev = 1) AS {prefix}bounces"
        )
    };
    let sql = format!(
        "SELECT * FROM (SELECT {} FROM ({current})) AS current \
         CROSS JOIN (SELECT {} FROM ({previous})) AS previous",
        columns(""),
        columns("previous_"),
    );
    b.finish(sql)
}

/// Session aggregates split by a dimension. Columns: `key`, `sessions`,
/// `avg_screens`, `avg_seconds`, `bounces`.
pub fn sessions_by(
    scope: &Scope,
    filters: &[(SessionDim, &[String])],
    dim: SessionDim,
    limit: u32,
) -> Statement {
    let mut b = Builder::default();
    let cte = sessions_cte(&mut b, scope.site_id, scope.from_ms, scope.to_ms, filters);
    let limit = b.param("UInt32", limit);
    let sql = format!(
        "SELECT {key} AS key, count() AS sessions, avg(screens) AS avg_screens, \
         avg({SESSION_SECONDS}) AS avg_seconds, countIf(ev = 1) AS bounces \
         FROM ({cte}) GROUP BY key ORDER BY sessions DESC, key ASC LIMIT {limit}",
        key = Builder::session_expr(dim),
    );
    b.finish(sql)
}

/// Session aggregates of the whole scope. Columns as [`sessions_by`], with
/// an empty `key`.
pub fn sessions_total(scope: &Scope, filters: &[(SessionDim, &[String])]) -> Statement {
    let mut b = Builder::default();
    let cte = sessions_cte(&mut b, scope.site_id, scope.from_ms, scope.to_ms, filters);
    let sql = format!(
        "SELECT '' AS key, count() AS sessions, avg(screens) AS avg_screens,          avg({SESSION_SECONDS}) AS avg_seconds, countIf(ev = 1) AS bounces FROM ({cte})"
    );
    b.finish(sql)
}

/// Views and taps per screen. Columns: `screen`, `views`, `viewers`, `taps`,
/// `tappers`. A view is a `screen` event, keyed by its `name`; a tap is a
/// `ui_tap` event, keyed by its `screen`.
pub fn screen_usage(scope: &Scope, filters: &[Filter], limit: u32) -> Statement {
    let mut b = Builder::default();
    let scope_sql = b.events_scope(scope);
    let filter_sql = b.events_filters(filters);
    let limit = b.param("UInt32", limit);
    let sql = format!(
        "SELECT if(name = 'screen', props_str['name'], props_str['screen']) AS screen, \
         countIf(name = 'screen') AS views, uniqExactIf(anon_id, name = 'screen') AS viewers, \
         countIf(name = 'ui_tap') AS taps, uniqExactIf(anon_id, name = 'ui_tap') AS tappers \
         FROM events WHERE {scope_sql} AND name IN ('screen', 'ui_tap'){filter_sql} \
         GROUP BY screen HAVING screen != '' \
         ORDER BY views + taps DESC, screen ASC LIMIT {limit}"
    );
    b.finish(sql)
}

/// Taps per element, on one screen or on all. Columns: `screen`, `element`,
/// `taps`, `tappers`.
pub fn element_usage(
    scope: &Scope,
    screen: Option<&str>,
    filters: &[Filter],
    limit: u32,
) -> Statement {
    let mut b = Builder::default();
    let scope_sql = b.events_scope(scope);
    let filter_sql = b.events_filters(filters);
    let screen_sql = match screen {
        Some(screen) => format!(" AND props_str['screen'] = {}", b.param("String", screen)),
        None => String::new(),
    };
    let limit = b.param("UInt32", limit);
    let sql = format!(
        "SELECT props_str['screen'] AS screen, props_str['element'] AS element, \
         count() AS taps, uniqExact(anon_id) AS tappers \
         FROM events WHERE {scope_sql} AND name = 'ui_tap'{screen_sql}{filter_sql} \
         GROUP BY screen, element ORDER BY taps DESC, screen ASC, element ASC LIMIT {limit}"
    );
    b.finish(sql)
}

/// Whether ClickHouse knows the time zone. Column: `n`.
pub fn time_zone_known(time_zone: &str) -> Statement {
    let mut b = Builder::default();
    let zone = b.param("String", time_zone);
    let sql = format!("SELECT count() AS n FROM system.time_zones WHERE time_zone = {zone}");
    b.finish(sql)
}

/// Escapes a parameter value the way ClickHouse reads it back: as a field of
/// the tab-separated format.
fn escape_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out
}

impl Store {
    /// Whether ClickHouse knows the time zone.
    pub fn time_zone_known(&self, time_zone: &str) -> Result<bool, Error> {
        let limits = Limits {
            max_execution_secs: 10,
            max_rows: 10,
        };
        let rows = self.select(&time_zone_known(time_zone), &limits)?;
        Ok(rows
            .first()
            .and_then(|row| row.get("n"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0))
    }

    /// Runs a read statement and returns its rows as JSON objects. A read
    /// that takes longer than `limits.max_execution_secs`, or would return
    /// more than `limits.max_rows`, fails (see [`Error::server_code`])
    /// rather than returning part of the answer.
    pub fn select(&self, statement: &Statement, limits: &Limits) -> Result<Vec<Value>, Error> {
        let mut path = format!(
            "/?database={}&default_format=JSONEachRow\
             &output_format_json_quote_64bit_integers=0\
             &max_execution_time={}&max_result_rows={}&result_overflow_mode=throw",
            http::url_encode(&self.config.database),
            limits.max_execution_secs,
            limits.max_rows,
        );
        for (name, value) in &statement.params {
            let _ = write!(
                path,
                "&param_{name}={}",
                http::url_encode(&escape_param(value))
            );
        }
        let response = self
            .client
            .post_limited(&path, statement.sql.as_bytes(), MAX_ANSWER_BYTES)
            .map_err(|e| {
                if e.to_string() == http::TOO_LARGE {
                    Error::AnswerTooLarge
                } else {
                    Error::from(e)
                }
            })?;
        if response.status != 200 {
            return Err(Error::Server {
                status: response.status,
                message: crate::summarize(&response.body, false),
            });
        }
        let text = String::from_utf8_lossy(&response.body).into_owned();
        text.lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|e| Error::Io(format!("unreadable answer from ClickHouse: {e}")))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            site_id: 7,
            from_ms: 1_000,
            to_ms: 2_000,
            time_zone: "America/Sao_Paulo".to_owned(),
        }
    }

    /// No statement carries a caller's text: the hostile value only ever
    /// appears among the parameters.
    #[test]
    fn caller_text_travels_only_as_parameters() {
        let hostile = "x' OR 1=1; DROP TABLE events --";
        let filters = [Filter {
            dim: Dim::Prop {
                event: hostile.to_owned(),
                name: hostile.to_owned(),
            },
            values: vec![hostile.to_owned()],
        }];
        let mut scope = scope();
        scope.time_zone = hostile.to_owned();
        let statements = [
            series(
                &scope,
                Some(hostile),
                &filters,
                Measure::Events,
                Granularity::Day,
                Some(&Dim::Prop {
                    event: hostile.to_owned(),
                    name: hostile.to_owned(),
                }),
            ),
            top(
                &scope,
                Some(hostile),
                &filters,
                Measure::Users,
                &Dim::Screen,
                5,
            ),
            distinct(&scope, None, &filters, &Dim::Country, 5),
            overview_events(&scope, &filters),
            screen_usage(&scope, &filters, 5),
            element_usage(&scope, Some(hostile), &filters, 5),
            time_zone_known(hostile),
        ];
        for statement in statements {
            assert!(!statement.sql.contains("DROP"), "{}", statement.sql);
            assert!(!statement.sql.contains("1=1"), "{}", statement.sql);
            assert!(statement.params.iter().any(|(_, v)| v == hostile));
        }
    }

    #[test]
    fn parameters_are_numbered_and_typed() {
        let statement = top(&scope(), None, &[], Measure::Events, &Dim::Platform, 3);
        assert!(statement.sql.contains("{p0:UInt32}"));
        assert_eq!(statement.params[0], ("p0".to_owned(), "7".to_owned()));
        assert_eq!(statement.params.len(), 4);
    }

    #[test]
    fn a_parameter_value_is_escaped_for_the_tab_separated_reader() {
        assert_eq!(escape_param("a\\b\tc\nd\re\0"), "a\\\\b\\tc\\nd\\re\\0");
        assert_eq!(escape_param("caf\u{e9} 'q'"), "caf\u{e9} 'q'");
    }

    #[test]
    fn a_property_dimension_limits_the_report_to_its_event() {
        let dim = Dim::Prop {
            event: "checkout".to_owned(),
            name: "coupon".to_owned(),
        };
        let statement = top(&scope(), None, &[], Measure::Events, &dim, 3);
        assert!(statement.sql.contains("AND name = {p"), "{}", statement.sql);
    }
}
