use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::ErasureState;

const PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aGFzaGhhc2hoYXNoaGFzaA";
const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn store_at(time: &Arc<AtomicI64>) -> SqliteMeta {
    let time = Arc::clone(time);
    SqliteMeta::open_in_memory()
        .expect("an in-memory database")
        .with_clock(move || time.load(Ordering::Relaxed))
}

fn clock() -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(1_000_000))
}

fn member(store: &SqliteMeta, email: &str) -> Member {
    store.create_member(email, "Ana", PHC).expect("member")
}

#[test]
fn a_member_is_stored_with_a_lowercase_unique_address() {
    let store = store_at(&clock());
    assert!(!store.has_members().expect("has"));
    let ana = member(&store, "  Ana@Example.com ");
    assert_eq!(ana.email, "ana@example.com");
    assert!(store.has_members().expect("has"));
    assert!(matches!(
        store.create_member("ANA@example.com", "Other", PHC),
        Err(MetaError::Conflict(_))
    ));
    let (found, hash) = store
        .member_for_login("ANA@EXAMPLE.COM")
        .expect("lookup")
        .expect("found");
    assert_eq!((found.id, hash.as_str()), (ana.id, PHC));
    assert!(
        store
            .member_for_login("nobody@example.com")
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn bad_addresses_names_and_unhashed_passwords_are_refused() {
    let store = store_at(&clock());
    for email in [
        "",
        "no-at",
        "@example.com",
        "a@b",
        "a b@example.com",
        "a@@example.com",
    ] {
        assert!(
            matches!(
                store.create_member(email, "Ana", PHC),
                Err(MetaError::Invalid(_))
            ),
            "{email:?}"
        );
    }
    let long = format!("{}@example.com", "a".repeat(250));
    assert!(matches!(
        store.create_member(&long, "Ana", PHC),
        Err(MetaError::Invalid(_))
    ));
    for name in ["", "  ", "a\nb", &"n".repeat(65)] {
        assert!(
            matches!(
                store.create_member("a@example.com", name, PHC),
                Err(MetaError::Invalid(_))
            ),
            "{name:?}"
        );
    }
    for password in [
        "hunter2hunter2",
        "",
        "$argon2i$v=19$x",
        "$argon2id$caf\u{e9}",
    ] {
        assert!(
            matches!(
                store.create_member("a@example.com", "Ana", password),
                Err(MetaError::Invalid(_))
            ),
            "{password:?}"
        );
    }
    assert!(!store.has_members().expect("has"));
}

#[test]
fn roles_are_ordered_and_an_organization_keeps_an_owner() {
    assert!(Role::Viewer < Role::Admin && Role::Admin < Role::Owner);
    for role in [Role::Viewer, Role::Admin, Role::Owner] {
        assert_eq!(Role::parse(role.as_str()), Some(role));
    }
    assert_eq!(Role::parse("root"), None);

    let store = store_at(&clock());
    let org = store.create_organization("Acme").expect("org");
    let other = store.create_organization("Other").expect("org");
    let ana = member(&store, "ana@example.com");
    let bia = member(&store, "bia@example.com");

    store
        .set_role(ana.id, org.id, Some(Role::Owner))
        .expect("owner");
    assert!(matches!(
        store.set_role(ana.id, org.id, Some(Role::Admin)),
        Err(MetaError::Conflict(_))
    ));
    assert!(matches!(
        store.set_role(ana.id, org.id, None),
        Err(MetaError::Conflict(_))
    ));
    store
        .set_role(bia.id, org.id, Some(Role::Owner))
        .expect("second owner");
    store
        .set_role(ana.id, org.id, Some(Role::Viewer))
        .expect("lowered");
    store
        .set_role(ana.id, other.id, Some(Role::Admin))
        .expect("elsewhere");

    let memberships = store.memberships(ana.id).expect("memberships");
    assert_eq!(
        memberships,
        vec![
            Membership {
                organization_id: org.id,
                role: Role::Viewer
            },
            Membership {
                organization_id: other.id,
                role: Role::Admin
            },
        ]
    );
    let members = store.organization_members(org.id).expect("members");
    assert_eq!(
        members
            .iter()
            .map(|m| (m.member.email.as_str(), m.role))
            .collect::<Vec<_>>(),
        vec![
            ("ana@example.com", Role::Viewer),
            ("bia@example.com", Role::Owner)
        ]
    );
    assert!(matches!(
        store.set_role(99, org.id, Some(Role::Viewer)),
        Err(MetaError::NotFound("member"))
    ));
    assert!(matches!(
        store.set_role(ana.id, 99, Some(Role::Viewer)),
        Err(MetaError::NotFound("organization"))
    ));
}

#[test]
fn a_session_lives_until_it_expires() {
    let time = clock();
    let store = store_at(&time);
    let org = store.create_organization("Acme").expect("org");
    let ana = member(&store, "ana@example.com");
    store
        .set_role(ana.id, org.id, Some(Role::Admin))
        .expect("role");

    let expires = store.create_session(HASH_A, ana.id, 3600).expect("session");
    assert_eq!(expires, 1_003_600);
    let login = store.session(HASH_A).expect("lookup").expect("valid");
    assert_eq!(login.member.id, ana.id);
    assert_eq!(login.memberships.len(), 1);

    time.store(1_003_599, Ordering::Relaxed);
    assert!(store.session(HASH_A).expect("lookup").is_some());
    time.store(1_003_600, Ordering::Relaxed);
    assert!(store.session(HASH_A).expect("lookup").is_none());
    assert_eq!(store.delete_expired_sessions().expect("purge"), 1);
    assert_eq!(store.delete_expired_sessions().expect("purge"), 0);

    assert!(store.session("not a hash").expect("lookup").is_none());
    assert!(store.session(HASH_B).expect("lookup").is_none());
    assert!(matches!(
        store.create_session("short", ana.id, 10),
        Err(MetaError::Invalid(_))
    ));
    assert!(matches!(
        store.create_session(HASH_B, ana.id, 0),
        Err(MetaError::Invalid(_))
    ));
    assert!(matches!(
        store.create_session(HASH_B, 99, 10),
        Err(MetaError::NotFound("member"))
    ));
}

#[test]
fn disabling_a_member_or_changing_their_password_ends_their_sessions() {
    let store = store_at(&clock());
    let ana = member(&store, "ana@example.com");
    store.create_session(HASH_A, ana.id, 3600).expect("session");
    store
        .set_password_hash(ana.id, &PHC.replace("c29t", "dGVz"))
        .expect("password");
    assert!(store.session(HASH_A).expect("lookup").is_none());
    assert!(matches!(
        store.set_password_hash(ana.id, "plain"),
        Err(MetaError::Invalid(_))
    ));
    assert!(matches!(
        store.set_password_hash(99, PHC),
        Err(MetaError::NotFound("member"))
    ));

    store.create_session(HASH_B, ana.id, 3600).expect("session");
    store.set_disabled(ana.id, true).expect("disable");
    assert!(store.session(HASH_B).expect("lookup").is_none());
    assert!(store.member(ana.id).expect("member").disabled);
    assert!(matches!(
        store.create_session(HASH_A, ana.id, 3600),
        Err(MetaError::Conflict(_))
    ));
    store.set_disabled(ana.id, false).expect("enable");
    assert!(store.create_session(HASH_A, ana.id, 3600).is_ok());
}

#[test]
fn deleting_a_session_logs_it_out() {
    let store = store_at(&clock());
    let ana = member(&store, "ana@example.com");
    store.create_session(HASH_A, ana.id, 3600).expect("session");
    store.delete_session(HASH_A).expect("delete");
    store.delete_session(HASH_A).expect("again");
    assert!(store.session(HASH_A).expect("lookup").is_none());
}

#[test]
fn a_read_token_belongs_to_one_site_and_can_be_revoked() {
    let store = store_at(&clock());
    let org = store.create_organization("Acme").expect("org");
    let shop = store.create_site(org.id, "shop").expect("site");
    let blog = store.create_site(org.id, "blog").expect("site");

    let token = store
        .issue_read_token(shop.id, "dashboard", HASH_A, None)
        .expect("token");
    assert_eq!(token.site_id, shop.id);
    let found = store.read_token(HASH_A).expect("lookup").expect("found");
    assert_eq!(found.id, token.id);
    assert!(store.read_token(HASH_B).expect("lookup").is_none());
    assert!(store.read_token("zz").expect("lookup").is_none());
    assert!(
        store
            .issue_read_token(shop.id, "again", HASH_A, None)
            .is_err(),
        "unique hash"
    );
    assert!(matches!(
        store.issue_read_token(99, "x", HASH_B, None),
        Err(MetaError::NotFound("site"))
    ));
    assert!(matches!(
        store.issue_read_token(shop.id, "x", "short", None),
        Err(MetaError::Invalid(_))
    ));

    assert!(matches!(
        store.revoke_read_token(blog.id, token.id),
        Err(MetaError::NotFound("read token"))
    ));
    assert!(store.read_token(HASH_A).expect("lookup").is_some());
    store.revoke_read_token(shop.id, token.id).expect("revoke");
    store.revoke_read_token(shop.id, token.id).expect("again");
    assert!(store.read_token(HASH_A).expect("lookup").is_none());
    let listed = store.read_tokens(shop.id).expect("list");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked_at.is_some());
    assert!(store.read_tokens(blog.id).expect("list").is_empty());
}

#[test]
fn no_secret_is_kept_in_the_clear() {
    let store = store_at(&clock());
    let org = store.create_organization("Acme").expect("org");
    let shop = store.create_site(org.id, "shop").expect("site");
    let ana = member(&store, "ana@example.com");
    store.create_session(HASH_A, ana.id, 60).expect("session");
    store
        .issue_read_token(shop.id, "t", HASH_B, Some(ana.id))
        .expect("token");
    let (_, stored) = store
        .member_for_login("ana@example.com")
        .expect("lookup")
        .expect("found");
    assert!(stored.starts_with("$argon2id$"));
}

#[test]
fn a_site_has_a_time_zone() {
    let store = store_at(&clock());
    let org = store.create_organization("Acme").expect("org");
    let shop = store.create_site(org.id, "shop").expect("site");
    assert_eq!(shop.time_zone, "UTC");
    store
        .set_site_time_zone(shop.id, "America/Sao_Paulo")
        .expect("set");
    assert_eq!(
        store.site(shop.id).expect("site").time_zone,
        "America/Sao_Paulo"
    );
    assert_eq!(
        store.sites().expect("sites")[0].time_zone,
        "America/Sao_Paulo"
    );
    for bad in ["", "..", "a b", "x'y", &"z".repeat(65)] {
        assert!(
            matches!(
                store.set_site_time_zone(shop.id, bad),
                Err(MetaError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        store.set_site_time_zone(99, "UTC"),
        Err(MetaError::NotFound("site"))
    ));
    assert!(matches!(store.site(99), Err(MetaError::NotFound("site"))));
}

#[test]
fn erasure_requests_of_one_site_are_listed_newest_first_within_a_limit() {
    let store = SqliteMeta::open_in_memory()
        .expect("an in-memory database")
        .with_hash_key(HashKey::from_secret("0123456789abcdef0123456789abcdef").expect("key"));
    let org = store.create_organization("Acme").expect("org");
    let shop = store.create_site(org.id, "shop").expect("site");
    let blog = store.create_site(org.id, "blog").expect("site");
    for n in 0..5 {
        store
            .request_erasure(shop.id, &format!("user-{n}"), "test")
            .expect("request");
    }
    store
        .request_erasure(blog.id, "user-x", "test")
        .expect("request");

    let all = store
        .site_erasure_requests(shop.id, None, 10)
        .expect("list");
    assert_eq!(all.len(), 5);
    assert!(all.iter().all(|r| r.site_id == shop.id));
    let newest = store.site_erasure_requests(shop.id, None, 2).expect("list");
    assert_eq!(newest.len(), 2);
    assert_eq!(newest[1].id, all[4].id, "the newest are kept, oldest first");
    assert!(
        store
            .site_erasure_requests(shop.id, Some(ErasureState::Complete), 10)
            .expect("list")
            .is_empty()
    );
}
