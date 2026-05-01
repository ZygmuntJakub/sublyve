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

- **Control** (`avengine — control`) — top transport bar; central clip grid (rows = layers, cols = columns); right panel **tabbed** (`Preview` / `Video` / `Audio` / `Project`) — Preview shows the live **Output** preview, the **Cue** preview, and the **TAKE** button; Video has the output monitor + fullscreen settings; Audio has device routing + master volume; Project has the composition setup (layer / column count, composition resolution). Bottom panel is **tabbed** (`Layer` / `Clip`) — Layer tab shows the inspector for whichever layer you've selected (click an `L0`/`L1`/… row label on the left of the grid), Clip tab shows the inspector for whichever cell is currently in the cue. The bottom panel auto-switches tab based on the action you just took: shift+click a cell (cue) → Clip tab; click a cell (trigger) or click a layer row label → Layer tab.
- **Output** (`avengine — output`) — clean composition, no overlay. Drag onto a projector and press `F`.

**Clip inspector (bottom panel · Clip tab).** The cue parks on a cell — filled or empty — and the Clip tab takes its cue from there:

- **Empty cell cued** → shows a `Browse…` button that opens a native file dialog (`rfd`); the picked file imports into the cell, and the panel auto-switches to the metadata view on the next frame.
- **Filled cell cued** → shows the clip's thumbnail, name, full path, source size, and per-clip default settings (loop / speed / blend). Defaults are applied every time the clip is triggered onto its layer; you can still override them mid-play from the right-hand layer inspector.
- **No cue** → small hint.

**Click semantics.**

- **Click** a cell → trigger that clip on its layer (loads + plays immediately).
- **Shift+click** a cell → cue it: the clip plays on a hidden preview deck so you can preview it in the **Cue** pane (right panel · Preview tab) without sending it to output. The TAKE button (or `Enter`) then promotes the cued clip to its real layer.
- **Right-click** a cell → stop the layer that owns the cell.
- **Double-click** is the same as click (kept for muscle memory).
- **Drag** a video file onto a cell → import it into that cell. If the cell's layer is currently empty, the new clip auto-triggers on it.

Triggering a clip auto-selects its layer in the Layer tab.

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

## Composition size (runtime)

The right panel's **Project** tab has the **Composition** section with `+`/`−` buttons for layer and column count. Add a layer → audio stream is rebuilt with one extra mixer source (preserving every existing layer's gain / mute settings). Remove a layer or column → clips and audio handles in the dropped row / column are released; if a layer was playing from a column you just removed, that layer goes empty. Limits: **1–16 layers**, **1–32 columns**.

The CLI's `--layers N --columns M` still sets the *initial* size at boot. The runtime UI lets you grow / shrink the grid mid-session without losing the current performance state on surviving cells.

Project files store the layer / column counts; loading a saved project resizes the running composition to match (any extra layers are dropped, missing layers are created with default settings).

## Quick controls

Each layer's row in the grid has a Resolume-style quick-controls strip on the left, between the row label and the cell columns:

- **`✕`** — clear the layer (drops its decoder; kills audio + video at once).
- **Vol** — vertical fader for the layer's audio gain (0.0–2.0).
- **Opa** — vertical fader for the layer's video opacity (0.0–1.0).
- **Mst** — **master fade** (0.0–1.0). Multiplies into both the visual opacity uniform *and* the audio mix gain at the same time, like a DJ channel fader. Drag to 0 → the whole layer fades to black and silent simultaneously, regardless of its individual opacity / volume settings.

The Layer tab in the bottom panel has the same controls with numeric labels for fine adjustment; both UIs bind to the same atomics, so changes are visible in both places live.

**Right-click any slider** (quick strip, layer inspector, master volume, per-clip default speed, seek bar) to snap it back to its default (`1.0` everywhere; the seek bar resets to `0:00`).

The Layer tab also has a regular media-player **scrub bar** (under Speed) — drag it to seek, click anywhere on it to jump there, right-click to restart. The position / duration label below shows `M:SS / M:SS` (or `H:MM:SS` past an hour). Scrub bar / loop / speed are hidden when the active source is a live capture device (camera) since live streams aren't seekable.

## Camera inputs

USB webcams (and any FFmpeg-supported capture device) appear in the **Camera** tab in the bottom panel — third tab, next to Layer / Clip. Drag a camera entry onto a grid cell to bind it; the cell now triggers / cues / TAKEs / saves like a clip cell, the cell renders a 📷 glyph instead of a thumbnail, and the camera's mic feeds the layer's audio path automatically (when a paired audio device exists). Clicking the cell triggers the camera onto its layer; right-click stops the layer; shift+click cues it on the preview deck. Hit **🔄 Refresh** if you plug in a camera mid-session — V1 doesn't auto-detect hotplug.

For *live layers* (those playing a camera), the Layer-tab inspector hides loop / speed / scrub since none apply. The Clip-tab inspector for a camera cell hides the loop / speed defaults editors but keeps the blend selector. Per-clip defaults still apply at trigger time (only `blend` is honoured for live cells).

Enumeration uses FFmpeg's `avdevice_list_input_sources` API — `avfoundation` on macOS (paired video+mic), `v4l2` on Linux (video only — alsa mic separate, not paired in V1), `dshow` on Windows. Camera identity is persisted by `format_name + device + display_name`; on project load the cell rebinds best-effort against whatever's currently enumerated and skips with a warning if unavailable. Cueing a camera opens the device twice (preview deck + main worker) — fine on macOS / Linux v4l2 with multi-reader support, may fail on platforms with exclusive camera access.

