use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use dionaea_core::{is_breaking, parse_schema};

use super::*;

const V1: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 100
required = true
"#;

/// V1 plus an optional field: a compatible change.
const V2: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 100
required = true

[events.checkout_started.fields.coupon]
kind = "text"
max = 32
"#;

/// Narrows `items`: a breaking change.
const BREAKING: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 10
required = true
"#;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

fn key() -> HashKey {
    HashKey::from_secret(SECRET).expect("a valid key")
}

fn store() -> SqliteMeta {
    SqliteMeta::open_in_memory()
        .expect("an in-memory database")
        .with_hash_key(key())
}

fn site(store: &SqliteMeta) -> Site {
    let organization = store.create_organization("Acme").expect("organization");
    store.create_site(organization.id, "shop").expect("site")
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("meta.db")
}

#[test]
fn organizations_and_sites_are_unique_by_name() {
    let store = store();
    let acme = store.create_organization("  Acme ").expect("organization");
    assert_eq!(acme.name, "Acme", "names are trimmed");
    assert!(matches!(
        store.create_organization("Acme"),
        Err(MetaError::Conflict(_))
    ));
    let shop = store.create_site(acme.id, "shop").expect("site");
    assert!(matches!(
        store.create_site(acme.id, "shop"),
        Err(MetaError::Conflict(_))
    ));
    let other = store.create_organization("Other").expect("organization");
    let same_name = store
        .create_site(other.id, "shop")
        .expect("same name elsewhere");
    assert_ne!(shop.id, same_name.id);
    assert!(matches!(
        store.create_site(999, "x"),
        Err(MetaError::NotFound("organization"))
    ));
    assert_eq!(store.sites().expect("sites"), vec![shop, same_name]);
    let names: Vec<String> = store
        .organizations()
        .expect("organizations")
        .into_iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(names, ["Acme", "Other"]);
}

#[test]
fn names_are_checked() {
    let store = store();
    for bad in ["", "   ", "tab\there", &"x".repeat(MAX_LABEL_LEN + 1)] {
        assert!(
            matches!(store.create_organization(bad), Err(MetaError::Invalid(_))),
            "{bad:?}"
        );
    }
    store
        .create_organization(&"é".repeat(MAX_LABEL_LEN))
        .expect("the limit counts characters, not bytes");
}

#[test]
fn a_write_key_has_the_documented_shape_and_is_unique() {
    let store = store();
    let site = site(&store);
    let first = store.issue_write_key(site.id, "ios", &[]).expect("key");
    let second = store.issue_write_key(site.id, "ios", &[]).expect("key");
    assert!(is_write_key_shape(&first.key), "{}", first.key);
    assert_eq!(first.key.len(), 3 + 26);
    assert_ne!(first.key, second.key);
    for bad in [
        "",
        "wk_",
        "wk_short",
        "xx_aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "wk_AAAAAAAAAAAAAAAAAAAAAAAAAA",
        "wk_aaaaaaaaaaaaaaaaaaaaaaaaa1",
        "wk_aaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(!is_write_key_shape(bad), "{bad:?}");
    }
    assert!(is_write_key_shape("wk_aaaaaaaaaaaaaaaaaaaaaaaa27"));
}

