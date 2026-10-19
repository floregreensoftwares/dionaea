//! From a request body to accepted events and drop counts. Pure: the time of
//! arrival is an argument.
//!
//! Each event is checked against every accepted schema version, newest first,
//! and admitted by the first that accepts it; the row records which. An event
//! no version accepts is dropped with the reason from the newest version.
//! Nothing here keeps a payload: a drop is only a name, a reason and a count.

use std::collections::{BTreeMap, HashSet};

use dionaea_core::{
    DropReason, FieldValue, RequestError, WireEvent, decode_batch, is_performance_event,
    reserved_event_names,
};
use dionaea_meta::{SAMPLE_FULL_PPM, rate_from_ppm};

use crate::cache::Admission;
use crate::dedup::Deduper;
use crate::enrich::Enrichment;

/// The number a sampled performance event carries: the share of such events
/// kept. The store reads it into `measurements.sample_rate`. The built-in
/// shapes of those events have no field of this name, so a client cannot send it.
const SAMPLE_RATE_FIELD: &str = "sample_rate";

/// The event name counted for a drop whose name the site never declared, so a
/// sender cannot mint counter keys by inventing names.
pub const UNDECLARED_EVENT: &str = "(undeclared)";
/// The event name counted for an event that could not be read far enough to
/// have one.
pub const UNPARSED_EVENT: &str = "(unparsed)";

/// How far from the arrival time a corrected event time may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Oldest accepted, in milliseconds before arrival.
    pub max_past_ms: u64,
    /// Furthest accepted, in milliseconds after arrival.
    pub max_future_ms: u64,
}

impl Default for Limits {
    /// Thirty days back, because an app can be offline for a long time and
    /// queues its events; one hour ahead, because after correcting for the
    /// device's clock only a small error should remain.
    fn default() -> Self {
        Self {
            max_past_ms: 30 * 24 * 3_600_000,
            max_future_ms: 3_600_000,
        }
    }
}

/// Why an event was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// The request or the schema rejected it.
    Guard(DropReason),
    /// The event time, corrected for the device's clock, is outside [`Limits`].
    TimestampOutOfRange,
    /// The same event id was already stored (a retried event).
    Duplicate,
    /// The sender is an automated client.
    Bot,
    /// The device belongs to a user whose data is erased.
    Erased,
    /// A performance event the site keeps only a share of, and not this one.
    Sampled,
}

impl Reason {
    /// The name used for counters and in `drops_daily`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guard(reason) => reason.as_str(),
            Self::TimestampOutOfRange => "timestamp_out_of_range",
            Self::Duplicate => "duplicate_event",
            Self::Bot => "bot",
            Self::Erased => "erased",
            Self::Sampled => "sampled",
        }
    }
}

/// One event that passed, with the fields the store needs that come from the
/// request. Enrichment (country, platform, browser) is added later.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub site_id: u32,
    /// The canonical text of the event's UUID.
    pub event_id: String,
    /// The same id as 128 bits, for [`Deduper::remember`].
    pub event_key: u128,
    /// Event time corrected for the device's clock, in milliseconds.
    pub ts_ms: u64,
    pub received_ms: u64,
    pub name: String,
    /// The schema version that admitted it.
    pub schema_version: u32,
    pub anon_id: String,
    pub session_id: Option<String>,
    pub screen: Option<String>,
    /// Text, id and enum fields, by name.
    pub strings: Vec<(String, String)>,
    /// Count, duration and unit fields, by name.
    pub numbers: Vec<(String, f64)>,
    pub app_version: Option<String>,
    pub sdk: String,
    /// From the request: country, platform, OS, browser, device class (empty
    /// when unknown).
    pub country: String,
    pub platform: String,
    pub os: String,
    pub browser: String,
    pub device_class: String,
}

/// How many events were dropped for one reason under one event name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropCount {
    pub event_name: String,
    pub reason: Reason,
    pub count: u64,
}

/// The result of one request body.
#[derive(Debug, Clone, PartialEq)]
pub struct Processed {
    pub accepted: Vec<Accepted>,
    pub drops: Vec<DropCount>,
}

/// What a request brings besides its body: who sent it and what to remember.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    pub enrichment: Option<&'a Enrichment>,
    /// Recent event ids; an id found here is dropped as a duplicate. Remember
    /// the ids of [`Processed::accepted`] with [`Deduper::remember`] once the
    /// request is durable.
    pub deduper: Option<&'a Deduper>,
}

/// Decodes `body` and checks every event. A body that is not a valid batch is
/// refused whole; an invalid event is dropped alone.
pub fn process(
    admission: &Admission,
    body: &[u8],
    received_ms: u64,
    limits: &Limits,
) -> Result<Processed, RequestError> {
    process_with(admission, body, received_ms, limits, &Context::default())
}

