use dionaea_core::{Schema, parse_schema};

use super::*;

const SCHEMA: &str = r#"
[events.checkout.fields.coupon]
kind = "enum"
values = ["save10", "save20"]

[events.checkout.fields.items]
kind = "count"
max = 100

[events.search.fields.term]
kind = "text"
max = 40
"#;

fn schema(source: &str) -> Schema {
    parse_schema(source).unwrap_or_else(|d| panic!("{d:?}"))
}

fn query(report: Report) -> Query {
    Query {
        from_ms: 1_700_000_000_000,
        to_ms: 1_700_000_000_000 + 7 * 86_400_000,
        filters: Vec::new(),
        report,
    }
}

fn series(event: Option<&str>, breakdown: Option<Dimension>, granularity: Granularity) -> Report {
    Report::Series {
        measure: Measure::Events,
        event: event.map(str::to_owned),
        granularity,
        breakdown,
    }
}

fn check(query: &Query) -> Result<Plan, QueryError> {
    let schemas = [schema(SCHEMA)];
    let site = Site {
        id: 7,
        time_zone: "America/Sao_Paulo",
        schemas: &schemas,
    };
    validate(site, query, &Limits::default())
}

fn property(event: &str, name: &str) -> Dimension {
    Dimension::Property {
        event: event.to_owned(),
        name: name.to_owned(),
    }
}

