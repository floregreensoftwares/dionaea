//! Identify requests through the guard: the link, the traits, and what is
//! removed on the way.

use dionaea_core::RequestError;
use dionaea_meta::{MetaStore, SqliteMeta};
use serde_json::{Value, json};

use super::*;

const NO_TRAITS: &str = "[events.e.fields.f]\nkind = \"id\"\n";
const V1: &str = "[traits.plan]\nkind = \"id\"\n\n[traits.seats]\nkind = \"count\"\nmax = 1000\n";
const V2: &str =
    "[traits.plan]\nkind = \"id\"\n\n[traits.beta]\nkind = \"enum\"\nvalues = [\"yes\", \"no\"]\n";

fn admission(schemas: &[&str]) -> Admission {
    let store = SqliteMeta::open_in_memory().expect("store");
    let organization = store.create_organization("o").expect("organization");
    let site = store.create_site(organization.id, "s").expect("site");
    for schema in schemas {
        store.publish_schema(site.id, schema).expect("schema");
    }
    let key = store.issue_write_key(site.id, "k", &[]).expect("key").key;
    let cache = Cache::new();
    cache.refresh(&store).expect("refresh");
    cache.admit(&key, None).expect("admitted")
}

fn body(traits: Option<Value>) -> Vec<u8> {
    let mut request = json!({
        "sent_at": 1_760_000_000_000_u64,
        "sdk": "t/1",
        "anon_id": "anon1",
        "user_id": "u_42",
    });
    if let Some(traits) = traits {
        request["traits"] = traits;
    }
    serde_json::to_vec(&request).expect("json")
}

fn identify(schemas: &[&str], traits: Option<Value>) -> Identified {
    process_identify(&admission(schemas), &body(traits), &Enrichment::default()).expect("valid")
}

#[test]
fn a_request_without_traits_links_and_stores_none() {
    let result = identify(&[V1], None);
    assert_eq!(
        (
            result.anon_id.as_str(),
            result.user_id.as_str(),
            result.site_id
        ),
        ("anon1", "u_42", 1)
    );
    assert_eq!(result.traits, None);
    assert_eq!((result.stripped, result.bot), (0, false));
    assert!(result.drops.is_empty());
    assert_eq!(
        identify(&[V1], Some(json!({}))).traits,
        None,
        "an empty object is none"
    );
}

#[test]
fn declared_traits_are_kept_split_by_kind_and_the_rest_is_stripped() {
    let result = identify(
        &[V1],
        Some(json!({"plan": "pro", "seats": 5, "email": "a@b.c", "seats_extra": 1})),
    );
    let traits = result.traits.expect("traits");
    assert_eq!(traits.strings, vec![("plan".to_owned(), "pro".to_owned())]);
    assert_eq!(traits.numbers, vec![("seats".to_owned(), 5.0)]);
    assert_eq!(result.stripped, 2, "two undeclared names");
}

#[test]
fn an_invalid_value_is_stripped_without_dropping_the_others() {
    let result = identify(&[V1], Some(json!({"plan": "pro", "seats": 5000})));
    let traits = result.traits.expect("traits");
    assert!(traits.numbers.is_empty());
    assert_eq!(traits.strings.len(), 1);
    assert_eq!(result.stripped, 1);
}

#[test]
fn traits_the_schema_does_not_declare_leave_the_earlier_ones_alone() {
    let result = identify(&[V1], Some(json!({"email": "a@b.c"})));
    assert_eq!(
        result.traits, None,
        "an empty set would erase the user's traits"
    );
    assert_eq!(result.stripped, 1);
    // A site with no traits declared at all.
    let result = identify(&[NO_TRAITS], Some(json!({"plan": "pro"})));
    assert_eq!((result.traits, result.stripped), (None, 1));
}

#[test]
fn the_accepted_version_that_keeps_the_most_traits_wins() {
    let schemas = [V1, V2];
    // seats only in V1, beta only in V2: each keeps two of the three.
    let result = identify(
        &schemas,
        Some(json!({"plan": "pro", "seats": 5, "beta": "yes"})),
    );
    let traits = result.traits.expect("traits");
    assert_eq!(traits.strings.len() + traits.numbers.len(), 2);
    assert_eq!(result.stripped, 1);
    // Only V1 declares seats and a count: it keeps more than V2 does.
    let result = identify(&schemas, Some(json!({"plan": "pro", "seats": 5})));
    let traits = result.traits.expect("traits");
    assert_eq!(traits.numbers, vec![("seats".to_owned(), 5.0)]);
    assert_eq!(result.stripped, 0);
}

#[test]
fn more_traits_than_the_property_limit_refuse_the_request() {
    // The decoder stops reading past the limit, so the guard never sees them.
    let many: serde_json::Map<String, Value> =
        (0..40).map(|n| (format!("t{n}"), json!(1))).collect();
    assert_eq!(
        process_identify(
            &admission(&[V1]),
            &body(Some(Value::Object(many))),
            &Enrichment::default()
        ),
        Err(RequestError::InvalidField("traits"))
    );
}

