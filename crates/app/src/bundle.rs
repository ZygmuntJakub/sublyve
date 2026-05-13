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
//! Inside a bundle the JSON refers to clips by a relative path under
//! `clips/` (e.g. `clips/foo.mp4`). [`load_from_path`] extracts the bundle
//! into a per-bundle directory under `dirs::cache_dir()/sublyve/bundles/`,
//! then rewrites every `File { path }` so the in-memory `Project` carries
//! absolute paths again — keeping the rest of the app oblivious to whether
//! the project came from a bundle or a loose `.sublyve.json`.
//!
//! ## Save atomicity
//!
//! [`save_to_path`] streams the zip to a sibling tempfile
//! (`.<filename>.tmp-<pid>-<nanos>` in the destination directory), fsyncs
//! the tempfile, renames into place, and (on Unix) fsyncs the parent
//! directory so the rename itself survives power loss. A crash mid-save
//! can leave a `.tmp-…` file behind, but never a truncated bundle.
//!
//! ## Extraction cache
//!
//! Extraction is keyed by `(size, mtime_secs, mtime_nanos, crc32 of first
//! 64 KiB)`. The CRC32 prefix defends against the case where HFS+ rounds
//! mtime to whole seconds and two genuinely different bundles end up with
//! the same `(size, mtime_secs, 0)`. v1 has no eviction; orphaned bundle
//! dirs grow until the user wipes their cache directory.
//!
//! ## Portability notes
//!
//! Clip basenames are over-sanitized on every host (not just Windows) so
//! a bundle authored on Unix with characters that are legal there but
//! illegal on Windows (`<`, `>`, `:`, `"`, `|`, `?`, `*`) still extracts
//! cleanly when moved to Windows. The Windows-reserved DOS device names
//! (CON, PRN, AUX, NUL, COM1-9, LPT1-9) are *not* specifically rewritten:
//! a clip literally named `CON.mp4` would still fail on Windows. We treat
//! that as a known edge case rather than handle it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tracing::info;
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
/// The zip is streamed directly to a sibling tempfile (no in-memory
/// buffering of the whole archive) and then renamed into place, with a
/// parent-directory fsync on Unix so the rename itself survives power
/// loss.
pub fn save_to_path(project: &Project, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("bundle path has no parent directory: {}", path.display()))?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }

    // Single-pass rewrite: clone the project, walk its File cells, and
    // for each one assign a `clips/<basename>` entry path. A
    // `src → entry` map dedupes by literal source path so two cells
    // pointing at the same file produce one zip entry rather than two.
    //
    // We intentionally do not canonicalize or content-hash here —
    // literal-path dedup catches the common "drag the same file twice"
    // case without doing surprising things behind the user's back.
    let mut rewritten = project.clone();
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut src_to_entry: HashMap<PathBuf, String> = HashMap::new();
    for cell in &mut rewritten.library.cells {
        if let CellSpecSource::File { path: cell_path } = &mut cell.source {
            let rel = match src_to_entry.get(cell_path) {
                Some(existing) => existing.clone(),
                None => {
                    let new_rel = unique_clip_entry(cell_path, &mut used);
                    src_to_entry.insert(cell_path.clone(), new_rel.clone());
                    plan.push((cell_path.clone(), new_rel.clone()));
                    new_rel
                }
            };
            let rewritten_path = PathBuf::from(&rel);
            // Invariant: every rewritten File path must live under
            // `clips/`. Catches anyone wiring up `unique_clip_entry`
            // to forget the prefix, which would slip past the loader's
            // path-traversal check.
            debug_assert!(
                rewritten_path.starts_with(Path::new(CLIPS_DIR)),
                "rewritten clip path must start with {CLIPS_DIR}/: {}",
                rewritten_path.display()
            );
            *cell_path = rewritten_path;
        }
    }

    // Stream the zip straight to a sibling tempfile. We use BufWriter so
    // small writes from `ZipWriter` don't translate into one syscall
    // each; the eventual `sync_all` flushes everything before rename.
    let tmp = sibling_tempfile(path);
    {
        let file = fs::File::create(&tmp)
            .with_context(|| format!("creating tempfile {}", tmp.display()))?;
        let mut writer = BufWriter::new(file);
        {
            let mut zip = ZipWriter::new(&mut writer);

            // Deflate the JSON (good compression ratio); store the clips
            // verbatim — modern video codecs are already entropy-coded,
            // so deflating them just burns CPU.
            let json_opts =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file(PROJECT_ENTRY, json_opts)
                .with_context(|| format!("starting {PROJECT_ENTRY} entry"))?;
            let json = project::to_versioned_json(&rewritten)
                .context("serializing bundle project json")?;
            zip.write_all(json.as_bytes())
                .context("writing project json into bundle")?;

            let clip_opts =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
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
        let file = writer
            .into_inner()
            .map_err(|e| anyhow!("flushing tempfile {}: {}", tmp.display(), e))?;
        file.sync_all()
            .with_context(|| format!("syncing tempfile {}", tmp.display()))?;
    }

    // Best-effort parent fsync after rename: on Unix the rename is a
    // metadata-only operation, and without a directory fsync a power
    // loss could leave us with the *old* file (or nothing) after reboot
    // even though the rename returned success. FAT/NFS may not support
    // it; ignore the result.
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    fsync_parent_dir(path);

    info!(
        "bundle saved → {} ({} clip(s))",
        path.display(),
        plan.len(),
    );
    Ok(())
}

