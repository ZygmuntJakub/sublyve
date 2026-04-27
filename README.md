# sublyve

Rust AV compositing engine — a Resolume-inspired real-time video playback and compositing tool.

## Architecture

```
crates/
├── core/          types       AvError, VideoFrame
├── playback/      decode      FFmpeg 8 decoder, drain-on-EOF, real PTS
├── compositor/    GPU         GpuContext + VideoPipeline + VideoTexture + WindowSurface
└── app/           binary      winit + egui — control window + clean output window
```

**GPU model:** one `GpuContext` (instance + adapter + device + queue) and one `VideoTexture` are shared across both windows. Each `WindowSurface` owns its own letterbox uniform + bind group, which it rebuilds automatically when the texture is reallocated.

**Per-tick flow:** `about_to_wait → decoder.next_frame → VideoTexture::upload (once) → both windows request_redraw → each window prepare_video + draw_video + (control window also: egui)`.

## Quick start

```bash
brew install ffmpeg@8

# Open with one or more clips. The first clip auto-activates so the
# output isn't black.
PKG_CONFIG_PATH="/opt/homebrew/opt/ffmpeg@8/lib/pkgconfig" \
  cargo run --release -- clip1.mp4 clip2.mp4 clip3.mp4

# Open the output window fullscreen on monitor 1 (e.g. a projector).
PKG_CONFIG_PATH="..." cargo run --release -- \
  --output-monitor 1 --fullscreen sample.mp4

# Discover monitors:
PKG_CONFIG_PATH="..." cargo run --release -- --list-monitors
```

Drag video files onto the control window to add them to the library.

`RUST_LOG=debug` enables verbose tracing output.

## Two-window UX

- **Control window** (`avengine — control`): clip library on the left, transport bar on top, output settings on the right, status bar on bottom. The video plays as a letterboxed preview behind the translucent panels.
- **Output window** (`avengine — output`): clean video, no overlay. Drag it to a projector and press `F` (or tick `Fullscreen`).

**Shortcuts:**

| key   | window  | action                                            |
|-------|---------|---------------------------------------------------|
| Space | both    | play / pause active deck                          |
| R     | both    | restart active deck                               |
| F     | both    | toggle output-window fullscreen                   |
| M     | both    | cycle output to the next monitor                  |
| H     | control | hide / show the UI overlay                        |
| Esc   | output  | exit fullscreen                                   |
| Esc   | control | quit the app                                      |

## Design notes

- **Color**: video uploaded as `Rgba8UnormSrgb`, surface picked sRGB. The GPU does symmetric sRGB↔linear conversion so colors match egui's pipeline.
- **Aspect ratio**: per-window `scale: vec2<f32>` uniform driven by `letterbox_scale(video, surface)` keeps the video correctly framed in both the (probably 16:10 laptop) control window and the (probably 16:9 projector) output window.
- **Decoder**: canonical FFmpeg send-packet / receive-frame loop with `send_eof` + drain at end-of-stream so B-frames buffered for reordering aren't dropped. Frame rate is read from the container.
- **Catch-up cap**: per-tick wall-clock delta is clamped to 250 ms so an unfocused window doesn't burst-decode dozens of frames on resume.
- **Errors**: libs use `AvError` (`thiserror`); the binary uses `anyhow` at the boundary. Surface acquisition handles `Lost`/`Outdated`/`Timeout` instead of panicking.
- **`unsafe` count**: zero in our code (the original `mem::transmute` for the egui pass was replaced by `RenderPass::forget_lifetime()`).

## Roadmap

- [ ] Decode on a worker thread with a bounded frame channel (today the decoder runs on the main thread; fine for one 1080p clip, will choke on 4K or multi-clip)
- [ ] Native file picker for "Open files…" (currently drag-drop only — needs `rfd`)
- [ ] Layer stack: multiple simultaneous decks with z-order, opacity, blend mode
- [ ] Blend mode pipelines (add, multiply, screen) — one `RenderPipeline` per `wgpu::BlendState`
- [ ] GPU-side YUV→RGB conversion (skip the FFmpeg software scaler hot path)
- [ ] Effects pipeline (brightness, contrast, HSV, chroma key)
- [ ] Audio decode + FFT beat detection
- [ ] NDI / Syphon output
- [ ] Projection mapping (mesh warp)
