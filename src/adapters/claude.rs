//! Claude Code source adapter.
//!
//! Understands the two local persistence surfaces Claude Code provides:
//!
//! **Session transcripts** (`~/.claude/projects/<dir>/<session>.jsonl`):
//! newline-delimited JSON. Usage lives on `type: "assistant"` records in
//! `message.usage` with Anthropic's field names — `input_tokens`,
//! `cache_creation_input_tokens` (→ cache write), `cache_read_input_tokens`
//! (→ cached input), `output_tokens`. During streaming, Claude Code appends
//! several entries sharing one `message.id` whose counters grow; entries are
//! *replacement records* and the last observation wins. Restart/resume can
//! re-write identical trailing lines; those are duplicates. Token classes
//! absent from a record stay null downstream, never zero. `sessionId` is
//! stored only as an FNV-1a hash (privacy rule); `version` is preserved as
//! `tool_version`; nothing else about the record (cwd, paths, content) is
//! ever read.
//!
//! **Quota captures** (vendor usage endpoint shape):
//! `{"five_hour": {"utilization": 42.5, "resets_at": "...Z"},
//!   "seven_day": {"utilization": 12.3}}`.
//! Windows map to aiu's window names (`five_hour` → `5h`, `seven_day` →
//! `week`). These are state observations of the shared account, never
//! consumption events.
//!
//! A stream in which no record matches any known Claude shape fails loudly
//! ([`AiuError::UnrecognizedFormat`]); isolated unknown record types are
//! skipped so new upstream record kinds do not break ingestion.

use std::io::{BufRead, BufReader, Read};

use serde_json::Value;

use crate::adapters::{EventSink, IngestContext, ParseSummary, ProgressFn, SourceAdapter};
use crate::error::{AiuError, Result};
use crate::hash::short_hash_hex;
use crate::store::{NewEvent, NewSnapshot};
use crate::utc;

pub const SOURCE: &str = "claude";
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Record types this adapter recognizes from real transcripts.
const KNOWN_RECORD_TYPES: [&str; 4] = ["assistant", "user", "system", "summary"];

pub struct ClaudeCodeAdapter;

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self
    }
}

/// A usage-bearing assistant entry held until we know no later replacement
/// for the same message id will arrive. Only ONE pending id exists at a
/// time, keeping memory bounded even for very long histories.
#[derive(PartialEq)]
struct PendingUsage {
    message_id: String,
    session_id_hash: Option<String>,
    ts_utc: String,
    exact_model: String,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    output_tokens: Option<i64>,
    tool_version: Option<String>,
    request_id: Option<String>,
}

