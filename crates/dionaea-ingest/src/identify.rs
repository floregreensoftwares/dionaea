//! From an identify request body to a link between an anonymous id and a
//! user, plus the user's declared traits. Pure, like the event pipeline: the
//! time of arrival is an argument and nothing is kept.
//!
//! Traits are checked against every accepted schema version, and the version
//! that keeps the most of them wins (the newest on a tie), so a trait that
//! only an older, still accepted version declares is not lost. An identify
//! whose traits all disappear in the guard stores the link and leaves the
//! user's earlier traits alone, because storing an empty set would erase
//! them.

use dionaea_core::{DropReason, FieldValue, RequestError, decode_identify};

use crate::cache::Admission;
use crate::enrich::Enrichment;
use crate::pipeline::{DropCount, Reason};

/// The name counted for drops of identify requests, so they do not share
/// counters with a declared event.
pub const IDENTIFY_EVENT: &str = "(identify)";

/// The traits that survived the guard.
#[derive(Debug, Clone, PartialEq)]
pub struct Traits {
    /// Text, id and enum traits, by name.
    pub strings: Vec<(String, String)>,
    /// Count, duration and unit traits, by name.
    pub numbers: Vec<(String, f64)>,
}

/// The result of one identify request.
#[derive(Debug, Clone, PartialEq)]
pub struct Identified {
    pub site_id: u32,
    pub anon_id: String,
    pub user_id: String,
    /// `None` when the request carried none that the schema declares.
    pub traits: Option<Traits>,
    /// Traits removed because the schema does not declare them or their value
    /// broke the rules.
    pub stripped: u64,
    /// What was dropped as a whole: traits past the property limit.
    pub drops: Vec<DropCount>,
    /// The sender is an automated client: nothing is to be stored.
    pub bot: bool,
}

/// Decodes `body` and checks its traits. A body that is not a valid identify
/// request is refused whole.
pub fn process_identify(
    admission: &Admission,
    body: &[u8],
    enrichment: &Enrichment,
) -> Result<Identified, RequestError> {
    let identify = decode_identify(body)?;
    let mut result = Identified {
        site_id: admission.site_id,
        anon_id: identify.anon_id,
        user_id: identify.user_id,
        traits: None,
        stripped: 0,
        drops: Vec::new(),
        bot: enrichment.bot,
    };
    if enrichment.bot {
        result.drops.push(DropCount {
            event_name: IDENTIFY_EVENT.to_owned(),
            reason: Reason::Bot,
            count: 1,
        });
        return Ok(result);
    }
    if identify.traits.is_empty() {
        return Ok(result);
    }

    let mut best: Option<(usize, u64, Traits)> = None;
    let mut first_error: Option<DropReason> = None;
    for (_, schema) in admission.schemas.iter() {
        match schema.guard_traits(&identify.traits) {
            Ok(guarded) => {
                let kept = guarded.fields.len();
                if best.as_ref().is_some_and(|(most, _, _)| *most >= kept) {
                    continue;
                }
                let stripped = (guarded.undeclared + guarded.invalid.len()) as u64;
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
                best = Some((kept, stripped, Traits { strings, numbers }));
            }
            Err(dropped) => {
                first_error.get_or_insert(dropped.reason);
            }
        }
    }
    match best {
        Some((kept, stripped, traits)) => {
            result.stripped = stripped;
            if kept > 0 {
                result.traits = Some(traits);
            }
        }
        None => {
            // No version could take the traits at all.
            result.stripped = identify.traits.len() as u64;
            if let Some(reason) = first_error {
                result.drops.push(DropCount {
                    event_name: IDENTIFY_EVENT.to_owned(),
                    reason: Reason::Guard(reason),
                    count: 1,
                });
            }
        }
    }
    Ok(result)
}
