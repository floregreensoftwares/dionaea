//! The HTTP API for members and scripts: logging in, administering
//! organizations, sites, keys, schemas, retention and erasure, and reading
//! reports. Everything is JSON under `/api/v1/`.
//!
//! This module is plain functions over [`ApiRequest`] and [`ApiResponse`]; the
//! HTTP server in `api_http` only turns requests into the first and the second
//! into responses. It blocks (SQLite, argon2, ClickHouse), so the server runs
//! it on a blocking thread.
//!
//! **Who may do what.** A request carries either a session cookie (a member)
//! or `Authorization: Bearer <read token>` (a script). Every route checks the
//! caller against the organization that owns the site in the path:
//!
//! - a caller with no standing there gets `404`, the same as for a site that
//!   does not exist, so ids cannot be probed;
//! - a caller who can see it but lacks the role gets `403`;
//! - a read token can only `POST .../query` on the site it was issued for.
//!
//! State-changing requests also refuse an `Origin` that is not this server's
//! own `Host`, on top of the `SameSite=Lax` cookie.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dionaea_artifacts::ArtifactStore;
use dionaea_auth::{Auth, AuthError, Denied, Permission, Principal, Scope, cookie_from_header};
use dionaea_core::Schema;
use dionaea_meta::{
    AccountStore, ErasureRequest, ErasureState, ErasureSubject, MAX_RETENTION_DAYS,
    MIN_RETENTION_DAYS, MetaError, MetaStore, RetentionFamily, Role, Site, SqliteMeta,
    WriteKeyInfo,
};
use dionaea_query::{Limits, Query, QueryError, is_time_zone_shape};
use dionaea_store::Store;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::{error, info};

/// Reads running at once. More wait for none: they are refused with `429`, so
/// a burst of reports cannot queue up work for ClickHouse without bound.
const MAX_QUERIES_IN_FLIGHT: usize = 8;
/// Most erasure requests one listing returns (the newest).
const MAX_LISTED_ERASURES: u32 = 500;

#[derive(Clone)]
pub(crate) struct ApiRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
    pub(crate) cookie: Option<String>,
    pub(crate) authorization: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
    /// Where the request came from, for the login limiter.
    pub(crate) client: String,
}

#[derive(Debug)]
pub(crate) struct ApiResponse {
    pub(crate) status: u16,
    pub(crate) body: Value,
    pub(crate) set_cookie: Option<String>,
    pub(crate) retry_after: Option<i64>,
}

impl ApiResponse {
    fn ok(body: Value) -> Self {
        Self::with(200, body)
    }

    fn with(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            set_cookie: None,
            retry_after: None,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: &'static str,
    message: String,
    extra: Option<Value>,
    retry_after: Option<i64>,
    set_cookie: Option<String>,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: None,
            retry_after: None,
            set_cookie: None,
        }
    }

    fn not_found() -> Self {
        Self::new(404, "not_found", "not found")
    }

    fn forbidden() -> Self {
        Self::new(403, "forbidden", "you may not do that")
    }

    fn unauthorized() -> Self {
        Self::new(401, "unauthorized", "log in, or send a read token")
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    fn internal() -> Self {
        Self::new(500, "internal", "something went wrong on the server")
    }

    fn into_response(self) -> ApiResponse {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let (Some(extra), Some(map)) = (self.extra, error.as_object_mut()) {
            map.insert("details".to_owned(), extra);
        }
        ApiResponse {
            status: self.status,
            body: json!({ "error": error }),
            set_cookie: self.set_cookie,
            retry_after: self.retry_after,
        }
    }
}

impl From<Denied> for ApiError {
    fn from(_: Denied) -> Self {
        Self::forbidden()
    }
}

