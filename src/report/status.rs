//! Local diagnostic report (spec story 30): "a status command covering
//! scheduler installation, last collection/sync, pending records, encryption
//! state, and relay reachability, so that I can diagnose problems locally."
//!
//! Everything the report cannot determine locally — whether a scheduler is
//! installed, whether the relay answers — is passed in by the caller, so this
//! module stays pure and the probes stay testable on their own.
//!
//! Encryption state is presence, never content: no key, credential, or any
//! part of one is rendered in either format.

use serde_json::json;

use crate::error::Result;
use crate::scheduler::{self, Drift, Platform};
use crate::store::Store;
use crate::utc;

#[derive(Debug, PartialEq)]
pub struct StatusReport {
    pub device_id: String,
    pub schedule: ScheduleStatus,
    pub last_collect_at_utc: Option<String>,
    pub last_sync_at_utc: Option<String>,
    pub pending_records: u64,
    pub encryption: EncryptionStatus,
    pub relay: RelayStatus,
    pub generated_at_epoch: u64,
}

/// What the OS scheduler is doing, as observed by the caller.
#[derive(Debug, PartialEq)]
pub enum ScheduleStatus {
    /// This OS has no scheduler aiu knows how to install.
    Unsupported,
    NotInstalled,
    /// Unit files exist but could not be read back. Distinct from
    /// `NotInstalled`: something *is* scheduled, and what it will do is
    /// exactly what cannot be determined — reporting it as absent would hide
    /// the case drift detection exists for.
    Unreadable {
        unit_paths: Vec<std::path::PathBuf>,
    },
    Installed {
        platform: Platform,
        interval_minutes: u64,
        activated: bool,
        unit_paths: Vec<std::path::PathBuf>,
        /// Ways the installed unit disagrees with this environment. `None`
        /// when the comparison could not be made at all, which must not be
        /// reported as "up to date".
        drift: Option<Vec<Drift>>,
    },
}

