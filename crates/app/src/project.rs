use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use avengine_core::BlendMode;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::library::{CellSource, ClipDefaults};

/// Schema version of the project JSON we emit. Bumped on any
/// breaking change to the on-disk shape; the loader rejects newer
/// versions with a clear error rather than silently misinterpreting
/// fields.
///
/// History:
/// - `1`: initial schema (multi-layer grid + audio + output settings).
/// - `2`: adds `master` to `LayerSpec` (defaults to 1.0 for v1 files).
/// - `3`: cells become source-discriminated (`File { path }` vs
///   `Camera { format_name, device, display_name }`). v2 cells with a
///   bare `path` field are migrated into `File` on load.
/// - `4`: adds `solo` to `LayerSpec` (defaults to false for older files).
pub const CURRENT_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub composition: CompositionSpec,
    pub library: LibrarySpec,
    pub layers: Vec<LayerSpec>,
    pub output: OutputSpec,
    pub audio: AudioSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionSpec {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibrarySpec {
    pub layers: usize,
    pub columns: usize,
    pub cells: Vec<CellSpec>,
}

/// What a saved cell points at. Tagged with `"type"` so the JSON is
/// self-describing and v3+ can grow new variants (NDI, Syphon, …)
/// without further breaking changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CellSpecSource {
    File { path: PathBuf },
    Camera {
        format_name: String,
        device: String,
        display_name: String,
        /// Added late in v3. `#[serde(default)]` so any v3 file written
        /// before this field existed loads with `has_audio = false`,
        /// matching the conservative-no-mic interpretation.
        #[serde(default)]
        has_audio: bool,
    },
}

