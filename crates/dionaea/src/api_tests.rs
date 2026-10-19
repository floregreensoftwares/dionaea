//! The API against a real metadata store. The analytics store is a ClickHouse
//! that is not there, so what is proven here is everything that happens
//! before a report is read: who is let in, what each role and each token may
//! reach, and what the administration endpoints do. Reading reports against
//! ClickHouse is in `tests/api.rs`.

use dionaea_auth::{Auth, AuthConfig, Hasher};
use dionaea_meta::{AccountStore, HashKey, MetaStore, Role, SqliteMeta};
use serde_json::{Value, json};

use super::*;

const PASSWORD: &str = "correct horse battery";
const V1: &str = "[events.checkout.fields.coupon]\nkind = \"enum\"\nvalues = [\"a\", \"b\"]\n";
const V2: &str =
    "[events.checkout.fields.coupon]\nkind = \"enum\"\nvalues = [\"a\", \"b\"]\n[events.search]\n";
const BREAKING: &str = "[events.checkout.fields.coupon]\nkind = \"count\"\nmax = 5\n";

fn artifacts_dir() -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "dionaea-api-artifacts-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct World {
    api: Api,
    meta: Arc<SqliteMeta>,
    acme: u32,
    other: u32,
    shop: u32,
    blog: u32,
    elsewhere: u32,
}

fn world() -> World {
    let meta = Arc::new(
        SqliteMeta::open_in_memory()
            .expect("an in-memory database")
            .with_hash_key(HashKey::from_secret("0123456789abcdef0123456789abcdef").expect("key")),
    );
    let auth = Arc::new(Auth::new(
        Arc::clone(&meta) as Arc<dyn AccountStore>,
        Hasher::with_cost(8, 1, 1).expect("a cost"),
        AuthConfig::default(),
    ));
    // Nothing listens on port 1.
    let mut config = dionaea_store::Config::new("127.0.0.1", 1, "dionaea");
    config.timeout = std::time::Duration::from_millis(300);
    let store = Store::new(config).expect("a store");
    let acme = meta.create_organization("Acme").expect("org").id;
    let other = meta.create_organization("Other").expect("org").id;
    let shop = meta.create_site(acme, "shop").expect("site").id;
    let blog = meta.create_site(acme, "blog").expect("site").id;
    let elsewhere = meta.create_site(other, "elsewhere").expect("site").id;
    let api = Api::new(
        Arc::clone(&meta),
        auth,
        store,
        Limits::default(),
        Arc::new(dionaea_artifacts::LocalArtifacts::open(artifacts_dir()).expect("artifacts")),
        || 395,
    );
    World {
        api,
        meta,
        acme,
        other,
        shop,
        blog,
        elsewhere,
    }
}

#[derive(Clone)]
struct Caller {
    cookie: Option<String>,
    bearer: Option<String>,
}

impl Caller {
    fn nobody() -> Self {
        Self {
            cookie: None,
            bearer: None,
        }
    }
}

fn request(method: &str, path: &str, caller: &Caller, body: Option<Value>) -> ApiRequest {
    ApiRequest {
        method: method.to_owned(),
        path: format!("/api/v1/{path}"),
        query: None,
        cookie: caller.cookie.clone(),
        authorization: caller.bearer.as_ref().map(|t| format!("Bearer {t}")),
        origin: None,
        host: Some("dionaea.test".to_owned()),
        content_type: body.as_ref().map(|_| "application/json".to_owned()),
        body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
        client: "10.0.0.1".to_owned(),
    }
}

impl World {
    fn call(&self, method: &str, path: &str, caller: &Caller, body: Option<Value>) -> ApiResponse {
        self.api.respond(&request(method, path, caller, body))
    }

    fn status(&self, method: &str, path: &str, caller: &Caller, body: Option<Value>) -> u16 {
        self.call(method, path, caller, body).status
    }
}

/// Creates a member straight in the store and logs them in through the API.
fn login(w: &World, email: &str, memberships: &[(u32, Role)]) -> Caller {
    let hasher = Hasher::with_cost(8, 1, 1).expect("a cost");
    let member = w
        .meta
        .create_member(email, "Name", &hasher.hash(PASSWORD).expect("hash"))
        .expect("member");
    for (org, role) in memberships {
        w.meta.set_role(member.id, *org, Some(*role)).expect("role");
    }
    let response = w.api.respond(&request(
        "POST",
        "login",
        &Caller::nobody(),
        Some(json!({ "email": email, "password": PASSWORD })),
    ));
    assert_eq!(response.status, 200, "{:?}", response.body);
    let cookie = response.set_cookie.expect("a cookie");
    let secret = cookie.split(';').next().expect("a pair").to_owned();
    Caller {
        cookie: Some(secret),
        bearer: None,
    }
}

fn token_for(w: &World, caller: &Caller, site: u32) -> Caller {
    let response = w.call(
        "POST",
        &format!("sites/{site}/tokens"),
        caller,
        Some(json!({ "label": "script" })),
    );
    assert_eq!(response.status, 201, "{:?}", response.body);
    Caller {
        cookie: None,
        bearer: response.body["token"].as_str().map(str::to_owned),
    }
}

/// A query that passes authorization and then fails validation: `400` proves
/// the caller got that far, without needing ClickHouse.
fn bad_query() -> Value {
    json!({ "from_ms": 5, "to_ms": 5, "report": { "type": "overview" } })
}

#[test]
fn nobody_gets_in_without_logging_in() {
    let w = world();
    let nobody = Caller::nobody();
    for (method, path) in [
        ("GET", "me".to_owned()),
        ("GET", "organizations".to_owned()),
        ("GET", format!("sites/{}", w.shop)),
        ("POST", format!("sites/{}/query", w.shop)),
        ("GET", format!("sites/{}/keys", w.shop)),
        ("POST", format!("sites/{}/erasures", w.shop)),
        ("POST", "logout".to_owned()),
    ] {
        assert_eq!(
            w.status(method, &path, &nobody, None),
            401,
            "{method} {path}"
        );
    }
    let garbage = Caller {
        cookie: Some(
            "dionaea_session=ds_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        ),
        bearer: None,
    };
    assert_eq!(w.status("GET", "me", &garbage, None), 401);
    let wrong_scheme = ApiRequest {
        authorization: Some("Basic abc".to_owned()),
        ..request("GET", "me", &nobody, None)
    };
    assert_eq!(w.api.respond(&wrong_scheme).status, 401);
}

#[test]
fn logging_in_and_out() {
    let w = world();
    let hasher = Hasher::with_cost(8, 1, 1).expect("a cost");
    w.meta
        .create_member(
            "ana@example.com",
            "Ana",
            &hasher.hash(PASSWORD).expect("hash"),
        )
        .expect("member");
    let wrong = w.call(
        "POST",
        "login",
        &Caller::nobody(),
        Some(json!({ "email": "ana@example.com", "password": "not the password" })),
    );
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.body["error"]["code"], "invalid_credentials");
    assert!(wrong.set_cookie.is_none());

    let ana = login_existing(&w, "ana@example.com");
    let me = w.call("GET", "me", &ana, None);
    assert_eq!(me.status, 200);
    assert_eq!(me.body["member"]["email"], "ana@example.com");

    let out = w.call("POST", "logout", &ana, None);
    assert_eq!(out.status, 204);
    assert!(out.set_cookie.expect("cookie").contains("Max-Age=0"));
    assert_eq!(w.status("GET", "me", &ana, None), 401);
}

