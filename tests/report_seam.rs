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
            output_tokens: out_tokens,
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
            output_tokens: 9999,
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
