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

    /// Runs a command that must succeed, returning its stdout.
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
fn every_report_command_runs_and_its_json_parses() {
    let cli = Cli::new();

    // Compact overview, then each per-source detail view and both breakdowns,
    // in text and JSON. A fresh install has no data, which is the case most
    // likely to divide by zero or render an empty table badly.
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
        !text.contains("panicked"),
        "`aiu sync` must not panic before setup: {text}"
    );
    if !run.ok {
        assert!(
            text.starts_with("aiu:") || text.contains("init") || text.contains("join"),
            "an unpaired sync should point at setup: {text}"
        );
    }
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