fn login_existing(w: &World, email: &str) -> Caller {
    let response = w.call(
        "POST",
        "login",
        &Caller::nobody(),
        Some(json!({ "email": email, "password": PASSWORD })),
    );
    assert_eq!(response.status, 200);
    let cookie = response.set_cookie.expect("cookie");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    Caller {
        cookie: Some(cookie.split(';').next().expect("pair").to_owned()),
        bearer: None,
    }
}

#[test]
fn repeated_failures_lock_the_login_out() {
    let w = world();
    for _ in 0..10 {
        let r = w.call(
            "POST",
            "login",
            &Caller::nobody(),
            Some(json!({ "email": "ghost@example.com", "password": "whatever whatever" })),
        );
        assert_eq!(r.status, 401);
    }
    let blocked = w.call(
        "POST",
        "login",
        &Caller::nobody(),
        Some(json!({ "email": "ghost@example.com", "password": "whatever whatever" })),
    );
    assert_eq!(blocked.status, 429);
    assert!(blocked.retry_after.is_some());
}

#[test]
fn a_viewer_reads_but_never_reaches_administration() {
    let w = world();
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);

    assert_eq!(
        w.status("GET", &format!("sites/{}", w.shop), &viewer, None),
        200
    );
    assert_eq!(
        w.status(
            "GET",
            &format!("organizations/{}/sites", w.acme),
            &viewer,
            None
        ),
        200
    );
    assert_eq!(
        w.status(
            "POST",
            &format!("sites/{}/query", w.shop),
            &viewer,
            Some(bad_query())
        ),
        400,
        "past authorization, stopped by validation"
    );

    let site = w.shop;
    let org = w.acme;
    let forbidden = [
        (
            "POST",
            format!("organizations/{org}/sites"),
            Some(json!({ "name": "new" })),
        ),
        ("GET", format!("organizations/{org}/members"), None),
        (
            "POST",
            format!("organizations/{org}/members"),
            Some(
                json!({ "email": "x@example.com", "name": "X", "password": PASSWORD, "role": "viewer" }),
            ),
        ),
        (
            "PUT",
            format!("sites/{site}/time-zone"),
            Some(json!({ "time_zone": "UTC" })),
        ),
        ("GET", format!("sites/{site}/keys"), None),
        (
            "POST",
            format!("sites/{site}/keys"),
            Some(json!({ "label": "k" })),
        ),
        ("GET", format!("sites/{site}/schema/versions"), None),
        ("POST", format!("sites/{site}/schema"), None),
        ("GET", format!("sites/{site}/tokens"), None),
        (
            "POST",
            format!("sites/{site}/tokens"),
            Some(json!({ "label": "t" })),
        ),
        ("GET", format!("sites/{site}/retention"), None),
        (
            "PUT",
            format!("sites/{site}/retention"),
            Some(json!({ "days": 30 })),
        ),
        ("DELETE", format!("sites/{site}/retention"), None),
        ("GET", format!("sites/{site}/erasures"), None),
        (
            "POST",
            format!("sites/{site}/erasures"),
            Some(json!({ "user_id": "u1" })),
        ),
    ];
    for (method, path, body) in forbidden {
        assert_eq!(
            w.status(method, &path, &viewer, body),
            403,
            "{method} {path}"
        );
    }
}

#[test]
fn an_admin_manages_sites_but_not_members() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let site = w.call(
        "POST",
        &format!("organizations/{}/sites", w.acme),
        &admin,
        Some(json!({ "name": "docs" })),
    );
    assert_eq!(site.status, 201);
    assert_eq!(site.body["time_zone"], "UTC");
    assert_eq!(
        w.status("GET", &format!("sites/{}/keys", w.shop), &admin, None),
        200
    );
    assert_eq!(
        w.status(
            "GET",
            &format!("organizations/{}/members", w.acme),
            &admin,
            None
        ),
        200
    );
    for (method, path, body) in [
        (
            "POST",
            format!("organizations/{}/members", w.acme),
            Some(
                json!({ "email": "x@example.com", "name": "X", "password": PASSWORD, "role": "viewer" }),
            ),
        ),
        (
            "PUT",
            format!("organizations/{}/members/1", w.acme),
            Some(json!({ "role": "owner" })),
        ),
        (
            "POST",
            "organizations".to_owned(),
            Some(json!({ "name": "Mine" })),
        ),
    ] {
        assert_eq!(
            w.status(method, &path, &admin, body),
            403,
            "{method} {path}"
        );
    }
}

#[test]
fn what_belongs_to_another_organization_looks_like_it_does_not_exist() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let owner_elsewhere = login(&w, "boss@example.com", &[(w.other, Role::Owner)]);

    let theirs = w.elsewhere;
    for (method, path, body) in [
        ("GET", format!("sites/{theirs}"), None),
        ("POST", format!("sites/{theirs}/query"), Some(bad_query())),
        ("GET", format!("sites/{theirs}/keys"), None),
        (
            "POST",
            format!("sites/{theirs}/keys"),
            Some(json!({ "label": "k" })),
        ),
        (
            "POST",
            format!("sites/{theirs}/tokens"),
            Some(json!({ "label": "t" })),
        ),
        (
            "PUT",
            format!("sites/{theirs}/retention"),
            Some(json!({ "days": 30 })),
        ),
        (
            "POST",
            format!("sites/{theirs}/erasures"),
            Some(json!({ "user_id": "u1" })),
        ),
        ("GET", format!("organizations/{}/sites", w.other), None),
        (
            "POST",
            format!("organizations/{}/sites", w.other),
            Some(json!({ "name": "s" })),
        ),
        ("GET", format!("organizations/{}/members", w.other), None),
    ] {
        assert_eq!(
            w.status(method, &path, &admin, body),
            404,
            "{method} {path}: as if it did not exist"
        );
    }
    // The same answer for a site that really does not exist.
    assert_eq!(w.status("GET", "sites/99999", &admin, None), 404);
    // And the other way round.
    assert_eq!(
        w.status("GET", &format!("sites/{}", w.shop), &owner_elsewhere, None),
        404
    );
}

