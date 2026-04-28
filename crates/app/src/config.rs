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
}

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

    /// Update `last_project` and write the config — best-effort. We log
    /// errors at warn level rather than propagating, since a failure
    /// here shouldn't cancel the user's save / open action.
    pub fn remember_project(&mut self, path: &Path) {
        self.last_project = Some(path.to_path_buf());
        if let Err(e) = self.save() {
            warn!("could not persist last_project: {e:#}");
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;
    Some(dir.join("sublyve").join("config.json"))
}
