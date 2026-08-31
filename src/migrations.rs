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
    // v2 — token columns become nullable. The spec's null discipline says
    // fields that cannot be determined stay null, never guessed as zero.
    // SQLite cannot drop constraints in place, so the table is rebuilt and
    // rows are copied verbatim (v1-era zeros remain zeros: they were
    // recorded values, not guesses).
    "
    ALTER TABLE usage_events RENAME TO usage_events_v1;

    CREATE TABLE usage_events (
        event_id            TEXT PRIMARY KEY,
        workspace_id        TEXT NOT NULL,
        device_id           TEXT NOT NULL REFERENCES devices(device_id),
        source              TEXT NOT NULL,
        tool                TEXT NOT NULL,
        exact_model         TEXT NOT NULL,
        session_id_hash     TEXT,
        ts_utc              TEXT NOT NULL,
        input_tokens        INTEGER,
        cached_input_tokens INTEGER,
        cache_write_tokens  INTEGER,
        output_tokens       INTEGER,
        reasoning_tokens    INTEGER,
        reported_cost_micros INTEGER,
        tool_version        TEXT,
        adapter_version     TEXT,
        imported_at_utc     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );

    INSERT INTO usage_events (
        event_id, workspace_id, device_id, source, tool, exact_model,
        session_id_hash, ts_utc,
        input_tokens, cached_input_tokens, cache_write_tokens,
        output_tokens, reasoning_tokens,
        reported_cost_micros, tool_version, adapter_version, imported_at_utc
    )
    SELECT
        event_id, workspace_id, device_id, source, tool, exact_model,
        session_id_hash, ts_utc,
        input_tokens, cached_input_tokens, cache_write_tokens,
        output_tokens, reasoning_tokens,
        reported_cost_micros, tool_version, adapter_version, imported_at_utc
    FROM usage_events_v1;

    DROP TABLE usage_events_v1;

    CREATE INDEX idx_usage_events_source_ts ON usage_events(source, ts_utc);
    CREATE INDEX idx_usage_events_device_ts ON usage_events(device_id, ts_utc);
    ",
    // v3 — durable sync record identities and inbox idempotency. The original
    // outbox rows predate record IDs; preserve them under opaque legacy IDs.
    "
    ALTER TABLE sync_outbox RENAME TO sync_outbox_v2;

    CREATE TABLE sync_outbox (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        record_id      TEXT NOT NULL UNIQUE,
        record_kind    TEXT NOT NULL,
        payload        BLOB NOT NULL,
        created_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
        sent_at_utc    TEXT
    );

    INSERT INTO sync_outbox (id, record_id, record_kind, payload, created_at_utc, sent_at_utc)
    SELECT id, 'legacy:' || id, record_kind, payload, created_at_utc, sent_at_utc
    FROM sync_outbox_v2;

    DROP TABLE sync_outbox_v2;
    CREATE INDEX idx_sync_outbox_unsent ON sync_outbox(sent_at_utc)
        WHERE sent_at_utc IS NULL;

    CREATE TABLE sync_applied_records (
        record_id     TEXT PRIMARY KEY,
        applied_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );
    ",
    // v4 — version friendly-name/OS metadata independently from sync
    // freshness so a later heartbeat cannot undo a rename.
    "
    ALTER TABLE devices ADD COLUMN metadata_updated_at_utc TEXT;
    ALTER TABLE devices ADD COLUMN metadata_version INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE devices ADD COLUMN revoked_at_utc TEXT;
    UPDATE devices
       SET metadata_updated_at_utc = created_at_utc
     WHERE metadata_updated_at_utc IS NULL;

    CREATE TABLE device_sources (
        device_id TEXT NOT NULL REFERENCES devices(device_id),
        source    TEXT NOT NULL,
        PRIMARY KEY (device_id, source)
    );
    INSERT OR IGNORE INTO device_sources (device_id, source)
    SELECT device_id, source FROM usage_events
    UNION
    SELECT observing_device_id, source FROM quota_snapshots;
    ",
];
