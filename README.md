# sublyve

Rust AV compositing engine — a Resolume-inspired real-time video playback and compositing tool.

## Architecture

```
crates/
├── core/          types       AvError, VideoFrame
├── playback/      decode      FFmpeg 8 decoder, drain-on-EOF, real PTS
├── compositor/    GPU         wgpu surface, sRGB pipeline, letterboxed quad
└── app/           binary      winit + egui UI, play/pause/loop/seek/speed
```

**Data flow:** `Decoder::next_frame() → VideoFrame → Engine::upload_frame() → draw_video → egui overlay → present`

## Quick start

```bash
brew install ffmpeg@8

PKG_CONFIG_PATH="/opt/homebrew/opt/ffmpeg@8/lib/pkgconfig" \
  cargo run --release -- video.mp4
```

`RUST_LOG=debug` enables verbose tracing output.

**Controls:** Space = play/pause · R = restart · Esc = quit

## Design notes

- **Color**: video frames are uploaded as `Rgba8UnormSrgb` and rendered into an sRGB surface, so the GPU does sRGB↔linear conversion symmetrically and egui's sRGB-aware shader matches.
- **Aspect ratio**: a small uniform passes per-axis NDC scale to the vertex shader; videos are letterboxed/pillarboxed instead of stretched.
- **Decoder**: follows the canonical FFmpeg send-packet / receive-frame loop with `send_eof` + drain on end-of-stream, so frames buffered for B-frame reordering are not dropped.
- **Frame rate**: read from the container (`avg_frame_rate`, falling back to `r_frame_rate`); a per-render wall-clock accumulator is clamped to 250 ms so the app doesn't burst-decode after a stall.
- **Errors**: libs use `AvError` (`thiserror`); the binary uses `anyhow` at the boundary. Surface acquisition handles `Lost`/`Outdated`/`Timeout` instead of panicking.

## Roadmap

- [ ] Decode on a worker thread with a bounded frame channel (current decode is on the render thread)
- [ ] Layer stack (multiple clips, z-ordered, crossfade) — `Layer` trait + per-layer `Rect`, opacity, blend mode
- [ ] Blend mode pipelines (add, multiply, screen) — one render pipeline per `wgpu::BlendState`
- [ ] GPU-side YUV→RGB conversion (skip the FFmpeg software scaler hot path)
- [ ] Effects pipeline (brightness, contrast, HSV, chroma key)
- [ ] Audio decode + FFT beat detection
- [ ] NDI / Syphon output
- [ ] Projection mapping (mesh warp)
