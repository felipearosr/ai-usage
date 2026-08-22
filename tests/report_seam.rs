//! Report-seam test harness (issue 01, primary seam).
//!
//! Fixtures flow through the production write path (`Store::record_*`) into
//! an in-memory database, then the same `report::build` + renderer functions
//! the CLI uses produce text and JSON. Assertions target rendered output —
//! never internal call graphs.

use aiu::report::{self, text};
use aiu::store::{NewDevice, NewEvent, NewSnapshot, Store};
use aiu::utc;

/// Deterministic "now": 2023-11-25T12:00:00Z.
const NOW: u64 = 1_700_913_600;

fn harness() -> Store {
    Store::open_in_memory().unwrap()
}

fn device(store: &Store, id: &str, name: &str, synced_secs_before_now: Option<u64>) {
    store
        .ensure_device(&NewDevice {
            device_id: id.to_string(),
            friendly_name: name.to_string(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_sync_at_utc: synced_secs_before_now.map(|s| utc::format_epoch(NOW - s)),
        })
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn event(store: &Store, event_id: &str, dev: &str, source: &str, model: &str, out_tokens: i64) {
    let inserted = store
        .record_event(&NewEvent {
            event_id: event_id.into(),
            workspace_id: "ws".into(),
            device_id: dev.into(),
            source: source.into(),
            tool: source.into(),
            exact_model: model.into(),
            session_id_hash: None,
            ts_utc: utc::format_epoch(NOW - 600),
            output_tokens: Some(out_tokens),
            ..Default::default()
        })
        .unwrap();
    assert!(inserted, "fixture event {event_id} should insert");
}

fn snapshot(store: &Store, source: &str, window: &str, percent: f64, resets_in_secs: Option<i64>) {
    store
        .record_snapshot(&NewSnapshot {
            source: source.into(),
            window: window.into(),
            used_percent: percent,
            resets_at_utc: resets_in_secs.map(|s| utc::format_epoch((NOW as i64 + s) as u64)),
            observed_at_utc: utc::format_epoch(NOW - 60),
            observing_device_id: "dev-laptop".into(),
        })
        .unwrap();
}

#[test]
fn empty_store_renders_the_empty_state_report() {
    let store = harness();
    let report = report::build(&store, NOW).unwrap();

    let out = text::render(&report);
    assert!(out.contains("aiu"));
    assert!(out.contains("No usage recorded yet."));
    assert!(out.contains("aiu init"));

    let doc = serde_json::from_str::<serde_json::Value>(&report::json::render(&report)).unwrap();
    assert_eq!(doc["sources"].as_array().unwrap().len(), 0);
    assert_eq!(doc["devices"].as_array().unwrap().len(), 0);
    assert!(
        doc["generated_at"].as_str().unwrap().ends_with('Z'),
        "generated_at must be UTC"
    );
}

#[test]
fn fixtures_render_quota_windows_top_model_and_machine() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-desk", "desktop", Some(3 * 3600));

    snapshot(&store, "claude", "5h", 42.5, Some(4440)); // resets in 1h 14m
    snapshot(&store, "claude", "week", 12.3, None);
    event(
        &store,
        "e1",
        "dev-laptop",
        "claude",
        "claude-opus-5",
        12_000,
    );
    event(&store, "e2", "dev-desk", "claude", "claude-sonnet-4", 300);

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);

    assert!(out.contains("claude\n"), "source header present");
    assert!(out.contains("42.5% used"));
    assert!(out.contains("resets in 1h 14m"));
    assert!(out.contains("week") && out.contains("12.3% used"));
    assert!(out.contains("claude-opus-5"), "top exact model shown");
    assert!(
        !out.contains("claude-sonnet-4"),
        "only top model appears in compact view"
    );
    assert!(out.contains("top machine  laptop"));
    assert!(
        out.contains("desktop") && out.contains("STALE"),
        ">30m silence marked"
    );
    assert!(out.contains("synced 2m ago"));

    // JSON shape from the same production path.
    let doc = serde_json::from_str::<serde_json::Value>(&report::json::render(&report)).unwrap();
    let claude = &doc["sources"][0];
    assert_eq!(claude["source"], "claude");
    assert_eq!(claude["windows"][0]["window"], "5h");
    assert_eq!(claude["windows"][0]["used_percent"], 42.5);
    assert!(claude["windows"][0]["resets_at"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    assert_eq!(claude["windows"][0]["resets_in_secs"], 4440);
    assert_eq!(claude["top_model"]["name"], "claude-opus-5");
    assert_eq!(claude["top_model"]["output_tokens"], 12_000);
    assert_eq!(claude["top_machine"]["name"], "laptop");

    let devices = doc["devices"].as_array().unwrap();
    let desktop = devices.iter().find(|d| d["name"] == "desktop").unwrap();
    assert_eq!(desktop["stale"], true);
}

#[test]
fn zero_usage_models_and_non_participating_machines_are_hidden() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-idle", "idlebox", Some(120)); // never touches claude

    event(&store, "z0", "dev-laptop", "codex", "gpt-5-codex", 0); // zero-use model
    event(&store, "z1", "dev-laptop", "codex", "gpt-5.2", 500);

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);
    assert!(out.contains("gpt-5.2"));
    assert!(
        !out.contains("gpt-5-codex"),
        "zero-usage models hidden entirely"
    );

    let doc = serde_json::from_str::<serde_json::Value>(&report::json::render(&report)).unwrap();
    let codex = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "codex")
        .unwrap();
    assert_eq!(codex["top_model"]["name"], "gpt-5.2");
}

