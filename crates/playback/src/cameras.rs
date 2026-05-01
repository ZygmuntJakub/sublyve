//! Live capture device enumeration.
//!
//! `list()` returns a `Vec<CameraDevice>` describing every video input
//! the host platform reports.
//!
//! ## Platform notes
//!
//! - **macOS** (`avfoundation`): FFmpeg's `avdevice_list_input_sources`
//!   doesn't work for `avfoundation` — that backend doesn't implement
//!   the device-list callback. Instead we spawn `ffmpeg -f avfoundation
//!   -list_devices true -i ""` and parse the device listing from its
//!   stderr (`[N] Display Name` lines, grouped under "AVFoundation
//!   video devices:" / "AVFoundation audio devices:" headers). Screen
//!   captures ("Capture screen N") are filtered out.
//! - **Linux** (`v4l2`): `avdevice_list_input_sources` works for the
//!   `v4l2` input format; we use it.
//! - **Windows** (`dshow`): same subprocess strategy as macOS.

use std::ffi::{CStr, CString};

use ffmpeg_next as ffmpeg;
use ffmpeg::sys;

use crate::AvError;

/// One enumerated capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// FFmpeg input format identifier — `"avfoundation"` / `"v4l2"`
    /// / `"dshow"`. Pass to `Decoder::open_camera`.
    pub format_name: String,
    /// FFmpeg-native device URL. For avfoundation we encode the
    /// "video[:audio]" selector here so the decoder gets both streams
    /// in one open call.
    pub device: String,
    /// Human-readable label for the UI and the project file.
    pub display_name: String,
    /// True if a matching audio capture device was paired with this
    /// video device. When false, the camera opens video-only.
    pub has_audio: bool,
}

/// Enumerate every video capture device on the host. Returns an empty
/// Vec if nothing is detected (or if the platform-specific enumeration
/// path failed); never errors loudly — the Camera tab UI shows "no
/// cameras detected" rather than blocking.
pub fn list() -> Result<Vec<CameraDevice>, AvError> {
    ffmpeg::init().map_err(AvError::ffmpeg)?;
    ffmpeg::device::register_all();

    if cfg!(target_os = "macos") {
        Ok(enumerate_via_ffmpeg_subprocess("avfoundation"))
    } else if cfg!(target_os = "windows") {
        Ok(enumerate_via_ffmpeg_subprocess("dshow"))
    } else {
        // Linux + the long tail: avdevice_list_input_sources actually
        // works for v4l2.
        Ok(enumerate_via_avdevice("v4l2", "alsa"))
    }
}

// ---- Subprocess-based enumeration (macOS / Windows) ----

/// Spawn `ffmpeg -f <format> -list_devices true -i ""`, parse the
/// device list from stderr. Empty Vec if `ffmpeg` isn't on PATH or the
/// output doesn't match the expected shape.
fn enumerate_via_ffmpeg_subprocess(format_name: &str) -> Vec<CameraDevice> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-f", format_name, "-list_devices", "true", "-i", ""])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                "camera enumeration: failed to spawn ffmpeg ({e}) — install with `brew install ffmpeg@8`"
            );
            return Vec::new();
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_device_listing(format_name, &stderr)
}

/// Parse the `[AVFoundation indev @ 0x…] [N] Name` style output that
/// both `avfoundation` and `dshow` emit when called with
/// `-list_devices true`.
fn parse_device_listing(format_name: &str, stderr: &str) -> Vec<CameraDevice> {
    let mut video = Vec::<RawDevice>::new();
    let mut audio = Vec::<RawDevice>::new();
    let mut state = ParseSection::Init;

    for line in stderr.lines() {
        // Strip the leading `[X indev @ 0x…] ` prefix if present.
        let payload = match line.split_once("] ") {
            Some((_, rest)) => rest,
            None => line,
        }
        .trim_end();

        if let Some(s) = next_section(payload) {
            state = s;
            continue;
        }
        let Some((idx, name)) = parse_indexed_device(payload) else {
            continue;
        };

        // Filter out screen captures — they're not VJ-relevant
        // (handled by the dedicated screen-capture roadmap item).
        if name.starts_with("Capture screen") {
            continue;
        }

        let raw = RawDevice { name: idx.to_string(), description: name };
        match state {
            ParseSection::Video => video.push(raw),
            ParseSection::Audio => audio.push(raw),
            ParseSection::Init => {}
        }
    }

    video
        .into_iter()
        .map(|v| pair_with_audio(format_name, v, &audio))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseSection {
    Init,
    Video,
    Audio,
}

fn next_section(payload: &str) -> Option<ParseSection> {
    let lower = payload.to_ascii_lowercase();
    if lower.contains("video devices") {
        Some(ParseSection::Video)
    } else if lower.contains("audio devices") {
        Some(ParseSection::Audio)
    } else {
        None
    }
}

/// Parse a `[N] Display Name` line into `(N, "Display Name")`.
fn parse_indexed_device(payload: &str) -> Option<(usize, String)> {
    let payload = payload.trim_start();
    let rest = payload.strip_prefix('[')?;
    let (idx_str, after) = rest.split_once(']')?;
    let idx = idx_str.trim().parse::<usize>().ok()?;
    let name = after.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some((idx, name))
    }
}

// ---- avdevice-based enumeration (Linux / fallback) ----

