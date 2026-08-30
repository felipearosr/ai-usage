//! SQLite storage: connection handling, migrations, and the write API.
//!
//! All timestamps are UTC RFC 3339 strings. Event identity is `event_id`
//! (deterministic, built by adapters later), so re-running collection or
//! import never double-counts.

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;
use crate::migrations::MIGRATIONS;

pub struct Store {
    conn: Connection,
}

pub(crate) struct PendingSyncRecord {
    pub outbox_id: i64,
    pub payload: Vec<u8>,
}

pub struct NewDevice {
    pub device_id: String,
    pub friendly_name: String,
    pub os: String,
    pub arch: String,
    pub last_sync_at_utc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceSyncState {
    pub workspace_id: String,
    pub device_id: String,
    pub friendly_name: String,
    pub os: String,
    pub arch: String,
    pub metadata_updated_at_utc: String,
    pub metadata_version: i64,
    pub last_sync_at_utc: Option<String>,
    pub sources: Vec<String>,
}

#[derive(Default, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NewEvent {
    pub event_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub source: String,
    pub tool: String,
    pub exact_model: String,
    pub session_id_hash: Option<String>,
    pub ts_utc: String,
    /// Token counts are `None` when the source did not report the class:
    /// unknown stays null, never guessed as zero (spec normalization rules).
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub reported_cost_micros: Option<i64>,
    pub tool_version: Option<String>,
    pub adapter_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewSnapshot {
    pub source: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at_utc: Option<String>,
    pub observed_at_utc: String,
    pub observing_device_id: String,
}

/// Per-source override state (spec story 24). `auto` follows detection,
/// `enabled` forces collection, `disabled` excludes the source from
/// collection and reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    Auto,
    Enabled,
    Disabled,
}

impl SourceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceMode::Auto => "auto",
            SourceMode::Enabled => "enabled",
            SourceMode::Disabled => "disabled",
        }
    }

    /// Parses a persisted mode string. Unknown values fall back to `auto`, the
    /// default, so a forward-written future mode never breaks collection.
    pub fn parse(s: &str) -> Self {
        match s {
            "enabled" => SourceMode::Enabled,
            "disabled" => SourceMode::Disabled,
            _ => SourceMode::Auto,
        }
    }
}

impl Store {
    /// Opens (creating if needed) the database file and applies migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory database for tests and the report seam.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        apply_migrations(&conn)?;
        Ok(Store { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Inserts or updates a device row. Returns true when a new row is created.
    pub fn ensure_device(&self, device: &NewDevice) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO devices (device_id, friendly_name, os, arch, last_sync_at_utc)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(device_id) DO NOTHING",
            rusqlite::params![
                device.device_id,
                device.friendly_name,
                device.os,
                device.arch,
                device.last_sync_at_utc
            ],
        )?;
        Ok(changed == 1)
    }

    /// Creates the local device row or refreshes its user-selected identity.
    /// Synced placeholder rows still use `ensure_device`, which never
    /// overwrites names learned through the encrypted record stream.
    pub fn upsert_local_device(&self, device: &NewDevice) -> Result<()> {
        self.conn.execute(
            "INSERT INTO devices (device_id, friendly_name, os, arch, last_sync_at_utc)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(device_id) DO UPDATE SET
                 friendly_name = excluded.friendly_name,
                 os = excluded.os,
                 arch = excluded.arch",
            rusqlite::params![
                device.device_id,
                device.friendly_name,
                device.os,
                device.arch,
                device.last_sync_at_utc
            ],
        )?;
        Ok(())
    }

