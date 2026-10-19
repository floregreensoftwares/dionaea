use dionaea_core::DropReason;
use dionaea_meta::{MetaStore, SqliteMeta};
use serde_json::json;

use super::*;

const V1: &str = r#"
[events.checkout.fields.items]
kind = "count"
max = 100
required = true
"#;

/// V1 plus optional fields.
const V2: &str = r#"
[events.checkout.fields.items]
kind = "count"
max = 100
required = true

[events.checkout.fields.coupon]
kind = "text"
max = 32

[events.checkout.fields.progress]
kind = "unit"
"#;

/// V2 plus a new event.
const V3: &str = r#"
[events.checkout.fields.items]
kind = "count"
max = 100
required = true

[events.checkout.fields.coupon]
kind = "text"
max = 32

[events.checkout.fields.progress]
kind = "unit"

[events.refund.fields.reason]
kind = "id"
required = true
"#;

const NOW: u64 = 1_760_000_000_000;

fn world(schemas: &[&str], origins: &[&str]) -> (SqliteMeta, Cache, String) {
    let store = SqliteMeta::open_in_memory().expect("store");
    let organization = store.create_organization("o").expect("organization");
    let site = store.create_site(organization.id, "s").expect("site");
    for schema in schemas {
        store.publish_schema(site.id, schema).expect("schema");
    }
    let origins: Vec<String> = origins.iter().map(|o| (*o).to_owned()).collect();
    let key = store
        .issue_write_key(site.id, "k", &origins)
        .expect("key")
        .key;
    let cache = Cache::new();
    assert!(cache.refresh(&store).expect("refresh"));
    (store, cache, key)
}

fn admission(schemas: &[&str]) -> Admission {
    let (_, cache, key) = world(schemas, &[]);
    cache.admit(&key, None).expect("admitted")
}

fn event(n: u32, ts: u64, name: &str, props: serde_json::Value) -> serde_json::Value {
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": ts,
        "name": name,
        "anon_id": "anon1",
        "props": props,
    })
}

fn body(sent_at: u64, events: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "sent_at": sent_at,
        "sdk": "test/1.0",
        "release": "1.4.2",
        "events": events,
    }))
    .expect("json")
}

fn run(admission: &Admission, sent_at: u64, events: &[serde_json::Value]) -> Processed {
    process(admission, &body(sent_at, events), NOW, &Limits::default()).expect("a valid batch")
}

fn drop_of(processed: &Processed, name: &str, reason: Reason) -> u64 {
    processed
        .drops
        .iter()
        .find(|d| d.event_name == name && d.reason == reason)
        .map_or(0, |d| d.count)
}

// Admission.

#[test]
fn an_empty_cache_admits_nothing_and_loads_once_then_only_on_change() {
    let cache = Cache::new();
    assert_eq!(
        cache
            .admit("wk_aaaaaaaaaaaaaaaaaaaaaaaaaa", None)
            .unwrap_err(),
        Refusal::UnknownKey
    );
    let store = SqliteMeta::open_in_memory().expect("store");
    assert!(
        cache.refresh(&store).expect("refresh"),
        "an empty store still loads once"
    );
    assert!(!cache.refresh(&store).expect("refresh"), "nothing changed");

    let organization = store.create_organization("o").expect("organization");
    let site = store.create_site(organization.id, "s").expect("site");
    assert!(
        !cache.refresh(&store).expect("refresh"),
        "sites are not cached"
    );
    let key = store.issue_write_key(site.id, "k", &[]).expect("key").key;
    assert!(cache.refresh(&store).expect("refresh"));
    assert_eq!(cache.generation(), store.generation().expect("generation"));
    assert_eq!(cache.admit(&key, None).expect("admitted").site_id, site.id);
}

#[test]
fn a_revoked_key_stops_being_admitted_after_a_refresh() {
    let (store, cache, key) = world(&[V1], &[]);
    assert!(cache.admit(&key, None).is_ok());
    store.revoke_write_key(&key).expect("revoke");
    assert!(
        cache.admit(&key, None).is_ok(),
        "the cache is stale until refreshed"
    );
    assert!(cache.refresh(&store).expect("refresh"));
    assert_eq!(cache.admit(&key, None).unwrap_err(), Refusal::UnknownKey);
}

