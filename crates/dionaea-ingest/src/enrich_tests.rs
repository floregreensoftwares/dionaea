//! The duplicate memory, the user agent rules, the country lookup and how the
//! pipeline uses them.

use std::net::IpAddr;

use dionaea_core::DropReason;
use dionaea_meta::{MetaStore, SqliteMeta};
use serde_json::json;

use super::*;

const NOW: u64 = 1_760_000_000_000;

// The duplicate memory.

#[test]
fn an_id_is_remembered_until_its_window_ends() {
    let seen = Deduper::new(10, 1_000);
    assert!(!seen.contains(1, 0));
    seen.remember([1, 2], 0);
    assert!(seen.contains(1, 999));
    assert!(!seen.contains(1, 1_000), "gone when the window is over");
    assert!(!seen.contains(2, 5_000));
    assert!(seen.is_empty());
}

#[test]
fn the_oldest_ids_are_forgotten_first_when_it_is_full() {
    let seen = Deduper::new(3, 1_000_000);
    seen.remember([1], 10);
    seen.remember([2, 3], 20);
    seen.remember([4], 30);
    assert_eq!(seen.len(), 3);
    assert!(!seen.contains(1, 40));
    assert!(seen.contains(2, 40) && seen.contains(3, 40) && seen.contains(4, 40));
}

#[test]
fn remembering_twice_keeps_the_first_time_and_capacity_zero_remembers_nothing() {
    let seen = Deduper::new(10, 100);
    seen.remember([7], 0);
    seen.remember([7], 90);
    assert!(!seen.contains(7, 100), "the first time counts");
    assert_eq!(seen.len(), 0);
    let off = Deduper::new(0, 100);
    off.remember([1, 2, 3], 0);
    assert!(off.is_empty() && !off.contains(1, 0));
}

#[test]
fn the_memory_never_passes_its_capacity() {
    let seen = Deduper::new(100, u64::MAX);
    for n in 0..10_000u128 {
        seen.remember([n], n as u64);
        assert!(seen.len() <= 100);
    }
}

// The user agent.

fn ua(text: &str) -> (&'static str, &'static str, &'static str, &'static str, bool) {
    let parsed = parse_user_agent(text);
    (
        parsed.platform,
        parsed.os,
        parsed.browser,
        parsed.device_class,
        parsed.bot,
    )
}

#[test]
fn common_browsers_and_systems_are_named() {
    let cases = [
        (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
            ("web", "windows", "chrome", "desktop", false),
        ),
        (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0",
            ("web", "windows", "edge", "desktop", false),
        ),
        (
            "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0",
            ("web", "linux", "firefox", "desktop", false),
        ),
        (
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
            ("web", "macos", "safari", "desktop", false),
        ),
        (
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
            ("web", "ios", "safari", "phone", false),
        ),
        (
            "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
            ("web", "ios", "safari", "tablet", false),
        ),
        (
            "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
            ("web", "android", "chrome", "phone", false),
        ),
        (
            "Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
            ("web", "android", "chrome", "tablet", false),
        ),
        (
            "Mozilla/5.0 (Linux; Android 14; SAMSUNG SM-S918B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/25.0 Chrome/121.0.0.0 Mobile Safari/537.36",
            ("web", "android", "samsung", "phone", false),
        ),
        (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 OPR/111.0.0.0",
            ("web", "windows", "opera", "desktop", false),
        ),
    ];
    for (agent, expected) in cases {
        assert_eq!(ua(agent), expected, "{agent}");
    }
}

#[test]
fn native_http_stacks_give_a_platform_and_no_browser() {
    assert_eq!(ua("okhttp/4.12.0"), ("android", "", "", "", false));
    assert_eq!(
        ua("MyApp/1.0 CFNetwork/1494.0.7 Darwin/23.4.0"),
        ("ios", "ios", "", "", false)
    );
}

#[test]
fn automated_clients_are_recognized() {
    for agent in [
        "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/126.0.0.0 Safari/537.36",
        "curl/8.5.0",
        "Wget/1.21",
        "python-requests/2.31.0",
        "Go-http-client/2.0",
        "Mozilla/5.0 (compatible; bingbot/2.0)",
        "Pingdom.com_bot_version_1.4",
    ] {
        assert!(parse_user_agent(agent).bot, "{agent}");
    }
    assert!(
        !parse_user_agent("Mozilla/5.0 (Windows NT 10.0) Chrome/126.0 Safari/537.36").bot,
        "an ordinary browser is not a bot"
    );
}