    pub fn touch_device_sync(&self, device_id: &str, ts_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET last_sync_at_utc = ?2 WHERE device_id = ?1",
            rusqlite::params![device_id, ts_utc],
        )?;
        Ok(())
    }

    pub fn device_sync_state(
        &self,
        workspace_id: &str,
        device_id: &str,
        sync_at_utc: Option<&str>,
    ) -> Result<DeviceSyncState> {
        let (friendly_name, os, arch, metadata_updated_at_utc, metadata_version, stored_sync) =
            self.conn.query_row(
                "SELECT friendly_name, os, arch,
                        COALESCE(metadata_updated_at_utc, created_at_utc),
                        metadata_version, last_sync_at_utc
                 FROM devices WHERE device_id = ?1",
                rusqlite::params![device_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;
        Ok(DeviceSyncState {
            workspace_id: workspace_id.to_string(),
            device_id: device_id.to_string(),
            friendly_name,
            os,
            arch,
            metadata_updated_at_utc,
            metadata_version,
            last_sync_at_utc: sync_at_utc.map(str::to_string).or(stored_sync),
            sources: self.device_sources(device_id)?,
        })
    }

    pub fn apply_device_sync_state(&self, device: &DeviceSyncState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO devices (
                 device_id, friendly_name, os, arch, last_sync_at_utc,
                 metadata_updated_at_utc, metadata_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(device_id) DO UPDATE SET
                 friendly_name = CASE
                     WHEN (devices.os = '' AND devices.arch = '')
                       OR excluded.metadata_version > devices.metadata_version
                       OR (excluded.metadata_version = devices.metadata_version
                           AND excluded.friendly_name > devices.friendly_name)
                     THEN excluded.friendly_name ELSE devices.friendly_name END,
                 os = CASE
                     WHEN (devices.os = '' AND devices.arch = '')
                       OR excluded.metadata_version > devices.metadata_version
                       OR (excluded.metadata_version = devices.metadata_version
                           AND excluded.friendly_name > devices.friendly_name)
                     THEN excluded.os ELSE devices.os END,
                 arch = CASE
                     WHEN (devices.os = '' AND devices.arch = '')
                       OR excluded.metadata_version > devices.metadata_version
                       OR (excluded.metadata_version = devices.metadata_version
                           AND excluded.friendly_name > devices.friendly_name)
                     THEN excluded.arch ELSE devices.arch END,
                 metadata_updated_at_utc = MAX(
                     excluded.metadata_updated_at_utc,
                     COALESCE(devices.metadata_updated_at_utc, devices.created_at_utc)
                 ),
                 metadata_version = MAX(excluded.metadata_version, devices.metadata_version),
                 last_sync_at_utc = CASE
                     WHEN excluded.last_sync_at_utc IS NOT NULL
                      AND (devices.last_sync_at_utc IS NULL OR excluded.last_sync_at_utc > devices.last_sync_at_utc)
                     THEN excluded.last_sync_at_utc ELSE devices.last_sync_at_utc END",
            rusqlite::params![
                device.device_id,
                device.friendly_name,
                device.os,
                device.arch,
                device.last_sync_at_utc,
                device.metadata_updated_at_utc,
                device.metadata_version,
            ],
        )?;
        self.conn.execute(
            "DELETE FROM device_sources WHERE device_id = ?1",
            rusqlite::params![device.device_id],
        )?;
        for source in &device.sources {
            self.conn.execute(
                "INSERT OR IGNORE INTO device_sources (device_id, source) VALUES (?1, ?2)",
                rusqlite::params![device.device_id, source],
            )?;
        }
        Ok(())
    }

    pub fn set_device_source(&self, device_id: &str, source: &str, tracked: bool) -> Result<()> {
        self.ensure_device(&NewDevice {
            device_id: device_id.to_string(),
            friendly_name: device_id.to_string(),
            os: String::new(),
            arch: String::new(),
            last_sync_at_utc: None,
        })?;
        if tracked {
            self.conn.execute(
                "INSERT OR IGNORE INTO device_sources (device_id, source) VALUES (?1, ?2)",
                rusqlite::params![device_id, source],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM device_sources WHERE device_id = ?1 AND source = ?2",
                rusqlite::params![device_id, source],
            )?;
        }
        Ok(())
    }

    pub fn device_sources(&self, device_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source FROM device_sources WHERE device_id = ?1 ORDER BY source")?;
        let sources = stmt
            .query_map(rusqlite::params![device_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(sources)
    }

    pub fn device_ids_matching(&self, reference: &str) -> Result<Vec<String>> {
        let exact = self.conn.query_row(
            "SELECT device_id FROM devices WHERE device_id = ?1",
            rusqlite::params![reference],
            |row| row.get::<_, String>(0),
        );
        match exact {
            Ok(id) => return Ok(vec![id]),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error.into()),
        }
        let mut stmt = self.conn.prepare(
            "SELECT device_id FROM devices WHERE friendly_name = ?1 ORDER BY device_id LIMIT 2",
        )?;
        let ids = stmt
            .query_map(rusqlite::params![reference], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    pub fn rename_device(&self, device_id: &str, name: &str, updated_at_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices
             SET friendly_name = ?2,
                 metadata_updated_at_utc = ?3,
                 metadata_version = metadata_version + 1
             WHERE device_id = ?1",
            rusqlite::params![device_id, name, updated_at_utc],
        )?;
        Ok(())
    }

    pub fn mark_device_revoked(&self, device_id: &str, revoked_at_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices
             SET revoked_at_utc = CASE
                 WHEN revoked_at_utc IS NULL OR ?2 > revoked_at_utc THEN ?2
                 ELSE revoked_at_utc END
             WHERE device_id = ?1",
            rusqlite::params![device_id, revoked_at_utc],
        )?;
        Ok(())
    }

    /// Records a normalized usage event. Returns false when the event was a
    /// duplicate (same deterministic event_id) and was ignored.
    pub fn record_event(&self, e: &NewEvent) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO usage_events (
                 event_id, workspace_id, device_id, source, tool, exact_model,
                 session_id_hash, ts_utc,
                 input_tokens, cached_input_tokens, cache_write_tokens,
                 output_tokens, reasoning_tokens,
                 reported_cost_micros, tool_version, adapter_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            rusqlite::params![
                e.event_id,
                e.workspace_id,
                e.device_id,
                e.source,
                e.tool,
                e.exact_model,
                e.session_id_hash,
                e.ts_utc,
                e.input_tokens,
                e.cached_input_tokens,
                e.cache_write_tokens,
                e.output_tokens,
                e.reasoning_tokens,
                e.reported_cost_micros,
                e.tool_version,
                e.adapter_version
            ],
        )?;
        self.set_device_source(&e.device_id, &e.source, true)?;
        Ok(changed == 1)
    }

    /// Records a quota snapshot (a state observation of the shared account).
    pub fn record_snapshot(&self, s: &NewSnapshot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO quota_snapshots (
                 source, window, used_percent, resets_at_utc,
                 observed_at_utc, observing_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                s.source,
                s.window,
                s.used_percent,
                s.resets_at_utc,
                s.observed_at_utc,
                s.observing_device_id
            ],
        )?;
        self.set_device_source(&s.observing_device_id, &s.source, true)?;
        Ok(())
    }

    /// Records a quota snapshot unless it is byte-identical to the latest
    /// observation for the same source/window. Re-running collection or
    /// import therefore does not grow snapshot history with no-op rows.
    /// Returns true when a new observation was stored.
    pub fn record_snapshot_if_changed(&self, s: &NewSnapshot) -> Result<bool> {
        self.set_device_source(&s.observing_device_id, &s.source, true)?;
        let latest = self.conn.query_row(
            "SELECT id, used_percent, resets_at_utc, observed_at_utc FROM quota_snapshots
             WHERE source = ?1 AND window = ?2
             ORDER BY observed_at_utc DESC, id DESC LIMIT 1",
            rusqlite::params![s.source, s.window],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        );
        let changed = match latest {
            Ok((id, percent, resets, observed_at)) => {
                let value_changed = !(percent == s.used_percent && resets == s.resets_at_utc);
                if !value_changed && s.observed_at_utc > observed_at {
                    self.conn.execute(
                        "UPDATE quota_snapshots
                         SET observed_at_utc = ?2, observing_device_id = ?3
                         WHERE id = ?1",
                        rusqlite::params![id, s.observed_at_utc, s.observing_device_id],
                    )?;
                }
                value_changed
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => true,
            Err(e) => return Err(e.into()),
        };
        if changed {
            self.record_snapshot(s)?;
        }
        Ok(changed)
    }

    /// Explicit transaction for batching many writes into periodic commits.
    /// Rolls back on drop unless committed.
    pub fn transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// Records an adapter diagnostic (e.g. unrecognized upstream format) in
    /// the local database so failures stay visible after the process exits.
    /// Newest diagnostic per key wins; history beyond that is not kept.
    pub fn record_diagnostic(&self, key: &str, detail: &str, ts_utc: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata (key, value)
             VALUES ('diagnostic:' || ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, format!("{ts_utc} {detail}")],
        )?;
        Ok(())
    }

    /// Returns the most recent diagnostic recorded for `source`, if any.
    pub fn diagnostic_for(&self, source: &str) -> Result<Option<String>> {
        let value = self.conn.query_row(
            "SELECT value FROM metadata WHERE key = 'diagnostic:' || ?1",
            rusqlite::params![source],
            |row| row.get::<_, String>(0),
        );
        match value {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Reads an arbitrary metadata value. `None` when the key is absent.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let value = self.conn.query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        );
        match value {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes (or overwrites) an arbitrary metadata value.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Records a per-source override. Overrides are managed by `aiu sources`
    /// (issue 07); this is the storage half.
    pub fn set_source_mode(&self, source: &str, mode: SourceMode) -> Result<()> {
        self.conn.execute(
            "INSERT INTO source_config (source, mode) VALUES (?1, ?2)
             ON CONFLICT(source) DO UPDATE SET mode = excluded.mode",
            rusqlite::params![source, mode.as_str()],
        )?;
        Ok(())
    }

    /// The current override for `source`, defaulting to `auto` when no row
    /// exists (never set, or set back to the default). Detection consumes this
    /// to decide which sources to actually collect (issue 07).
    pub fn source_mode(&self, source: &str) -> Result<SourceMode> {
        let value = self.conn.query_row(
            "SELECT mode FROM source_config WHERE source = ?1",
            rusqlite::params![source],
            |row| row.get::<_, String>(0),
        );
        match value {
            Ok(mode) => Ok(SourceMode::parse(&mode)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(SourceMode::Auto),
            Err(e) => Err(e.into()),
        }
    }

    /// The sources aiu is configured to track: every source that has usage or
    /// quota data plus every source explicitly configured non-disabled,
    /// minus any source deliberately disabled. Disabled sources are excluded
    /// even when historical data remains (spec: disabled = excluded from
    /// reports), so a subset of sources configured renders correctly.
    pub fn configured_sources(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT source FROM (
                 SELECT source FROM usage_events
                 UNION
                 SELECT source FROM quota_snapshots
                 UNION
                 SELECT source FROM source_config WHERE mode <> 'disabled'
             )
             WHERE source NOT IN (
                 SELECT source FROM source_config WHERE mode = 'disabled'
             )
             ORDER BY source",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub(crate) fn enqueue_sync_record(
        &self,
        record_id: &str,
        record_kind: &str,
        payload: &[u8],
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO sync_outbox (record_id, record_kind, payload)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![record_id, record_kind, payload],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn pending_sync_records(&self) -> Result<Vec<PendingSyncRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, payload FROM sync_outbox
             WHERE sent_at_utc IS NULL ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PendingSyncRecord {
                    outbox_id: row.get(0)?,
                    payload: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn pending_sync_count(&self) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_outbox WHERE sent_at_utc IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub(crate) fn mark_sync_records_sent(&self, outbox_ids: &[i64]) -> Result<()> {
        let sent_at = crate::utc::now_rfc3339();
        for id in outbox_ids {
            self.conn.execute(
                "UPDATE sync_outbox SET sent_at_utc = ?2 WHERE id = ?1",
                rusqlite::params![id, sent_at],
            )?;
        }
        Ok(())
    }

    pub(crate) fn sync_cursor(&self) -> Result<Option<String>> {
        let value = self.conn.query_row(
            "SELECT value FROM sync_cursors WHERE name = 'relay'",
            [],
            |row| row.get::<_, String>(0),
        );
        match value {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn set_sync_cursor(&self, cursor: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_cursors (name, value) VALUES ('relay', ?1)
             ON CONFLICT(name) DO UPDATE SET
                 value = excluded.value,
                 updated_at_utc = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            rusqlite::params![cursor],
        )?;
        Ok(())
    }

    pub(crate) fn sync_record_applied(&self, record_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_applied_records WHERE record_id = ?1)",
            rusqlite::params![record_id],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn mark_sync_record_applied(&self, record_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO sync_applied_records (record_id) VALUES (?1)",
            rusqlite::params![record_id],
        )?;
        Ok(())
    }
}

/// Applies pending migrations inside transactions, tracked via `user_version`.
/// Re-running on an up-to-date database is a no-op, making startup idempotent.
pub fn apply_migrations(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        conn.execute_batch(&format!(
            "BEGIN;
             {migration}
             PRAGMA user_version = {version};
             COMMIT;"
        ))?;
    }
    Ok(())
}

/// Current schema version (`user_version` after migrations).
pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}
