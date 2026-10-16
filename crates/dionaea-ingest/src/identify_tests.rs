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
