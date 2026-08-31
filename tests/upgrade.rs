//! Upgrade acceptance tests (issue 12): "Upgrades preserve local db, device
//! identity, workspace keys, overrides, schedule, history; support safe DB
//! migrations."
//!
//! An upgrade is two things happening at once: a new binary replacing the old
//! one on disk (covered in `tests/installer.rs`), and that new binary opening
//! a database an older version wrote. This file owns the second half. It
//! writes a real file-backed database at an older schema version, populates
//! every kind of state an install accumulates, then opens it the way the new
//! binary does and asserts each one came back.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use aiu::migrations::MIGRATIONS;
use aiu::report::status::LAST_COLLECT_KEY;
use aiu::scheduler::{self, Platform, ScheduleSpec};
use aiu::store::{self, NewEvent, SourceMode, Store};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Records nothing and succeeds: these tests are about files on disk, and
/// must never touch the developer's real systemd or launchd state.
struct NoopRunner;

impl scheduler::CommandRunner for NoopRunner {
    fn run(&mut self, _program: &str, _args: &[String]) -> std::io::Result<()> {
        Ok(())
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("aiu-upgrade-{tag}-{}-{unique}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A database as an older aiu left it: schema stopped at `version`, with the
/// state that version could hold already written.
fn old_install(path: &std::path::Path, version: usize) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    for (index, migration) in MIGRATIONS.iter().take(version).enumerate() {
        conn.execute_batch(&format!(
            "BEGIN; {migration} PRAGMA user_version = {}; COMMIT;",
            index + 1
        ))
        .unwrap();
    }
    conn
}

fn event(id: &str, device: &str, ts: &str) -> NewEvent {
    NewEvent {
        event_id: id.to_string(),
        workspace_id: "ws-original".to_string(),
        device_id: device.to_string(),
        source: "claude".to_string(),
        tool: "claude-code".to_string(),
        exact_model: "claude-opus-4-20250514".to_string(),
        session_id_hash: Some("hash".to_string()),
        ts_utc: ts.to_string(),
        input_tokens: Some(100),
        cached_input_tokens: Some(10),
        cache_write_tokens: Some(5),
        output_tokens: Some(50),
        reasoning_tokens: None,
        reported_cost_micros: Some(1234),
        tool_version: Some("1.0.0".to_string()),
        adapter_version: Some("1".to_string()),
    }
}

#[test]
fn upgrading_preserves_identity_keys_overrides_and_history() {
    let root = temp_root("state");
    let db = root.join("usage.db");

    {
        // An install one schema version behind the current binary. Starting
        // at len-1 rather than 1 keeps this test honest as migrations are
        // added: it always exercises the newest upgrade step.
        let conn = old_install(&db, MIGRATIONS.len() - 1);
        conn.execute(
            "INSERT INTO devices (device_id, friendly_name, os, arch) \
             VALUES ('device-original', 'studio', 'linux', 'x86_64')",
            [],
        )
        .unwrap();
        for (key, value) in [
            ("workspace_id", "ws-original"),
            ("device_id", "device-original"),
            ("device_credential", "credential-original"),
            (
                "workspace_key",
                "6b65790000000000000000000000000000000000000000000000000000000000",
            ),
            ("setup_complete", "1"),
            (LAST_COLLECT_KEY, "2026-08-30T10:00:00Z"),
        ] {
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO source_config (source, mode) VALUES ('codex', 'disabled'), ('go', 'enabled')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events (event_id, workspace_id, device_id, source, tool, \
             exact_model, ts_utc, input_tokens, output_tokens) \
             VALUES ('old-1', 'ws-original', 'device-original', 'claude', 'claude-code', \
             'claude-opus-4-20250514', '2026-08-01T00:00:00Z', 100, 50)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO quota_snapshots (source, window, used_percent, observing_device_id) \
             VALUES ('claude', '5h', 42.5, 'device-original')",
            [],
        )
        .unwrap();
    }

    // The new binary opening the same file is the whole upgrade.
    let store = Store::open(&db).unwrap();

    assert_eq!(
        store::schema_version(store.conn()).unwrap(),
        MIGRATIONS.len() as i64,
        "the new binary should bring the schema fully up to date"
    );

    // Identity and keys: a lost device_id orphans this machine's history, and
    // a lost workspace key makes every synced record permanently unreadable.
    assert_eq!(
        store.get_metadata("device_id").unwrap().as_deref(),
        Some("device-original")
    );
    assert_eq!(
        store.get_metadata("workspace_id").unwrap().as_deref(),
        Some("ws-original")
    );
    assert_eq!(
        store.get_metadata("device_credential").unwrap().as_deref(),
        Some("credential-original")
    );
    assert_eq!(
        store.get_metadata("workspace_key").unwrap().as_deref(),
        Some("6b65790000000000000000000000000000000000000000000000000000000000")
    );
    assert_eq!(
        store.get_metadata(LAST_COLLECT_KEY).unwrap().as_deref(),
        Some("2026-08-30T10:00:00Z"),
        "collection freshness should survive; resetting it would re-import"
    );

    // Source overrides are a user decision, not derived state.
    assert_eq!(
        store.source_mode("codex").unwrap(),
        SourceMode::Disabled,
        "a disabled source must stay disabled after an upgrade"
    );
    assert_eq!(store.source_mode("go").unwrap(), SourceMode::Enabled);
    assert_eq!(store.source_mode("claude").unwrap(), SourceMode::Auto);

    // History, and the friendly name that makes it legible.
    let events: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
        .unwrap();
    let snapshots: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM quota_snapshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!((events, snapshots), (1, 1));
    let name: String = store
        .conn()
        .query_row(
            "SELECT friendly_name FROM devices WHERE device_id='device-original'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "studio");
}

#[test]
fn upgrading_from_the_first_schema_version_keeps_every_row() {
    // The oldest database in the wild is the one the first release wrote.
    // Walking the whole chain in one open is the migration path a long-idle
    // machine takes when it finally updates.
    let root = temp_root("from-v1");
    let db = root.join("usage.db");

    {
        let conn = old_install(&db, 1);
        conn.execute(
            "INSERT INTO devices (device_id, friendly_name) VALUES ('d1', 'laptop')",
            [],
        )
        .unwrap();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO usage_events (event_id, workspace_id, device_id, source, tool, \
                 exact_model, ts_utc, input_tokens, output_tokens) \
                 VALUES (?1, 'ws', 'd1', 'codex', 'codex', 'gpt-5-codex', ?2, 7, 3)",
                rusqlite::params![format!("e{i}"), format!("2026-08-0{}T00:00:00Z", i + 1)],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO sync_outbox (record_kind, payload) VALUES ('usage_event', x'0a0b')",
            [],
        )
        .unwrap();
    }

    let store = Store::open(&db).unwrap();
    assert_eq!(
        store::schema_version(store.conn()).unwrap(),
        MIGRATIONS.len() as i64
    );

    let (events, outbox): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM usage_events), (SELECT COUNT(*) FROM sync_outbox)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((events, outbox), (5, 1), "no row may be dropped in transit");

    // v2 made the token columns nullable; values recorded under v1 were
    // observed, not guessed, so they must stay as they were rather than
    // becoming nulls.
    let tokens: (Option<i64>, Option<i64>) = store
        .conn()
        .query_row(
            "SELECT input_tokens, output_tokens FROM usage_events WHERE event_id='e0'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(tokens, (Some(7), Some(3)));
}

#[test]
fn an_upgraded_database_still_accepts_writes_and_reads_back_history() {
    // Schema version alone proves little; the point of preserving the file is
    // that the new binary can go on using it.
    let root = temp_root("writes");
    let db = root.join("usage.db");
    {
        let conn = old_install(&db, 1);
        conn.execute(
            "INSERT INTO devices (device_id, friendly_name) VALUES ('d1', 'laptop')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events (event_id, workspace_id, device_id, source, tool, \
             exact_model, ts_utc, input_tokens, output_tokens) \
             VALUES ('old', 'ws', 'd1', 'claude', 'claude-code', \
             'claude-opus-4-20250514', '2026-08-01T00:00:00Z', 1, 1)",
            [],
        )
        .unwrap();
    }

    let store = Store::open(&db).unwrap();
    assert!(
        store
            .record_event(&event("new-1", "d1", "2026-08-31T00:00:00Z"))
            .unwrap(),
        "a new event should be recorded against the preserved device row"
    );

    let count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2, "the old row and the new one should coexist");
}

#[test]
fn replacing_the_binary_leaves_the_installed_schedule_alone() {
    // The scheduler invokes an absolute path. An upgrade that writes to that
    // same path leaves the unit files correct and untouched, which is why the
    // installer moves a binary into place rather than reinstalling a
    // schedule it does not own.
    let root = temp_root("schedule");
    let exe = root.join("bin/aiu");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"v1 binary").unwrap();

    let spec = ScheduleSpec::new(exe.clone()).with_environment(vec![(
        "AIU_DATA_DIR".into(),
        root.join("data").display().to_string(),
    )]);
    let home = root.join("home");
    let mut runner = NoopRunner;
    let installation =
        scheduler::install(Platform::Linux, &home, None, &spec, &mut runner).unwrap();
    let before: Vec<String> = installation
        .unit_paths
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();

    // The upgrade: same path, new bytes.
    fs::write(&exe, b"v2 binary").unwrap();

    let after: Vec<String> = installation
        .unit_paths
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    assert_eq!(before, after, "an upgrade must not disturb the unit files");

    let recovered = scheduler::read_installed(Platform::Linux, &home, None)
        .expect("the schedule should still be readable after the binary is replaced");
    assert_eq!(recovered.spec.exe, exe);
    assert_eq!(
        recovered.spec.interval_minutes,
        scheduler::DEFAULT_INTERVAL_MINUTES
    );
    assert!(
        scheduler::drift(&recovered, &spec).is_empty(),
        "the schedule should still match the environment it was installed for"
    );
}
