use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

const ENV_PREFIX: &str = "DIONAEA_";

/// A spool smaller than this could not hold a few of the largest requests.
const MIN_SPOOL_QUOTA: u64 = 32 << 20;

/// A secret that never shows in `Debug` output, so it cannot reach a log.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct Secret(pub(crate) String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<hidden>")
    }
}

/// Every limit and interval of the running server. Each has a default and can
/// be set in the file or by `DIONAEA_<NAME>`; a value outside its range is a
/// startup error, not a silent clamp.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tuning {
    /// Requests per second and burst allowed per write key.
    pub(crate) key_rate_per_sec: f64,
    pub(crate) key_burst: f64,
    /// The same per client address (an IPv6 client counts by its /64).
    pub(crate) address_rate_per_sec: f64,
    pub(crate) address_burst: f64,
    /// How many keys and addresses each limiter remembers.
    pub(crate) tracked_keys: usize,
    /// How long a client has to deliver a whole request body.
    pub(crate) body_timeout: Duration,
    /// The `Retry-After` of a `503`, in seconds.
    pub(crate) retry_after_secs: u64,
    /// How far back and ahead an event time may be, after skew correction.
    pub(crate) max_event_age: Duration,
    pub(crate) max_event_future: Duration,
    /// Open connections at once; further ones are closed at once.
    pub(crate) max_connections: usize,
    /// How long a connection has to deliver the headers of a request, which
    /// also bounds how long an idle keep-alive connection is kept.
    pub(crate) header_timeout: Duration,
    /// How often keys and schemas are checked for changes.
    pub(crate) refresh_every: Duration,
    pub(crate) batch_max_records: usize,
    pub(crate) batch_max_bytes: usize,
    pub(crate) batch_min_interval: Duration,
    /// How many reverse proxies sit in front of the server. With 0 the client
    /// address is the socket peer and `client_address_header` is ignored.
    pub(crate) trusted_proxy_hops: u8,
    /// The header those proxies append the client address to.
    pub(crate) client_address_header: String,
    /// How many recent event ids are remembered to drop retried events, and
    /// for how long. 0 turns the check off.
    pub(crate) dedup_capacity: usize,
    pub(crate) dedup_window: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    /// Where `/healthz`, `/readyz` and `/metrics` are served, apart from the
    /// public listener so they need not be reachable from the internet.
    pub(crate) ops_listen: SocketAddr,
    pub(crate) clickhouse_url: String,
    pub(crate) clickhouse_user: String,
    pub(crate) clickhouse_password: Secret,
    pub(crate) clickhouse_database: String,
    /// A PEM file with the certificates to trust for an `https://` ClickHouse;
    /// without it the public roots are trusted.
    pub(crate) clickhouse_ca_file: Option<PathBuf>,
    /// The name the ClickHouse certificate must carry, when it is not the host.
    pub(crate) clickhouse_server_name: Option<String>,
    pub(crate) spool_dir: PathBuf,
    pub(crate) spool_quota_bytes: u64,
    pub(crate) metadata_path: PathBuf,
    /// A MaxMind country database (`.mmdb`); without it events have no country.
    pub(crate) geoip_path: Option<PathBuf>,
    pub(crate) tuning: Tuning,
    pub(crate) log_level: String,
    pub(crate) log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    File(String),
    Invalid { key: &'static str, reason: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(reason) => write!(f, "configuration file: {reason}"),
            Self::Invalid { key, reason } => write!(f, "configuration `{key}`: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen: Option<String>,
    ops_listen: Option<String>,
    clickhouse_url: Option<String>,
    clickhouse_user: Option<String>,
    clickhouse_password: Option<String>,
    clickhouse_database: Option<String>,
    clickhouse_ca_file: Option<PathBuf>,
    clickhouse_server_name: Option<String>,
    spool_dir: Option<PathBuf>,
    spool_quota_bytes: Option<u64>,
    metadata_path: Option<PathBuf>,
    geoip_path: Option<PathBuf>,
    log_level: Option<String>,
    log_format: Option<LogFormat>,
    key_rate_per_sec: Option<f64>,
    key_burst: Option<f64>,
    address_rate_per_sec: Option<f64>,
    address_burst: Option<f64>,
    tracked_keys: Option<usize>,
    body_timeout_secs: Option<u64>,
    retry_after_secs: Option<u64>,
    max_event_age_days: Option<u64>,
    max_event_future_secs: Option<u64>,
    max_connections: Option<usize>,
    header_timeout_secs: Option<u64>,
    refresh_secs: Option<u64>,
    batch_max_records: Option<usize>,
    batch_max_bytes: Option<usize>,
    batch_min_interval_ms: Option<u64>,
    trusted_proxy_hops: Option<u8>,
    client_address_header: Option<String>,
    dedup_capacity: Option<usize>,
    dedup_window_secs: Option<u64>,
}

/// Reads one setting: the environment first, then the file, then the default;
/// a value that does not parse or breaks `valid` is a typed error.
struct Source<E> {
    env: E,
}

impl<E: Fn(&str) -> Option<String>> Source<E> {
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(&format!("{ENV_PREFIX}{name}"))
    }

    fn pick<T: FromStr>(
        &self,
        name: &str,
        key: &'static str,
        file: Option<T>,
        default: T,
        valid: impl Fn(&T) -> bool,
        rule: &str,
    ) -> Result<T, ConfigError> {
        let value = match self.var(name) {
            Some(text) => text.trim().parse().map_err(|_| ConfigError::Invalid {
                key,
                reason: format!("`{text}` is not a valid value; {rule}"),
            })?,
            None => file.unwrap_or(default),
        };
        if valid(&value) {
            Ok(value)
        } else {
            Err(ConfigError::Invalid {
                key,
                reason: rule.to_owned(),
            })
        }
    }

    fn text(&self, name: &str, file: Option<String>, default: &str) -> String {
        self.var(name)
            .or(file)
            .unwrap_or_else(|| default.to_owned())
    }

    fn path(&self, name: &str, file: Option<PathBuf>) -> Option<PathBuf> {
        self.var(name).map(PathBuf::from).or(file)
    }
}

