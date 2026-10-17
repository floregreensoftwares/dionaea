//! Members, their roles, login sessions and read tokens.
//!
//! This store never sees a secret. A password arrives already hashed (an
//! argon2id string) and a session or read token arrives as the SHA-256 of
//! the secret, so a copy of the database cannot be used to log in or to read
//! reports. Secrets are made and checked by `dionaea-auth`.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::sqlite::{SqliteMeta, id_of, storage};
use crate::{MetaError, Site};

/// What a member may do inside an organization. Each role includes the ones
/// before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Reads the reports of the organization's sites.
    Viewer,
    /// Also manages sites, keys, schemas, retention and erasure.
    Admin,
    /// Also manages the organization's members.
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "viewer" => Some(Self::Viewer),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: u32,
    /// Lowercase.
    pub email: String,
    pub name: String,
    pub created_at: i64,
    pub disabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership {
    pub organization_id: u32,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationMember {
    pub member: Member,
    pub role: Role,
}

/// A member with the organizations they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub member: Member,
    pub memberships: Vec<Membership>,
}

/// A read token as listed; the secret is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadTokenInfo {
    pub id: u32,
    pub site_id: u32,
    pub label: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

/// Longest e-mail address, in bytes.
pub const MAX_EMAIL_LEN: usize = 254;
/// Longest member name, in characters.
pub const MAX_NAME_LEN: usize = 64;
/// Longest time zone name, in bytes.
pub const MAX_TIME_ZONE_LEN: usize = 64;

pub trait AccountStore: Send + Sync {
    /// Whether any member exists.
    fn has_members(&self) -> Result<bool, MetaError>;

    /// Adds a member. `password_hash` is an argon2id string. The address is
    /// stored in lowercase and must be new.
    fn create_member(
        &self,
        email: &str,
        name: &str,
        password_hash: &str,
    ) -> Result<Member, MetaError>;

    fn member(&self, id: u32) -> Result<Member, MetaError>;

    /// The member with this address and their password hash, for logging in.
    fn member_for_login(&self, email: &str) -> Result<Option<(Member, String)>, MetaError>;

    /// Replaces the password and ends every session of the member.
    fn set_password_hash(&self, member_id: u32, password_hash: &str) -> Result<(), MetaError>;

    /// Stops the member logging in and ends their sessions.
    fn set_disabled(&self, member_id: u32, disabled: bool) -> Result<(), MetaError>;

    /// Gives the member a role in the organization, or with `None` removes
    /// them from it. The last owner of an organization cannot be removed or
    /// lowered.
    fn set_role(
        &self,
        member_id: u32,
        organization_id: u32,
        role: Option<Role>,
    ) -> Result<(), MetaError>;

    fn memberships(&self, member_id: u32) -> Result<Vec<Membership>, MetaError>;

    fn organization_members(
        &self,
        organization_id: u32,
    ) -> Result<Vec<OrganizationMember>, MetaError>;

    /// Records a login session; `token_hash` is the SHA-256 of the secret in
    /// lowercase hex. Returns when it expires (seconds since the epoch).
    fn create_session(
        &self,
        token_hash: &str,
        member_id: u32,
        ttl_secs: i64,
    ) -> Result<i64, MetaError>;

    /// The member of a session that has not expired, if they are not
    /// disabled.
    fn session(&self, token_hash: &str) -> Result<Option<Login>, MetaError>;

    fn delete_session(&self, token_hash: &str) -> Result<(), MetaError>;

    /// Removes the sessions that expired. Returns how many.
    fn delete_expired_sessions(&self) -> Result<u64, MetaError>;

    /// Records a read token for a site; `token_hash` is the SHA-256 of the
    /// secret in lowercase hex.
    fn issue_read_token(
        &self,
        site_id: u32,
        label: &str,
        token_hash: &str,
        created_by: Option<u32>,
    ) -> Result<ReadTokenInfo, MetaError>;

    /// The token with this hash, unless revoked.
    fn read_token(&self, token_hash: &str) -> Result<Option<ReadTokenInfo>, MetaError>;

    fn read_tokens(&self, site_id: u32) -> Result<Vec<ReadTokenInfo>, MetaError>;

    /// Revokes a token of the site. Revoking a revoked token is not an error;
    /// a token of another site is not found.
    fn revoke_read_token(&self, site_id: u32, id: u32) -> Result<(), MetaError>;
}

fn is_hex_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_hash(hash: &str, what: &str) -> Result<(), MetaError> {
    if is_hex_hash(hash) {
        Ok(())
    } else {
        Err(MetaError::Invalid(format!(
            "{what} must be a SHA-256 in lowercase hex"
        )))
    }
}

