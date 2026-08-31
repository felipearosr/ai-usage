//! Per-source detail view (`aiu claude`): vendor quota and aiu's locally
//! observed attribution, per window, kept clearly distinct.
//!
//! Breakdowns always filter to the exact window being shown (events within
//! that window's span ending at "now"); zero-use models and machines never
//! produce rows; non-participating machines are absent by construction.
//! Vendor numbers come from quota snapshots — state observations of the
//! shared account; attribution comes from normalized usage events. The two
//! are never blended.

pub mod json;
pub mod text;

use rusqlite::params;

use crate::report::has_usage;
use crate::report::latest_window_quotas;
use crate::store::Store;
use crate::utc;

/// Known window spans. Window sets are data-driven per source; this table
/// maps the window names vendors report to their rolling duration so
/// breakdowns can be filtered to exactly the window shown. Windows without
/// an entry render vendor data but no attribution span.
pub(crate) fn window_span_secs(window: &str) -> Option<u64> {
    match window {
        "5h" => Some(5 * 3600),
        "week" => Some(7 * 86_400),
        "month" => Some(30 * 86_400),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub struct SourceDetail {
    pub source: String,
    pub generated_at_epoch: u64,
    pub windows: Vec<WindowDetail>,
    /// True when usage events exist for this source even though no vendor
    /// window is known yet; renders as an explicit gap, never as silence.
    pub has_usage: bool,
}

#[derive(Debug, PartialEq)]
pub struct WindowDetail {
    pub window: String,
    /// Latest vendor observation for this window; None is rendered as an
    /// explicit gap, never as zero percent.
    pub vendor: Option<VendorQuota>,
    /// Machine shares of output tokens within this window only.
    pub machines: Vec<Share>,
    /// Exact-model shares of output tokens within this window only. Exact
    /// identifiers are never collapsed into families.
    pub models: Vec<Share>,
}

#[derive(Debug, PartialEq)]
pub struct VendorQuota {
    pub used_percent: f64,
    pub resets_at_utc: Option<String>,
    pub observed_at_utc: String,
    pub observing_device_name: String,
    pub observer_last_sync_at_utc: Option<String>,
}

impl VendorQuota {
    pub fn resets_in_secs(&self, now_epoch: u64) -> Option<u64> {
        self.resets_at_utc
            .as_deref()
            .and_then(utc::parse_rfc3339_utc)
            .filter(|resets| *resets > now_epoch)
            .map(|resets| resets - now_epoch)
    }

    pub fn observation_age_secs(&self, now_epoch: u64) -> Option<u64> {
        crate::report::sync_age_secs(Some(&self.observed_at_utc), now_epoch)
    }

    pub fn is_stale(&self, now_epoch: u64) -> bool {
        self.observation_age_secs(now_epoch)
            .is_none_or(|age| age > crate::report::STALE_AFTER_SECS)
            || crate::report::is_stale_at(self.observer_last_sync_at_utc.as_deref(), now_epoch)
    }
}

#[derive(Debug, PartialEq)]
pub struct Share {
    pub name: String,
    pub output_tokens: i64,
    pub share_percent: f64,
    pub stale: bool,
    pub last_sync_at_utc: Option<String>,
}

/// Builds the detail view through the same queries the CLI renders.
pub fn build(store: &Store, source: &str, now_epoch: u64) -> crate::error::Result<SourceDetail> {
    let conn = store.conn();
    let quotas = latest_window_quotas(conn, source)?;

    let mut windows = Vec::with_capacity(quotas.len());
    for quota in quotas {
        let span = window_span_secs(&quota.window);
        // Attribution only exists when we know how long the window is;
        // otherwise breakdown rows would not match the window shown.
        let cutoff = span.map(|s| utc::format_epoch(now_epoch.saturating_sub(s)));
        let machines = match &cutoff {
            Some(cutoff) => machine_shares(
                conn,
                "SELECT d.friendly_name, SUM(e.output_tokens), d.last_sync_at_utc
                 FROM usage_events e
                 JOIN devices d ON d.device_id = e.device_id
                 WHERE e.source = ?1 AND e.ts_utc >= ?2
                 GROUP BY e.device_id, d.friendly_name, d.last_sync_at_utc
                 HAVING SUM(e.output_tokens) > 0
                 ORDER BY SUM(e.output_tokens) DESC, d.friendly_name ASC",
                source,
                cutoff,
                now_epoch,
            )?,
            None => Vec::new(),
        };
        let models = match &cutoff {
            Some(cutoff) => shares(
                conn,
                "SELECT exact_model, SUM(output_tokens)
                 FROM usage_events
                 WHERE source = ?1 AND ts_utc >= ?2
                 GROUP BY exact_model
                 HAVING SUM(output_tokens) > 0
                 ORDER BY SUM(output_tokens) DESC, exact_model ASC",
                source,
                cutoff,
            )?,
            None => Vec::new(),
        };
        windows.push(WindowDetail {
            window: quota.window,
            vendor: Some(VendorQuota {
                used_percent: quota.used_percent,
                resets_at_utc: quota.resets_at_utc,
                observed_at_utc: quota.observed_at_utc,
                observing_device_name: quota.observing_device_name,
                observer_last_sync_at_utc: quota.observer_last_sync_at_utc,
            }),
            machines,
            models,
        });
    }

    let has_usage: bool = has_usage(conn, source)?;

    Ok(SourceDetail {
        source: source.to_string(),
        generated_at_epoch: now_epoch,
        windows,
        has_usage,
    })
}

fn shares(
    conn: &rusqlite::Connection,
    query: &str,
    source: &str,
    cutoff_utc: &str,
) -> crate::error::Result<Vec<Share>> {
    let mut stmt = conn.prepare(query)?;
    let rows = stmt
        .query_map(params![source, cutoff_utc], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let total: i64 = rows.iter().map(|(_, t)| *t).sum();
    Ok(rows
        .into_iter()
        .map(|(name, tokens)| Share {
            share_percent: share_percent(tokens, total),
            name,
            output_tokens: tokens,
            stale: false,
            last_sync_at_utc: None,
        })
        .collect())
}

fn machine_shares(
    conn: &rusqlite::Connection,
    query: &str,
    source: &str,
    cutoff_utc: &str,
    now_epoch: u64,
) -> crate::error::Result<Vec<Share>> {
    let mut stmt = conn.prepare(query)?;
    let rows = stmt
        .query_map(params![source, cutoff_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let total: i64 = rows.iter().map(|(_, tokens, _)| *tokens).sum();
    Ok(rows
        .into_iter()
        .map(|(name, tokens, last_sync)| Share {
            stale: crate::report::is_stale_at(last_sync.as_deref(), now_epoch),
            last_sync_at_utc: last_sync,
            share_percent: share_percent(tokens, total),
            name,
            output_tokens: tokens,
        })
        .collect())
}

pub(crate) fn share_percent(tokens: i64, total: i64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    ((tokens as f64 / total as f64) * 100.0 * 10.0).round() / 10.0
}
