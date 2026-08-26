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
    assert!(out.contains("AI USAGE"));
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

// ---- Compact default command (issue 05) -----------------------------------

/// Extracts the text block belonging to one source, from its header line to
/// the next blank line, so per-source assertions stay scoped.
fn block<'a>(out: &'a str, source: &str) -> &'a str {
    let start = out.find(source).expect("source block present");
    let rest = &out[start..];
    match rest.find("\n\n") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

#[test]
fn compact_layout_has_ai_usage_header_top_lines_and_sync_section() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    snapshot(&store, "claude", "5h", 42.5, Some(4440));
    event(
        &store,
        "e1",
        "dev-laptop",
        "claude",
        "claude-opus-5",
        12_000,
    );

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);

    assert!(
        out.starts_with("AI USAGE\n"),
        "header leads the report: {out}"
    );
    assert!(out.contains("\nSYNC\n"), "sync section header: {out}");
    assert!(out.contains("laptop"));
    assert!(out.contains("synced 2m ago"));
    assert!(out.contains("top machine  laptop"));
    assert!(out.contains("top model    claude-opus-5"));
}

#[test]
fn never_synced_device_renders_as_unsynced_not_stale() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", None);
    snapshot(&store, "claude", "5h", 10.0, None);
    event(&store, "e1", "dev-laptop", "claude", "claude-opus-5", 5);

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);
    assert!(out.contains("never synced"), "{out}");
    assert!(!out.contains("STALE"), "never synced is not stale: {out}");

    let doc: serde_json::Value = serde_json::from_str(&report::json::render(&report)).unwrap();
    let laptop = doc["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "laptop")
        .unwrap();
    assert_eq!(laptop["stale"], false);
}

#[test]
fn three_sources_render_three_independent_blocks_with_no_merged_number() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-desk", "desktop", Some(120));

    // Claude: 5h + week.
    snapshot(&store, "claude", "5h", 42.5, Some(4440));
    snapshot(&store, "claude", "week", 12.3, None);
    event(
        &store,
        "c1",
        "dev-laptop",
        "claude",
        "claude-opus-5",
        12_000,
    );

    // Codex: 5h + week.
    snapshot(&store, "codex", "5h", 61.0, Some(2220));
    snapshot(&store, "codex", "week", 20.0, None);
    event(&store, "x1", "dev-desk", "codex", "gpt-5-codex", 8_000);

    // Go: 5h + week + month (month only for Go).
    snapshot(&store, "go", "5h", 30.0, None);
    snapshot(&store, "go", "week", 15.0, None);
    snapshot(&store, "go", "month", 5.0, None);
    event(&store, "g1", "dev-laptop", "go", "opengo-4", 2_000);

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);

    let claude = block(&out, "claude");
    assert!(claude.contains("5h") && claude.contains("week"));
    assert!(!claude.contains("month"), "claude has no month window");

    let codex = block(&out, "codex");
    assert!(codex.contains("5h") && codex.contains("week"));
    assert!(!codex.contains("month"), "codex has no month window");

    let go = block(&out, "go");
    assert!(go.contains("5h") && go.contains("week") && go.contains("month"));

    assert!(
        !out.contains("total"),
        "sources never merged into an overall number: {out}"
    );

    // JSON mirrors the same structure: three sources, each with its own windows.
    let doc: serde_json::Value = serde_json::from_str(&report::json::render(&report)).unwrap();
    let sources = doc["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3);
    let go_json = sources.iter().find(|s| s["source"] == "go").unwrap();
    assert_eq!(go_json["windows"].as_array().unwrap().len(), 3);
    assert_eq!(go_json["top_model"]["name"], "opengo-4");
}

#[test]
fn disabled_source_is_excluded_and_enabled_source_with_no_data_shows_a_block() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));

    // A source with data that the user disabled must not appear.
    store
        .set_source_mode("codex", aiu::store::SourceMode::Disabled)
        .unwrap();
    snapshot(&store, "codex", "5h", 61.0, None);
    event(&store, "x1", "dev-laptop", "codex", "gpt-5-codex", 800);

    // A source explicitly enabled but with no data yet still shows a block.
    store
        .set_source_mode("go", aiu::store::SourceMode::Enabled)
        .unwrap();

    let report = report::build(&store, NOW).unwrap();
    let out = text::render(&report);

    assert!(!out.contains("codex"), "disabled source excluded: {out}");
    assert!(out.contains("go\n"), "enabled source still appears: {out}");
    assert!(out.contains("no data yet"), "{out}");

    let doc: serde_json::Value = serde_json::from_str(&report::json::render(&report)).unwrap();
    let sources = doc["sources"].as_array().unwrap();
    assert!(!sources.iter().any(|s| s["source"] == "codex"));
    assert!(sources.iter().any(|s| s["source"] == "go"));
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