#[test]
fn origins_are_normalized_checked_and_deduplicated() {
    let store = store();
    let site = site(&store);
    let key = store
        .issue_write_key(
            site.id,
            "web",
            &[
                "https://Example.com".to_owned(),
                "https://example.com".to_owned(),
                "http://localhost:3000".to_owned(),
            ],
        )
        .expect("key");
    assert_eq!(
        key.origins,
        ["http://localhost:3000", "https://example.com"]
    );
    for bad in [
        "example.com",
        "ftp://example.com",
        "https://",
        "https://example.com/path",
        "https://exa mple.com",
        "https://example.com:",
        "https://example.com:0",
        "https://example.com:65536",
        "https://example.com:80a",
        "https://-example.com",
        "https://example.com.",
        "https://user@example.com",
    ] {
        assert!(
            matches!(
                store.issue_write_key(site.id, "web", &[bad.to_owned()]),
                Err(MetaError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    let many: Vec<String> = (0..=MAX_ORIGINS)
        .map(|i| format!("https://h{i}.example"))
        .collect();
    assert!(matches!(
        store.issue_write_key(site.id, "web", &many),
        Err(MetaError::Invalid(_))
    ));
    assert!(matches!(
        store.issue_write_key(999, "web", &[]),
        Err(MetaError::NotFound("site"))
    ));
    assert_eq!(
        store.write_keys(site.id).expect("keys").len(),
        1,
        "failures stored nothing"
    );
}

#[test]
fn a_revoked_key_leaves_the_snapshot_and_revoking_twice_is_fine() {
    let store = store();
    let site = site(&store);
    let live = store.issue_write_key(site.id, "a", &[]).expect("key");
    let dead = store.issue_write_key(site.id, "b", &[]).expect("key");
    store.revoke_write_key(&dead.key).expect("revoke");
    let generation = store.generation().expect("generation");
    store.revoke_write_key(&dead.key).expect("revoke again");
    assert_eq!(
        store.generation().expect("generation"),
        generation,
        "no change, no bump"
    );

    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(
        snapshot.keys,
        [KeyEntry {
            key: live.key.clone(),
            site_id: site.id,
            origins: vec![]
        }]
    );
    let listed = store.write_keys(site.id).expect("keys");
    assert_eq!(listed.len(), 2, "revoked keys stay listed for audit");
    assert!(
        listed
            .iter()
            .any(|k| k.key == dead.key && k.revoked_at.is_some())
    );
    assert!(matches!(
        store.revoke_write_key("wk_aaaaaaaaaaaaaaaaaaaaaaaaaa"),
        Err(MetaError::NotFound("write key"))
    ));
}

#[test]
fn the_first_schema_is_version_one_and_later_ones_count_up() {
    let store = store();
    let site = site(&store);
    let first = store.publish_schema(site.id, V1).expect("publish");
    assert_eq!((first.version, first.changes.len()), (1, 0));
    let second = store.publish_schema(site.id, V2).expect("publish");
    assert_eq!(second.version, 2);
    assert_eq!(second.changes.len(), 1, "one optional field was added");

    let versions = store.schema_versions(site.id).expect("versions");
    assert_eq!(
        versions.iter().map(|v| v.version).collect::<Vec<_>>(),
        [1, 2]
    );
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.schemas.len(), 1);
    assert_eq!(
        snapshot.schemas[0]
            .versions
            .iter()
            .map(|(v, _)| *v)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn a_bad_unchanged_or_breaking_schema_is_refused_and_stores_nothing() {
    let store = store();
    let site = site(&store);
    assert!(matches!(
        store.publish_schema(site.id, "this is not = [valid"),
        Err(MetaError::InvalidSchema(d)) if !d.is_empty()
    ));
    store.publish_schema(site.id, V1).expect("publish");
    let generation = store.generation().expect("generation");
    assert!(matches!(
        store.publish_schema(site.id, V1),
        Err(MetaError::Unchanged)
    ));
    assert!(matches!(
        store.publish_schema(site.id, BREAKING),
        Err(MetaError::Breaking(c)) if !c.is_empty()
    ));
    assert!(matches!(
        store.publish_schema(999, V1),
        Err(MetaError::NotFound("site"))
    ));
    assert_eq!(store.generation().expect("generation"), generation);
    assert_eq!(store.schema_versions(site.id).expect("versions").len(), 1);
}

#[test]
fn diff_schema_reports_without_storing() {
    let store = store();
    let site = site(&store);
    assert_eq!(
        store.diff_schema(site.id, V1).expect("diff").len(),
        1,
        "against nothing, the event is new"
    );
    store.publish_schema(site.id, V1).expect("publish");
    assert!(store.diff_schema(site.id, V1).expect("diff").is_empty());
    assert_eq!(store.diff_schema(site.id, V2).expect("diff").len(), 1);
    assert!(is_breaking(
        &store.diff_schema(site.id, BREAKING).expect("diff")
    ));
    assert_eq!(store.schema_versions(site.id).expect("versions").len(), 1);
}

#[test]
fn retiring_a_version_removes_it_from_the_snapshot_but_never_the_last_one() {
    let store = store();
    let site = site(&store);
    store.publish_schema(site.id, V1).expect("publish");
    assert!(matches!(
        store.retire_schema_version(site.id, 1),
        Err(MetaError::Conflict(_))
    ));
    store.publish_schema(site.id, V2).expect("publish");
    store.retire_schema_version(site.id, 1).expect("retire");
    assert!(matches!(
        store.retire_schema_version(site.id, 1),
        Err(MetaError::Conflict(_))
    ));
    assert!(matches!(
        store.retire_schema_version(site.id, 9),
        Err(MetaError::NotFound("schema version"))
    ));
    assert!(matches!(
        store.retire_schema_version(site.id, 2),
        Err(MetaError::Conflict(_))
    ));
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(
        snapshot.schemas[0]
            .versions
            .iter()
            .map(|(v, _)| *v)
            .collect::<Vec<_>>(),
        [2]
    );
    let versions = store.schema_versions(site.id).expect("versions");
    assert!(versions[0].retired_at.is_some() && versions[1].retired_at.is_none());

    // Versions keep counting up after a retirement, and a new file is
    // compared with the latest active version.
    let third = store
        .publish_schema(
            site.id,
            &format!("{V2}\n[traits.plan]\nkind = \"text\"\nmax = 8\n"),
        )
        .expect("publish");
    assert_eq!(third.version, 3);
}

#[test]
fn every_change_a_cache_can_see_bumps_the_generation_and_nothing_else_does() {
    let store = store();
    let site = site(&store);
    let mut seen = store.generation().expect("generation");
    let mut bumped = |what: &str| {
        let now = store.generation().expect("generation");
        assert!(now > seen, "{what} must bump the generation");
        seen = now;
    };
    let key = store.issue_write_key(site.id, "k", &[]).expect("key");
    bumped("issuing a key");
    store.publish_schema(site.id, V1).expect("publish");
    bumped("publishing");
    store.publish_schema(site.id, V2).expect("publish");
    bumped("publishing again");
    store.retire_schema_version(site.id, 1).expect("retire");
    bumped("retiring");
    store.revoke_write_key(&key.key).expect("revoke");
    bumped("revoking");

    // Reads, organizations and sites are not in a snapshot.
    store.snapshot().expect("snapshot");
    store.write_keys(site.id).expect("keys");
    store.create_organization("Second").expect("organization");
    assert_eq!(store.generation().expect("generation"), seen);
}

#[test]
fn a_snapshot_holds_only_live_keys_and_active_schemas_of_every_site() {
    let store = store();
    let organization = store.create_organization("Acme").expect("organization");
    let a = store.create_site(organization.id, "a").expect("site");
    let b = store.create_site(organization.id, "b").expect("site");
    store
        .create_site(organization.id, "no-schema")
        .expect("site");
    store.publish_schema(b.id, V1).expect("publish");
    store.publish_schema(a.id, V2).expect("publish");
    let web = store
        .issue_write_key(a.id, "web", &["https://a.example".to_owned()])
        .expect("key");
    let app = store.issue_write_key(b.id, "app", &[]).expect("key");

    let snapshot = store.snapshot().expect("snapshot");
    let mut keys = snapshot.keys.clone();
    keys.sort_by(|x, y| x.key.cmp(&y.key));
    let mut expected = vec![
        KeyEntry {
            key: web.key,
            site_id: a.id,
            origins: vec!["https://a.example".to_owned()],
        },
        KeyEntry {
            key: app.key,
            site_id: b.id,
            origins: vec![],
        },
    ];
    expected.sort_by(|x, y| x.key.cmp(&y.key));
    assert_eq!(keys, expected);
    assert_eq!(
        snapshot
            .schemas
            .iter()
            .map(|s| s.site_id)
            .collect::<Vec<_>>(),
        [a.id, b.id],
        "sites without a schema are absent"
    );
    assert_eq!(snapshot.generation, store.generation().expect("generation"));
    assert_eq!(
        parse_schema(V2).expect("schema"),
        snapshot.schemas[0].versions[0].1
    );
}

#[test]
fn timestamps_come_from_the_injected_clock() {
    let tick = Arc::new(AtomicI64::new(1_000));
    let clock = Arc::clone(&tick);
    let store = store().with_clock(move || clock.fetch_add(1, Ordering::Relaxed));
    let site = site(&store);
    let key = store.issue_write_key(site.id, "k", &[]).expect("key");
    assert!(
        key.created_at >= 1_000 && key.created_at < 1_100,
        "{}",
        key.created_at
    );
    store.revoke_write_key(&key.key).expect("revoke");
    let listed = store.write_keys(site.id).expect("keys");
    assert!(listed[0].revoked_at.expect("revoked") > listed[0].created_at);
}

#[test]
fn a_file_database_survives_a_reopen_and_migrates_once() {
    let path = temp_path("reopen");
    let (site_id, key) = {
        let store = SqliteMeta::open(&path).expect("open");
        let site = site(&store);
        store.publish_schema(site.id, V1).expect("publish");
        (
            site.id,
            store.issue_write_key(site.id, "k", &[]).expect("key").key,
        )
    };
    let store = SqliteMeta::open(&path).expect("reopen");
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.keys[0].key, key);
    assert_eq!(snapshot.schemas[0].site_id, site_id);
    store.ping().expect("ping");
    let connection = rusqlite::Connection::open(&path).expect("raw connection");
    let migrations: i64 = connection
        .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        migrations, 10,
        "each migration once, however many times it was opened"
    );
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("mode");
    assert_eq!(mode, "wal");
}

#[test]
fn two_processes_opening_a_new_database_at_once_both_succeed() {
    let path = temp_path("race");
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || SqliteMeta::open(&path).map(|_| ()))
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread").expect("open");
    }
}

#[test]
fn concurrent_writers_do_not_lose_updates() {
    let store = Arc::new(store());
    let site = site(&store);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..10 {
                    store.issue_write_key(site.id, "k", &[]).expect("key");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread");
    }
    assert_eq!(store.write_keys(site.id).expect("keys").len(), 80);
    assert_eq!(store.snapshot().expect("snapshot").keys.len(), 80);
    assert!(store.generation().expect("generation") >= 80);
}

#[test]
fn a_stored_schema_that_no_longer_parses_is_reported_not_skipped() {
    let path = temp_path("corrupt");
    let store = SqliteMeta::open(&path).expect("open");
    let site = site(&store);
    store.publish_schema(site.id, V1).expect("publish");
    rusqlite::Connection::open(&path)
        .expect("raw connection")
        .execute("UPDATE schema_versions SET source = 'not = [toml'", [])
        .expect("tamper");
    assert!(matches!(store.snapshot(), Err(MetaError::Corrupt(_))));
}

#[test]
fn the_trait_object_works() {
    let store: Box<dyn MetaStore> = Box::new(store());
    store.ping().expect("ping");
    assert_eq!(store.generation().expect("generation"), 0);
}

#[test]
fn keys_issued_in_the_same_second_are_listed_in_the_order_they_were_issued() {
    let store = store().with_clock(|| 1_000);
    let site = site(&store);
    let issued: Vec<String> = (0..8)
        .map(|n| {
            store
                .issue_write_key(site.id, &format!("k{n}"), &[])
                .expect("key")
                .key
        })
        .collect();
    let listed: Vec<String> = store
        .write_keys(site.id)
        .expect("keys")
        .into_iter()
        .map(|k| k.key)
        .collect();
    assert_eq!(listed, issued);
}

const NOW: i64 = 1_760_000_000_000;

fn set(store: &SqliteMeta, name: &str, value: &str) {
    store.set_setting(name, Some(value), NOW, NOW).expect("set");
}

fn value_now(store: &SqliteMeta, name: &str) -> Option<String> {
    store
        .settings()
        .expect("settings")
        .get(name)
        .and_then(|s| s.at(NOW + 1))
        .map(str::to_owned)
}

#[test]
fn settings_are_stored_read_replaced_and_removed() {
    let store = store();
    assert!(store.settings().expect("settings").is_empty());
    set(&store, "key_burst", "900");
    set(&store, "max_connections", "50");
    set(&store, "key_burst", "1000");
    assert_eq!(value_now(&store, "key_burst").as_deref(), Some("1000"));
    assert_eq!(value_now(&store, "max_connections").as_deref(), Some("50"));
    store
        .set_setting("key_burst", None, NOW, NOW)
        .expect("remove");
    assert_eq!(value_now(&store, "key_burst"), None);
    assert_eq!(
        store.settings().expect("settings").len(),
        1,
        "a removal that took effect leaves nothing behind"
    );
    store
        .set_setting("never_set", None, NOW, NOW)
        .expect("removing what is not there is fine");
    assert_eq!(store.settings().expect("settings").len(), 1);
}

#[test]
fn a_change_scheduled_for_later_leaves_the_old_value_in_force_until_then() {
    let store = store();
    set(&store, "key_burst", "400");
    let later = NOW + 5_000;
    store
        .set_setting("key_burst", Some("900"), later, NOW)
        .expect("schedule");
    let stored = store.settings().expect("settings")["key_burst"].clone();
    assert_eq!(stored.at(NOW), Some("400"), "before the time");
    assert_eq!(stored.at(later - 1), Some("400"));
    assert_eq!(stored.at(later), Some("900"), "from the time on");
    assert_eq!(stored.at(later + 60_000), Some("900"));

    // A second change before the first took effect replaces it, and what was
    // in force stays the old value.
    store
        .set_setting("key_burst", Some("700"), later + 1_000, NOW + 100)
        .expect("reschedule");
    let stored = store.settings().expect("settings")["key_burst"].clone();
    assert_eq!(stored.at(later), Some("400"));
    assert_eq!(stored.at(later + 1_000), Some("700"));
}

#[test]
fn a_removal_scheduled_for_later_also_waits() {
    let store = store();
    set(&store, "key_burst", "400");
    let later = NOW + 5_000;
    store
        .set_setting("key_burst", None, later, NOW)
        .expect("schedule the removal");
    let stored = store.settings().expect("settings")["key_burst"].clone();
    assert_eq!(stored.at(NOW + 1), Some("400"));
    assert_eq!(stored.at(later), None);
    // The next write after the time has passed tidies the removed row away.
    store
        .set_setting("max_connections", Some("5"), later + 1, later + 1)
        .expect("set");
    assert!(
        !store
            .settings()
            .expect("settings")
            .contains_key("key_burst")
    );
}

#[test]
fn a_setting_moves_the_generation_only_when_it_changes() {
    let store = store();
    let start = store.generation().expect("generation");
    set(&store, "key_burst", "900");
    let after_set = store.generation().expect("generation");
    assert!(after_set > start);
    set(&store, "key_burst", "900");
    assert_eq!(
        store.generation().expect("generation"),
        after_set,
        "nothing changed"
    );
    set(&store, "key_burst", "901");
    let after_change = store.generation().expect("generation");
    assert!(after_change > after_set);
    store
        .set_setting("missing", None, NOW, NOW)
        .expect("remove what is absent");
    assert_eq!(store.generation().expect("generation"), after_change);
    store
        .set_setting("key_burst", Some("901"), NOW + 9_000, NOW)
        .expect("the same value, at another time");
    assert!(
        store.generation().expect("generation") > after_change,
        "a new time is a change nodes must see"
    );
}

#[test]
fn the_snapshot_carries_the_settings_of_its_generation() {
    let store = store();
    set(&store, "address_burst", "7");
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.settings["address_burst"].at(NOW + 1), Some("7"));
    assert_eq!(snapshot.generation, store.generation().expect("generation"));
}

