use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

const ENV_PREFIX: &str = "DIONAEA_";

/// A secret that never shows in `Debug` output, so it cannot reach a log.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct Secret(pub(crate) String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<hidden>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) clickhouse_url: String,
    pub(crate) clickhouse_user: String,
    pub(crate) clickhouse_password: Secret,
    pub(crate) clickhouse_database: String,
    pub(crate) spool_dir: PathBuf,
    pub(crate) spool_quota_bytes: u64,
    pub(crate) metadata_path: PathBuf,
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
    clickhouse_url: Option<String>,
    clickhouse_user: Option<String>,
    clickhouse_password: Option<String>,
    clickhouse_database: Option<String>,
    spool_dir: Option<PathBuf>,
    spool_quota_bytes: Option<u64>,
    metadata_path: Option<PathBuf>,
    log_level: Option<String>,
    log_format: Option<LogFormat>,
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
        let var = |name: &str| env(&format!("{ENV_PREFIX}{name}"));

        let listen = var("LISTEN")
            .or(file.listen)
            .unwrap_or_else(|| "127.0.0.1:8080".to_owned());
        let listen =
            listen
                .parse()
                .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
                    key: "listen",
                    reason: e.to_string(),
                })?;

        let clickhouse_url = var("CLICKHOUSE_URL")
            .or(file.clickhouse_url)
            .unwrap_or_else(|| "http://127.0.0.1:8123".to_owned());
        clickhouse_endpoint(&clickhouse_url).map_err(|reason| ConfigError::Invalid {
            key: "clickhouse_url",
            reason,
        })?;

        let clickhouse_user = var("CLICKHOUSE_USER")
            .or(file.clickhouse_user)
            .unwrap_or_else(|| "default".to_owned());
        let clickhouse_password = Secret(
            var("CLICKHOUSE_PASSWORD")
                .or(file.clickhouse_password)
                .unwrap_or_default(),
        );
        let clickhouse_database = var("CLICKHOUSE_DATABASE")
            .or(file.clickhouse_database)
            .unwrap_or_else(|| "dionaea".to_owned());

        let spool_dir = var("SPOOL_DIR")
            .map(PathBuf::from)
            .or(file.spool_dir)
            .unwrap_or_else(|| PathBuf::from("spool"));
        let spool_quota_bytes = match var("SPOOL_QUOTA_BYTES") {
            Some(text) => text.parse().map_err(|_| ConfigError::Invalid {
                key: "spool_quota_bytes",
                reason: "expected a number of bytes".to_owned(),
            })?,
            None => file.spool_quota_bytes.unwrap_or(1 << 30),
        };
        if spool_quota_bytes < (1 << 20) {
            return Err(ConfigError::Invalid {
                key: "spool_quota_bytes",
                reason: "at least 1048576 (1 MiB)".to_owned(),
            });
        }

        let metadata_path = var("METADATA_PATH")
            .map(PathBuf::from)
            .or(file.metadata_path)
            .unwrap_or_else(|| PathBuf::from("dionaea.db"));

        let log_level = var("LOG_LEVEL")
            .or(file.log_level)
            .unwrap_or_else(|| "info".to_owned());

        let log_format = match var("LOG_FORMAT") {
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
            clickhouse_url,
            clickhouse_user,
            clickhouse_password,
            clickhouse_database,
            spool_dir,
            spool_quota_bytes,
            metadata_path,
            log_level,
            log_format,
        })
    }
}

