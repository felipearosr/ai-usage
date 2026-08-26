//! Source detection and overrides (issue 07).
//!
//! Detection is a cheap sentinel check — does the source's on-disk
//! persistence directory exist? — never a full history scan. Overrides
//! (`auto`/`enabled`/`disabled`) live in `source_config` and combine with
//! detection to decide which sources collection actually drives:
//!
//! * `auto`    — follow detection (the default).
//! * `enabled` — force collection even when detection misses it.
//! * `disabled` — deliberately ignore: excluded from collection and reports.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::store::{SourceMode, Store};

/// Every accounting domain aiu knows about, in canonical listing order.
pub const ALL_SOURCES: [&str; 3] = ["claude", "codex", "go"];

/// One source's cheap detection result: present or absent on this machine.
#[derive(Debug, PartialEq, Eq)]
pub struct Detection {
    pub source: &'static str,
    pub detected: bool,
}

/// A source's override plus whether it is currently detected. This is what
/// `aiu sources` lists.
#[derive(Debug, PartialEq, Eq)]
pub struct SourceStatus {
    pub source: &'static str,
    pub mode: SourceMode,
    pub detected: bool,
}

impl SourceStatus {
    /// Whether collection will actually drive this source given its override
    /// and detection state. `enabled` wins over detection, `disabled` loses,
    /// `auto` follows the filesystem.
    pub fn tracked(&self) -> bool {
        should_collect(self.mode, self.detected)
    }
}

/// Runs detection for every known source. Cheap: a single `is_dir` stat per
/// source, no recursion, no file listing.
pub fn detect(home: &Path) -> Vec<Detection> {
    ALL_SOURCES
        .iter()
        .map(|&source| Detection {
            source,
            detected: sentinel_dir(home, source).is_dir(),
        })
        .collect()
}

/// The directory whose existence marks a source as installed on this machine.
fn sentinel_dir(home: &Path, source: &str) -> PathBuf {
    match source {
        "claude" => home.join(".claude/projects"),
        "codex" => home.join(".codex/sessions"),
        // OpenCode stores its data under `~/.local/share/opencode` on both
        // macOS and Linux (it follows the XDG layout on both).
        "go" => home.join(".local/share/opencode"),
        _ => home.join(".never-present"),
    }
}

/// Resolves an override plus a detection result into a collect/don't-collect
/// decision. `enabled` attempts collection regardless of detection; `disabled`
/// never collects; `auto` collects only when detected.
pub fn should_collect(mode: SourceMode, detected: bool) -> bool {
    match mode {
        SourceMode::Enabled => true,
        SourceMode::Disabled => false,
        SourceMode::Auto => detected,
    }
}

/// The current mode + detection status for every source, for `aiu sources`.
pub fn statuses(store: &Store, home: &Path) -> Result<Vec<SourceStatus>> {
    detect(home)
        .into_iter()
        .map(|d| {
            let mode = store.source_mode(d.source)?;
            Ok(SourceStatus {
                source: d.source,
                mode,
                detected: d.detected,
            })
        })
        .collect()
}

/// Renders `aiu sources` as text: one line per source with its mode and
/// detected status, and a marker for sources currently tracked.
pub fn render_statuses(statuses: &[SourceStatus]) -> String {
    let mut out = String::from("SOURCES\n");
    for s in statuses {
        out.push_str(&format!(
            "  {:<8} {:<8} {} ({})\n",
            s.source,
            s.mode.as_str(),
            if s.detected {
                "detected"
            } else {
                "not detected"
            },
            if s.tracked() {
                "tracked"
            } else {
                "not tracked"
            },
        ));
    }
    out
}

/// Renders `aiu sources --json`.
pub fn render_statuses_json(statuses: &[SourceStatus]) -> String {
    let sources = statuses
        .iter()
        .map(|s| {
            serde_json::json!({
                "source": s.source,
                "mode": s.mode.as_str(),
                "detected": s.detected,
                "tracked": s.tracked(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({ "sources": sources }))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Renders `aiu sources detect` as text: detected status only.
pub fn render_detections(detections: &[Detection]) -> String {
    let mut out = String::from("DETECTED SOURCES\n");
    for d in detections {
        out.push_str(&format!(
            "  {:<8} {}\n",
            d.source,
            if d.detected {
                "detected"
            } else {
                "not detected"
            },
        ));
    }
    out
}

/// Renders `aiu sources detect --json`.
pub fn render_detections_json(detections: &[Detection]) -> String {
    let sources = detections
        .iter()
        .map(|d| {
            serde_json::json!({
                "source": d.source,
                "detected": d.detected,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::json!({ "sources": sources }))
        .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aiu-sources-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn detect_identifies_present_and_absent_sources_for_all_three() {
        let home = tmp("detect");
        // Only claude's sentinel exists.
        fs::create_dir_all(home.join(".claude/projects")).unwrap();

        let detections = detect(&home);
        let by_name = |name: &str| detections.iter().find(|d| d.source == name).unwrap();

        assert!(by_name("claude").detected, "claude dir present");
        assert!(!by_name("codex").detected, "codex dir absent");
        assert!(!by_name("go").detected, "go dir absent");

        // Installing go later flips it to present without any other change.
        fs::create_dir_all(home.join(".local/share/opencode")).unwrap();
        let re = detect(&home);
        assert!(
            re.iter().any(|d| d.source == "go" && d.detected),
            "go detected after install"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn detection_is_presence_not_a_history_scan() {
        // An empty sentinel directory counts as present: detection never walks
        // or lists files, so it cannot know (or care) how much history exists.
        let home = tmp("empty");
        fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::create_dir_all(home.join(".local/share/opencode")).unwrap();

        let detections = detect(&home);
        assert_eq!(detections.len(), 3);
        assert!(detections.iter().all(|d| d.detected));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_collect_resolves_each_mode() {
        assert!(should_collect(SourceMode::Auto, true));
        assert!(!should_collect(SourceMode::Auto, false));
        assert!(should_collect(SourceMode::Enabled, false));
        assert!(should_collect(SourceMode::Enabled, true));
        assert!(!should_collect(SourceMode::Disabled, true));
        assert!(!should_collect(SourceMode::Disabled, false));
    }

    #[test]
    fn statuses_reflect_persisted_overrides_and_detection() {
        let home = tmp("statuses");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let store = Store::open_in_memory().unwrap();
        store.set_source_mode("go", SourceMode::Enabled).unwrap();
        store
            .set_source_mode("codex", SourceMode::Disabled)
            .unwrap();

        let statuses = statuses(&store, &home).unwrap();
        let by_name = |name: &str| statuses.iter().find(|s| s.source == name).unwrap();

        assert_eq!(by_name("claude").mode, SourceMode::Auto);
        assert!(by_name("claude").tracked(), "auto + detected");

        assert_eq!(by_name("go").mode, SourceMode::Enabled);
        assert!(by_name("go").tracked(), "enabled tracked even undetected");
        assert!(!by_name("go").detected);

        assert_eq!(by_name("codex").mode, SourceMode::Disabled);
        assert!(!by_name("codex").tracked());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn rendered_statuses_show_mode_and_detection() {
        let statuses = vec![
            SourceStatus {
                source: "claude",
                mode: SourceMode::Auto,
                detected: true,
            },
            SourceStatus {
                source: "go",
                mode: SourceMode::Enabled,
                detected: false,
            },
        ];
        let text = render_statuses(&statuses);
        assert!(text.contains("claude") && text.contains("auto"));
        assert!(text.contains("detected"));
        assert!(text.contains("not detected"), "{text}");
        assert!(text.contains("tracked"), "{text}");
    }
}
