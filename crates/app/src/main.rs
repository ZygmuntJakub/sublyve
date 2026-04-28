mod audio;
mod composition;
mod config;
mod layer;
mod library;
mod project;
mod thumbs;
mod ui;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use avengine_compositor::{AcquiredFrame, GpuContext, Thumbnail, VideoPipelines, WindowSurface};
use clap::Parser;
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, WindowAttributes, WindowId};

use composition::Composition;
use layer::Layer;
use library::{ClipSlot, Library};
use ui::{LayerView, UiActions, UiContext};

/// Cap on per-tick wall-clock catch-up after a stall (window hidden, etc.).
const MAX_CATCHUP_SECS: f64 = 0.25;

const CONTROL_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1480, 900);
const OUTPUT_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1280, 720);

const DEFAULT_LAYERS: usize = 4;
const DEFAULT_COLUMNS: usize = 8;

/// Real-time video playback / compositing engine.
///
/// A control window with a Resolume-style 2D clip grid, layer inspector
/// and live preview/cue panes; a clean fullscreen output window for the
/// projected composition.
#[derive(Parser, Debug)]
#[command(name = "avengine", version)]
struct Cli {
    /// Video files to preload into the library, filling the first row L→R.
    clips: Vec<PathBuf>,

    /// Number of layers (rows in the grid).
    #[arg(long, default_value_t = DEFAULT_LAYERS)]
    layers: usize,

    /// Number of columns in the grid.
    #[arg(long, default_value_t = DEFAULT_COLUMNS)]
    columns: usize,

    /// Composition (output) resolution as `WIDTHxHEIGHT`. The output
    /// window samples this and letterboxes to fit.
    #[arg(long, value_name = "WxH", default_value = "1920x1080")]
    composition_size: SizeArg,

    /// Index of the monitor for the *output* window. Defaults to primary.
    #[arg(long, value_name = "N")]
    output_monitor: Option<usize>,

    /// Name of the audio output device (cpal device description). If
    /// unset, the host default is used. Use `--list-audio-devices` to
    /// see what's available.
    #[arg(long, value_name = "NAME")]
    audio_device: Option<String>,

    /// Start the output window in borderless fullscreen on its monitor.
    #[arg(long, short)]
    fullscreen: bool,

    /// List available monitors and exit.
    #[arg(long)]
    list_monitors: bool,

    /// List available audio output devices and exit.
    #[arg(long)]
    list_audio_devices: bool,

    /// Don't auto-load the last-used project on startup. The CLI's
    /// positional `clips` and the `--composition-size` etc. take
    /// precedence over the saved project anyway; this flag is for the
    /// rarer case of "open me with a fresh empty workspace" while
    /// still leaving the saved last-project alone.
    #[arg(long)]
    no_resume: bool,