#[test]
fn unknown_empty_and_oversized_agents_are_harmless() {
    assert_eq!(parse_user_agent(""), UserAgent::default());
    assert_eq!(
        parse_user_agent("something else entirely"),
        UserAgent::default()
    );
    let long = format!("Chrome/1 {}", "é".repeat(2_000));
    let parsed = parse_user_agent(&long);
    assert_eq!(parsed.browser, "chrome");
    // A cut in the middle of a multi-byte character must not panic.
    for length in 500..520 {
        let text = format!("{}é", "a".repeat(length));
        let _ = parse_user_agent(&text);
    }
}

#[test]
fn text_after_the_limit_is_not_read() {
    let padding = "x".repeat(600);
    assert!(!parse_user_agent(&format!("{padding} Googlebot")).bot);
    assert!(parse_user_agent(&format!("Googlebot {padding}")).bot);
}

// The country lookup, against a tiny database written here: IPv4 addresses
// below 128.0.0.0 are "AA", the rest "BB".

fn text(value: &str) -> Vec<u8> {
    let mut out = vec![(2 << 5) | u8::try_from(value.len()).unwrap()];
    out.extend(value.bytes());
    out
}

fn uint(kind: u8, value: u64) -> Vec<u8> {
    let bytes: Vec<u8> = value
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let length = u8::try_from(bytes.len()).unwrap();
    let mut out = if kind <= 7 {
        vec![(kind << 5) | length]
    } else {
        vec![length, kind - 7]
    };
    out.extend(bytes);
    out
}

fn country_record(code: &str) -> Vec<u8> {
    let mut out = vec![(7 << 5) | 1];
    out.extend(text("country"));
    out.push((7 << 5) | 1);
    out.extend(text("iso_code"));
    out.extend(text(code));
    out
}

fn database() -> Vec<u8> {
    let (a, b) = (country_record("AA"), country_record("BB"));
    let left = 1 + 16;
    let right = 1 + 16 + a.len();
    let mut file = Vec::new();
    for record in [left, right] {
        file.extend(&u32::try_from(record).unwrap().to_be_bytes()[1..]);
    }
    file.extend([0u8; 16]);
    file.extend(&a);
    file.extend(&b);
    file.extend(b"\xAB\xCD\xEFMaxMind.com");
    file.push((7 << 5) | 9);
    for (name, value) in [
        ("binary_format_major_version", uint(5, 2)),
        ("binary_format_minor_version", uint(5, 0)),
        ("build_epoch", uint(9, 1_700_000_000)),
        ("database_type", text("Test-Country")),
        ("ip_version", uint(5, 4)),
        ("node_count", uint(6, 1)),
        ("record_size", uint(5, 24)),
    ] {
        file.extend(text(name));
        file.extend(value);
    }
    file.extend(text("languages"));
    file.extend([1, 4]);
    file.extend(text("en"));
    file.extend(text("description"));
    file.push(7 << 5);
    file
}

fn lookup() -> MaxMind {
    MaxMind::from_bytes(database()).expect("a valid database")
}

#[test]
fn the_country_comes_from_the_database_and_unknown_addresses_have_none() {
    let lookup = lookup();
    let ip = |text: &str| text.parse::<IpAddr>().unwrap();
    assert_eq!(lookup.country(ip("10.1.2.3")).as_deref(), Some("AA"));
    assert_eq!(lookup.country(ip("200.1.2.3")).as_deref(), Some("BB"));
    assert_eq!(
        lookup.country(ip("2001:db8::1")),
        None,
        "an IPv6 address in an IPv4-only database"
    );
}

#[test]
fn a_file_that_is_not_a_database_is_an_error() {
    assert!(MaxMind::from_bytes(b"not a database".to_vec()).is_err());
    assert!(MaxMind::open(std::path::Path::new("no-such-file.mmdb")).is_err());
}

#[test]
fn the_enrichment_of_a_request_combines_agent_and_country() {
    let lookup = lookup();
    let ip: IpAddr = "200.1.2.3".parse().unwrap();
    let agent = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile/15E148 Safari/604.1";
    let full = Enrichment::of_request(Some(agent), Some(ip), Some(&lookup));
    assert_eq!(
        (
            full.country.as_str(),
            full.platform.as_str(),
            full.os.as_str(),
            full.browser.as_str(),
            full.device_class.as_str(),
            full.bot
        ),
        ("BB", "web", "ios", "safari", "phone", false)
    );
    let none = Enrichment::of_request(None, None, None);
    assert_eq!(none, Enrichment::default());
    let no_database = Enrichment::of_request(Some(agent), Some(ip), None);
    assert_eq!(no_database.country, "");
}

// The pipeline with a context.

const SCHEMA: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";