fn seconds(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn rate(value: &f64) -> bool {
    value.is_finite() && *value > 0.0 && *value <= 1e9
}

fn burst(value: &f64) -> bool {
    value.is_finite() && (1.0..=1e9).contains(value)
}

fn header_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Config {
    /// Defaults, then the file, then `DIONAEA_*` environment variables.
    /// `env` is injected so resolution stays free of ambient state.
    pub(crate) fn resolve(
        file: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let file: FileConfig = match file {
            Some(text) => toml::from_str(text).map_err(|e| ConfigError::File(e.to_string()))?,
            None => FileConfig::default(),
        };
        let source = Source { env };

        let address = |name: &str, key: &'static str, file: Option<String>, default: &str| {
            source
                .text(name, file, default)
                .parse::<SocketAddr>()
                .map_err(|e| ConfigError::Invalid {
                    key,
                    reason: e.to_string(),
                })
        };
        let listen = address("LISTEN", "listen", file.listen, "127.0.0.1:8080")?;
        let ops_listen = address(
            "OPS_LISTEN",
            "ops_listen",
            file.ops_listen,
            "127.0.0.1:9100",
        )?;

        let clickhouse_url = source.text(
            "CLICKHOUSE_URL",
            file.clickhouse_url,
            "http://127.0.0.1:8123",
        );
        clickhouse_endpoint(&clickhouse_url).map_err(|reason| ConfigError::Invalid {
            key: "clickhouse_url",
            reason,
        })?;
        let clickhouse_user = source.text("CLICKHOUSE_USER", file.clickhouse_user, "default");
        let clickhouse_password =
            Secret(source.text("CLICKHOUSE_PASSWORD", file.clickhouse_password, ""));
        let clickhouse_database =
            source.text("CLICKHOUSE_DATABASE", file.clickhouse_database, "dionaea");
        let clickhouse_ca_file = source.path("CLICKHOUSE_CA_FILE", file.clickhouse_ca_file);
        let clickhouse_server_name = source
            .var("CLICKHOUSE_SERVER_NAME")
            .or(file.clickhouse_server_name);

        let spool_dir = source
            .path("SPOOL_DIR", file.spool_dir)
            .unwrap_or_else(|| PathBuf::from("spool"));
        let spool_quota_bytes = source.pick(
            "SPOOL_QUOTA_BYTES",
            "spool_quota_bytes",
            file.spool_quota_bytes,
            1 << 30,
            |v| *v >= MIN_SPOOL_QUOTA,
            "at least 33554432 (32 MiB)",
        )?;
        let metadata_path = source
            .path("METADATA_PATH", file.metadata_path)
            .unwrap_or_else(|| PathBuf::from("dionaea.db"));
        let geoip_path = source.path("GEOIP_PATH", file.geoip_path);

        let tuning = Tuning {
            key_rate_per_sec: source.pick(
                "KEY_RATE_PER_SEC",
                "key_rate_per_sec",
                file.key_rate_per_sec,
                200.0,
                rate,
                "a number above 0 and at most 1e9",
            )?,
            key_burst: source.pick(
                "KEY_BURST",
                "key_burst",
                file.key_burst,
                400.0,
                burst,
                "a number from 1 to 1e9",
            )?,
            address_rate_per_sec: source.pick(
                "ADDRESS_RATE_PER_SEC",
                "address_rate_per_sec",
                file.address_rate_per_sec,
                50.0,
                rate,
                "a number above 0 and at most 1e9",
            )?,
            address_burst: source.pick(
                "ADDRESS_BURST",
                "address_burst",
                file.address_burst,
                100.0,
                burst,
                "a number from 1 to 1e9",
            )?,
            tracked_keys: source.pick(
                "TRACKED_KEYS",
                "tracked_keys",
                file.tracked_keys,
                100_000,
                |v| (1..=10_000_000).contains(v),
                "from 1 to 10000000",
            )?,
            body_timeout: seconds(source.pick(
                "BODY_TIMEOUT_SECS",
                "body_timeout_secs",
                file.body_timeout_secs,
                10,
                |v| (1..=300).contains(v),
                "from 1 to 300 seconds",
            )?),
            retry_after_secs: source.pick(
                "RETRY_AFTER_SECS",
                "retry_after_secs",
                file.retry_after_secs,
                5,
                |v| (1..=3600).contains(v),
                "from 1 to 3600 seconds",
            )?,
            max_event_age: seconds(
                86_400
                    * source.pick(
                        "MAX_EVENT_AGE_DAYS",
                        "max_event_age_days",
                        file.max_event_age_days,
                        30,
                        |v| (1..=3650).contains(v),
                        "from 1 to 3650 days",
                    )?,
            ),
            max_event_future: seconds(source.pick(
                "MAX_EVENT_FUTURE_SECS",
                "max_event_future_secs",
                file.max_event_future_secs,
                3600,
                |v| *v <= 86_400,
                "at most 86400 seconds",
            )?),
            max_connections: source.pick(
                "MAX_CONNECTIONS",
                "max_connections",
                file.max_connections,
                10_000,
                |v| (1..=1_000_000).contains(v),
                "from 1 to 1000000",
            )?,
            header_timeout: seconds(source.pick(
                "HEADER_TIMEOUT_SECS",
                "header_timeout_secs",
                file.header_timeout_secs,
                10,
                |v| (1..=300).contains(v),
                "from 1 to 300 seconds",
            )?),
            refresh_every: seconds(source.pick(
                "REFRESH_SECS",
                "refresh_secs",
                file.refresh_secs,
                2,
                |v| (1..=3600).contains(v),
                "from 1 to 3600 seconds",
            )?),
            batch_max_records: source.pick(
                "BATCH_MAX_RECORDS",
                "batch_max_records",
                file.batch_max_records,
                1000,
                |v| (1..=1000).contains(v),
                "from 1 to 1000",
            )?,
            batch_max_bytes: source.pick(
                "BATCH_MAX_BYTES",
                "batch_max_bytes",
                file.batch_max_bytes,
                8 << 20,
                |v| ((64 << 10)..=(64 << 20)).contains(v),
                "from 65536 to 67108864",
            )?,
            batch_min_interval: Duration::from_millis(source.pick(
                "BATCH_MIN_INTERVAL_MS",
                "batch_min_interval_ms",
                file.batch_min_interval_ms,
                200,
                |v| *v <= 60_000,
                "at most 60000 milliseconds",
            )?),
            trusted_proxy_hops: source.pick(
                "TRUSTED_PROXY_HOPS",
                "trusted_proxy_hops",
                file.trusted_proxy_hops,
                0,
                |v| *v <= 8,
                "from 0 to 8",
            )?,
            client_address_header: source.pick(
                "CLIENT_ADDRESS_HEADER",
                "client_address_header",
                file.client_address_header,
                "x-forwarded-for".to_owned(),
                |v| header_name(v),
                "a lowercase header name",
            )?,
            dedup_capacity: source.pick(
                "DEDUP_CAPACITY",
                "dedup_capacity",
                file.dedup_capacity,
                1_000_000,
                |v| *v <= 100_000_000,
                "at most 100000000 (0 turns it off)",
            )?,
            dedup_window: seconds(source.pick(
                "DEDUP_WINDOW_SECS",
                "dedup_window_secs",
                file.dedup_window_secs,
                600,
                |v| (1..=86_400).contains(v),
                "from 1 to 86400 seconds",
            )?),
        };

        let log_level = source.text("LOG_LEVEL", file.log_level, "info");
        let log_format = match source.var("LOG_FORMAT") {
            Some(value) => match value.as_str() {
                "json" => LogFormat::Json,
                "text" => LogFormat::Text,
                _ => {
                    return Err(ConfigError::Invalid {
                        key: "log_format",
                        reason: "expected `json` or `text`".to_owned(),
                    });
                }
            },
            None => file.log_format.unwrap_or(LogFormat::Json),
        };

        Ok(Self {
            listen,
            ops_listen,
            clickhouse_url,
            clickhouse_user,
            clickhouse_password,
            clickhouse_database,
            clickhouse_ca_file,
            clickhouse_server_name,
            spool_dir,
            spool_quota_bytes,
            metadata_path,
            geoip_path,
            tuning,
            log_level,
            log_format,
        })
    }
}