/// Host and port of a plain `http://host[:port]` ClickHouse URL.
pub(crate) fn clickhouse_endpoint(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only `http://host[:port]` URLs are supported")?;
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains(['/', '@', '?', '#']) {
        return Err("expected `http://host[:port]` with no path or credentials".to_owned());
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse().map_err(|_| "invalid port".to_owned())?;
            Ok((host.to_owned(), port))
        }
        Some(_) => Err("missing host".to_owned()),
        None => Ok((authority.to_owned(), 8123)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_apply_without_file_or_env() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.clickhouse_url, "http://127.0.0.1:8123");
        assert_eq!(config.metadata_path, PathBuf::from("dionaea.db"));
        assert_eq!(config.log_level, "info");
        assert_eq!(config.log_format, LogFormat::Json);
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
        let env = |name: &str| (name == "DIONAEA_LISTEN").then(|| "127.0.0.1:7000".to_owned());
        let config = Config::resolve(Some(file), env).unwrap();
        assert_eq!(config.listen, "127.0.0.1:7000".parse().unwrap());
    }

    #[test]
    fn the_metadata_path_comes_from_the_file_and_the_environment() {
        let file = "metadata_path = \"/data/meta.db\"
";
        let config = Config::resolve(Some(file), no_env).unwrap();
        assert_eq!(config.metadata_path, PathBuf::from("/data/meta.db"));
        let env = |name: &str| (name == "DIONAEA_METADATA_PATH").then(|| "other.db".to_owned());
        let config = Config::resolve(Some(file), env).unwrap();
        assert_eq!(config.metadata_path, PathBuf::from("other.db"));
    }

    #[test]
    fn unknown_file_key_is_rejected() {
        let error = Config::resolve(Some("lisen = \"x\"\n"), no_env).unwrap_err();
        assert!(matches!(error, ConfigError::File(_)));
    }

    #[test]
    fn invalid_listen_is_a_typed_error() {
        let env = |name: &str| (name == "DIONAEA_LISTEN").then(|| "not-an-address".to_owned());
        let error = Config::resolve(None, env).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { key: "listen", .. }));
    }

    #[test]
    fn invalid_log_format_is_a_typed_error() {
        let env = |name: &str| (name == "DIONAEA_LOG_FORMAT").then(|| "xml".to_owned());
        let error = Config::resolve(None, env).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: "log_format",
                ..
            }
        ));
    }

    #[test]
    fn spool_and_clickhouse_settings_have_defaults_and_overrides() {
        let config = Config::resolve(None, no_env).unwrap();
        assert_eq!(config.spool_dir, PathBuf::from("spool"));
        assert_eq!(config.spool_quota_bytes, 1 << 30);
        assert_eq!(config.clickhouse_user, "default");
        assert_eq!(config.clickhouse_database, "dionaea");

        let file = "spool_dir = \"/var/spool/d\"\nspool_quota_bytes = 5000000\nclickhouse_user = \"ingest\"\n";
        let env = |name: &str| (name == "DIONAEA_SPOOL_QUOTA_BYTES").then(|| "7000000".to_owned());
        let config = Config::resolve(Some(file), env).unwrap();
        assert_eq!(config.spool_dir, PathBuf::from("/var/spool/d"));
        assert_eq!(config.spool_quota_bytes, 7_000_000);
        assert_eq!(config.clickhouse_user, "ingest");
    }

    #[test]
    fn a_bad_or_tiny_spool_quota_is_a_typed_error() {
        for value in ["abc", "-1", "1000"] {
            let env = |name: &str| (name == "DIONAEA_SPOOL_QUOTA_BYTES").then(|| value.to_owned());
            let error = Config::resolve(None, env).unwrap_err();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "spool_quota_bytes",
                        ..
                    }
                ),
                "{value}"
            );
        }
    }

    #[test]
    fn the_clickhouse_password_never_shows_in_debug_output() {
        let env = |name: &str| {
            (name == "DIONAEA_CLICKHOUSE_PASSWORD").then(|| "hunter2-secret".to_owned())
        };
        let config = Config::resolve(None, env).unwrap();
        assert_eq!(config.clickhouse_password.0, "hunter2-secret");
        let shown = format!("{config:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<hidden>"));
    }

    #[test]
    fn clickhouse_endpoint_parsing() {
        assert_eq!(
            clickhouse_endpoint("http://clickhouse:8123"),
            Ok(("clickhouse".to_owned(), 8123))
        );
        assert_eq!(
            clickhouse_endpoint("http://clickhouse/"),
            Ok(("clickhouse".to_owned(), 8123))
        );
        assert!(clickhouse_endpoint("https://clickhouse").is_err());
        assert!(clickhouse_endpoint("http://user:pw@clickhouse").is_err());
        assert!(clickhouse_endpoint("http://clickhouse/path").is_err());
        assert!(clickhouse_endpoint("http://:8123").is_err());
        assert!(clickhouse_endpoint("http://clickhouse:99999").is_err());
    }
}