#[test]
fn an_owner_manages_members_and_the_last_owner_stays() {
    let w = world();
    let owner = login(&w, "owner@example.com", &[(w.acme, Role::Owner)]);
    let owner_id = w
        .meta
        .member_for_login("owner@example.com")
        .unwrap()
        .unwrap()
        .0
        .id;

    let added = w.call(
        "POST",
        &format!("organizations/{}/members", w.acme),
        &owner,
        Some(json!({ "email": "new@example.com", "name": "New", "password": PASSWORD, "role": "viewer" })),
    );
    assert_eq!(added.status, 201, "{:?}", added.body);
    assert_eq!(added.body["role"], "viewer");
    assert!(!added.body.to_string().contains(PASSWORD));
    let new_id = added.body["id"].as_u64().expect("an id");
    // The new member can log in, and is a viewer.
    let new = login_existing(&w, "new@example.com");
    assert_eq!(
        w.status("GET", &format!("sites/{}/keys", w.shop), &new, None),
        403
    );

    // Promote, and the same session now manages.
    let promoted = w.call(
        "PUT",
        &format!("organizations/{}/members/{new_id}", w.acme),
        &owner,
        Some(json!({ "role": "admin" })),
    );
    assert_eq!(promoted.status, 204);
    assert_eq!(
        w.status("GET", &format!("sites/{}/keys", w.shop), &new, None),
        200
    );

    // Duplicates, bad roles, weak passwords.
    for body in [
        json!({ "email": "NEW@example.com", "name": "N", "password": PASSWORD, "role": "viewer" }),
        json!({ "email": "o@example.com", "name": "N", "password": PASSWORD, "role": "root" }),
        json!({ "email": "o@example.com", "name": "N", "password": "short", "role": "viewer" }),
    ] {
        let status = w.status(
            "POST",
            &format!("organizations/{}/members", w.acme),
            &owner,
            Some(body),
        );
        assert!(status == 409 || status == 400, "{status}");
    }

    // The only owner cannot be lowered or removed.
    for body in [json!({ "role": "viewer" }), json!({ "role": null })] {
        let r = w.call(
            "PUT",
            &format!("organizations/{}/members/{owner_id}", w.acme),
            &owner,
            Some(body),
        );
        assert_eq!(r.status, 409, "{:?}", r.body);
    }
    // A member of another organization is not found here.
    let outsider = w
        .meta
        .create_member(
            "out@example.com",
            "Out",
            &Hasher::with_cost(8, 1, 1).unwrap().hash(PASSWORD).unwrap(),
        )
        .unwrap();
    assert_eq!(
        w.status(
            "PUT",
            &format!("organizations/{}/members/{}", w.acme, outsider.id),
            &owner,
            Some(json!({ "role": "viewer" }))
        ),
        404
    );
}

#[test]
fn an_owner_may_start_another_organization_and_others_may_not() {
    let w = world();
    let owner = login(&w, "owner@example.com", &[(w.acme, Role::Owner)]);
    let created = w.call(
        "POST",
        "organizations",
        &owner,
        Some(json!({ "name": "Fresh" })),
    );
    assert_eq!(created.status, 201);
    let listed = w.call("GET", "organizations", &owner, None);
    let names: Vec<_> = listed.body["organizations"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|o| o["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&"Acme".to_owned()) && names.contains(&"Fresh".to_owned()));
    assert!(
        !names.contains(&"Other".to_owned()),
        "organizations of others are not listed"
    );
}

#[test]
fn a_read_token_reads_its_own_site_and_nothing_else() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let token = token_for(&w, &admin, w.shop);

    assert_eq!(
        w.status(
            "POST",
            &format!("sites/{}/query", w.shop),
            &token,
            Some(bad_query())
        ),
        400,
        "its own site: past authorization"
    );
    assert_eq!(
        w.status("GET", &format!("sites/{}", w.shop), &token, None),
        200
    );
    // The sibling site in the same organization, and a site elsewhere.
    for site in [w.blog, w.elsewhere] {
        assert_eq!(
            w.status(
                "POST",
                &format!("sites/{site}/query"),
                &token,
                Some(bad_query())
            ),
            404,
            "site {site}"
        );
        assert_eq!(w.status("GET", &format!("sites/{site}"), &token, None), 404);
    }
    // Nothing administrative, even on its own site.
    let shop = w.shop;
    for (method, path, body) in [
        ("GET", format!("sites/{shop}/keys"), None),
        (
            "POST",
            format!("sites/{shop}/keys"),
            Some(json!({ "label": "k" })),
        ),
        ("GET", format!("sites/{shop}/tokens"), None),
        (
            "POST",
            format!("sites/{shop}/tokens"),
            Some(json!({ "label": "t" })),
        ),
        (
            "PUT",
            format!("sites/{shop}/retention"),
            Some(json!({ "days": 1 })),
        ),
        (
            "POST",
            format!("sites/{shop}/erasures"),
            Some(json!({ "user_id": "u" })),
        ),
        ("GET", format!("organizations/{}/sites", w.acme), None),
        ("GET", "organizations".to_owned(), None),
        (
            "PUT",
            "me/password".to_owned(),
            Some(json!({ "current": "a", "new": "b" })),
        ),
    ] {
        assert_eq!(
            w.status(method, &path, &token, body),
            403,
            "{method} {path}"
        );
    }
    let me = w.call("GET", "me", &token, None);
    assert_eq!(me.body["type"], "read_token");
    assert_eq!(me.body["read_token"]["site_id"], w.shop);
}

#[test]
fn a_revoked_token_and_a_made_up_one_are_refused() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let issued = w.call(
        "POST",
        &format!("sites/{}/tokens", w.shop),
        &admin,
        Some(json!({ "label": "script" })),
    );
    let token = Caller {
        cookie: None,
        bearer: issued.body["token"].as_str().map(str::to_owned),
    };
    let id = issued.body["id"].as_u64().expect("an id");
    assert_eq!(w.status("GET", "me", &token, None), 200);

    // A token of another site cannot be revoked through this one.
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/tokens/{id}", w.blog),
            &admin,
            None
        ),
        404
    );
    assert_eq!(w.status("GET", "me", &token, None), 200);
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/tokens/{id}", w.shop),
            &admin,
            None
        ),
        204
    );
    assert_eq!(w.status("GET", "me", &token, None), 401);

    let made_up = Caller {
        cookie: None,
        bearer: Some(format!("rt_{}", "a".repeat(52))),
    };
    assert_eq!(w.status("GET", "me", &made_up, None), 401);
    // A session secret is not a token.
    let session_secret = admin
        .cookie
        .as_ref()
        .and_then(|c| c.split('=').nth(1))
        .map(str::to_owned);
    let as_bearer = Caller {
        cookie: None,
        bearer: session_secret,
    };
    assert_eq!(w.status("GET", "me", &as_bearer, None), 401);
}

