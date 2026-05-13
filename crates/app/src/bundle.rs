//! `.sublyve` project bundles: a zip of the project JSON together with
//! every referenced clip file, so a project can be moved between machines
//! without re-importing.
//!
//! ## Layout inside the archive
//!
//! ```text
//! project.sublyve.json   ← the project JSON (same schema as a loose file)
//! clips/<basename>       ← one entry per `CellSpecSource::File` cell
//! ```
//!
//! Camera cells are *not* bundled (no file to ship); they round-trip their
//! device metadata unchanged. On a different machine the camera may simply
//! be unavailable, which falls through to the existing
//! "skipping unavailable camera" branch in `apply_project`.
//!
//! ## Paths
//!
//! Inside a bundle the JSON refers to clips by a relative path
//! (e.g. `clips/foo.mp4`). [`load_from_path`] extracts the bundle into a
//! per-bundle directory under `dirs::cache_dir()/sublyve/bundles/`, then
//! rewrites every `File { path }` so the in-memory `Project` carries
//! absolute paths again — keeping the rest of the app oblivious to
//! whether the project came from a bundle or a loose `.sublyve.json`.
//!
//! ## Save atomicity
//!
//! [`save_to_path`] writes the zip to a sibling tempfile (`*.tmp-<pid>-<nanos>`
//! in the destination directory) and only renames into place once the zip
//! has been finished and synced — so a crash mid-save can't truncate an
//! existing bundle.
//!
//! ## Extraction cache
//!
//! Extraction is keyed by `(bundle file mtime, bundle file size)`,
//! mirroring `thumb_cache`'s philosophy: if any of those change, the key
//! changes and we re-extract. v1 has no eviction; orphaned bundle dirs
//! grow until the user wipes their cache directory.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use tracing::{info, warn};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::project::{self, CellSpecSource, Project};

/// Name of the project JSON inside the archive.
const PROJECT_ENTRY: &str = "project.sublyve.json";
/// Directory inside the archive that holds all bundled clip files.
const CLIPS_DIR: &str = "clips";

/// True when `path` should be treated as a `.sublyve` bundle (zip) rather
/// than a loose `.sublyve.json`. Detection is by extension only — the
/// loader does *not* sniff the file contents, because both formats are
/// legitimate inputs and the user picks via the save dialog's filter.
pub fn is_bundle_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sublyve"))
}

