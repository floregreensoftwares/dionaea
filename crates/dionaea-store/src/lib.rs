//! Storage in ClickHouse: the schema (as migrations) and batch inserts.
//!
//! All SQL of the product lives in this crate, so a second backend stays
//! possible without touching the rest. It talks to ClickHouse's HTTP interface
//! with a small client of its own (plain HTTP only for now, one connection per
//! call) and uses only the standard
//! library apart from JSON encoding.
//!
//! Every value that reaches SQL is either a constant of this crate or a
//! validated identifier; rows travel as `JSONEachRow` data, never spliced
//! into a statement.
//!
//! **Deduplication.** An insert carries a token, and ClickHouse drops a block
//! whose token it has already seen (the tables keep the last 1000), so the
//! batcher can retry an insert it is unsure about, or replay a spool segment,
//! without storing the batch twice. The token should be derived from the
//! batch's position in the spool, which is stable across a replay.

mod erase;
mod http;
mod migrations;
pub mod report;
mod retention;
mod rows;
mod time;

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use erase::{IDENTIFIER_TABLES, IdentifierTable, Rewrite, TableCount};
pub use retention::Expired;
pub use rows::{AliasRow, DropRow, EventRow, TraitsRow, event_key, with_props_str};

/// Most rows one insert call accepts.
pub const MAX_ROWS_PER_INSERT: usize = 100_000;

/// How to reach ClickHouse.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Talk TLS (HTTPS) to the server. Without it the connection is plain HTTP.
    pub tls: Option<TlsConfig>,
    pub timeout: Duration,
}

/// How to verify the server when talking TLS.
#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    /// A PEM file with the certificates to trust (the server's CA). Without
    /// it the Mozilla root set is trusted.
    pub ca_file: Option<PathBuf>,
    /// The name the server's certificate must carry. Defaults to the host.
    pub server_name: Option<String>,
}

impl Config {
    pub fn new(host: impl Into<String>, port: u16, database: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            user: "default".to_owned(),
            password: String::new(),
            database: database.into(),
            tls: None,
            timeout: Duration::from_secs(30),
        }
    }
}

// The password never appears in a debug print or a log line.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<hidden>")
            .field("database", &self.database)
            .field("tls", &self.tls)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The connection or the transport failed.
    Io(String),
    /// ClickHouse answered with an error status.
    Server { status: u16, message: String },
    /// A row cannot be stored (a time out of range, a number that is not finite).
    InvalidRow(String),
    /// The database name or the deduplication token has a bad shape.
    InvalidName(String),
    /// The TLS settings cannot be used (a missing or unreadable CA file, a
    /// bad server name).
    Tls(String),
    /// Too many rows for one insert.
    TooManyRows,
    /// A read would return more data than a read may.
    AnswerTooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "cannot reach ClickHouse: {message}"),
            Self::Tls(message) => write!(f, "TLS settings: {message}"),
            Self::Server { status, message } => {
                write!(f, "ClickHouse answered {status}: {message}")
            }
            Self::InvalidRow(message) => write!(f, "a row cannot be stored: {message}"),
            Self::InvalidName(message) => write!(f, "invalid name: {message}"),
            Self::TooManyRows => write!(f, "more than {MAX_ROWS_PER_INSERT} rows in one insert"),
            Self::AnswerTooLarge => f.write_str("the answer is larger than a read may return"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// The ClickHouse error code in a server answer, when it names one
    /// (`Code: 159` is a read that ran out of time, `Code: 158` or `396` one
    /// that would return too many rows).
    pub fn server_code(&self) -> Option<u32> {
        let Self::Server { message, .. } = self else {
            return None;
        };
        let rest = message.split("Code: ").nth(1)?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }

    /// Whether trying the same call again cannot succeed: the server refused
    /// the data itself (400, 413 or 422) or the row is invalid. Everything else
    /// may pass on a later try, including 401, 403 and 404: a wrong password, a
    /// missing grant or a missing table are fixed by an operator, and a batcher
    /// that set those batches aside would silently discard the spool. A batcher
    /// retries the second kind and sets the first kind aside, or a rejected
    /// batch would block the spool forever.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::InvalidRow(_)
            | Self::InvalidName(_)
            | Self::TooManyRows
            | Self::AnswerTooLarge
            | Self::Tls(_) => true,
            Self::Server { status, .. } => {
                matches!(status, 400 | 413 | 422)
            }
            Self::Io(_) => false,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// A handle to the store. Clones share the idle connections to the server.
#[derive(Clone)]
pub struct Store {
    config: Config,
    client: Arc<http::Client>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("config", &self.config)
            .finish()
    }
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The two users a running node connects as (see
/// [`Store::provision_roles`]). The password of each is shown nowhere.
#[derive(Clone)]
pub struct Roles {
    pub ingest_user: String,
    pub ingest_password: String,
    pub query_user: String,
    pub query_password: String,
}

impl fmt::Debug for Roles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Roles")
            .field("ingest_user", &self.ingest_user)
            .field("ingest_password", &"<hidden>")
            .field("query_user", &self.query_user)
            .field("query_password", &"<hidden>")
            .finish()
    }
}