/// fsync the directory containing `path` so a freshly-published rename
/// survives power loss. No-ops on Windows (you can't open directories as
/// files there) and on filesystems that don't support directory fsync.
fn fsync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Ok(dir) = fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Read a `.sublyve` bundle from `path`. The zip is extracted to a
/// per-bundle directory under the user's cache dir (re-using a previous
/// extraction if the cache key matches), and the returned `Project` has
/// every relative `File` path rewritten to an absolute path inside that
/// directory.
pub fn load_from_path(path: &Path) -> Result<Project> {
    let extracted_root = ensure_extracted(path)?;

    // Read project JSON from the extracted tree. We use the same
    // project loader as for loose files so version-migration paths
    // (v1→…→current) stay in one place — and so a bundle with a newer
    // schema version reports the error through the standard route.
    let json_path = extracted_root.join(PROJECT_ENTRY);
    let mut project = project::load_from_path(&json_path)
        .with_context(|| format!("loading project JSON from {}", json_path.display()))?;

    // Resolve relative File paths that point inside the bundle against
    // the extraction root. Anything else (absolute paths, paths outside
    // `clips/`) is left untouched: a hand-edited bundle may reference
    // an external clip, and we deliberately don't break that.
    //
    // Note: `Path::is_relative()` is host-shaped — on Unix it returns
    // true for `C:\foo` because Unix only knows `/` as a root. Gating
    // on `starts_with(CLIPS_DIR)` first makes us robust to a Windows-
    // authored bundle being loaded on macOS.
    for cell in &mut project.library.cells {
        if let CellSpecSource::File { path: rel } = &mut cell.source
            && rel.starts_with(CLIPS_DIR)
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
    // hit. The key includes mtime+size+CRC prefix, so any change to the
    // bundle produces a new directory; we never have to worry about a
    // stale extraction with the right name.
    if root.join(PROJECT_ENTRY).is_file() {
        return Ok(root);
    }

    // Miss: extract afresh. Wipe any partial directory left over from a
    // previous failed extraction sharing this key (extremely unlikely
    // given the strong key, but cheap to be safe).
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
        // `enclosed_name` rejects absolute paths and `..` traversal.
        // Fail-closed: a single suspicious entry aborts the whole
        // extraction. The alternative (warn and continue) leaves the
        // cache dir half-populated, and the next load would hit that
        // half-populated dir as a "cache hit" — much worse than a clear
        // error here.
        let Some(rel) = entry.enclosed_name() else {
            bail!(
                "bundle entry {:?} is not enclosed in the archive root",
                entry.name()
            );
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

/// Per-bundle cache directory name:
/// `<size>-<mtime_secs>-<mtime_nanos>-<crc32 of first 64 KiB>`.
///
/// The CRC32 prefix guards against the case where HFS+ rounds mtime to
/// whole seconds and two materially-different bundles share a
/// `(size, mtime_secs, 0)` tuple. We hash a fixed-size head (not the
/// whole file) because bundles can be hundreds of MB and the cost of
/// reading them in full just to compute a cache key is unjustified —
/// the head plus the metadata triple is more than enough to discriminate
/// in practice.
fn extraction_key(path: &Path, meta: &fs::Metadata) -> Result<String> {
    let size = meta.len();
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime for {}", path.display()))?;
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    let head_crc = head_crc32(path)
        .with_context(|| format!("hashing head of {}", path.display()))?;

    Ok(format!(
        "{size}-{}-{}-{:08x}",
        dur.as_secs(),
        dur.subsec_nanos(),
        head_crc,
    ))
}

/// CRC32 of up to the first 64 KiB of `path`. We use `crc32fast` (already
/// in the dep-tree via `zip`) rather than rolling our own — the cache
/// key only needs to be stable and well-distributed, not cryptographic.
fn head_crc32(path: &Path) -> std::io::Result<u32> {
    const HEAD_BYTES: usize = 64 * 1024;
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; HEAD_BYTES];
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&buf[..filled]);
    Ok(hasher.finalize())
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

/// Replace any character that would confuse a zip reader or an OS path
/// resolver with `_`. We over-sanitize on every host (not just Windows)
/// so a bundle authored on Unix with a name like `final:cut.mp4` still
/// extracts cleanly on Windows.
///
/// Known edge case (not handled here): Windows-reserved DOS device names
/// like `CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9` would still fail
/// to extract on Windows. Treating that as out-of-scope for v1.
fn sanitize_basename(name: &str) -> String {
    // The union of Unix-illegal (`/`, `\`, `\0`) and Windows-illegal
    // (`<>:"|?*`) characters. The control-char check catches the rest
    // (newline, tab, etc.) that some filesystems also reject.
    const BAD: &[char] = &['/', '\\', '\0', '<', '>', ':', '"', '|', '?', '*'];
    let cleaned: String = name
        .chars()
        .map(|c| if BAD.contains(&c) || c.is_control() { '_' } else { c })
        .collect();
    // Windows also rejects names ending in `.` or ` ` (the shell
    // silently strips them, which produces surprising collisions). Trim
    // those from the tail on every host for the same portability reason.
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "clip".to_owned()
    } else {
        trimmed.to_owned()
    }
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

    fn one_cell_project(path: &Path) -> Project {
        Project {
            composition: CompositionSpec { width: 320, height: 240 },
            library: LibrarySpec {
                layers: 1,
                columns: 1,
                cells: vec![CellSpec {
                    row: 0,
                    col: 0,
                    source: CellSpecSource::File { path: path.to_path_buf() },
                    defaults: ClipDefaults {
                        looping: false,
                        speed: 1.0,
                        blend: BlendMode::Normal,
                    },
                }],
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

    /// Two cells pointing at the same literal source path must share a
    /// single zip entry — both at save time (don't write the bytes
    /// twice) and at load time (both cells resolve to the same extracted
    /// file).
    #[test]
    fn identical_source_paths_dedupe_to_one_entry() {
        with_tmp("dedupe", |dir| {
            let clip = dir.join("shared/intro.mp4");
            write_fake_clip(&clip, b"SHARED-PAYLOAD");

            let project = project_with_two_clips(&clip, &clip);
            let bundle = dir.join("dedupe.sublyve");
            save_to_path(&project, &bundle).expect("save bundle");

            // Inspect the on-disk zip directly: only one `clips/` entry.
            let file = fs::File::open(&bundle).expect("open bundle");
            let archive = ZipArchive::new(file).expect("read zip");
            let clip_entries = (0..archive.len())
                .map(|i| archive.name_for_index(i).unwrap_or(""))
                .filter(|n| n.starts_with(CLIPS_DIR))
                .count();
            assert_eq!(clip_entries, 1, "duplicate source paths must dedupe");

            let loaded = load_from_path(&bundle).expect("load bundle");
            let p0 = match &loaded.library.cells[0].source {
                CellSpecSource::File { path } => path.clone(),
                _ => unreachable!(),
            };
            let p1 = match &loaded.library.cells[1].source {
                CellSpecSource::File { path } => path.clone(),
                _ => unreachable!(),
            };
            assert_eq!(p0, p1, "both cells point at the same extracted clip");
            assert_eq!(fs::read(&p0).unwrap(), b"SHARED-PAYLOAD");
        });
    }

    /// A truncated zip must produce a clean error from `ZipArchive::new`,
    /// not a panic deeper in `extract`.
    #[test]
    fn corrupt_zip_errors_cleanly() {
        with_tmp("corrupt", |dir| {
            let clip = dir.join("a.mp4");
            write_fake_clip(&clip, b"hello");
            let project = one_cell_project(&clip);
            let bundle = dir.join("c.sublyve");
            save_to_path(&project, &bundle).expect("save bundle");

            // Truncate the bundle to half its length — guaranteed to
            // break the central directory at the tail of a zip.
            let bytes = fs::read(&bundle).expect("read bundle");
            fs::write(&bundle, &bytes[..bytes.len() / 2]).expect("truncate");

            let err = load_from_path(&bundle).expect_err("must error");
            let msg = format!("{err:#}");
            assert!(
                msg.to_lowercase().contains("zip")
                    || msg.contains("bundle"),
                "corrupt-zip error should mention zip/bundle context: {msg}"
            );
        });
    }

    /// A zip that doesn't contain `project.sublyve.json` must fail to
    /// load with a clear error (not a silent empty project).
    #[test]
    fn missing_project_entry_errors() {
        with_tmp("noproj", |dir| {
            let bundle = dir.join("empty.sublyve");
            {
                let file = fs::File::create(&bundle).expect("create");
                let mut zip = ZipWriter::new(BufWriter::new(file));
                zip.start_file(
                    "clips/orphan.mp4",
                    SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
                )
                .expect("start file");
                zip.write_all(b"no project json next to me")
                    .expect("write");
                zip.finish().expect("finish");
            }

            let err = load_from_path(&bundle).expect_err("must error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(PROJECT_ENTRY) || msg.to_lowercase().contains("project"),
                "missing-project-entry error should reference the JSON: {msg}"
            );
        });
    }

    /// A bundle whose embedded JSON claims a version greater than
    /// `CURRENT_VERSION` must be rejected via the standard project
    /// loader path (proving migrations and version checks run on bundle
    /// JSON too, not just loose files).
    #[test]
    fn future_version_inside_bundle_errors() {
        with_tmp("future", |dir| {
            let bundle = dir.join("future.sublyve");
            {
                let file = fs::File::create(&bundle).expect("create");
                let mut zip = ZipWriter::new(BufWriter::new(file));
                let opts = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated);
                zip.start_file(PROJECT_ENTRY, opts).expect("start file");
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
                zip.write_all(json.as_bytes()).expect("write");
                zip.finish().expect("finish");
            }

            let err = load_from_path(&bundle).expect_err("must error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("999"),
                "future-version error should mention the bad version: {msg}"
            );
        });
    }

    /// A bundle whose zip contains a path-traversal entry must fail
    /// closed (not warn-and-continue), so a half-populated extraction
    /// can't masquerade as a cache hit on the next load.
    #[test]
    fn path_traversal_entry_fails_closed() {
        with_tmp("traversal", |dir| {
            let bundle = dir.join("evil.sublyve");
            {
                let file = fs::File::create(&bundle).expect("create");
                let mut zip = ZipWriter::new(BufWriter::new(file));
                let opts = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Stored);
                // Two entries: one valid project JSON, one trying to
                // climb out of the extraction root.
                zip.start_file(PROJECT_ENTRY, opts).expect("start proj");
                zip.write_all(br#"{"version":4,"project":{"composition":{"width":1,"height":1},"library":{"layers":1,"columns":1,"cells":[]},"layers":[],"output":{"monitor_index":0,"fullscreen":false},"audio":{"device_name":null,"master_volume":1.0}}}"#).expect("write proj");
                zip.start_file("../escape.txt", opts).expect("start evil");
                zip.write_all(b"pwned").expect("write evil");
                zip.finish().expect("finish");
            }

            let err = load_from_path(&bundle).expect_err("must error");
            let msg = format!("{err:#}");
            assert!(
                msg.to_lowercase().contains("enclosed")
                    || msg.contains("escape"),
                "path-traversal error should mention the bad entry: {msg}"
            );
        });
    }

    /// If the embedded JSON references a clip the archive doesn't carry,
    /// `load_from_path` succeeds (the per-cell missing-clip warning fires
    /// downstream in `apply_project`, not here). Just verify the loader
    /// itself doesn't error.
    #[test]
    fn missing_clip_in_archive_still_loads() {
        with_tmp("missing-clip", |dir| {
            let bundle = dir.join("partial.sublyve");
            {
                let file = fs::File::create(&bundle).expect("create");
                let mut zip = ZipWriter::new(BufWriter::new(file));
                let opts = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated);
                zip.start_file(PROJECT_ENTRY, opts).expect("start proj");
                // Cell references `clips/gone.mp4` — never added.
                let json = r#"{
                    "version": 4,
                    "project": {
                        "composition": {"width": 1, "height": 1},
                        "library": {
                            "layers": 1, "columns": 1,
                            "cells": [
                                {"row": 0, "col": 0,
                                 "source": {"type": "File", "path": "clips/gone.mp4"},
                                 "defaults": {"looping": false, "speed": 1.0, "blend": "Normal"}}
                            ]
                        },
                        "layers": [{"index": 0, "blend": "Normal", "opacity": 1.0,
                                    "mute": false, "audio_gain": 1.0, "master": 1.0, "solo": false}],
                        "output": {"monitor_index": 0, "fullscreen": false},
                        "audio": {"device_name": null, "master_volume": 1.0}
                    }
                }"#;
                zip.write_all(json.as_bytes()).expect("write");
                zip.finish().expect("finish");
            }

            let loaded = load_from_path(&bundle).expect("loader tolerates missing clip");
            assert_eq!(loaded.library.cells.len(), 1);
            match &loaded.library.cells[0].source {
                CellSpecSource::File { path } => {
                    // Path got rewritten under the extraction root, but the
                    // file isn't there — `apply_project` handles that case.
                    assert!(path.is_absolute(), "should still be rewritten to absolute");
                    assert!(!path.exists(), "but the clip itself is absent");
                }
                other => panic!("expected File source, got {other:?}"),
            }
        });
    }

    #[test]
    fn sanitize_basename_replaces_windows_illegal_chars() {
        // Both directions of slash plus the Windows-illegal punctuation.
        assert_eq!(sanitize_basename("final:cut.mp4"), "final_cut.mp4");
        assert_eq!(sanitize_basename("a/b.mp4"), "a_b.mp4");
        assert_eq!(sanitize_basename("a\\b.mp4"), "a_b.mp4");
        assert_eq!(sanitize_basename(r#"weird<>|"?*.mp4"#), "weird______.mp4");
        // Trailing dots and spaces are stripped (Windows would silently
        // strip them and produce surprising collisions otherwise).
        assert_eq!(sanitize_basename("foo. "), "foo");
        assert_eq!(sanitize_basename("...   "), "clip");
        // Control chars (here a literal \n) become `_`.
        assert_eq!(sanitize_basename("line1\nline2.mp4"), "line1_line2.mp4");
    }
}
