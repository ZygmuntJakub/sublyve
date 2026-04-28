# sublyve

Rust AV compositing engine — a Resolume-inspired real-time video playback and compositing tool.

## Architecture

```
crates/
├── core/          types       AvError, BlendMode, VideoFrame
├── playback/      decode      FFmpeg 8 decoder (drain-on-EOF, real PTS, scaled-output)
├── compositor/    GPU         GpuContext + VideoPipelines (per blend mode)
│                              + VideoTexture + CompositionTarget + Thumbnail + WindowSurface
└── app/           binary      winit + egui — control window with grid/preview/cue/Take
                              + clean fullscreen output window
                              Library (2D grid) · Layer · Composition · thumbs
```

**Composition model.** Every layer (`Layer`) owns its own `Decoder + Transport + VideoTexture`. On each tick, every layer pulls a frame, then `Composition::render` draws all visible layers — back-to-front — into a shared offscreen `CompositionTarget` (default `1920×1080`, sRGB). Each `WindowSurface` then blits the composition target to its surface, letterboxed to fit. Result: one decode + one composition per tick, two surfaces drawing the same composition.

## Quick start

```bash
brew install ffmpeg@8

# Default: 4 layers × 8 columns. First arg gets pre-loaded into (0, 0).
PKG_CONFIG_PATH="/opt/homebrew/opt/ffmpeg@8/lib/pkgconfig" \
  cargo run --release -- sample.mp4

# Custom grid + composition resolution + projector setup + audio:
PKG_CONFIG_PATH="..." cargo run --release -- \
  --layers 6 --columns 10 \
  --composition-size 1920x1080 \
  --output-monitor 1 --fullscreen \
  --audio-device "External Headphones" \
  clip1.mp4 clip2.mp4 clip3.mp4

# Discover monitors / audio devices:
PKG_CONFIG_PATH="..." cargo run --release -- --list-monitors
PKG_CONFIG_PATH="..." cargo run --release -- --list-audio-devices
```

Drag video files onto cells in the grid to add them to the library. `RUST_LOG=debug` enables verbose tracing.

## UI

Two windows:

- **Control** (`avengine — control`) — top transport bar; central clip grid (rows = layers, cols = columns); left panel with the live **Output** preview, the **Cue** preview, the **TAKE** button, and Output settings (monitor + fullscreen); right panel with the **Layer settings** for whichever layer you've selected (click an `L0`/`L1`/… row label on the left of the grid); bottom panel with the **Clip inspector** for whichever cell is currently in the cue.
- **Output** (`avengine — output`) — clean composition, no overlay. Drag onto a projector and press `F`.

**Clip inspector (bottom panel).** The cue parks on a cell — filled or empty — and the bottom panel takes its cue from there:

- **Empty cell cued** → shows a `Browse…` button that opens a native file dialog (`rfd`); the picked file imports into the cell, and the panel auto-switches to the metadata view on the next frame.
- **Filled cell cued** → shows the clip's thumbnail, name, full path, source size, and per-clip default settings (loop / speed / blend). Defaults are applied every time the clip is triggered onto its layer; you can still override them mid-play from the right-hand layer inspector.
- **No cue** → small hint.

**Click semantics.**

- **Click** a cell → trigger that clip on its layer (loads + plays immediately).
- **Shift+click** a cell → cue it: the clip plays on a hidden preview deck so you can preview it in the **Cue** pane without sending it to output. The TAKE button (or `Enter`) then promotes the cued clip to its real layer.
- **Right-click** a cell → stop the layer that owns the cell.
- **Double-click** is the same as click (kept for muscle memory).
- **Drag** a video file onto a cell → import it into that cell. If the cell's layer is currently empty, the new clip auto-triggers on it.

Triggering a clip auto-selects its layer in the right-hand inspector.

**Shortcuts:**

| key   | window  | action                                                           |
|-------|---------|------------------------------------------------------------------|
| Space | both    | toggle composition play/pause (every loaded layer)               |
| R     | both    | restart every layer                                              |
| Enter | both    | Take the cued clip                                               |
| F     | both    | toggle output-window fullscreen                                  |
| M     | both    | cycle output to next monitor                                     |
| Esc   | output  | leave fullscreen                                                 |
| Esc   | control | clear cue (or quit if nothing cued)                              |

## Save / Load project

`💾 Save…` and `📂 Open…` in the top transport bar persist the workspace as a JSON project file (`.sublyve.json`). The format is human-readable and version-stamped (`{ "version": 1, "project": { … } }`); the loader rejects newer versions with a clear error rather than misinterpreting fields.

What's saved:

- **Library cells**: every occupied `(row, col)` with its absolute path and per-clip defaults (loop / speed / blend).
- **Per-layer compositing**: `blend_mode`, `opacity`, `mute`, `audio_gain` for every layer.
- **Composition**: target width × height. Loading resizes the offscreen target if it differs.
- **Output**: monitor index (best-effort by index across reboots), fullscreen flag.
- **Audio**: device name, master volume.

What's deliberately **not** saved (the file represents your *setup*, not a performance moment):

