//! Opportunistic retention pruning (spec: "365 days max across usage records,
//! snapshots, statistics, windows, sync/reporting metadata; older rows pruned
//! opportunistically during scheduled runs; no permanent aggregate archive").
//!
//! Pruning runs at the end of every collect pass rather than on a schedule of
//! its own — there is no daemon to run one. It is a plain set of deletes
//! inside a single transaction, so an interrupted run leaves the database
//! either fully pruned or untouched, and a re-run is a no-op.
//!
//! The cutoff is derived from a caller-supplied `now` so the boundary is
//! testable without waiting a year.

use crate::error::Result;
use crate::store::Store;

/// Maximum age of any retained row. The spec fixes this at one year; there is
/// no permanent aggregate archive behind it.
pub const RETENTION_DAYS: u64 = 365;

const SECONDS_PER_DAY: u64 = 86_400;

/// What one prune pass removed, per record type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneSummary {
    pub usage_events: u64,
    pub quota_snapshots: u64,
    pub sync_outbox: u64,
    pub sync_applied_records: u64,
}

impl PruneSummary {
    pub fn total(&self) -> u64 {
        self.usage_events + self.quota_snapshots + self.sync_outbox + self.sync_applied_records
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// The oldest timestamp still worth keeping, as the RFC 3339 UTC string the
/// schema stores. Rows strictly older than this are pruned.
pub fn cutoff(now_epoch: u64) -> String {
    let horizon = RETENTION_DAYS * SECONDS_PER_DAY;
    crate::utc::format_epoch(now_epoch.saturating_sub(horizon))
}

/// Deletes every row older than the retention horizon across usage events,
/// quota snapshots, and sync metadata.
///
/// Two tables are deliberately exempt. `sync_cursors` holds a single row
/// recording how far the relay download has progressed, and `adapter_state`
/// holds one row per adapter; neither grows with time, and pruning either by
/// age would be actively harmful — dropping the cursor forces a full
/// re-download of the workspace. The spec's "statistics" and "windows"
/// record types have no tables yet; when they arrive they belong here.
///
/// Outbox rows are pruned by age regardless of delivery state: a row older
/// than the horizon describes an event that is itself being pruned in the
/// same pass, so keeping it would queue data the workspace no longer retains.
/// Anything inside the window — delivered or still queued on an offline
/// machine — is left strictly alone.
pub fn prune(store: &Store, now_epoch: u64) -> Result<PruneSummary> {
    let cutoff = cutoff(now_epoch);
    let tx = store.transaction()?;

    let usage_events = tx.execute(
        "DELETE FROM usage_events WHERE ts_utc < ?1",
        rusqlite::params![cutoff],
    )? as u64;
    let quota_snapshots = tx.execute(
        "DELETE FROM quota_snapshots WHERE observed_at_utc < ?1",
        rusqlite::params![cutoff],
    )? as u64;
    let sync_outbox = tx.execute(
        "DELETE FROM sync_outbox WHERE created_at_utc < ?1",
        rusqlite::params![cutoff],
    )? as u64;
    let sync_applied_records = tx.execute(
        "DELETE FROM sync_applied_records WHERE applied_at_utc < ?1",
        rusqlite::params![cutoff],
    )? as u64;

    tx.commit()?;

    Ok(PruneSummary {
        usage_events,
        quota_snapshots,
        sync_outbox,
        sync_applied_records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_is_exactly_one_year_before_now() {
        let now = 1_717_200_000; // 2024-06-01T00:00:00Z
        assert_eq!(cutoff(now), crate::utc::format_epoch(now - 365 * 86_400));
    }

    #[test]
    fn cutoff_saturates_rather_than_underflowing_near_the_epoch() {
        assert_eq!(cutoff(0), crate::utc::format_epoch(0));
    }

    #[test]
    fn summary_totals_every_record_type() {
        let summary = PruneSummary {
            usage_events: 1,
            quota_snapshots: 2,
            sync_outbox: 3,
            sync_applied_records: 4,
        };
        assert_eq!(summary.total(), 10);
        assert!(!summary.is_empty());
        assert!(PruneSummary::default().is_empty());
    }
}