#[test]
fn duplicate_events_do_not_double_count() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    event(&store, "same-id", "dev-laptop", "go", "gpt-opengo-4", 1000);
    let inserted_again = store
        .record_event(&NewEvent {
            event_id: "same-id".into(),
            workspace_id: "ws".into(),
            device_id: "dev-laptop".into(),
            source: "go".into(),
            tool: "go".into(),
            exact_model: "gpt-opengo-4".into(),
            ts_utc: utc::format_epoch(NOW),
            output_tokens: Some(9999),
            ..Default::default()
        })
        .unwrap();
    assert!(!inserted_again, "duplicate identity must be ignored");

    let count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn non_participating_machines_never_appear_in_a_source_breakdown() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-box", "boxonly", Some(120));

    event(&store, "c1", "dev-laptop", "claude", "claude-opus-5", 800);
    event(&store, "g1", "dev-box", "go", "opengo-4", 400);

    let report = report::build(&store, NOW).unwrap();
    let doc = serde_json::from_str::<serde_json::Value>(&report::json::render(&report)).unwrap();
    let sources = doc["sources"].as_array().unwrap();
    let claude = sources.iter().find(|s| s["source"] == "claude").unwrap();
    let go = sources.iter().find(|s| s["source"] == "go").unwrap();

    assert_eq!(claude["top_machine"]["name"], "laptop");
    assert_eq!(go["top_machine"]["name"], "boxonly");
}

#[test]
fn usage_without_quota_snapshot_renders_an_explicit_gap_not_zero() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    event(&store, "e1", "dev-laptop", "codex", "gpt-5.2-codex", 700);

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);
    assert!(
        out.contains("no vendor snapshot yet"),
        "missing quota must be an explicit gap: {out}"
    );
    // JSON keeps the gap visible as an empty windows array, not a 0% row.
    let doc = serde_json::from_str::<serde_json::Value>(&report::json::render(&report)).unwrap();
    let codex = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "codex")
        .unwrap();
    assert_eq!(codex["windows"].as_array().unwrap().len(), 0);
}

#[test]
fn latest_snapshot_per_window_wins() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    snapshot(&store, "go", "5h", 10.0, None);
    snapshot(&store, "go", "5h", 55.5, None); // newer observation

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);
    assert!(out.contains("55.5% used"));
    assert!(!out.contains("10.0% used"));
}

// ---- `aiu claude` detail view (issue 02) ----------------------------------

use aiu::report::detail;

fn detail_fixture() -> Store {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-desk", "desktop", Some(120));
    snapshot(&store, "claude", "5h", 42.5, Some(4440)); // resets in 1h 14m
    snapshot(&store, "claude", "week", 12.3, None);
    // Inside the 5h window (10 minutes ago).
    event(
        &store,
        "w1",
        "dev-laptop",
        "claude",
        "claude-opus-5",
        12_000,
    );
    event(&store, "w2", "dev-desk", "claude", "claude-sonnet-4", 300);
    // Outside the 5h window but inside the week window.
    let mut old = NewEvent {
        event_id: "w3".into(),
        workspace_id: "ws".into(),
        device_id: "dev-laptop".into(),
        source: "claude".into(),
        tool: "claude-code".into(),
        exact_model: "claude-opus-4.8".into(),
        ts_utc: utc::format_epoch(NOW - 6 * 3600),
        ..Default::default()
    };
    old.output_tokens = Some(500);
    store.record_event(&old).unwrap();
    store
}