#[test]
fn bad_setting_names_and_values_are_refused() {
    let store = store();
    for name in [
        "",
        "Key",
        "key-burst",
        "key burst",
        "é",
        &"a".repeat(MAX_SETTING_NAME_LEN + 1),
    ] {
        assert!(
            matches!(
                store.set_setting(name, Some("1"), NOW, NOW),
                Err(MetaError::Invalid(_))
            ),
            "{name:?}"
        );
    }
    for value in [
        "",
        "tab\there",
        "new\nline",
        &"9".repeat(MAX_SETTING_VALUE_LEN + 1),
    ] {
        assert!(
            matches!(
                store.set_setting("key_burst", Some(value), NOW, NOW),
                Err(MetaError::Invalid(_))
            ),
            "{value:?}"
        );
    }
    assert!(store.settings().expect("settings").is_empty());
}

#[test]
fn settings_survive_a_reopen() {
    let path = temp_path("settings");
    {
        let store = SqliteMeta::open(&path).expect("open");
        store
            .set_setting("key_burst", Some("123"), NOW + 10_000, NOW)
            .expect("set");
    }
    let store = SqliteMeta::open(&path).expect("reopen");
    let stored = store.settings().expect("settings")["key_burst"].clone();
    assert_eq!(stored.at(NOW + 10_000), Some("123"));
    assert_eq!(stored.effective_at_ms, NOW + 10_000);
}

// Erasure requests and tombstones.

/// The tombstones a node would hold after loading them all.
fn tombstones(store: &SqliteMeta) -> Tombstones {
    let all = Tombstones::default();
    all.load_from(store).unwrap();
    all
}

fn clocked() -> (SqliteMeta, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(1_000));
    let clock = Arc::clone(&now);
    (
        store().with_clock(move || clock.load(Ordering::SeqCst)),
        now,
    )
}

#[test]
fn a_request_is_recorded_and_tombstones_the_user_at_once() {
    let store = store();
    let site = site(&store);
    let before = store.generation().unwrap();
    let request = store.request_erasure(site.id, "u_42", "ops").unwrap();
    assert_eq!(request.state, ErasureState::Requested);
    assert_eq!(request.user_id.as_deref(), Some("u_42"));
    assert_eq!(
        request.user_hash,
        key().tombstone_hash(site.id, IdKind::User, "u_42")
    );
    assert_eq!(request.requested_by, "ops");
    assert!(store.generation().unwrap() > before, "nodes must reload");

    let tombstones = tombstones(&store);
    assert!(tombstones.user_erased(site.id, "u_42"));
    assert!(!tombstones.user_erased(site.id, "u_43"));
    assert!(!tombstones.user_erased(site.id + 1, "u_42"), "per site");
    assert!(
        !tombstones.anon_erased(site.id, "u_42"),
        "a user is not a device"
    );
}

#[test]
fn an_unfinished_request_for_the_same_user_is_returned_not_duplicated() {
    let store = store();
    let site = site(&store);
    let first = store.request_erasure(site.id, "u_42", "ops").unwrap();
    let generation = store.generation().unwrap();
    let second = store
        .request_erasure(site.id, "u_42", "someone else")
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.requested_by, "ops");
    assert_eq!(store.generation().unwrap(), generation);
    assert_eq!(store.erasure_requests(None).unwrap().len(), 1);

    // Once complete, a new request is a new one.
    store
        .advance_erasure(first.id, ErasureState::Hidden, None)
        .unwrap();
    store
        .advance_erasure(first.id, ErasureState::Complete, None)
        .unwrap();
    let again = store.request_erasure(site.id, "u_42", "ops").unwrap();
    assert_ne!(again.id, first.id);
}

