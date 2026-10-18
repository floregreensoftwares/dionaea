//! The operational settings: limits and intervals that can be changed while
//! the server runs. They are defined once, in [`SPECS`], and read from three
//! places that all go through the same checks: the configuration file, the
//! environment, and the values an operator stores in the metadata store
//! (`dionaea settings set`). A later source wins: stored over environment over
//! file over the built-in default.
//!
//! What is not here (addresses, paths, credentials) is needed before the
//! metadata store can be read, or is secret, and stays in the file and the
//! environment.

use std::collections::BTreeMap;
use std::time::Duration;

use dionaea_meta::StoredSetting;

use crate::config::ConfigError;

/// Every limit and interval of the running server. A value outside its range
/// is an error, never a silent clamp.
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
    /// How often keys, schemas and settings are checked for changes.
    pub(crate) refresh_every: Duration,
    pub(crate) batch_max_records: usize,
    pub(crate) batch_max_bytes: usize,
    pub(crate) batch_min_interval: Duration,
    /// Translating the stacks of error events: threads, time per batch, memory
    /// for parsed files and how long an index answer is trusted.
    pub(crate) symbol_workers: usize,
    pub(crate) symbol_deadline: Duration,
    pub(crate) symbol_cache_bytes: usize,
    pub(crate) symbol_recheck: Duration,
    /// How many reverse proxies sit in front of the server. With 0 the client
    /// address is the socket peer and `client_address_header` is ignored.
    pub(crate) trusted_proxy_hops: u8,
    /// The header those proxies append the client address to.
    pub(crate) client_address_header: String,
    /// How many recent event ids are remembered to drop retried events, and
    /// for how long. 0 turns the check off.
    pub(crate) dedup_capacity: usize,
    pub(crate) dedup_window: Duration,
    /// Whether the batcher leaves out events another node already stored, and
    /// records the ids of the ones it stores, in ClickHouse. A single node can
    /// turn it off to save one query and one insert per batch.
    pub(crate) shared_dedup: bool,
    /// How often the drop counts of requests that kept no event are written
    /// to the spool. A crash loses the counts of the last interval.
    pub(crate) drop_flush: Duration,
    /// How long the erasure job waits, after tombstoning a user's devices,
    /// for every node to have loaded the tombstones before it deletes (at
    /// least twice the refresh interval is always waited).
    pub(crate) erase_grace: Duration,
    /// How long a user stays hidden before the daily physical removal, so a
    /// day of requests share one rewrite of the tables.
    pub(crate) erase_finish_after: Duration,
    /// How many times in a row trouble that retrying may fix may stop an
    /// erasure request before it fails.
    pub(crate) erase_max_attempts: u32,
    /// How long a failed erasure request keeps the user id and devices in the
    /// clear so that it can be retried; after that they are forgotten.
    pub(crate) erase_failed_keep: Duration,
    /// How many days a site keeps its analytics data unless it has its own
    /// number, and how often the retention job runs.
    pub(crate) retention_default_days: u32,
    pub(crate) retention_interval: Duration,
}

/// Where a setting's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Default,
    File,
    Environment,
    Stored,
}

impl Origin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            Self::Environment => "environment",
            Self::Stored => "stored",
        }
    }
}

/// One setting: its name, default, the rule its value must satisfy, and how
/// to apply and show it.
pub(crate) struct Spec {
    pub(crate) name: &'static str,
    pub(crate) default: &'static str,
    pub(crate) rule: &'static str,
    /// What the setting is, for `dionaea settings list`.
    pub(crate) help: &'static str,
    apply: fn(&mut Tuning, &str) -> Result<(), String>,
    show: fn(&Tuning) -> String,
}

