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
        service.contains(r#"ExecStart="/usr/local/bin/aiu" collect"#),
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

/// `OnCalendar=*:0/N` is only a valid minute specification when N divides an
/// hour: systemd rejects `*:0/60` outright, and a non-divisor like 7 leaves a
/// short gap at the top of every hour, so it is not "every 7 minutes" either.
/// An interval that cannot be scheduled truthfully on both platforms falls
/// back to the default rather than rendering a unit systemd refuses to load —
/// which would silently disable collection altogether.
#[test]
fn an_interval_that_does_not_divide_an_hour_falls_back_to_the_default() {
    for minutes in [7, 45, 60, 90, 3600] {
        let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).every_minutes(minutes);
        assert_eq!(
            spec.interval_minutes, DEFAULT_INTERVAL_MINUTES,
            "{minutes} does not divide an hour and must not be scheduled"
        );
    }
    for minutes in [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30] {
        let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).every_minutes(minutes);
        assert_eq!(spec.interval_minutes, minutes, "{minutes} divides an hour");
        assert!(
            scheduler::render_systemd_timer(&spec).contains(&format!("OnCalendar=*:0/{minutes}"))
        );
    }
}

#[test]
fn installing_on_linux_writes_user_units_and_enables_the_timer() {
    let root = temp_root("linux");
    let mut runner = RecordingRunner::default();

    let installed = scheduler::install(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();

    let unit_dir = root.join(".config/systemd/user");
    assert!(unit_dir.join("aiu-collect.service").is_file());
    assert!(unit_dir.join("aiu-collect.timer").is_file());
    assert!(read(&unit_dir.join("aiu-collect.service")).contains(r#""/usr/local/bin/aiu" collect"#));
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

    let installed = scheduler::install(Platform::MacOs, &root, None, &spec(), &mut runner).unwrap();

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

    scheduler::install(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();
    let updated = ScheduleSpec::new(PathBuf::from("/opt/aiu")).every_minutes(20);
    let second = scheduler::install(Platform::Linux, &root, None, &updated, &mut runner).unwrap();

    let service = read(&root.join(".config/systemd/user/aiu-collect.service"));
    assert!(
        service.contains(r#"ExecStart="/opt/aiu" collect"#),
        "{service}"
    );
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

    let installed = scheduler::install(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();

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
    let paths = scheduler::unit_paths(Platform::Linux, &root, Some(&xdg));
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
    scheduler::install(Platform::Linux, &root, None, &spec(), &mut runner).unwrap();
    scheduler::install(Platform::MacOs, &root, None, &spec(), &mut runner).unwrap();
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
        service.contains(r#"Environment="AIU_RELAY_URL=https://host/a&b\"q\"""#),
        "the quoted systemd value escapes its own quotes: {service}"
    );
}

/// systemd expands `%` specifiers inside `Environment=`. An unescaped `%`
/// makes it drop the whole assignment ("Failed to resolve specifiers ...
/// ignoring") — so a percent-encoded relay URL would leave scheduled runs
/// pointed at the default relay, which is the exact failure this capture
/// exists to prevent.
#[test]
fn a_percent_in_a_captured_value_is_escaped_for_systemd() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).with_environment(vec![(
        "AIU_RELAY_URL".to_string(),
        "https://host/a%20b".to_string(),
    )]);

    let service = scheduler::render_systemd_service(&spec);
    assert!(
        service.contains(r#"Environment="AIU_RELAY_URL=https://host/a%%20b""#),
        "a literal percent is written %% so systemd does not treat it as a specifier: {service}"
    );

    // The plist has no specifier syntax; the percent stays literal there.
    assert!(scheduler::render_launchd_plist(&spec).contains("https://host/a%20b"));
}

/// A value carrying a newline could close the quoted `Environment=`
/// assignment and inject further directives into the unit. Such a value is
/// certainly wrong rather than merely awkward, so it is dropped instead of
/// escaped.
#[test]
fn a_captured_value_containing_a_control_character_is_refused() {
    assert!(scheduler::is_capturable("https://host/ok"));
    assert!(scheduler::is_capturable("https://relay.example"));
    assert!(!scheduler::is_capturable("https://host\nExecStart=/bin/sh"));
    assert!(!scheduler::is_capturable("https://host\u{0}x"));
    assert!(!scheduler::is_capturable(""));
}

/// systemd expands `%` specifiers in `ExecStart` too, and an unresolvable one
/// is fatal to the whole unit ("Unit configuration has fatal error, unit will
/// not be started") rather than costing a single variable. An unquoted space
/// is just as bad in a quieter way: systemd splits the command on whitespace,
/// so `/opt/with space/aiu` resolves to `/opt/with`. The exe path comes from
/// `current_exe()`, so both are reachable from an ordinary home directory.
#[test]
fn the_executable_path_is_quoted_and_escaped_in_the_systemd_unit() {
    let spec = ScheduleSpec::new(PathBuf::from("/opt/a%20b/with space/aiu"));
    let service = scheduler::render_systemd_service(&spec);

    assert!(
        service.contains(r#"ExecStart="/opt/a%%20b/with space/aiu" collect"#),
        "the path is quoted as one argument and its percent doubled: {service}"
    );
}

/// The same path in the plist sits in an XML character-data position, so a
/// path containing an XML metacharacter must be escaped for that format
/// instead.
#[test]
fn the_executable_path_is_xml_escaped_in_the_launchd_plist() {
    let spec = ScheduleSpec::new(PathBuf::from("/opt/a&b/aiu"));
    let plist = scheduler::render_launchd_plist(&spec);

    assert!(
        plist.contains("<string>/opt/a&amp;b/aiu</string>"),
        "{plist}"
    );
    assert!(
        !plist.contains("<string>/opt/a&b/aiu</string>"),
        "a raw ampersand would make the plist unparsable: {plist}"
    );
}

/// The escaping order matters when a value carries both a backslash and a
/// percent: doubling backslashes must not go on to double the percent's, and
/// the percent pass must not disturb the backslash escapes.
#[test]
fn a_value_carrying_both_a_backslash_and_a_percent_escapes_once_each() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
        .with_environment(vec![("AIU_DATA_DIR".to_string(), r"a\b100%".to_string())]);

    let service = scheduler::render_systemd_service(&spec);
    assert!(
        service.contains(r#"Environment="AIU_DATA_DIR=a\\b100%%""#),
        "{service}"
    );
}

/// The capture filter is a courtesy at one call site; the render seam is
/// where an injected directive would actually land, so it refuses such a
/// value too rather than trusting every caller of `with_environment`.
#[test]
fn a_control_carrying_value_never_reaches_a_rendered_unit() {
    let spec = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu")).with_environment(vec![
        (
            "AIU_RELAY_URL".to_string(),
            "https://host\nExecStart=/bin/sh".to_string(),
        ),
        ("AIU_DATA_DIR".to_string(), "/srv/aiu".to_string()),
    ]);

    let service = scheduler::render_systemd_service(&spec);
    assert!(
        !service.contains("/bin/sh"),
        "the injected directive is dropped, not escaped: {service}"
    );
    assert!(
        service.contains(r#"Environment="AIU_DATA_DIR=/srv/aiu""#),
        "the well-formed variable beside it still survives: {service}"
    );

    let plist = scheduler::render_launchd_plist(&spec);
    assert!(!plist.contains("/bin/sh"), "{plist}");
    assert!(plist.contains("<key>AIU_DATA_DIR</key>"), "{plist}");
}

/// Schedule lifecycle (issue 14): reading an installed schedule back off
/// disk, detecting drift against the current environment, and removing it.
mod lifecycle {
    use super::*;
    use aiu::scheduler::{Drift, InstalledSchedule};

    fn spec_with_env() -> ScheduleSpec {
        ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
            .every_minutes(20)
            .with_environment(vec![
                (
                    "AIU_RELAY_URL".to_string(),
                    "https://relay.example".to_string(),
                ),
                ("AIU_DATA_DIR".to_string(), "/srv/aiu".to_string()),
            ])
    }

    /// The unit files are the scheduled state, so what was installed has to be
    /// recoverable from them alone — not from a sidecar that could desync.
    #[test]
    fn an_installed_schedule_round_trips_through_the_unit_files() {
        for platform in [Platform::Linux, Platform::MacOs] {
            let root = temp_root(&format!("roundtrip-{platform:?}"));
            let mut runner = RecordingRunner::default();
            let spec = spec_with_env();
            scheduler::install(platform, &root, None, &spec, &mut runner).unwrap();

            let read = scheduler::read_installed(platform, &root, None)
                .unwrap_or_else(|| panic!("{platform:?} schedule reads back"));

            assert_eq!(read.platform, platform);
            assert_eq!(read.spec.exe, spec.exe, "{platform:?}");
            assert_eq!(read.spec.interval_minutes, 20, "{platform:?}");
            assert_eq!(read.spec.environment, spec.environment, "{platform:?}");

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Values that needed escaping on the way in must come back out intact,
    /// or drift detection would report a difference that is not real.
    #[test]
    fn values_needing_escaping_survive_the_round_trip() {
        for platform in [Platform::Linux, Platform::MacOs] {
            let root = temp_root(&format!("escape-roundtrip-{platform:?}"));
            let mut runner = RecordingRunner::default();
            let spec = ScheduleSpec::new(PathBuf::from("/opt/a&b/aiu")).with_environment(vec![(
                "AIU_RELAY_URL".to_string(),
                r#"https://h/a%20b&c"d\e"#.to_string(),
            )]);
            scheduler::install(platform, &root, None, &spec, &mut runner).unwrap();

            let read = scheduler::read_installed(platform, &root, None).unwrap();

            assert_eq!(read.spec.exe, spec.exe, "{platform:?}");
            assert_eq!(read.spec.environment, spec.environment, "{platform:?}");

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn nothing_is_read_back_when_no_schedule_is_installed() {
        let root = temp_root("absent");
        assert!(scheduler::read_installed(Platform::Linux, &root, None).is_none());
        assert!(scheduler::read_installed(Platform::MacOs, &root, None).is_none());
        assert!(!scheduler::is_installed(Platform::Linux, &root, None));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partially_present_install_does_not_read_back_as_installed() {
        // A systemd install is two files; one alone is not a schedule.
        let root = temp_root("partial");
        let mut runner = RecordingRunner::default();
        scheduler::install(Platform::Linux, &root, None, &spec_with_env(), &mut runner).unwrap();
        std::fs::remove_file(root.join(".config/systemd/user/aiu-collect.timer")).unwrap();

        assert!(!scheduler::is_installed(Platform::Linux, &root, None));
        assert!(scheduler::read_installed(Platform::Linux, &root, None).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unchanged_environment_reports_no_drift() {
        let installed = InstalledSchedule {
            platform: Platform::Linux,
            spec: ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
                .every_minutes(20)
                .with_environment(spec_with_env().environment),
            unit_paths: Vec::new(),
        };
        assert!(scheduler::drift(&installed, &spec_with_env()).is_empty());
    }

    /// Drift has to name the value that changed: "your schedule is stale" is
    /// not something a user can act on.
    #[test]
    fn drift_names_each_value_that_changed() {
        let installed = InstalledSchedule {
            platform: Platform::Linux,
            spec: ScheduleSpec::new(PathBuf::from("/old/bin/aiu")).with_environment(vec![
                (
                    "AIU_RELAY_URL".to_string(),
                    "https://old.example".to_string(),
                ),
                ("AIU_DATA_DIR".to_string(), "/srv/aiu".to_string()),
            ]),
            unit_paths: Vec::new(),
        };
        let current = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
            .every_minutes(20)
            .with_environment(vec![
                (
                    "AIU_RELAY_URL".to_string(),
                    "https://new.example".to_string(),
                ),
                (
                    "XDG_DATA_HOME".to_string(),
                    "/home/me/.local/share".to_string(),
                ),
            ]);

        let drift = scheduler::drift(&installed, &current);

        assert!(drift.contains(&Drift::ExePath {
            installed: PathBuf::from("/old/bin/aiu"),
            current: PathBuf::from("/usr/local/bin/aiu"),
        }));
        assert!(drift.contains(&Drift::Interval {
            installed: 15,
            current: 20
        }));
        assert!(drift.contains(&Drift::Environment {
            key: "AIU_RELAY_URL".to_string(),
            installed: Some("https://old.example".to_string()),
            current: Some("https://new.example".to_string()),
        }));
        assert!(
            drift.contains(&Drift::Environment {
                key: "AIU_DATA_DIR".to_string(),
                installed: Some("/srv/aiu".to_string()),
                current: None,
            }),
            "a variable that is no longer set is drift too: {drift:?}"
        );
        assert!(drift.contains(&Drift::Environment {
            key: "XDG_DATA_HOME".to_string(),
            installed: None,
            current: Some("/home/me/.local/share".to_string()),
        }));
    }

    /// This is the failure the drift check exists for: a data directory that
    /// changed after install means the timer collects into one database while
    /// reports read another.
    #[test]
    fn a_changed_data_directory_is_reported_as_drift() {
        let installed = InstalledSchedule {
            platform: Platform::Linux,
            spec: ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
                .with_environment(vec![("AIU_DATA_DIR".to_string(), "/old/db".to_string())]),
            unit_paths: Vec::new(),
        };
        let current = ScheduleSpec::new(PathBuf::from("/usr/local/bin/aiu"))
            .with_environment(vec![("AIU_DATA_DIR".to_string(), "/new/db".to_string())]);

        let drift = scheduler::drift(&installed, &current);
        assert_eq!(drift.len(), 1);
        assert!(scheduler::describe_drift(&drift[0]).contains("AIU_DATA_DIR"));
    }

    #[test]
    fn reinstalling_repairs_drift() {
        let root = temp_root("repair");
        let mut runner = RecordingRunner::default();
        scheduler::install(Platform::Linux, &root, None, &spec_with_env(), &mut runner).unwrap();

        let repaired = ScheduleSpec::new(PathBuf::from("/opt/aiu"))
            .every_minutes(30)
            .with_environment(vec![("AIU_DATA_DIR".to_string(), "/new".to_string())]);
        scheduler::install(Platform::Linux, &root, None, &repaired, &mut runner).unwrap();

        let read = scheduler::read_installed(Platform::Linux, &root, None).unwrap();
        assert!(scheduler::drift(&read, &repaired).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_a_schedule_deactivates_and_deletes_the_units() {
        for platform in [Platform::Linux, Platform::MacOs] {
            let root = temp_root(&format!("remove-{platform:?}"));
            let mut runner = RecordingRunner::default();
            scheduler::install(platform, &root, None, &spec_with_env(), &mut runner).unwrap();
            runner.calls.clear();

            let removed = scheduler::uninstall(platform, &root, None, &mut runner).unwrap();

            assert!(removed, "{platform:?} reports that it removed something");
            assert!(!scheduler::is_installed(platform, &root, None));
            assert!(
                !runner.calls.is_empty(),
                "{platform:?} asked the OS to deactivate it first: {:?}",
                runner.calls
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn removing_a_schedule_that_was_never_installed_succeeds_quietly() {
        let root = temp_root("remove-absent");
        let mut runner = RecordingRunner::default();

        let removed = scheduler::uninstall(Platform::Linux, &root, None, &mut runner).unwrap();

        assert!(!removed, "nothing was there to remove");
        let _ = std::fs::remove_dir_all(&root);
    }
}
