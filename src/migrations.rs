//! Ordered schema migrations. Each entry upgrades the database to
//! `index + 1`, tracked in SQLite's `user_version` so re-runs are no-ops.

pub const MIGRATIONS: &[&str] = &[
    // v1 — core entities: devices, source config, usage events, quota
    // snapshots, sync outbox/state, adapter state, metadata.
    "
    CREATE TABLE devices (
        device_id        TEXT PRIMARY KEY,
        friendly_name    TEXT NOT NULL,
        os               TEXT NOT NULL DEFAULT '',
        arch             TEXT NOT NULL DEFAULT '',
        created_at_utc   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        last_sync_at_utc TEXT
    );

    CREATE TABLE source_config (
        source TEXT PRIMARY KEY,
        mode   TEXT NOT NULL DEFAULT 'auto'
               CHECK (mode IN ('auto', 'enabled', 'disabled'))
    );

    CREATE TABLE usage_events (
        event_id            TEXT PRIMARY KEY,
        -- workspace_id is an opaque random identifier minted by `aiu init`;
        -- it is never derived from or joined to project names or paths.
        workspace_id        TEXT NOT NULL,
        device_id           TEXT NOT NULL REFERENCES devices(device_id),
        source              TEXT NOT NULL,
        tool                TEXT NOT NULL,
        exact_model         TEXT NOT NULL,
        session_id_hash     TEXT,
        ts_utc              TEXT NOT NULL,
        input_tokens        INTEGER NOT NULL DEFAULT 0,
        cached_input_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
        output_tokens       INTEGER NOT NULL DEFAULT 0,
        reasoning_tokens    INTEGER NOT NULL DEFAULT 0,
        reported_cost_micros INTEGER,
        tool_version        TEXT,
        adapter_version     TEXT,
        imported_at_utc     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );

    CREATE INDEX idx_usage_events_source_ts ON usage_events(source, ts_utc);
    CREATE INDEX idx_usage_events_device_ts ON usage_events(device_id, ts_utc);

    CREATE TABLE quota_snapshots (
        id                  INTEGER PRIMARY KEY AUTOINCREMENT,
        source              TEXT NOT NULL,
        window              TEXT NOT NULL,
        used_percent        REAL NOT NULL,
        resets_at_utc       TEXT,
        observed_at_utc     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        observing_device_id TEXT NOT NULL REFERENCES devices(device_id)
    );

    CREATE INDEX idx_quota_snapshots_lookup
        ON quota_snapshots(source, window, observed_at_utc);

    CREATE TABLE sync_outbox (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        record_kind    TEXT NOT NULL,
        payload        BLOB NOT NULL,
        created_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        sent_at_utc    TEXT
    );
    CREATE INDEX idx_sync_outbox_unsent ON sync_outbox(sent_at_utc) WHERE sent_at_utc IS NULL;

    CREATE TABLE sync_cursors (
        name           TEXT PRIMARY KEY,
        value          TEXT NOT NULL,
        updated_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );

    CREATE TABLE adapter_state (
        adapter      TEXT PRIMARY KEY,
        last_position TEXT,
        updated_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );

    CREATE TABLE metadata (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
];
