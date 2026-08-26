//! Adapter-seam tests for the Claude Code adapter (issue 02, seam 2).
//!
//! Fixtures recorded in the shape of real Claude Code persistence are
//! streamed through `ClaudeCodeAdapter` and assertions target the emitted
//! normalized events + quota snapshots — never internal call graphs.
//! Mandated coverage: normal sessions, streamed responses, duplicate
//! records, truncated records, restart/resume, mid-session model switching,
//! concurrent sessions, quota reset.

use aiu::adapters::claude::ClaudeCodeAdapter;
use aiu::adapters::{EventSink, IngestContext, ParseSummary, SourceAdapter};
use aiu::error::AiuError;
use aiu::hash::short_hash_hex;
use aiu::import::{import_quota, ImportOptions, ImportSummary};
use aiu::store::{NewEvent, NewSnapshot};

const NOW: u64 = 1_700_913_600; // 2023-11-25T12:00:00Z

fn ctx() -> IngestContext {
    IngestContext {
        device_id: "dev-test".to_string(),
        workspace_id: "ws-test".to_string(),
        now_epoch: NOW,
    }
}

/// Collecting sink: adapter output is asserted directly on these vectors.
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
    let summary = ClaudeCodeAdapter
        .ingest(
            &mut input.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap();
    (summary, sink)
}

/// Builds one transcript line the way Claude Code writes assistant entries.
#[allow(clippy::too_many_arguments)]
fn assistant_line(
    message_id: &str,
    session_id: &str,
    model: &str,
    ts: &str,
    input: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    output: i64,
    version: &str,
) -> String {
    let usage_field = |name: &str, value: Option<i64>| match value {
        Some(v) => format!("\"{name}\":{v}"),
        None => format!("\"{name}\":null"),
    };
    // Field order mirrors real records; cwd/paths exist but must be ignored.
    format!(
        "{{\"parentUuid\":\"u1\",\"isSidechain\":false,\"userType\":\"external\",\
         \"cwd\":\"/home/user/secret-project\",\"sessionId\":\"{session_id}\",\
         \"version\":\"{version}\",\"gitBranch\":\"main\",\"type\":\"assistant\",\
         \"uuid\":\"uuid-{message_id}-{output}\",\"timestamp\":\"{ts}\",\
         \"requestId\":\"req-{message_id}\",\
         \"message\":{{\"id\":\"{message_id}\",\"model\":\"{model}\",\"role\":\"assistant\",\
         \"usage\":{{{},{},{},{}}}}}}}",
        usage_field("input_tokens", input),
        usage_field("cache_creation_input_tokens", cache_write),
        usage_field("cache_read_input_tokens", cache_read),
        usage_field("output_tokens", Some(output)),
    )
}

#[test]
fn normal_session_produces_one_fully_attributed_event() {
    let fixture = [
        assistant_line("msg_a1", "sess-1", "claude-opus-4.8", "2023-11-25T11:00:00.000Z", Some(4), Some(12_000), Some(1_000), 350, "1.0.44"),
        "{\"type\":\"user\",\"sessionId\":\"sess-1\",\"message\":{\"role\":\"user\"},\"timestamp\":\"2023-11-25T10:59:59.000Z\"}".to_string(),
        "{\"type\":\"summary\",\"summary\":\"ignored\",\"leafUuid\":\"x\"}".to_string(),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);

    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.records_seen, 3);
    assert_eq!(summary.malformed_skipped, 0);
    let e = &out.events[0];
    // Raw provider token values preserved per class — never summed.
    assert_eq!(e.input_tokens, Some(4));
    assert_eq!(e.cached_input_tokens, Some(12_000));
    assert_eq!(e.cache_write_tokens, Some(1_000));
    assert_eq!(e.output_tokens, Some(350));
    assert_eq!(e.reasoning_tokens, None, "unreported class stays null");
    assert_eq!(e.reported_cost_micros, None, "cost is never guessed");
    // Exact model id preserved verbatim.
    assert_eq!(e.exact_model, "claude-opus-4.8");
    assert_eq!(e.source, "claude");
    assert_eq!(e.tool, "claude-code");
    // Session stored only as a stable hash.
    assert_eq!(
        e.session_id_hash.as_deref(),
        Some(short_hash_hex("sess-1").as_str())
    );
    // Vendor tool version stamped; adapter version stamped.
    assert_eq!(e.tool_version.as_deref(), Some("1.0.44"));
    assert_eq!(
        e.adapter_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(e.ts_utc, "2023-11-25T11:00:00Z");
    assert_eq!(e.device_id, "dev-test");
    // Deterministic identity: same fixture re-ingests to the same id.
    let (_, again) = ingest(&fixture);
    assert_eq!(again.events[0].event_id, e.event_id);
}

