//! Removing data a site no longer keeps.
//!
//! Each site keeps its analytics data for its own number of days, so the
//! tables cannot expire by one rule of their own. A lightweight `DELETE`
//! removes a site's rows older than its cutoff; a partition that is older than
//! every site's cutoff is dropped whole, which costs nothing; and the
//! partitions that still hold deleted rows are rewritten so the data leaves
//! the disk.

use std::collections::BTreeSet;

use crate::erase::Rewrite;
use crate::{Error, Store, time};

/// A table that expires with the site's retention, how to find its rows older
/// than a cutoff, and how it is partitioned.
struct Expiring {
    name: &'static str,
    /// A condition true for the rows older than the cutoff (`{ts}` is the
    /// cutoff as a `DateTime64`, `{day}` as a `Date`).
    older: &'static str,
    /// The expression the table is partitioned by.
    partition: &'static str,
}

const ANALYTICS: &[Expiring] = &[
    Expiring {
        name: "events",
        older: "ts < toDateTime64('{ts}', 3, 'UTC')",
        partition: "toYYYYMM(ts)",
    },
    Expiring {
        name: "sessions",
        older: "started < toDate('{day}')",
        partition: "toYYYYMM(started)",
    },
    Expiring {
        name: "errors",
        older: "ts < toDateTime64('{ts}', 3, 'UTC')",
        partition: "toYYYYMM(ts)",
    },
];

fn older(table: &Expiring, cutoff_ms: u64) -> String {
    table
        .older
        .replace("{ts}", &time::format_datetime(cutoff_ms))
        .replace("{day}", &time::format_date(cutoff_ms))
}

/// Rows removed from a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expired {
    pub table: &'static str,
    pub rows: u64,
}

impl Store {
    /// Deletes the analytics rows of the site older than `cutoff_ms`
    /// (milliseconds since the epoch) with a lightweight delete and returns,
    /// per table, how many there were. Queries stop seeing them at once; the
    /// data leaves the disk when [`rewrite_expired`](Self::rewrite_expired)
    /// runs.
    pub fn expire_analytics(&self, site_id: u32, cutoff_ms: u64) -> Result<Vec<Expired>, Error> {
        let mut done = Vec::with_capacity(ANALYTICS.len());
        for table in ANALYTICS {
            let condition = older(table, cutoff_ms);
            let rows = self.count_older(table, site_id, cutoff_ms)?;
            if rows > 0 {
                self.execute_with(
                    &format!(
                        "DELETE FROM {} WHERE site_id = {site_id} AND {condition}",
                        table.name
                    ),
                    "lightweight_deletes_sync=2",
                )?;
            }
            done.push(Expired {
                table: table.name,
                rows,
            });
        }
        Ok(done)
    }

    fn count_older(&self, table: &Expiring, site_id: u32, cutoff_ms: u64) -> Result<u64, Error> {
        self.execute(&format!(
            "SELECT count() FROM {} WHERE site_id = {site_id} AND {}",
            table.name,
            older(table, cutoff_ms)
        ))?
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::InvalidRow("a count that is not a number".to_owned()))
    }

    /// How many analytics rows of the site are older than `cutoff_ms`, per
    /// table, without deleting anything: what a run would remove now.
    pub fn count_expired(&self, site_id: u32, cutoff_ms: u64) -> Result<Vec<Expired>, Error> {
        ANALYTICS
            .iter()
            .map(|table| {
                Ok(Expired {
                    table: table.name,
                    rows: self.count_older(table, site_id, cutoff_ms)?,
                })
            })
            .collect()
    }

    /// Drops every monthly partition of the analytics tables whose whole month
    /// is before `cutoff_ms`. Pass the oldest cutoff of any site: a partition
    /// older than that holds nothing anyone keeps. Returns the table and the
    /// partition of each.
    pub fn drop_expired_partitions(
        &self,
        cutoff_ms: u64,
    ) -> Result<Vec<(&'static str, u32)>, Error> {
        let day = time::format_date(cutoff_ms);
        let mut dropped = Vec::new();
        for table in ANALYTICS {
            let answer = self.execute(&format!(
                "SELECT DISTINCT partition FROM system.parts
                 WHERE database = currentDatabase() AND table = '{}' AND active
                   AND toDate(concat(substring(partition, 1, 4), '-',
                                     substring(partition, 5, 2), '-01'))
                       + INTERVAL 1 MONTH <= toDate('{day}')
                 ORDER BY partition FORMAT TabSeparated",
                table.name
            ))?;
            for line in answer.lines() {
                let partition = line.trim().parse::<u32>().map_err(|_| {
                    Error::InvalidRow("a partition that is not a number".to_owned())
                })?;
                self.execute(&format!(
                    "ALTER TABLE {} DROP PARTITION {partition}",
                    table.name
                ))?;
                dropped.push((table.name, partition));
            }
        }
        Ok(dropped)
    }

    /// Rewrites the partitions older than `cutoff_ms` that still hold rows
    /// marked deleted, so that what expired leaves the disk. Pass the newest
    /// cutoff of any site. Returns what it rewrote.
    pub fn rewrite_expired(&self, cutoff_ms: u64) -> Result<Vec<Rewrite>, Error> {
        let mut needed = BTreeSet::new();
        for table in ANALYTICS {
            let answer = self.execute(&format!(
                "SELECT DISTINCT {partition} FROM {name}
                 WHERE _row_exists = 0 AND {condition}
                 SETTINGS apply_deleted_mask = 0 FORMAT TabSeparated",
                partition = table.partition,
                name = table.name,
                condition = older(table, cutoff_ms)
            ))?;
            for line in answer.lines() {
                let partition = line.trim().parse::<u32>().map_err(|_| {
                    Error::InvalidRow("a partition that is not a number".to_owned())
                })?;
                needed.insert(Rewrite {
                    table: table.name,
                    partition: Some(partition),
                });
            }
        }
        let needed: Vec<Rewrite> = needed.into_iter().collect();
        self.apply_rewrites(&needed)?;
        Ok(needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cutoff_becomes_a_time_for_events_and_a_day_for_sessions() {
        // 2025-10-09 08:53:20.123 UTC
        let ms = 1_760_000_000_123;
        assert_eq!(
            older(&ANALYTICS[0], ms),
            "ts < toDateTime64('2025-10-09 08:53:20.123', 3, 'UTC')"
        );
        assert_eq!(older(&ANALYTICS[1], ms), "started < toDate('2025-10-09')");
    }

    #[test]
    fn every_expiring_table_is_an_identifier_table_that_erasure_covers() {
        for table in ANALYTICS {
            let registered = crate::IDENTIFIER_TABLES
                .iter()
                .find(|t| t.name == table.name)
                .unwrap_or_else(|| panic!("{} is not registered for erasure", table.name));
            assert_eq!(
                registered.partition,
                Some(table.partition),
                "{}",
                table.name
            );
        }
    }
}
