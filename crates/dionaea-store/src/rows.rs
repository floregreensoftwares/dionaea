//! The rows the store inserts, and how they become `JSONEachRow` lines.

use std::fmt::Write as _;

use serde_json::json;

use crate::{Error, time};

/// One accepted event, ready for the `events` table.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub site_id: u32,
    /// The canonical text of the event's UUID.
    pub event_id: String,
    /// Client time, corrected for skew, in milliseconds since the Unix epoch.
    pub ts_ms: u64,
    pub received_ms: u64,
    pub name: String,
    pub schema_version: u16,
    pub anon_id: String,
    pub session_id: String,
    pub screen: String,
    pub props_str: Vec<(String, String)>,
    pub props_num: Vec<(String, f64)>,
    pub country: String,
    pub platform: String,
    pub os: String,
    pub browser: String,
    pub device_class: String,
    pub app_version: String,
    pub sdk: String,
}

/// Events dropped for one reason on one day, for `drops_daily`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRow {
    pub site_id: u32,
    /// The declared event name, or a placeholder the caller chooses for names
    /// that are not declared, so an attacker cannot mint keys.
    pub event_name: String,
    pub reason: String,
    /// When the drop happened, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub count: u64,
}

/// An anonymous id linked to a user, for `identity_alias`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRow {
    pub site_id: u32,
    pub anon_id: String,
    pub user_id: String,
    pub linked_ms: u64,
}

/// The traits of a user, for `user_traits`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitsRow {
    pub site_id: u32,
    pub user_id: String,
    pub traits_str: Vec<(String, String)>,
    pub traits_num: Vec<(String, f64)>,
    pub updated_ms: u64,
}

fn check_time(ms: u64, what: &str) -> Result<(), Error> {
    if ms > time::MAX_MS {
        return Err(Error::InvalidRow(format!("{what} is beyond the year 2299")));
    }
    Ok(())
}

/// The canonical 36-character form of a UUID.
pub(crate) fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Whether a name appears twice. Property maps hold at most a few dozen
/// entries, so comparing pairs is cheaper than hashing.
fn has_repeat<'a>(names: impl Iterator<Item = &'a str> + Clone) -> bool {
    let mut outer = names.clone();
    while let Some(name) = outer.next() {
        if outer.clone().any(|other| other == name) {
            return true;
        }
    }
    false
}

/// Appends `text` as a JSON string, escaping what JSON requires. Runs of
/// characters that need no escape are copied in one piece.
fn push_string(out: &mut String, text: &str) {
    out.push('"');
    let mut run_start = 0;
    for (index, c) in text.char_indices() {
        let escape = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if c < ' ' => "",
            _ => continue,
        };
        out.push_str(&text[run_start..index]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push_str(escape);
        }
        run_start = index + c.len_utf8();
    }
    out.push_str(&text[run_start..]);
    out.push('"');
}

/// `"name":"value",` for a string column.
fn string_field(out: &mut String, name: &str, value: &str) {
    out.push('"');
    out.push_str(name);
    out.push_str("\":");
    push_string(out, value);
    out.push(',');
}

fn number_field(out: &mut String, name: &str, value: impl std::fmt::Display) {
    let _ = write!(out, "\"{name}\":{value},");
}

/// The site and event id of a line written by [`EventRow::to_json_line`],
/// without parsing the rest. Both are the first two fields and neither can
/// hold anything the sender chose (a number, and a checked UUID), so this
/// cannot be fooled by the content of the line.
pub fn event_key(line: &str) -> Option<(u32, &str)> {
    let rest = line.strip_prefix("{\"site_id\":")?;
    let (site, rest) = rest.split_once(',')?;
    let rest = rest.strip_prefix("\"event_id\":\"")?;
    let id = rest.get(..36)?;
    (rest[36..].starts_with('"') && is_uuid(id)).then_some(())?;
    Some((site.parse().ok()?, id))
}

/// The line `line` (written by [`EventRow::to_json_line`]) with its string
/// properties replaced by `props`. Everything else stays byte for byte, so the
/// site and event id stay the first two fields ([`event_key`]). `None` when the
/// line does not have the layout `to_json_line` writes.
///
/// The cut points are `,"props_str":{` and the first `},"props_num":{` after
/// it. Both contain an unescaped quote, which never occurs inside a string
/// written by `push_string`, and no value of `props_str` starts with `{`, so
/// neither can be matched inside content a sender chose.
pub fn with_props_str(line: &str, props: &[(String, String)]) -> Option<String> {
    const START: &str = ",\"props_str\":{";
    const END: &str = "},\"props_num\":{";
    let from = line.find(START)? + START.len();
    let to = from + line[from..].find(END)?;
    let mut out = String::with_capacity(line.len() + 64);
    out.push_str(&line[..from]);
    for (index, (key, value)) in props.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_string(&mut out, key);
        out.push(':');
        push_string(&mut out, value);
    }
    out.push_str(&line[to..]);
    Some(out)
}

