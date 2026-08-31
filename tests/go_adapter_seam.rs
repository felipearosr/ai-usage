//! Adapter-seam tests for the OpenCode Go adapter (issue 04, seam 2).
//!
//! Fixtures recorded in the shape of real OpenCode message files (one JSON
//! object per file) are streamed through `OpenCodeGoAdapter` and assertions
//! target the emitted normalized events + quota snapshots — never internal
//! call graphs. Mandated coverage: Go attribution by provider id, external
//! provider exclusion, user/system messages ignored, exact-model separation,
//! epoch-millisecond timestamps, null-vs-guess discipline, cost preservation,
//! idempotency, and the monthly quota window.

use aiu::adapters::go::OpenCodeGoAdapter;
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
    let summary = OpenCodeGoAdapter
        .ingest(
            &mut input.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap();
    (summary, sink)
}

fn ingest_err(input: &str) -> AiuError {
    let mut sink = Collecting::default();
    OpenCodeGoAdapter
        .ingest(
            &mut input.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap_err()
}

/// Builds a realistic OpenCode assistant message. `tokens` is inserted
/// verbatim so tests control exactly which token classes are present.
fn assistant_message(
    id: &str,
    session_id: &str,
    model_id: &str,
    provider_id: &str,
    created_ms: i64,
    tokens: &str,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"sessionID\":\"{session_id}\",\"role\":\"assistant\",\
          \"parentID\":\"msg-user-{id}\",\"time\":{{\"created\":{created_ms},\
          \"completed\":{created_ms}}},\"modelID\":\"{model_id}\",\
          \"providerID\":\"{provider_id}\",\"agent\":\"build\",\"mode\":\"primary\",\
          \"path\":{{\"cwd\":\"/secret/project\",\"root\":\"/secret/project\"}},\
          \"cost\":0.001,\"tokens\":{tokens},\"finish\":\"stop\"}}"
    )
}

fn full_tokens() -> &'static str {
    "{\"total\":175,\"input\":100,\"output\":50,\"reasoning\":10,\
      \"cache\":{\"read\":20,\"write\":5}}"
}

#[test]
fn go_subscription_message_produces_one_fully_attributed_event() {
    let fixture = assistant_message(
        "msg-1",
        "ses-1",
        "glm-5.3",
        "opencode",
        1_700_913_600_000,
        full_tokens(),
    );

    let (summary, out) = ingest(&fixture);

    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.records_seen, 1);
    assert_eq!(summary.events_emitted, 1);
    assert_eq!(summary.malformed_skipped, 0);

    let e = &out.events[0];
    assert_eq!(e.source, "go");
    assert_eq!(e.tool, "opencode");
    assert_eq!(e.exact_model, "glm-5.3");
    // Documented normalization: cache read/write are siblings to input, so no
    // subtraction is applied — each class is preserved exactly as reported.
    assert_eq!(e.input_tokens, Some(100));
    assert_eq!(e.cached_input_tokens, Some(20));
    assert_eq!(e.cache_write_tokens, Some(5));
    assert_eq!(e.output_tokens, Some(50));
    assert_eq!(e.reasoning_tokens, Some(10));
    assert_eq!(
        e.reported_cost_micros,
        Some(1000),
        "0.001 USD -> 1000 micros"
    );
    assert_eq!(
        e.session_id_hash.as_deref(),
        Some(short_hash_hex("ses-1").as_str())
    );
    assert_eq!(e.ts_utc, "2023-11-25T12:00:00Z", "epoch ms -> UTC");
    assert_eq!(e.device_id, "dev-test");
    assert_eq!(e.workspace_id, "ws-test");
    assert_eq!(e.tool_version, None, "message files carry no CLI version");
    assert_eq!(
        e.adapter_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    // Deterministic identity: re-ingesting the same fixture yields the same id.
    let (_, again) = ingest(&fixture);
    assert_eq!(again.events[0].event_id, e.event_id);
}

#[test]
fn external_provider_messages_are_excluded_not_misattributed() {
    for provider in ["anthropic", "openai", "openrouter"] {
        let fixture = assistant_message(
            "msg-ext",
            "ses-ext",
            "claude-opus-5",
            provider,
            1_700_913_600_000,
            full_tokens(),
        );
        let (summary, out) = ingest(&fixture);
        assert!(out.events.is_empty(), "provider {provider} excluded");
        assert_eq!(summary.malformed_skipped, 0, "recognized, not malformed");
        assert_eq!(summary.events_emitted, 0);
    }
}

#[test]
fn go_models_that_look_like_other_vendors_stay_in_the_go_domain() {
    // Go serves models through OpenAI/Anthropic-shaped endpoints; the routing
    // provider id — not the model name — decides the accounting domain.
    for model in ["gpt-5.6-luna", "minimax-m3", "glm-5.3", "kimi-k3"] {
        let fixture = assistant_message(
            "msg-go",
            "ses-go",
            model,
            "opencode",
            1_700_913_600_000,
            full_tokens(),
        );
        let (_, out) = ingest(&fixture);
        assert_eq!(out.events.len(), 1, "go model {model} attributed");
        assert_eq!(out.events[0].source, "go");
        assert_eq!(out.events[0].exact_model, model);
    }

    // The same model name through a real external provider is NOT go usage.
    let external = assistant_message(
        "msg-anthropic",
        "ses-go",
        "claude-opus-5",
        "anthropic",
        1_700_913_600_000,
        full_tokens(),
    );
    let (_, out) = ingest(&external);
    assert!(
        out.events.is_empty(),
        "external provider never leaks into go"
    );
}