    /// Open this project file directly instead of the last-used one.
    /// Implies `--no-resume`. Mutually exclusive with positional
    /// clip arguments.
    #[arg(long, value_name = "FILE")]
    project: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct SizeArg(u32, u32);

impl FromStr for SizeArg {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (w, h) = s
            .split_once(['x', 'X'])
            .ok_or_else(|| anyhow!("expected WIDTHxHEIGHT, got {s:?}"))?;
        Ok(SizeArg(w.parse()?, h.parse()?))
    }
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let event_loop = EventLoop::new().context("creating winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App { state: None, cli };
    event_loop.run_app(&mut app).context("event loop")?;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}

struct App {
    state: Option<AppState>,
    cli: Cli,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        if self.cli.list_monitors {
            print_monitors(event_loop);
            event_loop.exit();
            return;
        }
        if self.cli.list_audio_devices {
            print_audio_devices();
            event_loop.exit();
            return;
        }
        match AppState::new(event_loop, &self.cli) {
            Ok(state) => self.state = Some(state),
            Err(e) => {
                error!("failed to initialize: {e:#}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(state) = self.state.as_mut() {
            state.handle_window_event(event_loop, window_id, event);
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_mut()
            && let Err(e) = state.tick() {
                error!("tick error: {e:#}");
            }
    }
}

fn print_audio_devices() {
    let (engine, _handles) = audio::AudioEngine::new(0);
    let devices = engine.list_device_names();
    let default = engine.default_device_name();
    if devices.is_empty() {
        println!("No audio output devices detected.");
        return;
    }
    println!("Available audio output devices:");
    for name in &devices {
        let star = if Some(name.as_str()) == default.as_deref() {
            " *default*"
        } else {
            ""
        };
        println!("  {name}{star}");
    }
}

fn print_monitors(event_loop: &ActiveEventLoop) {
    let primary = event_loop.primary_monitor();
    let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
    if monitors.is_empty() {
        println!("No monitors detected.");
        return;
    }
    println!("Available monitors:");
    for (i, m) in monitors.iter().enumerate() {
        let name = m.name().unwrap_or_else(|| "(unnamed)".into());
        let size = m.size();
        let pos = m.position();
        let scale = m.scale_factor();
        let is_primary = primary.as_ref() == Some(m);
        let star = if is_primary { " *primary*" } else { "" };
        println!(
            "  [{i}] {name}  {}x{}  @ ({},{})  scale {scale:.2}{star}",
            size.width, size.height, pos.x, pos.y,
        );
    }
}

struct AppState {
    gpu: GpuContext,
    /// Pipelines targeting the offscreen `CompositionTarget`
    /// (Rgba8UnormSrgb). Used by every per-layer draw.
    composition_pipelines: VideoPipelines,
    /// Pipelines targeting the window surface format. Only the `Normal`
    /// pipeline is used (one fullscreen blit per surface) but we build the
    /// full set so blend selection stays consistent if it's needed later.
    surface_pipelines: VideoPipelines,
    composition: Composition,

    control: ControlWindow,
    output: OutputWindow,

    library: Library,
    /// Layer the right-hand inspector is currently bound to.
    selected_layer: Option<usize>,
    /// `(row, col)` of the clip currently sitting in the Cue pane.
    /// Set by Shift+click; cleared by Take or Esc.
    cued: Option<(usize, usize)>,
    /// A "preview deck" — a separate layer that plays the cued clip into
    /// the Cue pane, off-output. Has its own decoder + transport;
    /// crucially NOT in `composition.layers`, so its frames never reach
    /// the projector.
    preview: Layer,
    /// Egui TextureId for the preview deck's video texture (registered
    /// once; refreshed when the preview's texture is reallocated).
    preview_egui_id: Option<egui::TextureId>,
    bound_preview_gen: u64,
    /// Last `(row, col)` an egui cell hovered, used to target file drops.
    hovered_cell: Option<(usize, usize)>,

    monitors: Vec<MonitorHandle>,

    last_tick: Instant,

    /// Egui TextureId for the live composition target (registered once;
    /// re-registered when the target's generation bumps).
    composition_egui_id: Option<egui::TextureId>,
    bound_composition_gen: u64,

    /// Audio output engine. Holds the cpal stream + per-layer mixer.
    audio_engine: audio::AudioEngine,

    /// Persistent app preferences. Mutated whenever the user saves
    /// or opens a project so the next launch can resume.
    config: config::AppConfig,
}

struct ControlWindow {
    surface: WindowSurface,
    id: WindowId,
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

struct OutputWindow {
    surface: WindowSurface,
    id: WindowId,
    selected_monitor: usize,
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop, cli: &Cli) -> Result<Self> {
        let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
        let selected_monitor = resolve_monitor_index(cli.output_monitor, &monitors, event_loop);
        let target_monitor = monitors.get(selected_monitor).cloned();

        // Both windows up front. Control window stays floating; output
        // window opens on the chosen monitor (and fullscreens if asked).
        let control_window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("avengine — control")
                        .with_inner_size(CONTROL_DEFAULT_SIZE),
                )
                .context("creating control window")?,
        );

        let mut output_attrs = WindowAttributes::default()
            .with_title("avengine — output")
            .with_inner_size(OUTPUT_DEFAULT_SIZE);
        if cli.fullscreen {
            output_attrs =
                output_attrs.with_fullscreen(Some(Fullscreen::Borderless(target_monitor.clone())));
        } else if let Some(m) = target_monitor.as_ref() {
            output_attrs = output_attrs.with_position(m.position());
        }
        let output_window = Arc::new(
            event_loop
                .create_window(output_attrs)
                .context("creating output window")?,
        );

        // Bootstrap GPU on the control surface; subsequent surfaces sit
        // on the same adapter automatically.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let control_raw = instance
            .create_surface(control_window.clone())
            .map_err(anyhow::Error::from)
            .context("creating control surface")?;
        let gpu = pollster::block_on(GpuContext::with_instance(instance, Some(&control_raw)))?;
        let output_raw = gpu
            .instance
            .create_surface(output_window.clone())
            .map_err(anyhow::Error::from)
            .context("creating output surface")?;

        let control_surface = WindowSurface::new(&gpu, control_window.clone(), control_raw)?;
        let output_surface = WindowSurface::new(&gpu, output_window.clone(), output_raw)?;

        if control_surface.format() != output_surface.format() {
            warn!(
                "surface formats differ ({:?} vs {:?}); pipelines target the control format",
                control_surface.format(),
                output_surface.format(),
            );
        }

        // Two pipeline sets — one per target format. The composition pass
        // writes to `CompositionTarget::FORMAT` (sRGB Rgba8); the surface
        // blit writes to whatever the swapchain prefers (sRGB Bgra8 on
        // Apple). A single set would crash on the second pass with a
        // wgpu validation error.
        let composition_pipelines =
            VideoPipelines::new(&gpu.device, avengine_compositor::CompositionTarget::FORMAT);
        let surface_pipelines = VideoPipelines::new(&gpu.device, control_surface.format());

        let SizeArg(comp_w, comp_h) = cli.composition_size;

        // Audio engine: allocates one ring buffer per layer up front, hands
        // back the producer-side handles, and starts the cpal stream on
        // either the requested device or the host default. We launch it
        // before composing so each Layer can be wired with its audio
        // handle at construction time.
        let (mut audio_engine, audio_handles) = audio::AudioEngine::new(cli.layers);
        if let Err(e) = audio_engine.start(cli.audio_device.as_deref()) {
            warn!("audio engine failed to start: {e:#}; continuing silently");
        }
        let audio_config = audio_engine.audio_config();

        let composition = Composition::new(&gpu, audio_handles, audio_config, comp_w, comp_h);
        info!(
            "composition: {}x{}, {} layer(s) × {} column(s); audio device: {:?}",
            comp_w,
            comp_h,
            cli.layers,
            cli.columns,
            audio_engine.current_device_name(),
        );

        let egui_ctx = egui::Context::default();
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::viewport::ViewportId::ROOT,
            control_window.as_ref(),
            None,
            None,
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            control_surface.format(),
            None,
            1,
            false,
        );

