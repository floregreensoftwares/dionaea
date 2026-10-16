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

/// Tokens that mark automated clients, matched case-insensitively. A bare
/// `bot` is not one of them: it appears inside the names of phones (`Cubot`),
/// so `bot` counts only where it ends a product name.
const BOT_TOKENS: &[&str] = &[
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
    "scrapy",
];

/// Where `bot` ends a product name: `Googlebot/2.1`, `(bingbot;`, `_bot_`.
const BOT_ENDINGS: &[&str] = &[
    "bot/", "bot;", "bot)", "bot,", "bot+", "-bot", "_bot", "/bot", "bots",
];

/// What a user agent says, as short fixed names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserAgent {
    /// `web`, `ios` or `android`; empty when unknown.
    pub platform: String,
    pub os: String,
    pub browser: String,
    /// `phone`, `tablet` or `desktop`; empty when unknown.
    pub device_class: String,
    pub bot: bool,
}

fn has(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

/// A product name as a short lowercase identifier.
fn identifier(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().take(48) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(32).collect()
}

fn browser_from(name: &str) -> String {
    match identifier(name).as_str() {
        "internet_explorer" => "ie".to_owned(),
        "samsung_browser" | "samsung_internet" | "samsungbrowser" => "samsung".to_owned(),
        other => other.to_owned(),
    }
}

fn os_from(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.contains("windows phone") {
        "windows_phone".to_owned()
    } else if lower.starts_with("windows") {
        "windows".to_owned()
    } else if lower.starts_with("mac") {
        "macos".to_owned()
    } else if ["iphone", "ipad", "ipod", "ios"]
        .iter()
        .any(|ios| lower.starts_with(ios))
    {
        "ios".to_owned()
    } else {
        identifier(name)
    }
}

/// Reads a `User-Agent` header value.
pub fn parse_user_agent(user_agent: &str) -> UserAgent {
    let mut end = user_agent.len().min(MAX_USER_AGENT);
    while !user_agent.is_char_boundary(end) {
        end -= 1;
    }
    let original = &user_agent[..end];
    let lower = original.to_ascii_lowercase();
    let ua = lower.as_str();
    if ua.is_empty() {
        return UserAgent::default();
    }

    // The rules below name the common cases; the woothee dataset adds the long
    // tail of crawlers, browsers and systems.
    let known = woothee::parser::Parser::new().parse(original);
    let crawler = known.as_ref().is_some_and(|r| r.category == "crawler");
    let bot = crawler
        || BOT_TOKENS.iter().any(|token| has(ua, token))
        || BOT_ENDINGS.iter().any(|ending| has(ua, ending));

    let os = if has(ua, "android") {
        "android"
    } else if has(ua, "iphone") || has(ua, "ipad") || has(ua, "ipod") || has(ua, "cfnetwork") {
        "ios"
    } else if has(ua, "windows phone") {
        "windows_phone"
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

    // A browser the dataset names more specifically (Vivaldi, UC Browser) wins
    // over the generic "chrome" or "safari" the rules fall back on; the
    // system is only filled in when the rules found none.
    let mut browser = browser.to_owned();
    let mut os = os.to_owned();
    if let Some(result) = known
        .as_ref()
        .filter(|r| r.browser_type == "browser" && r.category != "crawler")
    {
        let named = browser_from(result.name);
        if !named.is_empty() && named != "unknown" {
            let generic = matches!(named.as_str(), "chrome" | "safari");
            if browser.is_empty() || !generic {
                browser = named;
            }
        }
        if os.is_empty() {
            let named = os_from(result.os);
            if !named.is_empty() && named != "unknown" {
                os = named;
            }
        }
    }

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
    } else if matches!(os.as_str(), "windows" | "macos" | "linux" | "chromeos") {
        "desktop"
    } else {
        ""
    };

    UserAgent {
        platform: platform.to_owned(),
        os,
        browser,
        device_class: device_class.to_owned(),
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
            platform: parsed.platform,
            os: parsed.os,
            browser: parsed.browser,
            device_class: parsed.device_class,
            bot: parsed.bot,
        }
    }
}
