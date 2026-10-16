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

fn store() -> SqliteMeta {
    SqliteMeta::open_in_memory().expect("an in-memory database")
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
    assert_eq!(migrations, 1);
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
