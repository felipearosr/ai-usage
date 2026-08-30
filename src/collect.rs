//! Quick local collection: the report path reads new local records before
//! rendering (spec: "any normal report command performs a quick local refresh
//! before rendering").
//!
//! This is the delta-collect half of `aiu collect` (issue 11) — it streams
//! discovered local files through the source adapters into the store. Sync,
//! scheduler, and retention stay out of scope here. Failure is contained per
//! file and per source: one unreadable or unrecognized file never prevents
//! the rest of the collection, or the report, from proceeding. Idempotency is
//! inherited from the import machinery's deterministic event identities.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::adapters::{IngestContext, SourceAdapter};
use crate::error::{AiuError, Result};
use crate::import::{import_usage, ImportOptions, ImportSummary};
use crate::store::Store;

/// What collecting one source did. Failure is counted, never fatal.
#[derive(Debug, Default)]
pub struct SourceCollect {
    pub source: &'static str,
    pub files_attempted: u64,
    pub files_failed: u64,
    pub events_imported: u64,
    pub duplicates_ignored: u64,
    pub malformed_skipped: u64,
    pub streamed_collapsed: u64,
    pub snapshots_stored: u64,
}

impl SourceCollect {
    fn accumulate(&mut self, summary: ImportSummary) {
        self.events_imported += summary.events_imported;
        self.duplicates_ignored += summary.duplicates_ignored;
        self.malformed_skipped += summary.malformed_skipped;
        self.streamed_collapsed += summary.streamed_collapsed;
        self.snapshots_stored += summary.snapshots_stored;
    }
}