fn check_password_hash(hash: &str) -> Result<(), MetaError> {
    if hash.starts_with("$argon2id$") && hash.len() <= 512 && hash.is_ascii() {
        Ok(())
    } else {
        Err(MetaError::Invalid(
            "the password must be stored as an argon2id hash".to_owned(),
        ))
    }
}

fn clean_email(email: &str) -> Result<String, MetaError> {
    let email = email.trim().to_lowercase();
    let bad = || MetaError::Invalid("the e-mail address is not valid".to_owned());
    if email.len() > MAX_EMAIL_LEN || email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(bad());
    };
    if local.is_empty() || domain.len() < 3 || !domain.contains('.') || domain.contains('@') {
        return Err(bad());
    }
    Ok(email)
}

fn clean_name(name: &str) -> Result<String, MetaError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN || name.chars().any(char::is_control)
    {
        return Err(MetaError::Invalid(format!(
            "a member name is 1 to {MAX_NAME_LEN} characters without control characters"
        )));
    }
    Ok(name.to_owned())
}

fn member_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(i64, String, String, i64, Option<i64>)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

const MEMBER_COLUMNS: &str = "id, email, name, created_at, disabled_at";

fn to_member(raw: (i64, String, String, i64, Option<i64>)) -> Result<Member, MetaError> {
    Ok(Member {
        id: id_of(raw.0)?,
        email: raw.1,
        name: raw.2,
        created_at: raw.3,
        disabled: raw.4.is_some(),
    })
}

