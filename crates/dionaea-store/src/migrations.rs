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

/// The rows `events` yields as measurements: one per measured value of an
/// `app_start`, `screen_load`, `http_request` or `web_vital` event. The view
/// and the one-time copy of older events share it. It ends in a WHERE clause,
/// so a condition can follow with ` AND`.
macro_rules! measurement_select {
    () => {
        "SELECT
            site_id, event_id, ts, received_at, anon_id, session_id,
            measure.1 AS metric,
            measure.2 AS value,
            if(mapContains(props_num, 'sample_rate'), props_num['sample_rate'], 1.0) AS sample_rate,
            multiIf(
                name = 'app_start', props_str['start'],
                name = 'http_request', props_str['method'],
                name = 'web_vital', props_str['rating'],
                ''
            ) AS detail,
            screen,
            multiIf(name IN ('http_request', 'web_vital'), props_str['route'], '') AS route,
            if(name = 'http_request', props_str['host'], '') AS host,
            if(name = 'http_request', toUInt16(props_num['status']), 0) AS status,
            if(name = 'http_request', props_str['connection'], '') AS connection,
            platform, os, browser, device_class, app_version, country, sdk
        FROM events
        ARRAY JOIN arrayFilter(
            m -> m.3 = 1,
            multiIf(
                name = 'app_start', [
                    ('app_start_first_frame', props_num['first_frame'], 1),
                    ('app_start_interactive', props_num['interactive'],
                        toUInt8(mapContains(props_num, 'interactive')))],
                name = 'screen_load', [
                    ('screen_load_render', props_num['render'], 1),
                    ('screen_load_interactive', props_num['interactive'],
                        toUInt8(mapContains(props_num, 'interactive')))],
                name = 'http_request', [
                    ('http_duration', props_num['duration'], 1)],
                name = 'web_vital', [
                    (concat('web_vital_', props_str['name']), props_num['value'], 1)],
                [('', 0., 0)]
            )
        ) AS measure
        WHERE name IN ('app_start', 'screen_load', 'http_request', 'web_vital')"
    };
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
    Migration {
        version: 7,
        name: "sessions deduplication",
        statements: &[
            // The table was created without a window, so a retried batch
            // (same token) was summed twice into `events`. Setting the
            // window is the same when run again.
            "ALTER TABLE sessions MODIFY SETTING non_replicated_deduplication_window = 1000",
        ],
    },
    Migration {
        version: 8,
        name: "measurements, releases and health rollups",
        statements: &[
            // One row per measured value of a health event: an app start
            // (first frame, interactive), a screen load (render,
            // interactive), a network call (duration) or a web vital. It
            // holds the device and session ids, so erasure and retention
            // cover it. `sample_rate` is the share of such events the server kept,
            // read from the `sample_rate` number it stores on a sampled event,
            // and 1 for an event that carries none.
            "CREATE TABLE IF NOT EXISTS measurements (
                site_id      UInt32,
                event_id     UUID,
                ts           DateTime64(3, 'UTC'),
                received_at  DateTime64(3, 'UTC'),
                anon_id      String,
                session_id   String,
                metric       LowCardinality(String),
                value        Float64,
                sample_rate  Float64,
                detail       LowCardinality(String),
                screen       LowCardinality(String),
                route        String,
                host         String,
                status       UInt16,
                connection   LowCardinality(String),
                platform     LowCardinality(String),
                os           LowCardinality(String),
                browser      LowCardinality(String),
                device_class LowCardinality(String),
                app_version  LowCardinality(String),
                country      LowCardinality(String),
                sdk          LowCardinality(String)
            )
            ENGINE = MergeTree
            PARTITION BY toYYYYMM(ts)
            ORDER BY (site_id, metric, ts, event_id)
            SETTINGS non_replicated_deduplication_window = 1000",
            // Errors per issue, release and day. No identifier is stored:
            // who was affected is read from `errors`, which erasure covers.
            "CREATE TABLE IF NOT EXISTS issues_daily (
                site_id     UInt32,
                fingerprint String,
                app_version LowCardinality(String),
                day         Date,
                events      SimpleAggregateFunction(sum, UInt64),
                first_ts    SimpleAggregateFunction(min, DateTime64(3, 'UTC')),
                last_ts     SimpleAggregateFunction(max, DateTime64(3, 'UTC'))
            )
            ENGINE = AggregatingMergeTree
            ORDER BY (site_id, fingerprint, app_version, day)
            TTL day + INTERVAL 2 YEAR
            SETTINGS non_replicated_deduplication_window = 1000",
            // Percentile states of each measurement per place (route, or
            // screen when there is none), release, device class and day.
            // The quantile levels are fixed here; no identifier is stored.
            "CREATE TABLE IF NOT EXISTS perf_daily (
                site_id      UInt32,
                metric       LowCardinality(String),
                target       String,
                detail       LowCardinality(String),
                app_version  LowCardinality(String),
                device_class LowCardinality(String),
                day          Date,
                samples      SimpleAggregateFunction(sum, UInt64),
                weight       SimpleAggregateFunction(sum, Float64),
                quantiles    AggregateFunction(quantilesTDigest(0.5, 0.75, 0.9, 0.95, 0.99), Float64)
            )
            ENGINE = AggregatingMergeTree
            ORDER BY (site_id, metric, target, detail, app_version, device_class, day)
            TTL day + INTERVAL 2 YEAR
            SETTINGS non_replicated_deduplication_window = 1000",
            // When each release was first and last received, so the order
            // of releases is one small read instead of a scan of `events`.
            // It keeps no identifier and outlives the events' retention.
            "CREATE TABLE IF NOT EXISTS releases (
                site_id     UInt32,
                app_version String,
                first_seen  SimpleAggregateFunction(min, DateTime64(3, 'UTC')),
                last_seen   SimpleAggregateFunction(max, DateTime64(3, 'UTC'))
            )
            ENGINE = AggregatingMergeTree
            ORDER BY (site_id, app_version)
            TTL toDateTime(last_seen) + INTERVAL 2 YEAR
            SETTINGS non_replicated_deduplication_window = 1000",
            // The views are fed by every insert into events (or into the
            // table before them), like errors_mv.
            concat!(
                "CREATE MATERIALIZED VIEW IF NOT EXISTS measurements_mv TO measurements
                DEFINER = CURRENT_USER SQL SECURITY DEFINER AS ",
                measurement_select!()
            ),
            "CREATE MATERIALIZED VIEW IF NOT EXISTS issues_daily_mv TO issues_daily
            DEFINER = CURRENT_USER SQL SECURITY DEFINER AS
            SELECT
                site_id, fingerprint, app_version,
                toDate(ts) AS day,
                count() AS events,
                min(ts) AS first_ts,
                max(ts) AS last_ts
            FROM errors
            GROUP BY site_id, fingerprint, app_version, day",
            "CREATE MATERIALIZED VIEW IF NOT EXISTS perf_daily_mv TO perf_daily
            DEFINER = CURRENT_USER SQL SECURITY DEFINER AS
            SELECT
                site_id, metric,
                if(route != '', route, screen) AS target,
                detail, app_version, device_class,
                toDate(ts) AS day,
                count() AS samples,
                sum(1 / sample_rate) AS weight,
                quantilesTDigestState(0.5, 0.75, 0.9, 0.95, 0.99)(value) AS quantiles
            FROM measurements
            GROUP BY site_id, metric, target, detail, app_version, device_class, day",
            "CREATE MATERIALIZED VIEW IF NOT EXISTS releases_mv TO releases
            DEFINER = CURRENT_USER SQL SECURITY DEFINER AS
            SELECT
                site_id, app_version,
                min(received_at) AS first_seen,
                max(received_at) AS last_seen
            FROM events
            WHERE app_version != ''
            GROUP BY site_id, app_version",
            // What the events stored before this migration would have
            // produced. Each statement does nothing when run again:
            // measurements skips the events it already has (and `perf_daily`
            // follows `measurements`), releases takes minimum and maximum,
            // and issues_daily only fills an empty table. Ingest is not
            // running on the node that migrates, since the batcher starts
            // after the tables are prepared.
            concat!(
                "INSERT INTO measurements ",
                measurement_select!(),
                " AND (site_id, event_id) NOT IN (SELECT site_id, event_id FROM measurements)"
            ),
            "INSERT INTO releases
            SELECT site_id, app_version, min(received_at), max(received_at)
            FROM events WHERE app_version != '' GROUP BY site_id, app_version",
            "INSERT INTO issues_daily
            SELECT site_id, fingerprint, app_version, toDate(ts) AS day,
                   count(), min(ts), max(ts)
            FROM errors
            WHERE (SELECT count() FROM issues_daily) = 0
            GROUP BY site_id, fingerprint, app_version, day",
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
                    statement.contains("IF NOT EXISTS")
                        || statement.contains(" MODIFY TTL ")
                        || statement.contains(" MODIFY SETTING ")
                        || statement.starts_with("INSERT INTO "),
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
                if statement.starts_with("CREATE TABLE")
                    && ["drops_daily", "issues_daily", "perf_daily"]
                        .iter()
                        .any(|rollup| statement.contains(rollup))
                {
                    for forbidden in ["anon_id", "user_id", "session_id"] {
                        assert!(!statement.contains(forbidden), "{forbidden}");
                    }
                }
            }
        }
    }
}
