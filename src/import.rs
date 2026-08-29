//! Historical import: streams local session histories into the store with
//! bounded memory, progress output, periodic commits, and restart-safe
//! idempotency.
//!
//! Idempotency rests on deterministic event identities: adapters build them
//! from stable record components, the store ignores repeats via
//! `INSERT OR IGNORE`, and snapshots only land when they differ from the
//! latest observation. Re-running an import therefore never double-counts.

use std::io::BufRead;

use crate::adapters::{EventSink, IngestContext, ProgressFn, SourceAdapter};
use crate::error::{AiuError, Result};
use crate::store::{NewDevice, Store};

#[derive(Clone, Copy)]
pub struct ImportOptions {
    /// Commit the open transaction after this many accepted items, so long
    /// imports persist progress incrementally instead of one giant commit.
    pub commit_every: usize,
    /// Minimum records between two progress callbacks.
    pub progress_every: u64,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            commit_every: 500,
            progress_every: 2_000,
        }
    }
}

/// What one import pass did. Malformed/duplicate counts are surfaced so
/// callers can print them; nothing is hidden silently.
#[derive(Debug, Default, PartialEq)]
pub struct ImportSummary {
    pub records_seen: u64,
    pub events_imported: u64,
    pub duplicates_ignored: u64,
    pub streamed_collapsed: u64,
    pub malformed_skipped: u64,
    pub snapshots_stored: u64,
}

struct StoreSink<'a> {
    store: &'a Store,
    tx: Option<rusqlite::Transaction<'a>>,
    commit_every: usize,
    ops_since_commit: usize,
    events_new: u64,
    duplicates: u64,
    snapshots_new: u64,
}

impl<'a> StoreSink<'a> {
    fn new(store: &'a Store, commit_every: usize) -> Result<Self> {
        Ok(StoreSink {
            store,
            tx: Some(store.transaction()?),
            commit_every: commit_every.max(1),
            ops_since_commit: 0,
            events_new: 0,
            duplicates: 0,
            snapshots_new: 0,
        })
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit()?;
        }
        Ok(())
    }
}

impl EventSink for StoreSink<'_> {
    fn accept_event(&mut self, event: crate::store::NewEvent) -> Result<bool> {
        let sync_record = crate::sync::SyncRecord::UsageEvent(Box::new(event.clone()));
        let stored = self.store.record_event(&event)?;
        if stored {
            crate::sync::enqueue_record(self.store, &sync_record)?;
            self.events_new += 1;
        } else {
            self.duplicates += 1;
        }
        self.ops_since_commit += 1;
        if self.ops_since_commit >= self.commit_every {
            // Periodic commit: persisted progress survives interruption;
            // deterministic ids make the re-run of the remainder idempotent.
            if let Some(tx) = self.tx.take() {
                tx.commit()?;
            }
            self.tx = Some(self.store.transaction()?);
            self.ops_since_commit = 0;
        }
        Ok(stored)
    }

    fn accept_snapshot(&mut self, snapshot: crate::store::NewSnapshot) -> Result<bool> {
        let sync_record = crate::sync::SyncRecord::QuotaSnapshot(Box::new(snapshot.clone()));
        let stored = self.store.record_snapshot_if_changed(&snapshot)?;
        if stored {
            crate::sync::enqueue_record(self.store, &sync_record)?;
            self.snapshots_new += 1;
        }
        Ok(stored)
    }
}

fn ensure_local_device(store: &Store, ctx: &IngestContext) -> Result<()> {
    store.ensure_device(&NewDevice {
        device_id: ctx.device_id.clone(),
        friendly_name: ctx.device_id.clone(),
        os: String::new(),
        arch: String::new(),
        last_sync_at_utc: None,
    })?;
    Ok(())
}

/// Streams a usage history through `adapter` into the store. Fails loudly on
/// unrecognized upstream formats after recording a durable diagnostic; other
/// sources are imported independently and stay unaffected.
pub fn import_usage(
    store: &Store,
    adapter: &dyn SourceAdapter,
    input: &mut dyn BufRead,
    ctx: &IngestContext,
    opts: ImportOptions,
    progress: &mut dyn FnMut(u64),
) -> Result<ImportSummary> {
    ensure_local_device(store, ctx)?;
    run_import(
        store,
        adapter,
        |adapter, ctx, sink, reporter| adapter.ingest(input, ctx, sink, reporter),
        ctx,
        opts,
        progress,
    )
}

/// Streams a vendor quota capture through `adapter` into the store.
pub fn import_quota(
    store: &Store,
    adapter: &dyn SourceAdapter,
    input: &mut dyn BufRead,
    ctx: &IngestContext,
    opts: ImportOptions,
    progress: &mut dyn FnMut(u64),
) -> Result<ImportSummary> {
    ensure_local_device(store, ctx)?;
    run_import(
        store,
        adapter,
        |adapter, ctx, sink, reporter| adapter.ingest_quota(input, ctx, sink, reporter),
        ctx,
        opts,
        progress,
    )
}

fn run_import(
    store: &Store,
    adapter: &dyn SourceAdapter,
    ingest: impl FnOnce(
        &dyn SourceAdapter,
        &IngestContext,
        &mut dyn EventSink,
        &mut ProgressFn<'_>,
    ) -> Result<crate::adapters::ParseSummary>,
    ctx: &IngestContext,
    opts: ImportOptions,
    progress: &mut dyn FnMut(u64),
) -> Result<ImportSummary> {
    let mut sink = StoreSink::new(store, opts.commit_every)?;
    let mut last_reported = 0u64;
    let mut reporter = |seen: u64| {
        if seen.saturating_sub(last_reported) >= opts.progress_every.max(1) {
            last_reported = seen;
            progress(seen);
        }
    };

    let result = ingest(adapter, ctx, &mut sink, &mut reporter);

    match result {
        Ok(summary) => {
            sink.finish()?;
            // The adapter reports in-stream duplicates; the sink reports
            // storage-level repeats (already-known deterministic ids).
            Ok(ImportSummary {
                records_seen: summary.records_seen,
                events_imported: sink.events_new,
                duplicates_ignored: summary.duplicates_skipped + sink.duplicates,
                streamed_collapsed: summary.streamed_snapshots_collapsed,
                malformed_skipped: summary.malformed_skipped,
                snapshots_stored: sink.snapshots_new.max(summary.snapshots_emitted),
            })
        }
        Err(e) => {
            // Roll back any uncommitted tail, then make the failure durable:
            // diagnostics survive the process for later inspection.
            sink.tx.take();
            if matches!(e, AiuError::UnrecognizedFormat { .. }) {
                store.record_diagnostic(
                    adapter.source(),
                    &e.to_string(),
                    &crate::utc::now_rfc3339(),
                )?;
            }
            Err(e)
        }
    }
}
