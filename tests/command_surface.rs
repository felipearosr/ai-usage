//! The full command surface, driven as a user drives it (issue 12).
//!
//! `cli.rs`'s own tests prove the parser maps words to commands. This file
//! proves the commands behind those words actually run: the real `aiu` binary,
//! against a sandboxed HOME and data directory, with every command from spec
//! §100 invoked and its exit status and output checked.
//!
//! Everything here is offline. `AIU_RELAY_URL` points at a closed local port,
//! so any command that would reach the network fails fast and locally instead
//! of touching a real relay.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A port nothing is listening on: reachable-host, refused-connection, which
/// resolves immediately rather than waiting out a DNS or TCP timeout.
const DEAD_RELAY: &str = "http://127.0.0.1:9";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Cli {
    root: PathBuf,
}

impl Cli {
    fn new() -> Self {
        // Tests in this binary run in parallel and each drives a real
        // database, so every one gets its own install.
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("aiu-surface-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_aiu"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("AIU_DATA_DIR", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("AIU_RELAY_URL", DEAD_RELAY)
            .output()
            .unwrap();
        Run {
            args: args.join(" "),
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Stands in for a completed `aiu init`: the workspace secrets a paired
    /// machine holds, written straight into the same database the CLI opens.
    /// `aiu init` itself needs a relay and a terminal, neither of which
    /// belongs in an offline test.
    fn pair(&self) {
        let store = aiu::store::Store::open(&self.root.join("data/usage.db")).unwrap();
        let identity = aiu::identity::ensure_local_identity(&store).unwrap();
        store
            .set_metadata("device_credential", "test-credential")
            .unwrap();
        store
            .set_metadata(
                "workspace_key",
                "6b65790000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap();
        store.set_metadata("setup_complete", "1").unwrap();
        assert!(!identity.device_id.is_empty());
    }

    /// Writes real usage into the same database the CLI opens, so a report
    /// has something to get wrong. Without it the report assertions only
    /// prove a command exits zero over an empty table.
    fn seed(&self) {
        let store = aiu::store::Store::open(&self.root.join("data/usage.db")).unwrap();
        store
            .ensure_device(&aiu::store::NewDevice {
                device_id: "seeded-device".into(),
                friendly_name: "seeded-machine".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                last_sync_at_utc: Some(aiu::utc::now_rfc3339()),
            })
            .unwrap();
        let now = aiu::utc::now_epoch();
        for (index, source) in SOURCES.iter().enumerate() {
            store
                .record_event(&aiu::store::NewEvent {
                    event_id: format!("seeded-{source}"),
                    workspace_id: "ws".into(),
                    device_id: "seeded-device".into(),
                    source: (*source).into(),
                    tool: (*source).into(),
                    exact_model: format!("{source}-exact-model-2026"),
                    ts_utc: aiu::utc::format_epoch(now - 600),
                    output_tokens: Some(1_000 + index as i64),
                    ..Default::default()
                })
                .unwrap();
            store
                .record_snapshot(&aiu::store::NewSnapshot {
                    source: (*source).into(),
                    window: "5h".into(),
                    used_percent: 42.0,
                    resets_at_utc: Some(aiu::utc::format_epoch(now + 3_600)),
                    observed_at_utc: aiu::utc::format_epoch(now - 60),
                    observing_device_id: "seeded-device".into(),
                })
                .unwrap();
        }
    }

    /// Runs a command that must succeed, returning its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let run = self.run(args);
        assert!(
            run.ok,
            "`aiu {}` failed\nstdout: {}\nstderr: {}",
            run.args, run.stdout, run.stderr
        );
        run.stdout
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Run {
    args: String,
    ok: bool,
    stdout: String,
    stderr: String,
}

fn as_json(output: &str) -> serde_json::Value {
    serde_json::from_str(output)
        .unwrap_or_else(|e| panic!("`--json` output should parse: {e}\n{output}"))
}

const SOURCES: [&str; 3] = ["claude", "codex", "go"];

#[test]
fn every_report_command_runs_on_a_fresh_install() {
    // A fresh install has no data, which is the case most likely to divide by
    // zero or render an empty table badly. Here the bar is only that every
    // command exits cleanly and any JSON parses; the seeded test below is
    // what checks the numbers actually reach the output.
    let cli = Cli::new();

    assert!(
        !cli.ok(&[]).is_empty(),
        "the compact report should say something"
    );
    as_json(&cli.ok(&["--json"]));

    for source in SOURCES {
        assert!(!cli.ok(&[source]).is_empty());
        as_json(&cli.ok(&[source, "--json"]));
        for kind in ["models", "machines"] {
            assert!(!cli.ok(&[source, kind]).is_empty());
            as_json(&cli.ok(&[source, kind, "--json"]));
        }
    }

    assert!(!cli.ok(&["machines"]).is_empty());
    as_json(&cli.ok(&["machines", "--json"]));
}

#[test]
fn seeded_usage_reaches_every_report_that_should_show_it() {
    let cli = Cli::new();
    cli.seed();

    // The compact overview carries every source's window and its top model,
    // under the exact model id — never collapsed into a family name.
    let overview = as_json(&cli.ok(&["--json"]));
    let sources = overview["sources"].as_array().expect("sources array");
    for source in SOURCES {
        let entry = sources
            .iter()
            .find(|entry| entry["source"] == source)
            .unwrap_or_else(|| panic!("{source} should appear in the overview: {overview}"));
        assert_eq!(
            entry["windows"][0]["used_percent"], 42.0,
            "the recorded quota should be the one reported: {entry}"
        );
        assert_eq!(
            entry["top_model"]["name"],
            format!("{source}-exact-model-2026"),
            "the exact model id should survive to the report: {entry}"
        );
    }

    // The machine's friendly name reaches the fleet view and the per-source
    // machine breakdowns; the exact model reaches the model matrix.
    assert!(cli.ok(&["machines"]).contains("seeded-machine"));
    for source in SOURCES {
        let text = cli.ok(&[source, "machines"]);
        assert!(
            text.contains("seeded-machine"),
            "`aiu {source} machines` should attribute usage to the machine:\n{text}"
        );
        let models = cli.ok(&[source, "models"]);
        assert!(
            models.contains(&format!("{source}-exact-model-2026")),
            "`aiu {source} models` should list the exact model:\n{models}"
        );
    }
}

#[test]
fn status_reports_the_five_facets_and_its_json_matches() {
    let cli = Cli::new();

    let text = cli.ok(&["status"]);
    for facet in [
        "Scheduler",
        "Last collection",
        "Last sync",
        "Pending records",
        "Encryption",
        "Relay",
    ] {
        assert!(
            text.contains(facet),
            "status should report {facet}:\n{text}"
        );
    }

    let json = as_json(&cli.ok(&["status", "--json"]));
    for key in [
        "scheduler",
        "last_collect_at_utc",
        "last_sync_at_utc",
        "pending_records",
        "encryption",
        "relay",
    ] {
        assert!(
            json.get(key).is_some(),
            "status JSON should carry {key}: {json}"
        );
    }
    // Presence, never content: the key itself must not be in the output.
    assert!(
        json["encryption"].get("workspace_key").is_none(),
        "status must report encryption as presence, not material"
    );
}

#[test]
fn source_overrides_round_trip_through_the_cli() {
    let cli = Cli::new();

    assert!(!cli.ok(&["sources"]).is_empty());
    as_json(&cli.ok(&["sources", "--json"]));
    cli.ok(&["sources", "detect"]);

    // Each override is set through the CLI and read back through it, so the
    // check covers persistence rather than just an accepted argument.
    for (mode, expected) in [
        ("disable", "disabled"),
        ("enable", "enabled"),
        ("auto", "auto"),
    ] {
        cli.ok(&["sources", mode, "claude"]);
        let json = as_json(&cli.ok(&["sources", "--json"]));
        let sources = json["sources"].as_array().expect("sources array");
        let claude = sources
            .iter()
            .find(|entry| entry["source"] == "claude")
            .expect("claude should be listed");
        assert_eq!(
            claude["mode"], expected,
            "`aiu sources {mode} claude` should persist as {expected}: {json}"
        );
    }
}

#[test]
fn collect_runs_end_to_end_without_a_relay() {
    let cli = Cli::new();
    // The scheduled pass on a machine with no sources installed and no
    // workspace: it must complete quietly, not fail, or every 15 minutes
    // would log an error.
    let run = cli.run(&["collect"]);
    assert!(
        run.ok,
        "`aiu collect` should succeed with nothing to collect\nstdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
}

#[test]
fn sync_without_a_workspace_explains_itself_rather_than_panicking() {
    let cli = Cli::new();
    let run = cli.run(&["sync"]);
    let text = format!("{}{}", run.stdout, run.stderr);
    assert!(
        !run.ok,
        "syncing without a workspace has nothing to sync and must not claim success: {text}"
    );
    assert!(
        text.contains("init") && text.contains("join"),
        "an unpaired sync should name both ways to set one up: {text}"
    );
}

#[test]
fn machine_rename_and_remove_reach_the_stored_device() {
    let cli = Cli::new();
    // `collect` mints this machine's device row, which is what the fleet
    // commands address; renaming it syncs, so the machine must be paired.
    cli.ok(&["collect"]);
    cli.pair();
    let json = as_json(&cli.ok(&["machines", "--json"]));
    let device = json["machines"]
        .as_array()
        .and_then(|machines| machines.first())
        .and_then(|machine| machine["device_id"].as_str())
        .expect("collect should have registered this machine")
        .to_string();

    // The relay is unreachable here, which must not fail the rename: it is
    // applied locally and queued in the durable outbox, so reporting failure
    // would send the user to repeat work that is already done.
    let renamed = cli.run(&["machine", "rename", &device, "renamed-in-a-test"]);
    assert!(
        renamed.ok,
        "an offline rename should succeed and queue\nstdout: {}\nstderr: {}",
        renamed.stdout, renamed.stderr
    );
    assert!(
        renamed.stdout.contains("Renamed"),
        "got: {}",
        renamed.stdout
    );
    assert!(
        renamed.stderr.contains("not yet synced"),
        "the user should be told propagation is pending: {}",
        renamed.stderr
    );
    assert!(
        cli.ok(&["machines"]).contains("renamed-in-a-test"),
        "a rename should show up in `aiu machines`"
    );

    // Removal revokes through the relay, so offline it must refuse rather
    // than claim a revocation that never reached anyone.
    let run = cli.run(&["machine", "remove", &device]);
    let text = format!("{}{}", run.stdout, run.stderr);
    assert!(
        !text.contains("panicked"),
        "machine remove panicked: {text}"
    );
    assert!(
        !run.ok && !run.stdout.contains("Removed"),
        "an unreachable relay must not be reported as a completed removal: {text}"
    );
}

#[test]
fn schedule_can_be_inspected_without_installing_anything() {
    let cli = Cli::new();
    // Showing the schedule must be read-only: `aiu status` calls the same
    // path, and a status check that silently installed a timer would be a
    // nasty surprise.
    let text = cli.ok(&["schedule"]);
    assert!(!text.is_empty());
    assert!(
        !cli.root.join("config/systemd").exists(),
        "`aiu schedule` must not write unit files"
    );
}

#[test]
fn setup_commands_are_reachable_and_reject_bad_input_locally() {
    let cli = Cli::new();

    let help = cli.ok(&["--help"]);
    for command in [
        "aiu init",
        "aiu join",
        "aiu sync",
        "aiu collect",
        "aiu status",
        "aiu machines",
        "aiu sources",
        "aiu schedule",
    ] {
        assert!(
            help.contains(command),
            "help should document `{command}`:\n{help}"
        );
    }

    // A malformed pairing code is refused by parsing it, before any relay
    // request — which is why this stays offline.
    let run = cli.run(&["join", "not-a-real-code"]);
    assert!(!run.ok, "a malformed pairing code should be refused");
    assert!(
        run.stderr.starts_with("aiu:"),
        "the refusal should be an aiu error, not a panic: {}",
        run.stderr
    );
}

#[test]
fn version_and_unknown_commands_behave() {
    let cli = Cli::new();
    let version = cli.ok(&["--version"]);
    assert!(version.starts_with("aiu "), "got: {version}");

    let run = cli.run(&["frobnicate"]);
    assert!(!run.ok);
    assert!(run.stderr.contains("USAGE"), "got: {}", run.stderr);
}
