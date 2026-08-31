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
    // A real machine always knows its own device id; device pruning refuses
    // to run without it.
    store.set_metadata("device_id", "dev-a").unwrap();
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

/// Device retention (issue 15). Spec §88: "Machine removal revokes sync
/// access but preserves history until retention expiry" — so a revoked
/// device's row outlives its usage, but not indefinitely.
mod devices {
    use super::*;

    fn revoked_device(store: &Store, id: &str, revoked_days_ago: u64) {
        store
            .ensure_device(&aiu::store::NewDevice {
                device_id: id.into(),
                friendly_name: id.into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                last_sync_at_utc: None,
            })
            .unwrap();
        store
            .mark_device_revoked(id, &at(revoked_days_ago * DAY))
            .unwrap();
    }

    fn live_device(store: &Store, id: &str) {
        store
            .ensure_device(&aiu::store::NewDevice {
                device_id: id.into(),
                friendly_name: id.into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                last_sync_at_utc: None,
            })
            .unwrap();
    }

    fn event_for(device_id: &str, id: &str, age_days: u64) -> NewEvent {
        NewEvent {
            device_id: device_id.into(),
            ..event(id, age_days)
        }
    }

    fn device_exists(store: &Store, id: &str) -> bool {
        store
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE device_id = ?1)",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_revoked_device_whose_history_has_expired_is_pruned() {
        let store = store_with_device();
        revoked_device(&store, "dev-gone", 400);
        store
            .record_event(&event_for("dev-gone", "ancient", 400))
            .unwrap();
        store.set_device_source("dev-gone", "claude", true).unwrap();

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(summary.usage_events, 1);
        assert_eq!(summary.devices, 1, "the device goes once its history has");
        assert!(!device_exists(&store, "dev-gone"));
        let sources: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM device_sources WHERE device_id = 'dev-gone'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sources, 0, "its source rows go with it, leaving no orphans");
    }

    #[test]
    fn a_revoked_device_with_history_still_in_window_is_kept() {
        // Reports must keep attributing usage that is still inside retention.
        let store = store_with_device();
        revoked_device(&store, "dev-recent", 400);
        store
            .record_event(&event_for("dev-recent", "still-here", 30))
            .unwrap();

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(summary.devices, 0);
        assert!(device_exists(&store, "dev-recent"));
    }

    #[test]
    fn a_recently_revoked_device_is_kept_even_with_no_history() {
        let store = store_with_device();
        revoked_device(&store, "dev-fresh-revoke", 10);

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(
            summary.devices, 0,
            "the revocation itself is still in window"
        );
        assert!(device_exists(&store, "dev-fresh-revoke"));
    }

    #[test]
    fn a_device_that_merely_went_quiet_is_never_pruned() {
        // A quiet machine may come back; only an explicit revocation retires
        // a device.
        let store = store_with_device();
        live_device(&store, "dev-quiet");
        store
            .record_event(&event_for("dev-quiet", "old", 400))
            .unwrap();

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(summary.usage_events, 1, "its history still expires");
        assert_eq!(summary.devices, 0);
        assert!(device_exists(&store, "dev-quiet"));
    }

    #[test]
    fn the_local_device_is_never_pruned() {
        let store = store_with_device();
        store.mark_device_revoked("dev-a", &at(400 * DAY)).unwrap();

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(summary.devices, 0, "this machine keeps its own row");
        assert!(device_exists(&store, "dev-a"));
    }

    #[test]
    fn a_revoked_device_holding_only_a_quota_snapshot_is_kept() {
        let store = store_with_device();
        revoked_device(&store, "dev-snap", 400);
        store
            .record_snapshot(&NewSnapshot {
                observing_device_id: "dev-snap".into(),
                ..snapshot(30, 42.0)
            })
            .unwrap();

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(
            summary.devices, 0,
            "an in-window observation is history too"
        );
        assert!(device_exists(&store, "dev-snap"));
    }

    /// The exemption protects the local device by name, so without that name
    /// it cannot protect anything. Refusing to prune is the safe direction:
    /// skipping a pass costs a day, retiring this machine's own row does not
    /// undo.
    #[test]
    fn no_device_is_pruned_when_the_local_machine_cannot_be_identified() {
        let store = Store::open_in_memory().unwrap();
        store
            .ensure_device(&aiu::store::NewDevice {
                device_id: "dev-orphan".into(),
                friendly_name: "orphan".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                last_sync_at_utc: None,
            })
            .unwrap();
        store
            .mark_device_revoked("dev-orphan", &at(400 * DAY))
            .unwrap();
        assert!(store.get_metadata("device_id").unwrap().is_none());

        let summary = retention::prune(&store, NOW).unwrap();

        assert_eq!(summary.devices, 0);
        assert!(device_exists(&store, "dev-orphan"));
    }

    #[test]
    fn device_pruning_is_idempotent() {
        let store = store_with_device();
        revoked_device(&store, "dev-gone", 400);
        let first = retention::prune(&store, NOW).unwrap();
        let second = retention::prune(&store, NOW).unwrap();
        assert_eq!(first.devices, 1);
        assert_eq!(second.devices, 0);
    }
}
