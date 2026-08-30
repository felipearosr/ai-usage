use aiu::report::fleet;
use aiu::store::{NewDevice, NewEvent, NewSnapshot, Store};
use aiu::utc;

const NOW: u64 = 1_700_913_600;

fn device(store: &Store, id: &str, name: &str, os: &str, age_secs: Option<u64>) {
    store
        .ensure_device(&NewDevice {
            device_id: id.into(),
            friendly_name: name.into(),
            os: os.into(),
            arch: "x86_64".into(),
            last_sync_at_utc: age_secs.map(|age| utc::format_epoch(NOW - age)),
        })
        .unwrap();
}

#[test]
fn global_machine_table_shows_os_freshness_sources_and_explicit_gaps() {
    let store = Store::open_in_memory().unwrap();
    device(&store, "device-a", "laptop", "macos", Some(8 * 60));
    device(&store, "device-b", "builder", "linux", Some(31 * 60));
    device(&store, "device-c", "newbox", "linux", None);

    store
        .record_event(&NewEvent {
            event_id: "event-a".into(),
            workspace_id: "workspace".into(),
            device_id: "device-a".into(),
            source: "claude".into(),
            tool: "claude".into(),
            exact_model: "claude-opus-5".into(),
            ts_utc: utc::format_epoch(NOW - 60),
            output_tokens: Some(10),
            ..Default::default()
        })
        .unwrap();
    store
        .record_snapshot(&NewSnapshot {
            source: "codex".into(),
            window: "5h".into(),
            used_percent: 42.0,
            resets_at_utc: None,
            observed_at_utc: utc::format_epoch(NOW - 30),
            observing_device_id: "device-a".into(),
        })
        .unwrap();

    let report = fleet::build(&store, NOW).unwrap();
    let text = fleet::render_text(&report);
    assert!(text.contains("NAME") && text.contains("OS") && text.contains("LAST SYNC"));
    assert!(text.contains("laptop") && text.contains("macos"));
    assert!(text.contains("8m ago"));
    assert!(text.contains("claude, codex"));
    assert!(text.contains("builder") && text.contains("31m ago") && text.contains("STALE"));
    assert!(text.contains("newbox") && text.contains("never synced"));
    assert!(text.contains("no tracked sources"));
    assert!(!text.contains("Unknown"));

    let json: serde_json::Value = serde_json::from_str(&fleet::render_json(&report)).unwrap();
    let machines = json["machines"].as_array().unwrap();
    assert_eq!(machines[0]["name"], "builder");
    assert_eq!(machines[0]["stale"], true);
    assert_eq!(machines[1]["last_sync_age_secs"], 480);
    assert_eq!(
        machines[1]["sources"],
        serde_json::json!(["claude", "codex"])
    );
    assert!(machines[2]["last_sync_at"].is_null());
    assert!(machines[2]["last_sync_age_secs"].is_null());
}