/// Write `project` to a `.sublyve` bundle at `path`. Every
/// `CellSpecSource::File` cell has its source copied into the archive
/// under `clips/<basename>`, with the JSON rewritten to reference the
/// relative path so the bundle is self-contained.
///
/// The archive is built entirely in-memory (a `Vec<u8>` cursor) and
/// written via a tempfile + `rename` so a crash mid-write can't corrupt
/// an existing bundle.
pub fn save_to_path(project: &Project, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("bundle path has no parent directory: {}", path.display()))?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }

    // Resolve every File cell → (absolute source path, relative archive
    // path). The archive path uses the basename, with a `-N` suffix if
    // two different sources share the same basename (e.g. two `intro.mp4`
    // dragged in from different folders).
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for cell in &project.library.cells {
        if let CellSpecSource::File { path: src } = &cell.source {
            let rel = unique_clip_entry(src, &mut used);
            plan.push((src.clone(), rel));
        }
    }

    // Rewrite the project so every File path becomes the relative
    // archive path. We clone rather than mutate the caller's Project.
    let mut rewritten = project.clone();
    {
        let mut plan_iter = plan.iter();
        for cell in &mut rewritten.library.cells {
            if let CellSpecSource::File { path: cell_path } = &mut cell.source {
                let (_, rel) = plan_iter
                    .next()
                    .expect("plan was built from the same File cells in the same order");
                *cell_path = PathBuf::from(rel);
            }
        }
        debug_assert!(plan_iter.next().is_none());
    }

    // Build the zip in memory. Deflate gives a useful win for the JSON
    // but the clip files themselves are already compressed video — we
    // stamp those as `Stored` to skip pointless CPU.
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let json_opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file(PROJECT_ENTRY, json_opts)
            .with_context(|| format!("starting {PROJECT_ENTRY} entry"))?;
        let json = project::to_versioned_json(&rewritten)
            .context("serializing bundle project json")?;
        zip.write_all(json.as_bytes())
            .context("writing project json into bundle")?;

        let clip_opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (src, rel) in &plan {
            zip.start_file(rel, clip_opts)
                .with_context(|| format!("starting {rel} entry"))?;
            let mut file = fs::File::open(src)
                .with_context(|| format!("opening clip {} for bundling", src.display()))?;
            std::io::copy(&mut file, &mut zip)
                .with_context(|| format!("copying clip {} into bundle", src.display()))?;
        }

        zip.finish().context("finalising zip archive")?;
    }

    let bytes = cursor.into_inner();

    // Atomic publish: write next to the target, fsync, rename.
    let tmp = sibling_tempfile(path);
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating tempfile {}", tmp.display()))?;
        f.write_all(&bytes)
            .with_context(|| format!("writing tempfile {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing tempfile {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;

    info!(
        "bundle saved → {} ({} clip(s), {} bytes)",
        path.display(),
        plan.len(),
        bytes.len()
    );
    Ok(())
}

/// Read a `.sublyve` bundle from `path`. The zip is extracted to a
/// per-bundle directory under the user's cache dir (re-using a previous
/// extraction if `(mtime, size)` matches), and the returned `Project`
/// has every relative `File` path rewritten to an absolute path inside
/// that directory.
pub fn load_from_path(path: &Path) -> Result<Project> {
    let extracted_root = ensure_extracted(path)?;

    // Read project JSON from the extracted tree. We use the same
    // project loader as for loose files so version-migration paths
    // (v1→…→current) stay in one place.
    let json_path = extracted_root.join(PROJECT_ENTRY);
    let mut project = project::load_from_path(&json_path)
        .with_context(|| format!("loading project JSON from {}", json_path.display()))?;

    // Resolve every relative File path against the extraction root.
    // Absolute paths are passed through untouched — a hand-edited bundle
    // could conceivably reference an external clip; we don't break that.
    for cell in &mut project.library.cells {
        if let CellSpecSource::File { path: rel } = &mut cell.source
            && rel.is_relative()
        {
            *rel = extracted_root.join(&*rel);
        }
    }

    info!(
        "bundle loaded ← {} (extracted to {})",
        path.display(),
        extracted_root.display()
    );
    Ok(project)
}

/// Extract the bundle at `path` into the cache, or return the path of an
/// existing extraction whose key matches.
fn ensure_extracted(path: &Path) -> Result<PathBuf> {
    let meta = fs::metadata(path)
        .with_context(|| format!("stat bundle {}", path.display()))?;
    let key = extraction_key(path, &meta)?;
    let root = bundles_cache_dir()
        .ok_or_else(|| anyhow!("no platform cache dir available; cannot extract bundle"))?
        .join(&key);

    // Treat "directory exists and contains the project JSON" as a cache
    // hit. The key includes mtime+size, so any change to the bundle
    // produces a new directory; we never have to worry about a stale
    // extraction with the right name.
    if root.join(PROJECT_ENTRY).is_file() {
        return Ok(root);
    }

    // Miss: extract afresh. Wipe any partial directory left over from a
    // previous failed extraction sharing this key (extremely unlikely
    // given the mtime+size keying, but cheap to be safe).
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
    }
    fs::create_dir_all(&root)
        .with_context(|| format!("creating extraction dir {}", root.display()))?;

    let file = fs::File::open(path)
        .with_context(|| format!("opening bundle {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("opening bundle {} as zip", path.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("reading entry {i} of bundle"))?;
        // `enclosed_name` rejects absolute paths and `..` traversal,
        // protecting against a malicious bundle writing outside `root`.
        let Some(rel) = entry.enclosed_name() else {
            warn!("skipping suspicious bundle entry: {}", entry.name());
            continue;
        };
        let out_path = root.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .with_context(|| format!("creating {}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out = fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("extracting {}", out_path.display()))?;
    }

    Ok(root)
}

/// Per-bundle cache directory name: `<size>-<mtime_secs>-<mtime_nanos>`.
/// Stable on the same machine; no need for canonical-path hashing because
/// the directory is scoped per bundle (so two different bundles never
/// share a key unless they're byte-identical, which is fine).
fn extraction_key(path: &Path, meta: &fs::Metadata) -> Result<String> {
    let size = meta.len();
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime for {}", path.display()))?;
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(format!(
        "{size}-{}-{}",
        dur.as_secs(),
        dur.subsec_nanos()
    ))
}

fn bundles_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("sublyve").join("bundles"))
}

/// Pick a unique `clips/<basename>` entry name for `src`, suffixing with
/// `-1`, `-2`, … if the basename is already taken.
fn unique_clip_entry(src: &Path, used: &mut HashSet<String>) -> String {
    let raw = src
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "clip".to_owned());
    let base = sanitize_basename(&raw);
    let mut candidate = format!("{CLIPS_DIR}/{base}");
    if !used.contains(&candidate) {
        used.insert(candidate.clone());
        return candidate;
    }
    let (stem, ext) = split_stem_ext(&base);
    let mut n = 1usize;
    loop {
        candidate = if ext.is_empty() {
            format!("{CLIPS_DIR}/{stem}-{n}")
        } else {
            format!("{CLIPS_DIR}/{stem}-{n}.{ext}")
        };
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// Strip path separators and other characters that would confuse a zip
/// reader (forward/back slash, NUL). Keep everything else — modern zip
/// readers handle Unicode just fine.
fn sanitize_basename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | '\0') { '_' } else { c })
        .collect();
    if cleaned.is_empty() { "clip".to_owned() } else { cleaned }
}

fn split_stem_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        // Treat dotfiles (`.bashrc`) as all stem, no extension.
        Some(0) | None => (name, ""),
        Some(i) => (&name[..i], &name[i + 1..]),
    }
}

