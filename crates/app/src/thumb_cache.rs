//! Persistent on-disk cache for clip thumbnails.
//!
//! Re-opening a project would otherwise force FFmpeg to demux + decode the
//! first I-frame of every clip again (~30–80 ms per file on Apple silicon
//! for 1080p H.264). For a 30-cell project that's a noticeable freeze on
//! project load. This module memoises that work to
//! `dirs::cache_dir()/sublyve/thumbs/`.
//!
//! ## Cache key
//!
//! Path alone is unsafe: a video file may be replaced in place. Full content
//! hashing is overkill (we'd have to read the whole file before deciding
//! whether to skip the decode). The compromise — used by every other tool
//! that solves this problem — is `(canonical path, mtime, size)`. If any of
//! those three change, the key changes and we re-decode.
//!
//! ## Storage format
//!
//! Raw RGBA8 with a fixed-size header. Picking PNG would add the `image`
//! crate just to compress a 230 KB blob we wrote ourselves; raw is simpler,
//! self-describing, and writes/reads at memory-bandwidth speed.
//!
//! Header layout (16 bytes, little-endian):
//!
//! | offset | bytes | field    |
//! |--------|-------|----------|
//! | 0      | 4     | magic `b"SVT0"` (Sublyve thumb v0) |
//! | 4      | 4     | version (= [`FORMAT_VERSION`])     |
//! | 8      | 4     | width   (u32)                      |
//! | 12     | 4     | height  (u32)                      |
//!
//! Followed by `width * height * 4` raw RGBA8 bytes.
//!
//! Validation on load: magic + version + that the payload length matches
//! `width * height * 4`. Anything else is treated as a miss (we silently
//! fall through to re-decode), so a future format bump or a half-written
//! file just costs one extra decode.
//!
//! ## Scope
//!
//! v1 intentionally has no size cap, no LRU, and no eviction. Every entry
//! is keyed by content-derived metadata, so stale entries simply become
//! unreachable rather than wrong. Disk usage grows linearly with the number
//! of unique `(path, mtime, size)` triples the user has ever imported.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use avengine_core::VideoFrame;
use tracing::{debug, trace, warn};

use crate::thumbs;

/// Magic prefix identifying our raw RGBA thumbnail format.
const MAGIC: &[u8; 4] = b"SVT0";
/// Bump this whenever the on-disk layout changes; older entries become misses.
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 16;

/// Decode the first frame of `path`, using the on-disk cache if there's a
/// hit. On a miss we decode via [`thumbs::extract_thumbnail`] and write the
/// result back to disk before returning it.
///
/// I/O failures on the cache side are logged and demoted to a miss — the
/// caller still gets a `VideoFrame` as long as FFmpeg can decode the file.
/// This keeps the cache strictly an optimisation; it can never break import.
pub fn load_or_decode(path: &Path, width: u32, height: u32) -> Result<VideoFrame> {
    let key = match cache_key(path) {
        Ok(k) => Some(k),
        Err(e) => {
            // Couldn't stat the source file or canonicalise its path.
            // The decode itself may still succeed (e.g. on a path that
            // isn't canonicalisable but is openable), so don't fail —
            // just skip caching for this clip.
            debug!("thumb cache: skipping cache for {} ({e:#})", path.display());
            None
        }
    };

    if let Some(key) = key
        && let Some(path) = cache_path(key)
    {
        match read_cached(&path, width, height) {
            Ok(Some(frame)) => {
                trace!("thumb cache hit: {}", path.display());
                return Ok(frame);
            }
            Ok(None) => {} // miss — fall through
            Err(e) => warn!("thumb cache read failed ({}): {e:#}", path.display()),
        }
    }

    let frame = thumbs::extract_thumbnail(path, width, height)?;

    if let Some(key) = key
        && let Some(out) = cache_path(key)
    {
        if let Err(e) = write_cached(&out, &frame) {
            warn!("thumb cache write failed ({}): {e:#}", out.display());
        } else {
            trace!("thumb cache wrote: {}", out.display());
        }
    }

    Ok(frame)
}