impl EventRow {
    /// One `JSONEachRow` line, written straight into a string: building a
    /// generic JSON tree first cost several times more.
    pub(crate) fn to_json_line(&self) -> Result<String, Error> {
        check_time(self.ts_ms, "the event time")?;
        check_time(self.received_ms, "the received time")?;
        // ClickHouse refuses a whole insert for one such row, and a retry would
        // fail the same way, so they are caught here, row by row.
        if !is_uuid(&self.event_id) {
            return Err(Error::InvalidRow(
                "the event id is not a canonical UUID".to_owned(),
            ));
        }
        if has_repeat(self.props_str.iter().map(|(key, _)| key.as_str()))
            || has_repeat(self.props_num.iter().map(|(key, _)| key.as_str()))
        {
            return Err(Error::InvalidRow("a property name is repeated".to_owned()));
        }
        let mut out = String::with_capacity(512);
        out.push('{');
        number_field(&mut out, "site_id", self.site_id);
        string_field(&mut out, "event_id", &self.event_id);
        string_field(&mut out, "ts", &time::format_datetime(self.ts_ms));
        string_field(
            &mut out,
            "received_at",
            &time::format_datetime(self.received_ms),
        );
        string_field(&mut out, "name", &self.name);
        number_field(&mut out, "schema_version", self.schema_version);
        string_field(&mut out, "anon_id", &self.anon_id);
        string_field(&mut out, "session_id", &self.session_id);
        string_field(&mut out, "screen", &self.screen);

        out.push_str("\"props_str\":{");
        for (index, (key, value)) in self.props_str.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            push_string(&mut out, key);
            out.push(':');
            push_string(&mut out, value);
        }
        out.push_str("},\"props_num\":{");
        for (index, (key, value)) in self.props_num.iter().enumerate() {
            if !value.is_finite() {
                return Err(Error::InvalidRow(format!(
                    "property `{key}` is not a finite number"
                )));
            }
            if index > 0 {
                out.push(',');
            }
            push_string(&mut out, key);
            let _ = write!(out, ":{value}");
        }
        out.push_str("},");

        string_field(&mut out, "country", &self.country);
        string_field(&mut out, "platform", &self.platform);
        string_field(&mut out, "os", &self.os);
        string_field(&mut out, "browser", &self.browser);
        string_field(&mut out, "device_class", &self.device_class);
        string_field(&mut out, "app_version", &self.app_version);
        // The last field has no trailing comma.
        out.push_str("\"sdk\":");
        push_string(&mut out, &self.sdk);
        out.push('}');
        Ok(out)
    }
}

impl DropRow {
    pub(crate) fn to_json_line(&self) -> Result<String, Error> {
        check_time(self.at_ms, "the drop time")?;
        Ok(json!({
            "site_id": self.site_id,
            "day": time::format_date(self.at_ms),
            "event_name": self.event_name,
            "reason": self.reason,
            "count": self.count,
        })
        .to_string())
    }
}

impl AliasRow {
    pub(crate) fn to_json_line(&self) -> Result<String, Error> {
        check_time(self.linked_ms, "the link time")?;
        let mut out = String::with_capacity(128);
        out.push('{');
        number_field(&mut out, "site_id", self.site_id);
        string_field(&mut out, "anon_id", &self.anon_id);
        string_field(&mut out, "user_id", &self.user_id);
        out.push_str("\"linked_at\":");
        push_string(&mut out, &time::format_datetime(self.linked_ms));
        out.push('}');
        Ok(out)
    }
}

