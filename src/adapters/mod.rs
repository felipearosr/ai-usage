//! The adapter seam: the trait every source adapter implements.
//!
//! Adapters turn raw local provider records into normalized events and
//! quota snapshots, pushing each item into an `EventSink` as it goes so
//! memory stays bounded regardless of history size. Fixtures recorded from
//! real tool output drive every test through this seam (spec, testing
//! decision 2).

pub mod claude;

use std::io::BufRead;

use crate::error::{AiuError, Result};
use crate::store::{NewEvent, NewSnapshot};

/// Everything an adapter needs from its environment. Injected so tests are
/// deterministic and adapters never touch the clock or the database.
pub struct IngestContext {
    pub device_id: String,
    pub workspace_id: String,
    pub now_epoch: u64,
}

/// Destination for normalized adapter output. Implementations decide how
/// items are stored; the import machinery backs one with transactions and
/// dedup counters, tests back one with plain vectors.
pub trait EventSink {
    /// Returns true when the event was newly stored, false when it was a
    /// duplicate of an already-known identity.
    fn accept_event(&mut self, event: NewEvent) -> Result<bool>;

    /// Returns true when the snapshot was newly stored, false when it was
    /// identical to the latest known observation for that window.
    fn accept_snapshot(&mut self, snapshot: NewSnapshot) -> Result<bool>;
}

/// What happened while parsing raw input. Malformed and unrecognized-format
/// discipline lives here: malformed individual records are skipped and
/// counted; a wholly unrecognizable stream is a loud error instead.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParseSummary {
    /// Non-empty lines/records examined at all.
    pub records_seen: u64,
    pub events_emitted: u64,
    /// Repeated record identities ignored within the stream (identical
    /// re-writes, e.g. after restart/resume).
    pub duplicates_skipped: u64,
    /// Intermediate streaming entries folded into their final record
    /// (cumulative counters replaced by the last observation).
    pub streamed_snapshots_collapsed: u64,
    /// Records that could not be parsed or lacked required fields.
    pub malformed_skipped: u64,
    pub snapshots_emitted: u64,
}

/// Receives progress ticks while an adapter streams its input. The argument
/// is the number of records examined so far. Import wires this to stderr;
/// tests collect or ignore it. Adapters must call it only periodically.
pub type ProgressFn<'a> = dyn FnMut(u64) + 'a;

/// A no-op progress reporter for callers that do not display progress.
pub fn silent_progress() -> impl FnMut(u64) {
    |_| {}
}

pub trait SourceAdapter {
    /// Accounting-domain identifier ("claude", "codex", "go").
    fn source(&self) -> &'static str;

    /// Adapter implementation version, stamped onto every emitted event.
    fn version(&self) -> &'static str;

    /// Streams usage history from `input`, emitting normalized events into
    /// `sink`. Must read incrementally: no full-buffering of the input.
    fn ingest(
        &self,
        input: &mut dyn BufRead,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        progress: &mut ProgressFn<'_>,
    ) -> Result<ParseSummary>;

    /// Streams a vendor quota capture (state observations, never events)
    /// into `sink`. Sources without a quota capture format fail loudly.
    fn ingest_quota(
        &self,
        _input: &mut dyn BufRead,
        _ctx: &IngestContext,
        _sink: &mut dyn EventSink,
        _progress: &mut ProgressFn<'_>,
    ) -> Result<ParseSummary> {
        Err(AiuError::UnrecognizedFormat {
            source: self.source(),
            detail: "this source has no supported quota capture format".to_string(),
        })
    }
}