#[test]
fn claude_detail_shows_vendor_and_attribution_as_distinct_things_per_window() {
    let store = detail_fixture();
    let d = detail::build(&store, "claude", NOW).unwrap();
    let out = detail::text::render(&d);

    assert!(out.contains("[5h]"));
    assert!(out.contains("vendor quota: 42.5% used"), "{out}");
    assert!(out.contains("resets in 1h 14m"), "{out}");
    assert!(
        out.contains("aiu attribution"),
        "attribution labelled distinctly: {out}"
    );
    assert!(
        !out.contains("42.5% of usage"),
        "vendor number never blended into attribution"
    );

    // Breakdowns match the exact window shown: 5h excludes the 6h-old event.
    let five_h = d.windows.iter().find(|w| w.window == "5h").unwrap();
    assert_eq!(five_h.models.len(), 2, "opus-5 and sonnet-4 only");
    assert_eq!(five_h.machines.len(), 2);
    let week = d.windows.iter().find(|w| w.window == "week").unwrap();
    assert_eq!(week.models.len(), 3, "week includes the opus-4.8 event");
}

#[test]
fn claude_detail_machine_and_model_shares_sum_to_the_whole_window() {
    let store = detail_fixture();
    let d = detail::build(&store, "claude", NOW).unwrap();
    let five_h = d.windows.iter().find(|w| w.window == "5h").unwrap();

    let total: i64 = five_h.machines.iter().map(|s| s.output_tokens).sum();
    assert_eq!(total, 12_300);
    let laptop = five_h.machines.iter().find(|s| s.name == "laptop").unwrap();
    assert_eq!(laptop.output_tokens, 12_000);
    assert_eq!(laptop.share_percent, 97.6);
    let desk = five_h
        .machines
        .iter()
        .find(|s| s.name == "desktop")
        .unwrap();
    assert_eq!(desk.share_percent, 2.4);

    let sum: f64 = five_h.machines.iter().map(|s| s.share_percent).sum();
    assert!(
        (sum - 100.0).abs() < 0.2,
        "shares sum to the whole window: {sum}"
    );

    // Exact model identifiers stay distinct — no family collapsing.
    let names: Vec<&str> = five_h.models.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["claude-opus-5", "claude-sonnet-4"]);
    assert!(
        names.contains(&"claude-sonnet-4") && !names.contains(&"sonnet"),
        "exact ids verbatim"
    );
}

#[test]
fn claude_detail_json_shape_matches_text_semantics() {
    use serde_json::Value;
    let store = detail_fixture();
    let d = detail::build(&store, "claude", NOW).unwrap();
    let doc: Value = serde_json::from_str(&detail::json::render(&d)).expect("valid JSON");

    assert_eq!(doc["source"], "claude");
    let windows = doc["windows"].as_array().unwrap();
    let five_h = windows.iter().find(|w| w["window"] == "5h").unwrap();
    assert_eq!(five_h["vendor"]["used_percent"], 42.5);
    assert_eq!(five_h["vendor"]["resets_in_secs"], 4440);
    assert_eq!(
        five_h["attribution"]["machines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["name"] == "laptop")
            .count(),
        1
    );
    assert_eq!(five_h["attribution"]["total_output_tokens"], 12_300);

    let models = five_h["attribution"]["models"].as_array().unwrap();
    assert_eq!(models[0]["name"], "claude-opus-5");
    assert_eq!(models[0]["share_percent"], 97.6);
}

#[test]
fn claude_detail_without_vendor_snapshot_renders_explicit_gap() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    event(&store, "g1", "dev-laptop", "claude", "claude-opus-5", 100);

    let d = detail::build(&store, "claude", NOW).unwrap();
    let out = detail::text::render(&d);
    assert!(
        out.contains("no vendor snapshot yet"),
        "explicit gap, never zero: {out}"
    );

    let doc: serde_json::Value = serde_json::from_str(&detail::json::render(&d)).unwrap();
    assert_eq!(doc["has_usage"], true);
    assert_eq!(doc["windows"].as_array().unwrap().len(), 0);
}

#[test]
fn claude_detail_empty_store_prints_init_hint_not_zero() {
    let store = harness();
    let d = detail::build(&store, "claude", NOW).unwrap();
    let out = detail::text::render(&d);
    assert!(out.contains("no usage recorded yet"));
    assert!(out.contains("aiu init"));
}