#[test]
fn origins_are_enforced_only_for_keys_that_list_them() {
    let (_, cache, key) = world(&[V1], &["https://shop.example"]);
    assert!(cache.admit(&key, Some("https://shop.example")).is_ok());
    assert!(
        cache.admit(&key, Some("https://SHOP.example")).is_ok(),
        "case does not matter"
    );
    for bad in [
        Some("https://evil.example"),
        Some("http://shop.example"),
        Some(""),
        None,
    ] {
        assert_eq!(
            cache.admit(&key, bad).unwrap_err(),
            Refusal::OriginNotAllowed,
            "{bad:?}"
        );
    }
    let (_, cache, key) = world(&[V1], &[]);
    assert!(
        cache.admit(&key, None).is_ok(),
        "a native app sends no origin"
    );
    assert!(cache.admit(&key, Some("https://anything.example")).is_ok());
}

#[test]
fn schemas_are_served_newest_first_and_retired_ones_are_gone() {
    let (store, cache, key) = world(&[V1, V2, V3], &[]);
    let versions = |cache: &Cache| -> Vec<u32> {
        cache
            .admit(&key, None)
            .expect("admitted")
            .schemas
            .iter()
            .map(|(v, _)| *v)
            .collect()
    };
    assert_eq!(versions(&cache), [3, 2, 1]);
    store.retire_schema_version(1, 1).expect("retire");
    assert!(cache.refresh(&store).expect("refresh"));
    assert_eq!(versions(&cache), [3, 2]);
}

#[test]
fn a_site_without_a_schema_is_admitted_with_none() {
    let (_, cache, key) = world(&[], &[]);
    assert!(
        cache
            .admit(&key, None)
            .expect("admitted")
            .schemas
            .is_empty()
    );
}

// Rate limiting.

#[test]
fn a_burst_is_allowed_then_refused_with_a_retry_time_then_refills() {
    let limiter = RateLimiter::new(2.0, 3.0, 100);
    for _ in 0..3 {
        limiter.check("k", 1_000).expect("within the burst");
    }
    assert_eq!(limiter.check("k", 1_000), Err(RetryAfter(1)));
    assert_eq!(
        limiter.check("k", 1_400),
        Err(RetryAfter(1)),
        "0.8 tokens is not enough"
    );
    limiter
        .check("k", 1_500)
        .expect("one token after 500 ms at 2 per second");
    assert!(limiter.check("k", 1_500).is_err());
    for _ in 0..3 {
        limiter.check("k", 100_000).expect("refilled");
    }
    assert!(
        limiter.check("k", 100_000).is_err(),
        "the burst is a ceiling"
    );
}

#[test]
fn the_retry_time_rounds_up_to_whole_seconds() {
    let limiter = RateLimiter::new(0.25, 1.0, 10);
    limiter.check("k", 0).expect("first");
    assert_eq!(
        limiter.check("k", 0),
        Err(RetryAfter(4)),
        "one token takes 4 s at 0.25 per second"
    );
    assert_eq!(limiter.check("k", 1_000), Err(RetryAfter(3)));
}

#[test]
fn keys_are_independent_and_a_clock_that_steps_back_is_harmless() {
    let limiter = RateLimiter::new(1.0, 1.0, 10);
    limiter.check("a", 10_000).expect("a");
    limiter.check("b", 10_000).expect("b is separate");
    assert!(limiter.check("a", 10_000).is_err());
    assert!(
        limiter.check("a", 5_000).is_err(),
        "going back refills nothing"
    );
    limiter
        .check("a", 11_000)
        .expect("and time moving on works");
}

#[test]
fn the_number_of_tracked_keys_never_passes_the_bound() {
    let limiter = RateLimiter::new(1.0, 2.0, 50);
    for n in 0..10_000u32 {
        let _ = limiter.check(n, 1_000);
        assert!(limiter.len() <= 50);
    }
    assert!(!limiter.is_empty());
}

#[test]
fn full_buckets_are_forgotten_before_active_ones() {
    let limiter = RateLimiter::new(1.0, 2.0, 2);
    limiter.check("busy", 0).expect("busy");
    limiter.check("busy", 0).expect("busy");
    assert!(limiter.check("busy", 0).is_err(), "busy is empty");
    limiter.check("idle", 0).expect("idle");
    // "idle" has one token short of full, "busy" none: a new key evicts
    // nobody that is still limited while a refilled bucket exists.
    limiter.check("other", 3_000).expect("room is made");
    assert!(
        limiter.check("other", 3_000).is_ok(),
        "other has its burst of two"
    );
    assert!(limiter.len() <= 2);
}

