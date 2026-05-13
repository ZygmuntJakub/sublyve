mod audio;
mod bundle;
mod composition;
mod config;
mod decode_worker;
mod layer;
mod library;
mod project;
mod thumb_cache;
mod thumbs;
mod ui;
mod undo;

use std::path::{Path, PathBuf};
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

use avengine_playback::CameraDevice;
use composition::Composition;
use layer::Layer;
use library::{CellSource, ClipSlot, Library};
use ui::{LayerView, UiActions, UiContext};

/// Cap on per-tick wall-clock catch-up after a stall (window hidden, etc.).
const MAX_CATCHUP_SECS: f64 = 0.25;

const CONTROL_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1480, 900);
const OUTPUT_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1280, 720);

const DEFAULT_LAYERS: usize = 4;
const DEFAULT_COLUMNS: usize = 8;
/// Hard limits on composition size. Higher values trash the egui
/// layout (cells get too small to read) and stress the main-thread
/// decode loop; ranges are pickable via the +/- buttons in the
/// left-panel Composition section.
pub const MAX_LAYERS: usize = 16;
pub const MAX_COLUMNS: usize = 32;

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

    /// Path of the project file the user is currently editing. Set
    /// after any successful save (dialog or Cmd+S) or load (CLI,
    /// auto-resume, dialog, recent-files menu). Cleared on a save /
    /// load failure that invalidates the previous path. Drives
    /// Cmd+S's "resave silently vs. prompt" branch.
    current_project_path: Option<PathBuf>,

    /// Which tab the bottom panel currently shows. Auto-switches on
    /// `cue` / `trigger` / `select_layer`; manual tab clicks come in
    /// via `UiActions::set_bottom_tab`.
    bottom_tab: ui::BottomTab,

    /// Which tab the right (settings) panel currently shows. Manual
    /// switching only — config-style tabs.
    right_tab: ui::RightTab,

    /// Live capture devices enumerated at startup and whenever the OS
    /// signals a hotplug. The Camera tab in the bottom panel renders
    /// this list as drag sources; project-load matches saved camera
    /// cells against it.
    cameras: Vec<CameraDevice>,

    /// OS-level hotplug signal. We drain it each tick and re-enumerate
    /// cameras if anything came through.
    camera_hotplug: avengine_playback::CameraHotplugWatcher,

    /// Bounded, session-scoped undo / redo for library edits — every
    /// `library.set` (place / replace), `library.clear`, and per-clip
    /// default change funnels through one of the `record_*` helpers.
    /// Cleared on project load and on structural grid changes
    /// (`remove_layer` / `remove_column`) that would invalidate
    /// stored coordinates. See `undo.rs` for the slot-preserving
    /// design.
    undo_history: undo::History,

    /// True while a "bulk reload" path is running (project load, CLI
    /// bootstrap). `import_clip` / `import_camera` and friends check
    /// this and skip recording undo ops while it's set — the user
    /// shouldn't be able to "undo" individual cells out of a loaded
    /// project.
    suppress_undo: bool,
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

        let composition = Composition::new(
            &gpu,
            audio_handles,
            audio_config,
            comp_w,
            comp_h,
            audio_engine.any_solo_active(),
        );
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
            current_project_path: None,
            bottom_tab: ui::BottomTab::default(),
            right_tab: ui::RightTab::default(),
            cameras: avengine_playback::cameras::list().unwrap_or_else(|e| {
                warn!("camera enumeration failed at startup: {e:#}");
                Vec::new()
            }),
            camera_hotplug: avengine_playback::hotplug::watch(),
            undo_history: undo::History::new(),
            suppress_undo: false,
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
            match load_project_any(&path) {
                Ok(project) => {
                    if let Err(e) = state.apply_project(project) {
                        warn!("failed to apply project from {}: {e:#}", path.display());
                    } else {
                        info!("auto-loaded project ← {}", path.display());
                        state.config.remember_project(&path);
                        state.current_project_path = Some(path);
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
            // activate the first one on layer 0. This is initial
            // setup — not user edits — so don't pollute the undo
            // stack with the bootstrap imports.
            state.suppress_undo = true;
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
            state.suppress_undo = false;
            // Trigger the freshly-imported clip on layer 0 (CLI clips
            // can only be files, so File-source dispatch only).
            if let Some(slot) = state.library.cell(0, 0)
                && let CellSource::File { path } = &slot.source
            {
                let path = path.clone();
                let defaults = slot.defaults;
                if let Err(e) =
                    state.composition.layers[0].load(&state.gpu, &path, 0, defaults)
                {
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

    /// Decode a thumbnail and place a clip slot at `(row, col)`. The
    /// previous occupant (if any) is handed to the undo history along
    /// with the recorded `Place` / `Replace` op — its texture won't
    /// be freed until that op falls off the cap or the redo tail is
    /// truncated.
    fn import_clip(&mut self, path: PathBuf, row: usize, col: usize) -> Result<()> {
        let mut slot = ClipSlot::from_path(path.clone());

        match thumb_cache::load_or_decode(&path, thumbs::DEFAULT_W, thumbs::DEFAULT_H) {
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

        let displaced = self.library.set(row, col, slot);
        self.record_place(row, col, displaced);
        info!("imported [{}, {}] {}", row, col, path.display());
        Ok(())
    }

    /// Place a camera-source slot at `(row, col)`. No thumbnail
    /// extraction (would require opening the device); the cell renders
    /// a glyph + display_name instead.
    fn import_camera(
        &mut self,
        format_name: String,
        device: String,
        display_name: String,
        has_audio: bool,
        row: usize,
        col: usize,
    ) -> Result<()> {
        let slot = ClipSlot::from_camera(
            format_name.clone(),
            device.clone(),
            display_name.clone(),
            has_audio,
        );
        let displaced = self.library.set(row, col, slot);
        self.record_place(row, col, displaced);
        info!(
            "imported camera [{}, {}] {} ({}, audio={})",
            row, col, display_name, device, has_audio,
        );
        Ok(())
    }

    /// Re-enumerate camera devices. Called from `tick()` whenever the
    /// OS hotplug watcher reports a connect/disconnect, and at startup
    /// via `AppState::new`.
    fn refresh_cameras(&mut self) {
        match avengine_playback::cameras::list() {
            Ok(list) => {
                info!("camera refresh: {} device(s)", list.len());
                self.cameras = list;
            }
            Err(e) => warn!("camera refresh failed: {e:#}"),
        }
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
        // Auto-switch the bottom panel to the Clip tab so the user
        // sees the inspector / browse for the cell they just cued.
        self.bottom_tab = ui::BottomTab::Clip;
        if let Some(slot) = self.library.cell(row, col) {
            let defaults = slot.defaults;
            let load_result = match &slot.source {
                CellSource::File { path } => {
                    let path = path.clone();
                    self.preview.load(&self.gpu, &path, col, defaults)
                }
                CellSource::Camera { format_name, device, has_audio, .. } => {
                    let format_name = format_name.clone();
                    let device = device.clone();
                    let has_audio = *has_audio;
                    self.preview.load_camera(
                        &self.gpu,
                        &format_name,
                        &device,
                        has_audio,
                        col,
                        defaults,
                    )
                }
            };
            if let Err(e) = load_result {
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
        let defaults = slot.defaults;
        let load_result = match &slot.source {
            CellSource::File { path } => {
                let path = path.clone();
                layer.load(&self.gpu, &path, col, defaults)
            }
            CellSource::Camera { format_name, device, has_audio, .. } => {
                let format_name = format_name.clone();
                let device = device.clone();
                let has_audio = *has_audio;
                layer.load_camera(
                    &self.gpu,
                    &format_name,
                    &device,
                    has_audio,
                    col,
                    defaults,
                )
            }
        };
        if let Err(e) = load_result {
            error!("trigger load failed: {e:#}");
            return;
        }
        self.selected_layer = Some(row);
        // The user just acted on a layer — show its inspector.
        self.bottom_tab = ui::BottomTab::Layer;
        // Empty→non-empty transition: a soloed-but-empty layer
        // becomes "actually soloed" the moment a clip lands on it.
        // recompute_solo only counts non-empty soloed layers.
        self.composition.recompute_solo();
    }

    fn stop_layer(&mut self, row: usize) {
        if let Some(layer) = self.composition.layers.get_mut(row) {
            layer.clear();
        }
        // Non-empty→empty transition: if the just-cleared layer was
        // the only soloed layer with a clip, the aggregate flips off
        // and the rest of the composition becomes audible/visible.
        self.composition.recompute_solo();
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

        // OS-driven hotplug: if a camera was plugged or unplugged
        // since the last frame, re-enumerate. Cheap when nothing
        // changed (a non-blocking try_recv on an empty channel).
        if self.camera_hotplug.changed() {
            self.refresh_cameras();
        }

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
                solo: l.is_solo(),
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
                is_live: l.is_live,
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
        // Stale-entry pruning runs only when the Open Recent submenu
        // is actually open — see `UiActions::prune_recents`. Keeping
        // it off the per-frame path avoids stat'ing every entry on
        // every tick (the syscalls can block on network mounts or
        // sleeping drives).
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
            max_layers: MAX_LAYERS,
            max_columns: MAX_COLUMNS,
            bottom_tab: self.bottom_tab,
            right_tab: self.right_tab,
            cameras: &self.cameras,
            recent_projects: &self.config.recent_projects,
            current_project_path: self.current_project_path.as_ref(),
            can_undo: self.undo_history.can_undo(),
            can_redo: self.undo_history.can_redo(),
            undo_label: self.undo_history.peek_undo(),
            redo_label: self.undo_history.peek_redo(),
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
                // Pick a row label → focus the layer inspector.
                self.bottom_tab = ui::BottomTab::Layer;
            }
        if let Some(t) = actions.set_bottom_tab {
            self.bottom_tab = t;
        }
        if let Some(t) = actions.set_right_tab {
            self.right_tab = t;
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
                // Muting (or unmuting) a soloed layer changes whether
                // the composition has any *audible* soloed layer — if
                // the only soloed layer just went silent we must
                // release the aggregate so the rest of the composition
                // returns.
                self.composition.recompute_solo();
            }
        if let Some((i, solo)) = actions.set_layer_solo {
            self.composition.set_layer_solo(i, solo);
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
                self.composition.recompute_solo();
            }
        if let Some((idx, secs)) = actions.seek_layer
            && let Some(l) = self.composition.layers.get_mut(idx) {
                l.seek(&self.gpu, secs);
            }

        if actions.add_layer {
            self.add_layer();
        }
        if actions.remove_layer {
            self.remove_layer();
        }
        if actions.add_column {
            self.add_column();
        }
        if actions.remove_column {
            self.remove_column();
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
                l.set_looping(looping);
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
        if let Some((row, col, format_name, device, display_name, has_audio)) =
            actions.bind_camera_to_cell
        {
            if let Err(e) = self.import_camera(
                format_name,
                device,
                display_name,
                has_audio,
                row,
                col,
            ) {
                error!("camera bind failed: {e:#}");
            } else if self
                .composition
                .layers
                .get(row)
                .is_some_and(|l| l.is_empty())
            {
                // Mirror the file-drag-drop "auto-trigger on empty
                // layer" UX so dropping a camera onto an idle row
                // immediately starts streaming it.
                self.trigger(row, col);
            }
        }
        if let Some(((r, c), looping)) = actions.set_clip_default_loop
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            let before = slot.defaults;
            slot.defaults.looping = looping;
            let after = slot.defaults;
            if before != after {
                self.record_defaults(r, c, before, after);
            }
        }
        if let Some(((r, c), speed)) = actions.set_clip_default_speed
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            let before = slot.defaults;
            slot.defaults.speed = speed;
            let after = slot.defaults;
            if before != after {
                self.record_defaults(r, c, before, after);
            }
        }
        if let Some(((r, c), blend)) = actions.set_clip_default_blend
            && let Some(slot) = self.library.cell_mut(r, c)
        {
            let before = slot.defaults;
            slot.defaults.blend = blend;
            let after = slot.defaults;
            if before != after {
                self.record_defaults(r, c, before, after);
            }
        }

        if actions.undo {
            self.undo();
        }
        if actions.redo {
            self.redo();
        }

        if actions.save_project
            && let Err(e) = self.save_project() {
                error!("project save failed: {e:#}");
            }
        if actions.save_project_as
            && let Err(e) = self.save_project_as_dialog() {
                error!("project save-as failed: {e:#}");
            }
        if actions.open_project
            && let Err(e) = self.open_project_dialog() {
                error!("project load failed: {e:#}");
            }
        if let Some(path) = actions.open_recent_project.clone()
            && let Err(e) = self.load_project_from_path(&path)
        {
            // `load_project_from_path` has already forgotten the
            // path if the error was a load/parse failure (stale
            // entry). `apply_project` failures leave the entry in
            // place since they're likely transient — the user can
            // retry from the same Recent menu.
            error!("could not open recent project {}: {e:#}", path.display());
        }
        if actions.clear_recent_projects {
            self.config.clear_recent_projects();
        }
        if actions.prune_recents {
            // Submenu is open — refresh the on-disk view of each
            // path. Bounded by `MAX_RECENT_PROJECTS` stat syscalls
            // and only runs while the user is actually inspecting
            // the list, so it can't block the render loop on a
            // closed menu.
            self.config.prune_missing_recents();
        }
    }

    /// Cmd+S / "Save" menu entry. Resaves to the existing project
    /// path if one is known, otherwise falls through to the save-as
    /// dialog so a fresh session's first save still picks a location.
    fn save_project(&mut self) -> Result<()> {
        let Some(path) = self.current_project_path.clone() else {
            return self.save_project_as_dialog();
        };
        let project = self.capture_project();
        write_project(&project, &path)?;
        info!("project saved → {}", path.display());
        self.config.remember_project(&path);
        Ok(())
    }

    /// Cmd+Shift+S / "Save As…" menu entry / `💾 Save…` button — always
    /// prompts. On success, the chosen path becomes the new
    /// `current_project_path`.
    fn save_project_as_dialog(&mut self) -> Result<()> {
        // Default the save dialog to the directory of whatever project
        // is currently loaded, so re-saving puts the file next to its
        // siblings instead of in `~`. `.sublyve` is the preferred
        // bundle format (zip including clip files); `.sublyve.json` is
        // the legacy loose JSON that just references absolute paths on
        // disk.
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save Sublyve project")
            .add_filter("Sublyve bundle", &["sublyve"])
            .add_filter("Sublyve project (JSON only)", &["sublyve.json", "json"])
            .set_file_name("project.sublyve");
        if let Some(parent) = self
            .current_project_path
            .as_ref()
            .or(self.config.last_project.as_ref())
            .and_then(|p| p.parent())
        {
            dialog = dialog.set_directory(parent);
        }
        let Some(path) = dialog.save_file() else { return Ok(()) };
        let project = self.capture_project();
        write_project(&project, &path)?;
        info!("project saved → {}", path.display());
        self.config.remember_project(&path);
        self.current_project_path = Some(path);
        Ok(())
    }

    fn open_project_dialog(&mut self) -> Result<()> {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open Sublyve project")
            .add_filter("Sublyve project", &["sublyve", "sublyve.json", "json"])
            .add_filter("All files", &["*"]);
        if let Some(parent) = self
            .current_project_path
            .as_ref()
            .or(self.config.last_project.as_ref())
            .and_then(|p| p.parent())
        {
            dialog = dialog.set_directory(parent);
        }
        let Some(path) = dialog.pick_file() else { return Ok(()) };
        self.load_project_from_path(&path)
    }

    /// Shared body for "Open Recent ▸ X" and the open dialog: load
    /// the project (or bundle), apply it, and record it as the
    /// current path.
    ///
    /// On a `load_project_any` failure (file gone / parse error /
    /// bundle corrupt) we proactively forget the path so a stale
    /// Recent entry stops reappearing. On `apply_project` failures
    /// we deliberately do *not* — those are likely transient
    /// (a future fallible rebuild step, GPU hiccup, …) and yanking
    /// the recent entry would punish the user for a problem they
    /// didn't cause.
    fn load_project_from_path(&mut self, path: &Path) -> Result<()> {
        let project = match load_project_any(path) {
            Ok(p) => p,
            Err(e) => {
                self.config.forget_project(path);
                return Err(e);
            }
        };
        self.apply_project(project)?;
        info!("project loaded ← {}", path.display());
        self.config.remember_project(path);
        self.current_project_path = Some(path.to_path_buf());
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

    /// Append a new layer to the composition + library, then rebuild
    /// the audio stream so the new layer gets a producer handle.
    /// Refuses past `MAX_LAYERS` (the +/- buttons are also
    /// `add_enabled`-gated, but we keep the guard here for safety).
    fn add_layer(&mut self) {
        if self.composition.layers.len() >= MAX_LAYERS {
            return;
        }
        let cfg = self.audio_engine.audio_config();
        self.composition
            .layers
            .push(Layer::new_pending_audio(&self.gpu, cfg));
        self.library.add_layer(MAX_LAYERS);
        self.rebuild_audio_for_current_device();
        // New layer is solo=false; aggregate is unchanged but recompute
        // is cheap and keeps the contract uniform across mutation sites.
        self.composition.recompute_solo();
    }

    /// Drop the highest-indexed layer (= the row visually at the top
    /// of the grid). Frees the dropped library row's egui textures,
    /// resets `selected_layer` / `cued` / `hovered_cell` if they
    /// referenced that row, and rebuilds the audio stream so the
    /// remaining layers each get a fresh producer handle.
    fn remove_layer(&mut self) {
        if self.composition.layers.len() <= 1 {
            return;
        }
        let removed_idx = self.composition.layers.len() - 1;

        // Drop the audio + decoder for the removed layer first.
        self.composition.layers.pop();

        // Drop the library row + free its egui textures.
        for slot in self.library.remove_layer().into_iter().flatten() {
            if let Some(id) = slot.thumbnail_id {
                self.control.egui_renderer.free_texture(&id);
            }
        }

        // Dropping a row shifts no coordinates (we always drop the
        // highest-indexed row), but any history op pointing at a
        // cell in the dropped row would now be a no-op on undo and
        // — worse — would resurrect a slot whose layer no longer
        // exists. Wipe the stack; it's the safest path. Defaults
        // ops + ops on cells in surviving rows are wiped along with
        // the rest, but that's the cost of a structural edit.
        let ids = self.undo_history.clear();
        for id in ids {
            self.control.egui_renderer.free_texture(&id);
        }

        // Repair UI / state references to the dropped row.
        if self.selected_layer == Some(removed_idx) {
            self.selected_layer = if removed_idx > 0 {
                Some(removed_idx - 1)
            } else {
                None
            };
        }
        if let Some((r, _)) = self.cued
            && r >= self.composition.layers.len()
        {
            self.clear_cue();
        }
        if let Some((r, _)) = self.hovered_cell
            && r >= self.composition.layers.len()
        {
            self.hovered_cell = None;
        }

        self.rebuild_audio_for_current_device();
        // Removed layer might have been the only soloed one; recompute
        // so non-soloed layers come back if appropriate.
        self.composition.recompute_solo();
    }

    /// Append an empty column on the right of the grid. No audio
    /// involvement — columns only exist in the library.
    fn add_column(&mut self) {
        self.library.add_column(MAX_COLUMNS);
    }

    /// Drop the rightmost column. Frees egui textures of any clips
    /// that lived in it, clears the active clip on any layer that
    /// was playing from that column, and clears the cue if it was
    /// pointing into that column.
    fn remove_column(&mut self) {
        if self.library.columns() <= 1 {
            return;
        }
        let dropped_col = self.library.columns() - 1;
        for slot in self.library.remove_column().into_iter().flatten() {
            if let Some(id) = slot.thumbnail_id {
                self.control.egui_renderer.free_texture(&id);
            }
        }
        // Layers that were playing from the now-gone column have a
        // dangling `active_col` pointing at it; clear them so the
        // grid's `▶` badge doesn't render off-grid.
        for layer in self.composition.layers.iter_mut() {
            if layer.active_col == Some(dropped_col) {
                layer.clear();
            }
        }
        // Recompute aggregate after the bulk clear — any of those
        // layers might have been the only soloed one.
        self.composition.recompute_solo();
        if let Some((_, c)) = self.cued
            && c >= self.library.columns()
        {
            self.clear_cue();
        }
        if let Some((_, c)) = self.hovered_cell
            && c >= self.library.columns()
        {
            self.hovered_cell = None;
        }

        // Same rationale as `remove_layer`: dropped column invalidates
        // any history op pointing at it. Wipe the stack.
        let ids = self.undo_history.clear();
        for id in ids {
            self.control.egui_renderer.free_texture(&id);
        }
    }

    /// Tear down + rebuild the audio stream on whatever device is
    /// currently active. Used after layer-add / layer-remove since
    /// those change the consumer count baked into the cpal callback.
    fn rebuild_audio_for_current_device(&mut self) {
        let device = self.audio_engine.current_device_name().map(str::to_owned);
        if let Err(e) = self
            .audio_engine
            .switch_device(&mut self.composition.layers, device.as_deref())
        {
            warn!("audio rebuild after layer count change failed: {e:#}");
        }
    }

    /// Record a `Place` / `Replace` op for a `library.set` that just
    /// ran, freeing any textures the history drops (cap overflow or
    /// truncated redo tail). No-op when `suppress_undo` is set
    /// (bulk reload paths) — in that mode the `displaced` slot is
    /// freed inline instead, since it isn't going into history.
    fn record_place(&mut self, row: usize, col: usize, displaced: Option<ClipSlot>) {
        if self.suppress_undo {
            if let Some(prev) = displaced
                && let Some(id) = prev.thumbnail_id
            {
                self.control.egui_renderer.free_texture(&id);
            }
            return;
        }
        let freed = self.undo_history.record_place(row, col, displaced);
        for id in freed {
            self.control.egui_renderer.free_texture(&id);
        }
    }

    /// Record a `Clear` op for a `library.clear` that just returned
    /// `Some(slot)`, freeing any textures the history drops. No-op
    /// when `suppress_undo` is set.
    ///
    /// Currently unused at runtime: the only existing call site that
    /// removes single cells is `clear_workspace`, which suppresses
    /// undo as part of a bulk reload. Kept ready for when a per-cell
    /// "remove this clip from the library" UI action is added.
    #[allow(dead_code)]
    fn record_clear(&mut self, row: usize, col: usize, removed: ClipSlot) {
        if self.suppress_undo {
            if let Some(id) = removed.thumbnail_id {
                self.control.egui_renderer.free_texture(&id);
            }
            return;
        }
        let freed = self.undo_history.record_clear(row, col, removed);
        for id in freed {
            self.control.egui_renderer.free_texture(&id);
        }
    }

    /// Record a per-clip defaults change. The chokepoint coalesces
    /// consecutive same-cell edits within a short window, so a
    /// slider drag through 100 values lands as one undo op.
    fn record_defaults(
        &mut self,
        row: usize,
        col: usize,
        before: library::ClipDefaults,
        after: library::ClipDefaults,
    ) {
        if self.suppress_undo {
            return;
        }
        // Defaults ops never own a texture, so the freed list is
        // always empty — but keep the same shape as the cell paths
        // in case a future variant changes that.
        let freed = self.undo_history.record_defaults(row, col, before, after);
        for id in freed {
            self.control.egui_renderer.free_texture(&id);
        }
    }

    /// Apply one undo step, if any.
    ///
    /// `apply_step` is a swap — the slot in history moves into the
    /// library and whatever was in the library moves back into history.
    /// No textures are freed here; that's the whole point of slot-
    /// preservation. Texture freeing only happens when an op falls off
    /// the cap (via `record_*`) or when the redo tail is truncated
    /// (via `record_op` after an undo).
    ///
    /// An undo can re-install a slot that was holding the active clip
    /// on a layer playing from `(row, col)`. The layer's decoder still
    /// references the OLD slot, not the newly-installed one, which is
    /// fine for files (the layer owns its own decoder) but means the
    /// visible "active clip" badge can mismatch the cell. We don't try
    /// to chase the layer state — undo is a *library* operation, not a
    /// transport operation, per the design notes. The user can
    /// re-trigger if they want the layer to play the restored clip.
    fn undo(&mut self) {
        let Some(label) = self.undo_history.peek_undo() else {
            info!("nothing to undo");
            return;
        };
        if self.undo_history.undo(&mut self.library).is_some() {
            info!("undo: {label}");
        }
    }

    /// Apply one redo step, if any.
    ///
    /// Like `undo`, this is a swap inside the history — no textures
    /// are freed here. Texture freeing only happens at the history
    /// edges (cap eviction, redo-tail truncation, or `History::clear`).
    fn redo(&mut self) {
        let Some(label) = self.undo_history.peek_redo() else {
            info!("nothing to redo");
            return;
        };
        if self.undo_history.redo(&mut self.library).is_some() {
            info!("redo: {label}");
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
        // The undo stack is *session* state — it should not survive
        // a project load. Drop every stored op and free the textures
        // they were holding onto.
        let ids = self.undo_history.clear();
        for id in ids {
            self.control.egui_renderer.free_texture(&id);
        }
    }

    /// Apply a loaded `project::Project` onto the live `AppState`.
    /// Clears the existing workspace, then replays the saved spec —
    /// importing each cell via `import_clip`, applying per-layer
    /// settings, and reconfiguring output / audio.
    fn apply_project(&mut self, project: project::Project) -> Result<()> {
        // Project apply is a bulk reload, not user-initiated cell
        // edits — suppress undo recording for its duration so the
        // history doesn't fill with one op per imported cell. The
        // history was already drained by `clear_workspace` below.
        self.suppress_undo = true;
        let result = self.apply_project_inner(project);
        self.suppress_undo = false;
        result
    }

    fn apply_project_inner(&mut self, project: project::Project) -> Result<()> {
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

        // Resize layer / column count to match the saved project.
        // Layer changes rebuild the audio stream once after the loop;
        // column changes are library-only.
        let target_layers = project.library.layers.clamp(1, MAX_LAYERS);
        while self.composition.layers.len() < target_layers {
            let cfg = self.audio_engine.audio_config();
            self.composition
                .layers
                .push(Layer::new_pending_audio(&self.gpu, cfg));
            self.library.add_layer(MAX_LAYERS);
        }
        while self.composition.layers.len() > target_layers {
            self.composition.layers.pop();
            for slot in self.library.remove_layer().into_iter().flatten() {
                if let Some(id) = slot.thumbnail_id {
                    self.control.egui_renderer.free_texture(&id);
                }
            }
        }
        let target_columns = project.library.columns.clamp(1, MAX_COLUMNS);
        while self.library.columns() < target_columns {
            self.library.add_column(MAX_COLUMNS);
        }
        while self.library.columns() > target_columns {
            for slot in self.library.remove_column().into_iter().flatten() {
                if let Some(id) = slot.thumbnail_id {
                    self.control.egui_renderer.free_texture(&id);
                }
            }
        }
        // Audio stream rebuild after every layer-count change.
        self.rebuild_audio_for_current_device();

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
            layer.set_solo(spec.solo);
            layer.set_audio_gain(spec.audio_gain);
            layer.set_master(spec.master);
        }
        // Refresh the shared `any_solo_active` aggregate after the
        // batch of per-layer settings. Note: every layer is still
        // empty at this point (clips are imported in the next loop),
        // so the empty-layer rule will compute `any_solo_active=false`
        // even when specs have `solo=true`. That's intentional —
        // `trigger()` re-fires `recompute_solo` once a clip lands on a
        // soloed layer, at which point the aggregate flips on.
        self.composition.recompute_solo();

        // Library: import every saved cell. Each call decodes a
        // thumbnail and registers it with egui — synchronous, so
        // larger libraries take a moment.
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for cell in &project.library.cells {
            let import_result = match &cell.source {
                project::CellSpecSource::File { path } => {
                    if !path.exists() {
                        warn!(
                            "skipping missing clip at L{}/C{}: {}",
                            cell.row,
                            cell.col,
                            path.display()
                        );
                        skipped += 1;
                        continue;
                    }
                    self.import_clip(path.clone(), cell.row, cell.col)
                }
                project::CellSpecSource::Camera {
                    format_name,
                    device,
                    display_name,
                    has_audio,
                } => {
                    // Best-effort match against currently-enumerated
                    // devices. We accept either an exact `device` URL
                    // match or a fuzzy `display_name` match — devices
                    // can change between sessions. Use the freshest
                    // `has_audio` from current enumeration when we
                    // find a match (the camera may have gained or
                    // lost a paired mic since the project was saved);
                    // fall back to the saved value otherwise.
                    let matched = self.cameras.iter().find(|c| {
                        c.format_name == *format_name
                            && (c.device == *device || c.display_name == *display_name)
                    });
                    let Some(matched) = matched else {
                        warn!(
                            "skipping unavailable camera at L{}/C{}: {}",
                            cell.row, cell.col, display_name,
                        );
                        skipped += 1;
                        continue;
                    };
                    let live_has_audio = matched.has_audio || *has_audio;
                    self.import_camera(
                        format_name.clone(),
                        device.clone(),
                        display_name.clone(),
                        live_has_audio,
                        cell.row,
                        cell.col,
                    )
                }
            };
            match import_result {
                Ok(()) => {
                    if let Some(slot) = self.library.cell_mut(cell.row, cell.col) {
                        slot.defaults = cell.defaults;
                    }
                    imported += 1;
                }
                Err(e) => {
                    warn!(
                        "failed to import L{}/C{}: {e:#}",
                        cell.row, cell.col
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
                solo: l.is_solo(),
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

/// Load a project from either a `.sublyve` bundle (zip) or a loose
/// `.sublyve.json`, picking the path based on file extension. Bundles
/// extract into a per-bundle cache dir on first load.
fn load_project_any(path: &Path) -> Result<project::Project> {
    if bundle::is_bundle_path(path) {
        bundle::load_from_path(path)
    } else {
        project::load_from_path(path)
    }
}

/// Symmetric to `load_project_any`: dispatch the save by extension so
/// a Cmd+S resave to `scene.sublyve` writes the zip bundle, not loose
/// JSON wearing a `.sublyve` extension (which would be unloadable on
/// reopen). Both `save_project` and `save_project_as_dialog` go
/// through here so the two can't drift.
fn write_project(project: &project::Project, path: &Path) -> Result<()> {
    if bundle::is_bundle_path(path) {
        bundle::save_to_path(project, path)
    } else {
        project::save_atomic(project, path)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_project() -> project::Project {
        project::Project {
            composition: project::CompositionSpec { width: 1280, height: 720 },
            library: project::LibrarySpec { layers: 1, columns: 1, cells: vec![] },
            layers: vec![],
            output: project::OutputSpec { monitor_index: 0, fullscreen: false },
            audio: project::AudioSpec { device_name: None, master_volume: 1.0 },
        }
    }

    /// Regression guard for the merge between `recent-files-cmd-s` and
    /// `feat/sublyve-bundle`: `save_project` (Cmd+S) used to call
    /// `project::save_atomic` unconditionally, which silently clobbered
    /// `.sublyve` bundles with loose JSON. Both save sites now go
    /// through `write_project`; this test pins that contract.
    #[test]
    fn write_project_dispatches_by_extension() {
        let dir = std::env::temp_dir().join(format!(
            "sublyve-write-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");

        let project = empty_project();

        let bundle_path = dir.join("scene.sublyve");
        write_project(&project, &bundle_path).expect("write bundle");
        let bytes = std::fs::read(&bundle_path).expect("read bundle");
        assert_eq!(
            &bytes[..4],
            b"PK\x03\x04",
            "expected zip magic for .sublyve, got non-bundle (Cmd+S resave regression?)"
        );

        let json_path = dir.join("scene.sublyve.json");
        write_project(&project, &json_path).expect("write json");
        let bytes = std::fs::read(&json_path).expect("read json");
        assert!(
            bytes.starts_with(b"{"),
            "expected JSON for .sublyve.json"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

