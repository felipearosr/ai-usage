//! OS scheduler installation (spec: "No daemon. OS scheduler (systemd user
//! timer / launchd) invokes `aiu collect` every 15 minutes").
//!
//! Nothing here stays resident. Both platforms install a unit that starts
//! `aiu collect`, lets it exit, and starts it again on the next tick, so idle
//! CPU and memory between runs are zero by construction.
//!
//! Two seams keep this testable off its native OS: [`Platform`] is an explicit
//! parameter rather than a `cfg!`, and activation goes through
//! [`CommandRunner`] rather than spawning `systemctl`/`launchctl` directly.

mod units;

use std::io;
use std::path::{Path, PathBuf};

use units::{parse_launchd, parse_systemd};
// The renderers stay part of the scheduler's public surface; `units` itself
// is private, so the split adds no new path and really is internal.
pub use units::{render_launchd_plist, render_systemd_service, render_systemd_timer};

/// The scheduled interval the spec fixes for collection.
pub const DEFAULT_INTERVAL_MINUTES: u64 = 15;

/// systemd unit names, also used to address the timer via `systemctl --user`.
pub const SYSTEMD_SERVICE: &str = "aiu-collect.service";
pub const SYSTEMD_TIMER: &str = "aiu-collect.timer";
/// launchd agent label, which doubles as the plist filename stem.
pub const LAUNCHD_LABEL: &str = "com.aiu.collect";

/// Which scheduler to install. Passed explicitly so both branches are
/// exercised by the test suite on either OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

/// The scheduler for the OS this binary is running on, or `None` where aiu
/// supports no scheduler.
pub fn current_platform() -> Option<Platform> {
    if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Platform::MacOs)
    } else {
        None
    }
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Linux => "systemd user timer",
            Platform::MacOs => "launchd agent",
        }
    }
}

/// What to schedule: which binary to invoke, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSpec {
    pub exe: PathBuf,
    pub interval_minutes: u64,
    /// Environment the scheduled run needs, captured at install time.
    ///
    /// A scheduled run inherits none of the shell environment the user set up
    /// interactively, so anything the run depends on — the relay URL, the
    /// data directory — has to be written into the unit. Without this a
    /// machine paired against a non-default relay would collect happily and
    /// queue to a relay that never receives it.
    pub environment: Vec<(String, String)>,
}

impl ScheduleSpec {
    pub fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            environment: Vec::new(),
        }
    }

    /// Captures environment variables into the unit.
    pub fn with_environment(mut self, environment: Vec<(String, String)>) -> Self {
        self.environment = environment;
        self
    }

    /// Overrides the interval, falling back to the default for any value that
    /// cannot be scheduled truthfully.
    ///
    /// Zero would mean "run continuously", which is the daemon the spec rules
    /// out. Anything that does not divide an hour is rejected for a subtler
    /// reason: the systemd schedule is a calendar expression, and
    /// `OnCalendar=*:0/60` is not a valid minute specification at all — the
    /// timer would fail to load and collection would stop silently. A
    /// non-divisor such as 7 loads but leaves a short gap at the top of each
    /// hour, so it is not "every 7 minutes" either, and would not match
    /// launchd's elapsed-time `StartInterval` on the other platform.
    pub fn every_minutes(mut self, minutes: u64) -> Self {
        self.interval_minutes = if schedulable(minutes) {
            minutes
        } else {
            DEFAULT_INTERVAL_MINUTES
        };
        self
    }

    fn interval_seconds(&self) -> u64 {
        self.interval_minutes * 60
    }
}

/// Whether an interval divides an hour, and so can be expressed identically
/// as a systemd calendar expression and a launchd elapsed interval.
fn schedulable(minutes: u64) -> bool {
    minutes > 0 && minutes < 60 && 60 % minutes == 0
}