#[test]
fn streamed_responses_collapse_into_the_final_record() {
    // Same message id appended three times while streaming: cumulative
    // counters grow; the last observation replaces earlier ones wholesale.
    let fixture = [
        assistant_line(
            "msg_s1",
            "s",
            "claude-opus-5",
            "2023-11-25T11:00:00.000Z",
            Some(2),
            None,
            None,
            10,
            "1.0.44",
        ),
        assistant_line(
            "msg_s1",
            "s",
            "claude-opus-5",
            "2023-11-25T11:00:01.000Z",
            Some(3),
            None,
            None,
            40,
            "1.0.44",
        ),
        assistant_line(
            "msg_s1",
            "s",
            "claude-opus-5",
            "2023-11-25T11:00:02.000Z",
            Some(5),
            None,
            None,
            90,
            "1.0.44",
        ),
    ]
    .join("\n");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1, "one response = one event");
    assert_eq!(summary.streamed_snapshots_collapsed, 2);
    let e = &out.events[0];
    assert_eq!(e.output_tokens, Some(90), "final stream snapshot wins");
    assert_eq!(e.input_tokens, Some(5));
}

#[test]
fn duplicate_records_are_counted_not_double_counted() {
    let line = assistant_line(
        "msg_d1",
        "s",
        "claude-sonnet-4",
        "2023-11-25T11:00:00.000Z",
        Some(1),
        None,
        None,
        20,
        "1.0.44",
    );
    let fixture = format!("{line}\n{line}");

    let (summary, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.duplicates_skipped, 1);
    assert_eq!(out.events[0].output_tokens, Some(20));
}

#[test]
fn truncated_and_corrupt_lines_are_skipped_and_counted() {
    let good = assistant_line(
        "msg_t1",
        "s",
        "claude-opus-5",
        "2023-11-25T11:00:00.000Z",
        Some(1),
        None,
        None,
        30,
        "1.0.44",
    );
    let truncated = assistant_line(
        "msg_t2",
        "s",
        "claude-opus-5",
        "2023-11-25T11:05:00.000Z",
        Some(1),
        None,
        None,
        40,
        "1.0.44",
    )[..120]
        .to_string();
    let fixture = format!("{good}\n{truncated}\n{{not json\n\n{good}");

    let (summary, out) = ingest(&fixture);
    // The identical repeat of msg_t1 is a duplicate; two bad lines skipped.
    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.malformed_skipped, 2);
    assert_eq!(summary.duplicates_skipped, 1);
}

#[test]
fn restart_resume_reattributes_each_segment_to_its_session() {
    // After a restart the session id changes mid-file; user/system records
    // interleave and carry no accounting.
    let fixture = [
        assistant_line(
            "msg_r1",
            "sess-old",
            "claude-opus-5",
            "2023-11-25T10:00:00.000Z",
            Some(1),
            None,
            None,
            15,
            "1.0.43",
        ),
        "{\"type\":\"system\",\"subtype\":\"resume\",\"timestamp\":\"2023-11-25T11:30:00.000Z\"}"
            .to_string(),
        assistant_line(
            "msg_r2",
            "sess-new",
            "claude-sonnet-4",
            "2023-11-25T11:31:00.000Z",
            Some(2),
            None,
            None,
            25,
            "1.0.44",
        ),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 2);
    assert_eq!(
        out.events
            .iter()
            .find(|e| e.exact_model == "claude-opus-5")
            .unwrap()
            .session_id_hash,
        Some(short_hash_hex("sess-old"))
    );
    assert_eq!(
        out.events
            .iter()
            .find(|e| e.exact_model == "claude-sonnet-4")
            .unwrap()
            .session_id_hash,
        Some(short_hash_hex("sess-new"))
    );
}

