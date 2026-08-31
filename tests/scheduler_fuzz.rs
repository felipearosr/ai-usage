//! Adversarial input for the unit-file parsers (issue 14). They read files
//! that a user can hand-edit, so malformed, truncated, and non-ASCII content
//! must produce `None` rather than a panic. Rust slices by byte offset, so
//! multi-byte UTF-8 is the interesting case.

use aiu::scheduler::{self, Platform, ScheduleSpec};
use std::path::PathBuf;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiu-fuzz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_units(root: &std::path::Path, service: &str, timer: &str) {
    let dir = root.join(".config/systemd/user");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("aiu-collect.service"), service).unwrap();
    std::fs::write(dir.join("aiu-collect.timer"), timer).unwrap();
}

fn write_plist(root: &std::path::Path, body: &str) {
    let dir = root.join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("com.aiu.collect.plist"), body).unwrap();
}

#[test]
fn malformed_systemd_units_read_back_as_absent_not_a_panic() {
    let cases = [
        ("", ""),
        ("[Service]\n", "[Timer]\n"),
        ("ExecStart=\n", "OnCalendar=*:0/15\n"),
        ("ExecStart=\"unterminated collect\n", "OnCalendar=*:0/15\n"),
        (
            "ExecStart=\"/bin/aiu\" collect\n",
            "OnCalendar=*:0/notanumber\n",
        ),
        ("ExecStart=\"/bin/aiu\" collect\n", "OnCalendar=*:0/\n"),
        // Multi-byte characters around every slice boundary.
        (
            "ExecStart=\"/opt/日本語/aiu\" collect\n",
            "OnCalendar=*:0/15\n",
        ),
        ("ExecStart=\"é\n", "OnCalendar=*:0/15\n"),
        (
            "Environment=\"é\nExecStart=\"/bin/aiu\" collect\n",
            "OnCalendar=*:0/15\n",
        ),
        (
            "ExecStart=\"/bin/aiu\" collect\nEnvironment=\"NOEQUALS\"\n",
            "OnCalendar=*:0/15\n",
        ),
        ("ExecStart=\"\\\\\"\n", "OnCalendar=*:0/15\n"),
    ];

    for (index, (service, timer)) in cases.iter().enumerate() {
        let root = temp_root(&format!("systemd-{index}"));
        write_units(&root, service, timer);
        // Must not panic; a valid parse is acceptable, a refusal is acceptable.
        let _ = scheduler::read_installed(Platform::Linux, &root, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn a_unit_with_a_multibyte_path_round_trips_intact() {
    let root = temp_root("multibyte");
    let mut runner = NoopRunner;
    let spec = ScheduleSpec::new(PathBuf::from("/opt/日本語/aiu")).with_environment(vec![(
        "AIU_DATA_DIR".to_string(),
        "/srv/données".to_string(),
    )]);

    for platform in [Platform::Linux, Platform::MacOs] {
        scheduler::install(platform, &root, None, &spec, &mut runner).unwrap();
        let read = scheduler::read_installed(platform, &root, None)
            .unwrap_or_else(|| panic!("{platform:?} reads back"));
        assert_eq!(read.exe, spec.exe, "{platform:?}");
        assert_eq!(read.environment, spec.environment, "{platform:?}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn malformed_plists_read_back_as_absent_not_a_panic() {
    let cases = [
        "",
        "<plist></plist>",
        "<key>ProgramArguments</key>",
        "<key>ProgramArguments</key><array><string>/bin/aiu</string></array>",
        "<key>ProgramArguments</key><array><string>é</string></array><key>StartInterval</key><integer>900</integer>",
        "<key>ProgramArguments</key><array><string>/bin/aiu</string></array><key>StartInterval</key><integer>notanumber</integer>",
        "<key>ProgramArguments</key><array><string>/bin/aiu</string></array><key>StartInterval</key><integer>900</integer><key>EnvironmentVariables</key><dict><key>A</key></dict>",
        "<key>ProgramArguments</key><array><string>/bin/aiu</string></array><key>StartInterval</key><integer>900</integer><key>EnvironmentVariables</key><dict><key>é</key><string>ü</string></dict>",
        "<key>StartInterval</key><integer>900</integer>",
    ];

    for (index, body) in cases.iter().enumerate() {
        let root = temp_root(&format!("plist-{index}"));
        write_plist(&root, body);
        let _ = scheduler::read_installed(Platform::MacOs, &root, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}

struct NoopRunner;

impl scheduler::CommandRunner for NoopRunner {
    fn run(&mut self, _program: &str, _args: &[String]) -> std::io::Result<()> {
        Ok(())
    }
}
