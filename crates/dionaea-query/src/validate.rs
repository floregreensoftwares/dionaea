//! Turns a [`Query`] into a [`Plan`] that can run, or refuses it. Pure: no
//! I/O and no clock, so every rule is tested without a database.

use std::collections::BTreeSet;

use dionaea_core::{FieldKind, Schema};
use dionaea_store::report as store;

use crate::QueryError;
use crate::model::{Dimension, Filter, Granularity, Measure, Query, Report, SessionDimension};

/// What a query may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest interval, in days.
    pub max_range_days: u32,
    /// Longest interval for hourly buckets, in days.
    pub max_hourly_range_days: u32,
    /// Most distinct values a breakdown may have.
    pub max_breakdown: u32,
    pub max_filters: usize,
    /// Largest and default `limit` of lists.
    pub max_list: u32,
    pub default_list: u32,
    /// What one read may cost.
    pub max_execution_secs: u32,
    pub max_rows: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_range_days: 366,
            max_hourly_range_days: 31,
            max_breakdown: 50,
            max_filters: 8,
            max_list: 100,
            default_list: 10,
            max_execution_secs: 30,
            max_rows: 20_000,
        }
    }
}

impl Limits {
    pub(crate) fn read(&self) -> store::Limits {
        store::Limits {
            max_execution_secs: self.max_execution_secs,
            max_rows: self.max_rows,
        }
    }
}

/// The site a query is about: its id, the zone its days are cut in, and its
/// active schema versions.
#[derive(Debug, Clone, Copy)]
pub struct Site<'a> {
    pub id: u32,
    pub time_zone: &'a str,
    pub schemas: &'a [Schema],
}

/// A validated query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub(crate) scope: store::Scope,
    pub(crate) kind: Kind,
}

pub(crate) type SessionFilters = Vec<(store::SessionDim, Vec<String>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Overview {
        filters: Vec<store::Filter>,
        /// `None` when a filter has no meaning for sessions.
        session_filters: Option<SessionFilters>,
    },
    Series {
        event: Option<String>,
        filters: Vec<store::Filter>,
        measure: store::Measure,
        granularity: store::Granularity,
        breakdown: Option<store::Dim>,
    },
    Top {
        event: Option<String>,
        filters: Vec<store::Filter>,
        measure: store::Measure,
        dim: store::Dim,
        limit: u32,
    },
    Screens {
        filters: Vec<store::Filter>,
        limit: u32,
    },
    Elements {
        screen: Option<String>,
        filters: Vec<store::Filter>,
        limit: u32,
    },
    Sessions {
        filters: SessionFilters,
        dim: Option<store::SessionDim>,
        limit: u32,
    },
}

const MAX_VALUE_BYTES: usize = 256;
const MAX_VALUES: usize = store::MAX_FILTER_VALUES;
const DAY_MS: i64 = 86_400_000;
/// 2100-01-01.
const MAX_TIME_MS: i64 = 4_102_444_800_000;

/// Whether a zone name has a shape ClickHouse can be asked about.
pub fn is_time_zone_shape(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= store::MAX_TIME_ZONE_LEN
        && !zone.contains("..")
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'+' | b'-'))
}