/// Whether a value can be safely written into a unit file. Applied both when
/// capturing from the environment and again at each render seam, since that
/// is where an injected directive would actually land.
///
/// A newline would close the quoted `Environment=` assignment and let the
/// rest of the value inject directives into the unit; other control
/// characters are illegal in XML 1.0 and would corrupt the plist. Such a
/// value is certainly wrong rather than merely awkward, so it is dropped
/// rather than escaped.
pub fn is_capturable(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_control())
}

/// The result of installing: what was written, and whether the OS accepted
/// the activation. Units on disk survive a reboot even when activation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub platform: Platform,
    pub interval_minutes: u64,
    pub unit_paths: Vec<PathBuf>,
    pub activated: bool,
}

/// Runs the platform's activation command. Real installs shell out; tests
/// record the calls instead of mutating the developer's scheduler state.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<()>;
}

/// Spawns the command for real, discarding its output and treating a non-zero
/// exit as an error.
#[derive(Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<()> {
        let status = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{program} exited with status {status}"
            )))
        }
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// `XDG_CONFIG_HOME` as this process sees it. Passed explicitly into the
/// functions that need it so unit locations never depend on ambient state.
pub fn config_home_from_env() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Where this platform's unit files live. `XDG_CONFIG_HOME` is passed in
/// rather than read here, so the override is resolvable without depending on
/// process-wide environment.
pub fn unit_paths(platform: Platform, home: &Path, xdg_config_home: Option<&Path>) -> Vec<PathBuf> {
    match platform {
        Platform::Linux => {
            let base = xdg_config_home
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            let dir = base.join("systemd/user");
            vec![dir.join(SYSTEMD_SERVICE), dir.join(SYSTEMD_TIMER)]
        }
        Platform::MacOs => vec![home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"))],
    }
}

fn rendered_units(platform: Platform, spec: &ScheduleSpec) -> Vec<String> {
    match platform {
        Platform::Linux => vec![render_systemd_service(spec), render_systemd_timer(spec)],
        Platform::MacOs => vec![render_launchd_plist(spec)],
    }
}

/// Writes this platform's units under `home` and asks the OS to activate them.
///
/// Writing and activating are deliberately separate outcomes: a machine
/// without a running user session (a fresh container, an SSH login with no
/// D-Bus) cannot activate a systemd timer, but the units still belong on disk
/// so the next login picks them up. Such a run returns `activated: false`
/// rather than an error.
pub fn install(
    platform: Platform,
    home: &Path,
    xdg_config_home: Option<&Path>,
    spec: &ScheduleSpec,
    runner: &mut dyn CommandRunner,
) -> io::Result<Installation> {
    let paths = unit_paths(platform, home, xdg_config_home);
    for (path, contents) in paths.iter().zip(rendered_units(platform, spec)) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }

    let activated = activate(platform, &paths, runner).is_ok();

    Ok(Installation {
        platform,
        interval_minutes: spec.interval_minutes,
        unit_paths: paths,
        activated,
    })
}

fn activate(
    platform: Platform,
    paths: &[PathBuf],
    runner: &mut dyn CommandRunner,
) -> io::Result<()> {
    match platform {
        Platform::Linux => {
            runner.run("systemctl", &args(&["--user", "daemon-reload"]))?;
            runner.run(
                "systemctl",
                &args(&["--user", "enable", "--now", SYSTEMD_TIMER]),
            )
        }
        Platform::MacOs => {
            let plist = paths[0].display().to_string();
            // An existing agent must be unloaded before the replacement loads;
            // a first install has nothing to unload, so that failure is
            // expected and ignored.
            let _ = runner.run("launchctl", &args(&["unload", &plist]));
            runner.run("launchctl", &args(&["load", &plist]))
        }
    }
}

/// Installs the default collection schedule for the running platform,
/// invoking the currently running binary.
///
/// Returns `None` on an OS with no supported scheduler, or when the running
/// binary's own path cannot be resolved — neither is a reason to fail a setup
/// that otherwise succeeded, since `aiu collect` still works by hand.
pub fn install_default(home: &Path, runner: &mut dyn CommandRunner) -> Option<Installation> {
    let platform = current_platform()?;
    let exe = std::env::current_exe().ok()?;
    let spec = ScheduleSpec::new(exe).with_environment(inherited_environment());
    install(
        platform,
        home,
        config_home_from_env().as_deref(),
        &spec,
        runner,
    )
    .ok()
}