#[test]
fn a_token_secret_is_shown_once() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let issued = w.call(
        "POST",
        &format!("sites/{}/tokens", w.shop),
        &admin,
        Some(json!({ "label": "dashboard" })),
    );
    let secret = issued.body["token"].as_str().expect("a secret").to_owned();
    let listed = w.call("GET", &format!("sites/{}/tokens", w.shop), &admin, None);
    assert!(!listed.body.to_string().contains(&secret));
    assert_eq!(listed.body["tokens"][0]["label"], "dashboard");
}

#[test]
fn state_changes_from_another_origin_are_refused() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let mut foreign = request(
        "POST",
        &format!("organizations/{}/sites", w.acme),
        &admin,
        Some(json!({ "name": "evil" })),
    );
    foreign.origin = Some("https://evil.example".to_owned());
    let refused = w.api.respond(&foreign);
    assert_eq!(refused.status, 403);
    assert_eq!(refused.body["error"]["code"], "cross_origin");

    let mut same = request(
        "POST",
        &format!("organizations/{}/sites", w.acme),
        &admin,
        Some(json!({ "name": "good" })),
    );
    same.origin = Some("https://dionaea.test".to_owned());
    assert_eq!(w.api.respond(&same).status, 201);

    // Reading is not a state change.
    let mut read = request("GET", "me", &admin, None);
    read.origin = Some("https://evil.example".to_owned());
    assert_eq!(w.api.respond(&read).status, 200);
}

#[test]
fn bodies_must_be_json_and_must_not_carry_unknown_fields() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let path = format!("organizations/{}/sites", w.acme);
    let mut text = request("POST", &path, &admin, Some(json!({ "name": "x" })));
    text.content_type = Some("text/plain".to_owned());
    assert_eq!(w.api.respond(&text).status, 415);
    assert_eq!(
        w.status(
            "POST",
            &path,
            &admin,
            Some(json!({ "name": "x", "admin": true }))
        ),
        400
    );
    let mut broken = request("POST", &path, &admin, Some(json!({})));
    broken.body = b"{not json".to_vec();
    assert_eq!(w.api.respond(&broken).status, 400);
}

#[test]
fn unknown_routes_are_not_found() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    for path in [
        "nope",
        "sites",
        "sites/abc",
        "sites/1/nope",
        "organizations/1/nope",
        "",
    ] {
        assert_eq!(w.status("GET", path, &admin, None), 404, "{path:?}");
    }
    let outside = ApiRequest {
        path: "/elsewhere".to_owned(),
        ..request("GET", "me", &admin, None)
    };
    assert_eq!(w.api.respond(&outside).status, 404);
}

#[test]
fn write_keys_are_issued_listed_and_revoked_within_their_site() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let issued = w.call(
        "POST",
        &format!("sites/{}/keys", w.shop),
        &admin,
        Some(json!({ "label": "web", "origins": ["https://shop.example"] })),
    );
    assert_eq!(issued.status, 201);
    let key = issued.body["key"].as_str().expect("a key").to_owned();
    assert!(key.starts_with("wk_"));
    assert_eq!(issued.body["origins"][0], "https://shop.example");

    let listed = w.call("GET", &format!("sites/{}/keys", w.shop), &admin, None);
    assert_eq!(listed.body["keys"][0]["key"], key.as_str());

    // Through the wrong site it is not found, and stays valid.
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/keys/{key}", w.blog),
            &admin,
            None
        ),
        404
    );
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/keys/{key}", w.shop),
            &admin,
            None
        ),
        204
    );
    let after = w.call("GET", &format!("sites/{}/keys", w.shop), &admin, None);
    assert!(after.body["keys"][0]["revoked_at"].is_number());
    assert_eq!(
        w.status(
            "POST",
            &format!("sites/{}/keys", w.shop),
            &admin,
            Some(json!({ "label": "" }))
        ),
        400
    );
}

#[test]
fn schemas_are_pushed_checked_and_retired() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let path = format!("sites/{}/schema", w.shop);
    let push = |source: &str, dry: bool| {
        let mut r = request("POST", &path, &admin, None);
        r.body = source.as_bytes().to_vec();
        r.content_type = Some("text/plain".to_owned());
        r.query = dry.then(|| "dry_run=true".to_owned());
        w.api.respond(&r)
    };
    let first = push(V1, false);
    assert_eq!(first.status, 201, "{:?}", first.body);
    assert_eq!(first.body["version"], 1);

    let dry = push(V2, true);
    assert_eq!(dry.status, 200);
    assert_eq!(dry.body["changes"][0]["path"], "events.search");
    let versions = w.call("GET", &format!("{path}/versions"), &admin, None);
    assert_eq!(
        versions.body["versions"].as_array().map(Vec::len),
        Some(1),
        "dry run stored nothing"
    );

    let second = push(V2, false);
    assert_eq!(second.body["version"], 2);
    assert_eq!(push(V2, false).status, 409, "unchanged");

    let breaking = push(BREAKING, false);
    assert_eq!(breaking.status, 409);
    assert_eq!(breaking.body["error"]["code"], "breaking_change");
    assert!(breaking.body["error"]["details"][0]["path"].is_string());

    let invalid = push("this is not toml [", false);
    assert_eq!(invalid.status, 422);
    assert!(invalid.body["error"]["details"][0]["line"].is_number());

    assert_eq!(
        w.status("POST", &format!("{path}/versions/1/retire"), &admin, None),
        204
    );
    assert_eq!(
        w.status("POST", &format!("{path}/versions/2/retire"), &admin, None),
        409,
        "the last active version stays"
    );
}

#[test]
fn retention_is_read_set_and_unset() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let path = format!("sites/{}/retention", w.shop);
    let before = w.call("GET", &path, &admin, None);
    assert_eq!(before.body["days"], 395);
    assert_eq!(before.body["own"], false);
    assert_eq!(
        w.status("PUT", &path, &admin, Some(json!({ "days": 30 }))),
        204
    );
    let set = w.call("GET", &path, &admin, None);
    assert_eq!(
        (set.body["days"].as_u64(), set.body["own"].as_bool()),
        (Some(30), Some(true))
    );
    assert_eq!(set.body["default_days"], 395);
    assert_eq!(
        w.status("PUT", &path, &admin, Some(json!({ "days": 0 }))),
        400
    );
    assert_eq!(
        w.status("PUT", &path, &admin, Some(json!({ "days": 99999 }))),
        400
    );
    assert_eq!(w.status("DELETE", &path, &admin, None), 204);
    assert_eq!(w.call("GET", &path, &admin, None).body["days"], 395);
    // Another site's retention is its own.
    assert_eq!(
        w.call("GET", &format!("sites/{}/retention", w.blog), &admin, None)
            .body["own"],
        false
    );
}

