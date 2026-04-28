use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use avengine_core::BlendMode;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::library::ClipDefaults;

/// Schema version of the project JSON we emit. Bumped on any
/// breaking change to the on-disk shape; the loader rejects newer
/// versions with a clear error rather than silently misinterpreting
/// fields.
pub const CURRENT_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellSpec {
    pub row: usize,
    pub col: usize,
    pub path: PathBuf,
    pub defaults: ClipDefaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LayerSpec {
    pub index: usize,
    pub blend: BlendMode,
    pub opacity: f32,
    pub mute: bool,
    pub audio_gain: f32,
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

/// Read a project file from disk. Returns the inner `Project` after
/// validating the version field.
pub fn load_from_path(path: &Path) -> Result<Project> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let file: ProjectFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as a Sublyve project", path.display()))?;
    if file.version > CURRENT_VERSION {
        return Err(anyhow!(
            "project file version {} is newer than supported ({CURRENT_VERSION}); upgrade the app",
            file.version
        ));
    }
    if file.version < CURRENT_VERSION {
        // No prior schemas exist yet, but log so future migrations have
        // a place to hook in.
        info!(
            "loading project at version {} (current is {CURRENT_VERSION})",
            file.version
        );
    }
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
                    path: slot.path.clone(),
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
                        path: PathBuf::from("/videos/alpha.mp4"),
                        defaults: ClipDefaults {
                            looping: true,
                            speed: 1.0,
                            blend: BlendMode::Normal,
                        },
                    },
                    CellSpec {
                        row: 1,
                        col: 2,
                        path: PathBuf::from("/videos/beta.mov"),
                        defaults: ClipDefaults {
                            looping: false,
                            speed: 0.5,
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
                },
                LayerSpec {
                    index: 1,
                    blend: BlendMode::Add,
                    opacity: 0.6,
                    mute: false,
                    audio_gain: 0.3,
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
