//! `dionaea site`, `dionaea key` and `dionaea schema push|versions|retire`:
//! the metadata operations an operator needs before there is a dashboard.
//! They take a [`MetaStore`] and two writers so they can be tested without a
//! process. Exit status: 0 success, 1 the store refused the request (a name in
//! use, an unknown id, a schema that is invalid or breaking), 2 the store could
//! not be used.
//!
//! Results go to stdout as tab-separated lines, so scripts can read them;
//! problems go to stderr.

use std::collections::BTreeMap;
use std::io::Write;

use dionaea_core::Severity;
use dionaea_meta::{
    ErasureRequest, ErasureState, ErasureSubject, MetaError, MetaStore, Organization,
    RetentionFamily,
};

use crate::settings::{self, Origin, Tuning};

pub(crate) const OK: u8 = 0;
pub(crate) const REFUSED: u8 = 1;
pub(crate) const UNUSABLE: u8 = 2;

/// The organization a site belongs to when none is named.
const DEFAULT_ORGANIZATION: &str = "default";

fn report(error: &MetaError, label: &str, err: &mut impl Write) -> u8 {
    match error {
        MetaError::InvalidSchema(diagnostics) => {
            for d in diagnostics {
                let _ = writeln!(err, "{label}:{}:{}: {}", d.line, d.column, d.message);
            }
        }
        MetaError::Breaking(changes) => {
            for change in changes {
                let _ = writeln!(err, "{change}");
            }
            let _ = writeln!(
                err,
                "error: the schema breaks apps that use an earlier version; \
                 declare a new event instead"
            );
        }
        other => {
            let _ = writeln!(err, "error: {other}");
        }
    }
    match error {
        MetaError::Corrupt(_) | MetaError::Storage(_) => UNUSABLE,
        _ => REFUSED,
    }
}

fn organization(store: &dyn MetaStore, name: &str) -> Result<Organization, MetaError> {
    if let Some(found) = store.organizations()?.into_iter().find(|o| o.name == name) {
        return Ok(found);
    }
    store.create_organization(name)
}