fn memberships_of(tx: &Transaction<'_>, member_id: u32) -> Result<Vec<Membership>, MetaError> {
    let mut statement = tx
        .prepare("SELECT org_id, role FROM memberships WHERE member_id = ?1 ORDER BY org_id")
        .map_err(storage)?;
    let rows = statement
        .query_map([member_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(storage)?;
    rows.map(|row| {
        let (organization_id, role) = row.map_err(storage)?;
        Ok(Membership {
            organization_id: id_of(organization_id)?,
            role: Role::parse(&role)
                .ok_or_else(|| MetaError::Corrupt(format!("role `{role}` is unknown")))?,
        })
    })
    .collect()
}

fn token_of(raw: (i64, i64, String, i64, Option<i64>)) -> Result<ReadTokenInfo, MetaError> {
    Ok(ReadTokenInfo {
        id: id_of(raw.0)?,
        site_id: id_of(raw.1)?,
        label: raw.2,
        created_at: raw.3,
        revoked_at: raw.4,
    })
}

impl AccountStore for SqliteMeta {
    fn has_members(&self) -> Result<bool, MetaError> {
        self.read(|tx| {
            tx.query_row("SELECT EXISTS (SELECT 1 FROM members)", [], |row| {
                row.get(0)
            })
            .map_err(storage)
        })
    }

    fn create_member(
        &self,
        email: &str,
        name: &str,
        password_hash: &str,
    ) -> Result<Member, MetaError> {
        let email = clean_email(email)?;
        let name = clean_name(name)?;
        check_password_hash(password_hash)?;
        self.write(|tx, at| {
            let taken: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM members WHERE email = ?1)",
                    [&email],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if taken {
                return Err(MetaError::Conflict(
                    "a member with that address exists".to_owned(),
                ));
            }
            tx.execute(
                "INSERT INTO members (email, name, password_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![email, name, password_hash, at],
            )
            .map_err(storage)?;
            Ok(Member {
                id: id_of(tx.last_insert_rowid())?,
                email,
                name,
                created_at: at,
                disabled: false,
            })
        })
    }

    fn member(&self, id: u32) -> Result<Member, MetaError> {
        self.read(|tx| {
            let raw = tx
                .query_row(
                    &format!("SELECT {MEMBER_COLUMNS} FROM members WHERE id = ?1"),
                    [id],
                    member_row,
                )
                .optional()
                .map_err(storage)?
                .ok_or(MetaError::NotFound("member"))?;
            to_member(raw)
        })
    }

    fn member_for_login(&self, email: &str) -> Result<Option<(Member, String)>, MetaError> {
        let email = email.trim().to_lowercase();
        self.read(|tx| {
            let found = tx
                .query_row(
                    &format!(
                        "SELECT {MEMBER_COLUMNS}, password_hash FROM members WHERE email = ?1"
                    ),
                    [&email],
                    |row| Ok((member_row(row)?, row.get::<_, String>(5)?)),
                )
                .optional()
                .map_err(storage)?;
            found
                .map(|(raw, hash)| Ok((to_member(raw)?, hash)))
                .transpose()
        })
    }

    fn set_password_hash(&self, member_id: u32, password_hash: &str) -> Result<(), MetaError> {
        check_password_hash(password_hash)?;
        self.write(|tx, _| {
            let changed = tx
                .execute(
                    "UPDATE members SET password_hash = ?1 WHERE id = ?2",
                    params![password_hash, member_id],
                )
                .map_err(storage)?;
            if changed == 0 {
                return Err(MetaError::NotFound("member"));
            }
            tx.execute(
                "DELETE FROM auth_sessions WHERE member_id = ?1",
                [member_id],
            )
            .map_err(storage)?;
            Ok(())
        })
    }

    fn set_disabled(&self, member_id: u32, disabled: bool) -> Result<(), MetaError> {
        self.write(|tx, at| {
            let changed = tx
                .execute(
                    "UPDATE members SET disabled_at = ?1 WHERE id = ?2",
                    params![disabled.then_some(at), member_id],
                )
                .map_err(storage)?;
            if changed == 0 {
                return Err(MetaError::NotFound("member"));
            }
            if disabled {
                tx.execute(
                    "DELETE FROM auth_sessions WHERE member_id = ?1",
                    [member_id],
                )
                .map_err(storage)?;
            }
            Ok(())
        })
    }

    fn set_role(
        &self,
        member_id: u32,
        organization_id: u32,
        role: Option<Role>,
    ) -> Result<(), MetaError> {
        self.write(|tx, _| {
            let member: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM members WHERE id = ?1)",
                    [member_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !member {
                return Err(MetaError::NotFound("member"));
            }
            let organization: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM organizations WHERE id = ?1)",
                    [organization_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !organization {
                return Err(MetaError::NotFound("organization"));
            }
            let current: Option<String> = tx
                .query_row(
                    "SELECT role FROM memberships WHERE member_id = ?1 AND org_id = ?2",
                    params![member_id, organization_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            if current.as_deref() == Some("owner") && role != Some(Role::Owner) {
                let owners: i64 = tx
                    .query_row(
                        "SELECT count(*) FROM memberships WHERE org_id = ?1 AND role = 'owner'",
                        [organization_id],
                        |row| row.get(0),
                    )
                    .map_err(storage)?;
                if owners <= 1 {
                    return Err(MetaError::Conflict(
                        "an organization keeps at least one owner".to_owned(),
                    ));
                }
            }
            match role {
                Some(role) => {
                    tx.execute(
                        "INSERT INTO memberships (member_id, org_id, role) VALUES (?1, ?2, ?3)
                         ON CONFLICT (member_id, org_id) DO UPDATE SET role = excluded.role",
                        params![member_id, organization_id, role.as_str()],
                    )
                    .map_err(storage)?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM memberships WHERE member_id = ?1 AND org_id = ?2",
                        params![member_id, organization_id],
                    )
                    .map_err(storage)?;
                }
            }
            Ok(())
        })
    }

    fn memberships(&self, member_id: u32) -> Result<Vec<Membership>, MetaError> {
        self.read(|tx| memberships_of(tx, member_id))
    }

    fn organization_members(
        &self,
        organization_id: u32,
    ) -> Result<Vec<OrganizationMember>, MetaError> {
        self.read(|tx| {
            let mut statement = tx
                .prepare(
                    "SELECT m.id, m.email, m.name, m.created_at, m.disabled_at, s.role
                     FROM memberships s JOIN members m ON m.id = s.member_id
                     WHERE s.org_id = ?1 ORDER BY m.id",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map([organization_id], |row| {
                    Ok((member_row(row)?, row.get::<_, String>(5)?))
                })
                .map_err(storage)?;
            rows.map(|row| {
                let (raw, role) = row.map_err(storage)?;
                Ok(OrganizationMember {
                    member: to_member(raw)?,
                    role: Role::parse(&role)
                        .ok_or_else(|| MetaError::Corrupt(format!("role `{role}` is unknown")))?,
                })
            })
            .collect()
        })
    }

    fn create_session(
        &self,
        token_hash: &str,
        member_id: u32,
        ttl_secs: i64,
    ) -> Result<i64, MetaError> {
        check_hash(token_hash, "the session token hash")?;
        if ttl_secs <= 0 {
            return Err(MetaError::Invalid(
                "a session lasts a positive time".to_owned(),
            ));
        }
        self.write(|tx, at| {
            let member: Option<Option<i64>> = tx
                .query_row(
                    "SELECT disabled_at FROM members WHERE id = ?1",
                    [member_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            match member {
                None => return Err(MetaError::NotFound("member")),
                Some(Some(_)) => {
                    return Err(MetaError::Conflict("the member is disabled".to_owned()));
                }
                Some(None) => {}
            }
            let expires_at = at.saturating_add(ttl_secs);
            tx.execute(
                "INSERT INTO auth_sessions (token_hash, member_id, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![token_hash, member_id, at, expires_at],
            )
            .map_err(storage)?;
            Ok(expires_at)
        })
    }

    fn session(&self, token_hash: &str) -> Result<Option<Login>, MetaError> {
        if !is_hex_hash(token_hash) {
            return Ok(None);
        }
        let now = self.clock_now();
        self.read(|tx| {
            let raw = tx
                .query_row(
                    "SELECT m.id, m.email, m.name, m.created_at, m.disabled_at
                     FROM auth_sessions a JOIN members m ON m.id = a.member_id
                     WHERE a.token_hash = ?1 AND a.expires_at > ?2 AND m.disabled_at IS NULL",
                    params![token_hash, now],
                    member_row,
                )
                .optional()
                .map_err(storage)?;
            raw.map(|raw| {
                let member = to_member(raw)?;
                let memberships = memberships_of(tx, member.id)?;
                Ok(Login {
                    member,
                    memberships,
                })
            })
            .transpose()
        })
    }

    fn delete_session(&self, token_hash: &str) -> Result<(), MetaError> {
        self.write(|tx, _| {
            tx.execute(
                "DELETE FROM auth_sessions WHERE token_hash = ?1",
                [token_hash],
            )
            .map_err(storage)?;
            Ok(())
        })
    }

    fn delete_expired_sessions(&self) -> Result<u64, MetaError> {
        self.write(|tx, at| {
            let removed = tx
                .execute("DELETE FROM auth_sessions WHERE expires_at <= ?1", [at])
                .map_err(storage)?;
            Ok(removed as u64)
        })
    }

    fn issue_read_token(
        &self,
        site_id: u32,
        label: &str,
        token_hash: &str,
        created_by: Option<u32>,
    ) -> Result<ReadTokenInfo, MetaError> {
        check_hash(token_hash, "the read token hash")?;
        let label = crate::sqlite::clean_label(label, "a token label")?;
        self.write(|tx, at| {
            let site: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sites WHERE id = ?1)",
                    [site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !site {
                return Err(MetaError::NotFound("site"));
            }
            tx.execute(
                "INSERT INTO read_tokens (site_id, label, token_hash, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![site_id, label, token_hash, created_by, at],
            )
            .map_err(storage)?;
            Ok(ReadTokenInfo {
                id: id_of(tx.last_insert_rowid())?,
                site_id,
                label,
                created_at: at,
                revoked_at: None,
            })
        })
    }

    fn read_token(&self, token_hash: &str) -> Result<Option<ReadTokenInfo>, MetaError> {
        if !is_hex_hash(token_hash) {
            return Ok(None);
        }
        self.read(|tx| {
            let raw = tx
                .query_row(
                    "SELECT id, site_id, label, created_at, revoked_at FROM read_tokens
                     WHERE token_hash = ?1 AND revoked_at IS NULL",
                    [token_hash],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(storage)?;
            raw.map(token_of).transpose()
        })
    }

    fn read_tokens(&self, site_id: u32) -> Result<Vec<ReadTokenInfo>, MetaError> {
        self.read(|tx| {
            let mut statement = tx
                .prepare(
                    "SELECT id, site_id, label, created_at, revoked_at FROM read_tokens
                     WHERE site_id = ?1 ORDER BY id",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map([site_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })
                .map_err(storage)?;
            rows.map(|row| token_of(row.map_err(storage)?)).collect()
        })
    }

    fn revoke_read_token(&self, site_id: u32, id: u32) -> Result<(), MetaError> {
        self.write(|tx, at| {
            let found: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM read_tokens WHERE id = ?1 AND site_id = ?2)",
                    params![id, site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !found {
                return Err(MetaError::NotFound("read token"));
            }
            tx.execute(
                "UPDATE read_tokens SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![at, id],
            )
            .map_err(storage)?;
            Ok(())
        })
    }
}

/// Everything about a site a read needs besides its schemas.
pub(crate) fn site_from_row(
    id: i64,
    organization_id: i64,
    name: String,
    time_zone: String,
) -> Result<Site, MetaError> {
    Ok(Site {
        id: id_of(id)?,
        organization_id: id_of(organization_id)?,
        name,
        time_zone,
    })
}