impl From<&CellSource> for CellSpecSource {
    fn from(source: &CellSource) -> Self {
        match source {
            CellSource::File { path } => Self::File { path: path.clone() },
            CellSource::Camera { format_name, device, display_name, has_audio } => Self::Camera {
                format_name: format_name.clone(),
                device: device.clone(),
                display_name: display_name.clone(),
                has_audio: *has_audio,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellSpec {
    pub row: usize,
    pub col: usize,
    pub source: CellSpecSource,
    pub defaults: ClipDefaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LayerSpec {
    pub index: usize,
    pub blend: BlendMode,
    pub opacity: f32,
    pub mute: bool,
    pub audio_gain: f32,
    /// Layer master fade (added in schema v2). Defaults to 1.0 when
    /// the JSON omits it — keeps v1 files loading cleanly.
    #[serde(default = "default_master")]
    pub master: f32,
    /// Solo flag (added in schema v4). Defaults to false when the JSON
    /// omits it — keeps v1/v2/v3 files loading cleanly.
    #[serde(default)]
    pub solo: bool,
}

fn default_master() -> f32 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    pub monitor_index: usize,
    pub fullscreen: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSpec {
    pub device_name: Option<String>,
    pub master_volume: f32,
}

// ---- v2 legacy shapes for migration ----

/// Pre-v3 cell shape: a bare `path`. Migrated into `CellSpec::source =
/// File { path }` on load.
#[derive(Debug, Clone, Deserialize)]
struct CellSpecV2 {
    pub row: usize,
    pub col: usize,
    pub path: PathBuf,
    pub defaults: ClipDefaults,
}

#[derive(Debug, Clone, Deserialize)]
struct LibrarySpecV2 {
    pub layers: usize,
    pub columns: usize,
    pub cells: Vec<CellSpecV2>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProjectV2 {
    pub composition: CompositionSpec,
    pub library: LibrarySpecV2,
    pub layers: Vec<LayerSpec>,
    pub output: OutputSpec,
    pub audio: AudioSpec,
}

#[derive(Debug, Clone, Deserialize)]
struct ProjectFileV2 {
    // `version` is read off via the `VersionPeek` step in
    // `load_from_path`; we don't keep it on the migrated struct.
    pub project: ProjectV2,
}

impl From<ProjectV2> for Project {
    fn from(p: ProjectV2) -> Self {
        Self {
            composition: p.composition,
            library: LibrarySpec {
                layers: p.library.layers,
                columns: p.library.columns,
                cells: p
                    .library
                    .cells
                    .into_iter()
                    .map(|c| CellSpec {
                        row: c.row,
                        col: c.col,
                        source: CellSpecSource::File { path: c.path },
                        defaults: c.defaults,
                    })
                    .collect(),
            },
            layers: p.layers,
            output: p.output,
            audio: p.audio,
        }
    }
}

/// Read a project file from disk. Returns the inner `Project` after
/// validating the version field.
pub fn load_from_path(path: &Path) -> Result<Project> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    // Peek the version first so we know which shape to deserialise as.
    #[derive(Deserialize)]
    struct VersionPeek { version: u32 }
    let peek: VersionPeek = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as a Sublyve project", path.display()))?;

    if peek.version > CURRENT_VERSION {
        return Err(anyhow!(
            "project file version {} is newer than supported ({CURRENT_VERSION}); upgrade the app",
            peek.version
        ));
    }

    if peek.version <= 2 {
        info!(
            "loading project at version {} (current is {CURRENT_VERSION}); migrating",
            peek.version
        );
        let v2: ProjectFileV2 = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {} as a v2 project", path.display()))?;
        return Ok(v2.project.into());
    }

    // v3+ — current shape.
    let file: ProjectFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as a v3 project", path.display()))?;
    Ok(file.project)
}

/// Serialize `project` to pretty JSON wrapped in the versioned envelope.
/// Used both for plain `.sublyve.json` writes (via [`save_atomic`]) and
/// for the project entry inside a `.sublyve` bundle (see
/// `bundle::save_to_path`).
pub fn to_versioned_json(project: &Project) -> Result<String> {
    let file = ProjectFile {
        version: CURRENT_VERSION,
        project: project.clone(),
    };
    serde_json::to_string_pretty(&file).context("serializing project")
}

/// Save `project` to `path` durably:
///
/// 1. Serialize and write to a sibling temp file `<name>.tmp.<pid>`.
/// 2. `fsync` the temp file so its bytes hit disk.
/// 3. `rename` the temp file over `path` (atomic w.r.t. concurrent
///    readers within one filesystem).
/// 4. Best-effort `fsync` the parent directory on Unix so the rename
///    entry itself survives a power loss.
///
/// The temp file's filename carries the process id (matching the
/// convention used in `thumb_cache::write_cached` and `bundle.rs`) so
/// two sublyve processes saving the same project — e.g. a CLI launch
/// running next to a Finder double-click, or a dev-time second
/// instance — don't truncate each other's temp file via `O_TRUNC`. On
/// rename failure the temp file is cleaned up rather than leaking
/// next to the project.
///
/// Atomicity caveats:
/// - `rename(2)` is only atomic when source and target are on the
///   same filesystem. We always pick a sibling path, so that holds.
/// - The directory `fsync` is wrapped in `#[cfg(unix)]` because
///   Windows doesn't expose a way to open a directory as a `File`,
///   and a handful of exotic filesystems don't implement it either.
///   Skipping it weakens durability against power loss but not
///   against process crashes.
pub fn save_atomic(project: &Project, path: &Path) -> Result<()> {
    use std::fs::{File, OpenOptions};
    use std::io::Write;

    let json = to_versioned_json(project)
        .with_context(|| format!("serializing project for {}", path.display()))?;

    let Some(name) = path.file_name() else {
        // Degenerate path with no file name (e.g. `/`). Fall back to a
        // direct write — losing atomicity here is preferable to
        // panicking on user input.
        return std::fs::write(path, json)
            .with_context(|| format!("writing {}", path.display()));
    };

    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        name.to_string_lossy(),
        std::process::id(),
    ));

    // Write + fsync the temp file inside a scope so the handle drops
    // (and flushes) before we rename.
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsyncing {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e).context(format!(
            "renaming {} → {}",
            tmp.display(),
            path.display()
        )));
    }

    // Best-effort directory fsync so the rename entry itself is
    // durable across power loss. Windows can't `open` a directory as
    // a regular file; some niche filesystems (FAT, exotic FUSE
    // mounts) return errors here too — we swallow them because the
    // file data is already on disk and that's the load-bearing part.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let _ = File::open(parent).and_then(|d| d.sync_all());
    }