// The pipeline.

#[test]
fn an_accepted_event_carries_its_fields_version_and_envelope() {
    let admission = admission(&[V2]);
    let processed = run(
        &admission,
        NOW,
        &[event(
            1,
            NOW - 5_000,
            "checkout",
            json!({"items": 3, "coupon": "SPRING", "progress": 0.12345, "undeclared": 1}),
        )],
    );
    assert!(processed.drops.is_empty());
    let [accepted] = processed.accepted.as_slice() else {
        panic!("one event")
    };
    assert_eq!(accepted.site_id, admission.site_id);
    assert_eq!(accepted.event_id, "01890000-0000-7000-8000-000000000001");
    assert_eq!((accepted.ts_ms, accepted.received_ms), (NOW - 5_000, NOW));
    assert_eq!(
        (accepted.name.as_str(), accepted.schema_version),
        ("checkout", 1)
    );
    assert_eq!(accepted.anon_id, "anon1");
    assert_eq!(
        accepted.strings,
        [("coupon".to_owned(), "SPRING".to_owned())]
    );
    assert_eq!(
        accepted.numbers,
        [("items".to_owned(), 3.0), ("progress".to_owned(), 0.123)],
        "counts and units are numbers, units to three decimals, undeclared stripped"
    );
    assert_eq!(
        (accepted.app_version.as_deref(), accepted.sdk.as_str()),
        (Some("1.4.2"), "test/1.0")
    );
}

#[test]
fn drops_are_counted_by_event_and_reason_and_never_keep_a_payload() {
    let admission = admission(&[V1]);
    let ok = json!({"items": 1});
    let processed = run(
        &admission,
        NOW,
        &[
            event(1, NOW, "checkout", ok.clone()),
            event(2, NOW, "checkout", json!({})),
            event(3, NOW, "checkout", json!({})),
            event(4, NOW, "checkout", json!({"items": "many"})),
            event(5, NOW, "mystery_one", ok.clone()),
            event(6, NOW, "mystery_two", ok),
            json!("not an event"),
            json!({"event_id": "nope", "ts": NOW, "name": "checkout", "anon_id": "a"}),
        ],
    );
    assert_eq!(processed.accepted.len(), 1);
    let missing = Reason::Guard(DropReason::MissingRequiredField);
    assert_eq!(drop_of(&processed, "checkout", missing), 2);
    assert_eq!(
        drop_of(
            &processed,
            "checkout",
            Reason::Guard(DropReason::InvalidRequiredField)
        ),
        1
    );
    assert_eq!(
        drop_of(
            &processed,
            UNDECLARED_EVENT,
            Reason::Guard(DropReason::UnknownEvent)
        ),
        2,
        "invented names share one counter"
    );
    assert_eq!(
        drop_of(
            &processed,
            UNPARSED_EVENT,
            Reason::Guard(DropReason::MalformedEvent)
        ),
        1
    );
    assert_eq!(
        drop_of(
            &processed,
            UNPARSED_EVENT,
            Reason::Guard(DropReason::InvalidEventId)
        ),
        1
    );
    let total: u64 = processed.drops.iter().map(|d| d.count).sum();
    assert_eq!(total, 7);
    let rendered = format!("{:?}", processed.drops);
    assert!(
        !rendered.contains("mystery") && !rendered.contains("many"),
        "{rendered}"
    );
}

#[test]
fn a_built_in_event_is_counted_under_its_own_name() {
    let admission = admission(&[V1]);
    let processed = run(&admission, NOW, &[event(1, NOW, "screen", json!({}))]);
    assert_eq!(
        drop_of(
            &processed,
            "screen",
            Reason::Guard(DropReason::MissingRequiredField)
        ),
        1
    );
}

