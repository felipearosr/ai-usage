//! Local source-file discovery.
//!
//! Finds the on-disk persistence each source leaves behind, so collection can
//! read new records without a resident daemon. This is the full recursive
//! file listing, gated by the cheap source detection in [`crate::sources`]
//! (issue 07) so absent sources never trigger a scan.
//!
//! Claude Code: `~/.claude/projects/**/*.jsonl` (session transcripts).
//! Codex: `~/.codex/sessions/**/rollout-*.jsonl` (rollout files).
//! OpenCode Go: `~/.local/share/opencode/storage/message/**/*.json` (one JSON
//! message per file), honoring `XDG_DATA_HOME` the same way source detection
//! does.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct DiscoveredSource {
    pub source: &'static str,
    pub files: Vec<PathBuf>,
}

/// Locates the local files for every known source under `home`. Sources with
/// no files yield an empty list (and are skipped by the caller).
pub fn discover(home: &Path) -> Vec<DiscoveredSource> {
    vec![
        DiscoveredSource {
            source: "claude",
            files: files_for(home, "claude"),
        },
        DiscoveredSource {
            source: "codex",
            files: files_for(home, "codex"),
        },
        DiscoveredSource {
            source: "go",
            files: files_for(home, "go"),
        },
    ]
}

/// The on-disk data files for one source. A full recursive listing, so it is
/// only run for sources that should actually be collected (issue 07 detection
/// gates this); sources without a file listing yield an empty list.
pub fn files_for(home: &Path, source: &str) -> Vec<PathBuf> {
    let xdg = std::env::var("XDG_DATA_HOME").ok();
    files_for_with(home, source, xdg.as_deref())
}

/// Discovery with an explicit `XDG_DATA_HOME` override, so tests can exercise
/// non-default installs without mutating process-global environment state.
fn files_for_with(home: &Path, source: &str, xdg_data_home: Option<&str>) -> Vec<PathBuf> {
    match source {
        "claude" => collect_files(&home.join(".claude/projects"), None, ".jsonl"),
        "codex" => collect_files(&home.join(".codex/sessions"), Some("rollout-"), ".jsonl"),
        "go" => collect_files(
            &go_data_dir(home, xdg_data_home).join("storage/message"),
            None,
            ".json",
        ),
        _ => Vec::new(),
    }
}

/// OpenCode's data directory, honoring `XDG_DATA_HOME` the same way source
/// detection does so a source is never detected without being discoverable.
fn go_data_dir(home: &Path, xdg_data_home: Option<&str>) -> PathBuf {
    match xdg_data_home {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("opencode"),
        _ => home.join(".local/share/opencode"),
    }
}

/// Recursively collects files under `dir` with the given extension,
/// optionally restricted to a filename prefix. Missing directories yield
/// nothing; results are sorted so collection order is deterministic.
fn collect_files(dir: &Path, prefix: Option<&str>, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, prefix, ext, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, prefix: Option<&str>, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, prefix, ext, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let matches_prefix = prefix.is_none_or(|p| name.starts_with(p));
            if name.ends_with(ext) && matches_prefix {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_claude_transcripts_and_codex_rollouts() {
        let home = std::env::temp_dir().join(format!("aiu-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);

        let claude_session = home.join(".claude/projects/secret-project/sess-1.jsonl");
        write(&claude_session, "{\"type\":\"assistant\"}\n");
        let codex_rollout = home.join(".codex/sessions/2026/8/26/rollout-1.jsonl");
        write(&codex_rollout, "{\"type\":\"session_meta\"}\n");
        // Not a rollout: ignored by the codex prefix filter.
        let codex_other = home.join(".codex/sessions/2026/8/26/other.jsonl");
        write(&codex_other, "{}");
        // Not under a watched directory: ignored entirely.
        let stray = home.join("unrelated/notes.jsonl");
        write(&stray, "{}");

        let found = discover(&home);

        let claude = found.iter().find(|s| s.source == "claude").unwrap();
        assert_eq!(claude.files, vec![claude_session]);
        let codex = found.iter().find(|s| s.source == "codex").unwrap();
        assert_eq!(codex.files, vec![codex_rollout]);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_directories_yield_nothing() {
        let home = std::env::temp_dir().join(format!("aiu-discover-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let found = discover(&home);
        assert!(found.iter().all(|s| s.files.is_empty()));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn discovers_opencode_go_message_files() {
        let home = std::env::temp_dir().join(format!("aiu-discover-go-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);

        let message = home.join(".local/share/opencode/storage/message/ses-1/msg-1.json");
        write(&message, "{\"role\":\"assistant\"}\n");
        // Session metadata lives in a sibling directory and is not collected.
        let session = home.join(".local/share/opencode/storage/session/global/ses-1.json");
        write(&session, "{}");
        // Parts are a sibling directory too.
        let part = home.join(".local/share/opencode/storage/part/msg-1/pt-1.json");
        write(&part, "{}");

        let found = discover(&home);
        let go = found.iter().find(|s| s.source == "go").unwrap();
        assert_eq!(go.files, vec![message]);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn go_discovery_honors_xdg_data_home() {
        let xdg = std::env::temp_dir().join(format!("aiu-discover-xdg-{}", std::process::id()));
        let home =
            std::env::temp_dir().join(format!("aiu-discover-xdg-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&xdg);
        let _ = fs::remove_dir_all(&home);

        let message = xdg.join("opencode/storage/message/ses-1/msg-1.json");
        write(&message, "{\"role\":\"assistant\"}\n");

        let xdg_str = xdg.to_str().unwrap();
        let files = files_for_with(&home, "go", Some(xdg_str));
        assert_eq!(files, vec![message]);

        // Without XDG_DATA_HOME, go falls back to `~/.local/share/opencode`.
        let fallback = files_for_with(&home, "go", None);
        assert!(fallback.is_empty(), "no fallback files present");

        let _ = fs::remove_dir_all(&xdg);
        let _ = fs::remove_dir_all(&home);
    }
}