struct RawDevice {
    name: String,
    description: String,
}

fn enumerate_via_avdevice(video_format: &'static str, audio_format: &'static str) -> Vec<CameraDevice> {
    let video = enumerate_avdevice_for(video_format).unwrap_or_default();
    let audio = enumerate_avdevice_for(audio_format).unwrap_or_default();
    video
        .into_iter()
        .map(|v| pair_with_audio(video_format, v, &audio))
        .collect()
}

fn enumerate_avdevice_for(format_name: &'static str) -> Option<Vec<RawDevice>> {
    let fmt_cstr = CString::new(format_name).ok()?;
    let format = unsafe { sys::av_find_input_format(fmt_cstr.as_ptr()) };
    if format.is_null() {
        return None;
    }

    let mut list_ptr: *mut sys::AVDeviceInfoList = std::ptr::null_mut();
    let ret = unsafe {
        sys::avdevice_list_input_sources(
            format,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut list_ptr,
        )
    };
    if ret < 0 || list_ptr.is_null() {
        return None;
    }

    let mut devices = Vec::new();
    unsafe {
        let nb = (*list_ptr).nb_devices as isize;
        let arr = (*list_ptr).devices;
        for i in 0..nb {
            let info_ptr = *arr.offset(i);
            if info_ptr.is_null() {
                continue;
            }
            let name = c_str_to_string((*info_ptr).device_name);
            let description = c_str_to_string((*info_ptr).device_description);
            devices.push(RawDevice { name, description });
        }
        sys::avdevice_free_list_devices(&mut list_ptr);
    }
    Some(devices)
}

// ---- Pairing + URL formatting ----

fn pair_with_audio(
    format_name: &str,
    video: RawDevice,
    audio: &[RawDevice],
) -> CameraDevice {
    let matched = audio.iter().find(|a| names_likely_paired(&a.description, &video.description));

    let display_name = video.description.clone();
    let device = format_camera_url(format_name, &video, matched);
    let has_audio = matched.is_some();

    CameraDevice {
        format_name: format_name.to_string(),
        device,
        display_name,
        has_audio,
    }
}

fn names_likely_paired(a: &str, b: &str) -> bool {
    let an = a.to_ascii_lowercase();
    let bn = b.to_ascii_lowercase();
    if an == bn {
        return true;
    }
    // Take the longest common shared prefix-word and accept if it's
    // ≥ 4 chars — catches "Jakub's iPhone Camera" / "Jakub's iPhone
    // Microphone" but rejects "FaceTime HD Camera" / "MacBook Pro
    // Microphone".
    let common = an
        .split_whitespace()
        .zip(bn.split_whitespace())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len() + 1)
        .sum::<usize>();
    common >= 4
}

fn format_camera_url(
    format_name: &str,
    video: &RawDevice,
    audio: Option<&RawDevice>,
) -> String {
    match format_name {
        "avfoundation" => match audio {
            Some(a) => format!("{}:{}", video.name, a.name),
            None => format!("{}:none", video.name),
        },
        "dshow" => match audio {
            Some(a) => format!("video={}:audio={}", video.description, a.description),
            None => format!("video={}", video.description),
        },
        // v4l2 takes a path; audio is a separate input format (alsa)
        // not coverable in one URL, so we open video-only.
        _ => video.name.clone(),
    }
}

fn c_str_to_string(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_likely_paired_matches_iphone() {
        assert!(names_likely_paired(
            "Jakub's iPhone Microphone",
            "Jakub's iPhone Camera"
        ));
    }

    #[test]
    fn names_likely_paired_case_insensitive() {
        assert!(names_likely_paired("Logitech BRIO", "logitech brio"));
    }

    #[test]
    fn names_likely_paired_rejects_unrelated() {
        assert!(!names_likely_paired(
            "MacBook Pro Microphone",
            "FaceTime HD Camera"
        ));
    }

    #[test]
    fn parses_avfoundation_listing() {
        let stderr = "\
[AVFoundation indev @ 0x123] AVFoundation video devices:
[AVFoundation indev @ 0x123] [0] FaceTime HD Camera
[AVFoundation indev @ 0x123] [1] Jakub's iPhone Camera
[AVFoundation indev @ 0x123] [2] Capture screen 0
[AVFoundation indev @ 0x123] AVFoundation audio devices:
[AVFoundation indev @ 0x123] [0] Jakub's iPhone Microphone
[AVFoundation indev @ 0x123] [1] MacBook Pro Microphone
[in#0 @ 0xb6d010000] Error opening input: Input/output error
";
        let devices = parse_device_listing("avfoundation", stderr);
        assert_eq!(devices.len(), 2, "screen capture must be filtered out");

        // FaceTime HD Camera has no plausibly-paired mic.
        let facetime = &devices[0];
        assert_eq!(facetime.display_name, "FaceTime HD Camera");
        assert!(!facetime.has_audio);
        assert_eq!(facetime.device, "0:none");

        // iPhone camera + mic should pair → "1:0".
        let iphone = &devices[1];
        assert_eq!(iphone.display_name, "Jakub's iPhone Camera");
        assert!(iphone.has_audio);
        assert_eq!(iphone.device, "1:0");
    }

    /// Just verify the public entrypoint doesn't panic on hosts
    /// without cameras (CI).
    #[test]
    fn list_returns_without_panicking() {
        let _ = list();
    }
}