pub fn validate(site: Site<'_>, query: &Query, limits: &Limits) -> Result<Plan, QueryError> {
    if !is_time_zone_shape(site.time_zone) {
        return Err(QueryError::InvalidTimeZone);
    }
    if query.from_ms < 0 || query.to_ms > MAX_TIME_MS {
        return Err(QueryError::TimeOutOfRange);
    }
    if query.to_ms <= query.from_ms {
        return Err(QueryError::EmptyRange);
    }
    let hourly = matches!(
        query.report,
        Report::Series {
            granularity: Granularity::Hour,
            ..
        }
    );
    let max_days = if hourly {
        limits.max_hourly_range_days
    } else {
        limits.max_range_days
    };
    if query.to_ms - query.from_ms > i64::from(max_days) * DAY_MS {
        return Err(QueryError::RangeTooLarge { max_days });
    }
    if query.filters.len() > limits.max_filters {
        return Err(QueryError::TooManyFilters {
            max: limits.max_filters,
        });
    }

    let scope = store::Scope {
        site_id: site.id,
        from_ms: query.from_ms,
        to_ms: query.to_ms,
        time_zone: site.time_zone.to_owned(),
    };
    let ctx = Context { site, limits };

    let mut events = BTreeSet::new();
    let filters = ctx.filters(&query.filters, &mut events)?;

    let kind = match &query.report {
        Report::Overview => Kind::Overview {
            session_filters: session_filters(&filters),
            filters,
        },
        Report::Series {
            measure,
            event,
            granularity,
            breakdown,
        } => {
            let event = ctx.event(event.as_deref(), &mut events)?;
            let breakdown = breakdown
                .as_ref()
                .map(|dimension| ctx.dim(dimension, &mut events))
                .transpose()?;
            Kind::Series {
                event,
                filters,
                measure: measure_of(*measure),
                granularity: granularity_of(*granularity),
                breakdown,
            }
        }
        Report::Top {
            dimension,
            measure,
            event,
            limit,
        } => {
            let event = ctx.event(event.as_deref(), &mut events)?;
            Kind::Top {
                event,
                dim: ctx.dim(dimension, &mut events)?,
                filters,
                measure: measure_of(*measure),
                limit: ctx.list_limit(*limit)?,
            }
        }
        Report::Screens { limit } => Kind::Screens {
            filters,
            limit: ctx.list_limit(*limit)?,
        },
        Report::Elements { screen, limit } => {
            if let Some(screen) = screen {
                check_value(screen)?;
            }
            Kind::Elements {
                screen: screen.clone(),
                filters,
                limit: ctx.list_limit(*limit)?,
            }
        }
        Report::Sessions { breakdown, limit } => Kind::Sessions {
            filters: session_filters(&filters).ok_or(QueryError::FilterNotAvailable)?,
            dim: breakdown.map(session_dim_of),
            limit: ctx.list_limit(*limit)?,
        },
    };
    if events.len() > 1 {
        return Err(QueryError::ConflictingEvents);
    }
    Ok(Plan { scope, kind })
}

struct Context<'a> {
    site: Site<'a>,
    limits: &'a Limits,
}

