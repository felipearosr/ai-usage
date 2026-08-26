//! Adapter-seam tests for the Codex adapter (issue 03, seam 2).
//!
//! Fixtures recorded in the shape of real Codex CLI rollouts are streamed
//! through `CodexAdapter` and assertions target the emitted normalized
//! events + quota snapshots — never internal call graphs. Mandated coverage:
//! normal sessions, streamed/replacement records, duplicate records,
//! truncated records, restart/resume, model switching, concurrent sessions,
//! quota reset, plus cumulative-counter semantics.

use aiu::adapters::codex::CodexAdapter;
use aiu::adapters::{EventSink, IngestContext, ParseSummary, SourceAdapter};
use aiu::error::AiuError;
use aiu::hash::short_hash_hex;
use aiu::import::{import_usage, ImportOptions};
use aiu::store::{NewEvent, NewSnapshot, Store};

const NOW: u64 = 1_700_913_600; // 2023-11-25T12:00:00Z

fn ctx() -> IngestContext {
    IngestContext {
        device_id: "dev-test".to_string(),
        workspace_id: "ws-test".to_string(),
        now_epoch: NOW,
    }
}

#[derive(Default)]
struct Collecting {
    events: Vec<NewEvent>,
    snapshots: Vec<NewSnapshot>,
}

impl EventSink for Collecting {
    fn accept_event(&mut self, event: NewEvent) -> aiu::error::Result<bool> {
        self.events.push(event);
        Ok(true)
    }
    fn accept_snapshot(&mut self, snapshot: NewSnapshot) -> aiu::error::Result<bool> {
        self.snapshots.push(snapshot);
        Ok(true)
    }
}

fn ingest(input: &str) -> (ParseSummary, Collecting) {
    let mut sink = Collecting::default();
    let summary = CodexAdapter
        .ingest(
            &mut input.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap();
    (summary, sink)
}

fn session_meta(session_id: &str, cli_version: &str) -> String {
    format!(
        "{{\"timestamp\":\"2023-11-25T10:00:00.000Z\",\"type\":\"session_meta\",\
         \"payload\":{{\"session_id\":\"{session_id}\",\"id\":\"thread-{session_id}\",\
         \"originator\":\"codex\",\"cli_version\":\"{cli_version}\"}}}}"
    )
}

fn turn_context(model: &str, turn: &str) -> String {
    format!(
        "{{\"timestamp\":\"2023-11-25T10:00:01.000Z\",\"type\":\"turn_context\",\
         \"payload\":{{\"model\":\"{model}\",\"turn_id\":\"{turn}\"}}}}"
    )
}

fn usage(input: i64, cached: i64, output: i64, reasoning: i64, total: i64) -> String {
    format!(
        "{{\"input_tokens\":{input},\"cached_input_tokens\":{cached},\
         \"output_tokens\":{output},\"reasoning_output_tokens\":{reasoning},\
         \"total_tokens\":{total}}}"
    )
}

fn token_count(ts: &str, total_usage: &str) -> String {
    format!(
        "{{\"timestamp\":\"{ts}\",\"type\":\"event_msg\",\
         \"payload\":{{\"type\":\"token_count\",\
         \"info\":{{\"total_token_usage\":{total_usage}}}}}}}"
    )
}

fn token_count_with_limits(ts: &str, total_usage: &str, rate_limits: &str) -> String {
    format!(
        "{{\"timestamp\":\"{ts}\",\"type\":\"event_msg\",\
         \"payload\":{{\"type\":\"token_count\",\
         \"info\":{{\"total_token_usage\":{total_usage}}},\
         \"rate_limits\":{rate_limits}}}}}"
    )
}

fn rate_limits(used_percent: f64, window_minutes: i64, resets_at: i64) -> String {
    format!(
        "{{\"limit_id\":\"codex\",\
         \"primary\":{{\"used_percent\":{used_percent},\
         \"window_minutes\":{window_minutes},\"resets_at\":{resets_at}}},\
         \"secondary\":null}}"
    )
}

#[test]
fn normal_session_produces_one_fully_attributed_event() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 20, 50, 10, 160)),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);

    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.records_seen, 3);
    assert_eq!(summary.malformed_skipped, 0);
    let e = &out.events[0];
    // Documented normalization: OpenAI reports cached reads inside input_tokens.
    assert_eq!(e.input_tokens, Some(80), "100 input - 20 cached");
    assert_eq!(e.cached_input_tokens, Some(20));
    assert_eq!(
        e.cache_write_tokens, None,
        "Codex never exposes cache writes"
    );
    assert_eq!(e.output_tokens, Some(50));
    assert_eq!(
        e.reasoning_tokens,
        Some(10),
        "reasoning preserved separately"
    );
    assert_eq!(e.reported_cost_micros, None, "cost is never guessed");
    assert_eq!(e.exact_model, "gpt-5-codex");
    assert_eq!(e.source, "codex");
    assert_eq!(e.tool, "codex");
    assert_eq!(
        e.session_id_hash.as_deref(),
        Some(short_hash_hex("sess-1").as_str())
    );
    assert_eq!(e.tool_version.as_deref(), Some("0.130.0"));
    assert_eq!(
        e.adapter_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(e.ts_utc, "2023-11-25T11:00:00Z");
    assert_eq!(e.device_id, "dev-test");
    // Deterministic identity: re-ingesting the same fixture yields the same id.
    let (_, again) = ingest(&fixture);
    assert_eq!(again.events[0].event_id, e.event_id);
}

