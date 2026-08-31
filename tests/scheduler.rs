//! Scheduler seam (issue 11): setup installs an OS scheduler that invokes
//! `aiu collect` every 15 minutes — a systemd user timer on Linux, a launchd
//! agent on macOS — and there is no resident daemon between runs.
//!
//! Both platforms are rendered and installed against a temporary root on
//! whichever OS runs the suite, so neither branch is untested on the other.

use aiu::scheduler::{
    self, Installation, Platform, ScheduleSpec, DEFAULT_INTERVAL_MINUTES, LAUNCHD_LABEL,
    SYSTEMD_TIMER,
};
use std::path::{Path, PathBuf};

/// Records the activation commands instead of running them, so the suite
/// never touches the developer's real systemd or launchd state.
#[derive(Default)]
struct RecordingRunner {
    calls: Vec<String>,
    fail: bool,
}

impl scheduler::CommandRunner for RecordingRunner {
    fn run(&mut self, program: &str, args: &[String]) -> std::io::Result<()> {
        self.calls.push(format!("{program} {}", args.join(" ")));
        if self.fail {
            return Err(std::io::Error::other("activation refused"));
        }
        Ok(())
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiu-sched-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spec() -> ScheduleSpec {
    ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn default_interval_is_fifteen_minutes() {
    assert_eq!(DEFAULT_INTERVAL_MINUTES, 15);
    assert_eq!(spec().interval_minutes, 15);
}

#[test]
fn systemd_units_run_collect_on_a_fifteen_minute_timer() {
    let service = scheduler::render_systemd_service(&spec());
    let timer = scheduler::render_systemd_timer(&spec());

    assert!(
        service.contains("ExecStart=/usr/local/bin/aiu collect"),
        "service invokes the collect pipeline: {service}"
    );
    assert!(
        service.contains("Type=oneshot"),
        "a oneshot unit starts, works, and exits — no resident daemon: {service}"
    );
    assert!(
        timer.contains("OnCalendar=*:0/15"),
        "repeats every 15 minutes: {timer}"
    );
    assert!(
        timer.contains("Persistent=true"),
        "a missed run while the machine was off is caught up: {timer}"
    );
    assert!(
        !timer.contains("OnUnitActiveSec"),
        "systemd honours Persistent= only on calendar timers, so the schedule \
         must be expressed as one rather than as a monotonic interval: {timer}"
    );
    assert!(timer.contains("WantedBy=timers.target"));
}

#[test]
fn launchd_agent_runs_collect_every_nine_hundred_seconds() {
    let plist = scheduler::render_launchd_plist(&spec());

    assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
    assert!(plist.contains("<string>/usr/local/bin/aiu</string>"));
    assert!(plist.contains("<string>collect</string>"));
    assert!(
        plist.contains("<key>StartInterval</key>\n    <integer>900</integer>"),
        "15 minutes expressed in seconds: {plist}"
    );
    assert!(
        !plist.contains("KeepAlive"),
        "the agent must not be kept resident: {plist}"
    );
    assert!(
        plist.starts_with("<?xml"),
        "a plist parser needs the declaration: {plist}"
    );
}

#[test]
fn a_custom_interval_reaches_both_platforms() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).every_minutes(30);
    assert!(scheduler::render_systemd_timer(&spec).contains("OnCalendar=*:0/30"));
    assert!(scheduler::render_launchd_plist(&spec).contains("<integer>1800</integer>"));
}

#[test]
fn a_zero_interval_falls_back_to_the_default() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).every_minutes(0);
    assert_eq!(spec.interval_minutes, DEFAULT_INTERVAL_MINUTES);
}

