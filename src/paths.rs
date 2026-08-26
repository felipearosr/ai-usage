//! Platform-specific data directory resolution.
//!
//! Linux: `$XDG_DATA_HOME/aiu` or `~/.local/share/aiu`
//! macOS: `~/Library/Application Support/aiu`
//!
//! The database lives at `<data dir>/usage.db`. `AIU_DATA_DIR` overrides both,
//! which also gives tests and sandboxed runs an explicit seam.

use std::path::{Path, PathBuf};

pub fn db_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("usage.db"))
}

pub fn data_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("AIU_DATA_DIR") {
        if !override_dir.is_empty() {
            return Some(PathBuf::from(override_dir));
        }
    }
    home_dir().map(|home| default_data_dir_for(&home))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory, used by local source-file discovery.
pub fn home() -> Option<PathBuf> {
    home_dir()
}

fn default_data_dir_for(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/aiu")
    } else {
        match std::env::var("XDG_DATA_HOME") {
            Ok(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("aiu"),
            _ => home.join(".local/share/aiu"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_default_is_xdg_data_home_under_home() {
        // Exercise the pure branch directly so both platforms are covered
        // regardless of the OS running the tests.
        let home = Path::new("/home/tester");
        let dir = default_data_dir_for(home);
        let expected = if cfg!(target_os = "macos") {
            PathBuf::from("/home/tester/Library/Application Support/aiu")
        } else {
            PathBuf::from("/home/tester/.local/share/aiu")
        };
        assert_eq!(dir, expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_matches_issue_location() {
        let dir = default_data_dir_for(Path::new("/Users/tester"));
        assert_eq!(
            dir,
            PathBuf::from("/Users/tester/Library/Application Support/aiu")
        );
    }
}