#[test]
fn cumulative_counters_emit_deltas_that_sum_to_the_total() {
    // Three cumulative snapshots of one session: 140 -> 370 -> 520.
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 0, 40, 0, 140)),
        token_count("2023-11-25T11:00:05.000Z", &usage(250, 50, 100, 20, 370)),
        token_count("2023-11-25T11:00:10.000Z", &usage(300, 50, 190, 30, 520)),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 3);
    assert_eq!(summary.events_emitted, 3);
    assert_eq!(summary.duplicates_skipped, 0);

    let outputs: Vec<i64> = out
        .events
        .iter()
        .map(|e| e.output_tokens.unwrap())
        .collect();
    assert_eq!(outputs, vec![40, 60, 90]);
    assert_eq!(
        outputs.iter().sum::<i64>(),
        190,
        "sum equals final cumulative"
    );

    let inputs: Vec<i64> = out.events.iter().map(|e| e.input_tokens.unwrap()).collect();
    assert_eq!(inputs, vec![100, 100, 50], "non-cached input deltas");
    assert_eq!(
        inputs.iter().sum::<i64>(),
        250,
        "sum equals final non-cached input"
    );

    let cached: Vec<i64> = out
        .events
        .iter()
        .map(|e| e.cached_input_tokens.unwrap())
        .collect();
    assert_eq!(cached, vec![0, 50, 0]);
}

#[test]
fn repeated_identical_cumulative_snapshots_are_collapsed() {
    // Streaming writes the same running total again with fresh timestamps.
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 20, 50, 10, 160)),
        token_count("2023-11-25T11:00:01.000Z", &usage(100, 20, 50, 10, 160)),
        token_count("2023-11-25T11:00:02.000Z", &usage(100, 20, 50, 10, 160)),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1, "replacement records collapse to one");
    assert_eq!(summary.duplicates_skipped, 2);
    assert_eq!(out.events[0].output_tokens, Some(50));
}

#[test]
fn truncated_and_corrupt_lines_are_skipped_and_counted() {
    let good = token_count("2023-11-25T11:00:00.000Z", &usage(100, 0, 40, 0, 140));
    let truncated =
        token_count("2023-11-25T11:00:05.000Z", &usage(200, 0, 90, 0, 290))[..120].to_string();
    let fixture = format!(
        "{}\n{}\n{}\n{}\n{{not json\n\n{}",
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        good,
        truncated,
        good,
    );

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.malformed_skipped, 2, "truncated + not-json");
    assert_eq!(summary.duplicates_skipped, 1, "identical repeat");
}