pub(crate) fn site_create(
    store: &dyn MetaStore,
    name: &str,
    organization_name: Option<&str>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let result = organization(store, organization_name.unwrap_or(DEFAULT_ORGANIZATION))
        .and_then(|org| store.create_site(org.id, name));
    match result {
        Ok(site) => {
            let _ = writeln!(out, "{}\t{}", site.id, site.name);
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn site_list(store: &dyn MetaStore, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let result = store.organizations().and_then(|orgs| {
        store.sites().map(|sites| {
            sites
                .into_iter()
                .map(|site| {
                    let org = orgs
                        .iter()
                        .find(|o| o.id == site.organization_id)
                        .map_or("?", |o| o.name.as_str())
                        .to_owned();
                    (site, org)
                })
                .collect::<Vec<_>>()
        })
    });
    match result {
        Ok(rows) => {
            for (site, org) in rows {
                let _ = writeln!(out, "{}\t{}\t{}", site.id, site.name, org);
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn key_issue(
    store: &dyn MetaStore,
    site_id: u32,
    label: &str,
    origins: &[String],
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.issue_write_key(site_id, label, origins) {
        Ok(key) => {
            let _ = writeln!(out, "{}", key.key);
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn key_revoke(store: &dyn MetaStore, key: &str, err: &mut impl Write) -> u8 {
    match store.revoke_write_key(key) {
        Ok(()) => OK,
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn key_list(
    store: &dyn MetaStore,
    site_id: u32,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.write_keys(site_id) {
        Ok(keys) => {
            for key in keys {
                let state = if key.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                };
                let origins = if key.origins.is_empty() {
                    "-".to_owned()
                } else {
                    key.origins.join(",")
                };
                let _ = writeln!(out, "{}\t{state}\t{}\t{origins}", key.key, key.label);
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// Publishes `source` as the next version of a site, or with `dry_run` only
/// lists what publishing would change.
pub(crate) fn schema_push(
    store: &dyn MetaStore,
    site_id: u32,
    (label, source): (&str, &str),
    dry_run: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let (changes, version) = if dry_run {
        (store.diff_schema(site_id, source), None)
    } else {
        match store.publish_schema(site_id, source) {
            Ok(published) => (Ok(published.changes), Some(published.version)),
            Err(error) => (Err(error), None),
        }
    };
    match changes {
        Ok(changes) => {
            for change in &changes {
                let _ = writeln!(out, "{change}");
            }
            let breaking = changes
                .iter()
                .filter(|c| c.severity() == Severity::Breaking)
                .count();
            match version {
                Some(version) => {
                    let _ = writeln!(
                        out,
                        "published version {version}: {} changes, {breaking} breaking",
                        changes.len()
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "dry run: {} changes, {breaking} breaking",
                        changes.len()
                    );
                }
            }
            if breaking > 0 { REFUSED } else { OK }
        }
        Err(error) => report(&error, label, err),
    }
}

pub(crate) fn schema_versions(
    store: &dyn MetaStore,
    site_id: u32,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.schema_versions(site_id) {
        Ok(versions) => {
            for v in versions {
                let state = if v.retired_at.is_some() {
                    "retired"
                } else {
                    "active"
                };
                let _ = writeln!(out, "{}\t{state}", v.version);
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn schema_retire(
    store: &dyn MetaStore,
    site_id: u32,
    version: u32,
    err: &mut impl Write,
) -> u8 {
    match store.retire_schema_version(site_id, version) {
        Ok(()) => OK,
        Err(error) => report(&error, "", err),
    }
}

/// One setting as the commands show it.
struct Row {
    spec: &'static settings::Spec,
    value: String,
    source: String,
}

/// The value of every setting in force at `now_ms`, where it comes from, and
/// what is about to change.
fn rows(
    store: &dyn MetaStore,
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    now_ms: i64,
) -> Result<Vec<Row>, MetaError> {
    let stored = store.settings()?;
    let resolved = settings::resolve_at(&stored, now_ms);
    let (tuning, skipped) = base.with_stored(&resolved);
    let overridden = settings::overridden(base, origins, &resolved);
    Ok(settings::SPECS
        .iter()
        .map(|spec| {
            let applied = resolved.contains_key(spec.name)
                && !skipped.iter().any(|(name, _)| name == spec.name);
            let mut source = if applied {
                match overridden.iter().find(|(name, _, _)| *name == spec.name) {
                    Some((_, from, was)) => {
                        format!("stored (overrides {}: {was})", from.as_str())
                    }
                    None => Origin::Stored.as_str().to_owned(),
                }
            } else {
                origins
                    .get(spec.name)
                    .copied()
                    .unwrap_or(Origin::Default)
                    .as_str()
                    .to_owned()
            };
            if let Some(change) = stored
                .get(spec.name)
                .filter(|change| change.effective_at_ms > now_ms)
            {
                let seconds = (change.effective_at_ms - now_ms).div_euclid(1000) + 1;
                let then = change.value.clone().unwrap_or_else(|| "removed".to_owned());
                source.push_str(&format!("; {then} in {seconds} s"));
            }
            Row {
                spec,
                value: tuning.show(spec),
                source,
            }
        })
        .collect())
}

/// Lists every setting that can change while the server runs: name, value in
/// force, where the value comes from, its range and what it is.
pub(crate) fn settings_list(
    store: &dyn MetaStore,
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    now_ms: i64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match rows(store, base, origins, now_ms) {
        Ok(rows) => {
            for row in rows {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}",
                    row.spec.name, row.value, row.source, row.spec.rule, row.spec.help
                );
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// Prints the value in force of one setting, alone, for scripts.
pub(crate) fn settings_get(
    store: &dyn MetaStore,
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    now_ms: i64,
    name: &str,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match rows(store, base, origins, now_ms) {
        Ok(rows) => match rows.into_iter().find(|row| row.spec.name == name) {
            Some(row) => {
                let _ = writeln!(out, "{}", row.value);
                OK
            }
            None => {
                let _ = writeln!(err, "error: {}", unknown(name));
                REFUSED
            }
        },
        Err(error) => report(&error, "", err),
    }
}

fn unknown(name: &str) -> String {
    settings::validate(name, "0")
        .err()
        .unwrap_or_else(|| format!("`{name}` is not a setting"))
}

/// Stores a setting to take effect `delay_ms` from `now_ms`. Every server that
/// reads the store switches at that moment, so the delay should be at least
/// one refresh interval. A server that is down applies it when it starts.
pub(crate) fn settings_set(
    store: &dyn MetaStore,
    name: &str,
    value: &str,
    (now_ms, delay_ms): (i64, i64),
    err: &mut impl Write,
) -> u8 {
    let canonical = match settings::canonical(name, value) {
        Ok(canonical) => canonical,
        Err(reason) => {
            let _ = writeln!(err, "error: {name}: {reason}");
            return REFUSED;
        }
    };
    match store.set_setting(name, Some(&canonical), now_ms + delay_ms, now_ms) {
        Ok(()) => {
            if delay_ms > 0 {
                let _ = writeln!(
                    err,
                    "{name} takes effect in {} s",
                    delay_ms.div_euclid(1000)
                );
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// Removes a stored setting from `delay_ms` on, so the file, the environment
/// or the default applies again.
pub(crate) fn settings_unset(
    store: &dyn MetaStore,
    name: &str,
    (now_ms, delay_ms): (i64, i64),
    err: &mut impl Write,
) -> u8 {
    if settings::find(name).is_none() {
        let _ = writeln!(err, "error: {}", unknown(name));
        return REFUSED;
    }
    let stored = match store.settings() {
        Ok(stored) => stored,
        Err(error) => return report(&error, "", err),
    };
    if stored
        .get(name)
        .is_none_or(|setting| setting.at(now_ms).is_none() && setting.effective_at_ms <= now_ms)
    {
        let _ = writeln!(err, "{name} was not stored");
        return OK;
    }
    match store.set_setting(name, None, now_ms + delay_ms, now_ms) {
        Ok(()) => {
            if delay_ms > 0 {
                let _ = writeln!(err, "{name} is removed in {} s", delay_ms.div_euclid(1000));
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// Sets how long a site keeps its analytics data, or with `None` goes back to
/// the default. The retention job applies it at its next run.
pub(crate) fn retention_set(
    store: &dyn MetaStore,
    site_id: u32,
    days: Option<u32>,
    err: &mut impl Write,
) -> u8 {
    match store.set_retention(site_id, RetentionFamily::Analytics, days) {
        Ok(()) => OK,
        Err(error) => report(&error, "", err),
    }
}

/// What a run of the retention job would remove now, per site and table, at
/// the retention each site has: nothing is deleted. A site's row is
/// `site, table, rows older than the cutoff, the cutoff as a day`.
pub(crate) fn retention_preview(
    store: &dyn MetaStore,
    clickhouse: &dyn Fn(u32, u64) -> Result<Vec<(&'static str, u64)>, String>,
    default_days: u32,
    now_ms: u64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let result = store
        .sites()
        .and_then(|sites| Ok((sites, store.retentions()?)));
    let (sites, own) = match result {
        Ok(found) => found,
        Err(error) => return report(&error, "", err),
    };
    for site in sites {
        let days = own
            .iter()
            .find(|r| r.site_id == site.id && r.family == RetentionFamily::Analytics)
            .map_or(default_days, |r| r.days);
        let cutoff = now_ms.saturating_sub(u64::from(days) * 86_400_000);
        match clickhouse(site.id, cutoff) {
            Ok(counts) => {
                for (table, rows) in counts {
                    let _ = writeln!(out, "{}\t{table}\t{rows}\t{days}", site.id);
                }
            }
            Err(message) => {
                let _ = writeln!(err, "error: {message}");
                return UNUSABLE;
            }
        }
    }
    OK
}

/// The retention default in force, which a stored setting may have changed.
pub(crate) fn retention_default_days(
    store: &dyn MetaStore,
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    now_ms: i64,
) -> Option<u32> {
    rows(store, base, origins, now_ms)
        .ok()?
        .iter()
        .find(|row| row.spec.name == "retention_default_days")
        .and_then(|row| row.value.parse().ok())
}

/// Lists every site's retention as: site, family, days, and `site` when the
/// site has its own number or `default` when it follows the default.
pub(crate) fn retention_list(
    store: &dyn MetaStore,
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    now_ms: i64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let result = rows(store, base, origins, now_ms).and_then(|rows| {
        let default = rows
            .iter()
            .find(|row| row.spec.name == "retention_default_days")
            .and_then(|row| row.value.parse::<u32>().ok())
            .ok_or_else(|| MetaError::Corrupt("no default retention".to_owned()))?;
        Ok((default, store.sites()?, store.retentions()?))
    });
    match result {
        Ok((default, sites, own)) => {
            for site in sites {
                let (days, from) = own
                    .iter()
                    .find(|r| r.site_id == site.id && r.family == RetentionFamily::Analytics)
                    .map_or((default, "default"), |r| (r.days, "site"));
                let _ = writeln!(
                    out,
                    "{}\t{}\t{days}\t{from}",
                    site.id,
                    RetentionFamily::Analytics.as_str()
                );
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

fn erasure_line(request: &ErasureRequest) -> String {
    let time = |value: Option<i64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        request.id,
        request.site_id,
        request.state,
        request.requested_by,
        request.requested_at,
        time(request.hidden_at),
        time(request.completed_at),
        request.anon_count,
        request.user_hash,
        request.attempts,
        if request.is_device { "device" } else { "user" },
    )
}

/// Asks for a user of a site to be erased. The user id is stored only until
/// the job has finished; every later listing shows its hash.
pub(crate) fn erase_request(
    store: &dyn MetaStore,
    site_id: u32,
    subject: ErasureSubject<'_>,
    requested_by: &str,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let asked = match subject {
        ErasureSubject::User(user) => store.request_erasure(site_id, user, requested_by),
        ErasureSubject::Device(device) => {
            store.request_device_erasure(site_id, device, requested_by)
        }
    };
    match asked {
        Ok(request) => {
            let _ = writeln!(out, "{}", erasure_line(&request));
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// Lists requests as: id, site, state, requested by, requested at, hidden at,
/// completed at (seconds since the epoch, `-` when not yet), devices
/// tombstoned, the hash of the user id (or of the device id), how many times
/// trouble has stopped the request in its current state, and whether it is for
/// a `user` or a `device`.
pub(crate) fn erase_list(
    store: &dyn MetaStore,
    state: Option<ErasureState>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.erasure_requests(state) {
        Ok(requests) => {
            for request in &requests {
                let _ = writeln!(out, "{}", erasure_line(request));
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

/// One request: its line, then what the job reported.
pub(crate) fn erase_show(
    store: &dyn MetaStore,
    id: u64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.erasure_request(id) {
        Ok(request) => {
            let _ = writeln!(out, "{}", erasure_line(&request));
            if let Some(detail) = &request.detail {
                let _ = writeln!(out, "{detail}");
            }
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

pub(crate) fn erase_retry(
    store: &dyn MetaStore,
    id: u64,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match store.retry_erasure(id) {
        Ok(request) => {
            let _ = writeln!(out, "{}", erasure_line(&request));
            OK
        }
        Err(error) => report(&error, "", err),
    }
}

#[cfg(test)]
mod tests {
    use dionaea_meta::SqliteMeta;

    use super::*;

    const V1: &str = "[events.e.fields.f]\nkind = \"id\"\n";
    const V2: &str = "[events.e.fields.f]\nkind = \"id\"\n[events.other]\n";
    const BREAKING: &str = "[events.e.fields.f]\nkind = \"unit\"\n";

    fn store() -> SqliteMeta {
        SqliteMeta::open_in_memory().unwrap()
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    /// Runs `command` and returns (code, stdout, stderr).
    fn run(command: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>) -> u8) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = command(&mut out, &mut err);
        (code, text(out), text(err))
    }

    #[test]
    fn a_site_is_created_in_the_default_organization_which_is_reused() {
        let store = store();
        let (code, out, _) = run(|o, e| site_create(&store, "shop", None, o, e));
        assert_eq!((code, out.as_str()), (OK, "1\tshop\n"));
        let (code, out, _) = run(|o, e| site_create(&store, "blog", None, o, e));
        assert_eq!((code, out.as_str()), (OK, "2\tblog\n"));
        assert_eq!(store.organizations().unwrap().len(), 1);

        let (code, _, _) = run(|o, e| site_create(&store, "app", Some("Acme"), o, e));
        assert_eq!(code, OK);
        let (code, out, _) = run(|o, e| site_list(&store, o, e));
        assert_eq!(code, OK);
        assert_eq!(out, "1\tshop\tdefault\n2\tblog\tdefault\n3\tapp\tAcme\n");
    }

    #[test]
    fn a_taken_or_invalid_name_is_refused_with_a_message() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        let (code, out, err) = run(|o, e| site_create(&store, "shop", None, o, e));
        assert_eq!(code, REFUSED);
        assert!(out.is_empty());
        assert!(err.starts_with("error: conflict:"), "{err}");
        let (code, _, err) = run(|o, e| site_create(&store, "  ", None, o, e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("invalid"), "{err}");
    }

    #[test]
    fn keys_are_issued_listed_and_revoked() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        let origins = ["https://shop.example".to_owned()];
        let (code, key, _) = run(|o, e| key_issue(&store, 1, "web", &origins, o, e));
        assert_eq!(code, OK);
        let key = key.trim().to_owned();
        assert!(dionaea_meta::is_write_key_shape(&key), "{key}");
        let (_, app, _) = run(|o, e| key_issue(&store, 1, "ios", &[], o, e));
        let app = app.trim().to_owned();

        let (_, list, _) = run(|o, e| key_list(&store, 1, o, e));
        assert_eq!(
            list,
            format!("{key}\tactive\tweb\thttps://shop.example\n{app}\tactive\tios\t-\n")
        );
        let (code, _, err) = run(|_, e| key_revoke(&store, &key, e));
        assert_eq!((code, err.as_str()), (OK, ""));
        let (_, list, _) = run(|o, e| key_list(&store, 1, o, e));
        assert!(list.contains(&format!("{key}\trevoked")), "{list}");

        let (code, _, err) = run(|_, e| key_revoke(&store, "wk_aaaaaaaaaaaaaaaaaaaaaaaaaa", e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("not found"), "{err}");
        let (code, _, _) = run(|o, e| key_issue(&store, 9, "x", &[], o, e));
        assert_eq!(code, REFUSED, "unknown site");
        let bad = ["not-an-origin".to_owned()];
        let (code, _, err) = run(|o, e| key_issue(&store, 1, "x", &bad, o, e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("origin"), "{err}");
    }

    #[test]
    fn pushing_publishes_versions_and_a_dry_run_stores_nothing() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        let (code, out, _) = run(|o, e| schema_push(&store, 1, ("s.toml", V1), false, o, e));
        assert_eq!(code, OK);
        assert_eq!(out, "published version 1: 0 changes, 0 breaking\n");

        let (code, out, _) = run(|o, e| schema_push(&store, 1, ("s.toml", V2), true, o, e));
        assert_eq!(code, OK);
        assert_eq!(
            out,
            "allowed: events.other: event added\ndry run: 1 changes, 0 breaking\n"
        );
        let (_, versions, _) = run(|o, e| schema_versions(&store, 1, o, e));
        assert_eq!(versions, "1\tactive\n");

        let (code, out, _) = run(|o, e| schema_push(&store, 1, ("s.toml", V2), false, o, e));
        assert_eq!(code, OK);
        assert!(
            out.ends_with("published version 2: 1 changes, 0 breaking\n"),
            "{out}"
        );
    }

    #[test]
    fn a_breaking_unchanged_or_invalid_push_is_refused_and_explained() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        run(|o, e| schema_push(&store, 1, ("s.toml", V1), false, o, e));

        let (code, out, err) =
            run(|o, e| schema_push(&store, 1, ("s.toml", BREAKING), false, o, e));
        assert_eq!(code, REFUSED);
        assert!(out.is_empty());
        assert!(
            err.contains("breaking: events.e.fields.f: kind changed from id to unit"),
            "{err}"
        );

        let (code, _, err) = run(|o, e| schema_push(&store, 1, ("s.toml", V1), false, o, e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("same as the latest"), "{err}");

        let bad = "[events.a.fields.b]\nkind = \"float\"\n";
        let (code, _, err) = run(|o, e| schema_push(&store, 1, ("s.toml", bad), false, o, e));
        assert_eq!(code, REFUSED);
        assert!(err.starts_with("s.toml:2:8: unknown kind `float`"), "{err}");

        let (code, _, err) = run(|o, e| schema_push(&store, 9, ("s.toml", V1), false, o, e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("site not found"), "{err}");

        let (_, versions, _) = run(|o, e| schema_versions(&store, 1, o, e));
        assert_eq!(versions, "1\tactive\n", "refused pushes stored nothing");
    }

    #[test]
    fn a_dry_run_exits_1_on_a_breaking_change_like_diff_does() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        run(|o, e| schema_push(&store, 1, ("s.toml", V1), false, o, e));
        let (code, out, _) = run(|o, e| schema_push(&store, 1, ("s.toml", BREAKING), true, o, e));
        assert_eq!(code, REFUSED);
        assert!(out.ends_with("dry run: 1 changes, 1 breaking\n"), "{out}");
    }

    #[test]
    fn versions_can_be_retired_but_not_the_last_one() {
        let store = store();
        run(|o, e| site_create(&store, "shop", None, o, e));
        run(|o, e| schema_push(&store, 1, ("s.toml", V1), false, o, e));
        let (code, _, err) = run(|_, e| schema_retire(&store, 1, 1, e));
        assert_eq!(code, REFUSED);
        assert!(err.contains("last active version"), "{err}");
        run(|o, e| schema_push(&store, 1, ("s.toml", V2), false, o, e));
        let (code, _, _) = run(|_, e| schema_retire(&store, 1, 1, e));
        assert_eq!(code, OK);
        let (_, versions, _) = run(|o, e| schema_versions(&store, 1, o, e));
        assert_eq!(versions, "1\tretired\n2\tactive\n");
    }

    #[test]
    fn the_preview_asks_for_each_site_at_its_own_cutoff_and_deletes_nothing() {
        let store = store();
        let organization = store.create_organization("o").unwrap();
        let first = store.create_site(organization.id, "a").unwrap();
        let second = store.create_site(organization.id, "b").unwrap();
        store
            .set_retention(second.id, RetentionFamily::Analytics, Some(10))
            .unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let now = 100 * 86_400_000;
        let (code, out, err) = run(|out, err| {
            retention_preview(
                &store,
                &|site, cutoff| {
                    asked.borrow_mut().push((site, cutoff));
                    Ok(vec![("events", u64::from(site) * 5), ("sessions", 1)])
                },
                30,
                now,
                out,
                err,
            )
        });
        assert_eq!((code, err.as_str()), (OK, ""));
        assert_eq!(
            *asked.borrow(),
            vec![(first.id, 70 * 86_400_000), (second.id, 90 * 86_400_000)]
        );
        assert_eq!(
            out,
            format!(
                "{a}	events	5	30
{a}	sessions	1	30
{b}	events	10	10
{b}	sessions	1	10
",
                a = first.id,
                b = second.id
            )
        );
    }

    #[test]
    fn a_preview_that_cannot_reach_clickhouse_says_so_and_exits_with_the_unusable_code() {
        let store = store();
        let organization = store.create_organization("o").unwrap();
        store.create_site(organization.id, "a").unwrap();
        let (code, out, err) = run(|out, err| {
            retention_preview(
                &store,
                &|_, _| Err("cannot count in ClickHouse: down".to_owned()),
                30,
                0,
                out,
                err,
            )
        });
        assert_eq!((code, out.as_str()), (UNUSABLE, ""));
        assert!(err.contains("down"), "{err}");
    }
}
