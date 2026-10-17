//! `dionaea member` and `dionaea token`: the first member has to come from
//! somewhere, and scripts need a read token before anyone logs in. Passwords
//! are read from standard input, never from the command line, so they stay
//! out of the process list and the shell history.
//!
//! Same conventions as `meta_cli`: tab-separated results on stdout, problems
//! on stderr, exit status 0, 1 (refused) or 2 (unusable).

use std::io::{BufRead, Write};

use dionaea_auth::{Auth, AuthConfig, AuthError, Hasher, check_policy};
use dionaea_meta::{AccountStore, MetaError, MetaStore, Role};

use crate::meta_cli::{OK, REFUSED, UNUSABLE};

fn failure(error: &AuthError, err: &mut impl Write) -> u8 {
    let _ = writeln!(err, "error: {error}");
    match error {
        AuthError::Hash(_) | AuthError::Random(_) => UNUSABLE,
        AuthError::Meta(MetaError::Corrupt(_) | MetaError::Storage(_)) => UNUSABLE,
        _ => REFUSED,
    }
}

fn meta_failure(error: &MetaError, err: &mut impl Write) -> u8 {
    let _ = writeln!(err, "error: {error}");
    match error {
        MetaError::Corrupt(_) | MetaError::Storage(_) => UNUSABLE,
        _ => REFUSED,
    }
}

/// One line from `input`, without its line ending.
pub(crate) fn read_password(input: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    input.read_line(&mut line).ok()?;
    let line = line.trim_end_matches(['\n', '\r']);
    (!line.is_empty()).then(|| line.to_owned())
}

fn role_of(text: &str, err: &mut impl Write) -> Result<Role, u8> {
    Role::parse(text).ok_or_else(|| {
        let _ = writeln!(err, "error: the role is viewer, admin or owner");
        REFUSED
    })
}

/// Adds a member, and with `organization` makes them part of it. The
/// organization is made when it does not exist yet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_create(
    meta: &(impl AccountStore + MetaStore),
    hasher: Hasher,
    email: &str,
    name: &str,
    password: Option<String>,
    organization: Option<&str>,
    role: &str,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let Some(password) = password else {
        let _ = writeln!(err, "error: send the password on standard input");
        return REFUSED;
    };
    let role = match role_of(role, err) {
        Ok(role) => role,
        Err(code) => return code,
    };
    let hash = match check_policy(&password).and_then(|()| hasher.hash(&password)) {
        Ok(hash) => hash,
        Err(error) => return failure(&error, err),
    };
    let member = match meta.create_member(email, name, &hash) {
        Ok(member) => member,
        Err(error) => return meta_failure(&error, err),
    };
    if let Some(organization) = organization {
        let result = crate::meta_cli::organization(meta, organization)
            .and_then(|org| meta.set_role(member.id, org.id, Some(role)));
        if let Err(error) = result {
            let _ = writeln!(
                err,
                "the member {} was created but not added to {organization}",
                member.id
            );
            return meta_failure(&error, err);
        }
    }
    let _ = writeln!(out, "{}\t{}", member.id, member.email);
    OK
}

/// Lists members as: id, address, name, `active` or `disabled`, and their
/// roles as `organization:role` separated by commas.
pub(crate) fn member_list(
    meta: &(impl AccountStore + MetaStore),
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let organizations = match meta.organizations() {
        Ok(organizations) => organizations,
        Err(error) => return meta_failure(&error, err),
    };
    let mut seen = std::collections::BTreeMap::new();
    for organization in &organizations {
        match meta.organization_members(organization.id) {
            Ok(members) => {
                for m in members {
                    let entry = seen
                        .entry(m.member.id)
                        .or_insert_with(|| (m.member.clone(), Vec::new()));
                    entry
                        .1
                        .push(format!("{}:{}", organization.name, m.role.as_str()));
                }
            }
            Err(error) => return meta_failure(&error, err),
        }
    }
    for (id, (member, roles)) in seen {
        let _ = writeln!(
            out,
            "{id}\t{}\t{}\t{}\t{}",
            member.email,
            member.name,
            if member.disabled {
                "disabled"
            } else {
                "active"
            },
            roles.join(",")
        );
    }
    OK
}

/// Gives a member a role in an organization, or with `role` of `none` removes
/// them from it.
pub(crate) fn member_role(
    meta: &(impl AccountStore + MetaStore),
    member: u32,
    organization: &str,
    role: &str,
    err: &mut impl Write,
) -> u8 {
    let role = if role == "none" {
        None
    } else {
        match role_of(role, err) {
            Ok(role) => Some(role),
            Err(code) => return code,
        }
    };
    let result = crate::meta_cli::organization(meta, organization)
        .and_then(|org| meta.set_role(member, org.id, role));
    match result {
        Ok(()) => OK,
        Err(error) => meta_failure(&error, err),
    }
}

pub(crate) fn member_disable(
    meta: &impl AccountStore,
    member: u32,
    disabled: bool,
    err: &mut impl Write,
) -> u8 {
    match meta.set_disabled(member, disabled) {
        Ok(()) => OK,
        Err(error) => meta_failure(&error, err),
    }
}

pub(crate) fn member_password(
    meta: &impl AccountStore,
    hasher: Hasher,
    member: u32,
    password: Option<String>,
    err: &mut impl Write,
) -> u8 {
    let Some(password) = password else {
        let _ = writeln!(err, "error: send the password on standard input");
        return REFUSED;
    };
    let hash = match check_policy(&password).and_then(|()| hasher.hash(&password)) {
        Ok(hash) => hash,
        Err(error) => return failure(&error, err),
    };
    match meta.set_password_hash(member, &hash) {
        Ok(()) => OK,
        Err(error) => meta_failure(&error, err),
    }
}

/// Issues a read token for a site and prints it: the only time it is shown.
pub(crate) fn token_issue(
    meta: std::sync::Arc<dyn AccountStore>,
    hasher: Hasher,
    site: u32,
    label: &str,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let auth = Auth::new(meta, hasher, AuthConfig::default());
    match auth.issue_read_token(site, label, None) {
        Ok((info, secret)) => {
            let _ = writeln!(out, "{}\t{secret}", info.id);
            OK
        }
        Err(error) => failure(&error, err),
    }
}

/// Lists a site's read tokens as: id, label, `active` or `revoked`, created at.
pub(crate) fn token_list(
    meta: &impl AccountStore,
    site: u32,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match meta.read_tokens(site) {
        Ok(tokens) => {
            for token in tokens {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}",
                    token.id,
                    token.label,
                    if token.revoked_at.is_some() {
                        "revoked"
                    } else {
                        "active"
                    },
                    token.created_at
                );
            }
            OK
        }
        Err(error) => meta_failure(&error, err),
    }
}

pub(crate) fn token_revoke(
    meta: &impl AccountStore,
    site: u32,
    id: u32,
    err: &mut impl Write,
) -> u8 {
    match meta.revoke_read_token(site, id) {
        Ok(()) => OK,
        Err(error) => meta_failure(&error, err),
    }
}