#[test]
fn erasure_is_requested_followed_and_kept_to_its_site() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let path = format!("sites/{}/erasures", w.shop);

    let asked = w.call("POST", &path, &admin, Some(json!({ "user_id": "user-42" })));
    assert_eq!(asked.status, 202, "{:?}", asked.body);
    assert_eq!(asked.body["kind"], "user");
    assert_eq!(asked.body["state"], "requested");
    assert_eq!(asked.body["requested_by"], "admin@example.com");
    assert!(
        !asked.body.to_string().contains("user-42"),
        "the id is not echoed"
    );
    let id = asked.body["id"].as_u64().expect("an id");

    let device = w.call("POST", &path, &admin, Some(json!({ "device_id": "dev-1" })));
    assert_eq!(
        (device.status, device.body["kind"].as_str()),
        (202, Some("device"))
    );

    let listed = w.call("GET", &path, &admin, None);
    assert_eq!(listed.body["requests"].as_array().map(Vec::len), Some(2));
    let filtered = ApiRequest {
        query: Some("state=complete".to_owned()),
        ..request("GET", &path, &admin, None)
    };
    assert_eq!(
        w.api.respond(&filtered).body["requests"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let bad_state = ApiRequest {
        query: Some("state=nonsense".to_owned()),
        ..request("GET", &path, &admin, None)
    };
    assert_eq!(w.api.respond(&bad_state).status, 400);

    assert_eq!(w.status("GET", &format!("{path}/{id}"), &admin, None), 200);
    // The same id through another site is not found.
    assert_eq!(
        w.status(
            "GET",
            &format!("sites/{}/erasures/{id}", w.blog),
            &admin,
            None
        ),
        404
    );
    assert_eq!(
        w.status(
            "POST",
            &format!("sites/{}/erasures/{id}/retry", w.blog),
            &admin,
            None
        ),
        404
    );
    // Retrying a request that did not fail is a conflict, not a success.
    assert_eq!(
        w.status("POST", &format!("{path}/{id}/retry"), &admin, None),
        409
    );

    // Exactly one subject.
    for body in [json!({}), json!({ "user_id": "a", "device_id": "b" })] {
        assert_eq!(w.status("POST", &path, &admin, Some(body)), 400);
    }
    // The other site sees none of it.
    let other = w.call("GET", &format!("sites/{}/erasures", w.blog), &admin, None);
    assert_eq!(other.body["requests"].as_array().map(Vec::len), Some(0));
}

#[test]
fn changing_a_password_needs_the_old_one_and_ends_every_session() {
    let w = world();
    let hasher = Hasher::with_cost(8, 1, 1).expect("a cost");
    w.meta
        .create_member(
            "ana@example.com",
            "Ana",
            &hasher.hash(PASSWORD).expect("hash"),
        )
        .expect("member");
    let first = login_existing(&w, "ana@example.com");
    let second = login_existing(&w, "ana@example.com");
    let new = "an entirely different one";

    let wrong = w.call(
        "PUT",
        "me/password",
        &first,
        Some(json!({ "current": "not it at all", "new": new })),
    );
    assert_eq!(wrong.status, 401);
    let weak = w.call(
        "PUT",
        "me/password",
        &first,
        Some(json!({ "current": PASSWORD, "new": "short" })),
    );
    assert_eq!(weak.status, 400);
    let done = w.call(
        "PUT",
        "me/password",
        &first,
        Some(json!({ "current": PASSWORD, "new": new })),
    );
    assert_eq!(done.status, 204);
    assert_eq!(w.status("GET", "me", &first, None), 401);
    assert_eq!(w.status("GET", "me", &second, None), 401);
    let relog = w.call(
        "POST",
        "login",
        &Caller::nobody(),
        Some(json!({ "email": "ana@example.com", "password": new })),
    );
    assert_eq!(relog.status, 200);
}

#[test]
fn a_report_that_cannot_be_read_says_so_without_naming_the_store() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let ok_query = json!({
        "from_ms": 1_760_000_000_000_u64,
        "to_ms": 1_760_086_400_000_u64,
        "report": { "type": "overview" }
    });
    let response = w.call(
        "POST",
        &format!("sites/{}/query", w.shop),
        &admin,
        Some(ok_query),
    );
    assert_eq!(response.status, 503);
    assert_eq!(response.body["error"]["code"], "unavailable");
    let text = response.body.to_string();
    assert!(!text.contains("127.0.0.1"), "{text}");
}

#[test]
fn queries_name_the_rule_they_break() {
    let w = world();
    w.meta.publish_schema(w.shop, V1).expect("schema");
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let path = format!("sites/{}/query", w.shop);
    let day = 86_400_000_u64;
    let ask = |report: Value, from: u64, to: u64| {
        w.call(
            "POST",
            &path,
            &admin,
            Some(json!({ "from_ms": from, "to_ms": to, "report": report })),
        )
    };
    let undeclared = ask(
        json!({ "type": "series", "measure": "events", "event": "purchase", "granularity": "day", "breakdown": null }),
        day,
        2 * day,
    );
    assert_eq!(undeclared.body["error"]["code"], "undeclared_event");
    let property = ask(
        json!({ "type": "series", "measure": "events", "event": "checkout", "granularity": "day",
                "breakdown": { "dimension": "property", "event": "checkout", "name": "email" } }),
        day,
        2 * day,
    );
    assert_eq!(property.body["error"]["code"], "undeclared_property");
    let range = ask(json!({ "type": "overview" }), day, 400 * day);
    assert_eq!(range.body["error"]["code"], "range_too_large");
    assert_eq!(range.status, 400);
}

#[test]
fn reads_in_flight_are_bounded() {
    let counter = AtomicUsize::new(0);
    let held: Vec<_> = (0..MAX_QUERIES_IN_FLIGHT)
        .map(|_| InFlight::acquire(&counter, MAX_QUERIES_IN_FLIGHT).expect("room"))
        .collect();
    assert!(InFlight::acquire(&counter, MAX_QUERIES_IN_FLIGHT).is_none());
    assert_eq!(counter.load(Ordering::Relaxed), MAX_QUERIES_IN_FLIGHT);
    drop(held);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    assert!(InFlight::acquire(&counter, MAX_QUERIES_IN_FLIGHT).is_some());
}

#[test]
fn the_origin_check_compares_scheme_less_authority() {
    let base = request("POST", "login", &Caller::nobody(), None);
    let with = |origin: Option<&str>, host: Option<&str>| ApiRequest {
        origin: origin.map(str::to_owned),
        host: host.map(str::to_owned),
        ..request("POST", "login", &Caller::nobody(), None)
    };
    assert!(same_origin(&base));
    assert!(same_origin(&with(
        Some("http://localhost:8090"),
        Some("localhost:8090")
    )));
    assert!(same_origin(&with(
        Some("https://DIONAEA.test"),
        Some("dionaea.test")
    )));
    assert!(!same_origin(&with(
        Some("https://dionaea.test.evil.com"),
        Some("dionaea.test")
    )));
    assert!(!same_origin(&with(Some("null"), Some("dionaea.test"))));
    assert!(!same_origin(&with(Some("https://dionaea.test"), None)));
}