// ---- `aiu codex` detail view (issue 03) ------------------------------------

fn codex_detail_fixture() -> Store {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-desk", "desktop", Some(120));
    snapshot(&store, "codex", "5h", 61.0, Some(2220)); // resets in 37m
    snapshot(&store, "codex", "week", 20.0, None);
    event(&store, "cx1", "dev-laptop", "codex", "gpt-5-codex", 8_000);
    event(&store, "cx2", "dev-desk", "codex", "gpt-5.2-codex", 2_000);
    // Outside the 5h window but inside the week window.
    let mut old = NewEvent {
        event_id: "cx3".into(),
        workspace_id: "ws".into(),
        device_id: "dev-laptop".into(),
        source: "codex".into(),
        tool: "codex".into(),
        exact_model: "gpt-5.1-codex".into(),
        ts_utc: utc::format_epoch(NOW - 6 * 3600),
        ..Default::default()
    };
    old.output_tokens = Some(500);
    store.record_event(&old).unwrap();
    store
}

#[test]
fn codex_detail_shows_vendor_and_attribution_per_window() {
    let store = codex_detail_fixture();
    let d = detail::build(&store, "codex", NOW).unwrap();
    let out = detail::text::render(&d);

    assert!(out.contains("codex"));
    assert!(out.contains("[5h]"));
    assert!(out.contains("vendor quota: 61.0% used"), "{out}");
    assert!(out.contains("resets in 37m"), "{out}");

    let five_h = d.windows.iter().find(|w| w.window == "5h").unwrap();
    assert_eq!(five_h.models.len(), 2, "5h excludes the 6h-old event");
    assert_eq!(five_h.machines.len(), 2);
    let week = d.windows.iter().find(|w| w.window == "week").unwrap();
    assert_eq!(week.models.len(), 3, "week includes the gpt-5.1 event");
}

#[test]
fn codex_detail_json_shape_matches_text_semantics() {
    use serde_json::Value;
    let store = codex_detail_fixture();
    let d = detail::build(&store, "codex", NOW).unwrap();
    let doc: Value = serde_json::from_str(&detail::json::render(&d)).expect("valid JSON");

    assert_eq!(doc["source"], "codex");
    let five_h = doc["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["window"] == "5h")
        .unwrap();
    assert_eq!(five_h["vendor"]["used_percent"], 61.0);
    assert_eq!(five_h["vendor"]["resets_in_secs"], 2220);
    assert_eq!(five_h["attribution"]["total_output_tokens"], 10_000);
    let models = five_h["attribution"]["models"].as_array().unwrap();
    assert_eq!(models[0]["name"], "gpt-5-codex");
    assert_eq!(models[0]["share_percent"], 80.0);
}

#[test]
fn claude_and_codex_remain_independent_accounting_domains() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    snapshot(&store, "claude", "5h", 42.5, Some(4440));
    snapshot(&store, "codex", "week", 20.0, None);
    event(
        &store,
        "cl1",
        "dev-laptop",
        "claude",
        "claude-opus-5",
        12_000,
    );
    event(&store, "cx1", "dev-laptop", "codex", "gpt-5-codex", 800);

    let r = report::build(&store, NOW).unwrap();
    let claude = r.sources.iter().find(|s| s.source == "claude").unwrap();
    let codex = r.sources.iter().find(|s| s.source == "codex").unwrap();

    assert!(claude.windows.iter().any(|w| w.window == "5h"));
    assert!(!claude.windows.iter().any(|w| w.window == "week"));
    assert_eq!(claude.top_model.as_ref().unwrap().name, "claude-opus-5");
    assert_eq!(codex.top_model.as_ref().unwrap().name, "gpt-5-codex");
    assert_eq!(
        codex
            .windows
            .iter()
            .find(|w| w.window == "week")
            .unwrap()
            .used_percent,
        20.0
    );
    assert!(!codex.windows.iter().any(|w| w.window == "5h"));
}

// ---- `aiu <source> models` / `machines` breakdowns (issue 06) ---------------

use aiu::report::breakdown;