impl TraitsRow {
    pub(crate) fn to_json_line(&self) -> Result<String, Error> {
        check_time(self.updated_ms, "the update time")?;
        if has_repeat(self.traits_str.iter().map(|(key, _)| key.as_str()))
            || has_repeat(self.traits_num.iter().map(|(key, _)| key.as_str()))
        {
            return Err(Error::InvalidRow("a trait name is repeated".to_owned()));
        }
        let mut out = String::with_capacity(256);
        out.push('{');
        number_field(&mut out, "site_id", self.site_id);
        string_field(&mut out, "user_id", &self.user_id);
        out.push_str("\"traits_str\":{");
        for (index, (key, value)) in self.traits_str.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            push_string(&mut out, key);
            out.push(':');
            push_string(&mut out, value);
        }
        out.push_str("},\"traits_num\":{");
        for (index, (key, value)) in self.traits_num.iter().enumerate() {
            if !value.is_finite() {
                return Err(Error::InvalidRow(format!(
                    "trait `{key}` is not a finite number"
                )));
            }
            if index > 0 {
                out.push(',');
            }
            push_string(&mut out, key);
            let _ = write!(out, ":{value}");
        }
        out.push_str("},\"updated_at\":");
        push_string(&mut out, &time::format_datetime(self.updated_ms));
        out.push('}');
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    pub(crate) fn event() -> EventRow {
        EventRow {
            site_id: 7,
            event_id: "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e".to_owned(),
            ts_ms: 1_760_000_000_123,
            received_ms: 1_760_000_001_000,
            name: "checkout".to_owned(),
            schema_version: 3,
            anon_id: "a1".to_owned(),
            session_id: "s1".to_owned(),
            screen: "/orders/:id".to_owned(),
            props_str: vec![("coupon".to_owned(), "SAVE \"10\"\n".to_owned())],
            props_num: vec![("items".to_owned(), 3.0), ("ratio".to_owned(), 0.25)],
            country: "BR".to_owned(),
            platform: "ios".to_owned(),
            os: "iOS 18".to_owned(),
            browser: String::new(),
            device_class: "phone".to_owned(),
            app_version: "1.4.2+57".to_owned(),
            sdk: "dionaea-rn/0.1.0".to_owned(),
        }
    }

    #[test]
    fn an_event_becomes_one_json_line_with_every_column() {
        let line = event().to_json_line().unwrap();
        assert!(!line.contains('\n'), "one row per line");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ts"], "2025-10-09 08:53:20.123");
        assert_eq!(value["received_at"], "2025-10-09 08:53:21.000");
        assert_eq!(value["props_str"]["coupon"], "SAVE \"10\"\n");
        assert_eq!(value["props_num"]["ratio"], 0.25);
        assert_eq!(value.as_object().unwrap().len(), 18);
    }