#[test]
fn the_retention_preview_is_for_administrators_and_checks_its_input() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);
    let path = format!("sites/{}/retention/preview", w.shop);
    assert_eq!(w.status("GET", &path, &viewer, None), 403);
    assert_eq!(
        w.status(
            "GET",
            &format!("sites/{}/retention/preview", w.elsewhere),
            &admin,
            None
        ),
        404
    );
    for days in ["0", "99999", "abc", ""] {
        let r = ApiRequest {
            query: Some(format!("days={days}")),
            ..request("GET", &path, &admin, None)
        };
        assert_eq!(w.api.respond(&r).status, 400, "{days:?}");
    }
    // Past validation it needs the store, which is not there.
    let r = ApiRequest {
        query: Some("days=30".to_owned()),
        ..request("GET", &path, &admin, None)
    };
    assert_eq!(w.api.respond(&r).status, 503);
}

#[test]
fn a_member_who_cannot_be_given_a_role_is_not_created() {
    let w = world();
    let owner = login(&w, "owner@example.com", &[(w.acme, Role::Owner)]);
    let created = w.call(
        "POST",
        &format!("organizations/{}/members", w.acme),
        &owner,
        Some(json!({ "email": "new@example.com", "name": "New", "password": PASSWORD, "role": "viewer" })),
    );
    assert_eq!(created.status, 201);
    assert_eq!(
        w.meta
            .memberships(created.body["id"].as_u64().unwrap() as u32)
            .unwrap()
            .len(),
        1,
        "created with its role"
    );
}

// Symbol files.

const SOURCE_MAP: &str = r#"{"version":3,"sources":["a.js"],"mappings":"AAAA"}"#;

fn upload(w: &World, caller: &Caller, site: u32, query: &str, body: &[u8]) -> ApiResponse {
    let mut r = request("POST", &format!("sites/{site}/symbols"), caller, None);
    r.query = Some(query.to_owned());
    r.body = body.to_vec();
    r.content_type = Some("application/octet-stream".to_owned());
    w.api.respond(&r)
}

fn upload_token_for(w: &World, caller: &Caller, site: u32) -> Caller {
    let response = w.call(
        "POST",
        &format!("sites/{site}/upload-tokens"),
        caller,
        Some(json!({ "label": "ci" })),
    );
    assert_eq!(response.status, 201, "{:?}", response.body);
    Caller {
        cookie: None,
        bearer: response.body["token"].as_str().map(str::to_owned),
    }
}

const ONE: &str = "release=1.4.2%2B57&kind=sourcemap&name=static%2Fjs%2Fmain.js.map";

#[test]
fn an_upload_token_uploads_symbol_files_to_its_site_and_nothing_else() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let ci = upload_token_for(&w, &admin, w.shop);

    let uploaded = upload(&w, &ci, w.shop, ONE, SOURCE_MAP.as_bytes());
    assert_eq!(uploaded.status, 201, "{:?}", uploaded.body);
    assert_eq!(uploaded.body["release"], "1.4.2+57");
    assert_eq!(uploaded.body["name"], "static/js/main.js.map");
    assert_eq!(uploaded.body["size"], SOURCE_MAP.len());
    assert_eq!(uploaded.body["sha256"].as_str().map(str::len), Some(64));

    // Other sites, other permissions.
    assert_eq!(
        upload(&w, &ci, w.blog, ONE, SOURCE_MAP.as_bytes()).status,
        404
    );
    assert_eq!(
        upload(&w, &ci, w.elsewhere, ONE, SOURCE_MAP.as_bytes()).status,
        404
    );
    let shop = w.shop;
    for (method, path, body) in [
        ("GET", format!("sites/{shop}"), None),
        ("POST", format!("sites/{shop}/query"), Some(bad_query())),
        ("GET", format!("sites/{shop}/symbols"), None),
        ("DELETE", format!("sites/{shop}/symbols"), None),
        ("GET", format!("sites/{shop}/keys"), None),
        ("GET", format!("sites/{shop}/upload-tokens"), None),
        (
            "POST",
            format!("sites/{shop}/upload-tokens"),
            Some(json!({ "label": "x" })),
        ),
        (
            "POST",
            format!("sites/{shop}/tokens"),
            Some(json!({ "label": "x" })),
        ),
        ("GET", "organizations".to_owned(), None),
    ] {
        assert_eq!(w.status(method, &path, &ci, body), 403, "{method} {path}");
    }
    let me = w.call("GET", "me", &ci, None);
    assert_eq!(me.body["type"], "upload_token");
}

