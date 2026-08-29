//! Sync/crypto seam acceptance tests (issue 08).
//!
//! These tests use the public sync API, a fake relay at the external system
//! boundary, and real authenticated encryption.

use std::collections::HashSet;

use aiu::adapters::claude::ClaudeCodeAdapter;
use aiu::adapters::IngestContext;
use aiu::collect::collect_source;
use aiu::import::ImportOptions;
use aiu::store::{NewDevice, NewEvent, NewSnapshot, Store};
use aiu::sync::{
    decrypt_record, encrypt_record, enqueue_record, sync_once, DownloadBatch, EncryptedRecord,
    RelayClient, RelayError, SyncConfig, SyncError, SyncRecord, WorkspaceKey,
};

#[derive(Default)]
struct FakeRelay {
    stored: Vec<EncryptedRecord>,
    ids: HashSet<(String, String)>,
    unavailable: bool,
    download_failures: usize,
    revoked_credentials: HashSet<String>,
}

impl RelayClient for FakeRelay {
    fn upload(&mut self, credential: &str, records: &[EncryptedRecord]) -> Result<(), RelayError> {
        if self.unavailable {
            return Err(RelayError::Unavailable);
        }
        if self.revoked_credentials.contains(credential) {
            return Err(RelayError::Revoked);
        }
        for record in records {
            let identity = (record.workspace_id.clone(), record.record_id.clone());
            if self.ids.insert(identity) {
                self.stored.push(record.clone());
            }
        }
        Ok(())
    }

    fn download(
        &mut self,
        credential: &str,
        workspace_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> Result<DownloadBatch, RelayError> {
        if self.unavailable {
            return Err(RelayError::Unavailable);
        }
        if self.download_failures > 0 {
            self.download_failures -= 1;
            return Err(RelayError::Unavailable);
        }
        if self.revoked_credentials.contains(credential) {
            return Err(RelayError::Revoked);
        }
        let start = after_cursor
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let records = self
            .stored
            .iter()
            .filter(|record| record.workspace_id == workspace_id)
            .skip(start)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(DownloadBatch {
            cursor: (start + records.len()).to_string(),
            records,
        })
    }
}

fn event(id: &str, device_id: &str) -> NewEvent {
    NewEvent {
        event_id: id.to_string(),
        workspace_id: "workspace-opaque".to_string(),
        device_id: device_id.to_string(),
        source: "codex".to_string(),
        tool: "codex-cli".to_string(),
        exact_model: "gpt-5.6-codex".to_string(),
        session_id_hash: Some("0123456789abcdef".to_string()),
        ts_utc: "2026-08-29T12:00:00Z".to_string(),
        input_tokens: Some(120),
        cached_input_tokens: Some(20),
        cache_write_tokens: None,
        output_tokens: Some(40),
        reasoning_tokens: Some(10),
        reported_cost_micros: None,
        tool_version: Some("0.200.0".to_string()),
        adapter_version: Some("1".to_string()),
    }
}

#[test]
fn authenticated_encryption_round_trip_preserves_a_record() {
    let key = WorkspaceKey::from_bytes([0x42; 32]);
    let original = SyncRecord::UsageEvent(Box::new(event("event-1", "device-a")));

    let encrypted = encrypt_record("workspace-opaque", &key, &original).unwrap();
    let decrypted = decrypt_record(&key, &encrypted).unwrap();

    assert_eq!(decrypted, original);
    assert_eq!(encrypted.workspace_id, "workspace-opaque");
    assert_eq!(encrypted.record_id.len(), 64);
    assert_ne!(encrypted.record_id, "event-1");
    assert!(!encrypted.ciphertext.is_empty());
}

#[test]
fn distinct_quota_observations_have_distinct_record_ids() {
    let snapshot = NewSnapshot {
        source: "codex".to_string(),
        window: "5h".to_string(),
        used_percent: 41.0,
        resets_at_utc: Some("2026-08-29T17:00:00Z".to_string()),
        observed_at_utc: "2026-08-29T12:00:00Z".to_string(),
        observing_device_id: "device-a".to_string(),
    };
    let first = SyncRecord::QuotaSnapshot(Box::new(snapshot.clone()));
    let mut changed = snapshot;
    changed.used_percent = 42.0;
    let second = SyncRecord::QuotaSnapshot(Box::new(changed));
    let key = WorkspaceKey::from_bytes([0x42; 32]);

    let first = encrypt_record("workspace-opaque", &key, &first).unwrap();
    let second = encrypt_record("workspace-opaque", &key, &second).unwrap();
    assert_ne!(first.record_id, second.record_id);
}

#[test]
fn quota_record_ids_are_keyed_and_hide_low_domain_values() {
    let record = SyncRecord::QuotaSnapshot(Box::new(NewSnapshot {
        source: "codex".to_string(),
        window: "5h".to_string(),
        used_percent: 41.0,
        resets_at_utc: Some("2026-08-29T17:00:00Z".to_string()),
        observed_at_utc: "2026-08-29T12:00:00Z".to_string(),
        observing_device_id: "device-a".to_string(),
    }));
    let first = encrypt_record(
        "workspace-opaque",
        &WorkspaceKey::from_bytes([0x11; 32]),
        &record,
    )
    .unwrap();
    let second = encrypt_record(
        "workspace-opaque",
        &WorkspaceKey::from_bytes([0x22; 32]),
        &record,
    )
    .unwrap();

    assert_ne!(first.record_id, second.record_id);
    for secret in ["codex", "5h", "41", "device-a"] {
        assert!(!first.record_id.contains(secret));
    }
}

#[test]
fn relay_only_receives_opaque_ciphertext() {
    let key = WorkspaceKey::from_bytes([0x24; 32]);
    let record = SyncRecord::UsageEvent(Box::new(event("opaque-id", "private-machine-name")));
    let encrypted = encrypt_record("opaque-workspace-id", &key, &record).unwrap();
    let mut relay = FakeRelay::default();

    relay
        .upload("opaque-device-credential", &[encrypted])
        .unwrap();

    assert_eq!(relay.stored.len(), 1);
    let stored = &relay.stored[0];
    let mut visible = Vec::new();
    visible.extend_from_slice(stored.workspace_id.as_bytes());
    visible.extend_from_slice(stored.record_id.as_bytes());
    visible.extend_from_slice(&stored.nonce);
    visible.extend_from_slice(&stored.ciphertext);
    for secret in ["private-machine-name", "gpt-5.6-codex", "codex-cli", "120"] {
        assert!(
            !visible
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes()),
            "relay ciphertext exposed {secret}"
        );
    }
}

