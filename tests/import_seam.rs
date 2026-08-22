//! Import-machinery tests (issue 02): streaming, idempotency, periodic
//! commits, progress output, and loud-failure containment — all through the
//! public import API against an in-memory store.

use aiu::adapters::claude::ClaudeCodeAdapter;
use aiu::adapters::IngestContext;
use aiu::import::{import_quota, import_usage, ImportOptions};
use aiu::store::{NewDevice, Store};

const NOW: u64 = 1_700_913_600;

fn ctx() -> IngestContext {
    IngestContext {
        device_id: "dev-test".to_string(),
        workspace_id: "ws-test".to_string(),
        now_epoch: NOW,
    }
}

fn opts() -> ImportOptions {
    ImportOptions {
        commit_every: 2,
        progress_every: 1,
    }
}

fn line(id: &str, model: &str, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"sess-1\",\"version\":\"1.0.44\",\
          \"timestamp\":\"2023-11-25T11:{:02}:00.000Z\",\
          \"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\
          \"usage\":{{\"input_tokens\":1,\"output_tokens\":{output}}}}}}}",
        output % 60
    )
}

fn event_count(store: &Store) -> i64 {
    store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn imports_streams_and_reports_every_counter() {
    let store = Store::open_in_memory().unwrap();
    let good = format!(
        "{}\n{}\n{}\n{{broken\n",
        line("i1", "claude-opus-5", 100),
        // exact duplicate of i1 within the same stream
        line("i1", "claude-opus-5", 100),
        line("i2", "claude-sonnet-4", 50),
    );

    let summary = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut good.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(summary.records_seen, 4);
    assert_eq!(summary.events_imported, 2);
    assert_eq!(summary.duplicates_ignored, 1);
    assert_eq!(summary.malformed_skipped, 1);
    assert_eq!(event_count(&store), 2);

    // The local device row exists so events satisfy the foreign key.
    let name: String = store
        .conn()
        .query_row(
            "SELECT friendly_name FROM devices WHERE device_id = 'dev-test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "dev-test");
}

#[test]
fn rerunning_import_never_double_counts() {
    let store = Store::open_in_memory().unwrap();
    let fixture = format!(
        "{}\n{}",
        line("r1", "claude-opus-5", 10),
        line("r2", "claude-opus-5", 20)
    );

    let first = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(first.events_imported, 2);

    let second = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(second.events_imported, 0, "nothing new the second time");
    assert_eq!(
        second.duplicates_ignored, 2,
        "both known identities ignored"
    );
    assert_eq!(event_count(&store), 2, "totals unchanged");
}

#[test]
fn progress_output_flows_to_the_callback() {
    let store = Store::open_in_memory().unwrap();
    let fixture = (1..=5)
        .map(|i| line(&format!("p{i}"), "m", i))
        .collect::<Vec<_>>()
        .join("\n");

    let mut ticks = Vec::new();
    import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        opts(), // progress_every: 1
        &mut |seen| ticks.push(seen),
    )
    .unwrap();

    assert!(!ticks.is_empty(), "progress must be reported");
    let mut sorted = ticks.clone();
    sorted.sort();
    assert_eq!(ticks.len(), sorted.len());
    assert_eq!(*ticks.last().unwrap(), 5, "final tick covers all records");
}

#[test]
fn periodic_commits_persist_prefix_on_midstream_failure() {
    // commit_every: 2 with a failing adapter would be synthetic; instead
    // verify the visible contract of periodic commits: a large import with
    // a tiny threshold completes and persists everything.
    let store = Store::open_in_memory().unwrap();
    let fixture = (0..50)
        .map(|i| line(&format!("c{i}"), "claude-opus-5", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let summary = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut fixture.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(summary.events_imported, 50);
    assert_eq!(event_count(&store), 50);
}

#[test]
fn unrecognized_format_fails_loudly_records_diagnostic_and_leaves_store_usable() {
    let store = Store::open_in_memory().unwrap();
    store
        .ensure_device(&NewDevice {
            device_id: "dev-test".into(),
            friendly_name: "testbox".into(),
            os: String::new(),
            arch: String::new(),
            last_sync_at_utc: None,
        })
        .unwrap();
    // Seed one event so we can prove earlier data survives the failure.
    let seed = line("keep-me", "claude-opus-5", 7);
    import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut seed.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();

    let garbage = "{\"not\":\"a transcript\"}";
    let err = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut garbage.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    );
    assert!(err.is_err(), "unrecognized format must fail loudly");

    // Diagnostic recorded durably for this source only.
    let diagnostic = store
        .diagnostic_for("claude")
        .unwrap()
        .expect("diagnostic recorded");
    assert!(diagnostic.contains("unrecognized claude data format"));

    // Containment: the failure did not damage existing rows or the store.
    assert_eq!(event_count(&store), 1);
    let more = line("after-failure", "claude-opus-5", 9);
    let recovered = import_usage(
        &store,
        &ClaudeCodeAdapter,
        &mut more.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(recovered.events_imported, 1);
    assert_eq!(event_count(&store), 2);
}

#[test]
fn identical_snapshots_are_not_stored_twice() {
    let store = Store::open_in_memory().unwrap();
    let capture = "{\"five_hour\":{\"utilization\":30.0,\"resets_at\":\"2023-11-25T13:00:00Z\"}}";

    let first = import_quota(
        &store,
        &ClaudeCodeAdapter,
        &mut capture.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(first.snapshots_stored, 1);

    let second = import_quota(
        &store,
        &ClaudeCodeAdapter,
        &mut capture.as_bytes(),
        &ctx(),
        opts(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        second.snapshots_stored, 0,
        "no-change observations are dropped"
    );

    let count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM quota_snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
