use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Persistent app preferences stored in the OS config directory.
///
/// Path on each platform (`dirs::config_dir()` + `sublyve`):
/// - macOS:   `~/Library/Application Support/sublyve/config.json`
/// - Linux:   `~/.config/sublyve/config.json`
/// - Windows: `%APPDATA%\sublyve\config.json`
///
/// V1 only tracks the most-recently-used project so the next launch
/// can reopen it. Future preferences (theme, default composition size,
/// recent files list, …) plug into the same struct without breaking
/// the file format — `serde` ignores unknown fields on read.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Path of the last project the user saved or opened. Cleared on
    /// load if it no longer resolves on disk.
    pub last_project: Option<PathBuf>,
    /// Most-recently-used project paths, front = most recent. Capped
    /// at `MAX_RECENT_PROJECTS`. Entries that no longer resolve on
    /// disk are pruned lazily — either when the file is loaded or
    /// just before the recent-files submenu is rendered.
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
}

/// How many recent project entries we surface in the menu. Small
/// enough to fit in a glance, large enough to span a session's worth
/// of switching between projects.
pub const MAX_RECENT_PROJECTS: usize = 8;

impl AppConfig {
    /// Load the config file, returning a default if it doesn't exist
    /// or is malformed. Never errors — config corruption shouldn't
    /// prevent the app from starting.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read(&path).and_then(|bytes| {
            serde_json::from_slice::<Self>(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(mut cfg) => {
                if let Some(p) = cfg.last_project.as_ref()
                    && !p.exists()
                {
                    debug!(
                        "config's last_project no longer exists ({}); clearing",
                        p.display()
                    );
                    cfg.last_project = None;
                }
                cfg.recent_projects.retain(|p| p.exists());
                cfg.recent_projects.truncate(MAX_RECENT_PROJECTS);
                cfg
            }
            Err(e) => {
                warn!("failed to parse {}: {e}; using defaults", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialising AppConfig")?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Update `last_project` and push onto the recent-files list,
    /// then write the config — best-effort. We log errors at warn
    /// level rather than propagating, since a failure here shouldn't
    /// cancel the user's save / open action.
    pub fn remember_project(&mut self, path: &Path) {
        self.last_project = Some(path.to_path_buf());
        push_recent(&mut self.recent_projects, path);
        if let Err(e) = self.save() {
            warn!("could not persist last_project: {e:#}");
        }
    }

    /// Drop entries from `recent_projects` whose files no longer
    /// exist on disk and persist the trimmed list. Returns the number
    /// of entries removed. Called before rendering the recent-files
    /// submenu so stale entries fade away on their own.
    pub fn prune_missing_recents(&mut self) -> usize {
        let before = self.recent_projects.len();
        self.recent_projects.retain(|p| p.exists());
        let removed = before - self.recent_projects.len();
        if removed > 0
            && let Err(e) = self.save()
        {
            warn!("could not persist pruned recent_projects: {e:#}");
        }
        removed
    }

    /// Wipe the recent-projects list. Triggered from the "Clear
    /// recent files" menu entry.
    pub fn clear_recent_projects(&mut self) {
        if self.recent_projects.is_empty() {
            return;
        }
        self.recent_projects.clear();
        if let Err(e) = self.save() {
            warn!("could not persist cleared recent_projects: {e:#}");
        }
    }
}

/// Move-to-front + dedupe + cap. Pulled out as a free function so
/// `AppConfig::remember_project` stays a one-liner and the behavior
/// is straightforward to unit-test in isolation.
fn push_recent(list: &mut Vec<PathBuf>, path: &Path) {
    list.retain(|p| p != path);
    list.insert(0, path.to_path_buf());
    list.truncate(MAX_RECENT_PROJECTS);
}

fn config_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;
    Some(dir.join("sublyve").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn push_recent_moves_to_front() {
        let mut list = vec![p("/a"), p("/b"), p("/c")];
        push_recent(&mut list, &p("/c"));
        assert_eq!(list, vec![p("/c"), p("/a"), p("/b")]);
    }

    #[test]
    fn push_recent_dedupes() {
        let mut list = vec![p("/a"), p("/b")];
        push_recent(&mut list, &p("/a"));
        push_recent(&mut list, &p("/a"));
        assert_eq!(list, vec![p("/a"), p("/b")]);
    }

    #[test]
    fn push_recent_caps_to_max() {
        let mut list = Vec::new();
        for i in 0..(MAX_RECENT_PROJECTS + 5) {
            push_recent(&mut list, &p(&format!("/p{i}")));
        }
        assert_eq!(list.len(), MAX_RECENT_PROJECTS);
        assert_eq!(list[0], p(&format!("/p{}", MAX_RECENT_PROJECTS + 4)));
    }
}