fn device(store: &Store, id: &str) {
    store
        .ensure_device(&NewDevice {
            device_id: id.to_string(),
            friendly_name: id.to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            last_sync_at_utc: None,
        })
        .unwrap();
}

fn config(device_id: &str) -> SyncConfig {
    SyncConfig {
        workspace_id: "workspace-opaque".to_string(),
        device_credential: format!("credential-{device_id}"),
        key: WorkspaceKey::from_bytes([0x31; 32]),
        download_limit: 100,
    }
}

#[test]
fn offline_outbox_retries_without_losing_the_record() {
    let store = Store::open_in_memory().unwrap();
    device(&store, "device-a");
    enqueue_record(
        &store,
        &SyncRecord::UsageEvent(Box::new(event("offline-event", "device-a"))),
    )
    .unwrap();
    let mut relay = FakeRelay {
        unavailable: true,
        ..FakeRelay::default()
    };

    let error = sync_once(&store, &mut relay, &config("device-a")).unwrap_err();
    assert!(matches!(error, SyncError::Relay(RelayError::Unavailable)));
    assert_eq!(store.pending_sync_count().unwrap(), 1);

    relay.unavailable = false;
    let summary = sync_once(&store, &mut relay, &config("device-a")).unwrap();
    assert_eq!(summary.uploaded, 1);
    assert_eq!(store.pending_sync_count().unwrap(), 0);
    assert_eq!(relay.stored.len(), 1);
    assert_eq!(
        decrypt_record(&config("device-a").key, &relay.stored[0]).unwrap(),
        SyncRecord::UsageEvent(Box::new(event("offline-event", "device-a")))
    );
}

