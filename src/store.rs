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

pub struct NewDevice {
    pub device_id: String,
    pub friendly_name: String,
    pub os: String,
    pub arch: String,
    pub last_sync_at_utc: Option<String>,
}

#[derive(Default)]
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

pub struct NewSnapshot {
    pub source: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at_utc: Option<String>,
    pub observed_at_utc: String,
    pub observing_device_id: String,
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

    pub fn touch_device_sync(&self, device_id: &str, ts_utc: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET last_sync_at_utc = ?2 WHERE device_id = ?1",
            rusqlite::params![device_id, ts_utc],
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
        Ok(())
    }

    /// Records a quota snapshot unless it is byte-identical to the latest
    /// observation for the same source/window. Re-running collection or
    /// import therefore does not grow snapshot history with no-op rows.
    /// Returns true when a new observation was stored.
    pub fn record_snapshot_if_changed(&self, s: &NewSnapshot) -> Result<bool> {
        let latest = self.conn.query_row(
            "SELECT used_percent, resets_at_utc FROM quota_snapshots
             WHERE source = ?1 AND window = ?2
             ORDER BY observed_at_utc DESC, id DESC LIMIT 1",
            rusqlite::params![s.source, s.window],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, Option<String>>(1)?)),
        );
        let changed = match latest {
            Ok((percent, resets)) => !(percent == s.used_percent && resets == s.resets_at_utc),
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