        let control = ControlWindow {
            id: control_window.id(),
            surface: control_surface,
            egui_ctx,
            egui_winit,
            egui_renderer,
        };
        let output = OutputWindow {
            id: output_window.id(),
            surface: output_surface,
            selected_monitor,
        };

        let library = Library::new(cli.layers, cli.columns);

        // Preview deck has no audio output — it only feeds the Cue pane.
        let preview = Layer::new_silent(&gpu);

        let app_config = config::AppConfig::load();

        let mut state = Self {
            gpu,
            composition_pipelines,
            surface_pipelines,
            composition,
            control,
            output,
            library,
            // Default to layer 0 so the inspector has something useful to
            // show on launch instead of the placeholder.
            selected_layer: Some(0),
            cued: None,
            preview,
            preview_egui_id: None,
            bound_preview_gen: u64::MAX,
            hovered_cell: None,
            monitors,
            last_tick: Instant::now(),
            composition_egui_id: None,
            bound_composition_gen: u64::MAX,
            audio_engine,
            config: app_config,
        };

        // Decide whether to auto-load a project. CLI args win over the
        // saved last-project so a user invoking `cargo run -- foo.mp4`
        // gets exactly that, not their previous workspace.
        let project_to_load: Option<PathBuf> = if let Some(p) = cli.project.clone() {
            Some(p)
        } else if !cli.no_resume && cli.clips.is_empty() {
            state.config.last_project.clone()
        } else {
            None
        };

        if let Some(path) = project_to_load {
            match project::load_from_path(&path) {
                Ok(project) => {
                    if let Err(e) = state.apply_project(project) {
                        warn!("failed to apply project from {}: {e:#}", path.display());
                    } else {
                        info!("auto-loaded project ← {}", path.display());
                        state.config.remember_project(&path);
                    }
                }
                Err(e) => {
                    warn!("could not auto-load {}: {e:#}", path.display());
                    if state.config.last_project.as_deref() == Some(path.as_path()) {
                        state.config.last_project = None;
                        let _ = state.config.save();
                    }
                }
            }
        } else {
            // CLI clips path: preload into row 0 left-to-right, then
            // activate the first one on layer 0.
            for path in &cli.clips {
                if let Some((row, col)) = state.library.first_empty() {
                    if let Err(e) = state.import_clip(path.clone(), row, col) {
                        error!("failed to import {}: {e:#}", path.display());
                    }
                } else {
                    warn!("library full, skipping {}", path.display());
                    break;
                }
            }
            if let Some(slot) = state.library.cell(0, 0) {
                let path = slot.path.clone();
                let defaults = slot.defaults;
                if let Err(e) = state.composition.layers[0].load(&state.gpu, &path, 0, defaults) {
                    error!("failed to load layer 0: {e:#}");
                }
            }
        }

        Ok(state)
    }

    fn handle_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if window_id == self.control.id {
            let _ = self
                .control
                .egui_winit
                .on_window_event(self.control.surface.window().as_ref(), &event);
        }