/// A password that is safe to put inside a statement: long enough to be a
/// generated token, and without quotes, backslashes or whitespace.
fn valid_secret(secret: &str) -> bool {
    (16..=128).contains(&secret.len())
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '~' | '-'))
}

/// A host name or address, as it goes into the `Host` header: no whitespace,
/// no control characters.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// ClickHouse's answer, kept short and on one line. With `redact` only the
/// error code and its name survive: the message of a rejected insert may quote
/// the row, and a row holds user content that must never reach a log.
fn summarize(body: &[u8], redact: bool) -> String {
    let text = String::from_utf8_lossy(body);
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !redact {
        return one_line.chars().take(400).collect();
    }
    let code: String = one_line
        .split_once("Code: ")
        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect())
        .unwrap_or_default();
    // The error name closes the message, as in `(INCORRECT_DATA)`, usually
    // followed by `(version ...)`: take the last `(NAME)` made of capitals.
    let name = one_line
        .match_indices('(')
        .filter_map(|(index, _)| {
            let tail = &one_line[index + 1..];
            let name: String = tail
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            (!name.is_empty() && tail[name.len()..].starts_with(')')).then_some(name)
        })
        .next_back()
        .unwrap_or_default();
    match (code.is_empty(), name.is_empty()) {
        (false, false) => format!("Code: {code} ({name})"),
        (false, true) => format!("Code: {code}"),
        _ => "ClickHouse refused the data".to_owned(),
    }
}

impl Store {
    pub fn new(config: Config) -> Result<Self, Error> {
        if !valid_host(&config.host) {
            return Err(Error::InvalidName(
                "the host is a name or address of letters, digits, dots, hyphens, colons and brackets".to_owned(),
            ));
        }
        if !valid_identifier(&config.database) {
            return Err(Error::InvalidName(
                "the database is a letter or underscore followed by letters, digits and underscores".to_owned(),
            ));
        }
        let tls = match &config.tls {
            None => None,
            Some(tls) => Some(
                http::Tls::new(
                    tls.server_name.as_deref().unwrap_or(&config.host),
                    tls.ca_file.as_deref(),
                )
                .map_err(Error::Tls)?,
            ),
        };
        let client = http::Client::new(
            &config.host,
            config.port,
            &config.user,
            &config.password,
            config.timeout,
            tls,
        );
        Ok(Self {
            config,
            client: Arc::new(client),
        })
    }

    /// How many connections this handle (and its clones) has opened. Reused
    /// connections are not counted; tests use it to show that they are reused.
    pub fn connections_opened(&self) -> u64 {
        self.client.connects()
    }

    /// Sends `body` to ClickHouse at `path` and returns the answer text.
    fn post(&self, path: &str, body: &[u8], redact: bool) -> Result<String, Error> {
        let response = self.client.post(path, body)?;
        if response.status != 200 {
            return Err(Error::Server {
                status: response.status,
                message: summarize(&response.body, redact),
            });
        }
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }

    /// Runs one statement in the configured database.
    fn execute(&self, sql: &str) -> Result<String, Error> {
        let path = format!("/?database={}", http::url_encode(&self.config.database));
        self.post(&path, sql.as_bytes(), false)
    }

    /// Whether ClickHouse answers a trivial query with these credentials.
    pub fn ping(&self) -> Result<(), Error> {
        self.post("/", b"SELECT 1", false).map(|_| ())
    }

