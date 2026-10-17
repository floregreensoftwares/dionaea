//! Every report against a real ClickHouse (the one in `compose.yaml`). Ignored
//! by default; `cargo test -p dionaea-query -- --ignored` runs them. The server
//! comes from `DIONAEA_TEST_CLICKHOUSE` as `host:port:user:password`.

use std::sync::atomic::{AtomicU32, Ordering};

use dionaea_core::{Schema, parse_schema};
use dionaea_query::{
    Answer, Dimension, Filter, Granularity, Limits, Measure, Query, QueryError, Report,
    SessionDimension, Site, run, validate,
};
use dionaea_store::{Config, EventRow, Store};

const DAY: i64 = 86_400_000;
/// 2025-10-13 00:00:00 UTC, a Monday.
const DAY0: i64 = 1_760_313_600_000;
const SITE: u32 = 7;

const SCHEMA: &str = r#"
[events.checkout.fields.coupon]
kind = "enum"
values = ["save10", "save20"]

[events.search.fields.term]
kind = "text"
max = 40
"#;

struct Scratch {
    store: Store,
    admin: Store,
    database: String,
    schemas: Vec<Schema>,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let setting = std::env::var("DIONAEA_TEST_CLICKHOUSE")
            .unwrap_or_else(|_| "127.0.0.1:8123:dionaea:dionaea-dev".to_owned());
        let parts: Vec<&str> = setting.splitn(4, ':').collect();
        let database = format!(
            "dionaea_query_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let make = |database: &str| {
            let mut config = Config::new(parts[0], parts[1].parse().expect("a port"), database);
            config.user = parts[2].to_owned();
            config.password = parts[3].to_owned();
            Store::new(config).expect("a valid config")
        };
        let scratch = Self {
            store: make(&database),
            admin: make("default"),
            database,
            schemas: vec![parse_schema(SCHEMA).expect("a valid schema")],
        };
        scratch.store.migrate().expect("migrate");
        scratch
    }

    /// Two blocks, so a session split over inserts has to be merged at read
    /// time, as sessions sent in separate requests are.
    fn insert(&self, rows: &[EventRow]) {
        let (first, second) = rows.split_at(rows.len() / 2);
        for (i, part) in [first, second].into_iter().enumerate() {
            self.store
                .insert_events(part, &format!("t{i}-{}", rows.len()))
                .expect("insert");
        }
    }

    fn ask(&self, query: &Query) -> Result<Answer, QueryError> {
        self.ask_with(query, &Limits::default(), "America/Sao_Paulo", SITE)
    }

    fn ask_with(
        &self,
        query: &Query,
        limits: &Limits,
        time_zone: &str,
        site: u32,
    ) -> Result<Answer, QueryError> {
        let site = Site {
            id: site,
            time_zone,
            schemas: &self.schemas,
        };
        let plan = validate(site, query, limits)?;
        run(&self.store, &plan, limits)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self
            .admin
            .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.database));
    }
}

#[allow(clippy::too_many_arguments)]
fn event(
    n: u32,
    offset_ms: i64,
    name: &str,
    anon: &str,
    session: &str,
    screen: &str,
    props: &[(&str, &str)],
    who: (&str, &str, &str),
) -> EventRow {
    EventRow {
        site_id: SITE,
        event_id: format!("0190f7e2-7c3e-7a52-9a0e-{n:012x}"),
        ts_ms: u64::try_from(DAY0 + offset_ms).expect("a time after the epoch"),
        received_ms: u64::try_from(DAY0 + offset_ms).expect("a time after the epoch"),
        name: name.to_owned(),
        schema_version: 1,
        anon_id: anon.to_owned(),
        session_id: session.to_owned(),
        screen: screen.to_owned(),
        props_str: props
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        props_num: Vec::new(),
        country: who.0.to_owned(),
        platform: who.1.to_owned(),
        os: String::new(),
        browser: String::new(),
        device_class: "phone".to_owned(),
        app_version: who.2.to_owned(),
        sdk: "test".to_owned(),
    }
}

