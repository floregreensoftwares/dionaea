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
    Migration {
        version: 3,
        name: "identity",
        statements: &[
            // Which user an anonymous id was linked to. One row per pair, so a
            // device that signed in as two people keeps both links: erasing
            // either person reaches the events of the whole device. The
            // latest link of an anonymous id is the one queries use.
            "CREATE TABLE IF NOT EXISTS identity_alias (
                site_id   UInt32,
                anon_id   String,
                user_id   String,
                linked_at DateTime64(3, 'UTC'),
                INDEX by_user user_id TYPE bloom_filter GRANULARITY 4
            )
            ENGINE = ReplacingMergeTree(linked_at)
            ORDER BY (site_id, anon_id, user_id)
            SETTINGS non_replicated_deduplication_window = 1000",
            // The declared traits of a user as of the latest identify that
            // carried any.
            "CREATE TABLE IF NOT EXISTS user_traits (
                site_id    UInt32,
                user_id    String,
                traits_str Map(LowCardinality(String), String),
                traits_num Map(LowCardinality(String), Float64),
                updated_at DateTime64(3, 'UTC')
            )
            ENGINE = ReplacingMergeTree(updated_at)
            ORDER BY (site_id, user_id)
            SETTINGS non_replicated_deduplication_window = 1000",
        ],
    },
    Migration {
        version: 4,
        name: "sessions",
        statements: &[
            // One row per session and device and insert block; rows of one
            // session merge into one in the background and are always read
            // with GROUP BY, so a session split over blocks is one session.
            // It holds the device id, so erasure covers it.
            "CREATE TABLE IF NOT EXISTS sessions (
                site_id      UInt32,
                session_id   String,
                anon_id      String,
                started      Date,
                first_ts     SimpleAggregateFunction(min, DateTime64(3, 'UTC')),
                last_ts      SimpleAggregateFunction(max, DateTime64(3, 'UTC')),
                events       SimpleAggregateFunction(sum, UInt64),
                entry_screen AggregateFunction(argMin, LowCardinality(String), DateTime64(3, 'UTC')),
                exit_screen  AggregateFunction(argMax, LowCardinality(String), DateTime64(3, 'UTC')),
                screens      AggregateFunction(uniq, LowCardinality(String)),
                platform     AggregateFunction(argMin, LowCardinality(String), DateTime64(3, 'UTC')),
                app_version  AggregateFunction(argMin, LowCardinality(String), DateTime64(3, 'UTC')),
                country      AggregateFunction(argMin, LowCardinality(String), DateTime64(3, 'UTC'))
            )
            ENGINE = AggregatingMergeTree
            PARTITION BY toYYYYMM(started)
            ORDER BY (site_id, session_id, anon_id)",
            // Fed by every insert into events; only events that carry the
            // session id the SDK generated take part. Events stored before
            // this migration are not summarized. It runs with the rights of
            // the user that migrates, so the ingest user, which may only
            // insert into events, needs no other privilege for it.
            "CREATE MATERIALIZED VIEW IF NOT EXISTS sessions_mv TO sessions
            DEFINER = CURRENT_USER SQL SECURITY DEFINER AS
            SELECT
                site_id, session_id, anon_id,
                toDate(min(ts)) AS started,
                min(ts) AS first_ts,
                max(ts) AS last_ts,
                count() AS events,
                argMinState(screen, ts) AS entry_screen,
                argMaxState(screen, ts) AS exit_screen,
                uniqState(screen) AS screens,
                argMinState(platform, ts) AS platform,
                argMinState(app_version, ts) AS app_version,
                argMinState(country, ts) AS country
            FROM events
            WHERE session_id != ''
            GROUP BY site_id, session_id, anon_id",
            // For plain HTTP integrations whose events carry no session id: the
            // session is derived at query time with the 30 minute inactivity
            // rule. A view stores nothing.
            "CREATE VIEW IF NOT EXISTS derived_sessions AS
            SELECT
                site_id, anon_id, number AS session_number,
                min(ts) AS first_ts,
                max(ts) AS last_ts,
                count() AS events,
                argMin(screen, ts) AS entry_screen,
                argMax(screen, ts) AS exit_screen,
                uniq(screen) AS screens
            FROM (
                SELECT
                    site_id, anon_id, ts, screen,
                    sum(starts) OVER (
                        PARTITION BY site_id, anon_id ORDER BY ts, event_id
                        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                    ) AS number
                FROM (
                    SELECT
                        site_id, anon_id, ts, screen, event_id,
                        if(
                            lagInFrame(ts, 1, toDateTime64(0, 3, 'UTC')) OVER (
                                PARTITION BY site_id, anon_id ORDER BY ts, event_id
                                ROWS BETWEEN 1 PRECEDING AND CURRENT ROW
                            ) + INTERVAL 30 MINUTE < ts,
                            1, 0
                        ) AS starts
                    FROM events
                    WHERE session_id = ''
                )
            )
            GROUP BY site_id, anon_id, number",
        ],
    },
    Migration {
        version: 5,
        name: "retention",
        statements: &[
            // Each site keeps its events for its own number of days, applied by
            // the retention job, so the table's own rule of 13 months for every
            // site goes. What is left is a rule that cannot fire in the life of
            // the product: removing a rule that is not there is an error and
            // this statement must be safe to run again, and 60 years is the
            // most that fits a DateTime (which ends in 2106) for any event
            // time the ingest path accepts.
            "ALTER TABLE events MODIFY TTL toDateTime(ts) + INTERVAL 60 YEAR",
        ],
    },
    Migration {
        version: 6,
        name: "errors",
        statements: &[
            // One row per error event, with the fields of the event that
            // matter for grouping and listing errors in columns of their own.
            // It holds the device and session ids, so erasure and retention
            // cover it.
            "CREATE TABLE IF NOT EXISTS errors (
                site_id     UInt32,
                event_id    UUID,
                ts          DateTime64(3, 'UTC'),
                received_at DateTime64(3, 'UTC'),
                anon_id     String,
                session_id  String,
                fingerprint String,
                symbols     LowCardinality(String),
                kind        LowCardinality(String),
                error_type  String,
                message     String,
                frames      String,
                screen      LowCardinality(String),
                platform    LowCardinality(String),
                os          LowCardinality(String),
                browser     LowCardinality(String),
                app_version LowCardinality(String),
                country     LowCardinality(String),
                sdk         LowCardinality(String)
            )
            ENGINE = MergeTree
            PARTITION BY toYYYYMM(ts)
            ORDER BY (site_id, fingerprint, ts, event_id)
            SETTINGS non_replicated_deduplication_window = 1000",
            // Fed by every insert into events, like sessions_mv. Error events
            // stored before this migration are not copied; they carry no
            // fingerprint either.
            "CREATE MATERIALIZED VIEW IF NOT EXISTS errors_mv TO errors
            DEFINER = CURRENT_USER SQL SECURITY DEFINER AS
            SELECT
                site_id, event_id, ts, received_at, anon_id, session_id,
                props_str['fingerprint'] AS fingerprint,
                props_str['symbols'] AS symbols,
                props_str['kind'] AS kind,
                props_str['type'] AS error_type,
                props_str['message'] AS message,
                props_str['frames'] AS frames,
                screen, platform, os, browser, app_version, country, sdk
            FROM events
            WHERE name = 'app_error'",
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
                // A statement that sets a table property is the same when run
                // again; the others must not fail when the object exists.
                assert!(
                    statement.contains("IF NOT EXISTS") || statement.contains(" MODIFY TTL "),
                    "{statement}"
                );
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