#[test]
fn mid_session_model_switch_stays_per_event_exact() {
    let fixture = [
        assistant_line(
            "msg_m1",
            "s",
            "claude-opus-5",
            "2023-11-25T11:00:00.000Z",
            Some(1),
            None,
            None,
            100,
            "1.0.44",
        ),
        assistant_line(
            "msg_m2",
            "s",
            "claude-haiku-4.5",
            "2023-11-25T11:00:30.000Z",
            Some(1),
            None,
            None,
            200,
            "1.0.44",
        ),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 2);
    let models: Vec<&str> = out.events.iter().map(|e| e.exact_model.as_str()).collect();
    assert_eq!(models, ["claude-opus-5", "claude-haiku-4.5"]);
    // Distinct ids despite shared session/device/timestamp components.
    assert_ne!(out.events[0].event_id, out.events[1].event_id);
}

#[test]
fn concurrent_sessions_interleave_without_mixing() {
    let fixture = [
        assistant_line(
            "msg_c1",
            "sess-a",
            "claude-opus-5",
            "2023-11-25T11:00:00.000Z",
            Some(1),
            None,
            None,
            10,
            "1.0.44",
        ),
        assistant_line(
            "msg_c2",
            "sess-b",
            "claude-opus-5",
            "2023-11-25T11:00:01.000Z",
            Some(1),
            None,
            None,
            20,
            "1.0.44",
        ),
        assistant_line(
            "msg_c3",
            "sess-a",
            "claude-opus-5",
            "2023-11-25T11:00:02.000Z",
            Some(1),
            None,
            None,
            30,
            "1.0.44",
        ),
    ]
    .join("\n");

    let (_, out) = ingest(&fixture);
    assert_eq!(out.events.len(), 3);
    let by_output: Vec<Option<&String>> = [10, 20, 30]
        .iter()
        .map(|tok| {
            out.events
                .iter()
                .find(|e| e.output_tokens == Some(*tok))
                .and_then(|e| e.session_id_hash.as_ref())
        })
        .collect();
    assert_eq!(
        by_output,
        vec![
            Some(&short_hash_hex("sess-a")),
            Some(&short_hash_hex("sess-b")),
            Some(&short_hash_hex("sess-a")),
        ]
    );
}

#[test]
fn missing_token_classes_stay_null_never_zero() {
    // Older records may omit cache classes entirely.
    let fixture = "{\"type\":\"assistant\",\"sessionId\":\"s\",\"version\":\"1.0.9\",\
          \"timestamp\":\"2023-11-25T11:00:00.000Z\",\
          \"message\":{\"id\":\"msg_n1\",\"model\":\"claude-sonnet-4\",\
          \"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}"
        .to_string();
    let (_, out) = ingest(&fixture);
    let e = &out.events[0];
    assert_eq!(e.input_tokens, Some(7));
    assert_eq!(e.output_tokens, Some(3));
    assert_eq!(e.cached_input_tokens, None);
    assert_eq!(e.cache_write_tokens, None);
}

#[test]
fn wrong_typed_usage_counts_as_malformed_not_guessed() {
    let fixture =
        "{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2023-11-25T11:00:00.000Z\",\
          \"message\":{\"id\":\"msg_w1\",\"model\":\"m\",\
          \"usage\":{\"input_tokens\":\"lots\",\"output_tokens\":3}}}"
            .to_string();
    let (summary, out) = ingest(&fixture);
    assert!(out.events.is_empty());
    assert_eq!(summary.malformed_skipped, 1);
}

#[test]
fn wholly_unrecognized_stream_fails_loudly_for_claude_only() {
    // Valid JSON, plausible shape, no known Claude record type anywhere.
    let garbage = "{\"hello\":1}\n{\"world\":[2,3]}";
    let mut sink = Collecting::default();
    let err = ClaudeCodeAdapter
        .ingest(
            &mut garbage.as_bytes(),
            &ctx(),
            &mut sink,
            &mut aiu::adapters::silent_progress(),
        )
        .unwrap_err();
    match err {
        AiuError::UnrecognizedFormat { source, .. } => assert_eq!(source, "claude"),
        other => panic!("expected UnrecognizedFormat, got {other:?}"),
    }
    assert!(sink.events.is_empty());

    // A typed-but-unknown record kind among known ones is forward-compatible.
    let mixed = format!(
        "{{\"type\":\"alien\",\"v\":7}}\n{}",
        assistant_line(
            "msg_x1",
            "s",
            "claude-opus-5",
            "2023-11-25T11:00:00.000Z",
            Some(1),
            None,
            None,
            5,
            "1.0.44"
        )
    );
    let (summary, out) = ingest(&mixed);
    assert_eq!(out.events.len(), 1);
    assert_eq!(summary.malformed_skipped, 0);

    // Empty input is not an error and emits nothing.
    let (empty_summary, empty_out) = ingest("");
    assert_eq!(empty_out.events.len(), 0);
    assert_eq!(empty_summary, ParseSummary::default());
}

