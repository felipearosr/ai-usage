//! Memory budget for a full collect (issue 11): "scheduled runs well under
//! 30 MB RSS".
//!
//! This lives in its own test binary deliberately. `/proc/self/status` reports
//! the whole process, so a measurement taken alongside other tests running in
//! parallel threads measures the harness, not the pipeline. One test per
//! process is the only way to attribute the peak honestly.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::path::PathBuf;

use aiu::adapters::IngestContext;
use aiu::pipeline;
use aiu::store::{NewDevice, Store};
use aiu::sync::{
    DownloadBatch, EncryptedRecord, RelayClient, RelayError, SyncConfig, WorkspaceKey,
};

/// Fixture scale: 40 transcripts x 250 records = 10k events in a single pass,
/// comfortably more than a real machine accumulates between 15-minute runs.
const TRANSCRIPTS: usize = 40;
const RECORDS_EACH: usize = 250;
const BUDGET_KB: u64 = 30 * 1024;

#[derive(Default)]
struct FakeRelay {
    stored: Vec<EncryptedRecord>,
    ids: HashSet<(String, String)>,
}

impl RelayClient for FakeRelay {
    fn upload(&mut self, _credential: &str, records: &[EncryptedRecord]) -> Result<(), RelayError> {
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

fn peak_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("procfs is mounted");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
        .expect("VmHWM is reported on Linux")
}

fn fixture_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("aiu-collect-rss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let projects = home.join(".claude/projects/fixture");
    std::fs::create_dir_all(&projects).unwrap();
    for file in 0..TRANSCRIPTS {
        let mut body = String::with_capacity(RECORDS_EACH * 200);
        for line in 0..RECORDS_EACH {
            body.push_str(&format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"s\",\
                  \"timestamp\":\"2024-05-31T11:00:00.000Z\",\
                  \"message\":{{\"id\":\"m-{file}-{line}\",\"model\":\"claude-opus-5\",\
                  \"usage\":{{\"input_tokens\":10,\"output_tokens\":{line}}}}}}}\n"
            ));
        }
        std::fs::write(projects.join(format!("s{file}.jsonl")), body).unwrap();
    }
    home
}

#[test]
fn a_full_collect_stays_well_under_the_thirty_megabyte_target() {
    let home = fixture_home();
    // A file-backed store, as a scheduled run uses. An in-memory database
    // would charge the whole dataset to RSS and measure the test, not the
    // pipeline.
    let db_dir = home.join("data");
    std::fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(&db_dir.join("usage.db")).unwrap();
    store
        .upsert_local_device(&NewDevice {
            device_id: "dev-a".into(),
            friendly_name: "studio".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_sync_at_utc: None,
        })
        .unwrap();
    let ctx = IngestContext {
        device_id: "dev-a".into(),
        workspace_id: "ws".into(),
        now_epoch: 1_717_200_000,
    };
    let config = SyncConfig {
        workspace_id: "ws".into(),
        device_id: "dev-a".into(),
        device_credential: "credential-dev-a".into(),
        key: WorkspaceKey::from_bytes([7u8; 32]),
        download_limit: 100,
    };
    let mut relay = FakeRelay::default();

    let baseline = peak_rss_kb();
    let run = pipeline::run(
        &store,
        &home,
        &ctx,
        Some(&mut relay),
        Some(&config),
        1_717_200_000,
    )
    .unwrap();

    assert_eq!(run.events_imported() as usize, TRANSCRIPTS * RECORDS_EACH);
    let peak = peak_rss_kb();
    println!("peak RSS {peak} kB (baseline {baseline} kB, budget {BUDGET_KB} kB)");
    assert!(
        peak < BUDGET_KB,
        "peak RSS {peak} kB (baseline {baseline} kB) must stay under the {BUDGET_KB} kB target"
    );

    let _ = std::fs::remove_dir_all(&home);
}
