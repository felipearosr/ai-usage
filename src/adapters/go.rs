//! OpenCode Go source adapter.
//!
//! Understands OpenCode's message persistence
//! (`~/.local/share/opencode/storage/message/<sessionID>/<messageID>.json`):
//! one pretty-printed JSON object per file describing a single message. The
//! assistant message carries the exact model in `modelID`, the routing
//! provider in `providerID`, the session in `sessionID` (stored only as a
//! hash), an epoch-millisecond `time.created`, and the token accounting in
//! `tokens` — `input`, `output`, `reasoning`, and `cache.{read,write}`.
//!
//! **Scope.** Only the OpenCode Go subscription is attributed to this source.
//! OpenCode's own managed gateway records `providerID: "opencode"`; a request
//! routed to Anthropic, OpenAI, OpenRouter, or any other external provider
//! carries that provider's id and is recognized-but-excluded, never
//! misattributed into `go` (nor into `claude`/`codex`). This matters because
//! Go serves models that *look* like other vendors' (e.g. `gpt-5.6-luna`
//! through the Go OpenAI endpoint, `minimax-m3` through the Go Anthropic
//! endpoint): those stay inside the `go` accounting domain by provider, not
//! by model name.
//!
//! **Normalization** (documented, source-specific): OpenCode reports cache
//! reads and writes as sibling fields to `input`, so — unlike Codex — no
//! subtraction is applied; each token class is preserved exactly as reported
//! and a class the source did not report stays null, never zero. `cost` is a
//! finite USD value reported per message; it is preserved as integer micros.
//! `sessionID` is stored only as a hash, and nothing else about the record
//! (`path.cwd`/`path.root`, text, tool content) is ever read.
//!
//! **Quota captures** (`ingest_quota`) map Go's three windows to aiu's names:
//! `five_hour` → `5h`, `seven_day` → `week`, `month` → `month`. The monthly
//! window is unique to Go and proves window sets are data-driven per source
//! rather than hardcoded globally.
//!
//! A file that is not a recognizable OpenCode message fails loudly
//! ([`AiuError::UnrecognizedFormat`]); a recognized user/system message, or a
//! recognized external-provider request, is skipped without error.

use std::io::BufRead;

use serde_json::Value;

use crate::adapters::{EventSink, IngestContext, ParseSummary, ProgressFn, SourceAdapter};
use crate::error::{AiuError, Result};
use crate::hash::short_hash_hex;
use crate::store::{NewEvent, NewSnapshot};
use crate::utc;

pub const SOURCE: &str = "go";
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The provider id that marks an OpenCode Go subscription request in a message
/// record. External providers carry their own id and are excluded.
const GO_PROVIDER_ID: &str = "opencode";

/// Message roles recognized as OpenCode messages. Only `assistant` carries
/// usage accounting; `user`/`system` are recognized-but-ignored so unrelated
/// message files never break ingestion, while a wholly non-message file still
/// fails loudly below.
const KNOWN_ROLES: [&str; 3] = ["user", "assistant", "system"];

pub struct OpenCodeGoAdapter;

impl Default for OpenCodeGoAdapter {
    fn default() -> Self {
        Self
    }
}

/// A usage-bearing assistant message parsed into normalized form.
struct ParsedMessage {
    message_id: String,
    session_id_hash: Option<String>,
    ts_utc: String,
    exact_model: String,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    reported_cost_micros: Option<i64>,
}

impl SourceAdapter for OpenCodeGoAdapter {
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
        // One file = one message. A single bounded document read (matching the
        // quota-capture path); the import machinery streams files one at a
        // time, so memory stays bounded regardless of history size.
        let mut raw = String::new();
        input.read_to_string(&mut raw)?;
        let mut summary = ParseSummary::default();
        if raw.trim().is_empty() {
            return Ok(summary);
        }
        summary.records_seen = 1;
        progress(1);

        let doc: Value = serde_json::from_str(raw.trim())
            .map_err(|e| unrecognized(&format!("message is not valid JSON: {e}")))?;
        let Some(obj) = doc.as_object() else {
            return Err(unrecognized("message is not a JSON object"));
        };
        let Some(role) = obj.get("role").and_then(Value::as_str) else {
            return Err(unrecognized("message has no role"));
        };
        if !KNOWN_ROLES.contains(&role) {
            return Err(unrecognized(&format!("unknown message role {role:?}")));
        }
        if role != "assistant" {
            // user/system carry no usage accounting.
            return Ok(summary);
        }

        // Discriminate Go vs external providers before touching usage: an
        // external-provider request is recognized and deliberately excluded,
        // never misattributed.
        let Some(provider) = obj.get("providerID").and_then(Value::as_str) else {
            summary.malformed_skipped += 1;
            return Ok(summary);
        };
        if provider != GO_PROVIDER_ID {
            return Ok(summary);
        }