/// Streams one already-opened source stream through the adapter. A source
/// whose format aiu does not recognize (or an I/O failure mid-read) is
/// contained — reported as `Ok(None)` after the import machinery records the
/// diagnostic — rather than aborting the whole report. Database failures are
/// the only errors that propagate.
pub fn collect_reader(
    store: &Store,
    adapter: &dyn SourceAdapter,
    reader: &mut dyn BufRead,
    ctx: &IngestContext,
    opts: ImportOptions,
) -> Result<Option<ImportSummary>> {
    match import_usage(store, adapter, reader, ctx, opts, &mut |_| {}) {
        Ok(summary) => Ok(Some(summary)),
        Err(AiuError::UnrecognizedFormat { .. }) | Err(AiuError::Io(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Collects every file of one source, skipping (and counting) files that
/// cannot be read or recognized. Returns a summary, never a source-level
/// error.
pub fn collect_source(
    store: &Store,
    adapter: &dyn SourceAdapter,
    files: &[PathBuf],
    ctx: &IngestContext,
    opts: ImportOptions,
) -> Result<SourceCollect> {
    let mut result = SourceCollect {
        source: adapter.source(),
        ..SourceCollect::default()
    };
    for path in files {
        result.files_attempted += 1;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => {
                result.files_failed += 1;
                continue;
            }
        };
        let mut reader = std::io::BufReader::new(file);
        match collect_reader(store, adapter, &mut reader, ctx, opts)? {
            Some(summary) => result.accumulate(summary),
            None => result.files_failed += 1,
        }
    }
    Ok(result)
}

/// Maps a source identifier to its adapter, so collection can drive whatever
/// discovery found. Sources without an adapter yet (e.g. `go`, issue 04) are
/// simply skipped.
pub fn adapter_for(source: &str) -> Option<&'static dyn SourceAdapter> {
    match source {
        "claude" => Some(&crate::adapters::claude::ClaudeCodeAdapter),
        "codex" => Some(&crate::adapters::codex::CodexAdapter),
        _ => None,
    }
}

/// The periodic cheap collection pass: detect which sources are present, apply
/// per-source overrides, and drive each source that should be tracked through
/// its adapter. Detection is a sentinel `is_dir` check (no history scan); the
/// full recursive file listing happens only for sources that survive the
/// override/detection gate. `enabled` overrides the sentinel and always
/// attempts the standard on-disk locations; `auto` follows detection, so a
/// source that appears after setup is picked up on the next pass with no
/// re-init.
pub fn collect_detected(
    store: &Store,
    home: &Path,
    ctx: &IngestContext,
) -> Result<Vec<SourceCollect>> {
    collect_detected_with_progress(store, home, ctx, &mut |_| {})
}

pub fn collect_detected_with_progress(
    store: &Store,
    home: &Path,
    ctx: &IngestContext,
    progress: &mut dyn FnMut(&SourceCollect),
) -> Result<Vec<SourceCollect>> {
    let mut results = Vec::new();
    for detection in crate::sources::detect(home) {
        let mode = store.source_mode(detection.source)?;
        if !crate::sources::should_collect(mode, detection.detected) {
            continue;
        }
        let Some(adapter) = adapter_for(detection.source) else {
            continue;
        };
        let files = crate::discover::files_for(home, detection.source);
        if files.is_empty() {
            continue;
        }
        let result = collect_source(store, adapter, &files, ctx, ImportOptions::default())?;
        progress(&result);
        results.push(result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::claude::ClaudeCodeAdapter;
    use crate::adapters::codex::CodexAdapter;
    use crate::import::ImportOptions;
    use crate::store::Store;

    const NOW: u64 = 1_700_913_600;

    fn ctx() -> IngestContext {
        IngestContext {
            device_id: "dev-test".to_string(),
            workspace_id: "ws-test".to_string(),
            now_epoch: NOW,
        }
    }

    fn opts() -> ImportOptions {
        ImportOptions::default()
    }

    fn claude_line(id: &str, output: i64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2023-11-25T11:00:00.000Z\",\
              \"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5\",\
              \"usage\":{{\"input_tokens\":1,\"output_tokens\":{output}}}}}}}"
        )
    }

    #[test]
    fn unrecognized_file_is_contained_and_counted_not_fatal() {
        let store = Store::open_in_memory().unwrap();
        let mut garbage = "{\"not\":\"a transcript\"}".as_bytes();
        let result =
            collect_reader(&store, &ClaudeCodeAdapter, &mut garbage, &ctx(), opts()).unwrap();
        assert!(result.is_none(), "unrecognized format is contained");
        // The import machinery recorded a durable diagnostic for this source.
        assert!(store.diagnostic_for("claude").unwrap().is_some());
    }

    #[test]
    fn collect_source_skips_bad_files_and_keeps_the_good() {
        let store = Store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("aiu-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.jsonl");
        std::fs::write(
            &good,
            format!("{}\n{}\n", claude_line("a", 10), claude_line("b", 20)),
        )
        .unwrap();
        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "{\"hello\": 1}\n").unwrap();

        let result =
            collect_source(&store, &ClaudeCodeAdapter, &[good, bad], &ctx(), opts()).unwrap();

        assert_eq!(result.source, "claude");
        assert_eq!(result.files_attempted, 2);
        assert_eq!(result.files_failed, 1, "bad file skipped");
        assert_eq!(result.events_imported, 2, "good file fully collected");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_broken_source_does_not_stop_another() {
        let store = Store::open_in_memory().unwrap();

        // Claude: an unrecognized stream (contained).
        let mut claude_bad = "{\"nope\":1}".as_bytes();
        let claude =
            collect_reader(&store, &ClaudeCodeAdapter, &mut claude_bad, &ctx(), opts()).unwrap();
        assert!(claude.is_none());

        // Codex: a valid stream still collects after Claude's failure.
        let codex_fixture = format!(
            "{}\n{}\n{}",
            "{\"timestamp\":\"2023-11-25T10:00:00.000Z\",\"type\":\"session_meta\",\
              \"payload\":{\"session_id\":\"s\",\"cli_version\":\"0.130.0\"}}",
            "{\"timestamp\":\"2023-11-25T10:00:01.000Z\",\"type\":\"turn_context\",\
              \"payload\":{\"model\":\"gpt-5-codex\",\"turn_id\":\"t\"}}",
            "{\"timestamp\":\"2023-11-25T11:00:00.000Z\",\"type\":\"event_msg\",\
              \"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":\
              {\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":40,\
              \"reasoning_output_tokens\":0,\"total_tokens\":140}}}}"
        );
        let mut codex = codex_fixture.as_bytes();
        let result = collect_reader(&store, &CodexAdapter, &mut codex, &ctx(), opts()).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().events_imported, 1);
    }
}