impl From<MetaError> for ApiError {
    fn from(error: MetaError) -> Self {
        match error {
            MetaError::NotFound(what) => Self::new(404, "not_found", format!("{what} not found")),
            MetaError::Invalid(message) => Self::new(400, "invalid_request", message),
            MetaError::Conflict(message) => Self::new(409, "conflict", message),
            MetaError::InvalidSchema(diagnostics) => {
                let mut api = Self::new(422, "invalid_schema", "the schema file has problems");
                api.extra = Some(Value::Array(
                    diagnostics
                        .iter()
                        .map(
                            |d| json!({ "line": d.line, "column": d.column, "message": d.message }),
                        )
                        .collect(),
                ));
                api
            }
            MetaError::Breaking(changes) => {
                let mut api = Self::new(
                    409,
                    "breaking_change",
                    "the schema breaks compatibility with the active version",
                );
                api.extra = Some(changes_json(&changes));
                api
            }
            MetaError::Unchanged => Self::new(
                409,
                "unchanged",
                "the schema is the same as the latest version",
            ),
            MetaError::Corrupt(_) | MetaError::Storage(_) => {
                error!(%error, "metadata failure while serving the API");
                Self::internal()
            }
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::InvalidCredentials => Self::new(
                401,
                "invalid_credentials",
                "the address or the password is wrong",
            ),
            AuthError::TooManyAttempts { retry_after_secs } => {
                let mut api = Self::new(429, "too_many_attempts", "too many failed attempts");
                api.retry_after = Some(retry_after_secs);
                api
            }
            AuthError::Busy => {
                let mut api = Self::new(
                    429,
                    "busy",
                    "too many passwords are being checked; retry shortly",
                );
                api.retry_after = Some(1);
                api
            }
            AuthError::WeakPassword(why) => {
                Self::new(400, "weak_password", format!("the password needs {why}"))
            }
            AuthError::Meta(error) => error.into(),
            AuthError::Hash(_) | AuthError::Random(_) => {
                error!(%error, "authentication failure while serving the API");
                Self::internal()
            }
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        let status = match &error {
            QueryError::TooSlow => 504,
            QueryError::TooManyRows => 422,
            QueryError::Unavailable(message) => {
                error!(%message, "a report could not be read");
                503
            }
            _ => 400,
        };
        let message = match &error {
            // The store's own message can name hosts and users.
            QueryError::Unavailable(_) => "the analytics store is unavailable".to_owned(),
            other => other.to_string(),
        };
        Self::new(status, error.code(), message)
    }
}

fn changes_json(changes: &[dionaea_core::Change]) -> Value {
    Value::Array(
        changes
            .iter()
            .map(|c| {
                json!({
                    "path": c.path,
                    "severity": c.severity().to_string(),
                    "description": c.kind.to_string(),
                })
            })
            .collect(),
    )
}

type ApiResult = Result<ApiResponse, ApiError>;

/// Active schemas by site, with the metadata generation they were read at.
type SchemaCache = HashMap<u32, (u64, Arc<Vec<Schema>>)>;

/// Counts a read in flight and releases it when dropped.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn acquire(counter: &'a AtomicUsize, max: usize) -> Option<Self> {
        let before = counter.fetch_add(1, Ordering::AcqRel);
        if before >= max {
            counter.fetch_sub(1, Ordering::AcqRel);
            None
        } else {
            Some(Self(counter))
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct Api {
    meta: Arc<SqliteMeta>,
    auth: Arc<Auth>,
    store: Store,
    limits: Limits,
    default_retention_days: Box<dyn Fn() -> u32 + Send + Sync>,
    /// Active schemas by site and the metadata generation they were read at.
    schemas: Mutex<SchemaCache>,
    in_flight: AtomicUsize,
    artifacts: Arc<dyn ArtifactStore>,
    /// Symbol uploads being received (taken before the body is read).
    uploads: Arc<AtomicUsize>,
    /// One upload at a time is checked against the quota and recorded.
    upload_lock: Mutex<()>,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    current: String,
    new: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMemberBody {
    email: String,
    name: String,
    password: String,
    role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleBody {
    /// `null` removes the member from the organization.
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeZoneBody {
    time_zone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewKeyBody {
    label: String,
    #[serde(default)]
    origins: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTokenBody {
    label: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionBody {
    days: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErasureBody {
    user_id: Option<String>,
    device_id: Option<String>,
}

fn id_of(segment: &str) -> Result<u32, ApiError> {
    segment.parse().map_err(|_| ApiError::not_found())
}

fn flag(query: Option<&str>, name: &str) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
            .any(|(k, v)| k == name && matches!(v, "" | "1" | "true"))
    })
}

fn value_of<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn member_json(member: &dionaea_meta::Member) -> Value {
    json!({
        "id": member.id,
        "email": member.email,
        "name": member.name,
        "disabled": member.disabled,
    })
}

fn site_json(site: &Site) -> Value {
    json!({
        "id": site.id,
        "organization_id": site.organization_id,
        "name": site.name,
        "time_zone": site.time_zone,
    })
}

fn key_json(key: &WriteKeyInfo) -> Value {
    json!({
        "key": key.key,
        "site_id": key.site_id,
        "label": key.label,
        "origins": key.origins,
        "created_at": key.created_at,
        "revoked_at": key.revoked_at,
    })
}

fn token_json(token: &dionaea_meta::ReadTokenInfo) -> Value {
    json!({
        "id": token.id,
        "site_id": token.site_id,
        "label": token.label,
        "created_at": token.created_at,
        "revoked_at": token.revoked_at,
    })
}

fn erasure_json(request: &ErasureRequest) -> Value {
    json!({
        "id": request.id,
        "site_id": request.site_id,
        "kind": if request.is_device { "device" } else { "user" },
        "subject_hash": request.user_hash,
        "state": request.state.as_str(),
        "requested_by": request.requested_by,
        "requested_at": request.requested_at,
        "hidden_at": request.hidden_at,
        "completed_at": request.completed_at,
        "failed_at": request.failed_at,
        "devices_blocked": request.anon_count,
        "attempts": request.attempts,
        "detail": request.detail,
    })
}

fn principal_json(principal: &Principal) -> Value {
    match principal {
        Principal::Member {
            member,
            memberships,
        } => json!({
            "type": "member",
            "member": member_json(member),
            "memberships": memberships
                .iter()
                .map(|m| json!({ "organization_id": m.organization_id, "role": m.role.as_str() }))
                .collect::<Vec<_>>(),
        }),
        Principal::ReadToken(token) => json!({
            "type": "read_token",
            "read_token": token_json(token),
        }),
        Principal::UploadToken(token) => json!({
            "type": "upload_token",
            "upload_token": token_json(token),
        }),
    }
}

impl Api {
    pub(crate) fn new(
        meta: Arc<SqliteMeta>,
        auth: Arc<Auth>,
        store: Store,
        limits: Limits,
        artifacts: Arc<dyn ArtifactStore>,
        default_retention_days: impl Fn() -> u32 + Send + Sync + 'static,
    ) -> Self {
        Self {
            artifacts,
            uploads: Arc::new(AtomicUsize::new(0)),
            upload_lock: Mutex::new(()),
            meta,
            auth,
            store,
            limits,
            default_retention_days: Box::new(default_retention_days),
            schemas: Mutex::new(HashMap::new()),
            in_flight: AtomicUsize::new(0),
        }
    }

    pub(crate) fn respond(&self, request: &ApiRequest) -> ApiResponse {
        match self.route(request) {
            Ok(response) => response,
            Err(error) => error.into_response(),
        }
    }

    fn route(&self, req: &ApiRequest) -> ApiResult {
        let path = req
            .path
            .strip_prefix("/api/v1/")
            .ok_or_else(ApiError::not_found)?;
        let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(ApiError::not_found());
        }
        let changes_state = !matches!(req.method.as_str(), "GET" | "HEAD");
        if changes_state && !same_origin(req) {
            return Err(ApiError::new(
                403,
                "cross_origin",
                "the request comes from another origin",
            ));
        }
        let method = req.method.as_str();
        if let ("POST", ["login"]) = (method, segments.as_slice()) {
            return self.login(req);
        }
        let principal = self.principal(req)?;
        let p = &principal;
        match (method, segments.as_slice()) {
            ("POST", ["logout"]) => self.logout(req),
            ("GET", ["me"]) => Ok(ApiResponse::ok(principal_json(p))),
            ("PUT", ["me", "password"]) => self.change_password(req, p),

            ("GET", ["organizations"]) => self.organizations(p),
            ("POST", ["organizations"]) => self.create_organization(req, p),
            ("GET", ["organizations", org, "sites"]) => self.org_sites(p, id_of(org)?),
            ("POST", ["organizations", org, "sites"]) => self.create_site(req, p, id_of(org)?),
            ("GET", ["organizations", org, "members"]) => self.members(p, id_of(org)?),
            ("POST", ["organizations", org, "members"]) => self.add_member(req, p, id_of(org)?),
            ("PUT", ["organizations", org, "members", member]) => {
                self.set_member_role(req, p, id_of(org)?, id_of(member)?)
            }

            ("GET", ["sites", site]) => {
                let site = self.site_access(p, id_of(site)?, Permission::Read)?;
                Ok(ApiResponse::ok(site_json(&site)))
            }
            ("PUT", ["sites", site, "time-zone"]) => self.set_time_zone(req, p, id_of(site)?),
            ("GET", ["sites", site, "keys"]) => self.keys(p, id_of(site)?),
            ("POST", ["sites", site, "keys"]) => self.issue_key(req, p, id_of(site)?),
            ("DELETE", ["sites", site, "keys", key]) => self.revoke_key(p, id_of(site)?, key),
            ("GET", ["sites", site, "schema", "versions"]) => self.schema_versions(p, id_of(site)?),
            ("POST", ["sites", site, "schema"]) => self.push_schema(req, p, id_of(site)?),
            ("POST", ["sites", site, "schema", "versions", version, "retire"]) => {
                self.retire_schema(p, id_of(site)?, id_of(version)?)
            }
            ("GET", ["sites", site, "tokens"]) => self.tokens(p, id_of(site)?),
            ("POST", ["sites", site, "tokens"]) => self.issue_token(req, p, id_of(site)?),
            ("DELETE", ["sites", site, "tokens", token]) => {
                self.revoke_token(p, id_of(site)?, id_of(token)?)
            }
            ("GET", ["sites", site, "retention"]) => self.retention(p, id_of(site)?),
            ("GET", ["sites", site, "retention", "preview"]) => {
                self.retention_preview(req, p, id_of(site)?)
            }
            ("PUT", ["sites", site, "retention"]) => self.set_retention(req, p, id_of(site)?),
            ("DELETE", ["sites", site, "retention"]) => self.unset_retention(p, id_of(site)?),
            ("GET", ["sites", site, "erasures"]) => self.erasures(req, p, id_of(site)?),
            ("POST", ["sites", site, "erasures"]) => self.request_erasure(req, p, id_of(site)?),
            ("GET", ["sites", site, "erasures", id]) => self.erasure(p, id_of(site)?, id),
            ("POST", ["sites", site, "erasures", id, "retry"]) => {
                self.retry_erasure(p, id_of(site)?, id)
            }
            ("GET", ["sites", site, "symbols"]) => self.symbols(req, p, id_of(site)?),
            ("POST", ["sites", site, "symbols"]) => self.upload_symbol(req, p, id_of(site)?),
            ("DELETE", ["sites", site, "symbols"]) => self.delete_symbols(req, p, id_of(site)?),
            ("GET", ["sites", site, "issues"]) => self.issues(req, p, id_of(site)?),
            ("GET", ["sites", site, "issues", fingerprint]) => {
                self.issue(p, id_of(site)?, fingerprint)
            }
            ("PUT", ["sites", site, "issues", fingerprint]) => {
                self.set_issue(req, p, id_of(site)?, fingerprint)
            }
            ("GET", ["sites", site, "upload-tokens"]) => self.upload_tokens(p, id_of(site)?),
            ("POST", ["sites", site, "upload-tokens"]) => {
                self.issue_upload_token(req, p, id_of(site)?)
            }
            ("DELETE", ["sites", site, "upload-tokens", token]) => {
                self.revoke_upload_token(p, id_of(site)?, id_of(token)?)
            }
            ("POST", ["sites", site, "query"]) => self.query(req, p, id_of(site)?),
            _ => Err(ApiError::not_found()),
        }
    }

    // ---- who is asking ----------------------------------------------------

    fn principal(&self, req: &ApiRequest) -> Result<Principal, ApiError> {
        let found = if let Some(header) = &req.authorization {
            let token = header
                .strip_prefix("Bearer ")
                .or_else(|| header.strip_prefix("bearer "))
                .ok_or_else(ApiError::unauthorized)?;
            let token = token.trim();
            match self.auth.authenticate_read_token(token)? {
                Some(principal) => Some(principal),
                None => self.auth.authenticate_upload_token(token)?,
            }
        } else if let Some(cookie) = req.cookie.as_deref().and_then(cookie_from_header) {
            self.auth.authenticate_session(cookie)?
        } else {
            None
        };
        found.ok_or_else(ApiError::unauthorized)
    }

    fn member_of(principal: &Principal) -> Result<&dionaea_meta::Member, ApiError> {
        principal.member().ok_or_else(ApiError::forbidden)
    }

    /// The site, if the caller may do `permission` on it. See the module
    /// documentation for which refusal is which.
    fn site_access(
        &self,
        principal: &Principal,
        site_id: u32,
        permission: Permission,
    ) -> Result<Site, ApiError> {
        let site = self.meta.site(site_id)?;
        let scope = Scope {
            organization_id: site.organization_id,
            site_id: Some(site.id),
        };
        match principal {
            Principal::Member { memberships, .. } => {
                if !memberships
                    .iter()
                    .any(|m| m.organization_id == site.organization_id)
                {
                    return Err(ApiError::not_found());
                }
            }
            Principal::ReadToken(token) | Principal::UploadToken(token) => {
                if token.site_id != site.id {
                    return Err(ApiError::not_found());
                }
            }
        }
        principal.authorize(permission, scope)?;
        Ok(site)
    }

    fn org_access(
        &self,
        principal: &Principal,
        organization_id: u32,
        permission: Permission,
    ) -> Result<(), ApiError> {
        match principal {
            Principal::Member { memberships, .. } => {
                if !memberships
                    .iter()
                    .any(|m| m.organization_id == organization_id)
                {
                    return Err(ApiError::not_found());
                }
            }
            Principal::ReadToken(_) | Principal::UploadToken(_) => {
                return Err(ApiError::forbidden());
            }
        }
        principal.authorize(
            permission,
            Scope {
                organization_id,
                site_id: None,
            },
        )?;
        Ok(())
    }

    fn json<T: DeserializeOwned>(req: &ApiRequest) -> Result<T, ApiError> {
        let is_json = req.content_type.as_deref().is_some_and(|t| {
            t.trim()
                .to_ascii_lowercase()
                .starts_with("application/json")
        });
        if !is_json {
            return Err(ApiError::new(
                415,
                "unsupported_media_type",
                "send the body as application/json",
            ));
        }
        serde_json::from_slice(&req.body).map_err(|e| ApiError::bad(format!("the body: {e}")))
    }

    fn audit(principal: &Principal, action: &str, site: Option<u32>) {
        info!(actor = %principal.describe(), action, site, "admin action");
    }

    // ---- login and account ------------------------------------------------

    fn login(&self, req: &ApiRequest) -> ApiResult {
        let body: LoginBody = Self::json(req)?;
        let session = self.auth.login(&body.email, &body.password, &req.client)?;
        let memberships = self.meta.memberships(session.member.id)?;
        let mut response = ApiResponse::ok(principal_json(&Principal::Member {
            member: session.member.clone(),
            memberships,
        }));
        response.set_cookie = Some(self.auth.session_cookie(&session));
        Ok(response)
    }

    fn logout(&self, req: &ApiRequest) -> ApiResult {
        if let Some(cookie) = req.cookie.as_deref().and_then(cookie_from_header) {
            self.auth.logout(cookie)?;
        }
        let mut response = ApiResponse::with(204, Value::Null);
        response.set_cookie = Some(self.auth.logout_cookie());
        Ok(response)
    }

    fn change_password(&self, req: &ApiRequest, principal: &Principal) -> ApiResult {
        let member = Self::member_of(principal)?;
        let body: PasswordBody = Self::json(req)?;
        self.auth
            .change_password(member, &body.current, &body.new, &req.client)?;
        Self::audit(principal, "change_password", None);
        let mut response = ApiResponse::with(204, Value::Null);
        // Every session ended, this one too.
        response.set_cookie = Some(self.auth.logout_cookie());
        Ok(response)
    }

    // ---- organizations and members ----------------------------------------

    fn organizations(&self, principal: &Principal) -> ApiResult {
        let member = Self::member_of(principal)?;
        let memberships = self.meta.memberships(member.id)?;
        let listed: Vec<Value> = self
            .meta
            .organizations()?
            .into_iter()
            .filter_map(|org| {
                memberships
                    .iter()
                    .find(|m| m.organization_id == org.id)
                    .map(|m| json!({ "id": org.id, "name": org.name, "role": m.role.as_str() }))
            })
            .collect();
        Ok(ApiResponse::ok(json!({ "organizations": listed })))
    }

    /// An owner of any organization may start another one and becomes its
    /// owner. The first organization and its owner come from the command line.
    fn create_organization(&self, req: &ApiRequest, principal: &Principal) -> ApiResult {
        let member = Self::member_of(principal)?;
        let owns_one = self
            .meta
            .memberships(member.id)?
            .iter()
            .any(|m| m.role == Role::Owner);
        if !owns_one {
            return Err(ApiError::forbidden());
        }
        let body: NameBody = Self::json(req)?;
        let org = self.meta.create_organization(&body.name)?;
        self.meta.set_role(member.id, org.id, Some(Role::Owner))?;
        Self::audit(principal, "create_organization", None);
        Ok(ApiResponse::with(
            201,
            json!({ "id": org.id, "name": org.name, "role": "owner" }),
        ))
    }

    fn org_sites(&self, principal: &Principal, org: u32) -> ApiResult {
        self.org_access(principal, org, Permission::Read)?;
        let sites: Vec<Value> = self
            .meta
            .sites()?
            .iter()
            .filter(|s| s.organization_id == org)
            .map(site_json)
            .collect();
        Ok(ApiResponse::ok(json!({ "sites": sites })))
    }

    fn create_site(&self, req: &ApiRequest, principal: &Principal, org: u32) -> ApiResult {
        self.org_access(principal, org, Permission::Manage)?;
        let body: NameBody = Self::json(req)?;
        let site = self.meta.create_site(org, &body.name)?;
        Self::audit(principal, "create_site", Some(site.id));
        Ok(ApiResponse::with(201, site_json(&site)))
    }

    fn members(&self, principal: &Principal, org: u32) -> ApiResult {
        self.org_access(principal, org, Permission::Manage)?;
        let members: Vec<Value> = self
            .meta
            .organization_members(org)?
            .iter()
            .map(|m| {
                let mut value = member_json(&m.member);
                if let Some(map) = value.as_object_mut() {
                    map.insert("role".to_owned(), json!(m.role.as_str()));
                }
                value
            })
            .collect();
        Ok(ApiResponse::ok(json!({ "members": members })))
    }

    fn add_member(&self, req: &ApiRequest, principal: &Principal, org: u32) -> ApiResult {
        self.org_access(principal, org, Permission::ManageMembers)?;
        let body: NewMemberBody = Self::json(req)?;
        let role = Role::parse(&body.role)
            .ok_or_else(|| ApiError::bad("the role is viewer, admin or owner"))?;
        let member =
            self.auth
                .create_member_in(&body.email, &body.name, &body.password, org, role)?;
        Self::audit(principal, "add_member", None);
        let mut value = member_json(&member);
        if let Some(map) = value.as_object_mut() {
            map.insert("role".to_owned(), json!(role.as_str()));
        }
        Ok(ApiResponse::with(201, value))
    }

    fn set_member_role(
        &self,
        req: &ApiRequest,
        principal: &Principal,
        org: u32,
        member: u32,
    ) -> ApiResult {
        self.org_access(principal, org, Permission::ManageMembers)?;
        let body: RoleBody = Self::json(req)?;
        let role = match body.role.as_deref() {
            Some(text) => Some(
                Role::parse(text)
                    .ok_or_else(|| ApiError::bad("the role is viewer, admin or owner"))?,
            ),
            None => None,
        };
        let belongs = self
            .meta
            .organization_members(org)?
            .iter()
            .any(|m| m.member.id == member);
        if !belongs {
            return Err(ApiError::not_found());
        }
        self.meta.set_role(member, org, role)?;
        Self::audit(principal, "set_member_role", None);
        Ok(ApiResponse::with(204, Value::Null))
    }

    // ---- sites ------------------------------------------------------------

    fn set_time_zone(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: TimeZoneBody = Self::json(req)?;
        if !is_time_zone_shape(&body.time_zone) {
            return Err(ApiError::bad("the time zone name is not valid"));
        }
        match self.store.time_zone_known(&body.time_zone) {
            Ok(true) => {}
            Ok(false) => return Err(ApiError::bad("the time zone is not known")),
            Err(error) => {
                error!(%error, "cannot check a time zone");
                return Err(ApiError::new(
                    503,
                    "unavailable",
                    "the analytics store is unavailable",
                ));
            }
        }
        self.meta.set_site_time_zone(site.id, &body.time_zone)?;
        Self::audit(principal, "set_time_zone", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    fn keys(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let keys: Vec<Value> = self
            .meta
            .write_keys(site.id)?
            .iter()
            .map(key_json)
            .collect();
        Ok(ApiResponse::ok(json!({ "keys": keys })))
    }

    fn issue_key(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: NewKeyBody = Self::json(req)?;
        let key = self
            .meta
            .issue_write_key(site.id, &body.label, &body.origins)?;
        Self::audit(principal, "issue_write_key", Some(site.id));
        Ok(ApiResponse::with(201, key_json(&key)))
    }

    fn revoke_key(&self, principal: &Principal, site: u32, key: &str) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        if !self.meta.write_keys(site.id)?.iter().any(|k| k.key == key) {
            return Err(ApiError::not_found());
        }
        self.meta.revoke_write_key(key)?;
        Self::audit(principal, "revoke_write_key", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    fn schema_versions(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let versions: Vec<Value> = self
            .meta
            .schema_versions(site.id)?
            .iter()
            .map(|v| {
                json!({
                    "version": v.version,
                    "created_at": v.created_at,
                    "retired_at": v.retired_at,
                })
            })
            .collect();
        Ok(ApiResponse::ok(json!({ "versions": versions })))
    }

    /// The body is the schema file as it is, not wrapped in JSON.
    fn push_schema(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let source = std::str::from_utf8(&req.body)
            .map_err(|_| ApiError::bad("the schema file is not valid UTF-8"))?;
        if flag(req.query.as_deref(), "dry_run") {
            let changes = self.meta.diff_schema(site.id, source)?;
            return Ok(ApiResponse::ok(
                json!({ "changes": changes_json(&changes) }),
            ));
        }
        let published = self.meta.publish_schema(site.id, source)?;
        Self::audit(principal, "publish_schema", Some(site.id));
        Ok(ApiResponse::with(
            201,
            json!({
                "version": published.version,
                "changes": changes_json(&published.changes),
            }),
        ))
    }

    fn retire_schema(&self, principal: &Principal, site: u32, version: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        self.meta.retire_schema_version(site.id, version)?;
        Self::audit(principal, "retire_schema_version", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    fn tokens(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let tokens: Vec<Value> = self
            .meta
            .read_tokens(site.id)?
            .iter()
            .map(token_json)
            .collect();
        Ok(ApiResponse::ok(json!({ "tokens": tokens })))
    }

    /// The secret is in this response and nowhere else, ever.
    fn issue_token(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: NewTokenBody = Self::json(req)?;
        let created_by = principal.member().map(|m| m.id);
        let (info, secret) = self
            .auth
            .issue_read_token(site.id, &body.label, created_by)?;
        Self::audit(principal, "issue_read_token", Some(site.id));
        let mut value = token_json(&info);
        if let Some(map) = value.as_object_mut() {
            map.insert("token".to_owned(), json!(secret));
        }
        Ok(ApiResponse::with(201, value))
    }

    fn revoke_token(&self, principal: &Principal, site: u32, token: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        self.meta.revoke_read_token(site.id, token)?;
        Self::audit(principal, "revoke_read_token", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    // ---- retention and erasure --------------------------------------------

    fn retention(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let own = self
            .meta
            .retentions()?
            .into_iter()
            .find(|r| r.site_id == site.id && r.family == RetentionFamily::Analytics)
            .map(|r| r.days);
        let default = (self.default_retention_days)();
        Ok(ApiResponse::ok(json!({
            "family": RetentionFamily::Analytics.as_str(),
            "days": own.unwrap_or(default),
            "own": own.is_some(),
            "default_days": default,
        })))
    }

    /// What the next retention run would remove from this site at `days` (or at
    /// the days it keeps now), counted and not deleted: use it before lowering
    /// the number, because a run cannot be undone.
    fn retention_preview(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let days = match value_of(req.query.as_deref(), "days") {
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|d| (MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(d))
                .ok_or_else(|| {
                    ApiError::bad(format!(
                        "days is a number from {MIN_RETENTION_DAYS} to {MAX_RETENTION_DAYS}"
                    ))
                })?,
            None => self
                .meta
                .retentions()?
                .into_iter()
                .find(|r| r.site_id == site.id && r.family == RetentionFamily::Analytics)
                .map_or_else(|| (self.default_retention_days)(), |r| r.days),
        };
        let cutoff = crate::collect::system_clock().saturating_sub(u64::from(days) * 86_400_000);
        let counted = self.store.count_expired(site.id, cutoff).map_err(|error| {
            error!(%error, "cannot count what retention would remove");
            ApiError::new(503, "unavailable", "the analytics store is unavailable")
        })?;
        let tables: Vec<Value> = counted
            .iter()
            .map(|e| json!({ "table": e.table, "rows": e.rows }))
            .collect();
        Ok(ApiResponse::ok(json!({
            "days": days,
            "cutoff_ms": cutoff,
            "tables": tables,
        })))
    }

    fn set_retention(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: RetentionBody = Self::json(req)?;
        self.meta
            .set_retention(site.id, RetentionFamily::Analytics, Some(body.days))?;
        Self::audit(principal, "set_retention", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    fn unset_retention(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        self.meta
            .set_retention(site.id, RetentionFamily::Analytics, None)?;
        Self::audit(principal, "unset_retention", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    fn erasures(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let state = match value_of(req.query.as_deref(), "state") {
            Some(text) => Some(ErasureState::parse(text).ok_or_else(|| {
                ApiError::bad("the state is requested, hidden, complete or failed")
            })?),
            None => None,
        };
        let requests: Vec<Value> = self
            .meta
            .site_erasure_requests(site.id, state, MAX_LISTED_ERASURES)?
            .iter()
            .map(erasure_json)
            .collect();
        Ok(ApiResponse::ok(json!({ "requests": requests })))
    }

    fn request_erasure(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: ErasureBody = Self::json(req)?;
        let subject = match (&body.user_id, &body.device_id) {
            (Some(user), None) => ErasureSubject::User(user),
            (None, Some(device)) => ErasureSubject::Device(device),
            _ => return Err(ApiError::bad("send either user_id or device_id")),
        };
        let by = principal.describe();
        let request = match subject {
            ErasureSubject::User(user) => self.meta.request_erasure(site.id, user, &by)?,
            ErasureSubject::Device(device) => {
                self.meta.request_device_erasure(site.id, device, &by)?
            }
        };
        Self::audit(principal, "request_erasure", Some(site.id));
        Ok(ApiResponse::with(202, erasure_json(&request)))
    }

    fn erasure_of(&self, site: &Site, id: &str) -> Result<ErasureRequest, ApiError> {
        let id: u64 = id.parse().map_err(|_| ApiError::not_found())?;
        let request = self.meta.erasure_request(id)?;
        if request.site_id == site.id {
            Ok(request)
        } else {
            Err(ApiError::not_found())
        }
    }

    fn erasure(&self, principal: &Principal, site: u32, id: &str) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        Ok(ApiResponse::ok(erasure_json(&self.erasure_of(&site, id)?)))
    }

    fn retry_erasure(&self, principal: &Principal, site: u32, id: &str) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let request = self.erasure_of(&site, id)?;
        let retried = self.meta.retry_erasure(request.id)?;
        Self::audit(principal, "retry_erasure", Some(site.id));
        Ok(ApiResponse::with(202, erasure_json(&retried)))
    }

    // ---- reports ----------------------------------------------------------

    fn schemas_of(&self, site_id: u32) -> Result<Arc<Vec<Schema>>, ApiError> {
        let generation = self.meta.generation()?;
        let mut cache = self.schemas.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((seen, schemas)) = cache.get(&site_id)
            && *seen == generation
        {
            return Ok(Arc::clone(schemas));
        }
        let (read_at, schemas) = self.meta.site_schemas(site_id)?;
        let schemas = Arc::new(schemas);
        cache.insert(site_id, (read_at, Arc::clone(&schemas)));
        Ok(schemas)
    }

    fn query(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Read)?;
        let query: Query = Self::json(req)?;
        let schemas = self.schemas_of(site.id)?;
        let _slot = InFlight::acquire(&self.in_flight, MAX_QUERIES_IN_FLIGHT).ok_or_else(|| {
            let mut busy = ApiError::new(
                429,
                "busy",
                "too many reports are being read; retry shortly",
            );
            busy.retry_after = Some(1);
            busy
        })?;
        let plan = dionaea_query::validate(
            dionaea_query::Site {
                id: site.id,
                time_zone: &site.time_zone,
                schemas: &schemas,
            },
            &query,
            &self.limits,
        )?;
        let answer = dionaea_query::run(&self.store, &plan, &self.limits)?;
        match serde_json::to_value(answer) {
            Ok(value) => Ok(ApiResponse::ok(value)),
            Err(error) => {
                error!(%error, "cannot encode a report");
                Err(ApiError::internal())
            }
        }
    }
}

/// An `Origin` header, when there is one, names this server's own `Host`.
fn same_origin(req: &ApiRequest) -> bool {
    match (&req.origin, &req.host) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin
            .split_once("://")
            .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host)),
        (Some(_), None) => false,
    }
}

#[path = "api_issues.rs"]
mod issues;

#[path = "api_symbols.rs"]
mod symbols;

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
