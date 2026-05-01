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
pub const CURRENT_VERSION: u32 = 3;

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

/// Serialize `project` to pretty JSON and write it to `path`.
pub fn save_to_path(project: &Project, path: &Path) -> Result<()> {
    let file = ProjectFile {
        version: CURRENT_VERSION,
        project: project.clone(),
    };
    let json = serde_json::to_string_pretty(&file)
        .with_context(|| format!("serializing project for {}", path.display()))?;
    std::fs::write(path, json)
        .with_context(|| format!("writing {}", path.display()))?;
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
                },
                LayerSpec {
                    index: 1,
                    blend: BlendMode::Add,
                    opacity: 0.6,
                    mute: false,
                    audio_gain: 0.3,
                    master: 0.75,
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
    fn loader_round_trips_through_disk() {
        let project = sample_project();
        let dir = tempdir_path();
        let path = dir.join("ok.sublyve.json");
        save_to_path(&project, &path).expect("save");
        let loaded = load_from_path(&path).expect("load");
        assert_eq!(loaded, project);
        let _ = std::fs::remove_file(&path);
    }

    fn tempdir_path() -> PathBuf {
        std::env::temp_dir()
    }
}
