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
//! the cumulative counter is absent it falls back to `last_token_usage`,
//! collapsing repeated identical fallback values the same way.
//!
//! **Scope.** The cumulative baseline is per ingest stream (one rollout file =
//! one session). Codex resumes append to the same file, so the running total
//! continues naturally within one stream. A counter that drops mid-stream (a
//! subagent fork replay) starts a fresh baseline. Reconciling a replayed
//! prefix against a parent file already imported is the collection/scheduler's
//! job (it owns adapter position state), not this single-file adapter's.
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

/// One quota observation from `rate_limits.primary`, used to suppress repeats.
#[derive(Clone, PartialEq)]
struct RateLimitObservation {
    used_percent: f64,
    resets_at_utc: Option<String>,
}

/// Mutable parse state for a single ingest pass. Grouping the running session
/// context and baselines here keeps the per-record helpers small instead of
/// threading a dozen parameters through them.
#[derive(Default)]
struct ParseState {
    session_id_hash: Option<String>,
    current_model: Option<String>,
    tool_version: Option<String>,
    /// Per-session cumulative baseline, so resumed and interleaved sessions
    /// each keep their own running total without mixing.
    baseline: HashMap<String, TokenUsage>,
    /// Last rate-limit observation emitted per window, so the identical
    /// observation riding on every token_count does not flood the sink.
    last_rate_limit: HashMap<String, RateLimitObservation>,
    /// Last `last_token_usage` fallback value per session, to collapse
    /// repeated identical snapshots that only differ in timestamp.
    last_fallback: HashMap<String, TokenUsage>,
}

impl ParseState {
    fn begin_session(&mut self, payload: &serde_json::Map<String, Value>) {
        self.session_id_hash = payload
            .get("session_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .map(short_hash_hex);
        self.tool_version = payload
            .get("cli_version")
            .and_then(Value::as_str)
            .map(str::to_string);
        // A new session starts its own model context.
        self.current_model = None;
    }

    fn set_model(&mut self, payload: &serde_json::Map<String, Value>) {
        if let Some(model) = payload.get("model").and_then(Value::as_str) {
            self.current_model = Some(model.to_string());
        }
    }

    fn handle_token_count(
        &mut self,
        payload: &serde_json::Map<String, Value>,
        ts_raw: Option<&str>,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        summary: &mut ParseSummary,
    ) -> Result<()> {
        if let Some(rate_limits) = payload.get("rate_limits").and_then(Value::as_object) {
            self.emit_quota_if_changed(rate_limits, ctx, sink, summary)?;
        }

        // `info` can be null on the very first emission of a session.
        let Some(info) = payload.get("info").and_then(Value::as_object) else {
            return Ok(());
        };

        let Some(model) = self.current_model.as_deref() else {
            // Honest accounting needs an exact model; a token_count without any
            // turn_context hint cannot be attributed and is skipped (never
            // guessed, per the null discipline).
            summary.malformed_skipped += 1;
            return Ok(());
        };
        let Some(session) = self.session_id_hash.as_deref() else {
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
            let prev = self.baseline.get(&key).copied();
            let delta = match prev {
                None => cur,
                Some(p) if cur == p => {
                    summary.duplicates_skipped += 1;
                    return Ok(());
                }
                // Counter reset (subagent fork replay): fresh baseline.
                Some(p) if cur.total_tokens < p.total_tokens => cur,
                Some(p) => TokenUsage {
                    input_tokens: cur.input_tokens - p.input_tokens,
                    cached_input_tokens: cur.cached_input_tokens - p.cached_input_tokens,
                    output_tokens: cur.output_tokens - p.output_tokens,
                    reasoning_output_tokens: cur.reasoning_output_tokens
                        - p.reasoning_output_tokens,
                    total_tokens: cur.total_tokens - p.total_tokens,
                },
            };
            self.baseline.insert(key, cur);
            if delta.total_tokens <= 0 {
                return Ok(());
            }
            let anchor = format!(
                "cum:{}:{}:{}:{}:{}",
                cur.input_tokens,
                cur.cached_input_tokens,
                cur.output_tokens,
                cur.reasoning_output_tokens,
                cur.total_tokens
            );
            emit_event(
                sink,
                ctx,
                session,
                model,
                &ts_utc,
                &delta,
                &anchor,
                self.tool_version.as_deref(),
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
            // A repeated identical fallback value is a streaming replacement,
            // not new usage.
            if self.last_fallback.get(session) == Some(&delta) {
                summary.duplicates_skipped += 1;
                return Ok(());
            }
            self.last_fallback.insert(session.to_string(), delta);
            let anchor = format!(
                "last:{}:{}:{}:{}:{}:{}",
                ts_utc,
                delta.input_tokens,
                delta.cached_input_tokens,
                delta.output_tokens,
                delta.reasoning_output_tokens,
                delta.total_tokens
            );
            emit_event(
                sink,
                ctx,
                session,
                model,
                &ts_utc,
                &delta,
                &anchor,
                self.tool_version.as_deref(),
            )?;
            summary.events_emitted += 1;
        }

        Ok(())
    }

    /// Emits a quota snapshot when the rate-limit observation changed since
    /// the last one for that window; unchanged observations are dropped so
    /// snapshot history stays sparse.
    fn emit_quota_if_changed(
        &mut self,
        rate_limits: &serde_json::Map<String, Value>,
        ctx: &IngestContext,
        sink: &mut dyn EventSink,
        summary: &mut ParseSummary,
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

        let observation = RateLimitObservation {
            used_percent,
            resets_at_utc: resets_at_utc.clone(),
        };
        if self.last_rate_limit.get(window) == Some(&observation) {
            return Ok(());
        }
        self.last_rate_limit.insert(window.to_string(), observation);

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
        let mut state = ParseState::default();
        let mut recognized_any = false;

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
                "session_meta" => state.begin_session(payload),
                "turn_context" => state.set_model(payload),
                "event_msg" => {
                    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                        continue;
                    }
                    let ts_raw = obj.get("timestamp").and_then(Value::as_str);
                    state.handle_token_count(payload, ts_raw, ctx, sink, &mut summary)?;
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
fn emit_event(
    sink: &mut dyn EventSink,
    ctx: &IngestContext,
    session_id_hash: &str,
    model: &str,
    ts_utc: &str,
    delta: &TokenUsage,
    anchor: &str,
    tool_version: Option<&str>,
) -> Result<()> {
    // Deterministic identity: cumulative mode anchors on the monotonic running
    // total, the fallback on the timestamp + values. Either way re-ingesting
    // the same file produces the same ids, so import never double-counts.
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

/// Maps a Codex rate-limit window length (minutes) to aiu's window name.
/// Codex provides 5-hour and weekly subscription windows; lengths that do not
/// match are skipped rather than guessed.
fn map_window(window_minutes: i64) -> Option<&'static str> {
    match window_minutes {
        300 => Some("5h"),
        10_080 => Some("week"),
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