#[test]
fn multiple_sessions_keep_independent_baselines() {
    // Two sessions concatenated: the second starts a fresh cumulative baseline.
    let fixture = [
        session_meta("sess-a", "0.130.0"),
        turn_context("gpt-5-codex", "turn-a"),
        token_count("2023-11-25T11:00:00.000Z", &usage(300, 0, 100, 0, 400)),
        session_meta("sess-b", "0.130.0"),
        turn_context("gpt-5.2-codex", "turn-b"),
        token_count("2023-11-25T11:05:00.000Z", &usage(50, 0, 20, 0, 70)),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 2);

    let b = out
        .events
        .iter()
        .find(|e| e.exact_model == "gpt-5.2-codex")
        .unwrap();
    assert_eq!(b.output_tokens, Some(20), "fresh baseline, not 20 - 400");
    assert_eq!(b.input_tokens, Some(50));
    assert_eq!(
        b.session_id_hash.as_deref(),
        Some(short_hash_hex("sess-b").as_str())
    );

    let a = out
        .events
        .iter()
        .find(|e| e.exact_model == "gpt-5-codex")
        .unwrap();
    assert_eq!(
        a.session_id_hash.as_deref(),
        Some(short_hash_hex("sess-a").as_str())
    );
}

#[test]
fn mid_session_model_switch_stays_per_event_exact() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 0, 40, 0, 140)),
        turn_context("gpt-5.2-codex", "turn-2"),
        token_count("2023-11-25T11:00:10.000Z", &usage(180, 0, 90, 0, 270)),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 2);
    let models: Vec<&str> = out.events.iter().map(|e| e.exact_model.as_str()).collect();
    assert_eq!(models, ["gpt-5-codex", "gpt-5.2-codex"]);
    assert_ne!(out.events[0].event_id, out.events[1].event_id);
}

#[test]
fn cumulative_counter_reset_starts_a_fresh_baseline() {
    // A subagent fork replay can drop the running total; the adapter must not
    // emit a negative delta.
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(300, 0, 100, 0, 400)),
        token_count("2023-11-25T11:00:10.000Z", &usage(50, 0, 20, 0, 70)),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 2);
    assert_eq!(out.events[1].output_tokens, Some(20));
    assert_eq!(out.events[1].input_tokens, Some(50));
}

#[test]
fn last_token_usage_fallback_emits_the_turn_delta_directly() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        "{\"timestamp\":\"2023-11-25T11:00:00.000Z\",\"type\":\"event_msg\",\
          \"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":\
          {\"input_tokens\":80,\"cached_input_tokens\":10,\"output_tokens\":30,\
          \"reasoning_output_tokens\":0,\"total_tokens\":110}}}}"
            .to_string(),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1);
    let e = &out.events[0];
    assert_eq!(e.input_tokens, Some(70), "80 - 10 cached");
    assert_eq!(e.output_tokens, Some(30));
}

#[test]
fn null_info_is_skipped_without_error() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        "{\"timestamp\":\"2023-11-25T11:00:00.000Z\",\"type\":\"event_msg\",\
          \"payload\":{\"type\":\"token_count\",\"info\":null}}"
            .to_string(),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 0);
    assert_eq!(summary.malformed_skipped, 0);
}

#[test]
fn token_count_without_model_hint_is_skipped_not_guessed() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        // no turn_context, so no exact model to attribute to
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 0, 40, 0, 140)),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 0);
    assert_eq!(summary.malformed_skipped, 1);
}

