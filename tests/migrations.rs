//! Schema migration acceptance tests (issue 01):
//! - migrations apply cleanly from an empty database
//! - re-running them is idempotent
//! - persisted timestamp defaults are UTC

use aiu::migrations::MIGRATIONS;
use aiu::store::{self, Store};
use aiu::utc;

#[test]
fn migrations_apply_from_empty_and_are_idempotent() {
    let store = Store::open_in_memory().unwrap();
    let conn = store.conn();

    assert_eq!(
        store::schema_version(conn).unwrap(),
        MIGRATIONS.len() as i64
    );

    // Core entities exist after a clean apply.
    for table in [
        "devices",
        "source_config",
        "usage_events",
        "quota_snapshots",
        "sync_outbox",
        "sync_applied_records",
        "sync_cursors",
        "adapter_state",
        "metadata",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                rusqlite::params![table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "table {table} should exist");
    }
}

#[test]
fn sync_migration_preserves_an_existing_outbox() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for (index, migration) in MIGRATIONS.iter().take(2).enumerate() {
        conn.execute_batch(&format!(
            "BEGIN; {migration} PRAGMA user_version = {}; COMMIT;",
            index + 1
        ))
        .unwrap();
    }
    conn.execute(
        "INSERT INTO sync_outbox (record_kind, payload) VALUES ('usage_event', x'010203')",
        [],
    )
    .unwrap();

    store::apply_migrations(&conn).unwrap();

    let row: (String, String, Vec<u8>, Option<String>) = conn
        .query_row(
            "SELECT record_id, record_kind, payload, sent_at_utc FROM sync_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, "legacy:1");
    assert_eq!(row.1, "usage_event");
    assert_eq!(row.2, vec![1, 2, 3]);
    assert_eq!(row.3, None);
}

#[test]
fn rerunning_migrations_on_a_populated_database_changes_nothing() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    store::apply_migrations(&conn).unwrap();

    conn.execute(
        "INSERT INTO devices (device_id, friendly_name) VALUES ('d1', 'laptop')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO metadata (key, value) VALUES ('k', 'v')", [])
        .unwrap();

    store::apply_migrations(&conn).unwrap();
    store::apply_migrations(&conn).unwrap();

    assert_eq!(
        store::schema_version(&conn).unwrap(),
        MIGRATIONS.len() as i64
    );
    let devices: i64 = conn
        .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
        .unwrap();
    let meta: i64 = conn
        .query_row("SELECT COUNT(*) FROM metadata", [], |r| r.get(0))
        .unwrap();
    assert_eq!((devices, meta), (1, 1), "re-run must not duplicate rows");
}

#[test]
fn timestamp_defaults_are_utc_rfc3339() {
    let store = Store::open_in_memory().unwrap();
    let conn = store.conn();

    // Insert relying on DEFAULT timestamps; nothing passes a clock value.
    conn.execute(
        "INSERT INTO devices (device_id, friendly_name) VALUES ('d1', 'laptop')",
        [],
    )
    .unwrap();

    let created: String = conn
        .query_row(
            "SELECT created_at_utc FROM devices WHERE device_id='d1'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();

    // Strict UTC RFC 3339: YYYY-MM-DDTHH:MM:SSZ, no offset, no local time.
    let parsed = utc::parse_rfc3339_utc(&created).expect("default must be UTC RFC 3339");
    let now = utc::now_epoch();
    assert!(
        parsed.saturating_sub(now) < 300 && now.saturating_sub(parsed) < 300,
        "default timestamp should be ~now in UTC (got {created})"
    );
}

#[test]
fn fresh_database_is_created_at_the_expected_location() {
    let unique = std::process::id();
    let dir = std::env::temp_dir().join(format!("aiu-test-{unique}"));
    std::fs::remove_dir_all(&dir).ok();

    let previous = std::env::var("AIU_DATA_DIR").ok();
    std::env::set_var("AIU_DATA_DIR", &dir);
    let result = std::panic::catch_unwind(|| {
        let path = aiu::paths::db_path().expect("AIU_DATA_DIR override must resolve");
        assert_eq!(path, dir.join("usage.db"));

        let store = Store::open(&path).unwrap();
        drop(store);

        assert!(
            path.exists(),
            "opening plain `aiu` storage must create usage.db"
        );
    });
    match previous {
        Some(value) => std::env::set_var("AIU_DATA_DIR", value),
        None => std::env::remove_var("AIU_DATA_DIR"),
    }
    std::fs::remove_dir_all(&dir).ok();
    result.expect("fresh-database test panicked");
}
