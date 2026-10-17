use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use dionaea_meta::{AccountStore, MetaStore, SqliteMeta};

use super::*;

const PASSWORD: &str = "correct horse battery";

struct World {
    meta: Arc<SqliteMeta>,
    auth: Auth,
    time: Arc<AtomicI64>,
}

fn world_with(config: AuthConfig) -> World {
    let time = Arc::new(AtomicI64::new(1_000_000));
    let clock = Arc::clone(&time);
    let meta = Arc::new(
        SqliteMeta::open_in_memory()
            .expect("an in-memory database")
            .with_clock({
                let time = Arc::clone(&time);
                move || time.load(Ordering::Relaxed)
            }),
    );
    let auth = Auth::new(
        Arc::clone(&meta) as Arc<dyn AccountStore>,
        Hasher::with_cost(8, 1, 1).expect("a cost"),
        config,
    )
    .with_clock(move || clock.load(Ordering::Relaxed));
    World { meta, auth, time }
}

fn world() -> World {
    world_with(AuthConfig::default())
}

#[test]
fn a_member_logs_in_and_the_session_identifies_them() {
    let w = world();
    let ana = w
        .auth
        .create_member("Ana@Example.com", "Ana", PASSWORD)
        .expect("member");
    let session = w
        .auth
        .login("ana@example.com", PASSWORD, "10.0.0.1")
        .expect("login");
    assert!(has_shape(SESSION_PREFIX, &session.secret));
    assert_eq!(session.member.id, ana.id);
    assert_eq!(session.expires_at, 1_000_000 + 7 * 24 * 3600);

    let principal = w
        .auth
        .authenticate_session(&session.secret)
        .expect("lookup")
        .expect("valid session");
    assert_eq!(principal.member().map(|m| m.id), Some(ana.id));

    let cookie = w.auth.session_cookie(&session);
    assert!(cookie.starts_with("dionaea_session=ds_"), "{cookie}");
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
    let sent = format!("theme=dark; dionaea_session={}", session.secret);
    assert_eq!(cookie_from_header(&sent), Some(session.secret.as_str()));
}

#[test]
fn the_database_holds_neither_the_password_nor_the_session_secret() {
    let w = world();
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let session = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    let (_, stored) = w
        .meta
        .member_for_login("ana@example.com")
        .expect("lookup")
        .expect("found");
    assert!(!stored.contains(PASSWORD));
    assert!(
        w.meta.session(&session.secret).expect("lookup").is_none(),
        "only the hash is a key"
    );
    assert!(
        w.meta
            .session(&hash(&session.secret))
            .expect("lookup")
            .is_some()
    );
}

#[test]
fn a_wrong_password_an_unknown_address_and_a_disabled_member_look_the_same() {
    let w = world();
    let ana = w
        .auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let wrong = w
        .auth
        .login("ana@example.com", "not the password", "")
        .unwrap_err();
    let unknown = w
        .auth
        .login("nobody@example.com", PASSWORD, "")
        .unwrap_err();
    w.meta.set_disabled(ana.id, true).expect("disable");
    let disabled = w.auth.login("ana@example.com", PASSWORD, "").unwrap_err();
    for error in [wrong, unknown, disabled] {
        assert!(matches!(error, AuthError::InvalidCredentials), "{error:?}");
    }
}

#[test]
fn a_huge_password_is_refused_without_being_hashed() {
    let w = world();
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let huge = "x".repeat(10_000_000);
    assert!(matches!(
        w.auth.login("ana@example.com", &huge, ""),
        Err(AuthError::InvalidCredentials)
    ));
}

#[test]
fn guessing_is_stopped_per_address_and_per_client() {
    let w = world_with(AuthConfig {
        max_login_failures: 3,
        login_window_secs: 60,
        ..AuthConfig::default()
    });
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    w.auth
        .create_member("bia@example.com", "Bia", PASSWORD)
        .expect("member");
    for _ in 0..3 {
        assert!(matches!(
            w.auth
                .login("ana@example.com", "wrong wrong wrong", "10.0.0.1"),
            Err(AuthError::InvalidCredentials)
        ));
    }
    // Even the right password waits, from that address...
    assert!(matches!(
        w.auth.login("ana@example.com", PASSWORD, "10.0.0.2"),
        Err(AuthError::TooManyAttempts {
            retry_after_secs: 60
        })
    ));
    // ...and the client that failed three times is held for others too.
    assert!(matches!(
        w.auth.login("bia@example.com", PASSWORD, "10.0.0.1"),
        Err(AuthError::TooManyAttempts { .. })
    ));
    // Another client logging in as someone else is not affected.
    assert!(
        w.auth
            .login("bia@example.com", PASSWORD, "10.0.0.3")
            .is_ok()
    );
    w.time.fetch_add(61, Ordering::Relaxed);
    assert!(
        w.auth
            .login("ana@example.com", PASSWORD, "10.0.0.1")
            .is_ok()
    );
}

