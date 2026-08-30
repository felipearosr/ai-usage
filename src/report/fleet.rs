//! Global machine health and source participation report.

use rusqlite::params;
use serde_json::json;

use crate::report::STALE_AFTER_SECS;
use crate::store::Store;
use crate::utc;

#[derive(Debug, PartialEq)]
pub struct FleetReport {
    pub generated_at_epoch: u64,
    pub machines: Vec<MachineRow>,
}

#[derive(Debug, PartialEq)]
pub struct MachineRow {
    pub device_id: String,
    pub name: String,
    pub os: String,
    pub last_sync_at_utc: Option<String>,
    pub revoked_at_utc: Option<String>,
    pub sources: Vec<String>,
}

impl MachineRow {
    pub fn last_sync_age_secs(&self, now_epoch: u64) -> Option<u64> {
        self.last_sync_at_utc
            .as_deref()
            .and_then(utc::parse_rfc3339_utc)
            .map(|synced| now_epoch.saturating_sub(synced))
    }

    pub fn is_stale(&self, now_epoch: u64) -> bool {
        self.last_sync_age_secs(now_epoch)
            .is_some_and(|age| age > STALE_AFTER_SECS)
    }
}

pub fn build(store: &Store, now_epoch: u64) -> crate::error::Result<FleetReport> {
    let mut stmt = store.conn().prepare(
        "SELECT device_id, friendly_name, os, last_sync_at_utc, revoked_at_utc
         FROM devices
         ORDER BY friendly_name, device_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut machines = Vec::with_capacity(rows.len());
    for (device_id, name, os, last_sync_at_utc, revoked_at_utc) in rows {
        let mut sources = store.conn().prepare(
            "SELECT DISTINCT source FROM (
                 SELECT source FROM usage_events WHERE device_id = ?1
                 UNION
                 SELECT source FROM quota_snapshots WHERE observing_device_id = ?1
             ) ORDER BY source",
        )?;
        let sources = sources
            .query_map(params![device_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        machines.push(MachineRow {
            device_id,
            name,
            os,
            last_sync_at_utc,
            revoked_at_utc,
            sources,
        });
    }

    Ok(FleetReport {
        generated_at_epoch: now_epoch,
        machines,
    })
}

pub fn render_text(report: &FleetReport) -> String {
    let mut out = String::from("MACHINES\n\n");
    if report.machines.is_empty() {
        out.push_str("No machines recorded. Run `aiu init` or `aiu join <code>`.\n");
        return out;
    }
    out.push_str(&format!(
        "{:<18} {:<10} {:<14} {:<24} {}\n",
        "NAME", "OS", "LAST SYNC", "TRACKED SOURCES", "STATUS"
    ));
    for machine in &report.machines {
        let last_sync = machine
            .last_sync_age_secs(report.generated_at_epoch)
            .map(|age| format!("{} ago", utc::humanize_duration_secs(age)))
            .unwrap_or_else(|| "never synced".to_string());
        let sources = if machine.sources.is_empty() {
            "no tracked sources".to_string()
        } else {
            machine.sources.join(", ")
        };
        let status = if machine.revoked_at_utc.is_some() {
            "REMOVED"
        } else if machine.is_stale(report.generated_at_epoch) {
            "STALE"
        } else if machine.last_sync_at_utc.is_none() {
            "NOT SYNCED"
        } else {
            "current"
        };
        out.push_str(&format!(
            "{:<18} {:<10} {:<14} {:<24} {}\n",
            machine.name, machine.os, last_sync, sources, status
        ));
    }
    out
}

pub fn render_json(report: &FleetReport) -> String {
    let machines = report
        .machines
        .iter()
        .map(|machine| {
            json!({
                "device_id": machine.device_id,
                "name": machine.name,
                "os": machine.os,
                "last_sync_at": machine.last_sync_at_utc,
                "revoked_at": machine.revoked_at_utc,
                "removed": machine.revoked_at_utc.is_some(),
                "last_sync_age_secs": machine.last_sync_age_secs(report.generated_at_epoch),
                "sources": machine.sources,
                "stale": machine.is_stale(report.generated_at_epoch),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&json!({
        "generated_at": utc::format_epoch(report.generated_at_epoch),
        "machines": machines,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}