/// The schedule this machine would install right now, for comparison against
/// what is actually installed. `None` when the running binary's path cannot
/// be resolved, since there would be nothing to compare.
pub fn current_spec() -> Option<ScheduleSpec> {
    Some(ScheduleSpec::new(std::env::current_exe().ok()?).with_environment(inherited_environment()))
}

/// The environment variables a scheduled run must keep. Only variables the
/// user actually set are captured; defaults stay defaults.
///
/// `HOME` is deliberately absent — both `systemd --user` and launchd agents
/// set it themselves. `XDG_DATA_HOME` is present because `paths::data_dir`
/// reads it: a user who exports it only in their shell rc would otherwise
/// collect into one database interactively and another on the timer.
fn inherited_environment() -> Vec<(String, String)> {
    ["AIU_RELAY_URL", "AIU_DATA_DIR", "XDG_DATA_HOME"]
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| is_capturable(value))
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

/// One line describing what automatic collection is doing now.
pub fn describe(installation: Option<&Installation>) -> String {
    match installation {
        Some(install) if install.activated => format!(
            "{} runs `aiu collect` every {} minutes",
            install.platform.as_str(),
            install.interval_minutes
        ),
        Some(install) => format!(
            "{} written but not activated; run `aiu collect` by hand or activate it at next login",
            install.platform.as_str()
        ),
        None => "automatic collection is not installed; run `aiu collect` manually".to_string(),
    }
}

/// A schedule as it actually exists on disk, recovered from the unit files
/// themselves rather than from any record aiu keeps — the units are what the
/// OS acts on, and a sidecar could disagree with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSchedule {
    pub platform: Platform,
    /// What the units say they will run, in the same shape as the spec that
    /// would be installed now — so drift is a comparison of like with like.
    pub spec: ScheduleSpec,
    pub unit_paths: Vec<PathBuf>,
}

/// One way the installed schedule disagrees with what this machine would
/// install now. Each variant names the value, so the report can tell the user
/// what to fix rather than only that something is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    ExePath {
        installed: PathBuf,
        current: PathBuf,
    },
    Interval {
        installed: u64,
        current: u64,
    },
    Environment {
        key: String,
        installed: Option<String>,
        current: Option<String>,
    },
}

/// Whether every unit this platform needs is present. A systemd install is
/// two files, and one alone is not a working schedule.
pub fn is_installed(platform: Platform, home: &Path, xdg_config_home: Option<&Path>) -> bool {
    let paths = unit_paths(platform, home, xdg_config_home);
    !paths.is_empty() && paths.iter().all(|path| path.is_file())
}