        let Some(msg) = parse_usage(obj) else {
            summary.malformed_skipped += 1;
            return Ok(summary);
        };
        flush(&msg, ctx, sink)?;
        summary.events_emitted += 1;
        Ok(summary)
    }

    fn ingest_quota(
        &self,
        input: &mut dyn BufRead,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        progress: &mut ProgressFn<'_>,
    ) -> Result<ParseSummary> {
        let mut raw = String::new();
        input.read_to_string(&mut raw)?;
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
                Some(Value::String(s)) => {
                    Some(utc::parse_rfc3339_utc_loose(s).ok_or_else(|| {
                        unrecognized_quota(&format!("window {key:?} has unparsable resets_at"))
                    })?)
                }
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

fn unrecognized(detail: &str) -> AiuError {
    AiuError::UnrecognizedFormat {
        source: SOURCE,
        detail: detail.to_string(),
    }
}

fn unrecognized_quota(detail: &str) -> AiuError {
    unrecognized(detail)
}

fn map_window(vendor_key: &str) -> Option<&'static str> {
    match vendor_key {
        "five_hour" => Some("5h"),
        "seven_day" => Some("week"),
        "month" => Some("month"),
        _ => None,
    }
}

fn flush(msg: &ParsedMessage, ctx: &IngestContext, sink: &mut dyn EventSink) -> Result<()> {
    let event_id = build_event_id(ctx, msg);
    let _newly_stored = sink.accept_event(NewEvent {
        event_id,
        workspace_id: ctx.workspace_id.clone(),
        device_id: ctx.device_id.clone(),
        source: SOURCE.to_string(),
        tool: "opencode".to_string(),
        exact_model: msg.exact_model.clone(),
        session_id_hash: msg.session_id_hash.clone(),
        ts_utc: msg.ts_utc.clone(),
        input_tokens: msg.input_tokens,
        cached_input_tokens: msg.cached_input_tokens,
        cache_write_tokens: msg.cache_write_tokens,
        output_tokens: msg.output_tokens,
        reasoning_tokens: msg.reasoning_tokens,
        reported_cost_micros: msg.reported_cost_micros,
        // Message files do not stamp the OpenCode CLI version.
        tool_version: None,
        adapter_version: Some(ADAPTER_VERSION.to_string()),
    })?;
    Ok(())
}

/// Deterministic identity from the strongest available components, so
/// re-running import never double-counts (spec idempotency rule). The message
/// id is a globally-unique ULID; device + session + timestamp + model keep the
/// identity stable and workspace-safe.
fn build_event_id(ctx: &IngestContext, msg: &ParsedMessage) -> String {
    let components = [
        SOURCE,
        &ctx.device_id,
        msg.session_id_hash.as_deref().unwrap_or("-"),
        &msg.message_id,
        &msg.ts_utc,
        &msg.exact_model,
    ];
    format!(
        "go:{:016x}",
        crate::hash::fnv1a64(components.join("\u{1f}").as_bytes())
    )
}

/// Extracts a normalized message from an assistant record. Returns None when
/// the record cannot be honestly accounted (missing id, model, timestamp, or
/// tokens object, or wrong-typed required fields). Absent token classes stay
/// null downstream, never zero; a wrong-typed class is malformed, never
/// coerced.
fn parse_usage(record: &serde_json::Map<String, Value>) -> Option<ParsedMessage> {
    let message_id = record.get("id")?.as_str()?.to_string();
    let exact_model = record.get("modelID")?.as_str()?.to_string();
    // OpenCode timestamps are epoch milliseconds.
    let created_ms = record.get("time")?.as_object()?.get("created")?.as_i64()?;
    let ts_utc = utc::format_epoch(created_ms.max(0) as u64 / 1000);
    let tokens = record.get("tokens")?.as_object()?;

    // Required token classes: wrong-typed is malformed, absent is null.
    let field = |key: &str| -> Option<Option<i64>> {
        match tokens.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(Value::Number(n)) => n.as_i64().map(Some),
            Some(_) => None,
        }
    };
    let input_tokens = field("input")?;
    let output_tokens = field("output")?;
    let reasoning_tokens = field("reasoning")?;

    // Cache reads/writes are siblings to input; absent (older records) is
    // null, wrong-typed is malformed.
    let cache_field = |key: &str| -> Option<Option<i64>> {
        match tokens
            .get("cache")
            .and_then(Value::as_object)
            .and_then(|c| c.get(key))
        {
            None | Some(Value::Null) => Some(None),
            Some(Value::Number(n)) => n.as_i64().map(Some),
            Some(_) => None,
        }
    };
    let cached_input_tokens = cache_field("read")?;
    let cache_write_tokens = cache_field("write")?;

    // Reported per-message cost in USD, preserved as integer micros. Absent or
    // null is unknown (null); wrong-typed is unknown, never coerced.
    let reported_cost_micros = match record.get("cost") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64().map(|c| (c * 1_000_000.0).round() as i64),
        Some(_) => None,
    };

    Some(ParsedMessage {
        message_id,
        session_id_hash: record
            .get("sessionID")
            .and_then(Value::as_str)
            .map(short_hash_hex),
        ts_utc,
        exact_model,
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
        reasoning_tokens,
        reported_cost_micros,
    })
}