impl Context<'_> {
    fn list_limit(&self, limit: Option<u32>) -> Result<u32, QueryError> {
        let limit = limit.unwrap_or(self.limits.default_list);
        if (1..=self.limits.max_list).contains(&limit) {
            Ok(limit)
        } else {
            Err(QueryError::InvalidLimit {
                max: self.limits.max_list,
            })
        }
    }

    /// Calls `f` with each schema that applies; the built-in events resolve
    /// even for a site that has published nothing.
    fn each_schema(&self, mut f: impl FnMut(&Schema)) {
        if self.site.schemas.is_empty() {
            f(&Schema::default());
        }
        self.site.schemas.iter().for_each(f);
    }

    fn known_event(&self, name: &str) -> bool {
        let mut known = false;
        self.each_schema(|schema| known |= schema.event(name).is_some());
        known
    }

    fn event(
        &self,
        name: Option<&str>,
        events: &mut BTreeSet<String>,
    ) -> Result<Option<String>, QueryError> {
        let Some(name) = name else {
            return Ok(None);
        };
        if !self.known_event(name) {
            return Err(QueryError::UndeclaredEvent(name.to_owned()));
        }
        events.insert(name.to_owned());
        Ok(Some(name.to_owned()))
    }

    fn property(
        &self,
        event: &str,
        name: &str,
        events: &mut BTreeSet<String>,
    ) -> Result<(), QueryError> {
        if !self.known_event(event) {
            return Err(QueryError::UndeclaredEvent(event.to_owned()));
        }
        let mut declared = false;
        let mut text_like = true;
        self.each_schema(|schema| {
            if let Some(field) = schema.event(event).and_then(|def| def.fields().get(name)) {
                declared = true;
                text_like &= matches!(
                    field.kind,
                    FieldKind::Id | FieldKind::Text { .. } | FieldKind::Enum { .. }
                );
            }
        });
        if !declared {
            return Err(QueryError::UndeclaredProperty {
                event: event.to_owned(),
                name: name.to_owned(),
            });
        }
        if !text_like {
            return Err(QueryError::NotGroupable {
                event: event.to_owned(),
                name: name.to_owned(),
            });
        }
        events.insert(event.to_owned());
        Ok(())
    }

    fn dim(
        &self,
        dimension: &Dimension,
        events: &mut BTreeSet<String>,
    ) -> Result<store::Dim, QueryError> {
        Ok(match dimension {
            Dimension::Country => store::Dim::Country,
            Dimension::Platform => store::Dim::Platform,
            Dimension::Os => store::Dim::Os,
            Dimension::Browser => store::Dim::Browser,
            Dimension::DeviceClass => store::Dim::DeviceClass,
            Dimension::AppVersion => store::Dim::AppVersion,
            Dimension::Screen => store::Dim::Screen,
            Dimension::Event => store::Dim::EventName,
            Dimension::Property { event, name } => {
                self.property(event, name, events)?;
                store::Dim::Prop {
                    event: event.clone(),
                    name: name.clone(),
                }
            }
        })
    }

    fn filters(
        &self,
        filters: &[Filter],
        events: &mut BTreeSet<String>,
    ) -> Result<Vec<store::Filter>, QueryError> {
        filters
            .iter()
            .map(|filter| {
                if filter.values.is_empty() || filter.values.len() > MAX_VALUES {
                    return Err(QueryError::InvalidFilterValues);
                }
                filter.values.iter().try_for_each(|v| check_value(v))?;
                Ok(store::Filter {
                    dim: self.dim(&filter.dimension, events)?,
                    values: filter.values.clone(),
                })
            })
            .collect()
    }
}

fn check_value(value: &str) -> Result<(), QueryError> {
    if value.is_empty() || value.len() > MAX_VALUE_BYTES {
        Err(QueryError::InvalidFilterValues)
    } else {
        Ok(())
    }
}

/// The filters as session filters, if every one has a meaning for sessions.
fn session_filters(filters: &[store::Filter]) -> Option<SessionFilters> {
    filters
        .iter()
        .map(|filter| {
            let dim = match filter.dim {
                store::Dim::Country => store::SessionDim::Country,
                store::Dim::Platform => store::SessionDim::Platform,
                store::Dim::AppVersion => store::SessionDim::AppVersion,
                _ => return None,
            };
            Some((dim, filter.values.clone()))
        })
        .collect()
}

fn measure_of(measure: Measure) -> store::Measure {
    match measure {
        Measure::Events => store::Measure::Events,
        Measure::Users => store::Measure::Users,
    }
}

fn granularity_of(granularity: Granularity) -> store::Granularity {
    match granularity {
        Granularity::Hour => store::Granularity::Hour,
        Granularity::Day => store::Granularity::Day,
        Granularity::Week => store::Granularity::Week,
        Granularity::Month => store::Granularity::Month,
    }
}

fn session_dim_of(dimension: SessionDimension) -> store::SessionDim {
    match dimension {
        SessionDimension::Country => store::SessionDim::Country,
        SessionDimension::Platform => store::SessionDim::Platform,
        SessionDimension::AppVersion => store::SessionDim::AppVersion,
        SessionDimension::EntryScreen => store::SessionDim::EntryScreen,
        SessionDimension::ExitScreen => store::SessionDim::ExitScreen,
    }
}