/// Where ClickHouse is, from an `http://host[:port]` or `https://host[:port]`
/// URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tls: bool,
}

pub(crate) fn clickhouse_endpoint(url: &str) -> Result<Endpoint, String> {
    let (rest, tls, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, true, 8443)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, false, 8123)
    } else {
        return Err("expected an `http://host[:port]` or `https://host[:port]` URL".to_owned());
    };
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains(['/', '@', '?', '#']) {
        return Err("expected a host and optional port with no path or credentials".to_owned());
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse().map_err(|_| "invalid port".to_owned())?;
            Ok(Endpoint {
                host: host.to_owned(),
                port,
                tls,
            })
        }
        Some(_) => Err("missing host".to_owned()),
        None => Ok(Endpoint {
            host: authority.to_owned(),
            port: default_port,
            tls,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn with(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |asked| (asked == format!("DIONAEA_{name}")).then(|| value.to_owned())
    }

    fn invalid_key(result: Result<Config, ConfigError>) -> &'static str {
        match result {
            Err(ConfigError::Invalid { key, .. }) => key,
            other => panic!("expected an invalid setting, got {other:?}"),
        }
    }

    #[test]
    fn defaults_apply_without_file_or_env() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.ops_listen, "127.0.0.1:9100".parse().unwrap());
        assert_eq!(config.clickhouse_url, "http://127.0.0.1:8123");
        assert_eq!(config.metadata_path, PathBuf::from("dionaea.db"));
        assert_eq!(config.log_level, "info");
        assert_eq!(config.log_format, LogFormat::Json);
        assert_eq!(
            config.tuning.max_event_age,
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(config.tuning.trusted_proxy_hops, 0);
    }

    #[test]
    fn file_overrides_defaults() {
        let file = "listen = \"0.0.0.0:9000\"\nlog_format = \"text\"\n";
        let config = Config::resolve(Some(file), no_env).unwrap();
        assert_eq!(config.listen, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(config.log_format, LogFormat::Text);
    }

    #[test]
    fn environment_overrides_file() {
        let file = "listen = \"0.0.0.0:9000\"\n";
        let config = Config::resolve(Some(file), with("LISTEN", "127.0.0.1:7000")).unwrap();
        assert_eq!(config.listen, "127.0.0.1:7000".parse().unwrap());
    }

    #[test]
    fn the_metadata_path_comes_from_the_file_and_the_environment() {
        let file = "metadata_path = \"/data/meta.db\"\n";
        let config = Config::resolve(Some(file), no_env).unwrap();
        assert_eq!(config.metadata_path, PathBuf::from("/data/meta.db"));
        let config = Config::resolve(Some(file), with("METADATA_PATH", "other.db")).unwrap();
        assert_eq!(config.metadata_path, PathBuf::from("other.db"));
    }

    #[test]
    fn unknown_file_key_is_rejected() {
        let error = Config::resolve(Some("lisen = \"x\"\n"), no_env).unwrap_err();
        assert!(matches!(error, ConfigError::File(_)));
    }

    #[test]
    fn invalid_addresses_are_typed_errors() {
        assert_eq!(
            invalid_key(Config::resolve(None, with("LISTEN", "not-an-address"))),
            "listen"
        );
        assert_eq!(
            invalid_key(Config::resolve(None, with("OPS_LISTEN", "nope"))),
            "ops_listen"
        );
    }

    #[test]
    fn invalid_log_format_is_a_typed_error() {
        assert_eq!(
            invalid_key(Config::resolve(None, with("LOG_FORMAT", "xml"))),
            "log_format"
        );
    }

    #[test]
    fn spool_and_clickhouse_settings_have_defaults_and_overrides() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.spool_dir, PathBuf::from("spool"));
        assert_eq!(config.spool_quota_bytes, 1 << 30);
        assert_eq!(config.clickhouse_user, "default");
        assert_eq!(config.clickhouse_database, "dionaea");

        let file = "spool_dir = \"/var/spool/d\"\nspool_quota_bytes = 50000000\nclickhouse_user = \"ingest\"\n";
        let config = Config::resolve(Some(file), with("SPOOL_QUOTA_BYTES", "70000000")).unwrap();
        assert_eq!(config.spool_dir, PathBuf::from("/var/spool/d"));
        assert_eq!(config.spool_quota_bytes, 70_000_000);
        assert_eq!(config.clickhouse_user, "ingest");
    }

    #[test]
    fn a_bad_or_tiny_spool_quota_is_a_typed_error() {
        for value in ["abc", "-1", "1000", "33554431"] {
            assert_eq!(
                invalid_key(Config::resolve(None, with("SPOOL_QUOTA_BYTES", value))),
                "spool_quota_bytes",
                "{value}"
            );
        }
    }

    #[test]
    fn every_limit_can_be_set_and_is_checked_against_its_range() {
        let file = "key_rate_per_sec = 1000.5\nkey_burst = 2000\naddress_rate_per_sec = 5\n\
                    address_burst = 10\ntracked_keys = 500\nbody_timeout_secs = 30\n\
                    retry_after_secs = 9\nmax_event_age_days = 90\nmax_event_future_secs = 60\n\
                    max_connections = 77\nheader_timeout_secs = 4\nrefresh_secs = 7\n\
                    batch_max_records = 10\nbatch_max_bytes = 100000\nbatch_min_interval_ms = 0\n\
                    trusted_proxy_hops = 2\nclient_address_header = \"x-real-ip\"\n\
                    dedup_capacity = 0\ndedup_window_secs = 5\n";
        let t = Config::resolve(Some(file), no_env).unwrap().tuning;
        assert_eq!((t.key_rate_per_sec, t.key_burst), (1000.5, 2000.0));
        assert_eq!((t.address_rate_per_sec, t.address_burst), (5.0, 10.0));
        assert_eq!(t.tracked_keys, 500);
        assert_eq!(t.body_timeout, Duration::from_secs(30));
        assert_eq!(t.retry_after_secs, 9);
        assert_eq!(t.max_event_age, Duration::from_secs(90 * 86_400));
        assert_eq!(t.max_event_future, Duration::from_secs(60));
        assert_eq!(t.max_connections, 77);
        assert_eq!(t.header_timeout, Duration::from_secs(4));
        assert_eq!(t.refresh_every, Duration::from_secs(7));
        assert_eq!((t.batch_max_records, t.batch_max_bytes), (10, 100_000));
        assert_eq!(t.batch_min_interval, Duration::ZERO);
        assert_eq!(t.trusted_proxy_hops, 2);
        assert_eq!(t.client_address_header, "x-real-ip");
        assert_eq!(
            (t.dedup_capacity, t.dedup_window),
            (0, Duration::from_secs(5))
        );

        for (name, key, bad) in [
            ("KEY_RATE_PER_SEC", "key_rate_per_sec", "0"),
            ("KEY_RATE_PER_SEC", "key_rate_per_sec", "nan"),
            ("KEY_BURST", "key_burst", "0.5"),
            ("ADDRESS_RATE_PER_SEC", "address_rate_per_sec", "-1"),
            ("TRACKED_KEYS", "tracked_keys", "0"),
            ("BODY_TIMEOUT_SECS", "body_timeout_secs", "0"),
            ("RETRY_AFTER_SECS", "retry_after_secs", "99999"),
            ("MAX_EVENT_AGE_DAYS", "max_event_age_days", "0"),
            ("MAX_EVENT_FUTURE_SECS", "max_event_future_secs", "999999"),
            ("MAX_CONNECTIONS", "max_connections", "0"),
            ("HEADER_TIMEOUT_SECS", "header_timeout_secs", "301"),
            ("REFRESH_SECS", "refresh_secs", "0"),
            ("BATCH_MAX_RECORDS", "batch_max_records", "1001"),
            ("BATCH_MAX_BYTES", "batch_max_bytes", "10"),
            ("BATCH_MIN_INTERVAL_MS", "batch_min_interval_ms", "60001"),
            ("TRUSTED_PROXY_HOPS", "trusted_proxy_hops", "9"),
            (
                "CLIENT_ADDRESS_HEADER",
                "client_address_header",
                "X-Forwarded-For",
            ),
            ("DEDUP_CAPACITY", "dedup_capacity", "999999999"),
            ("DEDUP_WINDOW_SECS", "dedup_window_secs", "0"),
        ] {
            let leaked: &'static str = Box::leak(bad.to_owned().into_boxed_str());
            assert_eq!(
                invalid_key(Config::resolve(None, with(name, leaked))),
                key,
                "{name}={bad}"
            );
        }
    }

    #[test]
    fn the_clickhouse_password_never_shows_in_debug_output() {
        let config = Config::resolve(None, with("CLICKHOUSE_PASSWORD", "hunter2-secret")).unwrap();
        assert_eq!(config.clickhouse_password.0, "hunter2-secret");
        let shown = format!("{config:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<hidden>"));
    }

    #[test]
    fn clickhouse_endpoint_parsing() {
        let endpoint = |host: &str, port, tls| Endpoint {
            host: host.to_owned(),
            port,
            tls,
        };
        assert_eq!(
            clickhouse_endpoint("http://clickhouse:8123"),
            Ok(endpoint("clickhouse", 8123, false))
        );
        assert_eq!(
            clickhouse_endpoint("http://clickhouse/"),
            Ok(endpoint("clickhouse", 8123, false))
        );
        assert_eq!(
            clickhouse_endpoint("https://clickhouse"),
            Ok(endpoint("clickhouse", 8443, true))
        );
        assert_eq!(
            clickhouse_endpoint("https://ch.example:9443/"),
            Ok(endpoint("ch.example", 9443, true))
        );
        assert!(clickhouse_endpoint("ftp://clickhouse").is_err());
        assert!(clickhouse_endpoint("http://user:pw@clickhouse").is_err());
        assert!(clickhouse_endpoint("http://clickhouse/path").is_err());
        assert!(clickhouse_endpoint("http://:8123").is_err());
        assert!(clickhouse_endpoint("http://clickhouse:99999").is_err());
    }

    #[test]
    fn tls_and_geoip_paths_come_from_the_file_and_the_environment() {
        let file = "clickhouse_url = \"https://ch:8443\"\nclickhouse_ca_file = \"/etc/ca.pem\"\n\
                    clickhouse_server_name = \"ch.internal\"\ngeoip_path = \"/data/c.mmdb\"\n";
        let config = Config::resolve(Some(file), no_env).unwrap();
        assert_eq!(
            config.clickhouse_ca_file,
            Some(PathBuf::from("/etc/ca.pem"))
        );
        assert_eq!(
            config.clickhouse_server_name.as_deref(),
            Some("ch.internal")
        );
        assert_eq!(config.geoip_path, Some(PathBuf::from("/data/c.mmdb")));
        let config = Config::resolve(Some(file), with("GEOIP_PATH", "/x.mmdb")).unwrap();
        assert_eq!(config.geoip_path, Some(PathBuf::from("/x.mmdb")));
    }
}