/// [`process`] with the sender's enrichment and the duplicate check.
pub fn process_with(
    admission: &Admission,
    body: &[u8],
    received_ms: u64,
    limits: &Limits,
    context: &Context<'_>,
) -> Result<Processed, RequestError> {
    let batch = decode_batch(body)?;
    let offset = i128::from(received_ms) - i128::from(batch.sent_at);
    let mut accepted = Vec::with_capacity(batch.events.len());
    let mut drops: BTreeMap<(String, Reason), u64> = BTreeMap::new();
    let mut count_drop = |name: &str, reason: Reason| {
        *drops.entry((name.to_owned(), reason)).or_default() += 1;
    };
    let enrichment = context.enrichment.cloned().unwrap_or_default();
    let mut seen: HashSet<u128> = HashSet::new();

    if enrichment.bot {
        // Automated clients are not guarded: every event is counted and gone.
        for _ in &batch.rejected {
            count_drop(UNPARSED_EVENT, Reason::Bot);
        }
        for event in &batch.events {
            count_drop(&counted_name(admission, &event.name), Reason::Bot);
        }
        return Ok(Processed {
            accepted,
            drops: into_counts(drops),
        });
    }

    for rejected in &batch.rejected {
        count_drop(UNPARSED_EVENT, Reason::Guard(rejected.reason));
    }
    for event in batch.events {
        if admission
            .tombstones
            .anon_erased(admission.site_id, &event.anon_id)
        {
            count_drop(&counted_name(admission, &event.name), Reason::Erased);
            continue;
        }
        let ts = i128::from(event.ts) + offset;
        let low = i128::from(received_ms) - i128::from(limits.max_past_ms);
        let high = i128::from(received_ms) + i128::from(limits.max_future_ms);
        if ts < low || ts > high {
            count_drop(
                &counted_name(admission, &event.name),
                Reason::TimestampOutOfRange,
            );
            continue;
        }
        // In range, so it is not negative and fits.
        let ts_ms = u64::try_from(ts).unwrap_or(0);
        let (schema_version, strings, numbers) = match guard(admission, &event) {
            Ok(fields) => fields,
            Err(reason) => {
                count_drop(&counted_name(admission, &event.name), Reason::Guard(reason));
                continue;
            }
        };
        let key = event.event_id.as_u128();
        let repeated = !seen.insert(key)
            || context
                .deduper
                .is_some_and(|deduper| deduper.contains(key, received_ms));
        if repeated {
            count_drop(&event.name, Reason::Duplicate);
            continue;
        }
        // After the duplicate check and before anything is remembered, so a
        // dropped event can be sent again and is judged the same way.
        let sampled = admission.sample_ppm < SAMPLE_FULL_PPM && is_performance_event(&event.name);
        if sampled && !crate::sampling::keep(key, admission.sample_ppm) {
            count_drop(&event.name, Reason::Sampled);
            continue;
        }
        let mut numbers = numbers;
        if sampled {
            // What the kept event stands for: read back into the measurement.
            numbers.push((
                SAMPLE_RATE_FIELD.to_owned(),
                rate_from_ppm(admission.sample_ppm),
            ));
        }
        accepted.push(Accepted {
            site_id: admission.site_id,
            event_id: event.event_id.to_string(),
            event_key: key,
            ts_ms,
            received_ms,
            name: event.name,
            schema_version,
            anon_id: event.anon_id,
            session_id: event.session_id,
            screen: event.screen.as_deref().map(crate::health::screen),
            strings,
            numbers,
            app_version: batch.release.clone(),
            sdk: batch.sdk.clone(),
            country: enrichment.country.clone(),
            platform: enrichment.platform.clone(),
            os: enrichment.os.clone(),
            browser: enrichment.browser.clone(),
            device_class: enrichment.device_class.clone(),
        });
    }

    Ok(Processed {
        accepted,
        drops: into_counts(drops),
    })
}

fn into_counts(drops: BTreeMap<(String, Reason), u64>) -> Vec<DropCount> {
    drops
        .into_iter()
        .map(|((event_name, reason), count)| DropCount {
            event_name,
            reason,
            count,
        })
        .collect()
}

type Fields = (u32, Vec<(String, String)>, Vec<(String, f64)>);

fn guard(admission: &Admission, event: &WireEvent) -> Result<Fields, DropReason> {
    let mut first_error = None;
    for (version, schema) in admission.schemas.iter() {
        match schema.guard_event(&event.name, &event.props) {
            Ok(guarded) => {
                let (mut strings, mut numbers) = (Vec::new(), Vec::new());
                for (name, value) in guarded.fields {
                    let name = name.to_string();
                    match value {
                        FieldValue::Text(text) => strings.push((name, text)),
                        FieldValue::Unit(number) => numbers.push((name, number)),
                        #[allow(clippy::cast_precision_loss)]
                        FieldValue::Integer(number) => numbers.push((name, number as f64)),
                    }
                }
                crate::health::sanitize(&event.name, &mut strings);
                return Ok((*version, strings, numbers));
            }
            Err(dropped) => {
                first_error.get_or_insert(dropped.reason);
            }
        }
    }
    // With no accepted version at all nothing can be declared.
    Err(first_error.unwrap_or(DropReason::UnknownEvent))
}

/// The name a drop is counted under: the event's own when the site (or the
/// built-in set) declares it, a placeholder otherwise.
fn counted_name(admission: &Admission, name: &str) -> String {
    let known = reserved_event_names().contains(&name)
        || admission
            .schemas
            .iter()
            .any(|(_, schema)| schema.events().contains_key(name));
    if known { name } else { UNDECLARED_EVENT }.to_owned()
}