/// Stable cache key for a file: `(canonical path, mtime, size)` reduced to a
/// `u64` via FNV-1a.
///
/// We deliberately do *not* use `std::collections::hash_map::DefaultHasher`:
/// its algorithm is explicitly documented as not stable across Rust releases,
/// which would silently invalidate every cached thumbnail on every toolchain
/// bump. The cache has no eviction policy, so orphaned entries would
/// accumulate forever.
///
/// FNV-1a is a fixed, versioned, well-specified hash. The inputs here total
/// ~30 bytes; cryptographic strength is irrelevant — we just need stability
/// and a low collision rate over the lifetime of a project. Bump
/// [`FORMAT_VERSION`] if this algorithm ever changes; old entries become
/// unreachable rather than wrong.
fn cache_key(path: &Path) -> Result<u64> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalising {}", path.display()))?;
    let meta = fs::metadata(&canonical)
        .with_context(|| format!("stat {}", canonical.display()))?;
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime for {}", canonical.display()))?;
    let mtime_dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let size = meta.len();

    let mut h = Fnv1a::new();
    // Hash the path as raw bytes — `Path` is `OsStr` under the hood, so on
    // Unix this is the underlying bytes and on Windows it's the WTF-8
    // encoding of the wide string. Both are stable per-platform, which is
    // all we need (the cache is keyed per-machine anyway).
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        h.write(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        h.write(canonical.to_string_lossy().as_bytes());
    }
    // Domain separator so e.g. a path that happens to end in the same byte
    // pattern as an mtime can't collide.
    h.write(b"|m|");
    h.write(&mtime_dur.as_secs().to_le_bytes());
    h.write(&mtime_dur.subsec_nanos().to_le_bytes());
    h.write(b"|s|");
    h.write(&size.to_le_bytes());
    Ok(h.finish())
}

/// FNV-1a 64-bit. ~15 lines, zero deps, byte-stable forever.
///
/// Algorithm: start with the offset basis; for each byte, XOR then multiply
/// by the FNV prime. We don't implement `std::hash::Hasher` here because
/// that trait's `write_*` integer methods are platform-endian, and we want
/// little-endian everywhere so the cache survives moving a project between
/// machines of different endianness (admittedly unlikely on modern hardware,
/// but free to get right).
struct Fnv1a(u64);

impl Fnv1a {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Fnv1a(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Resolve the cache directory and the file path for a given key. Returns
/// `None` if the platform doesn't expose a cache dir (very unusual; in
/// practice macOS/Windows/Linux all do).
fn cache_path(key: u64) -> Option<PathBuf> {
    let dir = cache_dir()?;
    Some(dir.join(format!("{key:016x}.thumb")))
}

fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("sublyve").join("thumbs"))
}

/// Read a cache entry. Returns `Ok(None)` for any "this isn't a usable
/// entry" reason — wrong magic, wrong version, mismatched dimensions, or
/// truncated payload. Only genuine I/O errors are surfaced.
fn read_cached(path: &Path, want_w: u32, want_h: u32) -> Result<Option<VideoFrame>> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::from(e)),
    };

    let mut header = [0u8; HEADER_LEN];
    if let Err(e) = file.read_exact(&mut header) {
        // Truncated header — treat as miss.
        debug!("thumb cache: short header at {} ({e})", path.display());
        return Ok(None);
    }
    if &header[0..4] != MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Ok(None);
    }
    let width = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes"));
    let height = u32::from_le_bytes(header[12..16].try_into().expect("4 bytes"));
    if width != want_w || height != want_h {
        // The cached entry was written at different default dimensions.
        // Treat as miss; the new entry will overwrite on write-back.
        return Ok(None);
    }

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("thumbnail dimensions overflow")?;
    let mut data = vec![0u8; expected_len];
    if let Err(e) = file.read_exact(&mut data) {
        debug!("thumb cache: short payload at {} ({e})", path.display());
        return Ok(None);
    }

    Ok(Some(VideoFrame::new(width, height, 0.0, data)))
}

