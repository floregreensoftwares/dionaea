use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use dionaea_meta::{HashKey, SqliteMeta};
use serde::Deserialize;

use crate::settings::{self, Origin, Tuning};

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
    /// The secret that keys the hashes erasure keeps (empty: none), or a file
    /// that holds it. The same on every server that shares the metadata.
    pub(crate) erasure_key: Secret,
    pub(crate) erasure_key_file: Option<PathBuf>,
    /// A MaxMind country database (`.mmdb`); without it events have no country.
    pub(crate) geoip_path: Option<PathBuf>,
    pub(crate) tuning: Tuning,
    /// Where each setting in `tuning` came from.
    pub(crate) tuning_origins: BTreeMap<&'static str, Origin>,
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

/// The keys of the file that are not operational settings (those are in
/// `settings::SPECS`).
const FILE_KEYS: &[&str] = &[
    "listen",
    "ops_listen",
    "clickhouse_url",
    "clickhouse_user",
    "clickhouse_password",
    "clickhouse_database",
    "clickhouse_ca_file",
    "clickhouse_server_name",
    "spool_dir",
    "spool_quota_bytes",
    "metadata_path",
    "erasure_key",
    "erasure_key_file",
    "geoip_path",
    "log_level",
    "log_format",
];

#[derive(Debug, Default, Deserialize)]
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
    erasure_key: Option<String>,
    erasure_key_file: Option<PathBuf>,
    geoip_path: Option<PathBuf>,
    log_level: Option<String>,
    log_format: Option<LogFormat>,
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

impl Config {
    /// Defaults, then the file, then `DIONAEA_*` environment variables.
    /// `env` is injected so resolution stays free of ambient state.
    pub(crate) fn resolve(
        file: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let table: toml::Table = match file {
            Some(text) => toml::from_str(text).map_err(|e| ConfigError::File(e.to_string()))?,
            None => toml::Table::new(),
        };
        if let Some(unknown) = table
            .keys()
            .find(|key| !FILE_KEYS.contains(&key.as_str()) && settings::find(key).is_none())
        {
            return Err(ConfigError::File(format!("unknown key `{unknown}`")));
        }
        let file: FileConfig = table
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::File(e.to_string()))?;
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

        let (tuning, tuning_origins) = settings::resolve(&table, &source.env)?;

        let erasure_key = Secret(source.text("ERASURE_KEY", file.erasure_key, ""));
        let erasure_key_file = source.path("ERASURE_KEY_FILE", file.erasure_key_file);
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
            erasure_key,
            erasure_key_file,
            geoip_path,
            tuning,
            tuning_origins,
            log_level,
            log_format,
        })
    }
}

impl Config {
    /// The key that keys the erasure hashes, from the configuration or the file
    /// it names; `None` when neither is set.
    pub(crate) fn erasure_key(&self) -> Result<Option<HashKey>, String> {
        match (self.erasure_key.0.is_empty(), &self.erasure_key_file) {
            (false, Some(_)) => Err("set `erasure_key` or `erasure_key_file`, not both".to_owned()),
            (false, None) => HashKey::from_secret(&self.erasure_key.0).map(Some),
            (true, Some(path)) => {
                let text = std::fs::read_to_string(path).map_err(|e| {
                    format!("cannot read the erasure key file {}: {e}", path.display())
                })?;
                HashKey::from_secret(text.trim_end()).map(Some)
            }
            (true, None) => Ok(None),
        }
    }

    /// Opens the metadata store with the erasure key, if there is one.
    pub(crate) fn open_meta(&self) -> Result<SqliteMeta, String> {
        let meta = SqliteMeta::open(&self.metadata_path)
            .map_err(|e| format!("cannot open {}: {e}", self.metadata_path.display()))?;
        Ok(match self.erasure_key()? {
            Some(key) => meta.with_hash_key(key),
            None => meta,
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
    use std::time::Duration;

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
                    dedup_capacity = 0\ndedup_window_secs = 5\nshared_dedup = false\n\
                    drop_flush_secs = 9\n";
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
        assert!(!t.shared_dedup);
        assert_eq!(t.drop_flush, Duration::from_secs(9));
        assert_eq!(t.trusted_proxy_hops, 2);
        assert_eq!(t.client_address_header, "x-real-ip");
        assert_eq!(
            (t.dedup_capacity, t.dedup_window),
            (0, Duration::from_secs(5))
        );
        assert_eq!(t.drop_flush, Duration::from_secs(9));

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
            ("DROP_FLUSH_SECS", "drop_flush_secs", "0"),
            ("SHARED_DEDUP", "shared_dedup", "sometimes"),
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
    fn the_erasure_key_comes_from_the_environment_or_a_file_and_is_never_shown() {
        let secret = "0123456789abcdef0123456789abcdef";
        let from_env = |name: &str| (name == "DIONAEA_ERASURE_KEY").then(|| secret.to_owned());
        let config = Config::resolve(None, from_env).unwrap();
        assert!(config.erasure_key().unwrap().is_some());
        assert!(!format!("{config:?}").contains(secret));

        let none = Config::resolve(None, |_| None).unwrap();
        assert_eq!(none.erasure_key(), Ok(None));

        let short = Config::resolve(None, |name| {
            (name == "DIONAEA_ERASURE_KEY").then(|| "short".to_owned())
        })
        .unwrap();
        assert!(short.erasure_key().unwrap_err().contains("32"));

        let both = Config::resolve(Some("erasure_key_file = \"k\"\n"), from_env).unwrap();
        assert!(both.erasure_key().unwrap_err().contains("not both"));

        let missing =
            Config::resolve(Some("erasure_key_file = \"/no/such/file\"\n"), |_| None).unwrap();
        assert!(missing.erasure_key().unwrap_err().contains("cannot read"));
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