#[test]
fn an_event_is_admitted_by_the_newest_version_that_accepts_it() {
    let admission = admission(&[V1, V2, V3]);
    let processed = run(
        &admission,
        NOW,
        &[
            event(1, NOW, "checkout", json!({"items": 1})),
            event(2, NOW, "refund", json!({"reason": "late"})),
            event(3, NOW, "checkout", json!({"items": 1, "coupon": "X"})),
        ],
    );
    let versions: Vec<u32> = processed
        .accepted
        .iter()
        .map(|a| a.schema_version)
        .collect();
    assert_eq!(versions, [3, 3, 3]);

    let admission = self::admission(&[V1, V2]);
    let processed = run(
        &admission,
        NOW,
        &[event(1, NOW, "checkout", json!({"items": 1}))],
    );
    assert_eq!(processed.accepted[0].schema_version, 2);
}

#[test]
fn an_event_only_an_older_version_accepts_is_admitted_by_it() {
    // Version 2 makes `region` a required field that version 1 never had;
    // an old app that does not send it is still valid under version 1.
    let strict = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n\n[events.checkout.fields.region]\nkind = \"id\"\nrequired = true\n";
    let mixed = Admission {
        site_id: 1,
        schemas: std::sync::Arc::from(vec![
            (2, dionaea_core::parse_schema(strict).expect("strict")),
            (1, dionaea_core::parse_schema(V1).expect("v1")),
        ]),
        tombstones: Default::default(),
        sample_ppm: dionaea_meta::SAMPLE_FULL_PPM,
    };
    let processed = run(
        &mixed,
        NOW,
        &[
            event(1, NOW, "checkout", json!({"items": 1})),
            event(2, NOW, "checkout", json!({"items": 1, "region": "eu"})),
        ],
    );
    let versions: Vec<u32> = processed
        .accepted
        .iter()
        .map(|a| a.schema_version)
        .collect();
    assert_eq!(
        versions,
        [1, 2],
        "old apps keep working, new ones use the newest"
    );
}

#[test]
fn a_dropped_event_reports_the_reason_from_the_newest_version() {
    let admission = admission(&[V1, V3]);
    let processed = run(&admission, NOW, &[event(1, NOW, "refund", json!({}))]);
    assert_eq!(
        drop_of(
            &processed,
            "refund",
            Reason::Guard(DropReason::MissingRequiredField)
        ),
        1,
        "version 1 does not know `refund`; the newest version says why it fails"
    );
}

#[test]
fn a_site_with_no_schema_drops_everything_as_unknown() {
    let admission = admission(&[]);
    let processed = run(
        &admission,
        NOW,
        &[event(1, NOW, "checkout", json!({"items": 1}))],
    );
    assert!(processed.accepted.is_empty());
    assert_eq!(
        drop_of(
            &processed,
            UNDECLARED_EVENT,
            Reason::Guard(DropReason::UnknownEvent)
        ),
        1
    );
}

#[test]
fn event_times_are_corrected_for_the_device_clock() {
    let admission = admission(&[V1]);
    let hour = 3_600_000;
    // The device is an hour behind: its clock says NOW - 1 h when sending,
    // and the event happened a minute before that.
    let processed = run(
        &admission,
        NOW - hour,
        &[event(
            1,
            NOW - hour - 60_000,
            "checkout",
            json!({"items": 1}),
        )],
    );
    assert_eq!(processed.accepted[0].ts_ms, NOW - 60_000);
    // A device an hour ahead.
    let processed = run(
        &admission,
        NOW + hour,
        &[event(
            1,
            NOW + hour - 1_000,
            "checkout",
            json!({"items": 1}),
        )],
    );
    assert_eq!(processed.accepted[0].ts_ms, NOW - 1_000);
}

#[test]
fn events_too_far_in_the_past_or_future_are_dropped_with_their_own_reason() {
    let admission = admission(&[V1]);
    let limits = Limits::default();
    let at = |ts: u64| {
        let body = body(NOW, &[event(1, ts, "checkout", json!({"items": 1}))]);
        process(&admission, &body, NOW, &limits).expect("batch")
    };
    assert_eq!(
        at(NOW - limits.max_past_ms).accepted.len(),
        1,
        "the edge is inside"
    );
    assert_eq!(at(NOW + limits.max_future_ms).accepted.len(), 1);
    for ts in [
        NOW - limits.max_past_ms - 1,
        NOW + limits.max_future_ms + 1,
        0,
    ] {
        let processed = at(ts);
        assert!(processed.accepted.is_empty(), "{ts}");
        assert_eq!(
            drop_of(&processed, "checkout", Reason::TimestampOutOfRange),
            1,
            "{ts}"
        );
    }
    assert_eq!(
        Reason::TimestampOutOfRange.as_str(),
        "timestamp_out_of_range"
    );
}

