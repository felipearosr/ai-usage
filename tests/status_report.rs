//! `aiu status` seam (issue 13). Spec story 30: "a status command covering
//! scheduler installation, last collection/sync, pending records, encryption
//! state, and relay reachability, so that I can diagnose problems locally."
//!
//! Everything the report needs is passed in, so these tests never touch a
//! real scheduler, relay, or clock.

use aiu::report::status::{self, EncryptionStatus, RelayStatus, ScheduleStatus, StatusReport};
use aiu::scheduler::{Drift, InstalledSchedule, Platform, ScheduleSpec};
use aiu::store::{NewDevice, Store};
use std::path::PathBuf;

const NOW: u64 = 1_717_200_000; // 2024-06-01T00:00:00Z

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
    store.set_metadata("device_id", "dev-a").unwrap();
    store
}

fn build(store: &Store, schedule: ScheduleStatus, relay: RelayStatus) -> StatusReport {
    status::build(store, schedule, relay, NOW).unwrap()
}

fn installed_schedule() -> ScheduleStatus {
    ScheduleStatus::Installed {
        platform: Platform::Linux,
        interval_minutes: 15,
        activated: true,
        unit_paths: vec![PathBuf::from(
            "/home/me/.config/systemd/user/aiu-collect.timer",
        )],
        drift: Some(Vec::new()),
    }
}

#[test]
fn a_healthy_machine_reports_every_facet_the_spec_names() {
    let store = store_with_device();
    store
        .set_metadata("last_collect_at_utc", &aiu::utc::format_epoch(NOW - 300))
        .unwrap();
    store
        .touch_device_sync("dev-a", &aiu::utc::format_epoch(NOW - 120))
        .unwrap();

    let report = build(&store, installed_schedule(), RelayStatus::Reachable);
    let text = status::render_text(&report);

    for facet in [
        "Scheduler",
        "Last collection",
        "Last sync",
        "Pending",
        "Encryption",
        "Relay",
    ] {
        assert!(text.contains(facet), "{facet} missing from:\n{text}");
    }
    assert!(
        text.contains("5m ago"),
        "collection age is rendered: {text}"
    );
    assert!(text.contains("2m ago"), "sync age is rendered: {text}");
    assert!(text.contains("reachable"), "{text}");
}

/// Unknown is not zero. A machine that has never collected must say so.
/// A fresh data directory has not minted a device id yet. An empty column is
/// exactly the "missing renders as blank" failure the spec rules out.
#[test]
fn a_machine_with_no_identity_yet_says_so_rather_than_rendering_blank() {
    let store = Store::open_in_memory().unwrap();

    let report = status::build(
        &store,
        ScheduleStatus::NotInstalled,
        RelayStatus::NotConfigured,
        NOW,
    )
    .unwrap();
    let text = status::render_text(&report);

    assert!(report.device_id.is_empty());
    assert!(text.contains("not yet assigned"), "{text}");
    assert!(
        !text.contains("Machine          \n"),
        "no empty value column: {text}"
    );
}

/// The report is read as a column, so labels of differing length must not
/// leave the values ragged.
#[test]
fn every_value_starts_in_the_same_column() {
    let store = store_with_device();
    let text = status::render_text(&build(&store, installed_schedule(), RelayStatus::Reachable));

    let starts: Vec<usize> = text
        .lines()
        .filter(|line| line.starts_with("  ") && line.contains(char::is_alphabetic))
        .map(|line| {
            let label_end = 2 + line[2..].find("  ").expect("label is padded");
            line[label_end..].len() - line[label_end..].trim_start().len() + label_end
        })
        .collect();

    assert!(starts.len() >= 6, "every facet is a row: {text}");
    assert!(
        starts.iter().all(|start| *start == starts[0]),
        "values are ragged:\n{text}"
    );
}

#[test]
fn values_that_have_never_happened_render_explicitly_not_as_zero() {
    let store = store_with_device();

    let report = build(
        &store,
        ScheduleStatus::NotInstalled,
        RelayStatus::NotConfigured,
    );
    let text = status::render_text(&report);

    assert!(report.last_collect_at_utc.is_none());
    assert!(report.last_sync_at_utc.is_none());
    assert!(text.contains("never"), "{text}");
    assert!(!text.contains("0s ago"), "no fabricated zero age: {text}");
    assert!(text.contains("not installed"), "{text}");
}

/// The relay probe is a diagnostic, not a gate: an unreachable relay is a
/// problem line, and the rest of the report still renders.
#[test]
fn an_unreachable_relay_is_a_problem_line_not_a_failure() {
    let store = store_with_device();

    let report = build(
        &store,
        installed_schedule(),
        RelayStatus::Unreachable("connection refused".into()),
    );
    let text = status::render_text(&report);

    assert!(text.contains("unreachable"), "{text}");
    assert!(text.contains("connection refused"), "{text}");
    assert!(text.contains("Scheduler"), "the rest still renders: {text}");
}

