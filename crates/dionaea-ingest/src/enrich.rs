//! What the server adds to an event from the request itself: the platform,
//! operating system, browser and device class read from the `User-Agent`, a
//! bot verdict, and the country of the client address.
//!
//! The address is used for the country lookup and then forgotten: nothing here
//! returns, stores or logs it. The user agent is read, never stored.
//!
//! The user agent parser is a short list of rules, not a full database: it
//! names the common browsers and systems and leaves the rest empty. A wrong
//! empty is better than a wrong name, so a rule only fires on a token that
//! belongs to one product.

use std::fmt;
use std::net::IpAddr;
use std::path::Path;

/// Longest user agent looked at, in bytes; the rest is ignored.
const MAX_USER_AGENT: usize = 512;

/// Tokens that mark automated clients. Matched case-insensitively.
const BOT_TOKENS: &[&str] = &[
    "bot",
    "crawler",
    "spider",
    "slurp",
    "headlesschrome",
    "phantomjs",
    "lighthouse",
    "pingdom",
    "uptimerobot",
    "facebookexternalhit",
    "python-requests",
    "python-urllib",
    "go-http-client",
    "curl/",
    "wget/",
    "libwww",
    "httpclient",
    "scrapy",
    "monitor",
];

/// What a user agent says, as short fixed names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserAgent {
    /// `web`, `ios` or `android`; empty when unknown.
    pub platform: &'static str,
    pub os: &'static str,
    pub browser: &'static str,
    /// `phone`, `tablet` or `desktop`; empty when unknown.
    pub device_class: &'static str,
    pub bot: bool,
}

fn has(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

/// Reads a `User-Agent` header value.
pub fn parse_user_agent(user_agent: &str) -> UserAgent {
    let mut end = user_agent.len().min(MAX_USER_AGENT);
    while !user_agent.is_char_boundary(end) {
        end -= 1;
    }
    let lower = user_agent[..end].to_ascii_lowercase();
    let ua = lower.as_str();
    if ua.is_empty() {
        return UserAgent::default();
    }
    let bot = BOT_TOKENS.iter().any(|token| has(ua, token));

    let os = if has(ua, "android") {
        "android"
    } else if has(ua, "iphone") || has(ua, "ipad") || has(ua, "ipod") || has(ua, "cfnetwork") {
        "ios"
    } else if has(ua, "windows") {
        "windows"
    } else if has(ua, "mac os x") || has(ua, "macintosh") {
        "macos"
    } else if has(ua, "cros") {
        "chromeos"
    } else if has(ua, "linux") || has(ua, "x11") {
        "linux"
    } else {
        ""
    };

    // Order matters: Edge, Opera and Samsung also say "Chrome"; Chrome also
    // says "Safari".
    let browser = if has(ua, "edg/") || has(ua, "edga/") || has(ua, "edgios/") {
        "edge"
    } else if has(ua, "opr/") || has(ua, "opera") {
        "opera"
    } else if has(ua, "samsungbrowser/") {
        "samsung"
    } else if has(ua, "firefox/") || has(ua, "fxios/") {
        "firefox"
    } else if has(ua, "chrome/") || has(ua, "crios/") {
        "chrome"
    } else if has(ua, "msie ") || has(ua, "trident/") {
        "ie"
    } else if has(ua, "safari/") && has(ua, "version/") {
        "safari"
    } else {
        ""
    };

    let platform = if !browser.is_empty() {
        "web"
    } else if has(ua, "okhttp") || has(ua, "dalvik") {
        "android"
    } else if has(ua, "cfnetwork") || has(ua, "darwin") {
        "ios"
    } else {
        ""
    };

    let device_class = if has(ua, "ipad") || has(ua, "tablet") {
        "tablet"
    } else if has(ua, "iphone") || has(ua, "ipod") || has(ua, "mobi") {
        "phone"
    } else if has(ua, "android") {
        // Android phones say "Mobile"; tablets do not.
        "tablet"
    } else if matches!(os, "windows" | "macos" | "linux" | "chromeos") {
        "desktop"
    } else {
        ""
    };

    UserAgent {
        platform,
        os,
        browser,
        device_class,
        bot,
    }
}

/// Looks up the country of an address.
pub trait CountryLookup: Send + Sync + fmt::Debug {
    /// The ISO 3166-1 alpha-2 code, or `None` when the address is unknown.
    fn country(&self, address: IpAddr) -> Option<String>;
}

/// A MaxMind country (or city) database read into memory.
pub struct MaxMind {
    reader: maxminddb::Reader<Vec<u8>>,
}

impl fmt::Debug for MaxMind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaxMind").finish_non_exhaustive()
    }
}

impl MaxMind {
    /// Reads the database file.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or is not a MaxMind database.
    pub fn open(path: &Path) -> Result<Self, String> {
        let reader = maxminddb::Reader::open_readfile(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Ok(Self { reader })
    }

    /// Wraps database bytes; for tests.
    ///
    /// # Errors
    ///
    /// When the bytes are not a MaxMind database.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        maxminddb::Reader::from_source(bytes)
            .map(|reader| Self { reader })
            .map_err(|e| e.to_string())
    }
}

impl CountryLookup for MaxMind {
    fn country(&self, address: IpAddr) -> Option<String> {
        let found = self.reader.lookup(address).ok()?;
        let record = found.decode::<maxminddb::geoip2::Country>().ok()??;
        let code = record.country.iso_code?;
        (code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())).then(|| code.to_owned())
    }
}

/// What is known about the sender of one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Enrichment {
    pub country: String,
    pub platform: String,
    pub os: String,
    pub browser: String,
    pub device_class: String,
    /// The client is automated; its events are dropped and counted.
    pub bot: bool,
}

impl Enrichment {
    /// Builds it from the request's user agent and client address. `lookup` is
    /// the country database, when one is configured.
    pub fn of_request(
        user_agent: Option<&str>,
        address: Option<IpAddr>,
        lookup: Option<&dyn CountryLookup>,
    ) -> Self {
        let parsed = user_agent.map(parse_user_agent).unwrap_or_default();
        let country = address
            .zip(lookup)
            .and_then(|(address, lookup)| lookup.country(address))
            .unwrap_or_default();
        Self {
            country,
            platform: parsed.platform.to_owned(),
            os: parsed.os.to_owned(),
            browser: parsed.browser.to_owned(),
            device_class: parsed.device_class.to_owned(),
            bot: parsed.bot,
        }
    }
}