macro_rules! spec {
    ($name:literal, $default:literal, $rule:literal, $help:literal, $ty:ty, $check:expr, $set:expr, $show:expr) => {
        Spec {
            name: $name,
            default: $default,
            rule: $rule,
            help: $help,
            apply: |tuning: &mut Tuning, text: &str| {
                let value: $ty = text
                    .trim()
                    .parse()
                    .map_err(|_| format!("`{text}` is not a valid value; {}", $rule))?;
                let check: fn(&$ty) -> bool = $check;
                if !check(&value) {
                    return Err($rule.to_owned());
                }
                let set: fn(&mut Tuning, $ty) = $set;
                set(tuning, value);
                Ok(())
            },
            show: $show,
        }
    };
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

fn secs(duration: Duration) -> String {
    duration.as_secs().to_string()
}

pub(crate) static SPECS: &[Spec] = &[
    spec!(
        "key_rate_per_sec",
        "200",
        "a number above 0 and at most 1e9",
        "requests per second allowed per write key",
        f64,
        rate,
        |t, v| t.key_rate_per_sec = v,
        |t| t.key_rate_per_sec.to_string()
    ),
    spec!(
        "key_burst",
        "400",
        "a number from 1 to 1e9",
        "burst allowed per write key",
        f64,
        burst,
        |t, v| t.key_burst = v,
        |t| t.key_burst.to_string()
    ),
    spec!(
        "address_rate_per_sec",
        "50",
        "a number above 0 and at most 1e9",
        "requests per second allowed per client address",
        f64,
        rate,
        |t, v| t.address_rate_per_sec = v,
        |t| t.address_rate_per_sec.to_string()
    ),
    spec!(
        "address_burst",
        "100",
        "a number from 1 to 1e9",
        "burst allowed per client address",
        f64,
        burst,
        |t, v| t.address_burst = v,
        |t| t.address_burst.to_string()
    ),
    spec!(
        "tracked_keys",
        "100000",
        "from 1 to 10000000",
        "keys and addresses each rate limiter remembers",
        usize,
        |v| (1..=10_000_000).contains(v),
        |t, v| t.tracked_keys = v,
        |t| t.tracked_keys.to_string()
    ),
    spec!(
        "body_timeout_secs",
        "10",
        "from 1 to 300 seconds",
        "time a client has to send a whole request body",
        u64,
        |v| (1..=300).contains(v),
        |t, v| t.body_timeout = Duration::from_secs(v),
        |t| secs(t.body_timeout)
    ),
    spec!(
        "retry_after_secs",
        "5",
        "from 1 to 3600 seconds",
        "the Retry-After of a 503",
        u64,
        |v| (1..=3600).contains(v),
        |t, v| t.retry_after_secs = v,
        |t| t.retry_after_secs.to_string()
    ),
    spec!(
        "max_event_age_days",
        "30",
        "from 1 to 3650 days",
        "oldest accepted event, after correcting for the device clock",
        u64,
        |v| (1..=3650).contains(v),
        |t, v| t.max_event_age = Duration::from_secs(v * 86_400),
        |t| (t.max_event_age.as_secs() / 86_400).to_string()
    ),
    spec!(
        "max_event_future_secs",
        "3600",
        "at most 86400 seconds",
        "furthest ahead an event time may be",
        u64,
        |v| *v <= 86_400,
        |t, v| t.max_event_future = Duration::from_secs(v),
        |t| secs(t.max_event_future)
    ),
    spec!(
        "max_connections",
        "10000",
        "from 1 to 1000000",
        "open connections at once on the public listener",
        usize,
        |v| (1..=1_000_000).contains(v),
        |t, v| t.max_connections = v,
        |t| t.max_connections.to_string()
    ),
    spec!(
        "header_timeout_secs",
        "10",
        "from 1 to 300 seconds",
        "time a connection has to send the headers of a request",
        u64,
        |v| (1..=300).contains(v),
        |t, v| t.header_timeout = Duration::from_secs(v),
        |t| secs(t.header_timeout)
    ),
    spec!(
        "refresh_secs",
        "2",
        "from 1 to 3600 seconds",
        "how often keys, schemas and settings are checked for changes",
        u64,
        |v| (1..=3600).contains(v),
        |t, v| t.refresh_every = Duration::from_secs(v),
        |t| secs(t.refresh_every)
    ),
    spec!(
        "batch_max_records",
        "1000",
        "from 1 to 1000",
        "most spool records joined into one ClickHouse insert",
        usize,
        |v| (1..=1000).contains(v),
        |t, v| t.batch_max_records = v,
        |t| t.batch_max_records.to_string()
    ),
    spec!(
        "batch_max_bytes",
        "8388608",
        "from 65536 to 67108864",
        "about how many bytes one insert holds",
        usize,
        |v| ((64 << 10)..=(64 << 20)).contains(v),
        |t, v| t.batch_max_bytes = v,
        |t| t.batch_max_bytes.to_string()
    ),
    spec!(
        "batch_min_interval_ms",
        "200",
        "at most 60000 milliseconds",
        "least time between two ClickHouse inserts",
        u64,
        |v| *v <= 60_000,
        |t, v| t.batch_min_interval = Duration::from_millis(v),
        |t| t.batch_min_interval.as_millis().to_string()
    ),
    spec!(
        "symbol_workers",
        "2",
        "from 1 to 16",
        "threads that translate error stacks for one batch",
        usize,
        |v| (1..=16).contains(v),
        |t, v| t.symbol_workers = v,
        |t| t.symbol_workers.to_string()
    ),
    spec!(
        "symbol_deadline_ms",
        "2000",
        "from 100 to 60000 milliseconds",
        "most time one batch spends translating error stacks; events not reached are stored raw and flagged",
        u64,
        |v| (100..=60_000).contains(v),
        |t, v| t.symbol_deadline = Duration::from_millis(v),
        |t| t.symbol_deadline.as_millis().to_string()
    ),
    spec!(
        "symbol_cache_bytes",
        "134217728",
        "from 1048576 to 4294967296",
        "memory for parsed source maps and mapping files",
        usize,
        |v| ((1 << 20)..=(4 << 30)).contains(v),
        |t, v| t.symbol_cache_bytes = v,
        |t| t.symbol_cache_bytes.to_string()
    ),
    spec!(
        "symbol_recheck_secs",
        "30",
        "from 1 to 3600 seconds",
        "how long a symbol file is trusted before the index is asked again (also how soon an upload is noticed)",
        u64,
        |v| (1..=3600).contains(v),
        |t, v| t.symbol_recheck = Duration::from_secs(v),
        |t| secs(t.symbol_recheck)
    ),
    spec!(
        "trusted_proxy_hops",
        "0",
        "from 0 to 8",
        "reverse proxies in front of the server (0: the socket peer is the client)",
        u8,
        |v| *v <= 8,
        |t, v| t.trusted_proxy_hops = v,
        |t| t.trusted_proxy_hops.to_string()
    ),
    spec!(
        "client_address_header",
        "x-forwarded-for",
        "a lowercase header name",
        "the header the proxies append the client address to",
        String,
        |v| header_name(v),
        |t, v| t.client_address_header = v,
        |t| t.client_address_header.clone()
    ),
    spec!(
        "dedup_capacity",
        "1000000",
        "at most 100000000 (0 turns it off)",
        "event ids remembered to drop retried events",
        usize,
        |v| *v <= 100_000_000,
        |t, v| t.dedup_capacity = v,
        |t| t.dedup_capacity.to_string()
    ),
    spec!(
        "dedup_window_secs",
        "600",
        "from 1 to 86400 seconds",
        "how long an event id is remembered",
        u64,
        |v| (1..=86_400).contains(v),
        |t, v| t.dedup_window = Duration::from_secs(v),
        |t| secs(t.dedup_window)
    ),
    spec!(
        "shared_dedup",
        "true",
        "true or false",
        "ask ClickHouse which events another node stored, and record this node's ids",
        bool,
        |_| true,
        |t, v| t.shared_dedup = v,
        |t| t.shared_dedup.to_string()
    ),
    spec!(
        "drop_flush_secs",
        "5",
        "from 1 to 300 seconds",
        "how often drop counts of requests that kept no event are written",
        u64,
        |v| (1..=300).contains(v),
        |t, v| t.drop_flush = Duration::from_secs(v),
        |t| secs(t.drop_flush)
    ),
    spec!(
        "erase_grace_secs",
        "10",
        "from 0 to 3600 seconds",
        "how long erasure waits for every server to know a user was erased before it deletes",
        u64,
        |v| *v <= 3600,
        |t, v| t.erase_grace = Duration::from_secs(v),
        |t| secs(t.erase_grace)
    ),
    spec!(
        "erase_finish_after_secs",
        "86400",
        "from 0 to 604800 seconds",
        "how long an erased user stays hidden before the rows are physically removed",
        u64,
        |v| *v <= 604_800,
        |t, v| t.erase_finish_after = Duration::from_secs(v),
        |t| secs(t.erase_finish_after)
    ),
    spec!(
        "erase_max_attempts",
        "10",
        "from 1 to 100",
        "how many times in a row an erasure step may meet trouble before the request fails",
        u32,
        |v| (1..=100).contains(v),
        |t, v| t.erase_max_attempts = v,
        |t| t.erase_max_attempts.to_string()
    ),
    spec!(
        "erase_failed_keep_days",
        "30",
        "from 1 to 3650 days",
        "how long a failed erasure request keeps the user id and devices in the clear before forgetting them",
        u64,
        |v| (1..=3650).contains(v),
        |t, v| t.erase_failed_keep = Duration::from_secs(v * 86_400),
        |t| (t.erase_failed_keep.as_secs() / 86_400).to_string()
    ),
    spec!(
        "retention_default_days",
        "395",
        "from 1 to 3650 days",
        "how long a site keeps its analytics data unless `dionaea retention set` gave it another number",
        u32,
        |v| (1..=3650).contains(v),
        |t, v| t.retention_default_days = v,
        |t| t.retention_default_days.to_string()
    ),
    spec!(
        "retention_interval_secs",
        "86400",
        "from 60 to 604800 seconds",
        "how often old data is removed",
        u64,
        |v| (60..=604_800).contains(v),
        |t, v| t.retention_interval = Duration::from_secs(v),
        |t| secs(t.retention_interval)
    ),
];

/// The setting called `name`.
pub(crate) fn find(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// Whether `value` is acceptable for the setting `name`; the message says why
/// not.
pub(crate) fn validate(name: &str, value: &str) -> Result<(), String> {
    let spec = find(name).ok_or_else(|| {
        let names: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
        format!(
            "`{name}` is not a setting that can change while the server runs; those are: {}",
            names.join(", ")
        )
    })?;
    (spec.apply)(&mut Tuning::blank(), value)
}

/// The value written the way it is stored: parsed, checked and shown, so
/// `0200` and `200` are the same setting.
pub(crate) fn canonical(name: &str, value: &str) -> Result<String, String> {
    validate(name, value)?;
    let spec = find(name).ok_or_else(|| format!("`{name}` is not a setting"))?;
    let mut tuning = Tuning::blank();
    (spec.apply)(&mut tuning, value)?;
    Ok(tuning.show(spec))
}

impl Tuning {
    /// A value to apply the defaults on top of.
    fn blank() -> Self {
        Self {
            key_rate_per_sec: 1.0,
            key_burst: 1.0,
            address_rate_per_sec: 1.0,
            address_burst: 1.0,
            tracked_keys: 1,
            body_timeout: Duration::ZERO,
            retry_after_secs: 1,
            max_event_age: Duration::ZERO,
            max_event_future: Duration::ZERO,
            max_connections: 1,
            header_timeout: Duration::ZERO,
            refresh_every: Duration::ZERO,
            batch_max_records: 1,
            batch_max_bytes: 1,
            batch_min_interval: Duration::ZERO,
            symbol_workers: 1,
            symbol_deadline: Duration::ZERO,
            symbol_cache_bytes: 1,
            symbol_recheck: Duration::ZERO,
            trusted_proxy_hops: 0,
            client_address_header: String::new(),
            dedup_capacity: 0,
            dedup_window: Duration::ZERO,
            shared_dedup: true,
            drop_flush: Duration::ZERO,
            erase_grace: Duration::ZERO,
            erase_finish_after: Duration::ZERO,
            erase_max_attempts: 1,
            erase_failed_keep: Duration::ZERO,
            retention_default_days: 1,
            retention_interval: Duration::ZERO,
        }
    }

    /// Every setting at its built-in default.
    pub(crate) fn defaults() -> Self {
        let mut tuning = Self::blank();
        for spec in SPECS {
            // The defaults are checked by a test; this cannot fail.
            let _ = (spec.apply)(&mut tuning, spec.default);
        }
        tuning
    }

    /// The value of `spec` as text, the way it is set.
    pub(crate) fn show(&self, spec: &Spec) -> String {
        (spec.show)(self)
    }

    /// This tuning with the stored values applied. A stored value that is not
    /// a known setting or is out of range is skipped, and the returned list
    /// says which and why: an edit made outside the program must not stop the
    /// server.
    pub(crate) fn with_stored(
        &self,
        stored: &BTreeMap<String, String>,
    ) -> (Self, Vec<(String, String)>) {
        let mut tuning = self.clone();
        let mut skipped = Vec::new();
        for (name, value) in stored {
            let result = match find(name) {
                Some(spec) => {
                    // Applied to a copy first so a failed value changes nothing.
                    let mut trial = tuning.clone();
                    (spec.apply)(&mut trial, value).map(|()| tuning = trial)
                }
                None => Err("not a setting".to_owned()),
            };
            if let Err(reason) = result {
                skipped.push((name.clone(), reason));
            }
        }
        (tuning, skipped)
    }
}

/// The stored values in force at `now_ms`: a change scheduled for later is not
/// in force yet, and a removal that has taken effect is not a value at all.
pub(crate) fn resolve_at(
    stored: &BTreeMap<String, StoredSetting>,
    now_ms: i64,
) -> BTreeMap<String, String> {
    stored
        .iter()
        .filter_map(|(name, setting)| Some((name.clone(), setting.at(now_ms)?.to_owned())))
        .collect()
}

/// When the next stored change takes effect, if one is waiting.
pub(crate) fn next_change(stored: &BTreeMap<String, StoredSetting>, now_ms: i64) -> Option<i64> {
    stored
        .values()
        .map(|setting| setting.effective_at_ms)
        .filter(|at| *at > now_ms)
        .min()
}

/// The settings a stored value overrides although the file or the environment
/// gave them a value: name, where that value came from, and what it was.
pub(crate) fn overridden(
    base: &Tuning,
    origins: &BTreeMap<&'static str, Origin>,
    applied: &BTreeMap<String, String>,
) -> Vec<(&'static str, Origin, String)> {
    SPECS
        .iter()
        .filter(|spec| applied.contains_key(spec.name))
        .filter_map(|spec| {
            let origin = origins.get(spec.name).copied()?;
            matches!(origin, Origin::File | Origin::Environment)
                .then(|| (spec.name, origin, base.show(spec)))
        })
        .collect()
}

/// The text of a TOML value used as a setting.
fn value_text(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(n) => Some(n.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Defaults, then the file, then the environment (`DIONAEA_<NAME>`), with the
/// origin of each value.
pub(crate) fn resolve(
    file: &toml::Table,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(Tuning, BTreeMap<&'static str, Origin>), ConfigError> {
    let mut tuning = Tuning::defaults();
    let mut origins = BTreeMap::new();
    for spec in SPECS {
        let from_env = env(&format!("DIONAEA_{}", spec.name.to_ascii_uppercase()));
        let (text, origin) = match (from_env, file.get(spec.name)) {
            (Some(text), _) => (text, Origin::Environment),
            (None, Some(value)) => (
                value_text(value).ok_or_else(|| ConfigError::Invalid {
                    key: spec.name,
                    reason: "expected a number, text or true/false".to_owned(),
                })?,
                Origin::File,
            ),
            (None, None) => {
                origins.insert(spec.name, Origin::Default);
                continue;
            }
        };
        (spec.apply)(&mut tuning, &text).map_err(|reason| ConfigError::Invalid {
            key: spec.name,
            reason,
        })?;
        origins.insert(spec.name, origin);
    }
    Ok((tuning, origins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_valid_and_shows_as_it_is_written() {
        let defaults = Tuning::defaults();
        for spec in SPECS {
            assert!(validate(spec.name, spec.default).is_ok(), "{}", spec.name);
            assert_eq!(defaults.show(spec), spec.default, "{}", spec.name);
        }
    }

    #[test]
    fn names_are_unique_lowercase_identifiers_with_a_rule_and_help() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in SPECS {
            assert!(seen.insert(spec.name), "{} twice", spec.name);
            assert!(
                spec.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
            assert!(
                !spec.rule.is_empty() && !spec.help.is_empty(),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn a_value_is_accepted_or_refused_with_the_rule() {
        assert!(validate("key_burst", "250").is_ok());
        assert!(validate("shared_dedup", "false").is_ok());
        assert!(validate("client_address_header", "x-real-ip").is_ok());
        for (name, value) in [
            ("key_burst", "0.5"),
            ("key_burst", "nan"),
            ("key_burst", "many"),
            ("max_connections", "0"),
            ("trusted_proxy_hops", "9"),
            ("client_address_header", "X-Real-IP"),
            ("shared_dedup", "maybe"),
        ] {
            assert!(validate(name, value).is_err(), "{name}={value}");
        }
        let unknown = validate("listen", "127.0.0.1:1").unwrap_err();
        assert!(
            unknown.contains("key_burst"),
            "it lists what can change: {unknown}"
        );
    }

    #[test]
    fn stored_values_override_and_bad_ones_are_skipped_and_reported() {
        let base = Tuning::defaults();
        let stored: BTreeMap<String, String> = [
            ("key_burst", "999"),
            ("address_burst", "0"),
            ("not_a_setting", "1"),
            ("shared_dedup", "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let (tuning, skipped) = base.with_stored(&stored);
        assert_eq!(tuning.key_burst, 999.0);
        assert!(!tuning.shared_dedup);
        assert_eq!(
            tuning.address_burst, base.address_burst,
            "the bad value changed nothing"
        );
        let names: Vec<&str> = skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["address_burst", "not_a_setting"]);
    }

    fn stored(value: Option<&str>, previous: Option<&str>, at: i64) -> StoredSetting {
        StoredSetting {
            value: value.map(str::to_owned),
            previous: previous.map(str::to_owned),
            effective_at_ms: at,
        }
    }

    #[test]
    fn stored_values_are_in_force_only_from_their_moment() {
        let map: BTreeMap<String, StoredSetting> = [
            (
                "key_burst".to_owned(),
                stored(Some("900"), Some("300"), 1_000),
            ),
            ("address_burst".to_owned(), stored(Some("5"), None, 1_000)),
            ("tracked_keys".to_owned(), stored(None, Some("77"), 1_000)),
        ]
        .into();
        let before = resolve_at(&map, 999);
        assert_eq!(before.get("key_burst").map(String::as_str), Some("300"));
        assert_eq!(before.get("address_burst"), None, "nothing was in force");
        assert_eq!(before.get("tracked_keys").map(String::as_str), Some("77"));
        let after = resolve_at(&map, 1_000);
        assert_eq!(after.get("key_burst").map(String::as_str), Some("900"));
        assert_eq!(after.get("address_burst").map(String::as_str), Some("5"));
        assert_eq!(
            after.get("tracked_keys"),
            None,
            "a removal that took effect"
        );
    }

    #[test]
    fn the_next_change_is_the_soonest_one_still_to_come() {
        let map: BTreeMap<String, StoredSetting> = [
            ("a".to_owned(), stored(Some("1"), None, 500)),
            ("b".to_owned(), stored(Some("1"), None, 2_000)),
            ("c".to_owned(), stored(None, Some("1"), 1_500)),
        ]
        .into();
        assert_eq!(next_change(&map, 100), Some(500));
        assert_eq!(next_change(&map, 500), Some(1_500), "500 has arrived");
        assert_eq!(next_change(&map, 1_500), Some(2_000));
        assert_eq!(next_change(&map, 2_000), None);
        assert_eq!(next_change(&BTreeMap::new(), 0), None);
    }

    #[test]
    fn only_stored_values_that_hide_a_file_or_environment_value_are_reported() {
        let file: toml::Table = toml::from_str("key_burst = 555\naddress_burst = 66\n").unwrap();
        let env = |name: &str| (name == "DIONAEA_TRACKED_KEYS").then(|| "9".to_owned());
        let (base, origins) = resolve(&file, &env).unwrap();
        let applied: BTreeMap<String, String> = [
            ("key_burst", "10"),
            ("tracked_keys", "20"),
            ("max_connections", "30"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let hidden = overridden(&base, &origins, &applied);
        let summary: Vec<_> = hidden
            .iter()
            .map(|(name, from, was)| (*name, from.as_str(), was.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("key_burst", "file", "555"),
                ("tracked_keys", "environment", "9")
            ],
            "max_connections hid only a default, address_burst was not overridden"
        );
    }

    #[test]
    fn the_file_and_the_environment_are_read_in_that_order_of_priority() {
        let file: toml::Table =
            toml::from_str("key_burst = 500\nkey_rate_per_sec = 12.5\nshared_dedup = false\n")
                .unwrap();
        let env = |name: &str| (name == "DIONAEA_KEY_BURST").then(|| "700".to_owned());
        let (tuning, origins) = resolve(&file, &env).unwrap();
        assert_eq!(
            tuning.key_burst, 700.0,
            "the environment wins over the file"
        );
        assert_eq!(tuning.key_rate_per_sec, 12.5);
        assert!(!tuning.shared_dedup);
        assert_eq!(origins["key_burst"], Origin::Environment);
        assert_eq!(origins["key_rate_per_sec"], Origin::File);
        assert_eq!(origins["address_burst"], Origin::Default);
    }

    #[test]
    fn a_file_value_of_the_wrong_kind_or_out_of_range_names_its_key() {
        let env = |_: &str| None;
        let array: toml::Table = toml::from_str("key_burst = [1, 2]\n").unwrap();
        assert!(matches!(
            resolve(&array, &env),
            Err(ConfigError::Invalid {
                key: "key_burst",
                ..
            })
        ));
        let range: toml::Table = toml::from_str("max_connections = 0\n").unwrap();
        assert!(matches!(
            resolve(&range, &env),
            Err(ConfigError::Invalid {
                key: "max_connections",
                ..
            })
        ));
    }
}