#[test]
fn scheduler_drift_is_reported_with_the_value_that_changed() {
    let store = store_with_device();
    let schedule = ScheduleStatus::Installed {
        platform: Platform::Linux,
        interval_minutes: 15,
        activated: true,
        unit_paths: Vec::new(),
        drift: Some(vec![Drift::Environment {
            key: "AIU_DATA_DIR".into(),
            installed: Some("/old/db".into()),
            current: Some("/new/db".into()),
        }]),
    };

    let text = status::render_text(&build(&store, schedule, RelayStatus::Reachable));

    assert!(text.contains("AIU_DATA_DIR"), "{text}");
    assert!(text.contains("/old/db"), "{text}");
}

#[test]
fn pending_records_are_counted() {
    let store = store_with_device();
    for index in 0..3 {
        store
            .conn()
            .execute(
                "INSERT INTO sync_outbox (record_id, record_kind, payload)
                 VALUES (?1, 'usage_event', X'00')",
                rusqlite::params![format!("r-{index}")],
            )
            .unwrap();
    }

    let report = build(&store, installed_schedule(), RelayStatus::Reachable);

    assert_eq!(report.pending_records, 3);
    assert!(status::render_text(&report).contains('3'));
}

/// The privacy hard rule and ordinary secret hygiene: status reports whether
/// key material exists, never any part of it.
#[test]
fn no_key_material_or_credential_ever_appears_in_either_rendering() {
    let store = store_with_device();
    let secret_key = "5f".repeat(32);
    let secret_credential = "c0ffee".repeat(8);
    store.set_metadata("workspace_key", &secret_key).unwrap();
    store
        .set_metadata("device_credential", &secret_credential)
        .unwrap();
    store.set_metadata("setup_complete", "1").unwrap();

    let report = build(&store, installed_schedule(), RelayStatus::Reachable);
    let text = status::render_text(&report);
    let json = status::render_json(&report);

    assert_eq!(
        report.encryption,
        EncryptionStatus {
            initialized: true,
            workspace_key_present: true,
            device_credential_present: true,
        }
    );
    for rendering in [&text, &json] {
        assert!(!rendering.contains(&secret_key), "workspace key leaked");
        assert!(
            !rendering.contains(&secret_credential),
            "device credential leaked"
        );
    }
    assert!(text.contains("workspace key present"), "{text}");
}

#[test]
fn an_uninitialized_machine_reports_missing_encryption_state() {
    let store = store_with_device();

    let report = build(
        &store,
        ScheduleStatus::NotInstalled,
        RelayStatus::NotConfigured,
    );

    assert_eq!(
        report.encryption,
        EncryptionStatus {
            initialized: false,
            workspace_key_present: false,
            device_credential_present: false,
        }
    );
    assert!(status::render_text(&report).contains("not initialized"));
}

#[test]
fn json_carries_the_same_facts_as_the_text() {
    let store = store_with_device();
    store
        .set_metadata("last_collect_at_utc", &aiu::utc::format_epoch(NOW - 60))
        .unwrap();

    let report = build(&store, installed_schedule(), RelayStatus::Reachable);
    let json: serde_json::Value = serde_json::from_str(&status::render_json(&report)).unwrap();

    assert_eq!(json["pending_records"], 0);
    assert_eq!(json["relay"]["state"], "reachable");
    assert_eq!(json["scheduler"]["state"], "installed");
    assert_eq!(json["scheduler"]["interval_minutes"], 15);
    assert_eq!(json["encryption"]["initialized"], false);
    assert!(json["last_collect_at_utc"].is_string());
    assert!(
        json["last_sync_at_utc"].is_null(),
        "never synced stays null"
    );
}

#[test]
fn a_platform_without_a_supported_scheduler_says_so() {
    let store = store_with_device();
    let text = status::render_text(&build(
        &store,
        ScheduleStatus::Unsupported,
        RelayStatus::Reachable,
    ));
    assert!(text.contains("no supported scheduler"), "{text}");
}