impl SourceAdapter for ClaudeCodeAdapter {
    fn source(&self) -> &'static str {
        SOURCE
    }

    fn version(&self) -> &'static str {
        ADAPTER_VERSION
    }

    fn ingest(
        &self,
        input: &mut dyn BufRead,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        progress: &mut ProgressFn<'_>,
    ) -> Result<ParseSummary> {
        let mut summary = ParseSummary::default();
        let mut recognized_any = false;
        let mut pending: Option<PendingUsage> = None;
        let mut lines_seen = 0u64;

        for line in input.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(AiuError::Io(e)),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            lines_seen += 1;
            summary.records_seen += 1;
            if lines_seen.is_multiple_of(1_000) {
                progress(summary.records_seen);
            }

            let parsed: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => {
                    // Truncated or corrupt line: skip, count, keep streaming.
                    summary.malformed_skipped += 1;
                    continue;
                }
            };
            let Some(obj) = parsed.as_object() else {
                summary.malformed_skipped += 1;
                continue;
            };
            let Some(record_type) = obj.get("type").and_then(Value::as_str) else {
                summary.malformed_skipped += 1;
                continue;
            };
            if !KNOWN_RECORD_TYPES.contains(&record_type) {
                // Unknown but typed: count it, expect other records to carry
                // the format recognition. Nothing recognizable at all in the
                // whole stream becomes a loud failure below.
                continue;
            }
            recognized_any = true;

            if record_type != "assistant" {
                continue; // user/system/summary carry no usage accounting
            }

            let Some(usage) = parse_usage(obj) else {
                summary.malformed_skipped += 1;
                continue;
            };

            match &pending {
                Some(prev) if prev.message_id == usage.message_id => {
                    if *prev == usage {
                        summary.duplicates_skipped += 1;
                    } else {
                        // Streaming progression: the newer entry replaces the
                        // older cumulative snapshot wholesale.
                        pending = Some(usage);
                        summary.streamed_snapshots_collapsed += 1;
                    }
                }
                Some(done) => {
                    flush(done, ctx, sink)?;
                    pending = Some(usage);
                }
                None => pending = Some(usage),
            }
        }
        progress(summary.records_seen);

        if summary.records_seen > 0 && !recognized_any {
            return Err(AiuError::UnrecognizedFormat {
                source: SOURCE,
                detail:
                    "no record matched any known Claude Code transcript type (assistant/user/system/summary)"
                        .to_string(),
            });
        }

        if let Some(done) = pending.take() {
            flush(&done, ctx, sink)?;
        }
        Ok(summary)
    }

    fn ingest_quota(
        &self,
        input: &mut dyn BufRead,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        progress: &mut ProgressFn<'_>,
    ) -> Result<ParseSummary> {
        // Quota captures are tiny documents; read fully, then validate hard.
        let mut raw = String::new();
        BufReader::new(input).read_to_string(&mut raw)?;
        progress(raw.lines().count() as u64);

        if raw.trim().is_empty() {
            return Ok(ParseSummary::default());
        }
        let doc: Value =
            serde_json::from_str(raw.trim()).map_err(|e| AiuError::UnrecognizedFormat {
                source: SOURCE,
                detail: format!("quota capture is not valid JSON: {e}"),
            })?;
        let Some(obj) = doc.as_object() else {
            return Err(unrecognized_quota("quota capture is not a JSON object"));
        };
        if obj.is_empty() {
            return Ok(ParseSummary::default()); // vendor reported nothing yet
        }

        let mut summary = ParseSummary {
            records_seen: obj.len() as u64,
            ..ParseSummary::default()
        };
        for (key, value) in obj {
            let Some(window) = map_window(key) else {
                return Err(unrecognized_quota(&format!("unknown quota window {key:?}")));
            };
            let entry = value
                .as_object()
                .ok_or_else(|| unrecognized_quota(&format!("window {key:?} is not an object")))?;
            let used_percent = entry
                .get("utilization")
                .and_then(Value::as_f64)
                .ok_or_else(|| {
                    unrecognized_quota(&format!("window {key:?} has no numeric utilization"))
                })?;
            let resets_at_utc = match entry.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(parse_timestamp(s).ok_or_else(|| {
                    unrecognized_quota(&format!("window {key:?} has unparsable resets_at"))
                })?),
                Some(_) => {
                    return Err(unrecognized_quota(&format!(
                        "window {key:?} has non-string resets_at"
                    )))
                }
            };
            let stored = sink.accept_snapshot(NewSnapshot {
                source: SOURCE.to_string(),
                window: window.to_string(),
                used_percent,
                resets_at_utc,
                observed_at_utc: utc::format_epoch(ctx.now_epoch),
                observing_device_id: ctx.device_id.clone(),
            })?;
            if stored {
                summary.snapshots_emitted += 1;
            }
        }
        Ok(summary)
    }
}

fn unrecognized_quota(detail: &str) -> AiuError {
    AiuError::UnrecognizedFormat {
        source: SOURCE,
        detail: detail.to_string(),
    }
}

fn map_window(vendor_key: &str) -> Option<&'static str> {
    match vendor_key {
        "five_hour" => Some("5h"),
        "seven_day" => Some("week"),
        _ => None,
    }
}