#[test]
fn a_bad_body_is_refused_whole() {
    let admission = admission(&[V1]);
    let limits = Limits::default();
    assert!(process(&admission, b"not json", NOW, &limits).is_err());
    assert!(process(&admission, b"{}", NOW, &limits).is_err());
    let too_big = vec![b' '; dionaea_core::MAX_BODY_LEN + 1];
    assert!(process(&admission, &too_big, NOW, &limits).is_err());
    let many: Vec<_> = (0..101)
        .map(|n| event(n, NOW, "checkout", json!({"items": 1})))
        .collect();
    assert!(process(&admission, &body(NOW, &many), NOW, &limits).is_err());
}

#[test]
fn extreme_times_do_not_overflow() {
    let admission = admission(&[V1]);
    let limits = Limits {
        max_past_ms: u64::MAX,
        max_future_ms: u64::MAX,
    };
    let max = dionaea_core::MAX_TIMESTAMP;
    let one = |sent: u64, ts: u64, received: u64| {
        let body = body(sent, &[event(1, ts, "checkout", json!({"items": 1}))]);
        process(&admission, &body, received, &limits).expect("batch")
    };
    assert!(one(max, max, u64::MAX).accepted.len() <= 1);
    assert!(one(0, max, 0).accepted.len() <= 1);
    assert!(one(max, 0, u64::MAX).accepted.len() <= 1);
}

// Changing limits while they are in use.

#[test]
fn a_limiter_takes_a_new_rate_and_burst_without_forgetting_its_keys() {
    let limiter = RateLimiter::new(1.0, 5.0, 100);
    for _ in 0..5 {
        limiter.check("k", 0).expect("within the burst");
    }
    assert!(limiter.check("k", 0).is_err());
    // A larger burst and rate are felt as time passes.
    limiter.reconfigure(10.0, 20.0, 100);
    assert!(
        limiter.check("k", 500).is_ok(),
        "5 tokens after 500 ms at 10 per second"
    );
    // A smaller burst caps what a bucket may hold.
    limiter.reconfigure(10.0, 2.0, 100);
    limiter.check("k", 100_000).expect("one");
    limiter.check("k", 100_000).expect("two");
    assert!(limiter.check("k", 100_000).is_err(), "the new burst is two");
}

#[test]
fn a_limiter_that_must_hold_fewer_keys_forgets_the_surplus_at_once() {
    let limiter = RateLimiter::new(1.0, 2.0, 100);
    for n in 0..60u32 {
        limiter.check(n, 0).expect("new key");
    }
    assert_eq!(limiter.len(), 60);
    limiter.reconfigure(1.0, 2.0, 10);
    assert_eq!(limiter.len(), 10);
    limiter.check(1000, 0).expect("still works");
    assert!(limiter.len() <= 10);
}

#[test]
fn a_limiter_ignores_limits_it_could_not_have_been_made_with() {
    let limiter = RateLimiter::new(1.0, 3.0, 10);
    for bad in [
        (0.0, 3.0, 10),
        (1.0, 0.5, 10),
        (1.0, 3.0, 0),
        (f64::NAN, 3.0, 10),
        (1.0, f64::INFINITY, 10),
    ] {
        limiter.reconfigure(bad.0, bad.1, bad.2);
    }
    for _ in 0..3 {
        limiter.check("k", 0).expect("the burst is still three");
    }
    assert!(limiter.check("k", 0).is_err());
}

#[test]
fn the_duplicate_memory_can_be_resized_and_turned_off_while_in_use() {
    let seen = Deduper::new(10, 1_000_000);
    seen.remember(0..10u128, 0);
    seen.reconfigure(3, 1_000_000);
    assert_eq!(seen.len(), 3);
    assert!(
        seen.contains(9, 1) && !seen.contains(0, 1),
        "the oldest went"
    );
    seen.reconfigure(10, 100);
    assert!(!seen.contains(9, 1_000), "the shorter window applies");
    seen.remember([50], 2_000);
    seen.reconfigure(0, 100);
    assert!(seen.is_empty(), "off forgets everything");
    seen.remember([51], 3_000);
    assert!(seen.is_empty(), "and remembers nothing");
    seen.reconfigure(5, 100);
    seen.remember([52], 4_000);
    assert!(seen.contains(52, 4_001), "on again");
}

