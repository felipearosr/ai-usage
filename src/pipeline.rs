//! The `aiu collect` pipeline (spec: "collect deltas → snapshot quotas →
//! persist → enqueue outbox → sync if reachable → opportunistic retention
//! prune → exit").
//!
//! One synchronous call performs every stage and returns. Nothing is spawned,
//! nothing is left scheduled, and there is no resident state — the OS
//! scheduler in [`crate::scheduler`] is what makes it periodic, so idle CPU
//! and memory between runs are zero.
//!
//! Reachability is not a precondition. A machine with no relay (never paired)
//! and a machine whose relay is unreachable (offline) both collect, persist,
//! queue, and prune exactly as usual; the queued outbox is what the next
//! reachable run delivers.

use std::path::Path;

use crate::adapters::IngestContext;
use crate::collect::{collect_detected, SourceCollect};
use crate::error::Result;
use crate::retention::{self, PruneSummary};
use crate::store::Store;
use crate::sync::{RelayClient, SyncConfig, SyncSummary};

/// What one collect pass did, stage by stage.
#[derive(Debug, Default)]
pub struct CollectRun {
    /// Per-source collection results. A source that failed is counted here
    /// rather than aborting the pass.
    pub sources: Vec<SourceCollect>,
    /// The sync result, or `None` when there was no relay to reach.
    pub sync: Option<SyncSummary>,
    /// Why syncing did not happen, when a relay was configured but could not
    /// be reached. Reported rather than swallowed; never fatal.
    pub sync_error: Option<String>,
    pub pruned: PruneSummary,
    /// Records still queued for delivery when the pass exited.
    pub pending_records: u64,
}

impl CollectRun {
    pub fn events_imported(&self) -> u64 {
        self.sources.iter().map(|s| s.events_imported).sum()
    }

    pub fn snapshots_stored(&self) -> u64 {
        self.sources.iter().map(|s| s.snapshots_stored).sum()
    }

    pub fn files_failed(&self) -> u64 {
        self.sources.iter().map(|s| s.files_failed).sum()
    }

    /// Whether the pass reached the relay. False both when offline and when
    /// the machine has never paired.
    pub fn synced(&self) -> bool {
        self.sync.is_some()
    }
}

/// Runs the full pipeline once and returns.
///
/// `relay` and `config` are both `None` on a machine that has not paired.
/// Only database failures propagate: a broken source is contained by
/// [`collect_detected`], and an unreachable relay is recorded in
/// [`CollectRun::sync_error`] so the pass can still persist and prune.
pub fn run(
    store: &Store,
    home: &Path,
    ctx: &IngestContext,
    relay: Option<&mut dyn RelayClient>,
    config: Option<&SyncConfig>,
    now_epoch: u64,
) -> Result<CollectRun> {
    // Collect deltas and snapshot quotas. Persisting an event also enqueues
    // it, so the outbox is current by the time syncing starts.
    let sources = collect_detected(store, home, ctx)?;

    let mut sync = None;
    let mut sync_error = None;
    if let (Some(relay), Some(config)) = (relay, config) {
        match crate::sync::sync_once(store, relay, config) {
            Ok(summary) => sync = Some(summary),
            Err(error) => sync_error = Some(error.to_string()),
        }
    }

    // Opportunistic prune: this is the only moment retention runs, since
    // there is no daemon to run it on its own schedule.
    let pruned = retention::prune(store, now_epoch)?;

    // Stamped after the work, so it records a pass that actually completed —
    // this is what `aiu status` reports as the last collection.
    store.set_metadata(
        crate::report::status::LAST_COLLECT_KEY,
        &crate::utc::format_epoch(now_epoch),
    )?;

    Ok(CollectRun {
        sources,
        sync,
        sync_error,
        pruned,
        pending_records: store.pending_sync_count()?,
    })
}

/// Human-readable one-run summary for `aiu collect`.
pub fn render(run: &CollectRun) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Collected {} new event(s) and {} quota snapshot(s) across {} source(s).\n",
        run.events_imported(),
        run.snapshots_stored(),
        run.sources.len()
    ));
    if run.files_failed() > 0 {
        out.push_str(&format!(
            "{} file(s) could not be read or recognized; see `aiu sources`.\n",
            run.files_failed()
        ));
    }
    match (&run.sync, &run.sync_error) {
        (Some(summary), _) => out.push_str(&format!(
            "Synced: {} uploaded, {} downloaded, {} duplicate(s) ignored.\n",
            summary.uploaded, summary.downloaded, summary.duplicates_ignored
        )),
        (None, Some(error)) => out.push_str(&format!(
            "Relay unreachable ({error}); {} record(s) queued for the next run.\n",
            run.pending_records
        )),
        (None, None) => out.push_str("Not paired; collected locally only.\n"),
    }
    if !run.pruned.is_empty() {
        out.push_str(&format!(
            "Pruned {} record(s) older than {} days.\n",
            run.pruned.total(),
            retention::RETENTION_DAYS
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with(sync: Option<SyncSummary>, sync_error: Option<String>) -> CollectRun {
        CollectRun {
            sources: vec![SourceCollect {
                source: "claude",
                events_imported: 3,
                snapshots_stored: 1,
                ..SourceCollect::default()
            }],
            sync,
            sync_error,
            ..CollectRun::default()
        }
    }

    #[test]
    fn totals_add_up_across_sources() {
        let run = run_with(None, None);
        assert_eq!(run.events_imported(), 3);
        assert_eq!(run.snapshots_stored(), 1);
        assert!(!run.synced());
    }

    #[test]
    fn an_unpaired_run_says_so_rather_than_reporting_a_failure() {
        let text = render(&run_with(None, None));
        assert!(text.contains("Not paired"), "{text}");
    }

    #[test]
    fn an_offline_run_reports_the_queue_depth() {
        let mut run = run_with(None, Some("relay unavailable".into()));
        run.pending_records = 4;
        let text = render(&run);
        assert!(text.contains("Relay unreachable"), "{text}");
        assert!(text.contains("4 record(s) queued"), "{text}");
    }

    #[test]
    fn a_synced_run_reports_relay_totals() {
        let text = render(&run_with(
            Some(SyncSummary {
                uploaded: 2,
                downloaded: 1,
                duplicates_ignored: 0,
            }),
            None,
        ));
        assert!(text.contains("2 uploaded, 1 downloaded"), "{text}");
    }

    #[test]
    fn a_prune_is_only_mentioned_when_it_removed_something() {
        let quiet = render(&run_with(None, None));
        assert!(!quiet.contains("Pruned"), "{quiet}");

        let mut run = run_with(None, None);
        run.pruned = PruneSummary {
            usage_events: 5,
            ..PruneSummary::default()
        };
        assert!(render(&run).contains("Pruned 5 record(s) older than 365 days"));
    }
}