#[test]
fn installing_on_linux_writes_user_units_and_enables_the_timer() {
    let root = temp_root("linux");
    let mut runner = RecordingRunner::default();

    let installed =
        scheduler::install_with_config(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();

    let unit_dir = root.join(".config/systemd/user");
    assert!(unit_dir.join("aiu-collect.service").is_file());
    assert!(unit_dir.join("aiu-collect.timer").is_file());
    assert!(read(&unit_dir.join("aiu-collect.service")).contains("aiu collect"));
    assert_eq!(
        installed,
        Installation {
            platform: Platform::Linux,
            interval_minutes: 15,
            unit_paths: vec![
                unit_dir.join("aiu-collect.service"),
                unit_dir.join("aiu-collect.timer"),
            ],
            activated: true,
        }
    );
    assert_eq!(
        runner.calls,
        vec![
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable --now {SYSTEMD_TIMER}"),
        ]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn installing_on_macos_writes_a_launch_agent_and_loads_it() {
    let root = temp_root("macos");
    let mut runner = RecordingRunner::default();

    let installed =
        scheduler::install_with_config(Platform::MacOs, &root, None, &spec(), &mut runner).unwrap();

    let plist = root.join("Library/LaunchAgents/com.aiu.collect.plist");
    assert!(plist.is_file());
    assert!(read(&plist).contains("StartInterval"));
    assert_eq!(installed.unit_paths, vec![plist.clone()]);
    assert!(installed.activated);
    assert_eq!(
        runner.calls,
        vec![
            format!("launchctl unload {}", plist.display()),
            format!("launchctl load {}", plist.display()),
        ],
        "unload-then-load makes reinstalling over an existing agent safe"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reinstalling_is_idempotent_and_rewrites_the_units() {
    let root = temp_root("idempotent");
    let mut runner = RecordingRunner::default();

    scheduler::install_with_config(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();
    let updated = ScheduleSpec::new(PathBuf::from("/opt/aiu")).every_minutes(20);
    let second =
        scheduler::install_with_config(Platform::Linux, &root, None, &updated, &mut runner)
            .unwrap();

    let service = read(&root.join(".config/systemd/user/aiu-collect.service"));
    assert!(service.contains("ExecStart=/opt/aiu collect"), "{service}");
    assert!(
        read(&root.join(".config/systemd/user/aiu-collect.timer")).contains("OnCalendar=*:0/20")
    );
    assert_eq!(second.interval_minutes, 20);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failed_activation_still_leaves_the_units_on_disk() {
    // The units are what survive a reboot; activation can be retried by hand.
    let root = temp_root("failed-activation");
    let mut runner = RecordingRunner {
        fail: true,
        ..RecordingRunner::default()
    };

    let installed =
        scheduler::install_with_config(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();

    assert!(!installed.activated, "activation failure is reported");
    assert!(
        installed.unit_paths.iter().all(|path| path.is_file()),
        "the units survive a failed activation and can be activated by hand"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn xdg_config_home_is_honoured_for_systemd_units() {
    let root = temp_root("xdg");
    let xdg = root.join("custom-config");
    let paths = scheduler::unit_paths_with_config(Platform::Linux, &root, Some(&xdg));
    assert_eq!(
        paths,
        vec![
            xdg.join("systemd/user/aiu-collect.service"),
            xdg.join("systemd/user/aiu-collect.timer"),
        ]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_current_platform_resolves_to_a_supported_scheduler() {
    let platform = scheduler::current_platform();
    if cfg!(target_os = "linux") {
        assert_eq!(platform, Some(Platform::Linux));
    } else if cfg!(target_os = "macos") {
        assert_eq!(platform, Some(Platform::MacOs));
    } else {
        assert_eq!(
            platform, None,
            "other platforms have no supported scheduler"
        );
    }
}

/// Renders the real units to disk so a human (and `systemd-analyze verify`)
/// can inspect exactly what setup installs.
#[test]
#[ignore = "developer aid: writes units under AIU_UNIT_DUMP for manual inspection"]
fn dump_units_for_inspection() {
    let Ok(dir) = std::env::var("AIU_UNIT_DUMP") else {
        return;
    };
    let root = PathBuf::from(dir);
    let mut runner = RecordingRunner::default();
    scheduler::install_with_config(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();
    scheduler::install_with_config(Platform::MacOs, &root, None, &spec(), &mut runner).unwrap();
}

/// A machine paired against a non-default relay, or using a non-default data
/// directory, must keep using them on every scheduled run. The scheduler runs
/// with none of the shell environment the user set up interactively, so
/// whatever the run depends on has to be captured into the unit at install
/// time — otherwise scheduled runs silently read a different database, or
/// queue records to a relay that never receives them.
#[test]
fn the_environment_a_run_depends_on_is_captured_into_the_units() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).with_environment(vec![
        (
            "AIU_RELAY_URL".to_string(),
            "https://relay.example".to_string(),
        ),
        ("AIU_DATA_DIR".to_string(), "/srv/aiu".to_string()),
    ]);

    let service = scheduler::render_systemd_service(&spec);
    assert!(
        service.contains("Environment=\"AIU_RELAY_URL=https://relay.example\""),
        "{service}"
    );
    assert!(
        service.contains("Environment=\"AIU_DATA_DIR=/srv/aiu\""),
        "{service}"
    );

    let plist = scheduler::render_launchd_plist(&spec);
    assert!(plist.contains("<key>EnvironmentVariables</key>"), "{plist}");
    assert!(plist.contains("<key>AIU_RELAY_URL</key>"), "{plist}");
    assert!(
        plist.contains("<string>https://relay.example</string>"),
        "{plist}"
    );
}

#[test]
fn units_carry_no_environment_block_when_there_is_nothing_to_capture() {
    let service = scheduler::render_systemd_service(&spec());
    let plist = scheduler::render_launchd_plist(&spec());
    assert!(!service.contains("Environment="), "{service}");
    assert!(!plist.contains("EnvironmentVariables"), "{plist}");
}

#[test]
fn captured_environment_values_are_escaped_for_their_format() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).with_environment(vec![(
        "AIU_RELAY_URL".to_string(),
        "https://host/a&b\"q\"".to_string(),
    )]);

    let plist = scheduler::render_launchd_plist(&spec);
    assert!(
        plist.contains("https://host/a&amp;b&quot;q&quot;"),
        "XML metacharacters must not break the plist: {plist}"
    );

    let service = scheduler::render_systemd_service(&spec);
    assert!(
        !service.contains("a&b\"q\""),
        "the quoted systemd value escapes its own quotes: {service}"
    );
}
