//! Symbol files over the API: uploading the source maps and mapping files of
//! a release, listing and removing them, and the upload tokens that let CI do
//! the first and nothing else.
//!
//! A file is read into memory (at most 64 MiB; at most four uploads at once, so
//! 256 MiB), checked, written to the artifact store and recorded in the index.
//! The token or session is checked **before** the body is read, so only
//! someone allowed to upload can make the server buffer a large body.

use dionaea_artifacts::ArtifactId;
use dionaea_meta::{SymbolFile, SymbolIndex, SymbolKind, is_release, is_symbol_name};

use super::*;

/// The largest symbol file.
pub(crate) const MAX_SYMBOL_BYTES: usize = 64 << 20;
/// The most symbol bytes one site keeps.
const MAX_SITE_SYMBOL_BYTES: u64 = 1 << 30;
/// Uploads being received at once on a node.
pub(crate) const MAX_UPLOADS_IN_FLIGHT: usize = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUploadTokenBody {
    label: String,
}

fn symbol_json(file: &SymbolFile) -> Value {
    json!({
        "release": file.release,
        "kind": file.kind.as_str(),
        "name": file.name,
        "size": file.size,
        "sha256": file.sha256,
        "uploaded_at": file.uploaded_at,
    })
}

/// The value of a query parameter, percent-decoded. `Ok(None)` when absent.
fn query_param(query: Option<&str>, name: &str) -> Result<Option<String>, ApiError> {
    let Some(raw) = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
    else {
        return Ok(None);
    };
    percent_decode(raw)
        .map(Some)
        .ok_or_else(|| ApiError::bad(format!("the parameter `{name}` is not valid")))
}

/// Decodes `%XX`; a `+` stays a plus, because releases contain one.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(pair, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A cheap check that the bytes are what the kind says, so a wrong upload is
/// found when it is made and not when an error needs it.
fn check_content(kind: SymbolKind, bytes: &[u8]) -> Result<(), ApiError> {
    match kind {
        SymbolKind::SourceMap => {
            // Valid JSON, read without building it, and an object.
            let first = bytes.iter().find(|b| !b.is_ascii_whitespace());
            let valid = first == Some(&b'{')
                && serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok();
            if valid {
                Ok(())
            } else {
                Err(ApiError::new(
                    422,
                    "invalid_symbol_file",
                    "a source map is a JSON object",
                ))
            }
        }
        SymbolKind::Proguard => {
            if !bytes.is_empty() && std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
                Ok(())
            } else {
                Err(ApiError::new(
                    422,
                    "invalid_symbol_file",
                    "a mapping file is non-empty UTF-8 text",
                ))
            }
        }
    }
}

impl Api {
    /// How large a body this request may have. For an upload of symbols the
    /// caller is checked first; for anything else the ordinary limit applies.
    pub(crate) fn body_limit(
        &self,
        req: &ApiRequest,
        ordinary: usize,
    ) -> Result<usize, ApiResponse> {
        let is_upload = req.method == "POST"
            && req
                .path
                .strip_prefix("/api/v1/sites/")
                .and_then(|rest| rest.strip_suffix("/symbols"))
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
        if !is_upload {
            return Ok(ordinary);
        }
        let check = || -> Result<(), ApiError> {
            if !same_origin(req) {
                return Err(ApiError::new(
                    403,
                    "cross_origin",
                    "the request comes from another origin",
                ));
            }
            let principal = self.principal(req)?;
            let site = req
                .path
                .trim_start_matches("/api/v1/sites/")
                .trim_end_matches("/symbols");
            self.site_access(&principal, id_of(site)?, Permission::Upload)?;
            Ok(())
        };
        check()
            .map(|()| MAX_SYMBOL_BYTES)
            .map_err(ApiError::into_response)
    }

    pub(super) fn symbols(&self, req: &ApiRequest, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let release = query_param(req.query.as_deref(), "release")?;
        let files: Vec<Value> = self
            .meta
            .symbol_files(site.id, release.as_deref())?
            .iter()
            .map(symbol_json)
            .collect();
        Ok(ApiResponse::ok(json!({
            "files": files,
            "bytes": self.meta.symbol_bytes(site.id)?,
            "limit_bytes": MAX_SITE_SYMBOL_BYTES,
        })))
    }

