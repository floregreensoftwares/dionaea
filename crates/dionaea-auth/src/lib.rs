//! Who is asking, and what they may do.
//!
//! Two kinds of caller exist. A **member** logs in with an e-mail address and
//! a password and then carries a session cookie; what they may do depends on
//! their role in the organization that owns the site (see [`Permission`]). A
//! **read token** is a secret issued for one site, for scripts and
//! dashboards; it can read that site's reports and nothing else.
//!
//! Secrets are never stored: a password as an argon2id hash, a session and a
//! read token as a SHA-256. A secret is shown once, when it is made.
//!
//! Everything here is synchronous; hashing a password is deliberately slow
//! and a caller on an async runtime runs it on a blocking thread.

mod cookie;
mod limiter;
mod password;
mod policy;
mod secret;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dionaea_meta::{AccountStore, Member, MetaError, ReadTokenInfo, Role};

pub use cookie::{COOKIE_NAME, clear_cookie, from_header as cookie_from_header, set_cookie};
pub use limiter::LoginLimiter;
pub use password::{Hasher, MAX_PASSWORD_CHARS, MIN_PASSWORD_CHARS, check_policy};
pub use policy::{Permission, allows};
pub use secret::{READ_TOKEN_PREFIX, SESSION_PREFIX, Secret, generate, has_shape, hash};

#[derive(Debug)]
pub enum AuthError {
    /// The address or the password is wrong, or the account is disabled. One
    /// answer for all three, so a caller cannot tell which accounts exist.
    InvalidCredentials,
    /// Too many failed attempts; try again after this many seconds.
    TooManyAttempts {
        retry_after_secs: i64,
    },
    /// The new password does not meet the policy; says what is wrong.
    WeakPassword(String),
    /// Too many passwords are being hashed at once; try again shortly.
    Busy,
    Hash(String),
    Random(String),
    Meta(MetaError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredentials => f.write_str("the address or the password is wrong"),
            Self::TooManyAttempts { retry_after_secs } => {
                write!(
                    f,
                    "too many attempts; try again in {retry_after_secs} seconds"
                )
            }
            Self::WeakPassword(why) => write!(f, "the password needs {why}"),
            Self::Busy => f.write_str("too many passwords are being checked; try again shortly"),
            Self::Hash(message) => write!(f, "hashing failed: {message}"),
            Self::Random(message) => write!(f, "no randomness: {message}"),
            Self::Meta(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<MetaError> for AuthError {
    fn from(error: MetaError) -> Self {
        Self::Meta(error)
    }
}

/// How sessions and login limits behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthConfig {
    /// How long a session lasts, in seconds.
    pub session_ttl_secs: i64,
    /// Whether the cookie carries `Secure`. Turn it off only for plain HTTP
    /// in local development.
    pub secure_cookies: bool,
    /// Failed logins per address or client in `login_window_secs`.
    pub max_login_failures: u32,
    pub login_window_secs: i64,
    /// Passwords hashed or verified at the same time. Each takes 19 MiB, and
    /// the failures that would slow an attacker are only counted once the hash
    /// is done, so a burst of logins would otherwise all run at once.
    pub max_concurrent_hashes: usize,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session_ttl_secs: 7 * 24 * 3600,
            secure_cookies: true,
            max_login_failures: 10,
            login_window_secs: 15 * 60,
            max_concurrent_hashes: 4,
        }
    }
}

/// Where a request is aimed: an organization, and a site inside it when the
/// request is about one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub organization_id: u32,
    pub site_id: Option<u32>,
}

/// Who is asking, as far as the server could tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Member {
        member: Member,
        memberships: Vec<dionaea_meta::Membership>,
    },
    ReadToken(ReadTokenInfo),
}

/// The caller may not do that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied;

impl Principal {
    /// Checks that the caller may do `permission` on `scope`. A member needs
    /// a role in the organization that is high enough; a read token can only
    /// read, and only the site it was issued for.
    pub fn authorize(&self, permission: Permission, scope: Scope) -> Result<(), Denied> {
        match self {
            Self::Member { memberships, .. } => {
                let role = memberships
                    .iter()
                    .find(|m| m.organization_id == scope.organization_id)
                    .map(|m| m.role);
                match role {
                    Some(role) if allows(role, permission) => Ok(()),
                    _ => Err(Denied),
                }
            }
            Self::ReadToken(token) => {
                if permission == Permission::Read && scope.site_id == Some(token.site_id) {
                    Ok(())
                } else {
                    Err(Denied)
                }
            }
        }
    }

