//! Acceptance tests for the one-line installer (issue 12).
//!
//! The script is the one piece of aiu a user runs before they have aiu, so it
//! is tested the way a user meets it: a real `sh install.sh` against a real
//! release tree. The tree is served over `file://`, which curl handles like
//! any other URL, so nothing here reaches the network.
//!
//! This seam owns: platform/arch → artifact selection for all four targets,
//! integrity verification (a tampered or unlisted artifact is refused before
//! anything is written), and upgrade-in-place.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// `uname -s` / `uname -m` pairs a user could actually present, and the
/// artifact each must resolve to.
const DETECTION: [(&str, &str, &str); 6] = [
    ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    ("Linux", "amd64", "x86_64-unknown-linux-musl"),
    ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
    ("Linux", "arm64", "aarch64-unknown-linux-musl"),
    ("Darwin", "x86_64", "x86_64-apple-darwin"),
    ("Darwin", "arm64", "aarch64-apple-darwin"),
];

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("aiu-install-{tag}-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Publishes a release: one tarball per target, each holding an `aiu`
    /// binary whose contents name the target so the test can tell which
    /// artifact was chosen, plus the `SHA256SUMS` and `VERSION` files the
    /// installer resolves against.
    fn publish(&self, version: &str) -> Release {
        let dir = self.path(&format!("releases/download/v{version}"));
        fs::create_dir_all(&dir).unwrap();
        for target in TARGETS {
            let stage = self.path(&format!("stage/{version}/{target}"));
            fs::create_dir_all(&stage).unwrap();
            fs::write(
                stage.join("aiu"),
                format!("#!/bin/sh\necho aiu {version} {target}\n"),
            )
            .unwrap();
            let status = Command::new("tar")
                .arg("-czf")
                .arg(dir.join(format!("aiu-{version}-{target}.tar.gz")))
                .arg("-C")
                .arg(&stage)
                .arg("aiu")
                .status()
                .unwrap();
            assert!(status.success(), "tar failed for {target}");
        }

        let latest = self.path("releases/latest/download");
        fs::create_dir_all(&latest).unwrap();
        fs::write(latest.join("VERSION"), format!("{version}\n")).unwrap();

        let release = Release {
            dir,
            version: version.to_string(),
        };
        release.rewrite_checksums();
        release
    }

    fn base_url(&self) -> String {
        format!("file://{}/releases", self.root.display())
    }

    /// Runs the installer with the sandbox's release tree, home, and install
    /// directory, optionally pretending to be another platform.
    fn install(&self, uname: Option<(&str, &str)>, extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg(repo_root().join("install.sh"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("AIU_BASE_URL", self.base_url())
            .env("AIU_INSTALL_DIR", self.path("home/.local/bin"));
        if let Some((os, arch)) = uname {
            cmd.env("AIU_UNAME_S", os).env("AIU_UNAME_M", arch);
        }
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let out = cmd.output().unwrap();
        Output {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn installed(&self) -> Option<String> {
        fs::read_to_string(self.path("home/.local/bin/aiu")).ok()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Release {
    dir: PathBuf,
    version: String,
}

impl Release {
    fn artifact(&self, target: &str) -> PathBuf {
        self.dir
            .join(format!("aiu-{}-{target}.tar.gz", self.version))
    }

    /// Regenerates `SHA256SUMS` from whatever the artifacts currently contain.
    fn rewrite_checksums(&self) {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                name.ends_with(".tar.gz").then_some(name)
            })
            .collect();
        names.sort();
        let mut sums = String::new();
        for name in names {
            sums.push_str(&format!("{}  {name}\n", sha256(&self.dir.join(&name))));
        }
        fs::write(self.dir.join("SHA256SUMS"), sums).unwrap();
    }
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Output {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn sha256(path: &Path) -> String {
    // `sha256sum` on Linux, `shasum -a 256` on macOS — the same pair the
    // installer itself picks between.
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .arg("-a")
                .arg("256")
                .arg(path)
                .output()
        })
        .expect("a sha256 tool is required to run these tests");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn installs_the_artifact_matching_the_detected_platform() {
    let sandbox = Sandbox::new("detect");
    sandbox.publish("1.2.3");

    for (os, arch, target) in DETECTION {
        let out = sandbox.install(Some((os, arch)), &[]);
        assert!(out.ok, "install failed for {os}/{arch}: {}", out.all());
        let installed = sandbox.installed().expect("binary should be installed");
        assert!(
            installed.contains(target),
            "{os}/{arch} should install {target}, got: {installed}"
        );
    }
}

#[test]
fn installing_leaves_no_staging_file_behind() {
    // The binary is staged inside the install directory so the final step is
    // an atomic rename; that staging file must not survive the install, and
    // must not survive a failed one either.
    let sandbox = Sandbox::new("staging");
    let release = sandbox.publish("1.2.3");
    assert!(sandbox.install(Some(("Linux", "x86_64")), &[]).ok);

    fs::write(release.artifact("x86_64-unknown-linux-musl"), b"tampered").unwrap();
    assert!(!sandbox.install(Some(("Linux", "x86_64")), &[]).ok);

    let entries: Vec<String> = fs::read_dir(sandbox.path("home/.local/bin"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["aiu".to_string()], "left behind: {entries:?}");
}

#[test]
fn installed_binary_is_executable() {
    let sandbox = Sandbox::new("exec");
    sandbox.publish("1.2.3");
    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(out.ok, "{}", out.all());

    let status = Command::new(sandbox.path("home/.local/bin/aiu"))
        .status()
        .expect("installed file should be runnable");
    assert!(status.success());
}

#[test]
fn tampered_artifact_is_refused_and_nothing_is_installed() {
    let sandbox = Sandbox::new("tamper");
    let release = sandbox.publish("1.2.3");

    // Swap the payload after the checksums were published — a CDN cache
    // poisoning or a substituted release asset looks exactly like this.
    fs::write(
        release.artifact("x86_64-unknown-linux-musl"),
        b"totally not a tarball",
    )
    .unwrap();

    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(!out.ok, "tampered artifact must be refused: {}", out.all());
    assert!(
        out.all().to_lowercase().contains("checksum"),
        "the refusal should name the failed check: {}",
        out.all()
    );
    assert!(
        sandbox.installed().is_none(),
        "nothing may be installed when verification fails"
    );
}

#[test]
fn artifact_missing_from_the_checksum_file_is_refused() {
    let sandbox = Sandbox::new("unlisted");
    let release = sandbox.publish("1.2.3");

    // An artifact nobody vouched for is not the same as a valid one: with no
    // line to compare against there is nothing to verify, so it is refused
    // rather than installed unchecked.
    let sums = fs::read_to_string(release.dir.join("SHA256SUMS")).unwrap();
    let kept: String = sums
        .lines()
        .filter(|line| !line.contains("x86_64-unknown-linux-musl"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(release.dir.join("SHA256SUMS"), kept).unwrap();

    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(!out.ok, "unlisted artifact must be refused: {}", out.all());
    assert!(sandbox.installed().is_none());
}

#[test]
fn unsupported_platform_is_refused_by_name() {
    let sandbox = Sandbox::new("unsupported");
    sandbox.publish("1.2.3");

    for (os, arch) in [("Windows_NT", "x86_64"), ("Linux", "riscv64")] {
        let out = sandbox.install(Some((os, arch)), &[]);
        assert!(!out.ok, "{os}/{arch} must be refused: {}", out.all());
        assert!(
            out.all().contains(os) || out.all().contains(arch),
            "the refusal should name what was unsupported: {}",
            out.all()
        );
        assert!(sandbox.installed().is_none());
    }
}

#[test]
fn a_stray_signature_file_does_not_change_what_is_verified() {
    // The installer makes no signature claim: there is no signing key to
    // check one against. What matters is that publishing a `.minisig`
    // neither weakens the checksum check nor silently appears to strengthen
    // it, so that nobody reads the file's presence as verification.
    let sandbox = Sandbox::new("signature");
    let release = sandbox.publish("1.2.3");
    fs::write(
        release.dir.join("SHA256SUMS.minisig"),
        "untrusted comment: signature\nRWQfake\n",
    )
    .unwrap();

    assert!(sandbox.install(Some(("Linux", "x86_64")), &[]).ok);
    assert!(sandbox.installed().is_some());

    // The checksum still governs: a tampered artifact is refused whether or
    // not a signature file sits beside it.
    fs::write(
        release.artifact("x86_64-unknown-linux-musl"),
        b"totally not a tarball",
    )
    .unwrap();
    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().to_lowercase().contains("checksum"));
}

#[test]
fn version_is_resolved_from_the_release_tree_and_overridable() {
    let sandbox = Sandbox::new("version");
    sandbox.publish("1.0.0");
    sandbox.publish("2.0.0");
    fs::write(sandbox.path("releases/latest/download/VERSION"), "2.0.0\n").unwrap();

    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(out.ok, "{}", out.all());
    assert!(sandbox.installed().unwrap().contains("2.0.0"));

    let out = sandbox.install(Some(("Linux", "x86_64")), &[("AIU_VERSION", "1.0.0")]);
    assert!(out.ok, "{}", out.all());
    assert!(sandbox.installed().unwrap().contains("1.0.0"));
}

#[test]
fn upgrading_replaces_the_binary_and_leaves_local_state_alone() {
    let sandbox = Sandbox::new("upgrade");
    sandbox.publish("1.0.0");
    fs::write(sandbox.path("releases/latest/download/VERSION"), "1.0.0\n").unwrap();
    assert!(sandbox.install(Some(("Linux", "x86_64")), &[]).ok);

    // Everything an existing install owns lives under the data directory and
    // the scheduler's unit files; neither is the installer's to touch.
    let data = sandbox.path("home/.local/share/aiu");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("usage.db"), b"existing database").unwrap();
    let units = sandbox.path("home/.config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    fs::write(units.join("aiu-collect.timer"), b"existing timer").unwrap();

    sandbox.publish("2.0.0");
    fs::write(sandbox.path("releases/latest/download/VERSION"), "2.0.0\n").unwrap();
    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(out.ok, "{}", out.all());

    assert!(sandbox.installed().unwrap().contains("2.0.0"));
    assert_eq!(
        fs::read_to_string(data.join("usage.db")).unwrap(),
        "existing database"
    );
    assert_eq!(
        fs::read_to_string(units.join("aiu-collect.timer")).unwrap(),
        "existing timer"
    );
}

#[test]
fn install_directory_outside_path_is_reported_not_hidden() {
    let sandbox = Sandbox::new("path");
    sandbox.publish("1.2.3");
    let out = sandbox.install(Some(("Linux", "x86_64")), &[]);
    assert!(out.ok, "{}", out.all());
    // The sandbox install dir is never on PATH, so the user must be told
    // where the binary went and how to reach it.
    let text = out.all();
    assert!(
        text.contains("PATH"),
        "a binary installed off PATH should say so: {text}"
    );
    assert!(text.contains(&sandbox.path("home/.local/bin").display().to_string()));
}

#[test]
fn the_script_stays_small_and_readable() {
    // "Script small and readable" is a spec requirement, not a style
    // preference: it is piped into a shell sight-unseen, so it has to be
    // something a cautious user can actually read first.
    let script = fs::read_to_string(repo_root().join("install.sh")).unwrap();
    let lines = script.lines().count();
    assert!(lines < 200, "install.sh grew to {lines} lines");
    assert!(
        script.starts_with("#!/bin/sh"),
        "the installer must be POSIX sh, not bash: many minimal images have no bash"
    );
}