// ---- Quota captures -------------------------------------------------------

fn quota_json(five_hour_util: f64, resets_at: &str, week_util: f64) -> String {
    format!(
        "{{\"five_hour\":{{\"utilization\":{five_hour_util},\"resets_at\":\"{resets_at}\"}},\
         \"seven_day\":{{\"utilization\":{week_util}}}}}"
    )
}

fn ingest_quota(store: &aiu::store::Store, input: &str) -> ImportSummary {
    import_quota(
        store,
        &ClaudeCodeAdapter,
        &mut input.as_bytes(),
        &ctx(),
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn quota_capture_becomes_window_snapshots_with_resets() {
    let store = aiu::store::Store::open_in_memory().unwrap();
    let summary = ingest_quota(&store, &quota_json(42.5, "2023-11-25T13:14:00Z", 12.3));

    assert_eq!(summary.snapshots_stored, 2);
    assert_eq!(summary.records_seen, 2);
    let latest = |window: &str| -> (f64, Option<String>) {
        store
            .conn()
            .query_row(
                "SELECT used_percent, resets_at_utc FROM quota_snapshots WHERE window = ?1",
                [window],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    let (pct, resets) = latest("5h");
    assert_eq!(pct, 42.5);
    assert_eq!(resets.as_deref(), Some("2023-11-25T13:14:00Z"));
    let (week_pct, week_resets) = latest("week");
    assert_eq!(week_pct, 12.3);
    assert_eq!(week_resets, None);
}

#[test]
fn quota_reset_arrives_as_new_observation_latest_wins() {
    use aiu::report;
    let store = aiu::store::Store::open_in_memory().unwrap();
    store
        .ensure_device(&aiu::store::NewDevice {
            device_id: "dev-test".into(),
            friendly_name: "testbox".into(),
            os: String::new(),
            arch: String::new(),
            last_sync_at_utc: None,
        })
        .unwrap();

    // Before reset: high utilization, countdown running.
    ingest_quota(&store, &quota_json(91.0, "2023-11-25T12:20:00Z", 40.0));
    // After the window rolls over: utilization drops, new reset time.
    let after_reset = quota_json(7.5, "2023-11-25T17:20:00Z", 41.0);
    let mut later_ctx = ctx();
    later_ctx.now_epoch = NOW + 3 * 3600;
    import_quota(
        &store,
        &ClaudeCodeAdapter,
        &mut after_reset.as_bytes(),
        &later_ctx,
        ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();

    let r = report::build(&store, NOW + 3 * 3600).unwrap();
    let claude = r.sources.iter().find(|s| s.source == "claude").unwrap();
    let five_h = claude.windows.iter().find(|w| w.window == "5h").unwrap();
    assert_eq!(five_h.used_percent, 7.5, "latest observation wins");
    // 17:20 minus now (15:00) = 2h 20m.
    assert_eq!(
        five_h.resets_in_secs(NOW + 3 * 3600),
        Some(2 * 3600 + 20 * 60)
    );
}

#[test]
fn unrecognized_quota_shape_fails_loudly() {
    let store = aiu::store::Store::open_in_memory().unwrap();
    for bad in [
        "{\"weekly\":{\"utilization\":5.0}}", // unknown window name
        "[{\"utilization\":1.0}]",            // not an object of windows
        "{\"five_hour\":{\"percent\":5.0}}",  // no utilization field
    ] {
        let result = import_quota(
            &store,
            &ClaudeCodeAdapter,
            &mut bad.as_bytes(),
            &ctx(),
            ImportOptions::default(),
            &mut |_| {},
        );
        assert!(
            matches!(
                result,
                Err(AiuError::UnrecognizedFormat {
                    source: "claude",
                    ..
                })
            ),
            "should fail loudly: {bad}"
        );
    }

    // Empty capture means "vendor reported nothing yet" — not an error.
    let summary = ingest_quota(&store, "{}");
    assert_eq!(summary.snapshots_stored, 0);
}