    pub fn member(&self) -> Option<&Member> {
        match self {
            Self::Member { member, .. } => Some(member),
            Self::ReadToken(_) => None,
        }
    }

    /// A short name for audit lines: the member's address or the token's id.
    pub fn describe(&self) -> String {
        match self {
            Self::Member { member, .. } => member.email.clone(),
            Self::ReadToken(token) => format!("read-token:{}", token.id),
        }
    }
}

/// A session just started: the secret goes to the client in a cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub secret: String,
    pub expires_at: i64,
    pub member: Member,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Holds one of the places for hashing a password; frees it when dropped.
struct HashSlot<'a>(&'a AtomicUsize);

impl Drop for HashSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Auth {
    hashing: AtomicUsize,
    accounts: Arc<dyn AccountStore>,
    hasher: Hasher,
    limiter: LoginLimiter,
    config: AuthConfig,
    clock: Clock,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl Auth {
    pub fn new(accounts: Arc<dyn AccountStore>, hasher: Hasher, config: AuthConfig) -> Self {
        Self {
            hashing: AtomicUsize::new(0),
            accounts,
            hasher,
            limiter: LoginLimiter::new(config.max_login_failures, config.login_window_secs),
            config,
            clock: Arc::new(wall_clock),
        }
    }

    /// Replaces the clock the login limiter uses, in seconds.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    fn slot(&self) -> Result<HashSlot<'_>, AuthError> {
        let before = self.hashing.fetch_add(1, Ordering::AcqRel);
        let slot = HashSlot(&self.hashing);
        if before >= self.config.max_concurrent_hashes {
            // Dropping the slot gives the place back.
            return Err(AuthError::Busy);
        }
        Ok(slot)
    }

    /// Adds a member after checking the password against the policy.
    pub fn create_member(
        &self,
        email: &str,
        name: &str,
        password: &str,
    ) -> Result<Member, AuthError> {
        check_policy(password)?;
        let _slot = self.slot()?;
        let hash = self.hasher.hash(password)?;
        Ok(self.accounts.create_member(email, name, &hash)?)
    }

    /// Adds a member to an organization with a role, in one step.
    pub fn create_member_in(
        &self,
        email: &str,
        name: &str,
        password: &str,
        organization_id: u32,
        role: Role,
    ) -> Result<Member, AuthError> {
        check_policy(password)?;
        let _slot = self.slot()?;
        let hash = self.hasher.hash(password)?;
        Ok(self
            .accounts
            .create_member_with_role(email, name, &hash, organization_id, role)?)
    }

    /// Replaces a member's password and ends their sessions.
    pub fn set_password(&self, member_id: u32, password: &str) -> Result<(), AuthError> {
        check_policy(password)?;
        let _slot = self.slot()?;
        let hash = self.hasher.hash(password)?;
        Ok(self.accounts.set_password_hash(member_id, &hash)?)
    }

    /// Replaces a member's password after checking the current one, and ends
    /// their sessions. A wrong current password counts as a failed login.
    pub fn change_password(
        &self,
        member: &Member,
        current: &str,
        new: &str,
        client: &str,
    ) -> Result<(), AuthError> {
        check_policy(new)?;
        let now = (self.clock)();
        let key = format!("email:{}", member.email);
        if let Some(retry_after_secs) = self.limiter.blocked_for(&key, now) {
            return Err(AuthError::TooManyAttempts { retry_after_secs });
        }
        let _slot = self.slot()?;
        let stored = self.accounts.member_for_login(&member.email)?;
        let right = current.chars().count() <= MAX_PASSWORD_CHARS
            && stored
                .as_ref()
                .is_some_and(|(_, hash)| self.hasher.verify(current, hash));
        if !right {
            self.limiter.record_failure(&key, now);
            if !client.is_empty() {
                self.limiter
                    .record_failure(&format!("client:{client}"), now);
            }
            return Err(AuthError::InvalidCredentials);
        }
        self.limiter.clear(&key);
        let hash = self.hasher.hash(new)?;
        Ok(self.accounts.set_password_hash(member.id, &hash)?)
    }

