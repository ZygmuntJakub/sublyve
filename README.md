# sublyve

Rust AV compositing engine — a Resolume-inspired real-time video playback and compositing tool.

## Architecture

```
crates/
├── core/          types       VideoFrame, Rect, Rgba, BlendMode, AvError
├── playback/      decode      FFmpeg 7 video decoder → RGBA frames
├── compositor/    GPU         wgpu render pipeline, fullscreen quad, WGSL shader
└── app/           binary      winit + egui UI, play/pause/loop/restart
```

**Data flow:** `Decoder::next_frame() → VideoFrame → Engine::update_texture() → GPU quad render → egui overlay`

## Quick start

```bash
# Requires FFmpeg 7 (Homebrew):
brew install ffmpeg@7

# Build and run:
PKG_CONFIG_PATH="/opt/homebrew/opt/ffmpeg@7/lib/pkgconfig" cargo run --release -- video.mp4
```

**Controls:** Space = play/pause, Esc = quit

## Roadmap

- [ ] Layer stack (multiple clips, z-ordered, crossfade)
- [ ] Blend mode shaders (add, multiply, screen)
- [ ] Effects pipeline (brightness, contrast, HSV, chroma key)
- [ ] Audio reactive (FFT → beat detection)
- [ ] NDI/Syphon output
- [ ] WASM plugin system for user effects
- [ ] Projection mapping (mesh warp)
