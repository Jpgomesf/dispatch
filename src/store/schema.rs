//! Migrations, applied in order at open; `schema_version` records the ones applied.

/// Each entry is one migration; never edit a released one, append a new one.
pub const MIGRATIONS: &[&str] = &[
    // 1: events, claims, cards, cursors.
    "
    CREATE TABLE events (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        runner      TEXT NOT NULL,
        source      TEXT NOT NULL,
        external_id TEXT NOT NULL,
        kind        TEXT NOT NULL,
        mentions_me INTEGER NOT NULL,
        sender      TEXT,
        occurred_at TEXT NOT NULL,
        payload     TEXT NOT NULL,
        status      TEXT NOT NULL DEFAULT 'new',
        created_at  TEXT NOT NULL,
        UNIQUE (source, external_id)
    );
    CREATE INDEX events_by_runner_status ON events (runner, status);
    CREATE TABLE claims (
        key         TEXT PRIMARY KEY,
        runner      TEXT NOT NULL,
        lease_until INTEGER NOT NULL,
        claimed_at  TEXT NOT NULL
    );
    CREATE TABLE cards (
        runner     TEXT NOT NULL,
        ref        TEXT NOT NULL,
        status     TEXT NOT NULL,
        blocked_by TEXT NOT NULL DEFAULT '[]',
        pr_url     TEXT,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (runner, ref)
    );
    CREATE TABLE cursors (
        runner TEXT NOT NULL,
        key    TEXT NOT NULL,
        value  TEXT NOT NULL,
        PRIMARY KEY (runner, key)
    );
    ",
];