#[test]
fn only_an_upload_token_or_an_admin_uploads() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);
    let read = token_for(&w, &admin, w.shop);
    let other_admin = login(&w, "boss@example.com", &[(w.other, Role::Admin)]);

    assert_eq!(
        upload(&w, &viewer, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        403
    );
    assert_eq!(
        upload(&w, &read, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        403
    );
    assert_eq!(
        upload(&w, &Caller::nobody(), w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        401
    );
    assert_eq!(
        upload(&w, &other_admin, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        404
    );
    assert_eq!(
        upload(&w, &admin, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        201
    );

    // A revoked upload token stops working.
    let ci = upload_token_for(&w, &admin, w.shop);
    assert_eq!(
        upload(&w, &ci, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        201
    );
    let id = w
        .call(
            "GET",
            &format!("sites/{}/upload-tokens", w.shop),
            &admin,
            None,
        )
        .body["tokens"][0]["id"]
        .as_u64()
        .expect("an id");
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/upload-tokens/{id}", w.blog),
            &admin,
            None
        ),
        404
    );
    assert_eq!(
        w.status(
            "DELETE",
            &format!("sites/{}/upload-tokens/{id}", w.shop),
            &admin,
            None
        ),
        204
    );
    assert_eq!(
        upload(&w, &ci, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        401
    );
}

#[test]
fn uploads_are_checked_before_anything_is_stored() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let good = SOURCE_MAP.as_bytes();
    for query in [
        "kind=sourcemap&name=a.map",
        "release=1&name=a.map",
        "release=1&kind=sourcemap",
        "release=1&kind=dsym&name=a.map",
        "release=has%20space&kind=sourcemap&name=a.map",
        "release=1&kind=sourcemap&name=..%2F..%2Fx",
        "release=1&kind=sourcemap&name=a%2F..%2Fb",
        "release=1&kind=sourcemap&name=%2Fabs",
        "release=%zz&kind=sourcemap&name=a.map",
    ] {
        assert_eq!(
            upload(&w, &admin, w.shop, query, good).status,
            400,
            "{query}"
        );
    }
    assert_eq!(upload(&w, &admin, w.shop, ONE, b"not json").status, 422);
    assert_eq!(upload(&w, &admin, w.shop, ONE, b"[1,2]").status, 422);
    assert_eq!(upload(&w, &admin, w.shop, ONE, b"").status, 413);
    assert_eq!(
        upload(
            &w,
            &admin,
            w.shop,
            "release=1&kind=proguard&name=mapping.txt",
            b"\xff\xfe"
        )
        .status,
        422
    );
    assert_eq!(
        w.call("GET", &format!("sites/{}/symbols", w.shop), &admin, None)
            .body["files"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "nothing was kept"
    );
}

#[test]
fn symbol_files_are_listed_replaced_counted_and_deleted() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let mapping = b"com.example.A -> a:\n    void run() -> b\n";
    assert_eq!(
        upload(&w, &admin, w.shop, ONE, SOURCE_MAP.as_bytes()).status,
        201
    );
    assert_eq!(
        upload(
            &w,
            &admin,
            w.shop,
            "release=1.4.2%2B57&kind=proguard&name=mapping.txt",
            mapping
        )
        .status,
        201
    );
    assert_eq!(
        upload(
            &w,
            &admin,
            w.shop,
            "release=1.5.0&kind=sourcemap&name=a.map",
            SOURCE_MAP.as_bytes()
        )
        .status,
        201
    );

    let path = format!("sites/{}/symbols", w.shop);
    let all = w.call("GET", &path, &admin, None);
    assert_eq!(all.body["files"].as_array().map(Vec::len), Some(3));
    assert_eq!(all.body["bytes"], SOURCE_MAP.len() * 2 + mapping.len());
    let one_release = ApiRequest {
        query: Some("release=1.5.0".to_owned()),
        ..request("GET", &path, &admin, None)
    };
    assert_eq!(
        w.api.respond(&one_release).body["files"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // Uploading the same file again replaces it and counts it once.
    let bigger = format!(r#"{{"version":3,"mappings":"{}"}}"#, "A".repeat(100));
    assert_eq!(
        upload(&w, &admin, w.shop, ONE, bigger.as_bytes()).status,
        201
    );
    let again = w.call("GET", &path, &admin, None);
    assert_eq!(again.body["files"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        again.body["bytes"],
        bigger.len() + SOURCE_MAP.len() + mapping.len()
    );

    // Another site sees none of it.
    assert_eq!(
        w.call("GET", &format!("sites/{}/symbols", w.blog), &admin, None)
            .body["files"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    let delete = |query: &str| {
        let r = ApiRequest {
            query: Some(query.to_owned()),
            ..request("DELETE", &path, &admin, None)
        };
        w.api.respond(&r).status
    };
    assert_eq!(
        delete("release=1.4.2%2B57&kind=proguard"),
        400,
        "kind without name"
    );
    assert_eq!(delete("release=1.4.2%2B57&kind=proguard&name=nope"), 404);
    assert_eq!(
        delete("release=1.4.2%2B57&kind=proguard&name=mapping.txt"),
        204
    );
    assert_eq!(
        delete("release=1.4.2%2B57&kind=proguard&name=mapping.txt"),
        404
    );
    assert_eq!(delete("release=1.4.2%2B57"), 204, "the rest of the release");
    assert_eq!(delete("release=1.4.2%2B57"), 404);
    assert_eq!(delete(""), 400);
    assert_eq!(
        w.call("GET", &path, &admin, None).body["files"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn the_body_limit_is_decided_after_the_caller_is_known() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);
    let ci = upload_token_for(&w, &admin, w.shop);
    let ordinary = 1000;
    let limit = |caller: &Caller, method: &str, path: &str| {
        let mut r = request(method, path, caller, None);
        r.content_type = Some("application/octet-stream".to_owned());
        w.api
            .body_limit(&r, ordinary)
            .map(|(limit, _slot)| limit)
            .map_err(|refusal| refusal.status)
    };
    let symbols = format!("sites/{}/symbols", w.shop);

    assert_eq!(limit(&ci, "POST", &symbols), Ok(symbols::MAX_SYMBOL_BYTES));
    assert_eq!(
        limit(&admin, "POST", &symbols),
        Ok(symbols::MAX_SYMBOL_BYTES)
    );
    assert_eq!(limit(&Caller::nobody(), "POST", &symbols), Err(401));
    assert_eq!(limit(&viewer, "POST", &symbols), Err(403));
    assert_eq!(
        limit(&ci, "POST", &format!("sites/{}/symbols", w.blog)),
        Err(404)
    );
    // Anything else keeps the ordinary limit, whoever asks.
    assert_eq!(
        limit(
            &Caller::nobody(),
            "POST",
            &format!("sites/{}/query", w.shop)
        ),
        Ok(ordinary)
    );
    assert_eq!(limit(&Caller::nobody(), "GET", &symbols), Ok(ordinary));
    assert_eq!(
        limit(&Caller::nobody(), "POST", "sites/x/symbols"),
        Ok(ordinary)
    );
    // A foreign origin is refused before the body is read.
    let mut foreign = request("POST", &symbols, &admin, None);
    foreign.origin = Some("https://evil.example".to_owned());
    assert_eq!(
        w.api
            .body_limit(&foreign, ordinary)
            .map(|(l, _)| l)
            .map_err(|r| r.status),
        Err(403)
    );
}

#[test]
fn uploads_are_bounded_before_their_bodies_are_read() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let symbols = format!("sites/{}/symbols", w.shop);
    let head = request("POST", &symbols, &admin, None);

    let held: Vec<_> = (0..symbols::MAX_UPLOADS_IN_FLIGHT)
        .map(|_| {
            w.api
                .body_limit(&head, 1000)
                .expect("room")
                .1
                .expect("a slot")
        })
        .collect();
    let refused = w.api.body_limit(&head, 1000).unwrap_err();
    assert_eq!(refused.status, 429);
    assert!(refused.retry_after.is_some());
    // Reads and other requests are not held up by uploads.
    assert!(
        w.api
            .body_limit(&request("GET", &symbols, &admin, None), 1000)
            .is_ok()
    );

    drop(held);
    assert!(
        w.api.body_limit(&head, 1000).is_ok(),
        "the places came back"
    );
}

#[test]
fn issue_states_are_read_by_viewers_and_changed_by_admins_within_their_site() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);
    let fingerprint = "0123456789abcdef0123456789abcdef";
    let path = format!("sites/{}/issues/{fingerprint}", w.shop);

    // Nobody touched it: unresolved, with no row behind it.
    let fresh = w.call("GET", &path, &viewer, None);
    assert_eq!(
        (
            fresh.status,
            fresh.body["state"].as_str(),
            fresh.body["regressions"].as_u64()
        ),
        (200, Some("unresolved"), Some(0))
    );
    assert!(
        w.call("GET", &format!("sites/{}/issues", w.shop), &viewer, None)
            .body["issues"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    // A viewer reads and cannot change.
    let resolve = json!({ "state": "resolved", "release": "1.2.0" });
    assert_eq!(w.status("PUT", &path, &viewer, Some(resolve.clone())), 403);
    let done = w.call("PUT", &path, &admin, Some(resolve));
    assert_eq!(done.status, 200, "{:?}", done.body);
    assert_eq!(
        (
            done.body["state"].as_str(),
            done.body["resolved_release"].as_str()
        ),
        (Some("resolved"), Some("1.2.0"))
    );
    let listed = ApiRequest {
        query: Some("state=resolved".to_owned()),
        ..request("GET", &format!("sites/{}/issues", w.shop), &viewer, None)
    };
    assert_eq!(
        w.api.respond(&listed).body["issues"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // Bad input is refused, not stored.
    for body in [
        json!({ "state": "resolved" }),
        json!({ "state": "resolved", "release": "not a release" }),
        json!({ "state": "ignored", "release": "1.0" }),
        json!({ "state": "fixed" }),
        json!({ "state": "ignored", "extra": 1 }),
    ] {
        assert_eq!(
            w.status("PUT", &path, &admin, Some(body.clone())),
            400,
            "{body}"
        );
    }
    for bad in ["nope", "0123456789ABCDEF0123456789ABCDEF"] {
        assert_eq!(
            w.status(
                "GET",
                &format!("sites/{}/issues/{bad}", w.shop),
                &admin,
                None
            ),
            400
        );
    }
    // The first resolution is untouched by the refusals.
    assert_eq!(
        w.call("GET", &path, &viewer, None).body["state"],
        "resolved"
    );

    // Another organization's site is not reachable, and not revealed.
    let foreign = format!("sites/{}/issues/{fingerprint}", w.elsewhere);
    assert_eq!(w.status("GET", &foreign, &admin, None), 404);
    assert_eq!(
        w.status("PUT", &foreign, &admin, Some(json!({ "state": "ignored" }))),
        404
    );
    assert_eq!(w.status("GET", &path, &Caller::nobody(), None), 401);
}

#[test]
fn issue_states_follow_the_token_that_asks_and_come_in_pages() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let reader = token_for(&w, &admin, w.shop);
    let other_site = token_for(&w, &admin, w.blog);
    let uploader = upload_token_for(&w, &admin, w.shop);
    let fingerprint = |n: u32| format!("{n:032x}");
    let path = |n: u32| format!("sites/{}/issues/{}", w.shop, fingerprint(n));
    let list = format!("sites/{}/issues", w.shop);

    // A read token reads its site's issues and changes nothing.
    assert_eq!(w.status("GET", &path(1), &reader, None), 200);
    assert_eq!(w.status("GET", &list, &reader, None), 200);
    assert_eq!(
        w.status(
            "PUT",
            &path(1),
            &reader,
            Some(json!({ "state": "ignored" }))
        ),
        403
    );
    // Another site's token and an upload token do not read these.
    // The site is not revealed to another site's token (404); an upload token
    // of the site is known but may only upload (403).
    for (name, caller, status) in [("other site", &other_site, 404), ("upload", &uploader, 403)] {
        let put = w.status("PUT", &path(1), caller, Some(json!({ "state": "ignored" })));
        let got = (
            w.status("GET", &path(1), caller, None),
            w.status("GET", &list, caller, None),
            put,
        );
        assert_eq!(got, (status, status, status), "{name}");
    }

    for n in 1..=5 {
        assert_eq!(
            w.status("PUT", &path(n), &admin, Some(json!({ "state": "ignored" }))),
            200
        );
    }
    let page = |query: &str| {
        let request = ApiRequest {
            query: Some(query.to_owned()),
            ..request("GET", &list, &reader, None)
        };
        w.api.respond(&request)
    };
    let first = page("limit=2");
    assert_eq!(first.body["issues"].as_array().map(Vec::len), Some(2));
    let next = first.body["next"].as_str().expect("a cursor").to_owned();
    assert_eq!(next, fingerprint(2));
    let second = page(&format!("limit=2&after={next}"));
    assert_eq!(second.body["issues"][0]["fingerprint"], fingerprint(3));
    let last = page(&format!("limit=2&after={}", fingerprint(4)));
    assert_eq!(last.body["issues"].as_array().map(Vec::len), Some(1));
    assert!(last.body["next"].is_null(), "a short page is the last");
    for bad in ["limit=0", "limit=1001", "limit=x", "after=nope"] {
        assert_eq!(page(bad).status, 400, "{bad}");
    }
}

#[test]
fn performance_sampling_is_read_set_and_unset_within_the_site() {
    let w = world();
    let admin = login(&w, "admin@example.com", &[(w.acme, Role::Admin)]);
    let viewer = login(&w, "viewer@example.com", &[(w.acme, Role::Viewer)]);
    let path = format!("sites/{}/sampling", w.shop);

    let before = w.call("GET", &path, &viewer, None);
    assert_eq!(
        (
            before.status,
            before.body["performance"].as_f64(),
            before.body["own"].as_bool()
        ),
        (200, Some(1.0), Some(false))
    );
    // A viewer reads and cannot change.
    assert_eq!(
        w.status("PUT", &path, &viewer, Some(json!({ "performance": 0.5 }))),
        403
    );
    let set = w.call("PUT", &path, &admin, Some(json!({ "performance": 0.25 })));
    assert_eq!(set.status, 200, "{:?}", set.body);
    assert_eq!(
        (set.body["performance"].as_f64(), set.body["own"].as_bool()),
        (Some(0.25), Some(true))
    );
    assert_eq!(
        w.call("GET", &path, &viewer, None).body["performance"],
        0.25
    );
    // The change is part of what the ingest cache loads.
    assert_eq!(
        w.meta.snapshot().expect("snapshot").sampling,
        [(w.shop, 250_000)]
    );
    // Another site is untouched.
    assert_eq!(
        w.call("GET", &format!("sites/{}/sampling", w.blog), &admin, None)
            .body["own"],
        false
    );
    for bad in [
        json!({ "performance": 0 }),
        json!({ "performance": 1.5 }),
        json!({ "performance": -1 }),
        json!({ "performance": 0.1234567 }),
        json!({ "performance": "half" }),
        json!({ "errors": 0.5 }),
        json!({ "performance": 0.5, "errors": 0.5 }),
    ] {
        assert_eq!(
            w.status("PUT", &path, &admin, Some(bad.clone())),
            400,
            "{bad}"
        );
    }
    assert_eq!(w.call("GET", &path, &admin, None).body["performance"], 0.25);
    assert_eq!(w.status("DELETE", &path, &admin, None), 204);
    assert_eq!(w.call("GET", &path, &admin, None).body["performance"], 1.0);
    assert_eq!(
        w.status(
            "PUT",
            &format!("sites/{}/sampling", w.elsewhere),
            &admin,
            Some(json!({ "performance": 0.5 }))
        ),
        404
    );
}