#[test]
fn user_and_system_messages_are_recognized_and_ignored() {
    for role in ["user", "system"] {
        let fixture = format!(
            "{{\"id\":\"msg-{role}\",\"sessionID\":\"ses-1\",\"role\":\"{role}\",\
              \"time\":{{\"created\":1700913600000}}}}"
        );
        let (summary, out) = ingest(&fixture);
        assert!(out.events.is_empty(), "{role} message produces no event");
        assert_eq!(summary.malformed_skipped, 0);
    }
}

#[test]
fn missing_required_fields_are_malformed_not_guessed() {
    // No modelID: cannot attribute.
    let no_model = assistant_message(
        "msg-1",
        "ses-1",
        "",
        "opencode",
        1_700_913_600_000,
        full_tokens(),
    )
    .replace("\"modelID\":\"\",", "");
    let (summary, out) = ingest(&no_model);
    assert!(out.events.is_empty());
    assert_eq!(summary.malformed_skipped, 1);

    // No providerID: cannot decide the domain.
    let no_provider = assistant_message(
        "msg-1",
        "ses-1",
        "glm-5.3",
        "",
        1_700_913_600_000,
        full_tokens(),
    )
    .replace("\"providerID\":\"\",", "");
    let (summary, out) = ingest(&no_provider);
    assert!(out.events.is_empty());
    assert_eq!(summary.malformed_skipped, 1);

    // Wrong-typed token value: malformed, never coerced.
    let wrong_tokens = assistant_message(
        "msg-1",
        "ses-1",
        "glm-5.3",
        "opencode",
        1_700_913_600_000,
        "{\"input\":\"lots\",\"output\":50,\"reasoning\":10,\
          \"cache\":{\"read\":20,\"write\":5}}",
    );
    let (summary, out) = ingest(&wrong_tokens);
    assert!(out.events.is_empty());
    assert_eq!(summary.malformed_skipped, 1);
}

#[test]
fn absent_token_classes_stay_null_never_zero() {
    let minimal = assistant_message(
        "msg-min",
        "ses-min",
        "glm-5.3",
        "opencode",
        1_700_913_600_000,
        "{\"input\":40,\"output\":20}",
    );
    let (summary, out) = ingest(&minimal);
    assert_eq!(out.events.len(), 1);
    let e = &out.events[0];
    assert_eq!(e.input_tokens, Some(40));
    assert_eq!(e.output_tokens, Some(20));
    assert_eq!(e.reasoning_tokens, None, "absent reasoning stays null");
    assert_eq!(e.cached_input_tokens, None, "absent cache.read stays null");
    assert_eq!(e.cache_write_tokens, None, "absent cache.write stays null");
    assert_eq!(summary.malformed_skipped, 0);
}

#[test]
fn absent_cost_stays_null_never_guessed() {
    let fixture = assistant_message(
        "msg-cost",
        "ses-cost",
        "glm-5.3",
        "opencode",
        1_700_913_600_000,
        full_tokens(),
    )
    .replace("\"cost\":0.001,", "");
    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].reported_cost_micros, None);
}

#[test]
fn quota_capture_maps_go_windows_including_month() {
    let capture = "{\"five_hour\":{\"utilization\":42.5,\
                    \"resets_at\":\"2023-11-25T17:00:00.000Z\"},\
                    \"seven_day\":{\"utilization\":12.3},\
                    \"month\":{\"utilization\":4.1,\
                    \"resets_at\":\"2023-12-01T00:00:00Z\"}}";
    let mut sink = Collecting::default();
    let summary = OpenCodeGoAdapter
        .ingest_quota(
            &mut capture.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap();

    assert_eq!(summary.snapshots_emitted, 3);
    let by_window = |w: &str| sink.snapshots.iter().find(|s| s.window == w).unwrap();
    assert_eq!(by_window("5h").used_percent, 42.5);
    assert_eq!(by_window("week").used_percent, 12.3);
    assert_eq!(by_window("month").used_percent, 4.1);
    for s in &sink.snapshots {
        assert_eq!(s.source, "go");
    }
    assert_eq!(
        by_window("5h").resets_at_utc.as_deref(),
        Some("2023-11-25T17:00:00Z"),
        "fractional seconds truncated"
    );
    assert_eq!(
        by_window("month").resets_at_utc.as_deref(),
        Some("2023-12-01T00:00:00Z"),
        "month resets"
    );
}

#[test]
fn rerunning_import_never_double_counts() {
    let store = Store::open_in_memory().unwrap();
    let fixture = assistant_message(
        "msg-1",
        "ses-1",
        "glm-5.3",
        "opencode",
        1_700_913_600_000,
        full_tokens(),
    );

    let first = import_usage(
        &store,
        &OpenCodeGoAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(first.events_imported, 1);

    let second = import_usage(
        &store,
        &OpenCodeGoAdapter,
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
fn wholly_unrecognized_file_fails_loudly_for_go_only() {
    let err = ingest_err("{\"hello\":1}");
    match err {
        AiuError::UnrecognizedFormat { source, .. } => assert_eq!(source, "go"),
        other => panic!("expected UnrecognizedFormat, got {other:?}"),
    }

    // A non-object JSON document is also unrecognized.
    let err = ingest_err("[1,2,3]");
    assert!(matches!(err, AiuError::UnrecognizedFormat { .. }));

    // Empty input is a no-op, never an error.
    let (summary, out) = ingest("");
    assert_eq!(summary, ParseSummary::default());
    assert!(out.events.is_empty());
}