    #[test]
    fn strings_survive_every_character_json_needs_escaped() {
        let nasty = "quote\" backslash\\ slash/ nl\n cr\r tab\t nul\u{0} bell\u{7} del\u{7f} caf\u{e9} \u{1f600} line\u{2028}sep";
        let mut row = event();
        row.name = nasty.to_owned();
        row.props_str = vec![(nasty.to_owned(), nasty.to_owned())];
        row.props_num = vec![(nasty.to_owned(), -0.5)];
        let line = row.to_json_line().unwrap();
        assert!(
            !line.contains('\n') && !line.contains('\r'),
            "one row per line"
        );
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["name"], nasty);
        assert_eq!(value["props_str"][nasty], nasty);
        assert_eq!(value["props_num"][nasty], -0.5);
    }

    #[test]
    fn replacing_the_string_properties_keeps_every_other_byte() {
        let nasty = "},\"props_num\":{\"x\":1},\"props_str\":{ \\\" é";
        let mut row = event();
        row.props_str = vec![(nasty.to_owned(), nasty.to_owned())];
        row.screen = nasty.to_owned();
        let line = row.to_json_line().unwrap();
        let props = vec![
            ("frames".to_owned(), nasty.to_owned()),
            ("symbols".to_owned(), "ok".to_owned()),
        ];
        let changed = with_props_str(&line, &props).unwrap();
        let value: Value = serde_json::from_str(&changed).unwrap();
        assert_eq!(value["props_str"]["frames"], nasty);
        assert_eq!(value["props_str"]["symbols"], "ok");
        assert_eq!(value["props_str"].as_object().unwrap().len(), 2);
        assert_eq!(value["screen"], nasty);
        assert_eq!(value["props_num"]["ratio"], 0.25);
        assert_eq!(value.as_object().unwrap().len(), 18);
        assert_eq!(event_key(&changed), event_key(&line));
        // Writing the original properties back gives the original line.
        assert_eq!(with_props_str(&line, &row.props_str).unwrap(), line);
        assert!(with_props_str("{}", &props).is_none());
    }

    #[test]
    fn empty_maps_and_extreme_numbers_encode() {
        let mut row = event();
        row.props_str.clear();
        row.props_num = vec![
            ("big".to_owned(), 1e300),
            ("tiny".to_owned(), 1e-300),
            ("neg".to_owned(), -1.5e15),
        ];
        let value: Value = serde_json::from_str(&row.to_json_line().unwrap()).unwrap();
        assert_eq!(value["props_str"].as_object().unwrap().len(), 0);
        assert_eq!(value["props_num"]["big"], 1e300);
        assert_eq!(value["props_num"]["tiny"], 1e-300);
        assert_eq!(value["props_num"]["neg"], -1.5e15);
        row.props_num.clear();
        assert!(serde_json::from_str::<Value>(&row.to_json_line().unwrap()).is_ok());
    }
    #[test]
    fn a_row_clickhouse_would_refuse_is_caught_before_sending() {
        for bad in [
            "",
            "x",
            "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6",
            "0190f7e27c3e7a529a0e5f3c2a1b4d6e0000",
            "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6g",
        ] {
            let mut row = event();
            row.event_id = bad.to_owned();
            assert!(
                matches!(row.to_json_line(), Err(Error::InvalidRow(_))),
                "{bad:?}"
            );
        }
        let mut upper = event();
        upper.event_id = "0190F7E2-7C3E-7A52-9A0E-5F3C2A1B4D6E".to_owned();
        assert!(upper.to_json_line().is_ok());

        let mut row = event();
        row.props_str
            .push(("coupon".to_owned(), "again".to_owned()));
        assert!(matches!(row.to_json_line(), Err(Error::InvalidRow(_))));
        let mut row = event();
        row.props_num.push(("items".to_owned(), 9.0));
        assert!(matches!(row.to_json_line(), Err(Error::InvalidRow(_))));
        // The same name in the string and the number map is two different columns.
        let mut row = event();
        row.props_num.push(("coupon".to_owned(), 1.0));
        assert!(row.to_json_line().is_ok());
    }

    #[test]
    fn times_beyond_clickhouse_are_refused() {
        let mut row = event();
        row.ts_ms = time::MAX_MS + 1;
        assert!(matches!(row.to_json_line(), Err(Error::InvalidRow(_))));
        let mut row = event();
        row.received_ms = u64::MAX;
        assert!(matches!(row.to_json_line(), Err(Error::InvalidRow(_))));
        let mut row = event();
        row.ts_ms = time::MAX_MS;
        assert!(row.to_json_line().is_ok());
    }

    #[test]
    fn non_finite_numbers_are_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut row = event();
            row.props_num.push(("bad".to_owned(), bad));
            assert!(
                matches!(row.to_json_line(), Err(Error::InvalidRow(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_drop_becomes_a_line_with_its_day() {
        let line = DropRow {
            site_id: 7,
            event_name: "checkout".to_owned(),
            reason: "missing_required_field".to_owned(),
            at_ms: 1_760_000_000_123,
            count: 12,
        }
        .to_json_line()
        .unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["day"], "2025-10-09");
        assert_eq!(value["count"], 12);
    }

    #[test]
    fn an_alias_becomes_one_json_line() {
        let row = AliasRow {
            site_id: 7,
            anon_id: "a\"1".to_owned(),
            user_id: "u_42".to_owned(),
            linked_ms: 1_760_000_000_123,
        };
        let value: Value = serde_json::from_str(&row.to_json_line().unwrap()).unwrap();
        assert_eq!(value["anon_id"], "a\"1");
        assert_eq!(value["user_id"], "u_42");
        assert_eq!(value["linked_at"], "2025-10-09 08:53:20.123");
        assert_eq!(value.as_object().unwrap().len(), 4);
        let late = AliasRow {
            linked_ms: time::MAX_MS + 1,
            ..row
        };
        assert!(late.to_json_line().is_err());
    }

    #[test]
    fn traits_become_one_json_line_and_bad_ones_are_refused() {
        let row = TraitsRow {
            site_id: 7,
            user_id: "u_42".to_owned(),
            traits_str: vec![("plan".to_owned(), "pro \"x\"\n".to_owned())],
            traits_num: vec![("seats".to_owned(), 3.0)],
            updated_ms: 1_760_000_000_123,
        };
        let line = row.to_json_line().unwrap();
        assert!(!line.contains('\n'), "one row per line");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["traits_str"]["plan"], "pro \"x\"\n");
        assert_eq!(value["traits_num"]["seats"], 3.0);
        assert_eq!(value["updated_at"], "2025-10-09 08:53:20.123");

        let mut repeated = row.clone();
        repeated.traits_num.push(("seats".to_owned(), 4.0));
        assert!(repeated.to_json_line().is_err());
        let mut infinite = row.clone();
        infinite.traits_num = vec![("seats".to_owned(), f64::INFINITY)];
        assert!(infinite.to_json_line().is_err());
        let empty = TraitsRow {
            traits_str: vec![],
            traits_num: vec![],
            ..row
        };
        let value: Value = serde_json::from_str(&empty.to_json_line().unwrap()).unwrap();
        assert_eq!(value["traits_str"], serde_json::json!({}));
    }
}