## Save / Load project

`💾 Save…` and `📂 Open…` in the top transport bar persist the workspace as a JSON project file (`.sublyve.json`). The format is human-readable and version-stamped (`{ "version": 3, "project": { … } }`); the loader rejects newer versions with a clear error rather than misinterpreting fields. Older versions still load — `version: 1` files (no `master` field on layers) come in with master defaulting to 1.0; `version: 2` files (cells held a bare `path` field) migrate cells into `source: { type: "File", path: … }` on load.

What's saved:

- **Library cells**: every occupied `(row, col)` with its source — `File { path }` for video files (absolute path) or `Camera { format_name, device, display_name }` for live capture devices — and per-clip defaults (loop / speed / blend; loop and speed are silently ignored at trigger time for camera cells).
- **Per-layer compositing**: `blend_mode`, `opacity`, `mute`, `audio_gain`, and `master` (added in schema v2) for every layer.
- **Composition**: target width × height. Loading resizes the offscreen target if it differs.
- **Output**: monitor index (best-effort by index across reboots), fullscreen flag.
- **Audio**: device name, master volume.

What's deliberately **not** saved (the file represents your *setup*, not a performance moment):

- Active clip on each layer, transport position / playing / looping / speed — these come from the per-clip defaults on the next trigger.
- Cue, hovered cell, selected layer.

A "snapshot" / scene system that captures active state for choreographed recall is a deliberate future milestone — different concept from a project file.

**Path policy**: absolute paths in V1. If a saved clip's source file has moved or been deleted, the loader logs a warning and skips that cell; the rest still load. Camera cells likewise log a warning and skip if the saved device no longer enumerates (unplugged, renamed).

**Auto-resume on startup**: launching with no arguments reopens the last project you saved or opened. Override with `--no-resume` (start with an empty workspace) or `--project /path/to/file.sublyve.json` (open that specific file). CLI clip arguments still win — `cargo run -- foo.mp4` always preloads `foo.mp4` and skips auto-resume.

The last-project path lives in the OS config directory:
- macOS: `~/Library/Application Support/sublyve/config.json`
- Linux: `~/.config/sublyve/config.json`
- Windows: `%APPDATA%\sublyve\config.json`

## Audio

Each layer decodes both the video and audio streams of its clip in a single FFmpeg pass; audio is resampled to **48 kHz f32 stereo** and pushed into a per-layer SPSC ring buffer (`ringbuf 0.4`). A `cpal` output stream — opened on the default device or whichever `--audio-device` names — runs a real-time callback that pulls from every layer's buffer, multiplies by the per-layer gain, sums, and applies a master volume + clamp.

- **Per-layer audio gain** in the bottom panel's Layer tab (0.0 – 2.0).
- **Per-layer mute** silences both video and audio.
- **Master volume** on the right panel's **Audio** tab.
- **Device selection**: at startup via `--audio-device <name>` (use `--list-audio-devices` to discover names), or live from the **Audio · Output** combobox on the same tab. Switching mid-session drops the active cpal stream, allocates fresh per-layer ring buffers (preserving each layer's gain / mute settings via the shared `Arc<AudioLayerControl>`), and builds a new stream — a brief audio gap, no app stall.
- Clips with no audio stream are silently skipped on the audio side; their video plays normally.

Audio + video are not yet PTS-locked — they share a wall-clock pump driven by the video frame rate, so they'll drift over very long clips. For tight A/V sync (PTS-driven master clock) see the roadmap.

## Design notes

- **Premultiplied alpha throughout.** The fragment shader emits `vec4(rgb * a, a)` where `a = src.a * opacity`, so the four blend states (`Normal`, `Add`, `Multiply`, `Screen`) all behave correctly under the per-layer opacity slider with no per-mode shader branching.
- **One pipeline per blend mode.** Switching modes is a `set_pipeline` call, not a uniform tweak; blend state is baked into the pipeline.
- **Composition target is fixed.** The output surface blits the composition target letterboxed to fit. The control window samples the same texture inside an egui `Image` for the live Output preview pane (registered once via `egui_wgpu::Renderer::register_native_texture`). Resizing windows doesn't reallocate GPU memory; only `--composition-size` does.
- **One decoder per layer, on its own worker thread.** `Layer::load` spawns a dedicated OS thread that owns the `Decoder` outright; the worker pushes decoded frames into a 3-deep bounded `sync_channel`, and the main thread (`Layer::tick`) only pops frames + uploads them to the GPU. Backpressure from the bounded channel paces decode at source frame rate; pausing a layer stops its decode work entirely (worker blocks on `send`). Each clip's frame rate comes from its container (`avg_frame_rate`).
- **Decode parallelism.** Decoders run concurrently on as many cores as there are loaded layers — 6+ layers at 1080p / 60 fps clips no longer choke the main thread. Audio samples are drained from the worker straight into the per-layer `ringbuf` (already SPSC, lock-free) without crossing back through the main thread.
- **Color.** Video uploaded as `Rgba8UnormSrgb`, composition target same, surface picked sRGB. Symmetric sRGB↔linear at every stage; matches egui's pipeline.
- **`unsafe` count.** Zero in our code. The egui render-pass crossing uses `RenderPass::forget_lifetime`.
- **Errors.** Libs use `AvError` (`thiserror`); the binary uses `anyhow` at the boundary. Surface acquisition handles `Lost`/`Outdated`/`Timeout` instead of panicking.

## Roadmap

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