#[test]
fn unknown_addresses_are_limited_too() {
    let w = world_with(AuthConfig {
        max_login_failures: 2,
        ..AuthConfig::default()
    });
    for _ in 0..2 {
        let _ = w.auth.login("ghost@example.com", "whatever whatever", "");
    }
    assert!(matches!(
        w.auth.login("ghost@example.com", "whatever whatever", ""),
        Err(AuthError::TooManyAttempts { .. })
    ));
}

#[test]
fn logging_out_or_changing_the_password_ends_the_session() {
    let w = world();
    let ana = w
        .auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let first = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    w.auth.logout(&first.secret).expect("logout");
    assert!(
        w.auth
            .authenticate_session(&first.secret)
            .expect("lookup")
            .is_none()
    );
    w.auth.logout("not a secret").expect("ignored");

    let second = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    w.auth
        .set_password(ana.id, "another long password")
        .expect("password");
    assert!(
        w.auth
            .authenticate_session(&second.secret)
            .expect("lookup")
            .is_none()
    );
    assert!(w.auth.login("ana@example.com", PASSWORD, "").is_err());
    assert!(
        w.auth
            .login("ana@example.com", "another long password", "")
            .is_ok()
    );
}

#[test]
fn a_session_expires() {
    let w = world();
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let session = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    w.time.store(session.expires_at - 1, Ordering::Relaxed);
    assert!(
        w.auth
            .authenticate_session(&session.secret)
            .expect("lookup")
            .is_some()
    );
    w.time.store(session.expires_at, Ordering::Relaxed);
    assert!(
        w.auth
            .authenticate_session(&session.secret)
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn weak_passwords_are_refused_before_anything_is_stored() {
    let w = world();
    for password in ["", "short", "            "] {
        assert!(matches!(
            w.auth.create_member("ana@example.com", "Ana", password),
            Err(AuthError::WeakPassword(_))
        ));
    }
    assert!(!w.meta.has_members().expect("has"));
}

#[test]
fn a_read_token_identifies_its_site_and_can_be_revoked() {
    let w = world();
    let org = w.meta.create_organization("Acme").expect("org");
    let shop = w.meta.create_site(org.id, "shop").expect("site");
    let (info, secret) = w
        .auth
        .issue_read_token(shop.id, "dashboard", None)
        .expect("token");
    assert!(has_shape(READ_TOKEN_PREFIX, &secret));

    let principal = w
        .auth
        .authenticate_read_token(&secret)
        .expect("lookup")
        .expect("valid token");
    assert_eq!(principal, Principal::ReadToken(info.clone()));
    assert!(principal.member().is_none());

    // A token is not a session and a session is not a token.
    assert!(
        w.auth
            .authenticate_session(&secret)
            .expect("lookup")
            .is_none()
    );
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let session = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    assert!(
        w.auth
            .authenticate_read_token(&session.secret)
            .expect("lookup")
            .is_none()
    );

    w.meta.revoke_read_token(shop.id, info.id).expect("revoke");
    assert!(
        w.auth
            .authenticate_read_token(&secret)
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn only_the_hash_of_a_read_token_is_stored() {
    let w = world();
    let org = w.meta.create_organization("Acme").expect("org");
    let shop = w.meta.create_site(org.id, "shop").expect("site");
    let (_, secret) = w
        .auth
        .issue_read_token(shop.id, "dashboard", None)
        .expect("token");
    assert!(w.meta.read_token(&secret).expect("lookup").is_none());
    assert!(w.meta.read_token(&hash(&secret)).expect("lookup").is_some());
}

fn member_of(w: &World, email: &str, org: u32, role: Option<Role>) -> Principal {
    let member = w
        .auth
        .create_member(email, "Name", PASSWORD)
        .expect("member");
    if let Some(role) = role {
        w.meta.set_role(member.id, org, Some(role)).expect("role");
    }
    let session = w.auth.login(email, PASSWORD, "").expect("login");
    w.auth
        .authenticate_session(&session.secret)
        .expect("lookup")
        .expect("valid")
}

#[test]
fn authorization_follows_the_role_in_the_organization_of_the_site() {
    let w = world();
    let acme = w.meta.create_organization("Acme").expect("org");
    let other = w.meta.create_organization("Other").expect("org");
    let shop = w.meta.create_site(acme.id, "shop").expect("site");
    let there = Scope {
        organization_id: acme.id,
        site_id: Some(shop.id),
    };
    let elsewhere = Scope {
        organization_id: other.id,
        site_id: None,
    };

    let viewer = member_of(&w, "viewer@example.com", acme.id, Some(Role::Viewer));
    let admin = member_of(&w, "admin@example.com", acme.id, Some(Role::Admin));
    let owner = member_of(&w, "owner@example.com", acme.id, Some(Role::Owner));
    let stranger = member_of(&w, "stranger@example.com", acme.id, None);

    let grid = [
        (&viewer, [true, false, false]),
        (&admin, [true, true, false]),
        (&owner, [true, true, true]),
        (&stranger, [false, false, false]),
    ];
    for (principal, expected) in grid {
        let got = [
            Permission::Read,
            Permission::Manage,
            Permission::ManageMembers,
        ]
        .map(|p| principal.authorize(p, there).is_ok());
        assert_eq!(got, expected, "{}", principal.describe());
        for permission in [
            Permission::Read,
            Permission::Manage,
            Permission::ManageMembers,
        ] {
            assert_eq!(
                principal.authorize(permission, elsewhere),
                Err(Denied),
                "a role in one organization means nothing in another"
            );
        }
    }
}

#[test]
fn a_read_token_reads_its_own_site_and_nothing_else() {
    let w = world();
    let acme = w.meta.create_organization("Acme").expect("org");
    let shop = w.meta.create_site(acme.id, "shop").expect("site");
    let blog = w.meta.create_site(acme.id, "blog").expect("site");
    let (_, secret) = w
        .auth
        .issue_read_token(shop.id, "dashboard", None)
        .expect("token");
    let token = w
        .auth
        .authenticate_read_token(&secret)
        .expect("lookup")
        .expect("valid");

    let scope = |site_id| Scope {
        organization_id: acme.id,
        site_id,
    };
    assert_eq!(
        token.authorize(Permission::Read, scope(Some(shop.id))),
        Ok(())
    );
    assert_eq!(
        token.authorize(Permission::Read, scope(Some(blog.id))),
        Err(Denied)
    );
    assert_eq!(
        token.authorize(Permission::Read, scope(None)),
        Err(Denied),
        "an organization-wide request is not a site"
    );
    for permission in [Permission::Manage, Permission::ManageMembers] {
        assert_eq!(
            token.authorize(permission, scope(Some(shop.id))),
            Err(Denied)
        );
    }
}

#[test]
fn the_debug_forms_show_no_secret() {
    let w = world();
    let rendered = format!("{:?}", w.auth);
    assert!(!rendered.contains("argon2"));
}

#[test]
fn only_a_few_passwords_are_hashed_at_once() {
    let w = world_with(AuthConfig {
        max_concurrent_hashes: 2,
        ..AuthConfig::default()
    });
    let first = w.auth.slot().expect("room");
    let second = w.auth.slot().expect("room");
    assert!(matches!(w.auth.slot(), Err(AuthError::Busy)));
    assert!(matches!(
        w.auth.login("ana@example.com", PASSWORD, ""),
        Err(AuthError::Busy)
    ));
    assert!(matches!(
        w.auth.create_member("ana@example.com", "Ana", PASSWORD),
        Err(AuthError::Busy)
    ));
    drop(first);
    // A refused attempt gave its place back and counted as no failure.
    assert!(w.auth.slot().is_ok());
    drop(second);
    assert_eq!(w.auth.hashing.load(Ordering::Relaxed), 0);
}

#[test]
fn expired_sessions_are_removed_as_new_ones_are_made() {
    let w = world();
    w.auth
        .create_member("ana@example.com", "Ana", PASSWORD)
        .expect("member");
    let old = w
        .auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    w.time.store(old.expires_at + 1, Ordering::Relaxed);
    w.auth
        .login("ana@example.com", PASSWORD, "")
        .expect("login");
    assert!(
        w.meta
            .session(&hash(&old.secret))
            .expect("lookup")
            .is_none()
    );
    assert_eq!(
        w.meta.delete_expired_sessions().expect("purge"),
        0,
        "already removed"
    );
}

#[test]
fn a_member_is_added_to_an_organization_in_one_step() {
    let w = world();
    let org = w.meta.create_organization("Acme").expect("org");
    let ana = w
        .auth
        .create_member_in("ana@example.com", "Ana", PASSWORD, org.id, Role::Admin)
        .expect("member");
    assert_eq!(
        w.meta.memberships(ana.id).expect("roles")[0].role,
        Role::Admin
    );
    // A missing organization leaves no member behind.
    assert!(
        w.auth
            .create_member_in("bia@example.com", "Bia", PASSWORD, 99, Role::Viewer)
            .is_err()
    );
    assert!(
        w.meta
            .member_for_login("bia@example.com")
            .expect("lookup")
            .is_none()
    );
}