fn sibling_tempfile(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let filename = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle.sublyve");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    parent.join(format!(".{filename}.tmp-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ClipDefaults;
    use crate::project::{
        AudioSpec, CellSpec, CompositionSpec, LayerSpec, LibrarySpec, OutputSpec,
    };
    use avengine_core::BlendMode;

    /// Run `f` inside a fresh `target/test-tmp/<name>-<nanos>` directory
    /// and tear it down afterwards. We deliberately don't depend on
    /// `tempfile` — the existing project tests use `std::env::temp_dir`
    /// directly, so we follow that style here.
    fn with_tmp<R>(name: &str, f: impl FnOnce(&Path) -> R) -> R {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("sublyve-bundle-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("create tmp");
        let result = f(&dir);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    fn write_fake_clip(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create clip parent");
        }
        fs::write(path, contents).expect("write clip");
    }

    fn project_with_two_clips(a: &Path, b: &Path) -> Project {
        Project {
            composition: CompositionSpec { width: 1920, height: 1080 },
            library: LibrarySpec {
                layers: 1,
                columns: 2,
                cells: vec![
                    CellSpec {
                        row: 0,
                        col: 0,
                        source: CellSpecSource::File { path: a.to_path_buf() },
                        defaults: ClipDefaults {
                            looping: true,
                            speed: 1.0,
                            blend: BlendMode::Normal,
                        },
                    },
                    CellSpec {
                        row: 0,
                        col: 1,
                        source: CellSpecSource::File { path: b.to_path_buf() },
                        defaults: ClipDefaults {
                            looping: false,
                            speed: 1.5,
                            blend: BlendMode::Add,
                        },
                    },
                ],
            },
            layers: vec![LayerSpec {
                index: 0,
                blend: BlendMode::Normal,
                opacity: 1.0,
                mute: false,
                audio_gain: 1.0,
                master: 1.0,
                solo: false,
            }],
            output: OutputSpec { monitor_index: 0, fullscreen: false },
            audio: AudioSpec { device_name: None, master_volume: 1.0 },
        }
    }

    #[test]
    fn extension_detects_bundles() {
        assert!(is_bundle_path(Path::new("/foo/bar.sublyve")));
        assert!(is_bundle_path(Path::new("/foo/BAR.SUBLYVE")));
        assert!(!is_bundle_path(Path::new("/foo/bar.sublyve.json")));
        assert!(!is_bundle_path(Path::new("/foo/bar.json")));
    }

    #[test]
    fn round_trip_save_then_load_resolves_clip_paths() {
        with_tmp("roundtrip", |dir| {
            let clip_a = dir.join("sources/alpha.mp4");
            let clip_b = dir.join("sources/beta.mp4");
            write_fake_clip(&clip_a, b"ALPHA");
            write_fake_clip(&clip_b, b"BETA");

            let project = project_with_two_clips(&clip_a, &clip_b);
            let bundle = dir.join("scene.sublyve");
            save_to_path(&project, &bundle).expect("save bundle");

            let loaded = load_from_path(&bundle).expect("load bundle");
            assert_eq!(loaded.library.cells.len(), 2);
            for cell in &loaded.library.cells {
                let CellSpecSource::File { path } = &cell.source else {
                    panic!("expected File source");
                };
                assert!(path.is_absolute(), "loader should resolve to absolute: {}", path.display());
                assert!(path.exists(), "extracted clip should exist: {}", path.display());
            }
            // Non-path fields round-trip unchanged.
            assert_eq!(loaded.library.cells[0].defaults.speed, 1.0);
            assert_eq!(loaded.library.cells[1].defaults.speed, 1.5);
            assert_eq!(loaded.library.cells[1].defaults.blend, BlendMode::Add);
            assert_eq!(loaded.composition, project.composition);

            // Extracted clip bytes match originals.
            let expected: [&[u8]; 2] = [b"ALPHA", b"BETA"];
            for (cell, want) in loaded.library.cells.iter().zip(expected) {
                let CellSpecSource::File { path } = &cell.source else { unreachable!() };
                let actual = fs::read(path).expect("read extracted");
                assert_eq!(actual, want, "bundled clip content mismatches");
            }
        });
    }

    #[test]
    fn duplicate_basenames_get_unique_entries() {
        with_tmp("dupes", |dir| {
            let a = dir.join("dirA/clip.mp4");
            let b = dir.join("dirB/clip.mp4");
            write_fake_clip(&a, b"A");
            write_fake_clip(&b, b"B");

            let project = project_with_two_clips(&a, &b);
            let bundle = dir.join("dupes.sublyve");
            save_to_path(&project, &bundle).expect("save bundle");

            let loaded = load_from_path(&bundle).expect("load bundle");
            let p0 = match &loaded.library.cells[0].source {
                CellSpecSource::File { path } => path.clone(),
                _ => unreachable!(),
            };
            let p1 = match &loaded.library.cells[1].source {
                CellSpecSource::File { path } => path.clone(),
                _ => unreachable!(),
            };
            assert_ne!(p0, p1, "clashing basenames must land at distinct paths");
            assert_eq!(fs::read(&p0).unwrap(), b"A");
            assert_eq!(fs::read(&p1).unwrap(), b"B");
        });
    }
}
