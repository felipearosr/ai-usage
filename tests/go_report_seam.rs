//! Report-seam tests for the OpenCode Go source (issue 04, primary seam).
//!
//! Go events + quota snapshots flow through the production write path into an
//! in-memory store, then the same `report::build` / `detail::build` +
//! renderer functions the CLI uses. Assertions target rendered output and
//! structures: the monthly window renders for Go and only Go, windows stay in
//! canonical 5h/week/month order, and Go-attributed models stay inside the Go
//! domain rather than leaking into Claude/Codex statistics.

use aiu::report::{self, detail};
use aiu::store::{NewDevice, NewEvent, NewSnapshot, Store};
use aiu::utc;

/// Deterministic "now": 2023-11-25T12:00:00Z.
const NOW: u64 = 1_700_913_600;

fn harness() -> Store {
    Store::open_in_memory().unwrap()
}

fn device(store: &Store, id: &str, name: &str) {
    store
        .ensure_device(&NewDevice {
            device_id: id.to_string(),
            friendly_name: name.to_string(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_sync_at_utc: None,
        })
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn event(store: &Store, event_id: &str, dev: &str, source: &str, model: &str, out_tokens: i64) {
    store
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
}

fn snapshot(store: &Store, source: &str, window: &str, percent: f64) {
    store
        .record_snapshot(&NewSnapshot {
            source: source.into(),
            window: window.into(),
            used_percent: percent,
            resets_at_utc: None,
            observed_at_utc: utc::format_epoch(NOW - 60),
            observing_device_id: "dev-laptop".into(),
        })
        .unwrap();
}

#[test]
fn month_window_renders_for_go_and_only_go_in_canonical_order() {
    let store = harness();
    device(&store, "dev-laptop", "laptop");

    // Go reports all three windows.
    snapshot(&store, "go", "5h", 42.5);
    snapshot(&store, "go", "week", 12.3);
    snapshot(&store, "go", "month", 4.1);
    // Claude reports only 5h + week; no month exists for it.
    snapshot(&store, "claude", "5h", 60.0);
    snapshot(&store, "claude", "week", 20.0);

    let report = report::build(&store, NOW).unwrap();

    let go = report.sources.iter().find(|s| s.source == "go").unwrap();
    let go_windows: Vec<&str> = go.windows.iter().map(|w| w.window.as_str()).collect();
    assert_eq!(go_windows, ["5h", "week", "month"]);

    let claude = report
        .sources
        .iter()
        .find(|s| s.source == "claude")
        .unwrap();
    let claude_windows: Vec<&str> = claude.windows.iter().map(|w| w.window.as_str()).collect();
    assert_eq!(
        claude_windows,
        ["5h", "week"],
        "claude window set unchanged"
    );
}

#[test]
fn go_detail_view_renders_month_with_attribution() {
    let store = harness();
    device(&store, "dev-laptop", "laptop");

    snapshot(&store, "go", "5h", 42.5);
    snapshot(&store, "go", "week", 12.3);
    snapshot(&store, "go", "month", 4.1);
    // Go models that look like other vendors stay in the go domain.
    event(&store, "go-1", "dev-laptop", "go", "gpt-5.6-luna", 1_500);
    event(&store, "go-2", "dev-laptop", "go", "glm-5.3", 500);

    let detail = detail::build(&store, "go", NOW).unwrap();
    let text = detail::text::render(&detail);

    assert_eq!(detail.windows.len(), 3);
    assert_eq!(detail.windows[2].window, "month");

    // Canonical ordering: 5h appears before week before month.
    let five = text.find("[5h]").unwrap();
    let week = text.find("[week]").unwrap();
    let month = text.find("[month]").unwrap();
    assert!(
        five < week && week < month,
        "5h < week < month in render: {text}"
    );

    assert!(text.contains("[month]"));
    assert!(
        text.contains("vendor quota: 4.1% used"),
        "month quota shown"
    );
    assert!(
        text.contains("gpt-5.6-luna"),
        "go model attributed under go"
    );

    // JSON shape from the same production path.
    let doc = serde_json::from_str::<serde_json::Value>(&detail::json::render(&detail)).unwrap();
    let windows = doc["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0]["window"], "5h");
    assert_eq!(windows[1]["window"], "week");
    assert_eq!(windows[2]["window"], "month");
    assert_eq!(windows[2]["vendor"]["used_percent"], 4.1);
    assert!(
        windows[2]["vendor"]["resets_at"].is_null(),
        "absent reset is null, not zero"
    );
}

#[test]
fn go_usage_never_leaks_into_claude_or_codex_statistics() {
    let store = harness();
    device(&store, "dev-laptop", "laptop");

    // A Go model routed through the Go gateway.
    event(&store, "go-1", "dev-laptop", "go", "gpt-5.6-luna", 1_500);
    // A genuine Claude event for contrast.
    event(&store, "cl-1", "dev-laptop", "claude", "claude-opus-5", 800);
    snapshot(&store, "go", "5h", 10.0);
    snapshot(&store, "claude", "5h", 20.0);

    let report = report::build(&store, NOW).unwrap();

    let go = report.sources.iter().find(|s| s.source == "go").unwrap();
    assert_eq!(go.top_model.as_ref().unwrap().name, "gpt-5.6-luna");

    let claude = report
        .sources
        .iter()
        .find(|s| s.source == "claude")
        .unwrap();
    assert_eq!(claude.top_model.as_ref().unwrap().name, "claude-opus-5");

    // The Go model never appears under the Claude accounting domain.
    let text = report::text::render(&report);
    let go_block_start = text.find("go\n").unwrap();
    let claude_block_start = text.find("claude\n").unwrap();
    assert!(text[go_block_start..].contains("gpt-5.6-luna"));
    assert!(!text[claude_block_start..go_block_start + 1].contains("gpt-5.6-luna"));
}

#[test]
fn go_quota_reset_replaces_the_earlier_observation() {
    let store = harness();
    device(&store, "dev-laptop", "laptop");

    // A month observation near exhaustion, then a later one after reset: the
    // report shows the latest observation, never the stale near-zero or the
    // pre-reset near-100.
    store
        .record_snapshot(&NewSnapshot {
            source: "go".into(),
            window: "month".into(),
            used_percent: 90.0,
            resets_at_utc: None,
            observed_at_utc: utc::format_epoch(NOW - 3600),
            observing_device_id: "dev-laptop".into(),
        })
        .unwrap();
    store
        .record_snapshot(&NewSnapshot {
            source: "go".into(),
            window: "month".into(),
            used_percent: 2.5,
            resets_at_utc: Some(utc::format_epoch(NOW + 86_400)),
            observed_at_utc: utc::format_epoch(NOW - 60),
            observing_device_id: "dev-laptop".into(),
        })
        .unwrap();

    let detail = detail::build(&store, "go", NOW).unwrap();
    let month = detail.windows.iter().find(|w| w.window == "month").unwrap();
    assert_eq!(month.vendor.as_ref().unwrap().used_percent, 2.5);
    assert_eq!(
        month.vendor.as_ref().unwrap().resets_in_secs(NOW),
        Some(86_400),
        "post-reset observation wins with its own reset time"
    );
}
