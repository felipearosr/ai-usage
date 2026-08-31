//! Retention seam (issue 11): records older than 365 days are pruned
//! opportunistically across every record type, with no permanent aggregate
//! archive. The cutoff is evaluated against a caller-supplied `now`, so these
//! tests never depend on the wall clock.

use aiu::retention::{self, RETENTION_DAYS};
use aiu::store::{NewEvent, NewSnapshot, Store};
use aiu::sync::{enqueue_record, SyncRecord};

const DAY: u64 = 86_400;
/// 2024-06-01T00:00:00Z — a fixed "now" for every cutoff assertion.
const NOW: u64 = 1_717_200_000;

fn store_with_device() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_local_device(&aiu::store::NewDevice {
            device_id: "dev-a".into(),
            friendly_name: "studio".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_sync_at_utc: None,
        })
        .unwrap();
    store
}

fn at(secs_before_now: u64) -> String {
    aiu::utc::format_epoch(NOW - secs_before_now)
}

fn event(id: &str, age_days: u64) -> NewEvent {
    NewEvent {
        event_id: id.into(),
        workspace_id: "ws".into(),
        device_id: "dev-a".into(),
        source: "claude".into(),
        tool: "claude-code".into(),
        exact_model: "claude-opus-5".into(),
        session_id_hash: None,
        ts_utc: at(age_days * DAY),
        input_tokens: Some(1),
        cached_input_tokens: None,
        cache_write_tokens: None,
        output_tokens: Some(2),
        reasoning_tokens: None,
        reported_cost_micros: None,
        tool_version: None,
        adapter_version: None,
    }
}

fn snapshot(age_days: u64, used: f64) -> NewSnapshot {
    NewSnapshot {
        source: "claude".into(),
        window: "week".into(),
        used_percent: used,
        resets_at_utc: None,
        observed_at_utc: at(age_days * DAY),
        observing_device_id: "dev-a".into(),
    }
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn retention_window_is_365_days() {
    assert_eq!(RETENTION_DAYS, 365);
}

#[test]
fn usage_events_older_than_a_year_are_pruned_and_recent_ones_survive() {
    let store = store_with_device();
    store.record_event(&event("old", 400)).unwrap();
    store.record_event(&event("edge-outside", 366)).unwrap();
    store.record_event(&event("edge-inside", 364)).unwrap();
    store.record_event(&event("fresh", 1)).unwrap();

    let summary = retention::prune(&store, NOW).unwrap();

    assert_eq!(summary.usage_events, 2, "both out-of-window events pruned");
    assert_eq!(count(&store, "usage_events"), 2);
    let kept: Vec<String> = {
        let conn = store.conn();
        let mut stmt = conn
            .prepare("SELECT event_id FROM usage_events ORDER BY event_id")
            .unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    };
    assert_eq!(kept, vec!["edge-inside".to_string(), "fresh".to_string()]);
}

#[test]
fn quota_snapshots_older_than_a_year_are_pruned() {
    let store = store_with_device();
    store.record_snapshot(&snapshot(400, 10.0)).unwrap();
    store.record_snapshot(&snapshot(30, 20.0)).unwrap();

    let summary = retention::prune(&store, NOW).unwrap();

    assert_eq!(summary.quota_snapshots, 1);
    assert_eq!(count(&store, "quota_snapshots"), 1);
}

#[test]
fn sync_metadata_older_than_a_year_is_pruned() {
    let store = store_with_device();
    // A delivered outbox row and an applied-record marker, both aged out.
    store
        .conn()
        .execute(
            "INSERT INTO sync_outbox (record_id, record_kind, payload, created_at_utc, sent_at_utc)
             VALUES ('r-old', 'usage_event', X'00', ?1, ?1)",
            rusqlite::params![at(400 * DAY)],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO sync_applied_records (record_id, applied_at_utc) VALUES ('a-old', ?1)",
            rusqlite::params![at(400 * DAY)],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO sync_applied_records (record_id, applied_at_utc) VALUES ('a-new', ?1)",
            rusqlite::params![at(10 * DAY)],
        )
        .unwrap();

    let summary = retention::prune(&store, NOW).unwrap();

    assert_eq!(summary.sync_outbox, 1);
    assert_eq!(summary.sync_applied_records, 1);
    assert_eq!(
        count(&store, "sync_applied_records"),
        1,
        "recent marker kept"
    );
}

#[test]
fn a_recent_undelivered_record_is_never_pruned() {
    // Offline machines must not lose queued work: an unsent row inside the
    // retention window survives pruning even though it has no sent_at_utc.
    let store = store_with_device();
    store.record_event(&event("queued", 2)).unwrap();
    enqueue_record(
        &store,
        &SyncRecord::UsageEvent(Box::new(event("queued", 2))),
    )
    .unwrap();
    let pending_before = store.pending_sync_count().unwrap();
    assert_eq!(pending_before, 1);

    retention::prune(&store, NOW).unwrap();

    assert_eq!(
        store.pending_sync_count().unwrap(),
        1,
        "queued record survives an opportunistic prune"
    );
}

#[test]
fn pruning_an_empty_database_is_a_no_op() {
    let store = store_with_device();
    let summary = retention::prune(&store, NOW).unwrap();
    assert_eq!(summary.total(), 0);
}

#[test]
fn pruning_is_idempotent() {
    let store = store_with_device();
    store.record_event(&event("old", 400)).unwrap();
    let first = retention::prune(&store, NOW).unwrap();
    let second = retention::prune(&store, NOW).unwrap();
    assert_eq!(first.usage_events, 1);
    assert_eq!(second.total(), 0, "second pass has nothing left to prune");
}