    /// Creates the database and applies the migrations that have not been
    /// applied, in order. Returns the versions applied by this call.
    pub fn migrate(&self) -> Result<Vec<u32>, Error> {
        self.post(
            "/",
            format!("CREATE DATABASE IF NOT EXISTS {}", self.config.database).as_bytes(),
            false,
        )?;
        self.execute(
            "CREATE TABLE IF NOT EXISTS _migrations (
                version UInt32, name String, applied_at DateTime('UTC') DEFAULT now()
            ) ENGINE = MergeTree ORDER BY version",
        )?;
        let applied =
            self.execute("SELECT DISTINCT version FROM _migrations FORMAT TabSeparated")?;
        let applied: Vec<u32> = applied
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();

        let mut done = Vec::new();
        for migration in migrations::MIGRATIONS {
            if applied.contains(&migration.version) {
                continue;
            }
            for statement in migration.statements {
                self.execute(statement)?;
            }
            self.execute(&format!(
                "INSERT INTO _migrations (version, name) VALUES ({}, '{}')",
                migration.version, migration.name
            ))?;
            done.push(migration.version);
        }
        Ok(done)
    }

    /// Creates (or updates the password of) the two least-privilege users the
    /// running node should connect as, and grants them what they need in the
    /// configured database: the ingest user may only insert into the tables
    /// events arrive in, the query user may only read. Call it as the
    /// privileged user that migrates, after [`migrate`](Self::migrate).
    ///
    /// Passwords should be random tokens; they are checked for a safe shape
    /// because they go into a statement, and an error never echoes them.
    pub fn provision_roles(&self, roles: &Roles) -> Result<(), Error> {
        for name in [&roles.ingest_user, &roles.query_user] {
            if !valid_identifier(name) {
                return Err(Error::InvalidName(
                    "a user name is a letter or underscore followed by letters, digits and underscores".to_owned(),
                ));
            }
        }
        for secret in [&roles.ingest_password, &roles.query_password] {
            if !valid_secret(secret) {
                return Err(Error::InvalidName(
                    "a password is 16 to 128 characters of letters, digits, '_', '.', '~' and '-'"
                        .to_owned(),
                ));
            }
        }
        let database = &self.config.database;
        let (ingest, query) = (&roles.ingest_user, &roles.query_user);
        let statements = [
            format!(
                "CREATE USER IF NOT EXISTS {ingest} IDENTIFIED WITH sha256_password BY '{}'",
                roles.ingest_password
            ),
            format!(
                "ALTER USER {ingest} IDENTIFIED WITH sha256_password BY '{}'",
                roles.ingest_password
            ),
            format!("GRANT INSERT ON {database}.events TO {ingest}"),
            format!("GRANT INSERT ON {database}.drops_daily TO {ingest}"),
            format!("GRANT INSERT ON {database}.identity_alias TO {ingest}"),
            format!("GRANT INSERT ON {database}.user_traits TO {ingest}"),
            // The ids of recent events, so a node can tell that another stored
            // an event it is about to insert. Only that table is readable.
            format!("GRANT INSERT, SELECT ON {database}.event_ids TO {ingest}"),
            format!(
                "CREATE USER IF NOT EXISTS {query} IDENTIFIED WITH sha256_password BY '{}'",
                roles.query_password
            ),
            format!(
                "ALTER USER {query} IDENTIFIED WITH sha256_password BY '{}'",
                roles.query_password
            ),
            format!("GRANT SELECT ON {database}.* TO {query}"),
        ];
        let path = format!("/?database={}", http::url_encode(database));
        for statement in &statements {
            // Redacted: these statements carry secrets.
            self.post(&path, statement.as_bytes(), true)?;
        }
        Ok(())
    }

    /// The `JSONEachRow` text an insert of these events sends. Exposed so the
    /// cost of encoding can be measured apart from the network.
    pub fn encode_events(rows: &[EventRow]) -> Result<String, Error> {
        encode(rows.iter().map(EventRow::to_json_line), rows.len())
    }

    /// The `JSONEachRow` text an insert of these drop counts sends.
    pub fn encode_drops(rows: &[DropRow]) -> Result<String, Error> {
        encode(rows.iter().map(DropRow::to_json_line), rows.len())
    }

    /// The `JSONEachRow` text an insert of these identity links sends.
    pub fn encode_aliases(rows: &[AliasRow]) -> Result<String, Error> {
        encode(rows.iter().map(AliasRow::to_json_line), rows.len())
    }

    /// The `JSONEachRow` text an insert of these user traits sends.
    pub fn encode_traits(rows: &[TraitsRow]) -> Result<String, Error> {
        encode(rows.iter().map(TraitsRow::to_json_line), rows.len())
    }

    /// Inserts lines produced by [`encode_aliases`](Self::encode_aliases).
    pub fn insert_alias_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert("identity_alias", lines, token)
    }

    /// Inserts lines produced by [`encode_traits`](Self::encode_traits).
    pub fn insert_traits_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert("user_traits", lines, token)
    }

    /// Inserts lines already produced by [`encode_events`](Self::encode_events)
    /// (a spool can hold them and join several batches into one insert). The
    /// lines are sent as they are: they must come from `encode_events`.
    pub fn insert_event_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert("events", lines, token)
    }

    /// The same for lines produced by [`encode_drops`](Self::encode_drops).
    pub fn insert_drop_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert("drops_daily", lines, token)
    }

    /// Inserts a batch of events. A second call with the same `token` stores
    /// nothing (see the crate documentation).
    pub fn insert_events(&self, rows: &[EventRow], token: &str) -> Result<(), Error> {
        let lines = Self::encode_events(rows)?;
        self.insert("events", &lines, token)
    }

    /// The lines recording that these events were stored at `seen_ms`, for
    /// [`insert_event_id_lines`](Self::insert_event_id_lines). Each key is a
    /// site id and the canonical text of an event id.
    pub fn encode_event_ids<'a>(
        keys: impl IntoIterator<Item = (u32, &'a str)>,
        seen_ms: u64,
    ) -> Result<String, Error> {
        let seen = time::format_datetime(seen_ms);
        let mut out = String::new();
        for (site, id) in keys {
            if !rows::is_uuid(id) {
                return Err(Error::InvalidRow(
                    "the event id is not a canonical UUID".to_owned(),
                ));
            }
            out.push_str(&format!(
                "{{\"site_id\":{site},\"event_id\":\"{id}\",\"seen_at\":\"{seen}\"}}\n"
            ));
        }
        Ok(out)
    }

    /// Records event ids as stored. The same `token` twice stores them once.
    pub fn insert_event_id_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert("event_ids", lines, token)
    }

    /// Which of these (site, event id) pairs were already recorded by any
    /// node. At most [`MAX_IDS_PER_LOOKUP`] at a time.
    pub fn known_event_ids(&self, keys: &[(u32, &str)]) -> Result<HashSet<(u32, String)>, Error> {
        if keys.is_empty() {
            return Ok(HashSet::new());
        }
        let sql = known_ids_sql(keys)?;
        let answer = self.execute(&sql)?;
        let mut known = HashSet::with_capacity(keys.len().min(1024));
        for line in answer.lines() {
            if let Some((site, id)) = line.split_once('\t')
                && let Ok(site) = site.parse()
            {
                known.insert((site, id.trim().to_owned()));
            }
        }
        Ok(known)
    }

    /// Inserts daily counts of dropped events.
    pub fn insert_drops(&self, rows: &[DropRow], token: &str) -> Result<(), Error> {
        let lines = encode(rows.iter().map(DropRow::to_json_line), rows.len())?;
        self.insert("drops_daily", &lines, token)
    }

    fn insert(&self, table: &str, lines: &str, token: &str) -> Result<(), Error> {
        if lines.is_empty() {
            return Ok(());
        }
        if !valid_token(token) {
            return Err(Error::InvalidName(
                "the deduplication token has a bad shape".to_owned(),
            ));
        }
        // `table` is always one of this crate's constants.
        let query = http::url_encode(&format!("INSERT INTO {table} FORMAT JSONEachRow"));
        let path = format!(
            "/?database={}&query={query}&insert_deduplication_token={}&async_insert=0&deduplicate_blocks_in_dependent_materialized_views=1",
            http::url_encode(&self.config.database),
            http::url_encode(token),
        );
        self.post(&path, lines.as_bytes(), true).map(|_| ())
    }

    /// Runs a statement and returns its answer as text. For tests and tools:
    /// never pass it anything that came from outside.
    pub fn raw_query(&self, sql: &str) -> Result<String, Error> {
        self.execute(sql)
    }
}

