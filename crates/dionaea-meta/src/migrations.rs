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
        -- A value derived from the key that made each tombstone, never the key:
        -- a server that does not have every key the tombstones were made with
        -- must not run.
        ALTER TABLE erasure_tombstones ADD COLUMN key_check TEXT NOT NULL DEFAULT '';
        -- The keys in use, so that checking them does not scan every tombstone.
        CREATE TABLE erasure_keys (key_check TEXT PRIMARY KEY);
    ",
    },
    Migration {
        version: 6,
        name: "erasure_failed_at",
        sql: "
        -- When a request failed, so the identifiers it still holds in the clear
        -- can be forgotten after a while.
        ALTER TABLE erasure_requests ADD COLUMN failed_at INTEGER;
    ",
    },
    Migration {
        version: 7,
        name: "erasure_device",
        sql: "
        -- A request may name one device instead of a user: for a person who
        -- gives the device id, or whose device never identified itself. The
        -- device id is kept in the clear only until the request completes.
        ALTER TABLE erasure_requests ADD COLUMN device_id TEXT;
        -- What the request is for, which the device id no longer says once the
        -- request is complete.
        ALTER TABLE erasure_requests ADD COLUMN is_device INTEGER NOT NULL DEFAULT 0;
    ",
    },
    Migration {
        version: 8,
        name: "retention",
        sql: "
        -- How long a site keeps each family of data, when it differs from the
        -- default; and when each scheduled job last started.
        CREATE TABLE site_retention (
            site_id INTEGER NOT NULL REFERENCES sites(id),
            family  TEXT NOT NULL CHECK (family IN ('analytics')),
            days    INTEGER NOT NULL CHECK (days BETWEEN 1 AND 3650),
            PRIMARY KEY (site_id, family)
        );
        CREATE TABLE job_runs (
            name     TEXT PRIMARY KEY,
            last_run INTEGER NOT NULL
        );
    ",
    },
    Migration {
        version: 9,
        name: "accounts",
        sql: "
        -- The zone a site's days are cut in for reports.
        ALTER TABLE sites ADD COLUMN time_zone TEXT NOT NULL DEFAULT 'UTC';
        -- Passwords are stored as argon2id hashes, sessions and read tokens as
        -- the SHA-256 of a secret that is shown once and never kept.
        CREATE TABLE members (
            id            INTEGER PRIMARY KEY,
            email         TEXT NOT NULL UNIQUE,
            name          TEXT NOT NULL,
            password_hash TEXT NOT NULL,
            created_at    INTEGER NOT NULL,
            disabled_at   INTEGER
        );
        CREATE TABLE memberships (
            member_id INTEGER NOT NULL REFERENCES members(id),
            org_id    INTEGER NOT NULL REFERENCES organizations(id),
            role      TEXT NOT NULL CHECK (role IN ('viewer', 'admin', 'owner')),
            PRIMARY KEY (member_id, org_id)
        );
        CREATE INDEX memberships_by_org ON memberships (org_id);
        CREATE TABLE auth_sessions (
            token_hash TEXT PRIMARY KEY,
            member_id  INTEGER NOT NULL REFERENCES members(id),
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX auth_sessions_by_member ON auth_sessions (member_id);
        CREATE INDEX auth_sessions_by_expiry ON auth_sessions (expires_at);
        CREATE TABLE read_tokens (
            id         INTEGER PRIMARY KEY,
            site_id    INTEGER NOT NULL REFERENCES sites(id),
            label      TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_by INTEGER REFERENCES members(id),
            created_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE INDEX read_tokens_by_site ON read_tokens (site_id);
    ",
    },
    Migration {
        version: 10,
        name: "upload_tokens_and_symbol_files",
        sql: "
        -- A token that can only upload symbol files for one site; kept apart
        -- from read tokens so that one kind can never be looked up as the other.
        CREATE TABLE upload_tokens (
            id         INTEGER PRIMARY KEY,
            site_id    INTEGER NOT NULL REFERENCES sites(id),
            label      TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_by INTEGER REFERENCES members(id),
            created_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE INDEX upload_tokens_by_site ON upload_tokens (site_id);
        -- What was uploaded for which release. The bytes are in the artifact
        -- store; this says what exists.
        CREATE TABLE symbol_files (
            site_id     INTEGER NOT NULL REFERENCES sites(id),
            release     TEXT NOT NULL,
            kind        TEXT NOT NULL CHECK (kind IN ('sourcemap', 'proguard')),
            name        TEXT NOT NULL,
            size        INTEGER NOT NULL CHECK (size >= 0),
            sha256      TEXT NOT NULL,
            uploaded_at INTEGER NOT NULL,
            PRIMARY KEY (site_id, release, kind, name)
        );
    ",
    },
    Migration {
        version: 11,
        name: "issues",
        sql: "
        -- The state a person gave an issue (one per fingerprint). An issue
        -- with no row is unresolved. Counts and timelines are in ClickHouse;
        -- no row holds a user, device or session id.
        CREATE TABLE issues (
            site_id           INTEGER NOT NULL REFERENCES sites(id),
            fingerprint       TEXT NOT NULL,
            state             TEXT NOT NULL CHECK (state IN ('unresolved', 'resolved', 'ignored')),
            resolved_release  TEXT,
            resolved_at       INTEGER,
            regressed_release TEXT,
            regressed_at      INTEGER,
            regressions       INTEGER NOT NULL DEFAULT 0,
            revision          INTEGER NOT NULL DEFAULT 1,
            errors_seen       INTEGER NOT NULL DEFAULT -1,
            checked_at        INTEGER NOT NULL DEFAULT 0,
            updated_at        INTEGER NOT NULL,
            PRIMARY KEY (site_id, fingerprint)
        );
        CREATE INDEX issues_to_check ON issues (state, checked_at);
    ",
    },
];
