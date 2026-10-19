//! The state of issues over the API: reading what was decided about an
//! error group and resolving, ignoring or reopening it. Counts and
//! timelines of the errors themselves are not here.

use dionaea_meta::{IssueIndex, IssueRecord, IssueState, MAX_ISSUES_LISTED, is_fingerprint};

/// Issues in a page when the caller names no limit.
const DEFAULT_PAGE: usize = 100;

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueStateBody {
    state: String,
    #[serde(default)]
    release: Option<String>,
}

fn issue_json(issue: &IssueRecord) -> Value {
    json!({
        "fingerprint": issue.fingerprint,
        "state": issue.state.as_str(),
        "resolved_release": issue.resolved_release,
        "resolved_at": issue.resolved_at,
        "regressed_release": issue.regressed_release,
        "regressed_at": issue.regressed_at,
        "regressions": issue.regressions,
        "updated_at": issue.updated_at,
    })
}

fn untouched_json(fingerprint: &str) -> Value {
    json!({
        "fingerprint": fingerprint,
        "state": IssueState::Unresolved.as_str(),
        "resolved_release": Value::Null,
        "resolved_at": Value::Null,
        "regressed_release": Value::Null,
        "regressed_at": Value::Null,
        "regressions": 0,
        "updated_at": Value::Null,
    })
}

fn fingerprint_of(text: &str) -> Result<&str, ApiError> {
    if is_fingerprint(text) {
        Ok(text)
    } else {
        Err(ApiError::bad(
            "a fingerprint is 32 lowercase hexadecimal characters",
        ))
    }
}

impl Api {
    pub(super) fn issues(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Read)?;
        let state = match value_of(req.query.as_deref(), "state") {
            Some(text) => Some(
                IssueState::parse(text)
                    .ok_or_else(|| ApiError::bad("the state is unresolved, resolved or ignored"))?,
            ),
            None => None,
        };
        let after = match value_of(req.query.as_deref(), "after") {
            Some(text) => Some(fingerprint_of(text)?),
            None => None,
        };
        let limit = match value_of(req.query.as_deref(), "limit") {
            Some(text) => text
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_ISSUES_LISTED).contains(n))
                .ok_or_else(|| {
                    ApiError::bad(format!("`limit` is a number from 1 to {MAX_ISSUES_LISTED}"))
                })?,
            None => DEFAULT_PAGE,
        };
        let page = self.meta.issues(site.id, state, after, limit)?;
        // A full page may be the last; `next` then leads to an empty one.
        let next = (page.len() == limit)
            .then(|| page.last().map(|issue| issue.fingerprint.clone()))
            .flatten();
        let issues: Vec<Value> = page.iter().map(issue_json).collect();
        Ok(ApiResponse::ok(json!({ "issues": issues, "next": next })))
    }

    /// An issue nobody touched has no row: it is unresolved.
    pub(super) fn issue(&self, principal: &Principal, site: u32, fingerprint: &str) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Read)?;
        let fingerprint = fingerprint_of(fingerprint)?;
        Ok(ApiResponse::ok(
            match self.meta.issue(site.id, fingerprint)? {
                Some(issue) => issue_json(&issue),
                None => untouched_json(fingerprint),
            },
        ))
    }

    pub(super) fn set_issue(
        &self,
        req: &ApiRequest,
        principal: &Principal,
        site: u32,
        fingerprint: &str,
    ) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let fingerprint = fingerprint_of(fingerprint)?;
        let body: IssueStateBody = Self::json(req)?;
        let state = IssueState::parse(&body.state)
            .ok_or_else(|| ApiError::bad("the state is unresolved, resolved or ignored"))?;
        let issue =
            self.meta
                .set_issue_state(site.id, fingerprint, state, body.release.as_deref())?;
        Self::audit(principal, "set_issue_state", Some(site.id));
        Ok(ApiResponse::ok(issue_json(&issue)))
    }
}
