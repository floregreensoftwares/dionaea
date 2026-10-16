//! Numbered migrations, applied in order, each in its own transaction and
//! recorded in `_migrations`.

pub(crate) struct Migration {
    pub(crate) version: u32,
    pub(crate) name: &'static str,
    pub(crate) sql: &'static str,
}

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "organizations_sites_keys_schemas",
        sql: "
        CREATE TABLE organizations (
            id         INTEGER PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE sites (
            id         INTEGER PRIMARY KEY,
            org_id     INTEGER NOT NULL REFERENCES organizations(id),
            name       TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE (org_id, name)
        );
        CREATE TABLE write_keys (
            key        TEXT PRIMARY KEY,
            site_id    INTEGER NOT NULL REFERENCES sites(id),
            label      TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE INDEX write_keys_by_site ON write_keys (site_id);
        CREATE TABLE key_origins (
            key    TEXT NOT NULL REFERENCES write_keys(key),
            origin TEXT NOT NULL,
            PRIMARY KEY (key, origin)
        );
        CREATE TABLE schema_versions (
            site_id    INTEGER NOT NULL REFERENCES sites(id),
            version    INTEGER NOT NULL,
            source     TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            retired_at INTEGER,
            PRIMARY KEY (site_id, version)
        );
        CREATE TABLE state (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            generation INTEGER NOT NULL
        );
        INSERT INTO state (id, generation) VALUES (1, 0);
    ",
    },
    Migration {
        version: 2,
        name: "settings",
        sql: "
        -- A change takes effect at a stated time: until then readers see
        -- `previous`, after it `value` (null: the setting is not stored). This
        -- lets every node switch at the same moment.
        CREATE TABLE settings (
            key             TEXT PRIMARY KEY,
            value           TEXT,
            previous        TEXT,
            effective_at_ms INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL
        );
    ",
    },
    Migration {
        version: 3,
        name: "erasure",
        sql: "
        -- A request to erase a user. `user_id` is cleared when it completes;
        -- `user_hash` stays as the audit record of who was erased.
        CREATE TABLE erasure_requests (
            id           INTEGER PRIMARY KEY,
            site_id      INTEGER NOT NULL REFERENCES sites(id),
            user_id      TEXT,
            user_hash    TEXT NOT NULL,
            state        TEXT NOT NULL
                         CHECK (state IN ('requested', 'hidden', 'complete', 'failed')),
            requested_by TEXT NOT NULL,
            requested_at INTEGER NOT NULL,
            hidden_at    INTEGER,
            completed_at INTEGER,
            anon_count   INTEGER NOT NULL DEFAULT 0,
            detail       TEXT,
            claimed_by   TEXT,
            lease_until  INTEGER
        );
        CREATE INDEX erasure_by_state ON erasure_requests (state, id);
        -- Hashes of erased users and of the devices linked to them. The
        -- ingest path drops anything that matches.
        CREATE TABLE erasure_tombstones (
            site_id    INTEGER NOT NULL,
            kind       TEXT NOT NULL CHECK (kind IN ('user', 'anon')),
            hash       TEXT NOT NULL,
            request_id INTEGER NOT NULL REFERENCES erasure_requests(id),
            created_at INTEGER NOT NULL,
            PRIMARY KEY (site_id, kind, hash)
        );
        -- The devices found linked to the user, kept in the clear only while
        -- the job needs them to delete and to verify; removed when the
        -- request completes. The tombstones keep only their hashes.
        CREATE TABLE erasure_anons (
            request_id INTEGER NOT NULL REFERENCES erasure_requests(id),
            anon_id    TEXT NOT NULL,
            PRIMARY KEY (request_id, anon_id)
        );
    ",
    },
    Migration {
        version: 4,
        name: "erasure_coordination",
        sql: "
        -- How often a request has met trouble that retrying may fix, and when
        -- it may be tried again.
        ALTER TABLE erasure_requests ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE erasure_requests ADD COLUMN retry_after INTEGER;
        -- Named leases: one node at a time does the work of a job.
        CREATE TABLE leases (
            name   TEXT PRIMARY KEY,
            holder TEXT NOT NULL,
            until  INTEGER NOT NULL
        );
    ",
    },
    Migration {
        version: 5,
        name: "erasure_key_check",
        sql: "
        -- A value derived from the key that keys the tombstone hashes, never
        -- the key: a server started with another key must not run.
        ALTER TABLE state ADD COLUMN erasure_key_check TEXT;
    ",
    },
];