#[test]
fn collected_usage_is_queued_before_an_offline_sync_attempt() {
    let store = Store::open_in_memory().unwrap();
    let directory = std::env::temp_dir().join(format!("aiu-sync-collect-{}", std::process::id()));
    std::fs::remove_dir_all(&directory).ok();
    std::fs::create_dir_all(&directory).unwrap();
    let fixture = directory.join("session.jsonl");
    std::fs::write(
        &fixture,
        "{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-29T12:00:00Z\",\"message\":{\"id\":\"collected-1\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .unwrap();
    let context = IngestContext {
        device_id: "device-a".to_string(),
        workspace_id: "workspace-opaque".to_string(),
        now_epoch: 1_777_464_000,
    };

    collect_source(
        &store,
        &ClaudeCodeAdapter,
        &[fixture],
        &context,
        ImportOptions::default(),
    )
    .unwrap();
    assert_eq!(store.pending_sync_count().unwrap(), 1);

    let mut relay = FakeRelay {
        unavailable: true,
        ..FakeRelay::default()
    };
    assert!(matches!(
        sync_once(&store, &mut relay, &config("device-a")),
        Err(SyncError::Relay(RelayError::Unavailable))
    ));
    assert_eq!(store.pending_sync_count().unwrap(), 1);

    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn download_resumes_from_its_cursor_after_interruption() {
    let key = WorkspaceKey::from_bytes([0x31; 32]);
    let mut relay = FakeRelay::default();
    let encrypted = [
        encrypt_record(
            "workspace-opaque",
            &key,
            &SyncRecord::UsageEvent(Box::new(event("remote-1", "device-a"))),
        )
        .unwrap(),
        encrypt_record(
            "workspace-opaque",
            &key,
            &SyncRecord::UsageEvent(Box::new(event("remote-2", "device-a"))),
        )
        .unwrap(),
    ];
    relay.upload("credential-device-a", &encrypted).unwrap();
    let store = Store::open_in_memory().unwrap();
    device(&store, "device-a");
    let mut settings = config("device-a");
    settings.download_limit = 1;

    assert_eq!(
        sync_once(&store, &mut relay, &settings).unwrap().downloaded,
        1
    );
    relay.download_failures = 1;
    assert!(matches!(
        sync_once(&store, &mut relay, &settings),
        Err(SyncError::Relay(RelayError::Unavailable))
    ));
    assert_eq!(
        sync_once(&store, &mut relay, &settings).unwrap().downloaded,
        1
    );
    assert_eq!(
        sync_once(&store, &mut relay, &settings).unwrap().downloaded,
        0
    );
    assert!(!store.record_event(&event("remote-1", "device-a")).unwrap());
    assert!(!store.record_event(&event("remote-2", "device-a")).unwrap());
}

#[test]
fn relay_enforces_record_identity_deduplication() {
    let key = WorkspaceKey::from_bytes([0x31; 32]);
    let record = SyncRecord::UsageEvent(Box::new(event("same-record", "device-a")));
    let first = encrypt_record("workspace-opaque", &key, &record).unwrap();
    let retry_with_fresh_nonce = encrypt_record("workspace-opaque", &key, &record).unwrap();
    assert_ne!(first.nonce, retry_with_fresh_nonce.nonce);
    let mut relay = FakeRelay::default();

    relay
        .upload("credential-device-a", &[first, retry_with_fresh_nonce])
        .unwrap();

    assert_eq!(relay.stored.len(), 1);
    assert_eq!(relay.stored[0].record_id.len(), 64);
    assert_ne!(relay.stored[0].record_id, "same-record");
}

#[test]
fn three_devices_converge_under_interleaved_syncs() {
    let stores = [
        Store::open_in_memory().unwrap(),
        Store::open_in_memory().unwrap(),
        Store::open_in_memory().unwrap(),
    ];
    let ids = ["device-a", "device-b", "device-c"];
    for (store, id) in stores.iter().zip(ids) {
        device(store, id);
        enqueue_record(
            store,
            &SyncRecord::UsageEvent(Box::new(event(&format!("event-{id}"), id))),
        )
        .unwrap();
    }
    let mut relay = FakeRelay::default();

    for index in [0, 1, 2, 0, 1, 2] {
        sync_once(&stores[index], &mut relay, &config(ids[index])).unwrap();
    }

    assert_eq!(relay.stored.len(), 3);
    for store in &stores {
        for id in ids {
            assert!(
                !store
                    .record_event(&event(&format!("event-{id}"), id))
                    .unwrap(),
                "{id} should already exist on every device"
            );
        }
    }
}

#[test]
fn revoked_device_is_rejected_without_draining_its_outbox() {
    let store = Store::open_in_memory().unwrap();
    device(&store, "device-a");
    enqueue_record(
        &store,
        &SyncRecord::UsageEvent(Box::new(event("revoked-event", "device-a"))),
    )
    .unwrap();
    let mut relay = FakeRelay::default();
    relay
        .revoked_credentials
        .insert("credential-device-a".to_string());

    assert!(matches!(
        sync_once(&store, &mut relay, &config("device-a")),
        Err(SyncError::Relay(RelayError::Revoked))
    ));
    assert_eq!(store.pending_sync_count().unwrap(), 1);
    assert!(relay.stored.is_empty());
}

#[test]
fn record_from_another_workspace_never_leaves_the_device() {
    let store = Store::open_in_memory().unwrap();
    device(&store, "device-a");
    let mut wrong_workspace = event("wrong-workspace", "device-a");
    wrong_workspace.workspace_id = "workspace-secret-other".to_string();
    enqueue_record(&store, &SyncRecord::UsageEvent(Box::new(wrong_workspace))).unwrap();
    let mut relay = FakeRelay::default();

    assert!(matches!(
        sync_once(&store, &mut relay, &config("device-a")),
        Err(SyncError::WorkspaceMismatch)
    ));
    assert!(relay.stored.is_empty());
    assert_eq!(store.pending_sync_count().unwrap(), 1);
}
