//! What goes into the spool for one request, and how it is read back.
//!
//! A spool record holds the rows of one request already encoded as the lines
//! the store sends, so the batcher joins records into an insert without
//! decoding or encoding anything again:
//!
//! ```text
//! version (1 byte, = 1) | length of the event lines (u32, little endian)
//!   | event lines | drop lines
//! ```
//!
//! Events and the drop counts of the request travel in the same record, so
//! one acknowledged request is stored or not as a whole. A request that does
//! not fit one record is split by events; the drop lines go in the first.

use dionaea_spool::{Spool, SpoolError};
use dionaea_store::{DropRow, EventRow, Store};

use crate::pipeline::{Accepted, Processed};

const VERSION: u8 = 1;
const HEADER: usize = 5;
/// Most bytes of event lines in one record. A request body is at most 256 KiB,
/// so its lines (escaping and field names included) stay far below this: every
/// valid request is one record, acknowledged or refused as a whole. The spool
/// must allow records of at least this size plus the drop lines.
pub const CHUNK_BYTES: usize = 3 * 1024 * 1024;
/// The drop reason for an accepted event the store could not take as a row.
pub const INVALID_ROW: &str = "invalid_row";

/// What [`enqueue`] stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub events: usize,
    /// Events that passed the guard but could not be made into a row; they
    /// are counted as drops.
    pub invalid: usize,
    pub records: usize,
}

fn row(accepted: &Accepted) -> Option<EventRow> {
    Some(EventRow {
        site_id: accepted.site_id,
        event_id: accepted.event_id.clone(),
        ts_ms: accepted.ts_ms,
        received_ms: accepted.received_ms,
        name: accepted.name.clone(),
        schema_version: u16::try_from(accepted.schema_version).ok()?,
        anon_id: accepted.anon_id.clone(),
        session_id: accepted.session_id.clone().unwrap_or_default(),
        screen: accepted.screen.clone().unwrap_or_default(),
        props_str: accepted.strings.clone(),
        props_num: accepted.numbers.clone(),
        country: accepted.country.clone(),
        platform: accepted.platform.clone(),
        os: accepted.os.clone(),
        browser: accepted.browser.clone(),
        device_class: accepted.device_class.clone(),
        app_version: accepted.app_version.clone().unwrap_or_default(),
        sdk: accepted.sdk.clone(),
    })
}

fn record(event_lines: &str, drop_lines: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(HEADER + event_lines.len() + drop_lines.len());
    payload.push(VERSION);
    // Bounded by CHUNK_BYTES.
    payload.extend_from_slice(
        &u32::try_from(event_lines.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    payload.extend_from_slice(event_lines.as_bytes());
    payload.extend_from_slice(drop_lines.as_bytes());
    payload
}

/// Writes the result of one request to the spool and returns once it is
/// durable. `site_id` and `received_ms` stamp the drop counts. On `Full` or
/// `Busy` nothing may be assumed stored when a request needed several
/// records; the sender retries the whole request, and the events of the
/// records that did persist are then stored twice.
pub fn enqueue(
    spool: &Spool,
    processed: &Processed,
    site_id: u32,
    received_ms: u64,
) -> Result<Enqueued, SpoolError> {
    let mut drops: Vec<DropRow> = processed
        .drops
        .iter()
        .map(|d| DropRow {
            site_id,
            event_name: d.event_name.clone(),
            reason: d.reason.as_str().to_owned(),
            at_ms: received_ms,
            count: d.count,
        })
        .collect();

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let (mut events, mut invalid) = (0, 0);
    for accepted in &processed.accepted {
        let line =
            row(accepted).and_then(|row| Store::encode_events(std::slice::from_ref(&row)).ok());
        let Some(line) = line else {
            invalid += 1;
            match drops
                .iter_mut()
                .find(|d| d.event_name == accepted.name && d.reason == INVALID_ROW)
            {
                Some(existing) => existing.count += 1,
                None => drops.push(DropRow {
                    site_id,
                    event_name: accepted.name.clone(),
                    reason: INVALID_ROW.to_owned(),
                    at_ms: received_ms,
                    count: 1,
                }),
            }
            continue;
        };
        if !current.is_empty() && current.len() + line.len() > CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(&line);
        events += 1;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }

    let drop_lines = if drops.is_empty() {
        String::new()
    } else {
        Store::encode_drops(&drops).map_err(|e| SpoolError::Io(e.to_string()))?
    };
    if events == 0 && drop_lines.is_empty() {
        return Ok(Enqueued {
            events: 0,
            invalid,
            records: 0,
        });
    }
    let records = chunks.len();
    for (index, chunk) in chunks.iter().enumerate() {
        spool.append(record(chunk, if index == 0 { &drop_lines } else { "" }))?;
    }
    Ok(Enqueued {
        events,
        invalid,
        records,
    })
}

/// The two halves of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lines<'a> {
    pub(crate) events: &'a str,
    pub(crate) drops: &'a str,
}

/// Splits a record into its lines; `None` when it is not a record this
/// version wrote (a bad version, a length past the end, text that is not
/// UTF-8).
pub(crate) fn decode(payload: &[u8]) -> Option<Lines<'_>> {
    let (&version, rest) = payload.split_first()?;
    if version != VERSION || rest.len() < 4 {
        return None;
    }
    let (length, rest) = rest.split_at(4);
    let length = usize::try_from(u32::from_le_bytes(length.try_into().ok()?)).ok()?;
    if length > rest.len() {
        return None;
    }
    let (events, drops) = rest.split_at(length);
    Some(Lines {
        events: std::str::from_utf8(events).ok()?,
        drops: std::str::from_utf8(drops).ok()?,
    })
}