/// Presence of key material, never its content.
#[derive(Debug, PartialEq, Eq)]
pub struct EncryptionStatus {
    pub initialized: bool,
    pub workspace_key_present: bool,
    pub device_credential_present: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayStatus {
    /// The machine has not paired, so there is no relay to reach.
    NotConfigured,
    Reachable,
    /// The relay answered and refused this device. Distinct from an
    /// unreachable relay: the network is fine and the fix is different, and
    /// a revoked machine is among the likeliest to be running `aiu status`.
    Revoked,
    /// No request was made at all: the relay address or the local
    /// configuration could not be loaded. Distinct from `Unreachable`, which
    /// means a request went out and failed.
    NotAttempted(String),
    Unreachable(String),
}

/// The metadata key the collect pipeline stamps on every pass.
pub const LAST_COLLECT_KEY: &str = "last_collect_at_utc";

pub fn build(
    store: &Store,
    schedule: ScheduleStatus,
    relay: RelayStatus,
    now_epoch: u64,
) -> Result<StatusReport> {
    let device_id = store.get_metadata("device_id")?.unwrap_or_default();
    let last_sync_at_utc = match device_id.is_empty() {
        true => None,
        false => store.device_last_sync(&device_id)?,
    };

    Ok(StatusReport {
        device_id,
        schedule,
        last_collect_at_utc: store.get_metadata(LAST_COLLECT_KEY)?,
        last_sync_at_utc,
        pending_records: store.pending_sync_count()?,
        encryption: EncryptionStatus {
            initialized: store.get_metadata("setup_complete")?.as_deref() == Some("1"),
            workspace_key_present: store.get_metadata("workspace_key")?.is_some(),
            device_credential_present: store.get_metadata("device_credential")?.is_some(),
        },
        relay,
        generated_at_epoch: now_epoch,
    })
}

/// "5m ago", or "never" when it has not happened. Never a zero.
fn age(timestamp: Option<&str>, now_epoch: u64) -> String {
    let Some(secs) = crate::report::sync_age_secs(timestamp, now_epoch) else {
        return "never".to_string();
    };
    format!("{} ago", utc::humanize_duration_secs(secs))
}

pub fn render_text(report: &StatusReport) -> String {
    let mut out = String::from("AIU STATUS\n\n");
    let mut row = |label: &str, value: String| {
        out.push_str(&format!("  {label:<16} {value}\n"));
    };

    // A machine that has never collected has not minted an id yet; say so
    // rather than printing an empty column.
    row(
        "Machine",
        match report.device_id.is_empty() {
            true => "not yet assigned".to_string(),
            false => report.device_id.clone(),
        },
    );
    row("Scheduler", schedule_line(&report.schedule));
    if let ScheduleStatus::Installed {
        drift: Some(drift), ..
    } = &report.schedule
    {
        for item in drift {
            row("", format!("! {}", scheduler::describe_drift(item)));
        }
    }
    row(
        "Last collection",
        age(
            report.last_collect_at_utc.as_deref(),
            report.generated_at_epoch,
        ),
    );
    row(
        "Last sync",
        age(
            report.last_sync_at_utc.as_deref(),
            report.generated_at_epoch,
        ),
    );
    row("Pending records", report.pending_records.to_string());
    row("Encryption", encryption_line(&report.encryption));
    row("Relay", relay_line(&report.relay));
    out
}

/// The schedule as prose, including any drift, for callers that render it
/// outside the status table — `aiu schedule` shows the same facts, and a
/// second copy of this wording would drift from this one.
pub fn render_schedule(schedule: &ScheduleStatus) -> String {
    let mut out = schedule_line(schedule);
    out.push('\n');
    if let ScheduleStatus::Installed {
        drift: Some(drift), ..
    } = schedule
    {
        for item in drift {
            out.push_str(&format!("  ! {}\n", scheduler::describe_drift(item)));
        }
    }
    out
}

fn schedule_line(schedule: &ScheduleStatus) -> String {
    match schedule {
        ScheduleStatus::Unsupported => {
            "no supported scheduler on this OS; run `aiu collect` manually".to_string()
        }
        ScheduleStatus::NotInstalled => "not installed; run `aiu schedule install`".to_string(),
        ScheduleStatus::Unreadable { unit_paths } => format!(
            "installed but unreadable ({}); run `aiu schedule install` to rewrite it",
            unit_paths
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown location".to_string())
        ),
        ScheduleStatus::Installed {
            platform,
            interval_minutes,
            activated,
            drift,
            ..
        } => {
            let mut line = format!("{} every {interval_minutes}m", platform.as_str());
            if !activated {
                line.push_str(" (written but not activated)");
            }
            match drift {
                Some(drift) if !drift.is_empty() => {
                    line.push_str(" — stale, run `aiu schedule install` to repair")
                }
                Some(_) => {}
                None => line.push_str(" — cannot be compared against this environment"),
            }
            line
        }
    }
}

fn encryption_line(encryption: &EncryptionStatus) -> String {
    if !encryption.initialized {
        return "not initialized; run `aiu init`".to_string();
    }
    let mut parts = Vec::new();
    parts.push(if encryption.workspace_key_present {
        "workspace key present"
    } else {
        "workspace key MISSING"
    });
    parts.push(if encryption.device_credential_present {
        "device credential present"
    } else {
        "device credential MISSING"
    });
    parts.join(", ")
}

fn relay_line(relay: &RelayStatus) -> String {
    match relay {
        RelayStatus::NotConfigured => "not paired; nothing to reach".to_string(),
        RelayStatus::Reachable => "reachable".to_string(),
        RelayStatus::Revoked => {
            "this machine's access was revoked; re-pair with `aiu join`".to_string()
        }
        RelayStatus::NotAttempted(why) => {
            format!("not contacted ({})", crate::redact::url_userinfo(why))
        }
        RelayStatus::Unreachable(why) => {
            format!("unreachable ({})", crate::redact::url_userinfo(why))
        }
    }
}

pub fn render_json(report: &StatusReport) -> String {
    let schedule = match &report.schedule {
        ScheduleStatus::Unsupported => json!({ "state": "unsupported" }),
        ScheduleStatus::NotInstalled => json!({ "state": "not_installed" }),
        ScheduleStatus::Unreadable { unit_paths } => json!({
            "state": "unreadable",
            "unit_paths": unit_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        }),
        ScheduleStatus::Installed {
            platform,
            interval_minutes,
            activated,
            unit_paths,
            drift,
        } => json!({
            "state": "installed",
            "platform": platform.as_str(),
            "interval_minutes": interval_minutes,
            "activated": activated,
            "unit_paths": unit_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
            "drift": drift.as_ref().map(|drift| drift
                .iter()
                .map(scheduler::describe_drift)
                .collect::<Vec<_>>()),
        }),
    };
    let relay = match &report.relay {
        RelayStatus::NotConfigured => json!({ "state": "not_configured" }),
        RelayStatus::Reachable => json!({ "state": "reachable" }),
        RelayStatus::Revoked => json!({ "state": "revoked" }),
        RelayStatus::NotAttempted(why) => json!({
            "state": "not_attempted",
            "detail": crate::redact::url_userinfo(why),
        }),
        RelayStatus::Unreachable(why) => json!({
            "state": "unreachable",
            "detail": crate::redact::url_userinfo(why),
        }),
    };

    let value = json!({
        "device_id": report.device_id,
        "scheduler": schedule,
        "last_collect_at_utc": report.last_collect_at_utc,
        "last_sync_at_utc": report.last_sync_at_utc,
        "pending_records": report.pending_records,
        "encryption": {
            "initialized": report.encryption.initialized,
            "workspace_key_present": report.encryption.workspace_key_present,
            "device_credential_present": report.encryption.device_credential_present,
        },
        "relay": relay,
        "generated_at_utc": utc::format_epoch(report.generated_at_epoch),
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    )
}