/// Write `frame` to `path`, creating parent directories as needed.
///
/// Atomic with respect to concurrent readers: we write to a `.tmp` sibling
/// and then `rename(2)` over the final path, so a reader either sees the
/// previous entry or the new one — never a half-written file. The tmp name
/// is suffixed with the current PID so two processes racing on the same
/// cache key don't clobber each other's tmp file.
///
/// This is **not** durable across power loss: we don't `fsync` the file or
/// the directory before/after rename, so the OS is free to lose the entry
/// on a crash. The cost of a lost entry is one extra FFmpeg decode on the
/// next project load, which is exactly what this cache exists to avoid but
/// not a correctness problem — so we skip the syncs for cheaper writes.
fn write_cached(path: &Path, frame: &VideoFrame) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Per-process tmp suffix avoids two concurrent `import_clip` calls on
    // the same source file fighting over a shared `<key>.thumb.tmp`. Same
    // process racing with itself is still possible in theory but vanishingly
    // unlikely in this codebase (one worker per layer).
    let tmp = path.with_extension(format!("thumb.tmp.{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(MAGIC)?;
        file.write_all(&FORMAT_VERSION.to_le_bytes())?;
        file.write_all(&frame.width.to_le_bytes())?;
        file.write_all(&frame.height.to_le_bytes())?;
        file.write_all(&frame.data)?;
        file.sync_data().ok(); // best-effort; not load-bearing
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a synthetic frame through write + read.
    #[test]
    fn round_trip_preserves_pixels() {
        let dir = tempdir();
        let path = dir.join("test.thumb");
        let (w, h) = (4u32, 8u32);
        let n = (w * h * 4) as usize;
        // NB: don't write `(0..(w*h*4) as u8)` — the cast applies to the
        // range bound, not to each element, and silently truncates if the
        // total ever exceeds 255.
        let pixels: Vec<u8> = (0..n).map(|i| i as u8).collect();
        let frame = VideoFrame::new(w, h, 0.0, pixels.clone());

        write_cached(&path, &frame).expect("write");
        let loaded = read_cached(&path, w, h).expect("read").expect("hit");
        assert_eq!(loaded.width, w);
        assert_eq!(loaded.height, h);
        assert_eq!(loaded.data, pixels);
    }

    #[test]
    fn read_returns_none_for_missing_file() {
        let dir = tempdir();
        let path = dir.join("nope.thumb");
        let res = read_cached(&path, 320, 180).expect("not an i/o error");
        assert!(res.is_none());
    }

    #[test]
    fn read_returns_none_for_bad_magic() {
        let dir = tempdir();
        let path = dir.join("bad.thumb");
        fs::write(&path, b"NOPE\x01\x00\x00\x00\x04\x00\x00\x00\x04\x00\x00\x00\x00")
            .expect("write garbage");
        let res = read_cached(&path, 4, 4).expect("not an i/o error");
        assert!(res.is_none());
    }

    #[test]
    fn read_returns_none_for_version_mismatch() {
        let dir = tempdir();
        let path = dir.join("oldver.thumb");
        // Right magic, wrong version. Width/height/payload are valid for
        // 1x1 RGBA — we want to prove the version check rejects this
        // *before* dimension/payload checks would.
        let bogus_version: u32 = FORMAT_VERSION.wrapping_add(1);
        let mut buf = Vec::with_capacity(HEADER_LEN + 4);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&bogus_version.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // width
        buf.extend_from_slice(&1u32.to_le_bytes()); // height
        buf.extend_from_slice(&[0u8; 4]); // one RGBA pixel
        fs::write(&path, &buf).expect("write");
        let res = read_cached(&path, 1, 1).expect("not an i/o error");
        assert!(res.is_none(), "version mismatch must be a miss");
    }

    #[test]
    fn read_returns_none_for_dimension_mismatch() {
        let dir = tempdir();
        let path = dir.join("mismatch.thumb");
        let frame = VideoFrame::new(2, 2, 0.0, vec![0; 2 * 2 * 4]);
        write_cached(&path, &frame).expect("write");
        // Asking for a different size should be a miss, not a panic.
        let res = read_cached(&path, 320, 180).expect("not an i/o error");
        assert!(res.is_none());
    }

    #[test]
    fn cache_key_changes_when_mtime_changes() {
        let dir = tempdir();
        let f = dir.join("clip.bin");
        fs::write(&f, b"hello").unwrap();
        let k1 = cache_key(&f).expect("first key");
        // Bump mtime by writing different content.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&f, b"world!").unwrap();
        let k2 = cache_key(&f).expect("second key");
        assert_ne!(k1, k2, "key must change when file changes");
    }

    /// Make a unique tempdir under the OS temp dir without pulling in the
    /// `tempfile` crate. We're a single-test binary in CI; collisions are
    /// not a concern.
    fn tempdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sublyve-thumb-test-{nanos}"));
        fs::create_dir_all(&dir).expect("mk tempdir");
        dir
    }
}