- Active clip on each layer, transport position / playing / looping / speed — these come from the per-clip defaults on the next trigger.
- Cue, hovered cell, selected layer.

A "snapshot" / scene system that captures active state for choreographed recall is a deliberate future milestone — different concept from a project file.

**Path policy**: absolute paths in V1. If a saved clip's source file has moved or been deleted, the loader logs a warning and skips that cell; the rest still load.

**Auto-resume on startup**: launching with no arguments reopens the last project you saved or opened. Override with `--no-resume` (start with an empty workspace) or `--project /path/to/file.sublyve.json` (open that specific file). CLI clip arguments still win — `cargo run -- foo.mp4` always preloads `foo.mp4` and skips auto-resume.

The last-project path lives in the OS config directory:
- macOS: `~/Library/Application Support/sublyve/config.json`
- Linux: `~/.config/sublyve/config.json`
- Windows: `%APPDATA%\sublyve\config.json`

## Audio

Each layer decodes both the video and audio streams of its clip in a single FFmpeg pass; audio is resampled to **48 kHz f32 stereo** and pushed into a per-layer SPSC ring buffer (`ringbuf 0.4`). A `cpal` output stream — opened on the default device or whichever `--audio-device` names — runs a real-time callback that pulls from every layer's buffer, multiplies by the per-layer gain, sums, and applies a master volume + clamp.

- **Per-layer audio gain** in the right-hand layer inspector (0.0 – 2.0).
- **Per-layer mute** silences both video and audio.
- **Master volume** in the left-panel Audio section.
- **Device selection**: at startup via `--audio-device <name>` (use `--list-audio-devices` to discover names), or live from the left-panel **Audio · Output** combobox. Switching mid-session drops the active cpal stream, allocates fresh per-layer ring buffers (preserving each layer's gain / mute settings via the shared `Arc<AudioLayerControl>`), and builds a new stream — a brief audio gap, no app stall.
- Clips with no audio stream are silently skipped on the audio side; their video plays normally.

Audio + video are not yet PTS-locked — they share a wall-clock pump driven by the video frame rate, so they'll drift over very long clips. For tight A/V sync (PTS-driven master clock) see the roadmap.

## Design notes

- **Premultiplied alpha throughout.** The fragment shader emits `vec4(rgb * a, a)` where `a = src.a * opacity`, so the four blend states (`Normal`, `Add`, `Multiply`, `Screen`) all behave correctly under the per-layer opacity slider with no per-mode shader branching.
- **One pipeline per blend mode.** Switching modes is a `set_pipeline` call, not a uniform tweak; blend state is baked into the pipeline.
- **Composition target is fixed.** The output surface blits the composition target letterboxed to fit. The control window samples the same texture inside an egui `Image` for the live Output preview pane (registered once via `egui_wgpu::Renderer::register_native_texture`). Resizing windows doesn't reallocate GPU memory; only `--composition-size` does.
- **One decoder per layer (V1: main thread).** `Layer::tick` runs each layer's send-packet / receive-frame loop with `send_eof` + drain. Frame rate is read from each clip's container (`avg_frame_rate`).
- **Sync decode budget.** ~5 ms per layer per frame on Apple silicon for 1080p H.264 → 4 layers @ 30 fps comfortably hits vsync. 60 fps clips, 4K layers, or 6+ layers will start dropping frames; threaded decode is the next milestone.
- **Color.** Video uploaded as `Rgba8UnormSrgb`, composition target same, surface picked sRGB. Symmetric sRGB↔linear at every stage; matches egui's pipeline.
- **`unsafe` count.** Zero in our code. The egui render-pass crossing uses `RenderPass::forget_lifetime`.
- **Errors.** Libs use `AvError` (`thiserror`); the binary uses `anyhow` at the boundary. Surface acquisition handles `Lost`/`Outdated`/`Timeout` instead of panicking.

## Roadmap

- [ ] **Threaded decode** — one worker per layer with a bounded frame channel. Mandatory before pushing past ~4×1080p layers or any 60 fps content.
- [ ] **PTS-locked A/V sync** — drive layer playback off the audio stream's master clock so very long clips don't drift.
- [ ] **Snapshots / scenes** — capture active clip + transport state per layer for choreographed recall. Different concept from a project file (which is the *setup*, not a moment).
- [ ] **Recent files** menu and Cmd+S resaves to the current file.
- [ ] **Project-relative paths** for portable bundles (zip the project file + clips together and move them between machines).
- [ ] Per-clip transform (Position X/Y, Scale, Rotate) — Resolume parameter inspector parity.
- [ ] Column launch (one shortcut triggers every layer at column N).
- [ ] Native file picker (replace the drag-drop-only flow).
- [ ] Solo (alongside the existing Mute).
- [ ] More blend modes — `Overlay` needs a custom shader, not a fixed-function blend state.
- [ ] Composition save/load.
- [ ] Effects pipeline (brightness, contrast, HSV, chroma key) — render targets per layer chain through effect passes.
- [ ] Audio decode + FFT beat detection + BPM sync.
- [ ] NDI / Syphon output.
- [ ] Projection mapping (mesh warp).