const H: i64 = 3_600_000;
const MIN: i64 = 60_000;
const BR_IOS: (&str, &str, &str) = ("BR", "ios", "1.0");
const BR_ANDROID: (&str, &str, &str) = ("BR", "android", "1.1");
const US_IOS: (&str, &str, &str) = ("US", "ios", "1.0");

/// Nine events in the week from `DAY0`, one in the week before, one for
/// another site. Sao Paulo is three hours behind UTC.
fn seed(scratch: &Scratch) {
    let mut rows = vec![
        // a1: a session of 3 minutes on day 0.
        event(
            1,
            12 * H,
            "screen",
            "a1",
            "s1",
            "home",
            &[("name", "home")],
            BR_IOS,
        ),
        event(
            2,
            12 * H + MIN,
            "ui_tap",
            "a1",
            "s1",
            "home",
            &[("screen", "home"), ("element", "buy")],
            BR_IOS,
        ),
        event(
            3,
            12 * H + 2 * MIN,
            "screen",
            "a1",
            "s1",
            "cart",
            &[("name", "cart")],
            BR_IOS,
        ),
        event(
            4,
            12 * H + 3 * MIN,
            "checkout",
            "a1",
            "s1",
            "cart",
            &[("coupon", "save10")],
            BR_IOS,
        ),
        // a2: a bounce.
        event(
            5,
            13 * H,
            "screen",
            "a2",
            "s2",
            "home",
            &[("name", "home")],
            BR_ANDROID,
        ),
        // a3: day 1.
        event(
            6,
            DAY + 12 * H,
            "screen",
            "a3",
            "s3",
            "home",
            &[("name", "home")],
            US_IOS,
        ),
        event(
            7,
            DAY + 12 * H + MIN,
            "checkout",
            "a3",
            "s3",
            "home",
            &[("coupon", "save20")],
            US_IOS,
        ),
        event(
            8,
            DAY + 12 * H + 2 * MIN,
            "ui_tap",
            "a3",
            "s3",
            "home",
            &[("screen", "home"), ("element", "buy")],
            US_IOS,
        ),
        // a4: 02:00 UTC is the evening before in Sao Paulo; no session id.
        event(
            9,
            2 * H,
            "search",
            "a4",
            "",
            "",
            &[("term", "shoes")],
            BR_IOS,
        ),
        // The week before, one session.
        event(
            10,
            -7 * DAY + 12 * H,
            "screen",
            "a1",
            "sp1",
            "home",
            &[("name", "home")],
            BR_IOS,
        ),
        event(
            11,
            -7 * DAY + 12 * H + MIN,
            "screen",
            "a1",
            "sp1",
            "cart",
            &[("name", "cart")],
            BR_IOS,
        ),
    ];
    let mut other = event(
        12,
        12 * H,
        "screen",
        "z1",
        "zs",
        "home",
        &[("name", "home")],
        BR_IOS,
    );
    other.site_id = 8;
    rows.push(other);
    scratch.insert(&rows);
}

fn week(report: Report) -> Query {
    Query {
        from_ms: DAY0,
        to_ms: DAY0 + 7 * DAY,
        filters: Vec::new(),
        report,
    }
}

fn series(
    measure: Measure,
    event: Option<&str>,
    granularity: Granularity,
    breakdown: Option<Dimension>,
) -> Report {
    Report::Series {
        measure,
        event: event.map(str::to_owned),
        granularity,
        breakdown,
    }
}

fn points(answer: Result<Answer, QueryError>) -> Vec<(String, Option<String>, f64)> {
    match answer.expect("answers") {
        Answer::Series { points } => points
            .into_iter()
            .map(|p| (p.bucket, p.key, p.value))
            .collect(),
        other => panic!("not a series: {other:?}"),
    }
}

fn ranked(answer: Result<Answer, QueryError>) -> Vec<(String, f64)> {
    match answer.expect("answers") {
        Answer::Top { rows } => rows.into_iter().map(|r| (r.key, r.value)).collect(),
        other => panic!("not a top list: {other:?}"),
    }
}

fn top(dimension: Dimension, measure: Measure) -> Report {
    Report::Top {
        dimension,
        measure,
        event: None,
        limit: None,
    }
}