#[test]
fn settings_stored_in_the_metadata_reach_the_cache_when_the_generation_moves() {
    let store = SqliteMeta::open_in_memory().expect("store");
    let cache = Cache::new();
    cache.refresh(&store).expect("refresh");
    assert!(cache.settings().is_empty());
    store
        .set_setting("key_burst", Some("300"), 1_000, 1_000)
        .expect("set");
    assert!(
        cache.refresh(&store).expect("refresh"),
        "a setting is a change"
    );
    assert_eq!(cache.settings()["key_burst"].at(1_000), Some("300"));
    store
        .set_setting("key_burst", None, 2_000, 2_000)
        .expect("remove");
    assert!(cache.refresh(&store).expect("refresh"));
    let removed = cache.settings();
    assert_eq!(removed.get("key_burst").and_then(|s| s.at(2_000)), None);
}

// Health events.

#[test]
fn health_events_are_stored_without_what_must_not_be_kept() {
    let admission = admission(&[V1]);
    let processed = run(
        &admission,
        NOW,
        &[
            event(
                1,
                NOW,
                "app_error",
                json!({
                    "kind": "unhandled",
                    "type": "TypeError",
                    "message": "no account for ana@example.com at https://x.example.com/p?a=1",
                    "frames": "app/cart.js|checkout|42|7|1",
                }),
            ),
            event(
                2,
                NOW,
                "http_request",
                json!({
                    "method": "get",
                    "host": "api.example.com",
                    "route": "https://api.example.com/v1/orders/123456?token=abc",
                    "status": 200,
                    "duration": 120,
                }),
            ),
        ],
    );
    assert_eq!(processed.accepted.len(), 2);
    let text = |index: usize, name: &str| -> String {
        processed.accepted[index]
            .strings
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        text(0, "message"),
        "no account for [email] at https://x.example.com/p?[query]"
    );
    assert_eq!(text(0, "frames"), "app/cart.js|checkout|42|7|1");
    assert_eq!(text(1, "route"), "/v1/orders/:id");
}

#[test]
fn a_site_cannot_redefine_a_health_event() {
    let (store, _, _) = world(&[], &[]);
    let site = store.sites().expect("sites")[0].id;
    let refused = store.publish_schema(
        site,
        "[events.app_error.fields.message]\nkind = \"text\"\nmax = 10\n",
    );
    assert!(refused.is_err());
}

// Performance sampling.

fn load(n: u32) -> serde_json::Value {
    event(n, NOW, "screen_load", json!({ "render": 120 }))
}

/// Runs the events in batches of the most a batch holds, and puts the answers
/// together.
fn run_many(admission: &Admission, events: &[serde_json::Value]) -> Processed {
    let mut all = Processed {
        accepted: Vec::new(),
        drops: Vec::new(),
    };
    for chunk in events.chunks(100) {
        let part = run(admission, NOW, chunk);
        all.accepted.extend(part.accepted);
        for drop in part.drops {
            match all
                .drops
                .iter_mut()
                .find(|d| d.event_name == drop.event_name && d.reason == drop.reason)
            {
                Some(found) => found.count += drop.count,
                None => all.drops.push(drop),
            }
        }
    }
    all
}

fn sampled(admission: Admission, ppm: u32) -> Admission {
    Admission {
        sample_ppm: ppm,
        ..admission
    }
}

#[test]
fn a_site_that_keeps_half_of_its_performance_events_keeps_about_half_and_says_so() {
    let admission = sampled(admission(&[V1]), 500_000);
    let loads: Vec<_> = (1..=400).map(load).collect();
    let result = run_many(&admission, &loads);
    let kept = result.accepted.len() as u64;
    let dropped = drop_of(&result, "screen_load", Reason::Sampled);
    assert_eq!(kept + dropped, 400, "every event is kept or counted");
    // Four standard deviations of a binomial count of 400 at one half.
    assert!((160..=240).contains(&kept), "{kept} kept");
    for event in &result.accepted {
        let rate = event
            .numbers
            .iter()
            .find(|(name, _)| name == "sample_rate")
            .map(|(_, value)| *value);
        assert_eq!(rate, Some(0.5), "a kept event says what it stands for");
    }
}

