//! Codex (OpenAI Codex CLI) source adapter.
//!
//! Understands Codex CLI rollout persistence
//! (`~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`): newline-delimited JSON
//! where every line is `{"timestamp": "...", "type": "...", "payload": {...}}`.
//!
//! The first line is `session_meta` (carrying the session id — stored only as
//! a hash — and the CLI version). Usage is reported through `event_msg` lines
//! whose inner type is `token_count`: `payload.info.total_token_usage` is a
//! *cumulative* counter for the whole session, and `payload.info.last_token_usage`
//! is the most recent turn's delta. `turn_context` lines carry the exact model
//! in use. `response_item`, `agent_message`, and `user_message` lines carry
//! content and are never read.
//!
//! **Cumulative/streaming semantics.** OpenAI reports cached reads *inside*
//! `input_tokens`, and `token_count` snapshots repeat the running total with
//! fresh timestamps as the session streams. Naïvely summing every line would
//! inflate totals. This adapter therefore emits the *delta* between consecutive
//! cumulative snapshots (a repeated identical snapshot collapses to nothing)
//! and attributes each delta to the exact model active at that moment. When
//! the cumulative counter is absent it falls back to `last_token_usage`.
//!
//! **Quota state** rides along on `token_count` events as `payload.rate_limits`
//! (`primary.used_percent`, `primary.window_minutes`, `primary.resets_at` in
//! epoch seconds). These are state observations of the shared account, never
//! consumption events; a new observation is emitted only when it changes.
//!
//! Normalization (documented, source-specific): `input_tokens` stores
//! non-cached input (`input_tokens - cached_input_tokens`) so cache reads are
//! never counted twice; `cached_input_tokens` preserves the raw cache reads;
//! `output_tokens` and `reasoning_tokens` are preserved separately, never
//! summed. Codex does not expose cache writes, so `cache_write_tokens` stays
//! null — never guessed as zero.

use std::collections::HashMap;
use std::io::BufRead;

use serde_json::Value;

use crate::adapters::{EventSink, IngestContext, ParseSummary, ProgressFn, SourceAdapter};
use crate::error::{AiuError, Result};
use crate::hash::short_hash_hex;
use crate::store::{NewEvent, NewSnapshot};
use crate::utc;

pub const SOURCE: &str = "codex";
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Line types this adapter recognizes from real rollout files.
const KNOWN_RECORD_TYPES: [&str; 4] =
    ["session_meta", "turn_context", "event_msg", "response_item"];

pub struct CodexAdapter;

impl Default for CodexAdapter {
    fn default() -> Self {
        Self
    }
}

/// A token snapshot as reported by `token_count.info`. All fields are always
/// present in Codex output (zero when unused), so the adapter never guesses.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct TokenUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
}

impl SourceAdapter for CodexAdapter {
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