/// The collect pipeline is what records the collection time status reports.
#[test]
fn the_collect_pipeline_records_the_time_status_reads() {
    let home = std::env::temp_dir().join(format!("aiu-status-collect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".claude/projects/d")).unwrap();
    std::fs::write(
        home.join(".claude/projects/d/s.jsonl"),
        "{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2024-05-31T11:00:00.000Z\",\
          \"message\":{\"id\":\"m1\",\"model\":\"claude-opus-5\",\
          \"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .unwrap();

    let store = store_with_device();
    let ctx = aiu::adapters::IngestContext {
        device_id: "dev-a".into(),
        workspace_id: "ws".into(),
        now_epoch: NOW,
    };
    aiu::pipeline::run(&store, &home, &ctx, None, None, NOW).unwrap();

    let report = build(
        &store,
        ScheduleStatus::NotInstalled,
        RelayStatus::NotConfigured,
    );
    assert_eq!(
        report.last_collect_at_utc.as_deref(),
        Some(aiu::utc::format_epoch(NOW).as_str()),
        "the pass recorded when it ran"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// A schedule spec is what drift is measured against; keep the report's view
/// of it aligned with the scheduler's.
#[test]
fn drift_is_computed_against_the_current_spec() {
    let installed = InstalledSchedule {
        platform: Platform::Linux,
        spec: ScheduleSpec::new(PathBuf::from("/old/aiu")),
        unit_paths: Vec::new(),
    };
    let current = ScheduleSpec::new(PathBuf::from("/new/aiu"));
    assert!(!aiu::scheduler::drift(&installed, &current).is_empty());
}

/// A hand-edited or corrupt unit is still a schedule: reporting it as absent
/// would hide exactly the case drift detection exists for.
#[test]
fn units_that_cannot_be_read_are_distinguished_from_no_units_at_all() {
    let store = store_with_device();
    let text = status::render_text(&build(
        &store,
        ScheduleStatus::Unreadable {
            unit_paths: vec![PathBuf::from(
                "/home/me/.config/systemd/user/aiu-collect.timer",
            )],
        },
        RelayStatus::Reachable,
    ));

    assert!(text.contains("unreadable"), "{text}");
    assert!(
        !text.contains("not installed"),
        "not the same thing: {text}"
    );
}

/// With nothing to compare against, "no drift found" would be a claim the
/// report cannot support.
#[test]
fn an_uncomparable_schedule_is_not_reported_as_up_to_date() {
    let store = store_with_device();
    let schedule = ScheduleStatus::Installed {
        platform: Platform::Linux,
        interval_minutes: 15,
        activated: true,
        unit_paths: Vec::new(),
        drift: None,
    };

    let text = status::render_text(&build(&store, schedule, RelayStatus::Reachable));
    assert!(text.contains("cannot be compared"), "{text}");
}

/// A revoked machine is among the likeliest to run `aiu status`, and telling
/// it the network is down would send it after the wrong problem.
#[test]
fn a_revoked_device_is_told_it_was_revoked_not_that_the_relay_is_down() {
    let store = store_with_device();
    let text = status::render_text(&build(&store, installed_schedule(), RelayStatus::Revoked));

    assert!(text.contains("revoked"), "{text}");
    assert!(!text.contains("unreachable"), "{text}");
    assert!(text.contains("aiu join"), "it names the fix: {text}");
}

#[test]
fn a_relay_that_cannot_be_addressed_is_not_reported_as_unreachable() {
    let store = store_with_device();
    let text = status::render_text(&build(
        &store,
        installed_schedule(),
        RelayStatus::Misconfigured("invalid relay url".into()),
    ));

    assert!(text.contains("misconfigured"), "{text}");
    assert!(text.contains("invalid relay url"), "{text}");
}

/// `aiu schedule` and `aiu status` must not describe the same state
/// differently; both render through the same function.
#[test]
fn the_schedule_renderer_is_shared_with_the_status_table() {
    let drifted = ScheduleStatus::Installed {
        platform: Platform::Linux,
        interval_minutes: 15,
        activated: true,
        unit_paths: Vec::new(),
        drift: Some(vec![Drift::Interval {
            installed: 15,
            current: 30,
        }]),
    };

    let standalone = status::render_schedule(&drifted);
    assert!(standalone.contains("stale"), "{standalone}");
    assert!(
        standalone.contains("scheduled every 15 minutes"),
        "{standalone}"
    );
}

/// Redaction has to reach the rendering, not just exist as a helper: a relay
/// URL with credentials is echoed back through drift lines and relay errors,
/// and `aiu status --json` is what people paste into bug reports.
#[test]
fn credentials_in_a_relay_url_never_reach_either_rendering() {
    let store = store_with_device();
    let secret = "s3cret-token";
    let schedule = ScheduleStatus::Installed {
        platform: Platform::Linux,
        interval_minutes: 15,
        activated: true,
        unit_paths: Vec::new(),
        drift: Some(vec![Drift::Environment {
            key: "AIU_RELAY_URL".into(),
            installed: Some(format!("https://user:{secret}@old.example")),
            current: Some(format!("https://user:{secret}@new.example")),
        }]),
    };
    let report = build(
        &store,
        schedule,
        RelayStatus::Unreachable(format!("https://user:{secret}@relay.example refused")),
    );

    for rendering in [status::render_text(&report), status::render_json(&report)] {
        assert!(
            !rendering.contains(secret),
            "credential leaked into output:\n{rendering}"
        );
        assert!(
            rendering.contains("old.example"),
            "the useful part is still shown:\n{rendering}"
        );
    }
}