fn filter(dimension: Dimension, values: &[&str]) -> Filter {
    Filter {
        dimension,
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

#[test]
fn a_declared_event_and_property_are_accepted() {
    let report = series(
        Some("checkout"),
        Some(property("checkout", "coupon")),
        Granularity::Day,
    );
    assert!(check(&query(report)).is_ok());
}

#[test]
fn builtin_events_need_no_declaration() {
    let schemas: [Schema; 0] = [];
    let site = Site {
        id: 1,
        time_zone: "UTC",
        schemas: &schemas,
    };
    let report = Report::Elements {
        screen: None,
        limit: None,
    };
    assert!(validate(site, &query(report), &Limits::default()).is_ok());
    let by_element = Dimension::Property {
        event: "ui_tap".to_owned(),
        name: "element".to_owned(),
    };
    let top = Report::Top {
        dimension: by_element,
        measure: Measure::Events,
        event: None,
        limit: None,
    };
    assert!(validate(site, &query(top), &Limits::default()).is_ok());
}

#[test]
fn an_undeclared_event_is_refused() {
    let report = series(Some("purchase"), None, Granularity::Day);
    assert_eq!(
        check(&query(report)),
        Err(QueryError::UndeclaredEvent("purchase".to_owned()))
    );
}

#[test]
fn an_undeclared_property_is_refused_in_a_breakdown_and_in_a_filter() {
    let report = series(
        Some("checkout"),
        Some(property("checkout", "email")),
        Granularity::Day,
    );
    let expected = Err(QueryError::UndeclaredProperty {
        event: "checkout".to_owned(),
        name: "email".to_owned(),
    });
    assert_eq!(check(&query(report)), expected);

    let mut q = query(Report::Overview);
    q.filters = vec![filter(property("checkout", "email"), &["a"])];
    assert_eq!(check(&q), expected);
}

#[test]
fn a_property_of_another_event_is_undeclared() {
    let report = series(None, Some(property("search", "coupon")), Granularity::Day);
    assert!(matches!(
        check(&query(report)),
        Err(QueryError::UndeclaredProperty { .. })
    ));
}

#[test]
fn a_numeric_property_cannot_be_grouped() {
    let report = series(None, Some(property("checkout", "items")), Granularity::Day);
    assert_eq!(
        check(&query(report)),
        Err(QueryError::NotGroupable {
            event: "checkout".to_owned(),
            name: "items".to_owned()
        })
    );
}

#[test]
fn a_property_that_changed_kind_between_versions_cannot_be_grouped() {
    let old = schema("[events.checkout.fields.coupon]\nkind = \"enum\"\nvalues = [\"a\"]\n");
    let new = schema("[events.checkout.fields.coupon]\nkind = \"count\"\nmax = 5\n");
    let schemas = [old, new];
    let site = Site {
        id: 7,
        time_zone: "UTC",
        schemas: &schemas,
    };
    let report = series(None, Some(property("checkout", "coupon")), Granularity::Day);
    assert!(matches!(
        validate(site, &query(report), &Limits::default()),
        Err(QueryError::NotGroupable { .. })
    ));
}

#[test]
fn two_events_in_one_query_conflict() {
    let report = series(
        Some("search"),
        Some(property("checkout", "coupon")),
        Granularity::Day,
    );
    assert_eq!(check(&query(report)), Err(QueryError::ConflictingEvents));

    let mut q = query(series(Some("checkout"), None, Granularity::Day));
    q.filters = vec![filter(property("search", "term"), &["a"])];
    assert_eq!(check(&q), Err(QueryError::ConflictingEvents));
}

#[test]
fn the_interval_is_checked() {
    let mut q = query(Report::Overview);
    q.to_ms = q.from_ms;
    assert_eq!(check(&q), Err(QueryError::EmptyRange));

    let mut q = query(Report::Overview);
    q.to_ms = q.from_ms + 367 * 86_400_000;
    assert_eq!(check(&q), Err(QueryError::RangeTooLarge { max_days: 366 }));
    q.to_ms = q.from_ms + 366 * 86_400_000;
    assert!(check(&q).is_ok());

    let mut q = query(series(None, None, Granularity::Hour));
    q.to_ms = q.from_ms + 32 * 86_400_000;
    assert_eq!(check(&q), Err(QueryError::RangeTooLarge { max_days: 31 }));

    let mut q = query(Report::Overview);
    q.from_ms = -1;
    assert_eq!(check(&q), Err(QueryError::TimeOutOfRange));
    let mut q = query(Report::Overview);
    q.to_ms = i64::MAX;
    assert_eq!(check(&q), Err(QueryError::TimeOutOfRange));
}

#[test]
fn filters_are_bounded() {
    let mut q = query(Report::Overview);
    q.filters = (0..9)
        .map(|_| filter(Dimension::Country, &["BR"]))
        .collect();
    assert_eq!(check(&q), Err(QueryError::TooManyFilters { max: 8 }));

    for bad in [
        filter(Dimension::Country, &[]),
        filter(Dimension::Country, &[""]),
        filter(Dimension::Country, &[&"x".repeat(257)]),
    ] {
        let mut q = query(Report::Overview);
        q.filters = vec![bad];
        assert_eq!(check(&q), Err(QueryError::InvalidFilterValues));
    }
    let mut q = query(Report::Overview);
    q.filters = vec![Filter {
        dimension: Dimension::Country,
        values: (0..101).map(|i| format!("v{i}")).collect(),
    }];
    assert_eq!(check(&q), Err(QueryError::InvalidFilterValues));
}

#[test]
fn list_limits_are_bounded() {
    for limit in [0, 101] {
        let report = Report::Screens { limit: Some(limit) };
        assert_eq!(
            check(&query(report)),
            Err(QueryError::InvalidLimit { max: 100 })
        );
    }
    assert!(check(&query(Report::Screens { limit: Some(100) })).is_ok());
}

#[test]
fn session_reports_take_only_session_filters() {
    let mut q = query(Report::Sessions {
        breakdown: None,
        limit: None,
    });
    q.filters = vec![filter(Dimension::Platform, &["ios"])];
    assert!(check(&q).is_ok());
    q.filters = vec![filter(Dimension::Os, &["iOS 18"])];
    assert_eq!(check(&q), Err(QueryError::FilterNotAvailable));
}

#[test]
fn the_overview_leaves_sessions_out_when_a_filter_does_not_apply_to_them() {
    let mut q = query(Report::Overview);
    q.filters = vec![filter(Dimension::Os, &["iOS 18"])];
    let plan = check(&q).expect("valid");
    assert!(matches!(
        plan.kind,
        crate::validate::Kind::Overview {
            session_filters: None,
            ..
        }
    ));
}

#[test]
fn a_bad_time_zone_is_refused() {
    let schemas: [Schema; 0] = [];
    for zone in ["", "../etc", "a b", "x'y", &"z".repeat(65)] {
        let site = Site {
            id: 1,
            time_zone: zone,
            schemas: &schemas,
        };
        assert_eq!(
            validate(site, &query(Report::Overview), &Limits::default()),
            Err(QueryError::InvalidTimeZone),
            "{zone:?}"
        );
    }
    assert!(is_time_zone_shape("America/Argentina/Buenos_Aires"));
    assert!(is_time_zone_shape("Etc/GMT+3"));
}

#[test]
fn a_request_reads_from_json_and_refuses_unknown_fields() {
    let json = r#"{
        "from_ms": 1, "to_ms": 2,
        "filters": [{"dimension": "country", "values": ["BR"]}],
        "report": {"type": "series", "measure": "users", "event": null,
                   "granularity": "day",
                   "breakdown": {"dimension": "property", "event": "checkout", "name": "coupon"}}
    }"#;
    let q: Query = serde_json::from_str(json).expect("reads");
    assert_eq!(q.filters[0].dimension, Dimension::Country);

    let bad = r#"{"from_ms": 1, "to_ms": 2, "report": {"type": "overview"}, "sql": "x"}"#;
    assert!(serde_json::from_str::<Query>(bad).is_err());
    let bad = r#"{"from_ms": 1, "to_ms": 2, "report": {"type": "nope"}}"#;
    assert!(serde_json::from_str::<Query>(bad).is_err());
}

#[test]
fn every_error_has_a_distinct_code() {
    let errors = [
        QueryError::TimeOutOfRange,
        QueryError::EmptyRange,
        QueryError::RangeTooLarge { max_days: 1 },
        QueryError::UndeclaredEvent(String::new()),
        QueryError::UndeclaredProperty {
            event: String::new(),
            name: String::new(),
        },
        QueryError::NotGroupable {
            event: String::new(),
            name: String::new(),
        },
        QueryError::ConflictingEvents,
        QueryError::TooManyFilters { max: 1 },
        QueryError::InvalidFilterValues,
        QueryError::FilterNotAvailable,
        QueryError::InvalidLimit { max: 1 },
        QueryError::BreakdownTooHigh { max: 1 },
        QueryError::InvalidTimeZone,
        QueryError::TooSlow,
        QueryError::TooManyRows,
        QueryError::Unavailable(String::new()),
    ];
    let codes: std::collections::BTreeSet<_> = errors.iter().map(QueryError::code).collect();
    assert_eq!(codes.len(), errors.len());
}