#[test]
fn a_bot_is_counted_and_nothing_is_kept() {
    let bot = Enrichment {
        bot: true,
        ..Enrichment::default()
    };
    let result = process_identify(&admission(&[V1]), &body(Some(json!({"plan": "pro"}))), &bot)
        .expect("valid");
    assert!(result.bot && result.traits.is_none());
    assert_eq!(result.drops.len(), 1);
    assert_eq!(result.drops[0].reason, Reason::Bot);
}

#[test]
fn a_malformed_request_is_refused_whole() {
    let admission = admission(&[V1]);
    let none = Enrichment::default();
    let wrong_user = json!({"sent_at": 1, "sdk": "t/1", "anon_id": "a", "user_id": "a@b.c"});
    let missing_user = json!({"sent_at": 1, "sdk": "t/1", "anon_id": "a"});
    let bad_traits =
        json!({"sent_at": 1, "sdk": "t/1", "anon_id": "a", "user_id": "u", "traits": [1]});
    for body in [
        b"not json".to_vec(),
        serde_json::to_vec(&wrong_user).unwrap(),
        serde_json::to_vec(&missing_user).unwrap(),
        serde_json::to_vec(&bad_traits).unwrap(),
    ] {
        assert!(
            matches!(
                process_identify(&admission, &body, &none),
                Err(RequestError::NotJson | RequestError::InvalidField(_))
            ),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
}

// Erased users and devices.

fn with_erased(schemas: &[&str]) -> Admission {
    use std::sync::Arc;

    let meta = SqliteMeta::open_in_memory().expect("meta").with_hash_key(
        dionaea_meta::HashKey::from_secret("0123456789abcdef0123456789abcdef").expect("key"),
    );
    let organization = meta.create_organization("o").expect("organization");
    let site = meta.create_site(organization.id, "s").expect("site");
    for schema in schemas {
        meta.publish_schema(site.id, schema).expect("schema");
    }
    let request = meta
        .request_erasure(site.id, "u_gone", "test")
        .expect("request");
    meta.add_erasure_anons(request.id, &["anon_gone".to_owned()])
        .expect("anons");
    let admission = admission(schemas);
    Admission {
        tombstones: {
            let all = dionaea_meta::Tombstones::default();
            all.load_from(&meta).expect("tombstones");
            Arc::new(all)
        },
        ..admission
    }
}

fn identify_for(admission: &Admission, anon: &str, user: &str) -> Identified {
    let body = serde_json::to_vec(&json!({
        "sent_at": 1, "sdk": "t/1", "anon_id": anon, "user_id": user,
        "traits": {"plan": "pro"}
    }))
    .expect("json");
    process_identify(admission, &body, &Enrichment::default()).expect("valid")
}

#[test]
fn an_identify_for_an_erased_user_or_device_stores_nothing() {
    let admission = with_erased(&[V1]);
    for (anon, user) in [("anon_gone", "u_fine"), ("a_new_device", "u_gone")] {
        let result = identify_for(&admission, anon, user);
        assert!(result.erased && !result.bot, "{anon} {user}");
        assert_eq!(result.traits, None);
        assert_eq!(
            result.drops,
            vec![DropCount {
                event_name: IDENTIFY_EVENT.to_owned(),
                reason: Reason::Erased,
                count: 1,
            }]
        );
    }
    let fine = identify_for(&admission, "a_new_device", "u_fine");
    assert!(!fine.erased && fine.traits.is_some());
}

#[test]
fn events_of_an_erased_device_are_dropped_and_counted_the_rest_kept() {
    let admission = with_erased(&[
        "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n",
    ]);
    let event = |n: u32, anon: &str| {
        json!({
            "event_id": format!("01890000-0000-7000-8000-{n:012}"),
            "ts": 1_760_000_000_000_u64 - 1000,
            "name": "checkout",
            "anon_id": anon,
            "props": {"items": 1},
        })
    };
    let body = serde_json::to_vec(&json!({
        "sent_at": 1_760_000_000_000_u64, "sdk": "t/1",
        "events": [event(1, "anon_gone"), event(2, "other"), event(3, "anon_gone")],
    }))
    .expect("json");
    let processed = process(
        &admission,
        &body,
        1_760_000_000_000,
        &crate::Limits::default(),
    )
    .expect("valid");
    assert_eq!(processed.accepted.len(), 1);
    assert_eq!(processed.accepted[0].anon_id, "other");
    assert_eq!(
        processed.drops,
        vec![DropCount {
            event_name: "checkout".to_owned(),
            reason: Reason::Erased,
            count: 2,
        }]
    );
    assert_eq!(Reason::Erased.as_str(), "erased");
}
