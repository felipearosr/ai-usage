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

use std::io;
use std::path::{Path, PathBuf};

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

/// Whether a value can be safely written into a unit file.
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

fn xdg_from_env() -> Option<PathBuf> {
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

/// The systemd unit that performs one collection and exits.
pub fn render_systemd_service(spec: &ScheduleSpec) -> String {
    let environment: String = spec
        .environment
        .iter()
        .map(|(key, value)| format!("Environment=\"{key}={}\"\n", escape_systemd(value)))
        .collect();
    format!(
        "[Unit]\n\
         Description=aiu — collect AI coding usage\n\
         Documentation=https://github.com/felipearosr/ai-usage\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         {environment}\
         ExecStart={} collect\n",
        spec.exe.display()
    )
}

/// Escapes a value for a double-quoted systemd `Environment=` assignment.
///
/// `%` starts a specifier that systemd expands; an unresolvable one makes it
/// drop the whole assignment, so a literal percent must be doubled.
fn escape_systemd(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

/// Escapes text for an XML character-data position in the plist.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The timer that starts the service on the scheduled interval.
pub fn render_systemd_timer(spec: &ScheduleSpec) -> String {
    format!(
        "[Unit]\n\
         Description=aiu — collect AI coding usage every {minutes} minutes\n\
         \n\
         [Timer]\n\
         OnCalendar=*:0/{minutes}\n\
         AccuracySec=1min\n\
         Persistent=true\n\
         Unit={service}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        minutes = spec.interval_minutes,
        service = SYSTEMD_SERVICE,
    )
}

/// The launchd agent. `StartInterval` re-runs a program that has exited, so
/// there is no `KeepAlive` and nothing stays resident.
pub fn render_launchd_plist(spec: &ScheduleSpec) -> String {
    let environment = if spec.environment.is_empty() {
        String::new()
    } else {
        let entries: String = spec
            .environment
            .iter()
            .map(|(key, value)| {
                format!(
                    "        <key>{}</key>\n        <string>{}</string>\n",
                    escape_xml(key),
                    escape_xml(value)
                )
            })
            .collect();
        format!("    <key>EnvironmentVariables</key>\n    <dict>\n{entries}    </dict>\n")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n    \
             <key>Label</key>\n    \
             <string>{label}</string>\n    \
             <key>ProgramArguments</key>\n    \
             <array>\n        \
                 <string>{exe}</string>\n        \
                 <string>collect</string>\n    \
             </array>\n    \
             <key>StartInterval</key>\n    \
             <integer>{seconds}</integer>\n    \
             <key>RunAtLoad</key>\n    \
             <false/>\n\
         {environment}\
         </dict>\n\
         </plist>\n",
        label = LAUNCHD_LABEL,
        exe = spec.exe.display(),
        seconds = spec.interval_seconds(),
    )
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
    spec: &ScheduleSpec,
    runner: &mut dyn CommandRunner,
) -> io::Result<Installation> {
    install_with_config(platform, home, xdg_from_env().as_deref(), spec, runner)
}

/// [`install`] with `XDG_CONFIG_HOME` supplied explicitly, so a caller (and
/// the test suite) can place units under a chosen root regardless of the
/// ambient environment.
pub fn install_with_config(
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
    install(platform, home, &spec, runner).ok()
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
