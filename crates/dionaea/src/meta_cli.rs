//! `dionaea site`, `dionaea key` and `dionaea schema push|versions|retire`:
//! the metadata operations an operator needs before there is a dashboard.
//! They take a [`MetaStore`] and two writers so they can be tested without a
//! process. Exit status: 0 success, 1 the store refused the request (a name in
//! use, an unknown id, a schema that is invalid or breaking), 2 the store could
//! not be used.
//!
//! Results go to stdout as tab-separated lines, so scripts can read them;
//! problems go to stderr.

use std::io::Write;

use dionaea_core::Severity;
use dionaea_meta::{MetaError, MetaStore, Organization};

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
}
