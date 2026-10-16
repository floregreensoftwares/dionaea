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

mod http;
mod migrations;
mod rows;
mod time;

use std::fmt;
use std::time::Duration;

pub use rows::{DropRow, EventRow};

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
    pub timeout: Duration,
}

impl Config {
    pub fn new(host: impl Into<String>, port: u16, database: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            user: "default".to_owned(),
            password: String::new(),
            database: database.into(),
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
    /// Too many rows for one insert.
    TooManyRows,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "cannot reach ClickHouse: {message}"),
            Self::Server { status, message } => {
                write!(f, "ClickHouse answered {status}: {message}")
            }
            Self::InvalidRow(message) => write!(f, "a row cannot be stored: {message}"),
            Self::InvalidName(message) => write!(f, "invalid name: {message}"),
            Self::TooManyRows => write!(f, "more than {MAX_ROWS_PER_INSERT} rows in one insert"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether trying the same call again cannot succeed: the data is refused
    /// (a 4xx other than a timeout or a rate limit) or the row is invalid. A
    /// transport failure or a server error may pass on a later try. A batcher
    /// retries the second kind and sets the first kind aside, or the spool
    /// would never drain.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::InvalidRow(_) | Self::InvalidName(_) | Self::TooManyRows => true,
            Self::Server { status, .. } => {
                (400..500).contains(status) && !matches!(status, 408 | 429)
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

/// A handle to the store. It holds no connection: each call opens one.
#[derive(Debug, Clone)]
pub struct Store {
    config: Config,
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
        Ok(Self { config })
    }

    fn endpoint(&self) -> http::Endpoint<'_> {
        http::Endpoint {
            host: &self.config.host,
            port: self.config.port,
            user: &self.config.user,
            password: &self.config.password,
            timeout: self.config.timeout,
        }
    }

    /// Sends `body` to ClickHouse at `path` and returns the answer text.
    fn post(&self, path: &str, body: &[u8], redact: bool) -> Result<String, Error> {
        let response = http::post(&self.endpoint(), path, body)?;
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

    /// The `JSONEachRow` text an insert of these events sends. Exposed so the    /// cost of encoding can be measured apart from the network.    pub fn encode_events(rows: &[EventRow]) -> Result<String, Error> {        encode(rows.iter().map(EventRow::to_json_line), rows.len())    }
    /// The `JSONEachRow` text an insert of these events sends. Exposed so the
    /// cost of encoding can be measured apart from the network.
    pub fn encode_events(rows: &[EventRow]) -> Result<String, Error> {
        encode(rows.iter().map(EventRow::to_json_line), rows.len())
    }

    /// Inserts a batch of events. A second call with the same `token` stores
    /// nothing (see the crate documentation).
    pub fn insert_events(&self, rows: &[EventRow], token: &str) -> Result<(), Error> {
        let lines = Self::encode_events(rows)?;
        self.insert("events", &lines, token)
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
            "/?database={}&query={query}&insert_deduplication_token={}&async_insert=0",
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
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(server(status).is_permanent(), "{status}");
        }
        for status in [408, 429, 500, 502, 503, 504] {
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
