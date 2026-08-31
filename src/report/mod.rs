//! The report pipeline: query aggregation from the store, then rendering.
//!
//! This module is the production query/render path — the CLI and the tests
//! go through the exact same `build` + renderer functions. The report seam
//! feeds normalized events/snapshots into an in-memory store and asserts on
//! what comes out here.

pub mod breakdown;
pub mod detail;
pub mod fleet;
pub mod json;
pub mod status;
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
    pub top_machine_stale: bool,
}

#[derive(Debug, PartialEq)]
pub struct WindowQuota {
    pub window: String,
    pub used_percent: f64,
    pub resets_at_utc: Option<String>,
    pub observed_at_utc: String,
    pub observing_device_name: String,
    pub observer_last_sync_at_utc: Option<String>,
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

    pub fn observation_age_secs(&self, now_epoch: u64) -> Option<u64> {
        sync_age_secs(Some(&self.observed_at_utc), now_epoch)
    }

    pub fn is_stale(&self, now_epoch: u64) -> bool {
        self.observation_age_secs(now_epoch)
            .is_none_or(|age| age > STALE_AFTER_SECS)
            || is_stale_at(self.observer_last_sync_at_utc.as_deref(), now_epoch)
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
        is_stale_at(self.last_sync_at_utc.as_deref(), now_epoch)
    }

    pub fn age_secs(&self, now_epoch: u64) -> Option<u64> {
        sync_age_secs(self.last_sync_at_utc.as_deref(), now_epoch)
    }
}

pub(crate) fn sync_age_secs(last_sync_at_utc: Option<&str>, now_epoch: u64) -> Option<u64> {
    last_sync_at_utc
        .and_then(crate::utc::parse_rfc3339_utc)
        .map(|synced| now_epoch.saturating_sub(synced))
}

pub(crate) fn is_stale_at(last_sync_at_utc: Option<&str>, now_epoch: u64) -> bool {
    sync_age_secs(last_sync_at_utc, now_epoch).is_some_and(|age| age > STALE_AFTER_SECS)
}

pub(crate) fn humanize_sync_age(age_secs: u64) -> String {
    if age_secs < 60 {
        "now".to_string()
    } else {
        format!("{} ago", crate::utc::humanize_duration_secs(age_secs))
    }
}

pub(crate) fn machine_freshness_label(
    name: &str,
    last_sync_at_utc: Option<&str>,
    now_epoch: u64,
) -> String {
    match sync_age_secs(last_sync_at_utc, now_epoch) {
        Some(age) if age > STALE_AFTER_SECS => {
            format!("{name} STALE ({})", humanize_sync_age(age))
        }
        Some(age) => format!("{name} ({})", humanize_sync_age(age)),
        None => format!("{name} (never synced)"),
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
            reports.push(source_report(conn, &source, now_epoch)?);
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

fn source_report(
    conn: &rusqlite::Connection,
    source: &str,
    now_epoch: u64,
) -> crate::error::Result<SourceReport> {
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

    let (top_machine, top_machine_stale) = machine_attribution(
        conn,
        source,
        "SELECT d.friendly_name, SUM(e.output_tokens), d.last_sync_at_utc
         FROM usage_events e
         JOIN devices d ON d.device_id = e.device_id
         WHERE e.source = ?1
         GROUP BY e.device_id, d.friendly_name, d.last_sync_at_utc
         HAVING SUM(e.output_tokens) > 0
         ORDER BY SUM(e.output_tokens) DESC, d.friendly_name ASC
         LIMIT 1",
        now_epoch,
    )?;

    Ok(SourceReport {
        source: source.to_string(),
        windows,
        top_model,
        top_machine,
        top_machine_stale,
    })
}

fn machine_attribution(
    conn: &rusqlite::Connection,
    source: &str,
    query: &str,
    now_epoch: u64,
) -> crate::error::Result<(Option<Attribution>, bool)> {
    let mut stmt = conn.prepare(query)?;
    let mut rows = stmt.query(params![source])?;
    match rows.next()? {
        Some(row) => {
            let last_sync_at_utc = row.get::<_, Option<String>>(2)?;
            Ok((
                Some(Attribution {
                    name: row.get(0)?,
                    output_tokens: row.get(1)?,
                }),
                is_stale_at(last_sync_at_utc.as_deref(), now_epoch),
            ))
        }
        None => Ok((None, false)),
    }
}

/// True when at least one usage event exists for `source`. Shared by the
/// detail and breakdown builders so an empty window never renders as silence.
pub(crate) fn has_usage(conn: &rusqlite::Connection, source: &str) -> crate::error::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_events WHERE source = ?1)",
        params![source],
        |row| row.get(0),
    )
    .map_err(crate::error::AiuError::from)
}

/// Latest vendor observation per window for a source, ordered by window.
/// Deterministic: newest `observed_at`, highest row id breaking ties between
/// same-second snapshots.
pub(crate) fn latest_window_quotas(
    conn: &rusqlite::Connection,
    source: &str,
) -> crate::error::Result<Vec<WindowQuota>> {
    let mut stmt = conn.prepare(
        "SELECT q.window, q.used_percent, q.resets_at_utc, q.observed_at_utc,
                d.friendly_name, d.last_sync_at_utc
         FROM quota_snapshots q
         JOIN devices d ON d.device_id = q.observing_device_id
         WHERE q.source = ?1
           AND q.id = (
               SELECT q2.id FROM quota_snapshots q2
               WHERE q2.source = q.source AND q2.window = q.window
               ORDER BY q2.observed_at_utc DESC, q2.id DESC
               LIMIT 1
            )
          ORDER BY q.window",
    )?;
    let mut rows = stmt
        .query_map(params![source], |row| {
            Ok(WindowQuota {
                window: row.get(0)?,
                used_percent: row.get(1)?,
                resets_at_utc: row.get(2)?,
                observed_at_utc: row.get(3)?,
                observing_device_name: row.get(4)?,
                observer_last_sync_at_utc: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Canonical ordering (5h < week < month), not lexicographic: a source
    // with a monthly window (Go) must render week before month, and unknown
    // future windows sort last by name.
    rows.sort_by(|a, b| window_order(&a.window).cmp(&window_order(&b.window)));
    Ok(rows)
}

/// Canonical window ordering key. Known rolling windows follow the spec's
/// order (`5h` → `week` → `month`); anything else sorts after them, by name,
/// so sorting never misbehaves on an unexpected window string.
fn window_order(window: &str) -> (u8, &str) {
    match window {
        "5h" => (0, window),
        "week" => (1, window),
        "month" => (2, window),
        _ => (3, window),
    }
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
