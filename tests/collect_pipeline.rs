//! Collect-pipeline seam (issue 11): `aiu collect` performs the complete
//! pipeline in one invocation and exits — collect deltas, persist, enqueue
//! outbox, sync if reachable, opportunistically prune, return.
//!
//! The relay is faked at the external boundary so "reachable" and "offline"
//! are both exercised deterministically.

use std::collections::HashSet;
use std::path::PathBuf;

use aiu::adapters::IngestContext;
use aiu::pipeline::{self, CollectRun};
use aiu::store::{NewDevice, Store};
use aiu::sync::{
    DownloadBatch, EncryptedRecord, RelayClient, RelayError, SyncConfig, WorkspaceKey,
};

const NOW: u64 = 1_717_200_000; // 2024-06-01T00:00:00Z
const DAY: u64 = 86_400;

#[derive(Default)]
struct FakeRelay {
    stored: Vec<EncryptedRecord>,
    ids: HashSet<(String, String)>,
    offline: bool,
}

impl RelayClient for FakeRelay {
    fn upload(&mut self, _credential: &str, records: &[EncryptedRecord]) -> Result<(), RelayError> {
        if self.offline {
            return Err(RelayError::Unavailable);
        }
        for record in records {
            if self
                .ids
                .insert((record.workspace_id.clone(), record.record_id.clone()))
            {
                self.stored.push(record.clone());
            }
        }
        Ok(())
    }

    fn download(
        &mut self,
        _credential: &str,
        workspace_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> Result<DownloadBatch, RelayError> {
        if self.offline {
            return Err(RelayError::Unavailable);
        }
        let start = after_cursor
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let records: Vec<_> = self
            .stored
            .iter()
            .filter(|record| record.workspace_id == workspace_id)
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        Ok(DownloadBatch {
            cursor: (start + records.len()).to_string(),
            records,
        })
    }

    fn revoke_device(
        &mut self,
        _credential: &str,
        _workspace_id: &str,
        _device_id: &str,
    ) -> Result<(), RelayError> {
        Ok(())
    }
}

fn fixture_home(tag: &str, transcripts: usize, lines_each: usize) -> PathBuf {
    let home = std::env::temp_dir().join(format!("aiu-pipeline-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let projects = home.join(".claude/projects/fixture");
    std::fs::create_dir_all(&projects).unwrap();
    for file in 0..transcripts {
        let mut body = String::new();
        for line in 0..lines_each {
            body.push_str(&claude_line(&format!("m-{file}-{line}"), line as i64));
            body.push('\n');
        }
        std::fs::write(projects.join(format!("s{file}.jsonl")), body).unwrap();
    }
    home
}

fn claude_line(id: &str, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2024-05-31T11:00:00.000Z\",\
          \"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5\",\
          \"usage\":{{\"input_tokens\":10,\"output_tokens\":{output}}}}}}}"
    )
}

fn store_with_device() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_local_device(&NewDevice {
            device_id: "dev-a".into(),
            friendly_name: "studio".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_sync_at_utc: None,
        })
        .unwrap();
    store
}

fn ctx() -> IngestContext {
    IngestContext {
        device_id: "dev-a".into(),
        workspace_id: "ws".into(),
        now_epoch: NOW,
    }
}

fn config() -> SyncConfig {
    SyncConfig {
        workspace_id: "ws".into(),
        device_id: "dev-a".into(),
        device_credential: "credential-dev-a".into(),
        key: WorkspaceKey::from_bytes([7u8; 32]),
        download_limit: 100,
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

fn insert_ancient_event(store: &Store) {
    store
        .conn()
        .execute(
            "INSERT INTO usage_events (event_id, workspace_id, device_id, source, tool,
                                       exact_model, ts_utc)
             VALUES ('ancient', 'ws', 'dev-a', 'claude', 'claude-code', 'claude-opus-5', ?1)",
            rusqlite::params![aiu::utc::format_epoch(NOW - 400 * DAY)],
        )
        .unwrap();
}

#[test]
fn one_invocation_collects_persists_syncs_and_prunes() {
    let home = fixture_home("full", 2, 3);
    let store = store_with_device();
    insert_ancient_event(&store);
    let mut relay = FakeRelay::default();

    let run: CollectRun = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();

    assert_eq!(
        run.events_imported(),
        6,
        "both fixture transcripts imported"
    );
    assert!(
        run.sync.is_some(),
        "a reachable relay is synced in the same pass"
    );
    assert!(run.sync_error.is_none());
    assert_eq!(run.pruned.usage_events, 1, "the year-old event is pruned");
    assert_eq!(
        run.pending_records, 0,
        "everything collected was delivered before exit"
    );
    assert!(!relay.stored.is_empty(), "records reached the relay");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_offline_machine_stores_locally_and_keeps_everything_queued() {
    let home = fixture_home("offline", 1, 4);
    let store = store_with_device();
    let mut relay = FakeRelay {
        offline: true,
        ..FakeRelay::default()
    };

    let run = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();

    assert_eq!(run.events_imported(), 4);
    assert_eq!(count(&store, "usage_events"), 4, "persisted locally");
    assert!(run.sync.is_none(), "no sync summary when unreachable");
    assert!(
        run.sync_error.is_some(),
        "the unreachable relay is reported, not swallowed"
    );
    assert_eq!(
        run.pending_records, 4,
        "every record stays queued for later delivery"
    );
    assert!(relay.stored.is_empty());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_queued_offline_batch_is_delivered_by_the_next_reachable_run() {
    let home = fixture_home("recover", 1, 3);
    let store = store_with_device();
    let mut relay = FakeRelay {
        offline: true,
        ..FakeRelay::default()
    };

    let offline_run = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();
    assert_eq!(offline_run.pending_records, 3);

    relay.offline = false;
    let online_run = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();

    assert_eq!(online_run.events_imported(), 0, "no new deltas to collect");
    assert_eq!(
        online_run.pending_records, 0,
        "the queued batch was delivered — nothing was lost"
    );
    assert!(online_run.sync.is_some());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pruning_still_runs_when_the_relay_is_unreachable() {
    let home = fixture_home("prune-offline", 1, 1);
    let store = store_with_device();
    insert_ancient_event(&store);
    let mut relay = FakeRelay {
        offline: true,
        ..FakeRelay::default()
    };

    let run = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();

    assert_eq!(run.pruned.usage_events, 1);
    assert!(run.sync_error.is_some());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_machine_that_never_paired_collects_and_prunes_without_a_relay() {
    let home = fixture_home("unpaired", 1, 2);
    let store = store_with_device();
    insert_ancient_event(&store);

    let run = pipeline::run(&store, &home, &ctx(), None, None, NOW).unwrap();

    assert_eq!(run.events_imported(), 2);
    assert!(run.sync.is_none());
    assert!(
        run.sync_error.is_none(),
        "not being paired is not a sync failure"
    );
    assert_eq!(run.pruned.usage_events, 1);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn re_running_the_pipeline_imports_nothing_twice() {
    let home = fixture_home("idempotent", 2, 2);
    let store = store_with_device();
    let mut relay = FakeRelay::default();

    let first = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();
    let second = pipeline::run(
        &store,
        &home,
        &ctx(),
        Some(&mut relay),
        Some(&config()),
        NOW,
    )
    .unwrap();

    assert_eq!(first.events_imported(), 4);
    assert_eq!(second.events_imported(), 0);
    assert_eq!(count(&store, "usage_events"), 4);

    let _ = std::fs::remove_dir_all(&home);
}