#[test]
fn errors_crashes_and_declared_events_are_never_sampled() {
    // The smallest rate there is: nearly every performance event would go.
    let admission = sampled(admission(&[V1]), 1);
    let events = vec![
        event(
            1,
            NOW,
            "app_error",
            json!({ "kind": "unhandled", "type": "TypeError" }),
        ),
        event(2, NOW, "checkout", json!({ "items": 3 })),
        event(3, NOW, "screen", json!({ "name": "home" })),
    ];
    let result = run(&admission, NOW, &events);
    assert_eq!(result.accepted.len(), 3);
    assert!(
        result
            .accepted
            .iter()
            .all(|e| e.numbers.iter().all(|(name, _)| name != "sample_rate"))
    );
    assert!(result.drops.is_empty());
}

#[test]
fn at_the_full_rate_nothing_is_dropped_and_no_rate_is_written() {
    let admission = admission(&[V1]);
    let loads: Vec<_> = (1..=50).map(load).collect();
    let result = run_many(&admission, &loads);
    assert_eq!(result.accepted.len(), 50);
    assert!(
        result
            .accepted
            .iter()
            .all(|e| e.numbers.iter().all(|(name, _)| name != "sample_rate"))
    );
}

#[test]
fn an_event_is_judged_the_same_when_it_is_sent_again() {
    let admission = sampled(admission(&[V1]), 300_000);
    let loads: Vec<_> = (1..=200).map(load).collect();
    let first = run_many(&admission, &loads);
    let second = run_many(&admission, &loads);
    let ids = |p: &Processed| {
        p.accepted
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&second));
    assert!(!first.accepted.is_empty() && first.accepted.len() < 200);

    // Once the kept ones are remembered, a retry counts them as repeated and
    // the ones that were sampled out are still sampled out, not repeated.
    let deduper = Deduper::new(1_000, 600_000);
    deduper.remember(first.accepted.iter().map(|e| e.event_key), NOW);
    let context = Context {
        deduper: Some(&deduper),
        ..Context::default()
    };
    let mut third = Processed {
        accepted: Vec::new(),
        drops: Vec::new(),
    };
    for chunk in loads.chunks(100) {
        let part = process_with(
            &admission,
            &body(NOW, chunk),
            NOW,
            &Limits::default(),
            &context,
        )
        .expect("a valid batch");
        third.accepted.extend(part.accepted);
        third.drops.extend(part.drops);
    }
    let total = |reason| -> u64 {
        third
            .drops
            .iter()
            .filter(|d| d.event_name == "screen_load" && d.reason == reason)
            .map(|d| d.count)
            .sum()
    };
    assert!(third.accepted.is_empty());
    assert_eq!(total(Reason::Duplicate), first.accepted.len() as u64);
    assert_eq!(total(Reason::Sampled), 200 - first.accepted.len() as u64);
}

#[test]
fn the_reason_is_counted_by_name() {
    assert_eq!(Reason::Sampled.as_str(), "sampled");
}

#[test]
fn a_client_cannot_send_its_own_sample_rate_to_change_what_an_event_weighs() {
    // At the full rate nothing is stored for it, and at a lower one the kept
    // events carry the rate of the site, never the client's.
    let forged = |n: u32| {
        event(
            n,
            NOW,
            "screen_load",
            json!({ "render": 120, "sample_rate": 0.0001 }),
        )
    };
    let events: Vec<_> = (1..=100).map(forged).collect();

    let full = run_many(&admission(&[V1]), &events);
    assert_eq!(full.accepted.len(), 100);
    assert!(
        full.accepted
            .iter()
            .all(|e| e.numbers.iter().all(|(name, _)| name != "sample_rate")),
        "the client's number is not kept"
    );

    let half = run_many(&sampled(admission(&[V1]), 500_000), &events);
    assert!(!half.accepted.is_empty());
    for event in &half.accepted {
        let rates: Vec<f64> = event
            .numbers
            .iter()
            .filter(|(name, _)| name == "sample_rate")
            .map(|(_, value)| *value)
            .collect();
        assert_eq!(rates, [0.5], "exactly the site's rate");
    }
}
