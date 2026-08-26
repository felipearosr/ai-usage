//! Local source-file discovery.
//!
//! Finds the on-disk persistence each source leaves behind, so collection can
//! read new records without a resident daemon. Discovery is a cheap listing
//! (no full rescans); the source-detection/override logic that decides *which*
//! sources to track lives in issue 07 — here we just locate files for the
//! sources aiu already knows about.
//!
//! Claude Code: `~/.claude/projects/**/*.jsonl` (session transcripts).
//! Codex: `~/.codex/sessions/**/rollout-*.jsonl` (rollout files).

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
            files: claude_files(home),
        },
        DiscoveredSource {
            source: "codex",
            files: codex_files(home),
        },
    ]
}

fn claude_files(home: &Path) -> Vec<PathBuf> {
    collect_jsonl(&home.join(".claude/projects"), None)
}

fn codex_files(home: &Path) -> Vec<PathBuf> {
    collect_jsonl(&home.join(".codex/sessions"), Some("rollout-"))
}

/// Recursively collects `.jsonl` files under `dir`, optionally restricted to a
/// filename prefix. Missing directories yield nothing; results are sorted so
/// collection order is deterministic.
fn collect_jsonl(dir: &Path, prefix: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, prefix, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, prefix: Option<&str>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, prefix, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let matches_prefix = prefix.is_none_or(|p| name.starts_with(p));
            if name.ends_with(".jsonl") && matches_prefix {
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
}
