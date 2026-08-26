//! The report pipeline: query aggregation from the store, then rendering.
//!
//! This module is the production query/render path — the CLI and the tests
//! go through the exact same `build` + renderer functions. The report seam
//! feeds normalized events/snapshots into an in-memory store and asserts on
//! what comes out here.

pub mod detail;
pub mod json;
pub mod text;

use rusqlite::params;

use crate::store::Store;

/// Machines silent longer than this render as STALE (spec: story 21).
pub const STALE_AFTER_SECS: u64 = 30 * 60;

#[derive(Debug, PartialEq)]
pub struct Report {
    pub generated_at_epoch: u64,
    pub sources: Vec<SourceReport>,
    pub devices: Vec<DeviceFreshness>,
}

#[derive(Debug, PartialEq)]
pub struct SourceReport {
    pub source: String,
    /// Latest vendor quota observation per window, ordered by window name.
    pub windows: Vec<WindowQuota>,
    /// Top exact model by output tokens; zero-usage models never appear.
    pub top_model: Option<Attribution>,
    /// Top participating machine by output tokens for this source only.
    pub top_machine: Option<Attribution>,
}

#[derive(Debug, PartialEq)]
pub struct WindowQuota {
    pub window: String,
    pub used_percent: f64,
    pub resets_at_utc: Option<String>,
}

impl WindowQuota {
    /// Seconds until reset, when the reset time is still in the future.
    /// Shared by both renderers so text and JSON always agree.
    pub fn resets_in_secs(&self, now_epoch: u64) -> Option<u64> {
        self.resets_at_utc
            .as_deref()
            .and_then(crate::utc::parse_rfc3339_utc)
            .filter(|resets| *resets > now_epoch)
            .map(|resets| resets - now_epoch)
    }
}

#[derive(Debug, PartialEq)]
pub struct Attribution {
    pub name: String,
    pub output_tokens: i64,
}

#[derive(Debug, PartialEq)]
pub struct DeviceFreshness {
    pub name: String,
    pub last_sync_at_utc: Option<String>,
}

impl DeviceFreshness {
    pub fn is_stale(&self, now_epoch: u64) -> bool {
        match self
            .last_sync_at_utc
            .as_deref()
            .and_then(crate::utc::parse_rfc3339_utc)
        {
            Some(synced) => now_epoch.saturating_sub(synced) > STALE_AFTER_SECS,
            // A device that has never synced is not "silent past 30 minutes";
            // it is simply unsynced, and renders as such (never STALE).
            None => false,
        }
    }

    pub fn age_secs(&self, now_epoch: u64) -> Option<u64> {
        self.last_sync_at_utc
            .as_deref()
            .and_then(crate::utc::parse_rfc3339_utc)
            .map(|synced| now_epoch.saturating_sub(synced))
    }
}

/// Builds the report via the same queries the CLI renders. `now_epoch` is
/// injected so staleness/reset math is deterministic in tests.
pub fn build(store: &Store, now_epoch: u64) -> crate::error::Result<Report> {
    let conn = store.conn();

    let sources = {
        let names = store.configured_sources()?;
        let mut reports = Vec::new();
        for source in names {
            reports.push(source_report(conn, &source)?);
        }
        reports
    };

    let devices = {
        let mut stmt = conn.prepare(
            "SELECT friendly_name, last_sync_at_utc FROM devices ORDER BY friendly_name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DeviceFreshness {
                    name: row.get(0)?,
                    last_sync_at_utc: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    Ok(Report {
        generated_at_epoch: now_epoch,
        sources,
        devices,
    })
}

fn source_report(conn: &rusqlite::Connection, source: &str) -> crate::error::Result<SourceReport> {
    let windows = latest_window_quotas(conn, source)?;

    let top_model = attribution(
        conn,
        source,
        // Ranked by output tokens: summing every token class would repeat the
        // blind-sum normalization the spec forbids; output tokens are the
        // documented ranking metric until adapters bring source-specific
        // normalization (metric hierarchy: observed usage, never fabricated).
        "SELECT exact_model, SUM(output_tokens)
         FROM usage_events
         WHERE source = ?1
         GROUP BY exact_model
         HAVING SUM(output_tokens) > 0
         ORDER BY SUM(output_tokens) DESC, exact_model ASC
         LIMIT 1",
    )?;

    let top_machine = attribution(
        conn,
        source,
        "SELECT d.friendly_name, SUM(e.output_tokens)
         FROM usage_events e
         JOIN devices d ON d.device_id = e.device_id
         WHERE e.source = ?1
         GROUP BY e.device_id, d.friendly_name
         HAVING SUM(e.output_tokens) > 0
         ORDER BY SUM(e.output_tokens) DESC, d.friendly_name ASC
         LIMIT 1",
    )?;

    Ok(SourceReport {
        source: source.to_string(),
        windows,
        top_model,
        top_machine,
    })
}

/// Latest vendor observation per window for a source, ordered by window.
/// Deterministic: newest `observed_at`, highest row id breaking ties between
/// same-second snapshots.
pub(crate) fn latest_window_quotas(
    conn: &rusqlite::Connection,
    source: &str,
) -> crate::error::Result<Vec<WindowQuota>> {
    let mut stmt = conn.prepare(
        "SELECT q.window, q.used_percent, q.resets_at_utc
         FROM quota_snapshots q
         WHERE q.source = ?1
           AND q.id = (
               SELECT q2.id FROM quota_snapshots q2
               WHERE q2.source = q.source AND q2.window = q.window
               ORDER BY q2.observed_at_utc DESC, q2.id DESC
               LIMIT 1
            )
          ORDER BY q.window",
    )?;
    let rows = stmt
        .query_map(params![source], |row| {
            Ok(WindowQuota {
                window: row.get(0)?,
                used_percent: row.get(1)?,
                resets_at_utc: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn attribution(
    conn: &rusqlite::Connection,
    source: &str,
    query: &str,
) -> crate::error::Result<Option<Attribution>> {
    let mut stmt = conn.prepare(query)?;
    let mut rows = stmt.query(params![source])?;
    match rows.next()? {
        Some(row) => Ok(Some(Attribution {
            name: row.get(0)?,
            output_tokens: row.get(1)?,
        })),
        None => Ok(None),
    }
}