fn flush(done: &PendingUsage, ctx: &IngestContext, sink: &mut dyn EventSink) -> Result<()> {
    let event_id = build_event_id(ctx, done);
    // Storage-level repeats (already-known identities) are accounted by the
    // sink; the adapter's duplicate count covers in-stream repeats only.
    let _newly_stored = sink.accept_event(NewEvent {
        event_id,
        workspace_id: ctx.workspace_id.clone(),
        device_id: ctx.device_id.clone(),
        source: SOURCE.to_string(),
        tool: "claude-code".to_string(),
        exact_model: done.exact_model.clone(),
        session_id_hash: done.session_id_hash.clone(),
        ts_utc: done.ts_utc.clone(),
        input_tokens: done.input_tokens,
        cached_input_tokens: done.cached_input_tokens,
        cache_write_tokens: done.cache_write_tokens,
        output_tokens: done.output_tokens,
        // Claude Code transcripts report no separate reasoning class; it
        // stays null rather than being guessed as zero.
        reasoning_tokens: None,
        reported_cost_micros: None,
        tool_version: done.tool_version.clone(),
        adapter_version: Some(ADAPTER_VERSION.to_string()),
    })?;
    Ok(())
}

/// Deterministic identity from the strongest available components, so
/// re-running import never double-counts (spec idempotency rule).
fn build_event_id(ctx: &IngestContext, u: &PendingUsage) -> String {
    let components = [
        SOURCE,
        &ctx.device_id,
        u.session_id_hash.as_deref().unwrap_or("-"),
        &u.message_id,
        u.request_id.as_deref().unwrap_or("-"),
        &u.ts_utc,
        &u.exact_model,
    ];
    format!(
        "claude:{:016x}",
        crate::hash::fnv1a64(components.join("\u{1f}").as_bytes())
    )
}

/// Extracts a normalized pending usage entry from an assistant record.
/// Returns None when the record cannot be honestly accounted (missing id,
/// model, timestamp, usage object, or wrong-typed token values).
fn parse_usage(record: &serde_json::Map<String, Value>) -> Option<PendingUsage> {
    let message = record.get("message")?.as_object()?;
    let message_id = message.get("id")?.as_str()?.to_string();
    let exact_model = message.get("model")?.as_str()?.to_string();
    let usage = message.get("usage")?.as_object()?;

    let ts_raw = record.get("timestamp")?.as_str()?;
    let ts_utc = parse_timestamp(ts_raw)?;

    let tokens = |key: &str| -> Option<Option<i64>> {
        match usage.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(Value::Number(n)) => n.as_i64().map(Some),
            Some(_) => None,
        }
    };
    let input_tokens = tokens("input_tokens")?;
    let cached_input_tokens = tokens("cache_read_input_tokens")?;
    let cache_write_tokens = tokens("cache_creation_input_tokens")?;
    let output_tokens = tokens("output_tokens")?;

    Some(PendingUsage {
        message_id,
        session_id_hash: record
            .get("sessionId")
            .and_then(Value::as_str)
            .map(short_hash_hex),
        ts_utc,
        exact_model,
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
        tool_version: record
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        request_id: record
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Parses RFC 3339 UTC timestamps, tolerating fractional seconds
/// ("2025-06-01T12:00:00.123Z") as written by Claude Code. Returns seconds.
fn parse_timestamp(raw: &str) -> Option<String> {
    if let Some(epoch) = utc::parse_rfc3339_utc(raw) {
        return Some(utc::format_epoch(epoch));
    }
    let bytes = raw.as_bytes();
    if bytes.len() > 20 && bytes[19] == b'.' && *bytes.last()? == b'Z' {
        let epoch = utc::parse_rfc3339_utc(&format!("{}Z", &raw[..19]))?;
        return Some(utc::format_epoch(epoch));
    }
    None
}