#[test]
fn wrong_typed_usage_is_malformed_not_guessed() {
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        "{\"timestamp\":\"2023-11-25T11:00:00.000Z\",\"type\":\"event_msg\",\
          \"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":\
          {\"input_tokens\":\"lots\",\"output_tokens\":3,\"total_tokens\":3}}}}"
            .to_string(),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert!(out.events.is_empty());
    assert_eq!(summary.malformed_skipped, 1);
}

#[test]
fn rate_limits_become_window_snapshots_with_resets() {
    let resets = 1_700_918_040; // 2023-11-25T13:14:00Z
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count_with_limits(
            "2023-11-25T11:00:00.000Z",
            &usage(100, 20, 50, 10, 160),
            &rate_limits(42.5, 10_080, resets),
        ),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(summary.snapshots_emitted, 1);
    assert_eq!(out.snapshots.len(), 1);
    let s = &out.snapshots[0];
    assert_eq!(s.source, "codex");
    assert_eq!(s.window, "week", "10080 minutes maps to week");
    assert_eq!(s.used_percent, 42.5);
    assert_eq!(s.resets_at_utc.as_deref(), Some("2023-11-25T13:14:00Z"));
    assert_eq!(s.observing_device_id, "dev-test");
}

#[test]
fn unchanged_rate_limit_observations_are_not_reemitted() {
    let rl = rate_limits(42.5, 10_080, 1_700_918_040);
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count_with_limits(
            "2023-11-25T11:00:00.000Z",
            &usage(100, 20, 50, 10, 160),
            &rl,
        ),
        token_count_with_limits(
            "2023-11-25T11:00:05.000Z",
            &usage(150, 25, 80, 15, 250),
            &rl,
        ),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.snapshots.len(), 1);
    assert_eq!(summary.snapshots_emitted, 1);
}

#[test]
fn quota_reset_arrives_as_new_observation_latest_wins() {
    use aiu::report;
    let store = Store::open_in_memory().unwrap();
    let before = format!(
        "{}\n{}\n{}",
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count_with_limits(
            "2023-11-25T11:00:00.000Z",
            &usage(100, 0, 40, 0, 140),
            &rate_limits(91.0, 10_080, 1_700_914_200),
        ),
    );
    import_usage(
        &store,
        &CodexAdapter,
        &mut before.as_bytes(),
        &ctx(),
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();

    let after = format!(
        "{}\n{}\n{}",
        session_meta("sess-2", "0.130.0"),
        turn_context("gpt-5-codex", "turn-2"),
        token_count_with_limits(
            "2023-11-25T14:00:00.000Z",
            &usage(10, 0, 5, 0, 15),
            &rate_limits(7.5, 10_080, 1_700_925_000),
        ),
    );
    let mut later = ctx();
    later.now_epoch = NOW + 3 * 3600;
    import_usage(
        &store,
        &CodexAdapter,
        &mut after.as_bytes(),
        &later,
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();

    let r = report::build(&store, NOW + 3 * 3600).unwrap();
    let codex = r.sources.iter().find(|s| s.source == "codex").unwrap();
    let week = codex.windows.iter().find(|w| w.window == "week").unwrap();
    assert_eq!(week.used_percent, 7.5, "latest observation wins");
    assert_eq!(week.resets_in_secs(NOW + 3 * 3600), Some(600));
}

#[test]
fn rerunning_import_never_double_counts() {
    let store = Store::open_in_memory().unwrap();
    let fixture = [
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "turn-1"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 20, 50, 10, 160)),
    ]
    .join("\n");

    let first = import_usage(
        &store,
        &CodexAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(first.events_imported, 1);

    let second = import_usage(
        &store,
        &CodexAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(second.events_imported, 0, "nothing new the second time");
    assert_eq!(second.duplicates_ignored, 1);

    let count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "totals never inflate on re-collection");
}

#[test]
fn wholly_unrecognized_stream_fails_loudly_for_codex_only() {
    let garbage = "{\"hello\":1}\n{\"world\":[2,3]}";
    let mut sink = Collecting::default();
    let err = CodexAdapter
        .ingest(
            &mut garbage.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap_err();
    match err {
        AiuError::UnrecognizedFormat { source, .. } => assert_eq!(source, "codex"),
        other => panic!("expected UnrecognizedFormat, got {other:?}"),
    }
    assert!(sink.events.is_empty());

    // A typed-but-unknown record among known ones is forward-compatible.
    let mixed = format!(
        "{{\"type\":\"some_future_type\",\"payload\":{{}}}}\n{}\n{}\n{}",
        session_meta("sess-1", "0.130.0"),
        turn_context("gpt-5-codex", "t"),
        token_count("2023-11-25T11:00:00.000Z", &usage(100, 0, 40, 0, 140)),
    );
    let (summary, out) = ingest(&mixed);
    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.malformed_skipped, 0);

    let (empty_summary, empty_out) = ingest("");
    assert_eq!(empty_out.events.len(), 0);
    assert_eq!(empty_summary, ParseSummary::default());
}