    pub(super) fn upload_symbol(
        &self,
        req: &ApiRequest,
        principal: &Principal,
        site: u32,
    ) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Upload)?;
        let release = query_param(req.query.as_deref(), "release")?
            .filter(|r| is_release(r))
            .ok_or_else(|| {
                ApiError::bad("`release` is 1 to 64 characters of letters, digits and `_.+/-`")
            })?;
        let kind = query_param(req.query.as_deref(), "kind")?
            .and_then(|k| SymbolKind::parse(&k))
            .ok_or_else(|| ApiError::bad("`kind` is sourcemap or proguard"))?;
        let name = query_param(req.query.as_deref(), "name")?
            .filter(|n| is_symbol_name(n))
            .ok_or_else(|| {
                ApiError::bad("`name` is 1 to 256 characters of letters, digits and `_.+/-@`")
            })?;
        if req.body.is_empty() || req.body.len() > MAX_SYMBOL_BYTES {
            return Err(ApiError::new(
                413,
                "symbol_file_size",
                "a symbol file is 1 byte to 64 MiB",
            ));
        }
        check_content(kind, &req.body)?;

        let _slot = InFlight::acquire(&self.uploads, MAX_UPLOADS_IN_FLIGHT).ok_or_else(|| {
            let mut busy = ApiError::new(
                429,
                "busy",
                "too many uploads are in progress; retry shortly",
            );
            busy.retry_after = Some(2);
            busy
        })?;
        let replaced = self.meta.symbol_file(site.id, &release, kind, &name)?;
        let total = self.meta.symbol_bytes(site.id)?;
        let after = total - replaced.as_ref().map_or(0, |f| f.size) + req.body.len() as u64;
        if after > MAX_SITE_SYMBOL_BYTES {
            return Err(ApiError::new(
                413,
                "symbol_quota",
                "the site would keep more than 1 GiB of symbol files",
            ));
        }
        let id = ArtifactId {
            site_id: site.id,
            release: &release,
            kind: kind.as_str(),
            name: &name,
        };
        let stored = self.artifacts.put(id, &req.body).map_err(|error| {
            error!(%error, "cannot store a symbol file");
            ApiError::internal()
        })?;
        let file = SymbolFile {
            site_id: site.id,
            release,
            kind,
            name,
            size: stored.size,
            sha256: stored.sha256,
            uploaded_at: 0,
        };
        self.meta.record_symbol_file(&file)?;
        Self::audit(principal, "upload_symbol_file", Some(site.id));
        let recorded = self
            .meta
            .symbol_file(site.id, &file.release, kind, &file.name)?
            .unwrap_or(file);
        Ok(ApiResponse::with(201, symbol_json(&recorded)))
    }

    /// With `kind` and `name`, one file; with only `release`, all of its files.
    pub(super) fn delete_symbols(
        &self,
        req: &ApiRequest,
        principal: &Principal,
        site: u32,
    ) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let release = query_param(req.query.as_deref(), "release")?
            .filter(|r| is_release(r))
            .ok_or_else(|| ApiError::bad("`release` is required"))?;
        let kind = query_param(req.query.as_deref(), "kind")?;
        let name = query_param(req.query.as_deref(), "name")?;
        let storage_failed = |error: dionaea_artifacts::ArtifactError| {
            error!(%error, "cannot remove a symbol file");
            ApiError::internal()
        };
        match (kind, name) {
            (None, None) => {
                let removed = self.meta.remove_release_symbols(site.id, &release)?;
                if removed.is_empty() {
                    return Err(ApiError::not_found());
                }
                self.artifacts
                    .delete_release(site.id, &release)
                    .map_err(storage_failed)?;
            }
            (Some(kind), Some(name)) => {
                let kind = SymbolKind::parse(&kind)
                    .ok_or_else(|| ApiError::bad("`kind` is sourcemap or proguard"))?;
                if !self
                    .meta
                    .remove_symbol_file(site.id, &release, kind, &name)?
                {
                    return Err(ApiError::not_found());
                }
                self.artifacts
                    .delete(ArtifactId {
                        site_id: site.id,
                        release: &release,
                        kind: kind.as_str(),
                        name: &name,
                    })
                    .map_err(storage_failed)?;
            }
            _ => return Err(ApiError::bad("send `kind` and `name` together, or neither")),
        }
        Self::audit(principal, "delete_symbol_files", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }

    pub(super) fn upload_tokens(&self, principal: &Principal, site: u32) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let tokens: Vec<Value> = self
            .meta
            .upload_tokens(site.id)?
            .iter()
            .map(token_json)
            .collect();
        Ok(ApiResponse::ok(json!({ "tokens": tokens })))
    }

    /// The secret is in this response and nowhere else, ever.
    pub(super) fn issue_upload_token(
        &self,
        req: &ApiRequest,
        principal: &Principal,
        site: u32,
    ) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        let body: NewUploadTokenBody = Self::json(req)?;
        let created_by = principal.member().map(|m| m.id);
        let (info, secret) = self
            .auth
            .issue_upload_token(site.id, &body.label, created_by)?;
        Self::audit(principal, "issue_upload_token", Some(site.id));
        let mut value = token_json(&info);
        if let Some(map) = value.as_object_mut() {
            map.insert("token".to_owned(), json!(secret));
        }
        Ok(ApiResponse::with(201, value))
    }

    pub(super) fn revoke_upload_token(
        &self,
        principal: &Principal,
        site: u32,
        token: u32,
    ) -> ApiResult {
        let site = self.site_access(principal, site, Permission::Manage)?;
        self.meta.revoke_upload_token(site.id, token)?;
        Self::audit(principal, "revoke_upload_token", Some(site.id));
        Ok(ApiResponse::with(204, Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_keeps_plus_and_refuses_bad_escapes() {
        assert_eq!(percent_decode("1.4.2+57").as_deref(), Some("1.4.2+57"));
        assert_eq!(percent_decode("a%2Fb%20c").as_deref(), Some("a/b c"));
        assert_eq!(percent_decode("%2B").as_deref(), Some("+"));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("%ff"), None, "not UTF-8");
    }

    #[test]
    fn a_parameter_is_found_among_others() {
        let found = query_param(Some("a=1&release=1.4.2%2B57&kind=proguard"), "release").unwrap();
        assert_eq!(found.as_deref(), Some("1.4.2+57"));
        assert_eq!(query_param(Some("a=1"), "release").unwrap(), None);
        assert_eq!(query_param(None, "release").unwrap(), None);
        assert!(query_param(Some("release=%zz"), "release").is_err());
    }

    #[test]
    fn content_is_checked_for_its_kind() {
        assert!(check_content(SymbolKind::SourceMap, br#" {"version":3,"mappings":""}"#).is_ok());
        for bad in [&b""[..], b"[1]", b"not json", b"{\"a\":", b"\"text\""] {
            assert!(
                check_content(SymbolKind::SourceMap, bad).is_err(),
                "{bad:?}"
            );
        }
        assert!(check_content(SymbolKind::Proguard, b"com.example.A -> a:\n").is_ok());
        for bad in [&b""[..], b"\xff\xfe", b"a\0b"] {
            assert!(check_content(SymbolKind::Proguard, bad).is_err(), "{bad:?}");
        }
    }
}
