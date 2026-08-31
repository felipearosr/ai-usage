//! Source detection + override tests (issue 07): the cheap periodic
//! detection pass, override-gated collection, and the "newly installed source
//! starts tracking without re-init" behavior — all through the public
//! `collect::collect_detected` entry point against fixture home directories.

use aiu::adapters::IngestContext;
use aiu::collect;
use aiu::store::{SourceMode, Store};
use aiu::utc;

const NOW: u64 = 1_700_913_600;

fn ctx() -> IngestContext {
    IngestContext {
        device_id: "dev-test".to_string(),
        workspace_id: "ws-test".to_string(),
        now_epoch: NOW,
    }
}

fn claude_line(id: &str, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2023-11-25T11:00:00.000Z\",\
          \"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5\",\
          \"usage\":{{\"input_tokens\":1,\"output_tokens\":{output}}}}}}}"
    )
}

fn write(path: &std::path::Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn event_count(store: &Store, source: &str) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM usage_events WHERE source = ?1",
            rusqlite::params![source],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn detection_gates_the_full_scan_and_picks_up_newly_installed_sources() {
    let store = Store::open_in_memory().unwrap();
    let home = std::env::temp_dir().join(format!("aiu-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    // Only claude is installed at "setup" time.
    write(
        &home.join(".claude/projects/p/sess.jsonl"),
        &format!("{}\n", claude_line("c1", 10)),
    );

    let first = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(event_count(&store, "claude"), 1);
    assert_eq!(event_count(&store, "codex"), 0);

    // A codex rollout appears later — no re-init, just the next periodic pass.
    let real_rollout = home.join(".codex/sessions/2026/8/26/rollout-1.jsonl");
    write(
        &real_rollout,
        &format!(
            "{}\n{}\n{}",
            "{\"timestamp\":\"2023-11-25T10:00:00.000Z\",\"type\":\"session_meta\",\
              \"payload\":{\"session_id\":\"s\",\"cli_version\":\"0.130.0\"}}",
            "{\"timestamp\":\"2023-11-25T10:00:01.000Z\",\"type\":\"turn_context\",\
              \"payload\":{\"model\":\"gpt-5-codex\",\"turn_id\":\"t\"}}",
            "{\"timestamp\":\"2023-11-25T11:00:00.000Z\",\"type\":\"event_msg\",\
              \"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":\
              {\"input_tokens\":100,\"cached_input_tokens\":0,\"output_tokens\":40,\
              \"reasoning_output_tokens\":0,\"total_tokens\":140}}}}",
        ),
    );

    let second = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert_eq!(second.len(), 2, "both sources now collected");
    assert_eq!(
        event_count(&store, "codex"),
        1,
        "codex tracked after install"
    );

    // Re-running is idempotent: nothing new.
    let third = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert_eq!(event_count(&store, "claude"), 1);
    assert_eq!(event_count(&store, "codex"), 1);
    assert!(third.iter().all(|s| s.events_imported == 0));

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn disabled_source_is_not_collected_even_when_present() {
    let store = Store::open_in_memory().unwrap();
    let home = std::env::temp_dir().join(format!("aiu-dis-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    write(
        &home.join(".claude/projects/p/sess.jsonl"),
        &format!("{}\n", claude_line("c1", 10)),
    );
    store
        .set_source_mode("claude", SourceMode::Disabled)
        .unwrap();

    let results = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert!(results.is_empty(), "disabled source skipped");
    assert_eq!(event_count(&store, "claude"), 0);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn enabled_source_is_attempted_even_when_undetected() {
    let store = Store::open_in_memory().unwrap();
    let home = std::env::temp_dir().join(format!("aiu-en-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    // No claude directory at all, but the override forces an attempt. The
    // attempt finds no files and succeeds vacuously (never an error).
    store
        .set_source_mode("claude", SourceMode::Enabled)
        .unwrap();

    let results = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert!(results.is_empty(), "no files to collect, but no failure");
    assert_eq!(event_count(&store, "claude"), 0);
    assert_eq!(store.device_sources("dev-test").unwrap(), vec!["claude"]);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn auto_follows_detection_for_every_source() {
    let store = Store::open_in_memory().unwrap();
    let home = std::env::temp_dir().join(format!("aiu-auto-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);

    // Nothing installed: auto mode collects nothing.
    assert!(collect::collect_detected(&store, &home, &ctx())
        .unwrap()
        .is_empty());

    // Install claude only: auto mode now collects claude and nothing else.
    write(
        &home.join(".claude/projects/p/sess.jsonl"),
        &format!("{}\n", claude_line("c1", 5)),
    );
    let results = collect::collect_detected(&store, &home, &ctx()).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(event_count(&store, "claude"), 1);
    assert_eq!(event_count(&store, "codex"), 0);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn source_mode_defaults_to_auto_when_unset() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.source_mode("claude").unwrap(), SourceMode::Auto);
    store
        .set_source_mode("claude", SourceMode::Enabled)
        .unwrap();
    assert_eq!(store.source_mode("claude").unwrap(), SourceMode::Enabled);
    store.set_source_mode("claude", SourceMode::Auto).unwrap();
    assert_eq!(store.source_mode("claude").unwrap(), SourceMode::Auto);
}

#[test]
fn override_changes_report_membership_immediately() {
    let store = Store::open_in_memory().unwrap();
    store
        .ensure_device(&aiu::store::NewDevice {
            device_id: "dev-test".into(),
            friendly_name: "testbox".into(),
            os: String::new(),
            arch: String::new(),
            last_sync_at_utc: None,
        })
        .unwrap();
    // A source with data, disabled after the fact.
    store
        .record_snapshot(&aiu::store::NewSnapshot {
            source: "codex".into(),
            window: "5h".into(),
            used_percent: 61.0,
            resets_at_utc: None,
            observed_at_utc: utc::format_epoch(NOW - 60),
            observing_device_id: "dev-test".into(),
        })
        .unwrap();

    assert!(store
        .configured_sources()
        .unwrap()
        .contains(&"codex".to_string()));
    store
        .set_source_mode("codex", SourceMode::Disabled)
        .unwrap();
    assert!(
        !store
            .configured_sources()
            .unwrap()
            .contains(&"codex".to_string()),
        "disabled source leaves reports immediately"
    );

    // An enabled-but-empty source joins reports immediately.
    store.set_source_mode("go", SourceMode::Enabled).unwrap();
    assert!(store
        .configured_sources()
        .unwrap()
        .contains(&"go".to_string()));
}