    Ok(())
}

/// Walk a `Library` and emit a `CellSpec` for every occupied cell.
/// Used by `AppState::capture_project` (in `main.rs`) which has the
/// `&Library` reference.
pub fn collect_cells(library: &crate::library::Library) -> Vec<CellSpec> {
    let mut cells = Vec::new();
    for row in 0..library.layers() {
        for col in 0..library.columns() {
            if let Some(slot) = library.cell(row, col) {
                cells.push(CellSpec {
                    row,
                    col,
                    source: (&slot.source).into(),
                    defaults: slot.defaults,
                });
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_project() -> Project {
        Project {
            composition: CompositionSpec { width: 1920, height: 1080 },
            library: LibrarySpec {
                layers: 4,
                columns: 8,
                cells: vec![
                    CellSpec {
                        row: 0,
                        col: 0,
                        source: CellSpecSource::File {
                            path: PathBuf::from("/videos/alpha.mp4"),
                        },
                        defaults: ClipDefaults {
                            looping: true,
                            speed: 1.0,
                            blend: BlendMode::Normal,
                        },
                    },
                    CellSpec {
                        row: 1,
                        col: 2,
                        source: CellSpecSource::Camera {
                            format_name: "avfoundation".to_string(),
                            device: "0:0".to_string(),
                            display_name: "FaceTime HD Camera".to_string(),
                            has_audio: true,
                        },
                        defaults: ClipDefaults {
                            looping: false,
                            speed: 1.0,
                            blend: BlendMode::Add,
                        },
                    },
                ],
            },
            layers: vec![
                LayerSpec {
                    index: 0,
                    blend: BlendMode::Normal,
                    opacity: 1.0,
                    mute: false,
                    audio_gain: 1.0,
                    master: 1.0,
                    solo: true,
                },
                LayerSpec {
                    index: 1,
                    blend: BlendMode::Add,
                    opacity: 0.6,
                    mute: false,
                    audio_gain: 0.3,
                    master: 0.75,
                    solo: false,
                },
            ],
            output: OutputSpec {
                monitor_index: 1,
                fullscreen: true,
            },
            audio: AudioSpec {
                device_name: Some("MacBook Pro Speakers".to_string()),
                master_volume: 0.8,
            },
        }
    }

    #[test]
    fn round_trip_serializes_and_deserializes_equal() {
        let project = sample_project();
        let file = ProjectFile { version: CURRENT_VERSION, project: project.clone() };
        let json = serde_json::to_string_pretty(&file).expect("serialize");
        let back: ProjectFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.version, CURRENT_VERSION);
        assert_eq!(back.project, project);
    }

    #[test]
    fn loader_rejects_newer_version() {
        let json = r#"{
            "version": 999,
            "project": {
                "composition": {"width": 1920, "height": 1080},
                "library": {"layers": 1, "columns": 1, "cells": []},
                "layers": [],
                "output": {"monitor_index": 0, "fullscreen": false},
                "audio": {"device_name": null, "master_volume": 1.0}
            }
        }"#;
        let dir = tempdir_path();
        let path = dir.join("future.sublyve.json");
        std::fs::write(&path, json).expect("write tmp");
        let err = load_from_path(&path).expect_err("should reject");
        let msg = format!("{err}");
        assert!(msg.contains("999"), "error mentions the unsupported version: {msg}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v1_files_load_with_master_defaulting_to_1() {
        // A `version: 1` JSON has no `master` field on each layer.
        // The `serde(default = …)` attribute fills it in as 1.0.
        let json = r#"{
            "version": 1,
            "project": {
                "composition": {"width": 1280, "height": 720},
                "library": {"layers": 2, "columns": 4, "cells": []},
                "layers": [
                    {"index": 0, "blend": "Normal", "opacity": 0.8, "mute": false, "audio_gain": 1.5},
                    {"index": 1, "blend": "Add", "opacity": 0.5, "mute": true, "audio_gain": 0.0}
                ],
                "output": {"monitor_index": 0, "fullscreen": false},
                "audio": {"device_name": null, "master_volume": 1.0}
            }
        }"#;
        let dir = tempdir_path();
        let path = dir.join("v1.sublyve.json");
        std::fs::write(&path, json).expect("write tmp");
        let loaded = load_from_path(&path).expect("load v1");
        assert_eq!(loaded.layers.len(), 2);
        for spec in &loaded.layers {
            assert_eq!(spec.master, 1.0, "v1 layer must default master to 1.0");
        }
        // Other fields still come through correctly.
        assert_eq!(loaded.layers[0].opacity, 0.8);
        assert_eq!(loaded.layers[1].audio_gain, 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v2_cell_path_migrates_to_file_source() {
        // v2 cells stored a bare `path`; v3 wraps it in
        // `source: { type: "File", path: ... }`.
        let json = r#"{
            "version": 2,
            "project": {
                "composition": {"width": 1920, "height": 1080},
                "library": {
                    "layers": 1, "columns": 1,
                    "cells": [
                        {"row": 0, "col": 0, "path": "/old/clip.mp4",
                         "defaults": {"looping": true, "speed": 1.0, "blend": "Normal"}}
                    ]
                },
                "layers": [{"index": 0, "blend": "Normal", "opacity": 1.0,
                            "mute": false, "audio_gain": 1.0, "master": 1.0}],
                "output": {"monitor_index": 0, "fullscreen": false},
                "audio": {"device_name": null, "master_volume": 1.0}
            }
        }"#;
        let dir = tempdir_path();
        let path = dir.join("v2.sublyve.json");
        std::fs::write(&path, json).expect("write tmp");
        let loaded = load_from_path(&path).expect("load v2");
        assert_eq!(loaded.library.cells.len(), 1);
        match &loaded.library.cells[0].source {
            CellSpecSource::File { path } => {
                assert_eq!(path.to_str(), Some("/old/clip.mp4"));
            }
            other => panic!("expected File source, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v3_files_load_with_solo_defaulting_to_false() {
        // A `version: 3` JSON has no `solo` field on each layer.
        // `#[serde(default)]` fills it in as `false`.
        let json = r#"{
            "version": 3,
            "project": {
                "composition": {"width": 1920, "height": 1080},
                "library": {"layers": 2, "columns": 4, "cells": []},
                "layers": [
                    {"index": 0, "blend": "Normal", "opacity": 1.0,
                     "mute": false, "audio_gain": 1.0, "master": 1.0},
                    {"index": 1, "blend": "Add", "opacity": 0.5,
                     "mute": true, "audio_gain": 0.0, "master": 0.5}
                ],
                "output": {"monitor_index": 0, "fullscreen": false},
                "audio": {"device_name": null, "master_volume": 1.0}
            }
        }"#;
        let dir = tempdir_path();
        let path = dir.join("v3.sublyve.json");
        std::fs::write(&path, json).expect("write tmp");
        let loaded = load_from_path(&path).expect("load v3");
        assert_eq!(loaded.layers.len(), 2);
        for spec in &loaded.layers {
            assert!(!spec.solo, "v3 layer must default solo to false");
        }
        // Pre-existing fields still parse.
        assert_eq!(loaded.layers[1].master, 0.5);
        assert!(loaded.layers[1].mute);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_atomic_round_trips_and_cleans_up_tmp() {
        let project = sample_project();
        let dir = tempdir_path();
        // Unique filename so parallel test runs don't collide.
        let path = dir.join(format!("atomic-{}.sublyve.json", std::process::id()));
        save_atomic(&project, &path).expect("save_atomic");
        let loaded = load_from_path(&path).expect("load");
        assert_eq!(loaded, project);

        // No `<name>.tmp.<pid>` should remain after a successful save.
        let tmp = path.with_file_name(format!(
            "{}.tmp.{}",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id(),
        ));
        assert!(!tmp.exists(), "tmp file should be renamed away, not left behind");

        let _ = std::fs::remove_file(&path);
    }

    fn tempdir_path() -> PathBuf {
        std::env::temp_dir()
    }
}
