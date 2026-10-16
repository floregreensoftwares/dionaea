//! Event ids as the store records and looks them up.

use super::*;

fn row(site: u32, id: &str, note: &str) -> EventRow {
    EventRow {
        site_id: site,
        event_id: id.to_owned(),
        ts_ms: 1_760_000_000_000,
        received_ms: 1_760_000_001_000,
        name: note.to_owned(),
        schema_version: 1,
        anon_id: note.to_owned(),
        session_id: note.to_owned(),
        screen: String::new(),
        props_str: vec![("coupon".to_owned(), note.to_owned())],
        props_num: Vec::new(),
        country: String::new(),
        platform: String::new(),
        os: String::new(),
        browser: String::new(),
        device_class: String::new(),
        app_version: String::new(),
        sdk: String::new(),
    }
}

#[test]
fn the_key_of_a_line_is_read_whatever_the_rest_of_it_holds() {
    let id = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";
    // Content that imitates the start of another line must not matter: the
    // key comes from the first two fields, which the sender cannot write.
    let tricky = r#"{"site_id":99,"event_id":"ffffffff-ffff-7fff-bfff-ffffffffffff"}"#;
    for note in ["plain", tricky, "quote\" and \\ and \n newline", "é 😀"] {
        for site in [0, 7, u32::MAX] {
            let lines = Store::encode_events(&[row(site, id, note)]).expect("encode");
            let line = lines.lines().next().expect("one line");
            assert_eq!(event_key(line), Some((site, id)), "{note}");
        }
    }
}

#[test]
fn a_line_that_is_not_an_event_has_no_key() {
    for line in [
        "",
        "{}",
        r#"{"site_id":7}"#,
        r#"{"site_id":x,"event_id":"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e","#,
        r#"{"site_id":7,"event_id":"not-a-uuid-not-a-uuid-not-a-uuid-12","#,
        r#"{"site_id":7,"event_id":"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6eXX","#,
        r#"{"event_id":"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e","site_id":7}"#,
        r#"{"site_id":99999999999,"event_id":"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e","#,
    ] {
        assert_eq!(event_key(line), None, "{line}");
    }
}

#[test]
fn recorded_ids_become_lines_with_the_time_they_were_seen() {
    let id = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";
    let other = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6f";
    let lines = Store::encode_event_ids([(7, id), (8, other)], 1_760_000_000_123).expect("encode");
    let parsed: Vec<serde_json::Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0]["site_id"], 7);
    assert_eq!(parsed[1]["event_id"], other);
    assert_eq!(parsed[0]["seen_at"], "2025-10-09 08:53:20.123");
    assert!(matches!(
        Store::encode_event_ids([(7, "x'); DROP TABLE events; --")], 0),
        Err(Error::InvalidRow(_))
    ));
}

#[test]
fn the_lookup_statement_holds_only_numbers_and_checked_ids() {
    let a = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";
    let b = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6f";
    let sql = known_ids_sql(&[(7, a), (u32::MAX, b)]).expect("sql");
    assert_eq!(
        sql,
        format!(
            "SELECT site_id, toString(event_id) FROM event_ids WHERE (site_id, event_id) IN ((7,'{a}'),(4294967295,'{b}'))"
        )
    );
    for bad in [
        "",
        "x",
        "' OR 1=1 --",
        "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e'",
        "0190F7E2-7C3E-7A52-9A0E-5F3C2A1B4D6G",
    ] {
        assert!(known_ids_sql(&[(1, bad)]).is_err(), "{bad:?}");
    }
    let many: Vec<(u32, &str)> = vec![(1, a); MAX_IDS_PER_LOOKUP + 1];
    assert!(matches!(known_ids_sql(&many), Err(Error::TooManyRows)));
    assert!(known_ids_sql(&many[..MAX_IDS_PER_LOOKUP]).is_ok());
}

#[test]
fn an_empty_lookup_asks_nothing() {
    let mut config = Config::new("127.0.0.1", 1, "dionaea");
    config.timeout = std::time::Duration::from_millis(100);
    let store = Store::new(config).expect("config");
    assert!(store.known_event_ids(&[]).expect("no request").is_empty());
    assert_eq!(store.connections_opened(), 0);
}