    /// Logs a member in. `client` names where the attempt comes from (an
    /// address) and is limited like the e-mail address is; pass an empty
    /// string when unknown.
    pub fn login(
        &self,
        email: &str,
        password: &str,
        client: &str,
    ) -> Result<NewSession, AuthError> {
        let now = (self.clock)();
        let email_key = format!("email:{}", email.trim().to_lowercase());
        let client_key = format!("client:{client}");
        let keys: &[&str] = if client.is_empty() {
            &[&email_key]
        } else {
            &[&email_key, &client_key]
        };
        for key in keys {
            if let Some(retry_after_secs) = self.limiter.blocked_for(key, now) {
                return Err(AuthError::TooManyAttempts { retry_after_secs });
            }
        }
        let _slot = self.slot()?;
        // A password longer than the policy allows is never hashed.
        let within_bounds = password.chars().count() <= MAX_PASSWORD_CHARS;
        let found = self.accounts.member_for_login(email)?;
        let member = match (&found, within_bounds) {
            (Some((member, hash)), true) if self.hasher.verify(password, hash) => {
                (!member.disabled).then_some(member.clone())
            }
            (None, true) => {
                self.hasher.verify_decoy(password);
                None
            }
            _ => None,
        };
        let Some(member) = member else {
            for key in keys {
                self.limiter.record_failure(key, now);
            }
            return Err(AuthError::InvalidCredentials);
        };
        self.limiter.clear(&email_key);
        let secret = generate(SESSION_PREFIX)?;
        let expires_at =
            self.accounts
                .create_session(&secret.hash, member.id, self.config.session_ttl_secs)?;
        // Expired sessions are removed as new ones are made, so the table stays
        // the size of the sessions in use. A failure here changes nothing for
        // this login.
        let _ = self.accounts.delete_expired_sessions();
        Ok(NewSession {
            secret: secret.value,
            expires_at,
            member,
        })
    }

    /// The caller behind a session secret (the value of the cookie).
    pub fn authenticate_session(&self, secret: &str) -> Result<Option<Principal>, AuthError> {
        if !has_shape(SESSION_PREFIX, secret) {
            return Ok(None);
        }
        Ok(self
            .accounts
            .session(&hash(secret))?
            .map(|login| Principal::Member {
                member: login.member,
                memberships: login.memberships,
            }))
    }

    /// The caller behind a read token.
    pub fn authenticate_read_token(&self, secret: &str) -> Result<Option<Principal>, AuthError> {
        if !has_shape(READ_TOKEN_PREFIX, secret) {
            return Ok(None);
        }
        Ok(self
            .accounts
            .read_token(&hash(secret))?
            .map(Principal::ReadToken))
    }

    /// Ends a session. A secret that is not one is ignored.
    pub fn logout(&self, secret: &str) -> Result<(), AuthError> {
        if has_shape(SESSION_PREFIX, secret) {
            self.accounts.delete_session(&hash(secret))?;
        }
        Ok(())
    }

    /// Makes a read token for a site. The secret in the result is the only
    /// time it exists in the clear.
    pub fn issue_read_token(
        &self,
        site_id: u32,
        label: &str,
        created_by: Option<u32>,
    ) -> Result<(ReadTokenInfo, String), AuthError> {
        let secret = generate(READ_TOKEN_PREFIX)?;
        let info = self
            .accounts
            .issue_read_token(site_id, label, &secret.hash, created_by)?;
        Ok((info, secret.value))
    }

    /// The `Set-Cookie` value that starts a session on the client.
    pub fn session_cookie(&self, session: &NewSession) -> String {
        set_cookie(
            &session.secret,
            self.config.session_ttl_secs,
            self.config.secure_cookies,
        )
    }

    pub fn logout_cookie(&self) -> String {
        clear_cookie(self.config.secure_cookies)
    }

    /// The role a member has in an organization, if any.
    pub fn role_of(principal: &Principal, organization_id: u32) -> Option<Role> {
        match principal {
            Principal::Member { memberships, .. } => memberships
                .iter()
                .find(|m| m.organization_id == organization_id)
                .map(|m| m.role),
            Principal::ReadToken(_) => None,
        }
    }
}