fn admission() -> Admission {
    let store = SqliteMeta::open_in_memory().unwrap();
    let organization = store.create_organization("o").unwrap();
    let site = store.create_site(organization.id, "s").unwrap();
    store.publish_schema(site.id, SCHEMA).unwrap();
    let key = store.issue_write_key(site.id, "k", &[]).unwrap().key;
    let cache = Cache::new();
    cache.refresh(&store).unwrap();
    cache.admit(&key, None).unwrap()
}

fn event(n: u32, name: &str) -> serde_json::Value {
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": NOW,
        "name": name,
        "anon_id": "a1",
        "props": {"items": 1},
    })
}

fn body(events: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({"sent_at": NOW, "sdk": "t/1", "events": events})).unwrap()
}

fn run(context: &Context<'_>, events: &[serde_json::Value]) -> Processed {
    process_with(
        &admission(),
        &body(events),
        NOW,
        &Limits::default(),
        context,
    )
    .unwrap()
}

fn dropped(processed: &Processed, name: &str, reason: Reason) -> u64 {
    processed
        .drops
        .iter()
        .find(|d| d.event_name == name && d.reason == reason)
        .map_or(0, |d| d.count)
}

#[test]
fn accepted_events_carry_the_enrichment_of_their_request() {
    let enrichment = Enrichment {
        country: "BR".to_owned(),
        platform: "web".to_owned(),
        os: "linux".to_owned(),
        browser: "firefox".to_owned(),
        device_class: "desktop".to_owned(),
        bot: false,
    };
    let context = Context {
        enrichment: Some(&enrichment),
        deduper: None,
    };
    let processed = run(&context, &[event(1, "checkout")]);
    let a = &processed.accepted[0];
    assert_eq!(
        (
            a.country.as_str(),
            a.platform.as_str(),
            a.os.as_str(),
            a.browser.as_str(),
            a.device_class.as_str()
        ),
        ("BR", "web", "linux", "firefox", "desktop")
    );
    assert_eq!(a.event_key & 0xFFFF_FFFF_FFFF, 1, "the id's last digits");
}

#[test]
fn a_bot_sends_events_that_are_all_counted_and_none_kept() {
    let enrichment = Enrichment {
        bot: true,
        ..Enrichment::default()
    };
    let context = Context {
        enrichment: Some(&enrichment),
        deduper: None,
    };
    let processed = run(
        &context,
        &[
            event(1, "checkout"),
            event(2, "mystery"),
            json!("not an event"),
        ],
    );
    assert!(processed.accepted.is_empty());
    assert_eq!(dropped(&processed, "checkout", Reason::Bot), 1);
    assert_eq!(dropped(&processed, UNDECLARED_EVENT, Reason::Bot), 1);
    assert_eq!(dropped(&processed, UNPARSED_EVENT, Reason::Bot), 1);
    assert_eq!(Reason::Bot.as_str(), "bot");
}

#[test]
fn an_id_already_stored_is_dropped_as_a_duplicate_and_one_not_stored_is_kept() {
    let seen = Deduper::new(100, 1_000_000);
    let context = Context {
        enrichment: None,
        deduper: Some(&seen),
    };
    let first = run(&context, &[event(1, "checkout"), event(2, "checkout")]);
    assert_eq!(first.accepted.len(), 2);
    // Not durable yet: the ids are not remembered, a retry is still accepted.
    let retry = run(&context, &[event(1, "checkout")]);
    assert_eq!(
        retry.accepted.len(),
        1,
        "an id is remembered only once stored"
    );

    seen.remember(first.accepted.iter().map(|a| a.event_key), NOW);
    let again = run(&context, &[event(1, "checkout"), event(3, "checkout")]);
    assert_eq!(again.accepted.len(), 1);
    assert_eq!(again.accepted[0].event_key & 0xFFFF_FFFF_FFFF, 3);
    assert_eq!(dropped(&again, "checkout", Reason::Duplicate), 1);
    assert_eq!(Reason::Duplicate.as_str(), "duplicate_event");
}

#[test]
fn the_same_id_twice_in_one_batch_is_stored_once() {
    let processed = run(
        &Context::default(),
        &[event(5, "checkout"), event(5, "checkout")],
    );
    assert_eq!(processed.accepted.len(), 1);
    assert_eq!(dropped(&processed, "checkout", Reason::Duplicate), 1);
}

#[test]
fn an_invalid_duplicate_is_counted_for_its_own_reason_not_as_a_duplicate() {
    let seen = Deduper::new(100, 1_000_000);
    seen.remember([9], NOW);
    let context = Context {
        enrichment: None,
        deduper: Some(&seen),
    };
    let mut bad = event(9, "checkout");
    bad["props"] = json!({});
    let processed = run(&context, &[bad]);
    assert_eq!(
        dropped(
            &processed,
            "checkout",
            Reason::Guard(DropReason::MissingRequiredField)
        ),
        1
    );
}
