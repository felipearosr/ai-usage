//! Rendering latency at retention scale (issue 12): "local report rendering
//! effectively instantaneous and independent of network latency, so that the
//! CLI feels like `git status`" — spec target ≲100 ms.
//!
//! Timed by running the real `aiu` binary, not by calling `report::build` in
//! process. "Feels like `git status`" is a claim about what a user waits for,
//! which includes process start and opening the database — neither of which a
//! library-level measurement would see.
//!
//! What these numbers do not include is a working refresh. Report commands go
//! through `refreshed_store`, which collects and opportunistically syncs
//! before rendering, but the sandbox `HOME` holds no source files and no
//! workspace, so both return almost immediately; `aiu status` does not refresh
//! at all. So this is the query, render and startup cost at retention scale,
//! which is what the ≲100 ms target is about. Ingest cost is a separate
//! budget, held by `collect_memory.rs`.
//!
//! Its own test binary, like `collect_memory.rs`: a wall-clock budget measured
//! next to other tests running in parallel measures the harness rather than
//! the work.
//!
//! The database is filled to the retention ceiling — a full year of events
//! across every source, machine and model — because that is the largest local
//! report aiu can be asked to render before pruning takes over.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use aiu::store::{NewDevice, Store};
use aiu::utc;

/// Deterministic "now": 2026-08-31T12:00:00Z.
const NOW: u64 = 1_788_522_000;
const DAY: u64 = 86_400;

const DEVICES: usize = 6;
const SOURCES: [&str; 3] = ["claude", "codex", "go"];
const MODELS: [&str; 3] = ["a", "b", "c"];
/// A full retention year at a busy-but-plausible rate.
const DAYS: u64 = 365;
const EVENTS_PER_DAY_PER_DEVICE: usize = 8;

/// A string only the fixture can produce. `machine 0` is the top machine for
/// every source, so it appears in the fleet view, the compact overview and
/// every per-source view. `aiu status` reports diagnostics rather than usage
/// and so has a witness of its own.
const USAGE_WITNESS: &str = "machine 0";
const STATUS_WITNESS: &str = "Pending records";

/// `cargo test --release` builds the binary users actually run, so it holds
/// the spec's 100 ms target directly. An unoptimized build compiles SQLite's
/// bundled C without optimization and inlines nothing, so a debug run is
/// several times slower for reasons that say nothing about the query path;
/// there the budget only has to catch an accidental full-history scan or an
/// N+1 query, which is what a latency regression actually looks like.
const BUDGET: Duration = if cfg!(debug_assertions) {
    Duration::from_millis(1_000)
} else {
    Duration::from_millis(100)
};

fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiu-perf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn populate(store: &Store) {
    for d in 0..DEVICES {
        store
            .ensure_device(&NewDevice {
                device_id: format!("device-{d}"),
                friendly_name: format!("machine {d}"),
                os: "linux".into(),
                arch: "x86_64".into(),
                last_sync_at_utc: Some(utc::format_epoch(NOW - 300)),
            })
            .unwrap();
    }

    // Fixture loading only; the measured renders are all reads.
    store
        .conn()
        .pragma_update(None, "synchronous", "OFF")
        .unwrap();

    let tx = store.transaction().unwrap();
    for day in 0..DAYS {
        let base = NOW - day * DAY;
        for d in 0..DEVICES {
            for n in 0..EVENTS_PER_DAY_PER_DEVICE {
                let source = SOURCES[n % SOURCES.len()];
                let model = MODELS[(n / SOURCES.len()) % MODELS.len()];
                tx.execute(
                    "INSERT INTO usage_events (event_id, workspace_id, device_id, source, tool, \
                     exact_model, ts_utc, input_tokens, output_tokens) \
                     VALUES (?1, 'ws', ?2, ?3, ?3, ?4, ?5, 100, 50)",
                    rusqlite::params![
                        format!("e-{day}-{d}-{n}"),
                        format!("device-{d}"),
                        source,
                        format!("{source}-model-{model}-2026"),
                        utc::format_epoch(base - n as u64 * 60),
                    ],
                )
                .unwrap();
            }
        }
    }
    tx.commit().unwrap();

    // Quota snapshots accumulate once per collection; a year of 15-minute
    // runs is what a report has to look past to find the current window.
    let tx = store.transaction().unwrap();
    for tick in 0..(DAYS * 24 * 4) {
        let observed = utc::format_epoch(NOW - tick * 900);
        for source in SOURCES {
            tx.execute(
                "INSERT INTO quota_snapshots (source, window, used_percent, resets_at_utc, \
                 observed_at_utc, observing_device_id) VALUES (?1, '5h', ?2, ?3, ?4, 'device-0')",
                rusqlite::params![
                    source,
                    (tick % 100) as f64,
                    utc::format_epoch(NOW + 3600),
                    observed,
                ],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
}

/// Runs a command a few times and returns the fastest, so an unlucky
/// scheduler slice on a shared CI box does not decide whether the query path
/// regressed.
fn best_of(root: &std::path::Path, args: &[&str], witness: &str) -> Duration {
    (0..5)
        .map(|_| {
            let start = Instant::now();
            let out = Command::new(env!("CARGO_BIN_EXE_aiu"))
                .args(args)
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", root.join("home"))
                .env("AIU_DATA_DIR", root.join("data"))
                .output()
                .unwrap();
            let elapsed = start.elapsed();
            assert!(
                out.status.success(),
                "`aiu {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            // A timing over an empty report would be fast and meaningless, so
            // every measured command has to prove it saw the fixture: if the
            // AIU_DATA_DIR seam ever broke, each run would render the
            // empty-install report and sail under the budget.
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains(witness),
                "`aiu {}` did not render the fixture data, so its timing means \
                 nothing:\n{stdout}",
                args.join(" ")
            );
            elapsed
        })
        .min()
        .unwrap()
}

#[test]
fn every_local_report_renders_within_the_latency_budget() {
    let root = temp_root();
    {
        let store = Store::open(&root.join("data/usage.db")).unwrap();
        populate(&store);
    }

    // Every report command in the spec's §100 surface that renders locally.
    let mut commands: Vec<(Vec<&str>, &str)> = vec![
        (vec![], USAGE_WITNESS),
        (vec!["machines"], USAGE_WITNESS),
        (vec!["status"], STATUS_WITNESS),
    ];
    for source in SOURCES {
        commands.push((vec![source], USAGE_WITNESS));
        commands.push((vec![source, "models"], USAGE_WITNESS));
        commands.push((vec![source, "machines"], USAGE_WITNESS));
    }

    let measured: Vec<(String, Duration)> = commands
        .iter()
        .map(|(args, witness)| {
            (
                format!("aiu {}", args.join(" ")).trim_end().to_string(),
                best_of(&root, args, witness),
            )
        })
        .collect();

    let _ = std::fs::remove_dir_all(&root);

    let events = DAYS as usize * DEVICES * EVENTS_PER_DAY_PER_DEVICE;
    // Printed first so a failing run still records every number, not just the
    // one that tripped the budget.
    for (name, elapsed) in &measured {
        println!("{name:<20} {elapsed:?}");
    }
    for (name, elapsed) in &measured {
        assert!(
            *elapsed < BUDGET,
            "`{name}` took {elapsed:?} over {events} events, budget {BUDGET:?}"
        );
    }
}