/// Multi-machine, multi-model, multi-window fixture with clean percentages:
/// within the 5h window, laptop (80%) and desktop (20%) split opus-5 (70%)
/// and sonnet-4 (30%) across four cells summing to 100%.
fn matrix_fixture() -> Store {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    device(&store, "dev-desk", "desktop", Some(120));
    device(&store, "dev-box", "boxonly", Some(120)); // never touches claude

    snapshot(&store, "claude", "5h", 42.5, Some(4440));
    snapshot(&store, "claude", "week", 12.3, None);

    event(&store, "m1", "dev-laptop", "claude", "claude-opus-5", 6_000);
    event(
        &store,
        "m2",
        "dev-laptop",
        "claude",
        "claude-sonnet-4",
        2_000,
    );
    event(&store, "m3", "dev-desk", "claude", "claude-opus-5", 1_000);
    event(&store, "m4", "dev-desk", "claude", "claude-sonnet-4", 1_000);

    // Zero-use model: hidden entirely.
    event(&store, "m5", "dev-laptop", "claude", "claude-haiku-4", 0);

    // Non-participating machine: go only, absent from claude's matrix.
    event(&store, "m6", "dev-box", "go", "opengo-4", 400);

    // Outside the 5h window but inside the week window.
    let mut old = NewEvent {
        event_id: "m7".into(),
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
fn matrix_rows_columns_and_cells_each_sum_to_the_whole_window() {
    let store = matrix_fixture();
    let b = breakdown::build(&store, "claude", NOW).unwrap();
    let five_h = b.windows.iter().find(|w| w.window == "5h").unwrap();
    let m = &five_h.matrix;

    assert_eq!(
        m.models,
        vec!["claude-opus-5".to_string(), "claude-sonnet-4".to_string()]
    );
    assert_eq!(
        m.machines,
        vec!["laptop".to_string(), "desktop".to_string()]
    );
    assert_eq!(m.grand_total(), 10_000);

    // Row totals equal model share, column totals equal machine share, and
    // every cell plus the two margins each sum to exactly 100%.
    let grand = m.grand_total() as f64;
    let row_sum: f64 = (0..m.models.len())
        .map(|i| m.model_total(i) as f64 / grand * 100.0)
        .sum();
    let col_sum: f64 = (0..m.machines.len())
        .map(|j| m.machine_total(j) as f64 / grand * 100.0)
        .sum();
    let cell_sum: f64 = m
        .cells
        .iter()
        .flatten()
        .map(|c| *c as f64 / grand * 100.0)
        .sum();
    assert!(
        (row_sum - 100.0).abs() < 1e-9,
        "rows sum to 100%: {row_sum}"
    );
    assert!(
        (col_sum - 100.0).abs() < 1e-9,
        "columns sum to 100%: {col_sum}"
    );
    assert!(
        (cell_sum - 100.0).abs() < 1e-9,
        "cells sum to 100%: {cell_sum}"
    );

    // Spot-check exact shares: laptop 80%, opus-5 70%, laptop×opus-5 60%.
    assert_eq!(m.model_total(0), 7_000);
    assert_eq!(m.machine_total(0), 8_000);
    assert_eq!(m.cells[0][0], 6_000);
}

#[test]
fn matrix_hides_zero_use_models_and_non_participating_machines() {
    let store = matrix_fixture();
    let b = breakdown::build(&store, "claude", NOW).unwrap();
    let five_h = b.windows.iter().find(|w| w.window == "5h").unwrap();
    let m = &five_h.matrix;

    assert!(
        !m.models.iter().any(|x| x == "claude-haiku-4"),
        "zero-use model hidden entirely"
    );
    assert!(
        !m.machines.iter().any(|x| x == "boxonly"),
        "non-participating machine hidden entirely"
    );
}

#[test]
fn breakdown_filters_to_the_exact_window_shown() {
    let store = matrix_fixture();
    let b = breakdown::build(&store, "claude", NOW).unwrap();
    let five_h = b.windows.iter().find(|w| w.window == "5h").unwrap();
    let week = b.windows.iter().find(|w| w.window == "week").unwrap();

    assert_eq!(five_h.matrix.grand_total(), 10_000);
    assert!(
        !five_h.matrix.models.iter().any(|m| m == "claude-opus-4.8"),
        "5h excludes the 6h-old event"
    );
    assert_eq!(week.matrix.grand_total(), 10_500);
    assert!(week.matrix.models.iter().any(|m| m == "claude-opus-4.8"));
}

#[test]
fn models_and_machines_render_text_for_the_window() {
    let store = matrix_fixture();
    let b = breakdown::build(&store, "claude", NOW).unwrap();

    let models_out = breakdown::text::render_models(&b);
    assert!(
        models_out.contains("machine × model matrix"),
        "{models_out}"
    );
    assert!(models_out.contains("claude-opus-5"));
    assert!(
        models_out.contains("60.0%"),
        "laptop×opus-5 cell: {models_out}"
    );

    let machines_out = breakdown::text::render_machines(&b);
    assert!(machines_out.contains("laptop"), "{machines_out}");
    assert!(
        machines_out.contains("80.0%"),
        "laptop machine share: {machines_out}"
    );
    assert!(
        machines_out.contains("75.0%"),
        "laptop's per-machine opus-5 share: {machines_out}"
    );
    assert!(
        !machines_out.contains("boxonly"),
        "non-participating machine absent"
    );
}

#[test]
fn breakdown_json_emits_the_full_matrix_structurally() {
    let store = matrix_fixture();
    let b = breakdown::build(&store, "claude", NOW).unwrap();

    let doc: serde_json::Value =
        serde_json::from_str(&breakdown::json::render_matrix(&b)).expect("valid JSON");
    assert_eq!(doc["source"], "claude");
    let five_h = doc["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["window"] == "5h")
        .unwrap();
    let matrix = &five_h["matrix"];
    assert_eq!(matrix["models"].as_array().unwrap().len(), 2);
    assert_eq!(matrix["machines"].as_array().unwrap().len(), 2);
    assert_eq!(matrix["machine_ids"].as_array().unwrap().len(), 2);
    assert_eq!(matrix["grand_total"], 10_000);
    let cells = matrix["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].as_array().unwrap().len(), 2);
    assert_eq!(matrix["model_totals"][0], 7_000);
    assert_eq!(matrix["machine_totals"][0], 8_000);
    assert_eq!(matrix["model_shares"][0], 70.0);
    assert_eq!(matrix["machine_shares"][0], 80.0);

    // `machines --json` carries the per-machine model list.
    let mdoc: serde_json::Value =
        serde_json::from_str(&breakdown::json::render_machines(&b)).expect("valid JSON");
    let m5h = mdoc["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["window"] == "5h")
        .unwrap();
    let machines = m5h["machines"].as_array().unwrap();
    assert_eq!(machines[0]["name"], "laptop");
    assert_eq!(machines[0]["share_percent"], 80.0);
    assert_eq!(machines[0]["models"].as_array().unwrap().len(), 2);
}

#[test]
fn every_source_has_working_models_and_machines_breakdowns() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    snapshot(&store, "codex", "5h", 61.0, Some(2220));
    snapshot(&store, "go", "5h", 30.0, None);
    event(&store, "x1", "dev-laptop", "codex", "gpt-5-codex", 8_000);
    event(&store, "g1", "dev-laptop", "go", "opengo-4", 2_000);

    for source in ["codex", "go"] {
        let b = breakdown::build(&store, source, NOW).unwrap();
        let five_h = b.windows.iter().find(|w| w.window == "5h").unwrap();
        assert!(!five_h.matrix.is_empty(), "{source} matrix populated");
        let models = breakdown::text::render_models(&b);
        let machines = breakdown::text::render_machines(&b);
        assert!(models.contains(source), "{source} models header");
        assert!(machines.contains("laptop"), "{source} machines list");
    }
}

#[test]
fn breakdown_without_vendor_snapshot_renders_explicit_gap() {
    let store = harness();
    device(&store, "dev-laptop", "laptop", Some(120));
    event(&store, "g1", "dev-laptop", "go", "opengo-4", 100);

    let b = breakdown::build(&store, "go", NOW).unwrap();
    assert_eq!(b.has_usage, true);
    assert_eq!(b.windows.len(), 0);
    let out = breakdown::text::render_models(&b);
    assert!(out.contains("no vendor snapshot yet"), "{out}");
}

#[test]
fn breakdown_empty_store_prints_init_hint_not_zero() {
    let store = harness();
    let b = breakdown::build(&store, "claude", NOW).unwrap();
    let out = breakdown::text::render_machines(&b);
    assert!(out.contains("no usage recorded yet"), "{out}");
    assert!(out.contains("aiu init"));
}

#[test]
fn two_machines_sharing_a_name_stay_separate_columns() {
    let store = harness();
    device(&store, "dev-a", "laptop", Some(120));
    device(&store, "dev-b", "laptop", Some(120)); // same friendly name
    store
        .record_snapshot(&NewSnapshot {
            source: "claude".into(),
            window: "5h".into(),
            used_percent: 42.5,
            resets_at_utc: None,
            observed_at_utc: utc::format_epoch(NOW - 60),
            observing_device_id: "dev-a".into(),
        })
        .unwrap();
    event(&store, "a1", "dev-a", "claude", "claude-opus-5", 300);
    event(&store, "b1", "dev-b", "claude", "claude-opus-5", 700);

    let b = breakdown::build(&store, "claude", NOW).unwrap();
    let m = &b.windows.iter().find(|w| w.window == "5h").unwrap().matrix;

    // Attribution is per device, not per name: two columns, one per device.
    assert_eq!(m.machines.len(), 2, "same-named devices kept apart");
    assert_eq!(m.machine_ids.len(), 2);
    assert_eq!(m.grand_total(), 1_000);
    assert_eq!(m.machine_total(0) + m.machine_total(1), 1_000);

    // The disambiguating device id surfaces in the machines JSON.
    let doc: serde_json::Value =
        serde_json::from_str(&breakdown::json::render_machines(&b)).unwrap();
    let machines = doc["windows"][0]["machines"].as_array().unwrap();
    let ids: Vec<&str> = machines
        .iter()
        .map(|x| x["device_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"dev-a"));
    assert!(ids.contains(&"dev-b"));
}
