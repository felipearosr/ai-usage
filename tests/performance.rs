//! Rendering latency at retention scale (issue 12): "local report rendering
//! effectively instantaneous and independent of network latency, so that the
//! CLI feels like `git status`" — spec target ≲100 ms.
//!
//! Its own test binary, like `collect_memory.rs`: a wall-clock budget measured
//! next to other tests running in parallel measures the harness rather than
//! the query path.
//!
//! The database is filled to the retention ceiling — a full year of events
//! across every source, machine and model — because that is the largest local
//! report aiu can be asked to render before pruning takes over. The store is
//! file-backed so real SQLite I/O is included, not an in-memory shortcut.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aiu::report::{self, breakdown, detail};
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

/// `cargo test --release` measures the binary users actually run, so it holds
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

fn temp_db() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiu-perf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("usage.db")
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

/// Runs a render a few times and returns the fastest, so an unlucky scheduler
/// slice on a shared CI box does not decide whether the query path regressed.
fn best_of<T>(times: usize, mut render: impl FnMut() -> T) -> Duration {
    (0..times)
        .map(|_| {
            let start = Instant::now();
            let value = render();
            let elapsed = start.elapsed();
            std::hint::black_box(value);
            elapsed
        })
        .min()
        .unwrap()
}

#[test]
fn every_local_report_renders_within_the_latency_budget() {
    let path = temp_db();
    let store = Store::open(&path).unwrap();
    populate(&store);

    let mut measured: Vec<(&str, Duration)> = Vec::new();

    measured.push((
        "aiu",
        best_of(5, || {
            report::text::render(&report::build(&store, NOW).unwrap())
        }),
    ));
    measured.push((
        "aiu machines",
        best_of(5, || {
            report::fleet::render_text(&report::fleet::build(&store, NOW).unwrap())
        }),
    ));
    for source in SOURCES {
        measured.push((
            source,
            best_of(5, || {
                detail::text::render(&detail::build(&store, source, NOW).unwrap())
            }),
        ));
        measured.push((
            "breakdown",
            best_of(5, || {
                breakdown::text::render_models(&breakdown::build(&store, source, NOW).unwrap())
            }),
        ));
    }

    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    let events = DAYS as usize * DEVICES * EVENTS_PER_DAY_PER_DEVICE;
    for (name, elapsed) in &measured {
        assert!(
            *elapsed < BUDGET,
            "`{name}` took {elapsed:?} over {events} events, budget {BUDGET:?}"
        );
    }
    // Printed so a run records the real numbers, not just that they passed.
    for (name, elapsed) in &measured {
        println!("{name:<16} {elapsed:?}");
    }
}