/// The most ids one [`Store::known_event_ids`] call takes.
/// Kept under the 256 KiB `max_query_size` a ClickHouse server allows by
/// default: each id is about 53 characters in the statement.
pub const MAX_IDS_PER_LOOKUP: usize = 4_000;

/// The statement that looks up `keys`. Every id is checked to be a canonical
/// UUID before it is written into the text, and a site id is a number, so no
/// part of it comes from outside unchecked.
fn known_ids_sql(keys: &[(u32, &str)]) -> Result<String, Error> {
    if keys.len() > MAX_IDS_PER_LOOKUP {
        return Err(Error::TooManyRows);
    }
    let mut sql = String::from(
        "SELECT site_id, toString(event_id) FROM event_ids WHERE (site_id, event_id) IN (",
    );
    for (index, (site, id)) in keys.iter().enumerate() {
        if !rows::is_uuid(id) {
            return Err(Error::InvalidRow(
                "the event id is not a canonical UUID".to_owned(),
            ));
        }
        if index > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({site},'{id}')"));
    }
    sql.push(')');
    Ok(sql)
}

fn encode(
    lines: impl Iterator<Item = Result<String, Error>>,
    count: usize,
) -> Result<String, Error> {
    if count > MAX_ROWS_PER_INSERT {
        return Err(Error::TooManyRows);
    }
    let mut out = String::new();
    for line in lines {
        out.push_str(&line?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(ingest_user: &str, password: &str) -> Roles {
        Roles {
            ingest_user: ingest_user.to_owned(),
            ingest_password: password.to_owned(),
            query_user: "query_a".to_owned(),
            query_password: "0123456789abcdef0123".to_owned(),
        }
    }

    #[test]
    fn passwords_must_be_safe_to_put_in_a_statement() {
        for good in ["0123456789abcdef", "a.b_c~d-0123456789", &"x".repeat(128)] {
            assert!(valid_secret(good), "{good}");
        }
        for bad in [
            "",
            "short",
            "0123456789abcde",
            "has space 0123456789",
            "quote' 0123456789ab",
            "back\\slash0123456789",
            "semi;colon0123456789",
            "é0123456789abcdef",
            &"x".repeat(129),
        ] {
            assert!(!valid_secret(bad), "{bad:?}");
        }
    }

    #[test]
    fn provisioning_rejects_bad_names_and_passwords_before_any_request() {
        let mut config = Config::new("127.0.0.1", 1, "dionaea");
        config.timeout = std::time::Duration::from_millis(200);
        let store = Store::new(config).unwrap();
        let good = "0123456789abcdef0123";
        for roles in [
            roles("bad name", good),
            roles("ok_user", "short"),
            roles("x; DROP USER y", good),
            roles("ok_user", "quote'quote'quote'quote'"),
        ] {
            assert!(
                matches!(store.provision_roles(&roles), Err(Error::InvalidName(_))),
                "{roles:?}"
            );
        }
        assert_eq!(store.connections_opened(), 0);
    }

    #[test]
    fn roles_never_show_their_passwords() {
        let shown = format!("{:?}", roles("ingest_a", "SECRET-0123456789abcdef"));
        assert!(shown.contains("ingest_a"));
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn identifiers_and_tokens_are_checked() {
        for good in ["dionaea", "_x", "a1_b2"] {
            assert!(valid_identifier(good), "{good}");
        }
        for bad in ["", "1a", "a-b", "a b", "a;drop", "é", &"a".repeat(65)] {
            assert!(!valid_identifier(bad), "{bad:?}");
        }
        for good in ["a", "0001-0002-3", "seg1:off200:n50", "x.y_z"] {
            assert!(valid_token(good), "{good}");
        }
        for bad in ["", "a b", "a&b", "a/b", "a=b", "a%20", &"a".repeat(129)] {
            assert!(!valid_token(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_bad_database_name_is_refused_before_any_connection() {
        let error = Store::new(Config::new("localhost", 8123, "bad name;")).unwrap_err();
        assert!(matches!(error, Error::InvalidName(_)));
    }

    #[test]
    fn an_insert_error_keeps_the_code_and_name_but_none_of_the_row() {
        let body = b"Code: 117. DB::Exception: Cannot parse JSON object here: {\"anon_id\":\"secret-user-42\",\"coupon\":\"SAVE\"}: (at row 3): While executing JSONEachRowRowInputFormat. (INCORRECT_DATA) (version 26.8.27.9)\n";
        let redacted = summarize(body, true);
        assert_eq!(redacted, "Code: 117 (INCORRECT_DATA)");
        assert!(!redacted.contains("secret-user-42"));
        // Other statements carry no row data, so their message is kept.
        assert!(summarize(body, false).contains("Cannot parse JSON object"));
        // An answer that is not a ClickHouse error says nothing about the data.
        assert_eq!(
            summarize(b"<html>bad gateway with secret-user-42</html>", true),
            "ClickHouse refused the data"
        );
        assert_eq!(summarize(b"", true), "ClickHouse refused the data");
    }

    #[test]
    fn only_errors_a_retry_cannot_fix_are_permanent() {
        let server = |status| Error::Server {
            status,
            message: String::new(),
        };
        for status in [400, 413, 422] {
            assert!(server(status).is_permanent(), "{status}");
        }
        for status in [401, 403, 404, 408, 409, 429, 500, 502, 503, 504] {
            assert!(!server(status).is_permanent(), "{status}");
        }
        assert!(!Error::Io("reset".to_owned()).is_permanent());
        assert!(Error::InvalidRow("x".to_owned()).is_permanent());
        assert!(Error::InvalidName("x".to_owned()).is_permanent());
        assert!(Error::TooManyRows.is_permanent());
    }

    #[test]
    fn a_host_that_could_alter_the_request_is_refused() {
        for bad in [
            "",
            "a b",
            "host\r\nX-Evil: 1",
            "host\n",
            "ho/st",
            "ho@st",
            &"a".repeat(254),
        ] {
            let config = Config::new(bad, 8123, "dionaea");
            assert!(
                matches!(Store::new(config), Err(Error::InvalidName(_))),
                "{bad:?}"
            );
        }
        for good in [
            "localhost",
            "127.0.0.1",
            "clickhouse.internal",
            "[::1]",
            "ch-1_a",
        ] {
            assert!(
                Store::new(Config::new(good, 8123, "dionaea")).is_ok(),
                "{good}"
            );
        }
    }
    #[test]
    fn the_debug_form_hides_the_password() {
        let mut config = Config::new("localhost", 8123, "dionaea");
        config.password = "s3cret-value".to_owned();
        let config_text = format!("{config:?}");
        let text = format!("{config_text} {:?}", Store::new(config).unwrap());
        assert!(!text.contains("s3cret-value"), "{text}");
        assert!(text.contains("<hidden>"));
    }

    #[test]
    fn server_errors_are_summarised_on_one_line_without_data() {
        let body = b"Code: 60. DB::Exception: Table dionaea.nope does not exist.\n(UNKNOWN_TABLE) (version 26.8)\n";
        let summary = summarize(body, false);
        assert!(!summary.contains('\n'));
        assert!(summary.starts_with("Code: 60."));
        assert!(summarize(&[b'x'; 5000], false).len() <= 400);
    }

    #[test]
    fn an_empty_batch_is_a_no_op_that_needs_no_connection() {
        // Port 1 is closed: reaching it would be an error.
        let store = Store::new(Config::new("127.0.0.1", 1, "dionaea")).unwrap();
        assert_eq!(store.insert_events(&[], "t"), Ok(()));
        assert_eq!(store.insert_drops(&[], "t"), Ok(()));
    }

    #[test]
    fn a_bad_token_is_refused_before_any_connection() {
        let store = Store::new(Config::new("127.0.0.1", 1, "dionaea")).unwrap();
        let row = DropRow {
            site_id: 1,
            event_name: "e".to_owned(),
            reason: "r".to_owned(),
            at_ms: 0,
            count: 1,
        };
        assert!(matches!(
            store.insert_drops(&[row], "bad token"),
            Err(Error::InvalidName(_))
        ));
    }

    #[test]
    fn an_unreachable_server_is_an_io_error_not_a_panic() {
        let mut config = Config::new("127.0.0.1", 1, "dionaea");
        config.timeout = Duration::from_millis(500);
        assert!(matches!(
            Store::new(config).unwrap().ping(),
            Err(Error::Io(_))
        ));
    }
}

#[cfg(test)]
mod id_tests;
