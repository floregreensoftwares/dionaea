//! The schema of the store, as numbered migrations applied in order.
//!
//! Every statement is idempotent (`IF NOT EXISTS`), so two nodes migrating at
//! the same moment both succeed, and a migration that stopped half way can be
//! run again. A migration is never edited once released; a change is a new
//! migration.

pub(crate) struct Migration {
    pub(crate) version: u32,
    pub(crate) name: &'static str,
    pub(crate) statements: &'static [&'static str],
}

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "events and drops",
        statements: &[
            // `non_replicated_deduplication_window` is 0 by default, which would
            // make a retried insert with the same deduplication token store the
            // batch twice. The window keeps the hashes of the last 1000 blocks.
            "CREATE TABLE IF NOT EXISTS events (
            site_id        UInt32,
            event_id       UUID,
            ts             DateTime64(3, 'UTC'),
            received_at    DateTime64(3, 'UTC'),
            name           LowCardinality(String),
            schema_version UInt16,
            anon_id        String,
            session_id     String,
            screen         LowCardinality(String),
            props_str      Map(LowCardinality(String), String),
            props_num      Map(LowCardinality(String), Float64),
            country        LowCardinality(String),
            platform       LowCardinality(String),
            os             LowCardinality(String),
            browser        LowCardinality(String),
            device_class   LowCardinality(String),
            app_version    LowCardinality(String),
            sdk            LowCardinality(String)
        )
        ENGINE = MergeTree
        PARTITION BY toYYYYMM(ts)
        ORDER BY (site_id, toDate(ts), name, anon_id, ts)
        TTL toDateTime(ts) + INTERVAL 13 MONTH
        SETTINGS non_replicated_deduplication_window = 1000",
            // Rollups never hold identifiers, so erasure leaves them untouched.
            "CREATE TABLE IF NOT EXISTS drops_daily (
            site_id    UInt32,
            day        Date,
            event_name LowCardinality(String),
            reason     LowCardinality(String),
            count      UInt64
        )
        ENGINE = SummingMergeTree
        ORDER BY (site_id, day, event_name, reason)
        SETTINGS non_replicated_deduplication_window = 1000",
        ],
    },
    Migration {
        version: 2,
        name: "recent event ids",
        statements: &[
            // The ids of events stored in the last day, so that every node
            // can recognise an event another node already stored. It holds
            // no user, device or session identifier, only random event ids,
            // and is dropped by the TTL.
            "CREATE TABLE IF NOT EXISTS event_ids (
                site_id  UInt32,
                event_id UUID,
                seen_at  DateTime64(3, 'UTC')
            )
            ENGINE = MergeTree
            ORDER BY (site_id, event_id)
            TTL toDateTime(seen_at) + INTERVAL 1 DAY
            SETTINGS non_replicated_deduplication_window = 1000",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_consecutive_from_one_and_named() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(migration.version as usize, index + 1);
            assert!(!migration.name.is_empty());
            assert!(!migration.statements.is_empty());
        }
    }

    #[test]
    fn every_statement_is_idempotent_and_single() {
        for migration in MIGRATIONS {
            for statement in migration.statements {
                assert!(statement.contains("IF NOT EXISTS"), "{statement}");
                assert!(!statement.contains(';'), "one statement per entry");
            }
        }
    }

    #[test]
    fn no_rollup_holds_an_identifier_column() {
        for migration in MIGRATIONS {
            for statement in migration.statements {
                if statement.contains("drops_daily") {
                    for forbidden in ["anon_id", "user_id", "session_id"] {
                        assert!(!statement.contains(forbidden), "{forbidden}");
                    }
                }
            }
        }
    }
}
