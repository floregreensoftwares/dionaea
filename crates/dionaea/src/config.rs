use std::net::SocketAddr;

use serde::Deserialize;

const ENV_PREFIX: &str = "DIONAEA_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) clickhouse_url: String,
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