/// Recovers the installed schedule from its unit files, or `None` when no
/// complete install is present or the units cannot be parsed.
pub fn read_installed(
    platform: Platform,
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> Option<InstalledSchedule> {
    let unit_paths = unit_paths(platform, home, xdg_config_home);
    if !is_installed(platform, home, xdg_config_home) {
        return None;
    }
    let contents = unit_paths
        .iter()
        .map(std::fs::read_to_string)
        .collect::<io::Result<Vec<_>>>()
        .ok()?;

    let parsed = match platform {
        Platform::Linux => parse_systemd(&contents[0], &contents[1])?,
        Platform::MacOs => parse_launchd(&contents[0])?,
    };

    Some(InstalledSchedule {
        platform,
        spec: ScheduleSpec {
            exe: parsed.exe,
            interval_minutes: parsed.interval_minutes,
            environment: parsed.environment,
        },
        unit_paths,
    })
}

/// Every way the installed schedule disagrees with `current`. An empty result
/// means a scheduled run would behave exactly as an interactive one.
pub fn drift(installed: &InstalledSchedule, current: &ScheduleSpec) -> Vec<Drift> {
    // Both sides are a ScheduleSpec, so every field compares directly.
    let installed = &installed.spec;
    let mut drift = Vec::new();
    if installed.exe != current.exe {
        drift.push(Drift::ExePath {
            installed: installed.exe.clone(),
            current: current.exe.clone(),
        });
    }
    if installed.interval_minutes != current.interval_minutes {
        drift.push(Drift::Interval {
            installed: installed.interval_minutes,
            current: current.interval_minutes,
        });
    }

    let lookup = |pairs: &[(String, String)], key: &str| {
        pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    let mut keys: Vec<&str> = installed
        .environment
        .iter()
        .chain(current.environment.iter())
        .map(|(key, _)| key.as_str())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let was = lookup(&installed.environment, key);
        let now = lookup(&current.environment, key);
        if was != now {
            drift.push(Drift::Environment {
                key: key.to_string(),
                installed: was,
                current: now,
            });
        }
    }
    drift
}

/// One line a user can act on.
pub fn describe_drift(drift: &Drift) -> String {
    match drift {
        Drift::ExePath { installed, current } => format!(
            "scheduled binary is {} but this is {}",
            installed.display(),
            current.display()
        ),
        Drift::Interval { installed, current } => {
            format!("scheduled every {installed} minutes, expected {current}")
        }
        Drift::Environment {
            key,
            installed,
            current,
        } => {
            // These values are echoed back to the user and land in pasted bug
            // reports; a relay URL can carry credentials.
            let was = installed.as_deref().map(crate::redact::url_userinfo);
            let now = current.as_deref().map(crate::redact::url_userinfo);
            match (was, now) {
                (Some(was), Some(now)) => format!("{key} is {was} in the schedule but {now} here"),
                (Some(was), None) => format!("{key} is {was} in the schedule but is not set here"),
                (None, Some(now)) => format!("{key} is {now} here but missing from the schedule"),
                (None, None) => format!("{key} differs"),
            }
        }
    }
}

/// Whether the OS reports the schedule as active. Unlike the unit files,
/// this can only be answered by asking the OS, so it needs a runner.
pub fn is_activated(platform: Platform, runner: &mut dyn CommandRunner) -> bool {
    match platform {
        Platform::Linux => runner
            .run(
                "systemctl",
                &args(&["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]),
            )
            .is_ok(),
        Platform::MacOs => runner
            .run("launchctl", &args(&["list", LAUNCHD_LABEL]))
            .is_ok(),
    }
}

/// Deactivates and deletes this platform's units. Returns whether anything
/// was actually removed, so a caller can distinguish "removed" from "there
/// was nothing there" without treating the latter as an error.
pub fn uninstall(
    platform: Platform,
    home: &Path,
    xdg_config_home: Option<&Path>,
    runner: &mut dyn CommandRunner,
) -> io::Result<bool> {
    let paths = unit_paths(platform, home, xdg_config_home);
    if !paths.iter().any(|path| path.exists()) {
        return Ok(false);
    }

    // Deactivation is best-effort for the same reason activation is: a
    // machine with no user session cannot talk to systemd, but the units
    // should still come off disk.
    match platform {
        Platform::Linux => {
            let _ = runner.run(
                "systemctl",
                &args(&["--user", "disable", "--now", SYSTEMD_TIMER]),
            );
        }
        Platform::MacOs => {
            let _ = runner.run(
                "launchctl",
                &args(&["unload", &paths[0].display().to_string()]),
            );
        }
    }

    let mut removed = false;
    for path in &paths {
        match std::fs::remove_file(path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_labels_name_the_mechanism() {
        assert_eq!(Platform::Linux.as_str(), "systemd user timer");
        assert_eq!(Platform::MacOs.as_str(), "launchd agent");
    }

    #[test]
    fn interval_seconds_converts_minutes() {
        assert_eq!(
            ScheduleSpec::new(PathBuf::from("aiu")).interval_seconds(),
            900
        );
    }
}