        match event {
            WindowEvent::CloseRequested => {
                if window_id == self.control.id {
                    event_loop.exit();
                } else {
                    info!("output window closed; exiting");
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if window_id == self.control.id {
                    self.control.surface.resize(&self.gpu, size.width, size.height);
                } else if window_id == self.output.id {
                    self.output.surface.resize(&self.gpu, size.width, size.height);
                }
            }
            WindowEvent::DroppedFile(path) => {
                self.handle_drop(path);
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => self.handle_key(event_loop, window_id, code),
            WindowEvent::RedrawRequested => {
                let res = if window_id == self.control.id {
                    self.render_control()
                } else if window_id == self.output.id {
                    self.render_output()
                } else {
                    Ok(())
                };
                if let Err(e) = res {
                    error!("render error: {e:#}");
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, code: KeyCode) {
        let on_output = window_id == self.output.id;
        match code {
            KeyCode::Space => {
                let any = self.composition.any_playing();
                self.composition.set_all_playing(!any);
            }
            KeyCode::KeyR => {
                self.composition.restart_all();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                self.take();
            }
            KeyCode::KeyF => self.toggle_output_fullscreen(),
            KeyCode::KeyM => self.cycle_output_monitor(),
            KeyCode::Escape => {
                if on_output && self.is_output_fullscreen() {
                    self.set_output_fullscreen(false);
                } else if !on_output {
                    if self.cued.is_some() {
                        self.clear_cue();
                    } else {
                        event_loop.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_drop(&mut self, path: PathBuf) {
        let target = self
            .hovered_cell
            .filter(|&(r, c)| self.library.cell(r, c).is_none())
            .or_else(|| self.library.first_empty());
        let Some((row, col)) = target else {
            warn!("library full, drop ignored: {}", path.display());
            return;
        };
        if let Err(e) = self.import_clip(path, row, col) {
            error!("import failed: {e:#}");
            return;
        }
        // If the layer we just dropped onto isn't already running a clip,
        // trigger this one immediately. Matches the Resolume "drop on
        // empty slot in empty layer just plays" expectation; if the layer
        // is busy, we silently load to avoid hijacking it.
        let layer_empty = self
            .composition
            .layers
            .get(row)
            .is_some_and(|l| l.is_empty());
        if layer_empty {
            self.trigger(row, col);
        }
    }

    /// Decode a thumbnail and place a clip slot at `(row, col)`. Frees any
    /// previous occupant's egui texture id.
    fn import_clip(&mut self, path: PathBuf, row: usize, col: usize) -> Result<()> {
        let mut slot = ClipSlot::from_path(path.clone());

        match thumbs::extract_thumbnail(&path, thumbs::DEFAULT_W, thumbs::DEFAULT_H) {
            Ok(frame) => {
                let thumb = Thumbnail::from_frame(&self.gpu.device, &self.gpu.queue, &frame);
                let id = self.control.egui_renderer.register_native_texture(
                    &self.gpu.device,
                    thumb.view(),
                    wgpu::FilterMode::Linear,
                );
                slot.thumbnail = Some(thumb);
                slot.thumbnail_id = Some(id);
            }
            Err(e) => warn!("thumbnail for {} failed: {e:#}", path.display()),
        }

        if let Some(prev) = self.library.set(row, col, slot)
            && let Some(id) = prev.thumbnail_id {
                self.control.egui_renderer.free_texture(&id);
            }
        info!("imported [{}, {}] {}", row, col, path.display());
        Ok(())
    }

    /// Park `(row, col)` in the cue. If the cell is filled the clip
    /// starts playing on the preview deck; if it's empty the preview
    /// deck is cleared and the bottom inspector switches to its
    /// "browse for a file" mode. Either way the cue index is set —
    /// the bottom panel uses it as its focus handle.
    fn cue(&mut self, row: usize, col: usize) {
        if self.library.idx(row, col).is_none() {
            return;
        }
        self.cued = Some((row, col));
        if let Some(slot) = self.library.cell(row, col) {
            let path = slot.path.clone();
            let defaults = slot.defaults;
            if let Err(e) = self.preview.load(&self.gpu, &path, col, defaults) {
                error!("preview load failed: {e:#}");
                self.preview.clear();
            }
        } else {
            self.preview.clear();
        }
    }

    /// Stop the preview deck and forget the cue.
    fn clear_cue(&mut self) {
        self.cued = None;
        self.preview.clear();
    }

    /// Send the cued clip (if any) to output and clear the cue.
    fn take(&mut self) {
        if let Some((row, col)) = self.cued.take() {
            self.preview.clear();
            self.trigger(row, col);
        }
    }

    /// Trigger the clip at `(row, col)` on its layer (loads + plays) and
    /// auto-select that layer in the inspector — chances are the user
    /// wants to tweak the layer they just acted on. Per-clip defaults
    /// are applied on entry.
    fn trigger(&mut self, row: usize, col: usize) {
        let Some(slot) = self.library.cell(row, col) else {
            return;
        };
        let Some(layer) = self.composition.layers.get_mut(row) else {
            warn!("trigger: row {row} out of layer range");
            return;
        };
        let path = slot.path.clone();
        let defaults = slot.defaults;
        if let Err(e) = layer.load(&self.gpu, &path, col, defaults) {
            error!("trigger load failed: {e:#}");
            return;
        }
        self.selected_layer = Some(row);
    }

    fn stop_layer(&mut self, row: usize) {
        if let Some(layer) = self.composition.layers.get_mut(row) {
            layer.clear();
        }
    }

    fn refresh_monitors(&mut self) {
        self.monitors = self.control.surface.window().available_monitors().collect();
        if self.output.selected_monitor >= self.monitors.len() {
            self.output.selected_monitor = 0;
        }
    }

    fn is_output_fullscreen(&self) -> bool {
        self.output.surface.window().fullscreen().is_some()
    }

    fn set_output_fullscreen(&mut self, on: bool) {
        let target = if on {
            Some(Fullscreen::Borderless(
                self.monitors.get(self.output.selected_monitor).cloned(),
            ))
        } else {
            None
        };
        self.output.surface.window().set_fullscreen(target);
    }

    fn toggle_output_fullscreen(&mut self) {
        self.set_output_fullscreen(!self.is_output_fullscreen());
    }

    fn set_output_monitor(&mut self, index: usize) {
        if index >= self.monitors.len() {
            return;
        }
        self.output.selected_monitor = index;
        let monitor = self.monitors[index].clone();
        let window = self.output.surface.window();
        if window.fullscreen().is_some() {
            window.set_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
        } else {
            window.set_outer_position(monitor.position());
        }
    }

    fn cycle_output_monitor(&mut self) {
        self.refresh_monitors();
        if self.monitors.is_empty() {
            return;
        }
        let next = (self.output.selected_monitor + 1) % self.monitors.len();
        info!("output monitor → [{next}]");
        self.set_output_monitor(next);
    }

    /// Per-tick: advance every layer's decoder, render the composition
    /// into the offscreen target, and request both windows to redraw.
    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f64().min(MAX_CATCHUP_SECS);
        self.last_tick = now;

        self.composition.tick(&self.gpu, dt);
        // Pump the off-output preview deck so the Cue pane shows live
        // playback of whatever the user shift+clicked.
        self.preview.tick(&self.gpu, dt);

        let mut encoder = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("avengine.composition.encoder"),
        });
        self.composition.render(&self.gpu, &self.composition_pipelines, &mut encoder);
        self.gpu.queue.submit(std::iter::once(encoder.finish()));

        // Refresh egui's view of the composition target if it was reallocated.
        let target_gen = self.composition.target.generation;
        if self.composition_egui_id.is_none() || self.bound_composition_gen != target_gen {
            let view = &self.composition.target.view;
            let id = if let Some(id) = self.composition_egui_id {
                self.control.egui_renderer.update_egui_texture_from_wgpu_texture(
                    &self.gpu.device,
                    view,
                    wgpu::FilterMode::Linear,
                    id,
                );
                id
            } else {
                self.control.egui_renderer.register_native_texture(
                    &self.gpu.device,
                    view,
                    wgpu::FilterMode::Linear,
                )
            };
            self.composition_egui_id = Some(id);
            self.bound_composition_gen = target_gen;
        }

        // Same dance for the preview deck's video texture. Generation
        // bumps every time the deck switches to a clip with different
        // dimensions, so we re-register with egui then.
        let preview_gen = self.preview.video_texture.generation();
        if self.preview_egui_id.is_none() || self.bound_preview_gen != preview_gen {
            let view = self.preview.video_texture.view();
            let id = if let Some(id) = self.preview_egui_id {
                self.control.egui_renderer.update_egui_texture_from_wgpu_texture(
                    &self.gpu.device,
                    view,
                    wgpu::FilterMode::Linear,
                    id,
                );
                id
            } else {
                self.control.egui_renderer.register_native_texture(
                    &self.gpu.device,
                    view,
                    wgpu::FilterMode::Linear,
                )
            };
            self.preview_egui_id = Some(id);
            self.bound_preview_gen = preview_gen;
        }

        self.control.surface.window().request_redraw();
        self.output.surface.window().request_redraw();
        Ok(())
    }

    fn render_output(&mut self) -> Result<()> {
        self.output
            .surface
            .prepare_blit(&self.gpu, &self.surface_pipelines, &self.composition.target);

        let acquired = self
            .output
            .surface
            .acquire(&self.gpu)
            .context("acquire output surface")?;
        let surface_tex = match acquired {
            AcquiredFrame::Ready(t) => t,
            AcquiredFrame::Skip => return Ok(()),
        };
        let view = surface_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("avengine.output.frame"),
        });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("avengine.output.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.output.surface.draw_composition(&mut rpass, &self.surface_pipelines);
        }
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        surface_tex.present();
        Ok(())
    }

    fn render_control(&mut self) -> Result<()> {
        let raw_input = self
            .control
            .egui_winit
            .take_egui_input(self.control.surface.window().as_ref());

        self.control.egui_ctx.begin_pass(raw_input);

        let layer_views: Vec<LayerView<'_>> = self
            .composition
            .layers
            .iter()
            .enumerate()
            .map(|(idx, l)| LayerView {
                index: idx,
                blend_mode: l.blend_mode,
                opacity: l.opacity,
                master: l.master,
                mute: l.mute,
                playing: l.transport.playing,
                looping: l.transport.looping,
                speed: l.transport.speed,
                position: l.transport.position,
                active_col: l.active_col,
                info: l.info,
                active_name: l
                    .active_col
                    .and_then(|c| self.library.cell(idx, c).map(|s| s.name.as_str())),
                audio_gain: l.audio_gain(),
            })
            .collect();
        let composition_playing = self.composition.any_playing();
        // Cue pane shows the live preview deck once it has produced its
        // first real frame; before that we fall back to the static
        // thumbnail so the pane isn't blank between Shift+click and the
        // first decoded preview frame.
        let cue_id_aspect = if self.cued.is_some() && self.bound_preview_gen > 0 {
            self.preview_egui_id.map(|id| {
                let (w, h) = self.preview.video_texture.size();
                let aspect = if h > 0 { w as f32 / h as f32 } else { 16.0 / 9.0 };
                (id, aspect)
            })
        } else {
            self.cued
                .and_then(|(r, c)| self.library.cell(r, c))
                .and_then(|slot| {
                    slot.thumbnail_id
                        .zip(slot.thumbnail.as_ref().map(Thumbnail::aspect_ratio))
                })
        };
        let composition_aspect = {
            let (w, h) = self.composition.target.size;
            if h == 0 { 16.0 / 9.0 } else { w as f32 / h as f32 }
        };
        let audio_devices = self.audio_engine.list_device_names();
        let ui_ctx = UiContext {
            library: &self.library,
            layers: &layer_views,
            cued: self.cued,
            composition_playing,
            output_texture: self.composition_egui_id,
            output_aspect: composition_aspect,
            cue_texture: cue_id_aspect.map(|(id, _)| id),
            cue_aspect: cue_id_aspect.map_or(16.0 / 9.0, |(_, a)| a),
            selected_layer: self
                .selected_layer
                .filter(|i| *i < self.composition.layers.len()),
            monitors: &self.monitors,
            selected_monitor: self.output.selected_monitor,
            output_fullscreen: self.output.surface.window().fullscreen().is_some(),
            audio_devices: &audio_devices,
            current_audio_device: self.audio_engine.current_device_name(),
            master_volume: self.audio_engine.master_volume(),
        };
        let actions = ui::draw_control(&self.control.egui_ctx, ui_ctx);

        let full_output = self.control.egui_ctx.end_pass();
        self.control.egui_winit.handle_platform_output(
            self.control.surface.window().as_ref(),
            full_output.platform_output.clone(),
        );

        // Render: the control window no longer mirrors the composition
        // behind the UI. We just clear to black and let egui paint on
        // top, so panel translucency reads as a flat dark surface
        // instead of a distracting moving image.
        let pixels_per_point = self.control.surface.window().scale_factor() as f32;
        let paint_jobs = self
            .control
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);
        let (sw, sh) = self.control.surface.size();
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [sw, sh],
            pixels_per_point,
        };

        for (id, delta) in &full_output.textures_delta.set {
            self.control
                .egui_renderer
                .update_texture(&self.gpu.device, &self.gpu.queue, *id, delta);
        }

        let acquired = self
            .control
            .surface
            .acquire(&self.gpu)
            .context("acquire control surface")?;
        let surface_tex = match acquired {
            AcquiredFrame::Ready(t) => t,
            AcquiredFrame::Skip => return Ok(()),
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("avengine.control.frame"),
        });
        let extra_cmds = self.control.egui_renderer.update_buffers(
            &self.gpu.device,
            &self.gpu.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("avengine.control.pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.control
                .egui_renderer
                .render(&mut rpass, &paint_jobs, &screen_descriptor);
        }
        let mut cmd_buffers: Vec<wgpu::CommandBuffer> = Vec::with_capacity(1 + extra_cmds.len());
        cmd_buffers.push(encoder.finish());
        cmd_buffers.extend(extra_cmds);
        self.gpu.queue.submit(cmd_buffers);
        surface_tex.present();

        for id in &full_output.textures_delta.free {
            self.control.egui_renderer.free_texture(id);
        }

        self.apply_actions(actions);
        Ok(())
    }

    fn apply_actions(&mut self, actions: UiActions) {
        // Track hover for drag-drop targeting.
        self.hovered_cell = actions.hovered_cell;

        if let Some(idx) = actions.select_layer
            && idx < self.composition.layers.len() {
                self.selected_layer = Some(idx);
            }

        if let Some((r, c)) = actions.trigger_cell {
            // Triggering directly clears any pending cue — the user is
            // choosing immediacy over the staged workflow this round.
            self.clear_cue();
            self.trigger(r, c);
        }
        if let Some((r, c)) = actions.cue_cell {
            self.cue(r, c);
        }
        if actions.take {
            self.take();
        }
        if let Some((r, _)) = actions.stop_layer_at {
            self.stop_layer(r);
        }
        if let Some((i, mute)) = actions.set_layer_mute
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.set_mute(mute);
            }
        if let Some((i, gain)) = actions.set_layer_audio_gain
            && let Some(l) = self.composition.layers.get(i) {
                l.set_audio_gain(gain);
            }
        if let Some((i, master)) = actions.set_layer_master
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.set_master(master);
            }
        if let Some(idx) = actions.clear_layer
            && let Some(l) = self.composition.layers.get_mut(idx) {
                l.clear();
            }
        if let Some(v) = actions.set_master_volume {
            self.audio_engine.set_master_volume(v);
        }
        if let Some(name) = actions.set_audio_device {
            if let Err(e) = self
                .audio_engine
                .switch_device(&mut self.composition.layers, Some(&name))
            {
                error!("audio device switch to {name:?} failed: {e:#}");
            } else {
                info!("audio device → {name:?}");
            }
        }
        if let Some((i, blend)) = actions.set_layer_blend
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.blend_mode = blend;
            }
        if let Some((i, op)) = actions.set_layer_opacity
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.opacity = op;
            }
        if let Some((i, looping)) = actions.set_layer_looping
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.transport.looping = looping;
            }
        if let Some((i, sp)) = actions.set_layer_speed
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.transport.speed = sp;
            }
        if let Some(i) = actions.toggle_layer_play
            && let Some(l) = self.composition.layers.get_mut(i)
                && !l.is_empty() {
                    l.transport.toggle_play();
                }
        if let Some(i) = actions.restart_layer
            && let Some(l) = self.composition.layers.get_mut(i) {
                l.restart();
            }
        if actions.toggle_composition_play {
            let any = self.composition.any_playing();
            self.composition.set_all_playing(!any);
        }
        if actions.restart_composition {
            self.composition.restart_all();
        }
        if let Some(idx) = actions.set_output_monitor {
            self.set_output_monitor(idx);
        }
        if let Some(on) = actions.set_output_fullscreen {
            self.set_output_fullscreen(on);
        }
        if actions.refresh_monitors {
            self.refresh_monitors();
        }

        if let Some((row, col)) = actions.browse_for_cell {
            self.browse_into_cell(row, col);
        }
        if let Some(((r, c), looping)) = actions.set_clip_default_loop
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            slot.defaults.looping = looping;
        }
        if let Some(((r, c), speed)) = actions.set_clip_default_speed
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            slot.defaults.speed = speed;
        }
        if let Some(((r, c), blend)) = actions.set_clip_default_blend
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            slot.defaults.blend = blend;
        }

        if actions.save_project
            && let Err(e) = self.save_project_dialog() {
                error!("project save failed: {e:#}");
            }
        if actions.open_project
            && let Err(e) = self.open_project_dialog() {
                error!("project load failed: {e:#}");
            }
    }

    fn save_project_dialog(&mut self) -> Result<()> {
        // Default the save dialog to the directory of whatever project
        // is currently loaded, so re-saving puts the file next to its
        // siblings instead of in `~`.
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save Sublyve project")
            .add_filter("Sublyve project", &["sublyve.json", "json"])
            .set_file_name("project.sublyve.json");
        if let Some(parent) = self
            .config
            .last_project
            .as_ref()
            .and_then(|p| p.parent())
        {
            dialog = dialog.set_directory(parent);
        }
        let Some(path) = dialog.save_file() else { return Ok(()) };
        let project = self.capture_project();
        project::save_to_path(&project, &path)?;
        info!("project saved → {}", path.display());
        self.config.remember_project(&path);
        Ok(())
    }

    fn open_project_dialog(&mut self) -> Result<()> {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open Sublyve project")
            .add_filter("Sublyve project", &["sublyve.json", "json"])
            .add_filter("All files", &["*"]);
        if let Some(parent) = self
            .config
            .last_project
            .as_ref()
            .and_then(|p| p.parent())
        {
            dialog = dialog.set_directory(parent);
        }
        let Some(path) = dialog.pick_file() else { return Ok(()) };
        let project = project::load_from_path(&path)?;
        self.apply_project(project)?;
        info!("project loaded ← {}", path.display());
        self.config.remember_project(&path);
        Ok(())
    }

    /// Open a native file dialog and import the picked file into
    /// `(row, col)`. Cue stays parked on the same cell, so the bottom
    /// panel naturally switches from "empty / Browse" to "filled /
    /// metadata + defaults" on the next render. Blocks the main loop
    /// for the dialog's lifetime — acceptable since the user won't
    /// browse mid-performance.
    fn browse_into_cell(&mut self, row: usize, col: usize) {
        let picked = rfd::FileDialog::new()
            .set_title(format!("Choose a video for L{row} · C{col}"))
            .add_filter("Video", &["mp4", "mov", "mkv", "webm", "avi", "m4v"])
            .add_filter("All files", &["*"])
            .pick_file();
        let Some(path) = picked else { return };
        if let Err(e) = self.import_clip(path, row, col) {
            error!("import failed: {e:#}");
        }
    }

    /// Reset every layer + drop the entire library, freeing per-clip
    /// egui textures along the way. Used as the first step of
    /// `apply_project` so the load starts from a clean slate.
    fn clear_workspace(&mut self) {
        // Clear the cue + preview deck so we don't leak references to
        // a slot that's about to be discarded.
        self.clear_cue();
        for layer in self.composition.layers.iter_mut() {
            layer.clear();
        }
        let layers = self.library.layers();
        let cols = self.library.columns();
        for r in 0..layers {
            for c in 0..cols {
                if let Some(prev) = self.library.clear(r, c)
                    && let Some(id) = prev.thumbnail_id
                {
                    self.control.egui_renderer.free_texture(&id);
                }
            }
        }
    }

    /// Apply a loaded `project::Project` onto the live `AppState`.
    /// Clears the existing workspace, then replays the saved spec —
    /// importing each cell via `import_clip`, applying per-layer
    /// settings, and reconfiguring output / audio.
    fn apply_project(&mut self, project: project::Project) -> Result<()> {
        self.clear_workspace();

        // Resize the composition target if the project asks for a
        // different size. The output window samples this; nothing
        // else to recreate.
        let want = (project.composition.width, project.composition.height);
        if want != self.composition.target.size {
            self.composition
                .target
                .resize(&self.gpu.device, want.0, want.1);
            info!(
                "composition resized to {}x{} on project load",
                want.0, want.1
            );
        }

        // Output: monitor + fullscreen. Monitor is best-effort by
        // index — if the requested index is out of range now we just
        // keep the current selection.
        if project.output.monitor_index < self.monitors.len() {
            self.set_output_monitor(project.output.monitor_index);
        } else {
            warn!(
                "saved monitor index {} is out of range ({} monitors); keeping current",
                project.output.monitor_index,
                self.monitors.len()
            );
        }
        self.set_output_fullscreen(project.output.fullscreen);

        // Audio: device + master volume. Switch only if the saved
        // device name differs from what's currently active.
        self.audio_engine.set_master_volume(project.audio.master_volume);
        if let Some(name) = project.audio.device_name.as_deref()
            && self.audio_engine.current_device_name() != Some(name)
                && let Err(e) = self
                    .audio_engine
                    .switch_device(&mut self.composition.layers, Some(name))
                {
                    warn!("could not switch to saved audio device {name:?}: {e:#}");
                }

        // Per-layer compositing settings. We apply these *before*
        // importing clips so the right-panel inspector shows the
        // right values immediately. Defaults from each cell are
        // applied on the next trigger.
        for spec in &project.layers {
            let Some(layer) = self.composition.layers.get_mut(spec.index) else {
                warn!(
                    "saved layer index {} is out of range ({} layers); skipping",
                    spec.index,
                    self.composition.layers.len()
                );
                continue;
            };
            layer.blend_mode = spec.blend;
            layer.opacity = spec.opacity;
            layer.set_mute(spec.mute);
            layer.set_audio_gain(spec.audio_gain);
            layer.set_master(spec.master);
        }

        // Library: import every saved cell. Each call decodes a
        // thumbnail and registers it with egui — synchronous, so
        // larger libraries take a moment.
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for cell in &project.library.cells {
            if !cell.path.exists() {
                warn!(
                    "skipping missing clip at L{}/C{}: {}",
                    cell.row,
                    cell.col,
                    cell.path.display()
                );
                skipped += 1;
                continue;
            }
            match self.import_clip(cell.path.clone(), cell.row, cell.col) {
                Ok(()) => {
                    if let Some(slot) = self.library.cell_mut(cell.row, cell.col) {
                        slot.defaults = cell.defaults;
                    }
                    imported += 1;
                }
                Err(e) => {
                    warn!(
                        "failed to import {} into L{}/C{}: {e:#}",
                        cell.path.display(),
                        cell.row,
                        cell.col
                    );
                    skipped += 1;
                }
            }
        }
        info!(
            "project loaded: {imported} cell(s) imported, {skipped} skipped"
        );
        Ok(())
    }

    /// Read-only snapshot of the workspace into a `project::Project`.
    /// Active clips, transport state, and the cue are deliberately not
    /// captured — see the project plan for the "setup, not snapshot"
    /// rationale.
    fn capture_project(&self) -> project::Project {
        let layers = self
            .composition
            .layers
            .iter()
            .enumerate()
            .map(|(idx, l)| project::LayerSpec {
                index: idx,
                blend: l.blend_mode,
                opacity: l.opacity,
                mute: l.mute,
                audio_gain: l.audio_gain(),
                master: l.master,
            })
            .collect();
        project::Project {
            composition: project::CompositionSpec {
                width: self.composition.target.size.0,
                height: self.composition.target.size.1,
            },
            library: project::LibrarySpec {
                layers: self.library.layers(),
                columns: self.library.columns(),
                cells: project::collect_cells(&self.library),
            },
            layers,
            output: project::OutputSpec {
                monitor_index: self.output.selected_monitor,
                fullscreen: self.is_output_fullscreen(),
            },
            audio: project::AudioSpec {
                device_name: self.audio_engine.current_device_name().map(str::to_owned),
                master_volume: self.audio_engine.master_volume(),
            },
        }
    }
}

fn resolve_monitor_index(
    requested: Option<usize>,
    monitors: &[MonitorHandle],
    event_loop: &ActiveEventLoop,
) -> usize {
    if let Some(i) = requested {
        if i < monitors.len() {
            return i;
        }
        warn!(
            "--output-monitor {i} is out of range (have {} monitors); falling back to primary",
            monitors.len()
        );
    }
    let primary = event_loop.primary_monitor();
    monitors
        .iter()
        .position(|m| Some(m) == primary.as_ref())
        .unwrap_or(0)
}