#[test]
fn a_request_needs_a_site_a_user_id_shape_and_a_requester() {
    let store = store();
    let site = site(&store);
    assert!(matches!(
        store.request_erasure(site.id + 9, "u_42", "ops"),
        Err(MetaError::NotFound("site"))
    ));
    for bad in ["", "a@b.c", "has space", &"x".repeat(65)] {
        assert!(
            matches!(
                store.request_erasure(site.id, bad, "ops"),
                Err(MetaError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    for bad in ["", "  ", "bad\nname"] {
        assert!(
            matches!(
                store.request_erasure(site.id, "u_42", bad),
                Err(MetaError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(store.erasure_requests(None).unwrap().is_empty());
    assert!(tombstones(&store).is_empty());
}

#[test]
fn anonymous_ids_found_for_the_user_become_tombstones() {
    let store = store();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_42", "ops").unwrap();
    let ids = vec!["a1".to_owned(), "a2".to_owned(), "not valid!".to_owned()];
    let generation = store.generation().unwrap();
    assert_eq!(store.add_erasure_anons(request.id, &ids).unwrap(), 2);
    assert!(store.generation().unwrap() > generation);
    assert_eq!(
        store.add_erasure_anons(request.id, &ids).unwrap(),
        0,
        "again adds none"
    );
    assert_eq!(store.erasure_request(request.id).unwrap().anon_count, 2);

    let tombstones = tombstones(&store);
    assert!(tombstones.anon_erased(site.id, "a1") && tombstones.anon_erased(site.id, "a2"));
    assert!(!tombstones.anon_erased(site.id, "a3"));
    assert!(!tombstones.anon_erased(site.id + 1, "a1"));

    store
        .advance_erasure(request.id, ErasureState::Hidden, None)
        .unwrap();
    assert!(matches!(
        store.add_erasure_anons(request.id, &ids),
        Err(MetaError::Conflict(_))
    ));
}

#[test]
fn a_request_moves_forward_only_and_completing_forgets_the_user_id_but_not_its_hash() {
    let (store, now) = clocked();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_42", "ops").unwrap();
    assert_eq!(request.requested_at, 1_000);
    assert!(matches!(
        store.advance_erasure(request.id, ErasureState::Complete, None),
        Err(MetaError::Conflict(_))
    ));
    now.store(2_000, Ordering::SeqCst);
    let hidden = store
        .advance_erasure(request.id, ErasureState::Hidden, Some("events 3"))
        .unwrap();
    assert_eq!(
        (hidden.state, hidden.hidden_at),
        (ErasureState::Hidden, Some(2_000))
    );
    assert_eq!(
        hidden.user_id.as_deref(),
        Some("u_42"),
        "still needed by the job"
    );
    now.store(3_000, Ordering::SeqCst);
    let done = store
        .advance_erasure(request.id, ErasureState::Complete, Some("verified"))
        .unwrap();
    assert_eq!(done.state, ErasureState::Complete);
    assert_eq!(done.completed_at, Some(3_000));
    assert_eq!(done.hidden_at, Some(2_000));
    assert_eq!(done.user_id, None, "the id is forgotten");
    assert_eq!(done.user_hash, request.user_hash, "the audit hash stays");
    assert_eq!(done.detail.as_deref(), Some("verified"));
    for to in [
        ErasureState::Requested,
        ErasureState::Hidden,
        ErasureState::Failed,
    ] {
        assert!(matches!(
            store.advance_erasure(request.id, to, None),
            Err(MetaError::Conflict(_))
        ));
    }
    assert!(
        tombstones(&store).user_erased(site.id, "u_42"),
        "the tombstone outlives the request"
    );
}

#[test]
fn a_failed_request_can_be_retried_and_only_a_failed_one() {
    let store = store();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_42", "ops").unwrap();
    assert!(matches!(
        store.retry_erasure(request.id),
        Err(MetaError::Conflict(_))
    ));
    let failed = store
        .advance_erasure(request.id, ErasureState::Failed, Some("clickhouse down"))
        .unwrap();
    assert_eq!(failed.detail.as_deref(), Some("clickhouse down"));
    let retried = store.retry_erasure(request.id).unwrap();
    assert_eq!(
        (retried.state, retried.detail),
        (ErasureState::Requested, None)
    );
    assert!(matches!(
        store.retry_erasure(999),
        Err(MetaError::NotFound("erasure request"))
    ));
}

#[test]
fn a_worker_holds_a_request_until_its_lease_runs_out() {
    let (store, now) = clocked();
    let site = site(&store);
    let first = store.request_erasure(site.id, "u_1", "ops").unwrap();
    let second = store.request_erasure(site.id, "u_2", "ops").unwrap();
    assert!(
        store
            .claim_erasure(ErasureState::Hidden, "a", 60, i64::MAX)
            .unwrap()
            .is_none()
    );

    let one = store
        .claim_erasure(ErasureState::Requested, "a", 60, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(one.id, first.id, "oldest first");
    let two = store
        .claim_erasure(ErasureState::Requested, "b", 60, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(two.id, second.id, "the first is held");
    assert!(
        store
            .claim_erasure(ErasureState::Requested, "c", 60, i64::MAX)
            .unwrap()
            .is_none()
    );

    now.store(1_000 + 61, Ordering::SeqCst);
    let again = store
        .claim_erasure(ErasureState::Requested, "c", 60, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(again.id, first.id, "a dead worker lets go");

    // Moving a request on releases it.
    store
        .advance_erasure(first.id, ErasureState::Hidden, None)
        .unwrap();
    let hidden = store
        .claim_erasure(ErasureState::Hidden, "d", 60, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(hidden.id, first.id);
}

#[test]
fn requests_are_listed_by_state_and_a_hash_is_stable_and_not_the_id() {
    let store = store();
    let site = site(&store);
    let a = store.request_erasure(site.id, "u_1", "ops").unwrap();
    let b = store.request_erasure(site.id, "u_2", "ops").unwrap();
    store
        .advance_erasure(b.id, ErasureState::Failed, Some("x"))
        .unwrap();
    let ids = |state| {
        store
            .erasure_requests(state)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(None), vec![a.id, b.id]);
    assert_eq!(ids(Some(ErasureState::Requested)), vec![a.id]);
    assert_eq!(ids(Some(ErasureState::Failed)), vec![b.id]);

    let hash = key().tombstone_hash(1, IdKind::User, "u_1");
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!hash.contains("u_1"));
    assert_eq!(hash, key().tombstone_hash(1, IdKind::User, "u_1"));
    assert_ne!(hash, key().tombstone_hash(2, IdKind::User, "u_1"), "site");
    assert_ne!(hash, key().tombstone_hash(1, IdKind::Anon, "u_1"), "kind");
    assert_ne!(hash, key().tombstone_hash(1, IdKind::User, "u_2"), "id");
}

#[test]
fn tombstones_survive_reopening_the_database() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-erasure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    let site_id = {
        let store = SqliteMeta::open(&path).unwrap().with_hash_key(key());
        let site = site(&store);
        let request = store.request_erasure(site.id, "u_42", "ops").unwrap();
        store
            .add_erasure_anons(request.id, &["a1".to_owned()])
            .unwrap();
        site.id
    };
    let store = SqliteMeta::open(&path).unwrap().with_hash_key(key());
    let tombstones = tombstones(&store);
    assert!(tombstones.user_erased(site_id, "u_42") && tombstones.anon_erased(site_id, "a1"));
    assert_eq!(tombstones.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_request_is_claimed_only_once_it_has_waited_long_enough() {
    let (store, now) = clocked();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
    store
        .advance_erasure(request.id, ErasureState::Hidden, None)
        .unwrap();
    // Hidden at 1_000: not due before that, due from then on.
    assert!(
        store
            .claim_erasure(ErasureState::Hidden, "a", 60, 999)
            .unwrap()
            .is_none()
    );
    now.store(5_000, Ordering::SeqCst);
    assert!(
        store
            .claim_erasure(ErasureState::Hidden, "a", 60, 1_000)
            .unwrap()
            .is_some()
    );
}

#[test]
fn the_devices_are_kept_in_the_clear_only_until_the_request_completes() {
    let store = store();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
    assert!(store.erasure_anons(request.id).unwrap().is_empty());
    store
        .add_erasure_anons(
            request.id,
            &["b".to_owned(), "a".to_owned(), "a".to_owned()],
        )
        .unwrap();
    assert_eq!(store.erasure_anons(request.id).unwrap(), vec!["a", "b"]);
    store
        .advance_erasure(request.id, ErasureState::Hidden, None)
        .unwrap();
    assert_eq!(store.erasure_anons(request.id).unwrap().len(), 2);
    store
        .advance_erasure(request.id, ErasureState::Complete, None)
        .unwrap();
    assert!(store.erasure_anons(request.id).unwrap().is_empty());
    assert!(
        tombstones(&store).anon_erased(site.id, "a"),
        "only the hashes remain, and they keep blocking"
    );
    assert!(matches!(
        store.erasure_anons(999),
        Err(MetaError::NotFound("erasure request"))
    ));
}

// Coordination: one worker at a time, and bounded retries.

#[test]
fn a_lease_belongs_to_one_holder_until_it_runs_out_or_is_let_go() {
    let (store, now) = clocked();
    assert!(store.try_lease("job", "a", 30).unwrap());
    assert!(!store.try_lease("job", "b", 30).unwrap(), "held by a");
    assert!(store.try_lease("job", "a", 30).unwrap(), "renewed by a");
    assert!(
        store.try_lease("other", "b", 30).unwrap(),
        "another lease is another lock"
    );

    now.store(1_000 + 29, Ordering::SeqCst);
    assert!(
        !store.try_lease("job", "b", 30).unwrap(),
        "still within the time"
    );
    now.store(1_000 + 30, Ordering::SeqCst);
    assert!(
        store.try_lease("job", "b", 30).unwrap(),
        "a lets go by silence"
    );
    assert!(!store.try_lease("job", "a", 30).unwrap(), "now b has it");

    store.release_lease("job", "a").unwrap();
    assert!(
        !store.try_lease("job", "a", 30).unwrap(),
        "a was not the holder"
    );
    store.release_lease("job", "b").unwrap();
    assert!(store.try_lease("job", "a", 30).unwrap(), "let go at once");
}

#[test]
fn trouble_delays_a_request_and_after_enough_of_it_the_request_fails() {
    let (store, now) = clocked();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();

    let first = store
        .record_erasure_trouble(request.id, "clickhouse is down", 60, 3)
        .unwrap();
    assert_eq!(first.state, ErasureState::Requested);
    assert_eq!((first.attempts, first.retry_after), (1, Some(1_060)));
    assert!(first.detail.unwrap().contains("attempt 1 of 3"));
    assert!(
        store
            .claim_erasure(ErasureState::Requested, "w", 60, i64::MAX)
            .unwrap()
            .is_none(),
        "not before the retry time"
    );

    now.store(1_060, Ordering::SeqCst);
    let claimed = store
        .claim_erasure(ErasureState::Requested, "w", 60, i64::MAX)
        .unwrap()
        .expect("due again");
    assert_eq!(claimed.attempts, 1);
    let second = store
        .record_erasure_trouble(request.id, "still down", 120, 3)
        .unwrap();
    assert_eq!((second.attempts, second.retry_after), (2, Some(1_180)));
    assert_eq!(second.state, ErasureState::Requested);

    now.store(1_180, Ordering::SeqCst);
    let third = store
        .record_erasure_trouble(request.id, "down for good", 240, 3)
        .unwrap();
    assert_eq!(third.state, ErasureState::Failed, "the limit was reached");
    assert_eq!(third.retry_after, None);
    let detail = third.detail.unwrap();
    assert!(
        detail.contains("gave up after 3 attempts") && detail.contains("down for good"),
        "{detail}"
    );
    assert!(matches!(
        store.record_erasure_trouble(request.id, "x", 1, 3),
        Err(MetaError::Conflict(_))
    ));
}

#[test]
fn progress_or_a_retry_starts_the_count_again() {
    let (store, _) = clocked();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
    store
        .record_erasure_trouble(request.id, "down", 0, 5)
        .unwrap();
    let hidden = store
        .advance_erasure(request.id, ErasureState::Hidden, None)
        .unwrap();
    assert_eq!((hidden.attempts, hidden.retry_after), (0, None));

    store
        .record_erasure_trouble(request.id, "down", 0, 2)
        .unwrap();
    let failed = store
        .record_erasure_trouble(request.id, "down", 0, 2)
        .unwrap();
    assert_eq!(failed.state, ErasureState::Failed);
    let again = store.retry_erasure(request.id).unwrap();
    assert_eq!((again.attempts, again.state), (0, ErasureState::Requested));
}

#[test]
fn a_limit_of_one_fails_at_the_first_trouble_and_zero_counts_as_one() {
    let store = store();
    let site = site(&store);
    let a = store.request_erasure(site.id, "u_1", "ops").unwrap();
    let b = store.request_erasure(site.id, "u_2", "ops").unwrap();
    assert_eq!(
        store.record_erasure_trouble(a.id, "x", 1, 1).unwrap().state,
        ErasureState::Failed
    );
    assert_eq!(
        store.record_erasure_trouble(b.id, "x", 1, 0).unwrap().state,
        ErasureState::Failed
    );
}

#[test]
fn the_summary_counts_states_waiting_retries_and_the_oldest_open_request() {
    let (store, now) = clocked();
    let site = site(&store);
    assert_eq!(store.erasure_summary().unwrap(), ErasureSummary::default());

    let a = store.request_erasure(site.id, "u_1", "ops").unwrap();
    now.store(2_000, Ordering::SeqCst);
    let b = store.request_erasure(site.id, "u_2", "ops").unwrap();
    let c = store.request_erasure(site.id, "u_3", "ops").unwrap();
    store
        .advance_erasure(b.id, ErasureState::Hidden, None)
        .unwrap();
    store
        .advance_erasure(c.id, ErasureState::Failed, Some("x"))
        .unwrap();
    store.record_erasure_trouble(a.id, "down", 100, 5).unwrap();

    let summary = store.erasure_summary().unwrap();
    assert_eq!(
        (
            summary.requested,
            summary.hidden,
            summary.complete,
            summary.failed
        ),
        (1, 1, 0, 1)
    );
    assert_eq!(summary.retrying, 1, "a waits for its time");
    assert_eq!(summary.oldest_open_at, Some(1_000));

    now.store(2_200, Ordering::SeqCst);
    assert_eq!(
        store.erasure_summary().unwrap().retrying,
        0,
        "the time came"
    );
}

// Loading tombstones as they come.

#[test]
fn a_node_asks_only_for_the_tombstones_made_since_it_last_looked() {
    let store = store();
    let site = site(&store);
    let held = Tombstones::default();
    assert_eq!(held.load_from(&store).unwrap(), 0);
    assert_eq!(held.last_seen(), 0);

    let first = store.request_erasure(site.id, "u_1", "ops").unwrap();
    assert_eq!(held.load_from(&store).unwrap(), 1);
    let seen = held.last_seen();
    assert!(seen > 0);
    assert!(held.user_erased(site.id, "u_1"));

    // Nothing new: nothing is read again.
    assert_eq!(held.load_from(&store).unwrap(), 0);
    assert_eq!(held.last_seen(), seen);
    let batch = store.tombstones_since(seen, 100).unwrap();
    assert!(batch.entries.is_empty());
    assert_eq!(batch.last, seen, "the position asked from comes back");

    store
        .add_erasure_anons(first.id, &["a1".to_owned(), "a2".to_owned()])
        .unwrap();
    store.request_erasure(site.id, "u_2", "ops").unwrap();
    let batch = store.tombstones_since(seen, 100).unwrap();
    assert_eq!(batch.entries.len(), 3, "only what came after");
    assert_eq!(held.load_from(&store).unwrap(), 3);
    assert_eq!(held.len(), 4);
    assert!(held.anon_erased(site.id, "a2") && held.user_erased(site.id, "u_2"));
}

#[test]
fn many_tombstones_are_read_in_batches_and_none_is_lost_or_counted_twice() {
    let store = store();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
    let anons: Vec<String> = (0..25_000).map(|n| format!("anon_{n}")).collect();
    store.add_erasure_anons(request.id, &anons).unwrap();

    let held = Tombstones::default();
    assert_eq!(held.load_from(&store).unwrap(), 25_001);
    assert_eq!(held.len(), 25_001);
    assert!(held.anon_erased(site.id, "anon_0"));
    assert!(held.anon_erased(site.id, "anon_24999"));
    assert!(!held.anon_erased(site.id, "anon_25000"));

    // A second node loading while the first keeps going gets the same set.
    let other = Tombstones::default();
    other.load_from(&store).unwrap();
    other.load_from(&store).unwrap();
    assert_eq!(other.len(), held.len());
    let page = store.tombstones_since(0, 10).unwrap();
    assert_eq!(page.entries.len(), 10);
    let next = store.tombstones_since(page.last, 10).unwrap();
    assert_eq!(next.entries.len(), 10);
    assert_ne!(page.entries[0], next.entries[0]);
}

#[test]
fn the_memory_kept_for_a_tombstone_is_a_short_digest_of_its_hash() {
    let store = store();
    let site = site(&store);
    store.request_erasure(site.id, "u_1", "ops").unwrap();
    let held = Tombstones::default();
    held.load_from(&store).unwrap();
    // The answer is the same as from the full hash, for the same identifier
    // and not for another.
    assert!(held.user_erased(site.id, "u_1"));
    assert!(!held.user_erased(site.id, "u_2"));
    assert!(!held.user_erased(site.id + 1, "u_1"));
    assert!(!held.anon_erased(site.id, "u_1"));
    let empty = Tombstones::default();
    assert!(
        !empty.user_erased(site.id, "u_1"),
        "nothing loaded, nothing erased"
    );
}

// The key that keys the tombstone hashes.

#[test]
fn hmac_sha256_matches_the_published_test_vectors() {
    use crate::erasure::hmac_sha256;
    let hex = |bytes: [u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    // RFC 4231, test case 1.
    assert_eq!(
        hex(hmac_sha256(&[0x0b; 20], &[b"Hi There"])),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    // Test case 2, and the message given in two parts.
    assert_eq!(
        hex(hmac_sha256(
            b"Jefe",
            &[b"what do ya want ", b"for nothing?"]
        )),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // Test case 6: a key longer than the block.
    assert_eq!(
        hex(hmac_sha256(
            &[0xaa; 131],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn a_key_must_be_long_enough_and_plain_text() {
    assert!(HashKey::from_secret(SECRET).is_ok());
    for bad in [
        "",
        "short",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef 0123456789abcdef",
        "0123456789abcdef\n0123456789abcdef",
    ] {
        assert!(HashKey::from_secret(bad).is_err(), "{bad:?}");
    }
    assert!(HashKey::from_secret(&"k".repeat(MAX_KEY_LEN + 1)).is_err());
    assert!(HashKey::from_secret(&"k".repeat(MAX_KEY_LEN)).is_ok());
    let shown = format!("{:?}", key());
    assert!(!shown.contains(SECRET) && !shown.contains('['), "{shown}");
}

#[test]
fn the_same_identifier_hashes_differently_under_another_key() {
    let other = HashKey::from_secret("fedcba9876543210fedcba9876543210").unwrap();
    let a = key().tombstone_hash(1, IdKind::User, "u_1");
    let b = other.tombstone_hash(1, IdKind::User, "u_1");
    assert_ne!(a, b);
    assert_eq!(a, key().tombstone_hash(1, IdKind::User, "u_1"));
    assert_ne!(key().check(), other.check());
    assert!(!key().check().contains(SECRET));
}

#[test]
fn without_a_key_no_erasure_can_be_requested_and_the_message_says_what_to_do() {
    let store = SqliteMeta::open_in_memory().unwrap();
    let site = site(&store);
    let error = store.request_erasure(site.id, "u_1", "ops").unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("erasure_key") && message.contains("32"),
        "{message}"
    );
    assert!(store.erasure_requests(None).unwrap().is_empty());
    // Reading tombstones is fine while there are none.
    assert_eq!(Tombstones::default().load_from(&store).unwrap(), 0);
}

fn reopened(path: &std::path::Path, secret: Option<&str>) -> SqliteMeta {
    let store = SqliteMeta::open(path).unwrap();
    match secret {
        Some(secret) => store.with_hash_key(HashKey::from_secret(secret).unwrap()),
        None => store,
    }
}

#[test]
fn a_server_with_another_key_or_none_refuses_to_read_or_add_tombstones() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    let site_id = {
        let store = reopened(&path, Some(SECRET));
        let site = site(&store);
        store.request_erasure(site.id, "u_1", "ops").unwrap();
        site.id
    };

    // The right key reads them.
    let held = Tombstones::default();
    held.load_from(&reopened(&path, Some(SECRET))).unwrap();
    assert!(held.user_erased(site_id, "u_1"));

    // Another key, or none, neither reads nor adds: it must not run as if
    // nothing had been erased.
    for secret in [Some("fedcba9876543210fedcba9876543210"), None] {
        let store = reopened(&path, secret);
        let error = Tombstones::default().load_from(&store).unwrap_err();
        assert!(matches!(error, MetaError::Invalid(_)), "{error}");
        assert!(error.to_string().contains("key"), "{error}");
        assert!(store.request_erasure(site_id, "u_2", "ops").is_err());
        assert!(store.add_erasure_anons(1, &["a".to_owned()]).is_err());
    }
    let wrong = reopened(&path, Some("fedcba9876543210fedcba9876543210"));
    assert!(
        wrong
            .request_erasure(site_id, "u_2", "ops")
            .unwrap_err()
            .to_string()
            .contains("not configured"),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tombstone_made_under_one_key_is_not_found_with_the_hash_of_another() {
    let store = store();
    let site = site(&store);
    store.request_erasure(site.id, "u_1", "ops").unwrap();
    let batch = store.tombstones_since(0, 10).unwrap();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(
        batch.entries[0].2,
        key().tombstone_hash(site.id, IdKind::User, "u_1")
    );
    let other = HashKey::from_secret("fedcba9876543210fedcba9876543210").unwrap();
    assert_ne!(
        batch.entries[0].2,
        other.tombstone_hash(site.id, IdKind::User, "u_1"),
        "what the file holds is of no use without the key"
    );
}

// Forgetting what a failed request keeps in the clear.

#[test]
fn a_request_that_failed_long_ago_forgets_the_user_and_the_devices_but_keeps_blocking_them() {
    let (store, now) = clocked();
    let site = site(&store);
    let old = store.request_erasure(site.id, "u_old", "ops").unwrap();
    store
        .add_erasure_anons(old.id, &["dev_old".to_owned()])
        .unwrap();
    let failed = store
        .advance_erasure(old.id, ErasureState::Failed, Some("clickhouse said no"))
        .unwrap();
    assert_eq!(failed.failed_at, Some(1_000));

    now.store(5_000, Ordering::SeqCst);
    let recent = store.request_erasure(site.id, "u_recent", "ops").unwrap();
    store
        .advance_erasure(recent.id, ErasureState::Failed, Some("also"))
        .unwrap();
    let waiting = store.request_erasure(site.id, "u_waiting", "ops").unwrap();

    // Only what failed at or before the cutoff.
    assert_eq!(store.purge_failed_erasures(4_999).unwrap(), 1);
    let purged = store.erasure_request(old.id).unwrap();
    assert_eq!(purged.user_id, None);
    assert!(store.erasure_anons(old.id).unwrap().is_empty());
    let detail = purged.detail.unwrap();
    assert!(
        detail.contains("clickhouse said no") && detail.contains("ask again"),
        "{detail}"
    );
    assert_eq!(purged.user_hash, failed.user_hash, "the audit hash stays");
    assert_eq!(
        store.erasure_request(recent.id).unwrap().user_id.as_deref(),
        Some("u_recent")
    );
    assert_eq!(
        store
            .erasure_request(waiting.id)
            .unwrap()
            .user_id
            .as_deref(),
        Some("u_waiting"),
        "an unfinished request is never touched"
    );
    assert_eq!(store.purge_failed_erasures(4_999).unwrap(), 0, "once");

    // Still blocked.
    let blocked = tombstones(&store);
    assert!(blocked.user_erased(site.id, "u_old"));
    assert!(blocked.anon_erased(site.id, "dev_old"));

    now.store(9_000, Ordering::SeqCst);
    assert_eq!(store.purge_failed_erasures(8_000).unwrap(), 1);
    assert_eq!(
        store.erasure_request(recent.id).unwrap().user_id,
        None,
        "later, the recent one too"
    );
}

#[test]
fn a_purged_request_cannot_be_retried_but_asking_again_works() {
    let (store, _) = clocked();
    let site = site(&store);
    let old = store.request_erasure(site.id, "u_1", "ops").unwrap();
    store
        .advance_erasure(old.id, ErasureState::Failed, None)
        .unwrap();
    assert_eq!(store.purge_failed_erasures(1_000).unwrap(), 1);

    let error = store.retry_erasure(old.id).unwrap_err();
    assert!(
        matches!(error, MetaError::Conflict(_))
            && error.to_string().contains("ask for the erasure again"),
        "{error}"
    );
    let again = store.request_erasure(site.id, "u_1", "ops").unwrap();
    assert_ne!(again.id, old.id, "a new request");
    assert_eq!(again.user_id.as_deref(), Some("u_1"));
    assert_eq!(again.state, ErasureState::Requested);
}

#[test]
fn giving_up_and_retrying_set_and_clear_the_time_of_failure() {
    let (store, now) = clocked();
    let site = site(&store);
    let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
    now.store(2_000, Ordering::SeqCst);
    let failed = store
        .record_erasure_trouble(request.id, "down", 0, 1)
        .unwrap();
    assert_eq!(
        (failed.state, failed.failed_at),
        (ErasureState::Failed, Some(2_000))
    );
    let again = store.retry_erasure(request.id).unwrap();
    assert_eq!(again.failed_at, None);
    assert_eq!(
        store.purge_failed_erasures(i64::MAX).unwrap(),
        0,
        "not failed any more"
    );
}

// Replacing the key.

const NEW_SECRET: &str = "fedcba9876543210fedcba9876543210";

fn new_key() -> HashKey {
    HashKey::from_secret(NEW_SECRET).unwrap()
}

#[test]
fn after_the_key_is_replaced_old_tombstones_are_still_found_and_new_ones_use_the_new_key() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    let site_id = {
        let store = reopened(&path, Some(SECRET));
        let site = site(&store);
        store.request_erasure(site.id, "u_old", "ops").unwrap();
        site.id
    };

    // The new key is current and the old one is kept as a previous key.
    let rotated = SqliteMeta::open(&path)
        .unwrap()
        .with_hash_key(new_key())
        .with_previous_keys(vec![key()]);
    let held = Tombstones::default();
    held.load_from(&rotated).unwrap();
    assert!(held.user_erased(site_id, "u_old"), "made under the old key");

    let request = rotated.request_erasure(site_id, "u_new", "ops").unwrap();
    rotated
        .add_erasure_anons(request.id, &["dev_new".to_owned()])
        .unwrap();
    held.load_from(&rotated).unwrap();
    assert!(held.user_erased(site_id, "u_new") && held.anon_erased(site_id, "dev_new"));
    assert!(!held.user_erased(site_id, "u_other"));
    assert_eq!(
        request.user_hash,
        new_key().tombstone_hash(site_id, IdKind::User, "u_new"),
        "new tombstones use the new key"
    );

    // Everything is found by a server that has both, and the old key alone
    // no longer understands the new tombstones.
    let both = Tombstones::default();
    both.load_from(&rotated).unwrap();
    assert_eq!(both.len(), 3);
    let old_only = reopened(&path, Some(SECRET));
    let error = Tombstones::default().load_from(&old_only).unwrap_err();
    assert!(error.to_string().contains("not configured"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_a_key_that_tombstones_were_made_with_is_refused_and_says_which() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-drop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    let site_id = {
        let store = reopened(&path, Some(SECRET));
        let site = site(&store);
        store.request_erasure(site.id, "u_old", "ops").unwrap();
        site.id
    };
    // A new current key without the old one anywhere.
    let lonely = SqliteMeta::open(&path).unwrap().with_hash_key(new_key());
    let error = Tombstones::default().load_from(&lonely).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("not configured") && message.contains("erasure_previous_keys"),
        "{message}"
    );
    assert!(
        message.contains(&key().check()[..8]),
        "names it by its check: {message}"
    );
    assert!(!message.contains(SECRET));
    assert!(lonely.request_erasure(site_id, "u_new", "ops").is_err());
    assert!(lonely.add_erasure_anons(1, &["a".to_owned()]).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_previous_key_without_a_current_one_does_nothing() {
    let store = SqliteMeta::open_in_memory()
        .unwrap()
        .with_previous_keys(vec![key()]);
    assert!(store.erasure_keys().current().is_none());
    assert!(store.request_erasure(1, "u_1", "ops").is_err());
}

#[test]
fn every_configured_key_is_tried_on_a_lookup_and_a_wrong_one_finds_nothing() {
    let store = store();
    let site = site(&store);
    store.request_erasure(site.id, "u_1", "ops").unwrap();
    let batch = store.tombstones_since(0, 10).unwrap();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(
        store.erasure_keys().all().count(),
        1,
        "only the current key here"
    );
    let keys = HashKeys::new(new_key(), vec![key()]);
    assert_eq!(keys.all().count(), 2);
    assert_eq!(keys.current(), Some(&new_key()));
    assert!(keys.knows(&key().check()) && keys.knows(&new_key().check()));
    assert!(!keys.knows("0000"));
}

// Erasing one device.

#[test]
fn a_device_request_blocks_the_device_at_once_and_not_a_user() {
    let store = store();
    let site = site(&store);
    let request = store
        .request_device_erasure(site.id, "dev_9", "ops")
        .unwrap();
    assert_eq!(request.state, ErasureState::Requested);
    assert_eq!(request.user_id, None);
    assert_eq!(request.device_id.as_deref(), Some("dev_9"));
    assert_eq!(request.anon_count, 1);
    assert_eq!(
        request.user_hash,
        key().tombstone_hash(site.id, IdKind::Anon, "dev_9"),
        "the audit hash is the device's"
    );
    assert_eq!(store.erasure_anons(request.id).unwrap(), vec!["dev_9"]);

    let blocked = tombstones(&store);
    assert!(blocked.anon_erased(site.id, "dev_9"));
    assert!(!blocked.anon_erased(site.id + 1, "dev_9"), "per site");
    assert!(
        !blocked.user_erased(site.id, "dev_9"),
        "a device is not a user"
    );
    assert_eq!(blocked.len(), 1);
}

#[test]
fn a_device_request_is_not_duplicated_checked_and_independent_of_a_user_with_the_same_name() {
    let store = store();
    let site = site(&store);
    let first = store
        .request_device_erasure(site.id, "same", "ops")
        .unwrap();
    let again = store
        .request_device_erasure(site.id, "same", "someone else")
        .unwrap();
    assert_eq!(first.id, again.id);
    let user = store.request_erasure(site.id, "same", "ops").unwrap();
    assert_ne!(user.id, first.id, "a user called `same` is another subject");
    assert_eq!(user.user_id.as_deref(), Some("same"));
    assert_eq!(user.device_id, None);

    for bad in ["", "a b", "a@b.c", &"x".repeat(65)] {
        assert!(
            matches!(
                store.request_device_erasure(site.id, bad, "ops"),
                Err(MetaError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        store.request_device_erasure(site.id + 9, "d", "ops"),
        Err(MetaError::NotFound("site"))
    ));
}

#[test]
fn completing_or_purging_a_device_request_forgets_the_device_but_keeps_blocking_it() {
    let (store, _) = clocked();
    let site = site(&store);
    let done = store
        .request_device_erasure(site.id, "dev_a", "ops")
        .unwrap();
    store
        .advance_erasure(done.id, ErasureState::Hidden, None)
        .unwrap();
    let complete = store
        .advance_erasure(done.id, ErasureState::Complete, None)
        .unwrap();
    assert_eq!(complete.device_id, None);
    assert!(store.erasure_anons(done.id).unwrap().is_empty());

    let failed = store
        .request_device_erasure(site.id, "dev_b", "ops")
        .unwrap();
    store
        .advance_erasure(failed.id, ErasureState::Failed, Some("no"))
        .unwrap();
    assert_eq!(store.purge_failed_erasures(1_000).unwrap(), 1);
    let purged = store.erasure_request(failed.id).unwrap();
    assert_eq!(purged.device_id, None);
    assert!(matches!(
        store.retry_erasure(failed.id),
        Err(MetaError::Conflict(_))
    ));

    let blocked = tombstones(&store);
    assert!(blocked.anon_erased(site.id, "dev_a") && blocked.anon_erased(site.id, "dev_b"));
}

#[test]
fn a_failed_device_request_can_be_retried_while_the_device_is_remembered() {
    let store = store();
    let site = site(&store);
    let request = store
        .request_device_erasure(site.id, "dev_c", "ops")
        .unwrap();
    store
        .advance_erasure(request.id, ErasureState::Failed, Some("no"))
        .unwrap();
    let retried = store.retry_erasure(request.id).unwrap();
    assert_eq!(retried.state, ErasureState::Requested);
    assert_eq!(retried.device_id.as_deref(), Some("dev_c"));
}

// Fixes found in review.

#[test]
fn a_device_request_still_says_it_is_for_a_device_after_it_completes_or_is_purged() {
    let (store, _) = clocked();
    let site = site(&store);
    let done = store
        .request_device_erasure(site.id, "dev_a", "ops")
        .unwrap();
    assert!(done.is_device);
    store
        .advance_erasure(done.id, ErasureState::Hidden, None)
        .unwrap();
    let complete = store
        .advance_erasure(done.id, ErasureState::Complete, None)
        .unwrap();
    assert!(complete.is_device && complete.device_id.is_none());

    let failed = store
        .request_device_erasure(site.id, "dev_b", "ops")
        .unwrap();
    store
        .advance_erasure(failed.id, ErasureState::Failed, None)
        .unwrap();
    store.purge_failed_erasures(i64::MAX).unwrap();
    assert!(store.erasure_request(failed.id).unwrap().is_device);

    let user = store.request_erasure(site.id, "u_1", "ops").unwrap();
    assert!(!user.is_device);
    assert!(!user.names_nobody() && complete.names_nobody());
}

#[test]
fn asking_again_after_the_key_was_replaced_finds_the_request_made_under_the_old_key() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-reask-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    let (site_id, first) = {
        let store = reopened(&path, Some(SECRET));
        let site = site(&store);
        let first = store.request_erasure(site.id, "u_1", "ops").unwrap();
        let device = store
            .request_device_erasure(site.id, "dev_1", "ops")
            .unwrap();
        (site.id, (first.id, device.id))
    };
    let rotated = SqliteMeta::open(&path)
        .unwrap()
        .with_hash_key(new_key())
        .with_previous_keys(vec![key()]);
    assert_eq!(
        rotated.request_erasure(site_id, "u_1", "x").unwrap().id,
        first.0
    );
    assert_eq!(
        rotated
            .request_device_erasure(site_id, "dev_1", "x")
            .unwrap()
            .id,
        first.1
    );
    assert_eq!(
        rotated.erasure_requests(None).unwrap().len(),
        2,
        "no duplicates"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_keys_in_use_are_listed_apart_so_that_checking_them_reads_no_tombstone() {
    let dir = std::env::temp_dir().join(format!("dionaea-meta-keys-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("meta.db");
    {
        let store = reopened(&path, Some(SECRET));
        let site = site(&store);
        let request = store.request_erasure(site.id, "u_1", "ops").unwrap();
        let devices: Vec<String> = (0..50).map(|n| format!("d{n}")).collect();
        store.add_erasure_anons(request.id, &devices).unwrap();
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let count = |table: &str| -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(
        (count("erasure_keys"), count("erasure_tombstones")),
        (1, 51),
        "one key however many tombstones"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Retention and scheduled runs.

#[test]
fn a_site_can_keep_its_data_for_its_own_number_of_days_and_go_back_to_the_default() {
    let store = store();
    let first = site(&store);
    let organization = store.organizations().unwrap()[0].id;
    let second = store.create_site(organization, "Other").unwrap();
    assert!(store.retentions().unwrap().is_empty(), "all on the default");

    store
        .set_retention(first.id, RetentionFamily::Analytics, Some(90))
        .unwrap();
    store
        .set_retention(second.id, RetentionFamily::Analytics, Some(3650))
        .unwrap();
    assert_eq!(
        store.retentions().unwrap(),
        vec![
            Retention {
                site_id: first.id,
                family: RetentionFamily::Analytics,
                days: 90
            },
            Retention {
                site_id: second.id,
                family: RetentionFamily::Analytics,
                days: 3650
            },
        ]
    );
    store
        .set_retention(first.id, RetentionFamily::Analytics, Some(30))
        .unwrap();
    assert_eq!(
        store.retentions().unwrap()[0].days,
        30,
        "replaced, not added"
    );
    store
        .set_retention(first.id, RetentionFamily::Analytics, None)
        .unwrap();
    assert_eq!(store.retentions().unwrap().len(), 1);
    store
        .set_retention(first.id, RetentionFamily::Analytics, None)
        .unwrap();
}

#[test]
fn retention_is_bounded_and_needs_a_site() {
    let store = store();
    let site = site(&store);
    for days in [0, 3651, u32::MAX] {
        assert!(
            matches!(
                store.set_retention(site.id, RetentionFamily::Analytics, Some(days)),
                Err(MetaError::Invalid(_))
            ),
            "{days}"
        );
    }
    for days in [MIN_RETENTION_DAYS, MAX_RETENTION_DAYS] {
        store
            .set_retention(site.id, RetentionFamily::Analytics, Some(days))
            .unwrap();
    }
    assert!(matches!(
        store.set_retention(site.id + 9, RetentionFamily::Analytics, Some(10)),
        Err(MetaError::NotFound("site"))
    ));
    assert_eq!(
        RetentionFamily::parse("analytics"),
        Some(RetentionFamily::Analytics)
    );
    assert_eq!(RetentionFamily::parse("health"), None);
}

#[test]
fn a_scheduled_job_is_claimed_by_one_server_once_per_interval() {
    let (store, now) = clocked();
    assert_eq!(store.last_run("retention").unwrap(), None);
    assert!(store.claim_run("retention", 100).unwrap(), "never ran: due");
    assert_eq!(store.last_run("retention").unwrap(), Some(1_000));
    assert!(!store.claim_run("retention", 100).unwrap(), "taken");
    assert!(store.claim_run("other", 100).unwrap(), "another job");

    now.store(1_099, Ordering::SeqCst);
    assert!(!store.claim_run("retention", 100).unwrap());
    now.store(1_100, Ordering::SeqCst);
    assert!(
        store.claim_run("retention", 100).unwrap(),
        "an interval later"
    );
    assert_eq!(store.last_run("retention").unwrap(), Some(1_100));
}

#[test]
fn a_failed_run_comes_due_again_after_the_retry_time_not_a_whole_interval() {
    let (store, now) = clocked();
    assert!(store.claim_run("retention", 1_000).unwrap());
    store.postpone_run("retention", 1_000, 60).unwrap();
    now.store(1_059, Ordering::SeqCst);
    assert!(!store.claim_run("retention", 1_000).unwrap());
    now.store(1_060, Ordering::SeqCst);
    assert!(store.claim_run("retention", 1_000).unwrap());
    // Postponing a job that never ran just records it.
    store.postpone_run("new", 1_000, 5).unwrap();
    assert!(!store.claim_run("new", 1_000).unwrap());
}