fn property(event: &str, name: &str) -> Dimension {
    Dimension::Property {
        event: event.to_owned(),
        name: name.to_owned(),
    }
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_overview_counts_users_events_and_sessions_with_the_previous_period() {
    let scratch = Scratch::new();
    seed(&scratch);
    let Answer::Overview {
        users,
        events,
        sessions,
    } = scratch.ask(&week(Report::Overview)).expect("answers")
    else {
        panic!("not an overview");
    };
    assert_eq!((users.value, users.previous), (4.0, 1.0));
    assert_eq!((events.value, events.previous), (9.0, 2.0));
    let sessions = sessions.expect("sessions");
    assert_eq!(
        (sessions.sessions.value, sessions.sessions.previous),
        (3.0, 1.0)
    );
    assert_eq!(sessions.bounces.value, 1.0);
    assert_eq!(sessions.bounces.previous, 0.0);
    // s1 saw two screens, s2 and s3 one each.
    assert!((sessions.avg_screens.value - 4.0 / 3.0).abs() < 1e-9);
    assert!((sessions.avg_seconds.value - 100.0).abs() < 1e-9);
    assert!((sessions.avg_seconds.previous - 60.0).abs() < 1e-9);
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_filter_narrows_the_overview_and_one_that_sessions_lack_drops_their_figures() {
    let scratch = Scratch::new();
    seed(&scratch);
    let mut query = week(Report::Overview);
    query.filters = vec![Filter {
        dimension: Dimension::Country,
        values: vec!["US".to_owned()],
    }];
    let Answer::Overview {
        users, sessions, ..
    } = scratch.ask(&query).expect("answers")
    else {
        panic!("not an overview");
    };
    assert_eq!(users.value, 1.0);
    assert_eq!(sessions.expect("sessions").sessions.value, 1.0);

    query.filters = vec![Filter {
        dimension: Dimension::Screen,
        values: vec!["home".to_owned()],
    }];
    let Answer::Overview {
        users, sessions, ..
    } = scratch.ask(&query).expect("answers")
    else {
        panic!("not an overview");
    };
    assert_eq!(users.value, 3.0);
    assert!(sessions.is_none());
}

#[test]
#[ignore = "needs ClickHouse"]
fn days_are_cut_in_the_site_time_zone() {
    let scratch = Scratch::new();
    seed(&scratch);
    let query = week(series(Measure::Events, None, Granularity::Day, None));
    let sao_paulo = points(scratch.ask(&query));
    assert_eq!(
        sao_paulo,
        vec![
            ("2025-10-12".to_owned(), None, 1.0),
            ("2025-10-13".to_owned(), None, 5.0),
            ("2025-10-14".to_owned(), None, 3.0),
        ]
    );
    let utc = points(scratch.ask_with(&query, &Limits::default(), "UTC", SITE));
    assert_eq!(
        utc,
        vec![
            ("2025-10-13".to_owned(), None, 6.0),
            ("2025-10-14".to_owned(), None, 3.0),
        ]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn weeks_months_and_hours_bucket_in_the_site_zone() {
    let scratch = Scratch::new();
    seed(&scratch);
    let weekly = points(scratch.ask(&week(series(
        Measure::Events,
        None,
        Granularity::Week,
        None,
    ))));
    assert_eq!(
        weekly,
        vec![
            ("2025-10-06".to_owned(), None, 1.0),
            ("2025-10-13".to_owned(), None, 8.0),
        ]
    );
    let monthly = points(scratch.ask(&week(series(
        Measure::Events,
        None,
        Granularity::Month,
        None,
    ))));
    assert_eq!(monthly, vec![("2025-10-01".to_owned(), None, 9.0)]);
    let hourly = points(scratch.ask(&week(series(
        Measure::Events,
        Some("search"),
        Granularity::Hour,
        None,
    ))));
    assert_eq!(hourly, vec![("2025-10-12 23:00:00".to_owned(), None, 1.0)]);
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_series_splits_by_a_built_in_dimension_and_by_a_declared_property() {
    let scratch = Scratch::new();
    seed(&scratch);
    let by_country = points(scratch.ask(&week(series(
        Measure::Users,
        None,
        Granularity::Month,
        Some(Dimension::Country),
    ))));
    assert_eq!(
        by_country,
        vec![
            ("2025-10-01".to_owned(), Some("BR".to_owned()), 3.0),
            ("2025-10-01".to_owned(), Some("US".to_owned()), 1.0),
        ]
    );
    let by_coupon = points(scratch.ask(&week(series(
        Measure::Events,
        Some("checkout"),
        Granularity::Month,
        Some(property("checkout", "coupon")),
    ))));
    assert_eq!(
        by_coupon,
        vec![
            ("2025-10-01".to_owned(), Some("save10".to_owned()), 1.0),
            ("2025-10-01".to_owned(), Some("save20".to_owned()), 1.0),
        ]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_breakdown_over_the_cardinality_limit_is_refused() {
    let scratch = Scratch::new();
    seed(&scratch);
    let query = week(series(
        Measure::Events,
        None,
        Granularity::Day,
        Some(Dimension::Country),
    ));
    let tight = Limits {
        max_breakdown: 1,
        ..Limits::default()
    };
    assert_eq!(
        scratch.ask_with(&query, &tight, "UTC", SITE),
        Err(QueryError::BreakdownTooHigh { max: 1 })
    );
    let exact = Limits {
        max_breakdown: 2,
        ..Limits::default()
    };
    assert!(scratch.ask_with(&query, &exact, "UTC", SITE).is_ok());
}

#[test]
#[ignore = "needs ClickHouse"]
fn an_answer_over_the_row_limit_fails_instead_of_truncating() {
    let scratch = Scratch::new();
    seed(&scratch);
    let query = week(series(Measure::Events, None, Granularity::Day, None));
    let tight = Limits {
        max_rows: 1,
        ..Limits::default()
    };
    assert_eq!(
        scratch.ask_with(&query, &tight, "UTC", SITE),
        Err(QueryError::TooManyRows)
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn top_lists_rank_screens_events_countries_and_versions() {
    let scratch = Scratch::new();
    seed(&scratch);
    assert_eq!(
        ranked(scratch.ask(&week(top(Dimension::Country, Measure::Users)))),
        vec![("BR".to_owned(), 3.0), ("US".to_owned(), 1.0)]
    );
    assert_eq!(
        ranked(scratch.ask(&week(top(Dimension::AppVersion, Measure::Events)))),
        vec![("1.0".to_owned(), 8.0), ("1.1".to_owned(), 1.0)]
    );
    assert_eq!(
        ranked(scratch.ask(&week(top(Dimension::Event, Measure::Events)))),
        vec![
            ("screen".to_owned(), 4.0),
            ("checkout".to_owned(), 2.0),
            ("ui_tap".to_owned(), 2.0),
            ("search".to_owned(), 1.0),
        ]
    );
    let by_screen = ranked(scratch.ask(&week(top(Dimension::Screen, Measure::Events))));
    assert_eq!(by_screen[0], ("home".to_owned(), 6.0));

    let limited = Query {
        report: Report::Top {
            dimension: Dimension::Event,
            measure: Measure::Events,
            event: None,
            limit: Some(1),
        },
        ..week(Report::Overview)
    };
    assert_eq!(ranked(scratch.ask(&limited)).len(), 1);
}

#[test]
#[ignore = "needs ClickHouse"]
fn usage_is_reported_per_screen_and_per_element() {
    let scratch = Scratch::new();
    seed(&scratch);
    let Answer::Screens { rows } = scratch
        .ask(&week(Report::Screens { limit: None }))
        .expect("answers")
    else {
        panic!("not screens");
    };
    let summary: Vec<_> = rows
        .iter()
        .map(|r| (r.screen.as_str(), r.views, r.viewers, r.taps, r.tappers))
        .collect();
    assert_eq!(summary, vec![("home", 3, 3, 2, 2), ("cart", 1, 1, 0, 0)]);

    let Answer::Elements { rows } = scratch
        .ask(&week(Report::Elements {
            screen: None,
            limit: None,
        }))
        .expect("answers")
    else {
        panic!("not elements");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].screen.as_str(),
            rows[0].element.as_str(),
            rows[0].taps,
            rows[0].tappers
        ),
        ("home", "buy", 2, 2)
    );
    let Answer::Elements { rows } = scratch
        .ask(&week(Report::Elements {
            screen: Some("cart".to_owned()),
            limit: None,
        }))
        .expect("answers")
    else {
        panic!("not elements");
    };
    assert!(rows.is_empty());
}

#[test]
#[ignore = "needs ClickHouse"]
fn session_aggregates_are_split_by_platform_entry_screen_and_filtered() {
    let scratch = Scratch::new();
    seed(&scratch);
    let rows = |breakdown: Option<SessionDimension>, filters: Vec<Filter>| {
        let query = Query {
            filters,
            ..week(Report::Sessions {
                breakdown,
                limit: None,
            })
        };
        match scratch.ask(&query).expect("answers") {
            Answer::Sessions { rows } => rows,
            other => panic!("not sessions: {other:?}"),
        }
    };
    let total = rows(None, Vec::new());
    assert_eq!((total[0].sessions, total[0].bounces), (3, 1));

    let by_platform = rows(Some(SessionDimension::Platform), Vec::new());
    let summary: Vec<_> = by_platform
        .iter()
        .map(|r| (r.key.clone().unwrap_or_default(), r.sessions, r.bounces))
        .collect();
    assert_eq!(
        summary,
        vec![("ios".to_owned(), 2, 0), ("android".to_owned(), 1, 1)]
    );

    let by_entry = rows(Some(SessionDimension::EntryScreen), Vec::new());
    assert_eq!(by_entry[0].key.as_deref(), Some("home"));
    assert_eq!(by_entry[0].sessions, 3);
    let by_exit = rows(Some(SessionDimension::ExitScreen), Vec::new());
    let exits: Vec<_> = by_exit
        .iter()
        .map(|r| (r.key.clone().unwrap_or_default(), r.sessions))
        .collect();
    assert_eq!(exits, vec![("home".to_owned(), 2), ("cart".to_owned(), 1)]);

    let br = rows(
        None,
        vec![Filter {
            dimension: Dimension::Country,
            values: vec!["BR".to_owned()],
        }],
    );
    assert_eq!(br[0].sessions, 2);
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_site_never_reads_another_sites_events() {
    let scratch = Scratch::new();
    seed(&scratch);
    let query = week(top(Dimension::Event, Measure::Events));
    let other = ranked(scratch.ask_with(&query, &Limits::default(), "UTC", 8));
    assert_eq!(other, vec![("screen".to_owned(), 1.0)]);
    let none = ranked(scratch.ask_with(&query, &Limits::default(), "UTC", 9));
    assert!(none.is_empty());
}

#[test]
#[ignore = "needs ClickHouse"]
fn hostile_filter_values_match_literally_and_do_no_harm() {
    let scratch = Scratch::new();
    seed(&scratch);
    let weird = "/a'b\\c\nd\te\"; DROP TABLE events; --";
    let mut row = event(
        20,
        14 * H,
        "screen",
        "a9",
        "s9",
        weird,
        &[("name", "home")],
        BR_IOS,
    );
    row.screen = weird.to_owned();
    scratch.insert(&[row]);
    let mut query = week(top(Dimension::Event, Measure::Events));
    query.filters = vec![Filter {
        dimension: Dimension::Screen,
        values: vec![weird.to_owned()],
    }];
    assert_eq!(
        ranked(scratch.ask(&query)),
        vec![("screen".to_owned(), 1.0)]
    );
    let rows = scratch
        .store
        .raw_query("SELECT count() FROM events")
        .expect("the table is still there");
    assert_eq!(rows.trim(), "13");
}

#[test]
#[ignore = "needs ClickHouse"]
fn an_unknown_time_zone_is_an_error_not_a_panic() {
    let scratch = Scratch::new();
    seed(&scratch);
    let query = week(series(Measure::Events, None, Granularity::Day, None));
    let answer = scratch.ask_with(&query, &Limits::default(), "Mars/Olympus_Mons", SITE);
    assert!(
        matches!(answer, Err(QueryError::Unavailable(_))),
        "{answer:?}"
    );
}