        let mut session_id_hash: Option<String> = None;
        let mut current_model: Option<String> = None;
        let mut tool_version: Option<String> = None;
        // Per-session cumulative baseline, so resumed and interleaved
        // sessions each keep their own running total without mixing.
        let mut baseline: HashMap<String, TokenUsage> = HashMap::new();
        // Last rate-limit observation emitted per window, so the identical
        // observation riding on every token_count does not flood the sink.
        let mut last_rate_limit: HashMap<String, (f64, Option<String>)> = HashMap::new();

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
                continue;
            }
            recognized_any = true;

            let Some(payload) = obj.get("payload").and_then(Value::as_object) else {
                summary.malformed_skipped += 1;
                continue;
            };

            match record_type {
                "session_meta" => {
                    session_id_hash = payload
                        .get("session_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .map(short_hash_hex);
                    tool_version = payload
                        .get("cli_version")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    // A new session starts its own model context.
                    current_model = None;
                }
                "turn_context" => {
                    if let Some(model) = payload.get("model").and_then(Value::as_str) {
                        current_model = Some(model.to_string());
                    }
                }
                "event_msg" => {
                    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                        continue;
                    }
                    let ts_raw = obj.get("timestamp").and_then(Value::as_str);
                    handle_token_count(
                        payload,
                        ts_raw,
                        ctx,
                        sink,
                        &mut summary,
                        session_id_hash.as_deref(),
                        &current_model,
                        tool_version.as_deref(),
                        &mut baseline,
                        &mut last_rate_limit,
                    )?;
                }
                "response_item" => { /* content only, never read */ }
                _ => unreachable!(),
            }
        }
        progress(summary.records_seen);

        if summary.records_seen > 0 && !recognized_any {
            return Err(AiuError::UnrecognizedFormat {
                source: SOURCE,
                detail: "no record matched any known Codex rollout type \
                     (session_meta/turn_context/event_msg/response_item)"
                    .to_string(),
            });
        }
        Ok(summary)
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_token_count(
    payload: &serde_json::Map<String, Value>,
    ts_raw: Option<&str>,
    ctx: &IngestContext,
    sink: &mut dyn EventSink,
    summary: &mut ParseSummary,
    session_id_hash: Option<&str>,
    current_model: &Option<String>,
    tool_version: Option<&str>,
    baseline: &mut HashMap<String, TokenUsage>,
    last_rate_limit: &mut HashMap<String, (f64, Option<String>)>,
) -> Result<()> {
    if let Some(rate_limits) = payload.get("rate_limits").and_then(Value::as_object) {
        emit_quota_if_changed(rate_limits, ctx, sink, summary, last_rate_limit)?;
    }

    // `info` can be null on the very first emission of a session.
    let Some(info) = payload.get("info").and_then(Value::as_object) else {
        return Ok(());
    };

    let Some(model) = current_model.as_deref() else {
        // Honest accounting needs an exact model; a token_count without any
        // turn_context hint cannot be attributed and is skipped (never
        // guessed, per the null discipline).
        summary.malformed_skipped += 1;
        return Ok(());
    };
    let Some(session) = session_id_hash else {
        summary.malformed_skipped += 1;
        return Ok(());
    };
    let Some(ts_utc) = ts_raw.and_then(utc::parse_rfc3339_utc_loose) else {
        summary.malformed_skipped += 1;
        return Ok(());
    };

    if let Some(cum) = info.get("total_token_usage").and_then(Value::as_object) {
        let Some(cur) = parse_usage(cum) else {
            summary.malformed_skipped += 1;
            return Ok(());
        };
        let key = session.to_string();
        let prev = baseline.get(&key).copied();
        let delta = match prev {
            None => cur,
            Some(p) if cur == p => {
                summary.duplicates_skipped += 1;
                return Ok(());
            }
            // Counter reset (subagent fork replay): treat as a fresh baseline.
            Some(p) if cur.total_tokens < p.total_tokens => cur,
            Some(p) => TokenUsage {
                input_tokens: cur.input_tokens - p.input_tokens,
                cached_input_tokens: cur.cached_input_tokens - p.cached_input_tokens,
                output_tokens: cur.output_tokens - p.output_tokens,
                reasoning_output_tokens: cur.reasoning_output_tokens - p.reasoning_output_tokens,
                total_tokens: cur.total_tokens - p.total_tokens,
            },
        };
        baseline.insert(key, cur);
        if delta.total_tokens <= 0 {
            return Ok(());
        }
        emit_event(
            sink,
            ctx,
            session,
            model,
            &ts_utc,
            &delta,
            &cur,
            tool_version,
            false,
        )?;
        summary.events_emitted += 1;
    } else if let Some(last) = info.get("last_token_usage").and_then(Value::as_object) {
        let Some(delta) = parse_usage(last) else {
            summary.malformed_skipped += 1;
            return Ok(());
        };
        if delta.total_tokens <= 0 {
            return Ok(());
        }
        emit_event(
            sink,
            ctx,
            session,
            model,
            &ts_utc,
            &delta,
            &delta,
            tool_version,
            true,
        )?;
        summary.events_emitted += 1;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_event(
    sink: &mut dyn EventSink,
    ctx: &IngestContext,
    session_id_hash: &str,
    model: &str,
    ts_utc: &str,
    delta: &TokenUsage,
    cumulative: &TokenUsage,
    tool_version: Option<&str>,
    fallback: bool,
) -> Result<()> {
    // Deterministic identity. Cumulative mode anchors on the monotonic running
    // total; the fallback anchors on the timestamp + values. Either way
    // re-ingesting the same file produces the same ids, so import never
    // double-counts.
    let anchor = if fallback {
        format!(
            "last:{}:{}:{}:{}:{}:{}",
            ts_utc,
            delta.input_tokens,
            delta.cached_input_tokens,
            delta.output_tokens,
            delta.reasoning_output_tokens,
            delta.total_tokens
        )
    } else {
        format!(
            "cum:{}:{}:{}:{}:{}",
            cumulative.input_tokens,
            cumulative.cached_input_tokens,
            cumulative.output_tokens,
            cumulative.reasoning_output_tokens,
            cumulative.total_tokens
        )
    };
    let joined = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        SOURCE, ctx.device_id, session_id_hash, model, anchor
    );
    let event_id = format!("codex:{:016x}", crate::hash::fnv1a64(joined.as_bytes()));

    sink.accept_event(NewEvent {
        event_id,
        workspace_id: ctx.workspace_id.clone(),
        device_id: ctx.device_id.clone(),
        source: SOURCE.to_string(),
        tool: "codex".to_string(),
        exact_model: model.to_string(),
        session_id_hash: Some(session_id_hash.to_string()),
        ts_utc: ts_utc.to_string(),
        input_tokens: Some(delta.input_tokens.saturating_sub(delta.cached_input_tokens)),
        cached_input_tokens: Some(delta.cached_input_tokens),
        cache_write_tokens: None,
        output_tokens: Some(delta.output_tokens),
        reasoning_tokens: Some(delta.reasoning_output_tokens),
        reported_cost_micros: None,
        tool_version: tool_version.map(str::to_string),
        adapter_version: Some(ADAPTER_VERSION.to_string()),
    })?;
    Ok(())
}

/// Emits a quota snapshot when the rate-limit observation changed since the
/// last one for that window. Unchanged observations riding on later events
/// are dropped so the snapshot history stays sparse.
fn emit_quota_if_changed(
    rate_limits: &serde_json::Map<String, Value>,
    ctx: &IngestContext,
    sink: &mut dyn EventSink,
    summary: &mut ParseSummary,
    last: &mut HashMap<String, (f64, Option<String>)>,
) -> Result<()> {
    let Some(primary) = rate_limits.get("primary").and_then(Value::as_object) else {
        return Ok(());
    };
    // Unknown window lengths are skipped, never mapped to a guessed window.
    let Some(window) = primary
        .get("window_minutes")
        .and_then(Value::as_i64)
        .and_then(map_window)
    else {
        return Ok(());
    };
    let Some(used_percent) = primary.get("used_percent").and_then(Value::as_f64) else {
        return Ok(());
    };
    let resets_at_utc = match primary.get("resets_at").and_then(Value::as_i64) {
        Some(epoch) if epoch > 0 => Some(utc::format_epoch(epoch as u64)),
        _ => None,
    };

    if last.get(window) == Some(&(used_percent, resets_at_utc.clone())) {
        return Ok(());
    }
    last.insert(window.to_string(), (used_percent, resets_at_utc.clone()));

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
    Ok(())
}

fn map_window(window_minutes: i64) -> Option<&'static str> {
    match window_minutes {
        300 => Some("5h"),
        10_080 => Some("week"),
        43_200 => Some("month"),
        _ => None,
    }
}

/// Parses a `token_count` usage object. All fields are always present in
/// Codex output; a missing field defaults to zero, a wrong-typed field is
/// malformed (never coerced).
fn parse_usage(obj: &serde_json::Map<String, Value>) -> Option<TokenUsage> {
    let field = |key: &str| -> Option<i64> {
        match obj.get(key) {
            None => Some(0),
            Some(Value::Number(v)) => v.as_i64(),
            Some(_) => None,
        }
    };
    Some(TokenUsage {
        input_tokens: field("input_tokens")?,
        cached_input_tokens: field("cached_input_tokens")?,
        output_tokens: field("output_tokens")?,
        reasoning_output_tokens: field("reasoning_output_tokens")?,
        total_tokens: field("total_tokens")?,
    })
}
