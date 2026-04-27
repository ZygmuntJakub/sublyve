mod deck;
mod library;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use avengine_compositor::{AcquiredFrame, GpuContext, VideoPipeline, VideoTexture, WindowSurface};
use clap::Parser;
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, WindowAttributes, WindowId};

use deck::Deck;
use library::Library;
use ui::{DeckView, UiActions, UiContext};

/// Cap on wall-clock catch-up after a stall (e.g. window hidden).
const MAX_CATCHUP_SECS: f64 = 0.25;

const CONTROL_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1280, 800);
const OUTPUT_DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1280, 720);

/// Real-time video playback / compositing engine.
///
/// Two windows: a control window with the clip library + transport, and
/// a clean output window for the projected fullscreen video.
#[derive(Parser, Debug)]
#[command(name = "avengine", version)]
struct Cli {
    /// Video files to load into the clip library.
    clips: Vec<PathBuf>,

    /// Index of the monitor for the *output* window. Defaults to primary.
    #[arg(long, value_name = "N")]
    output_monitor: Option<usize>,

    /// Start the output window in borderless fullscreen on its monitor.
    #[arg(long, short)]
    fullscreen: bool,

    /// List available monitors and exit.
    #[arg(long)]
    list_monitors: bool,
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
        if let Some(state) = self.state.as_mut() {
            state.tick();
        }
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
    pipeline: VideoPipeline,
    video: VideoTexture,

    control: ControlWindow,
    output: OutputWindow,

    library: Library,
    deck: Option<Deck>,

    monitors: Vec<MonitorHandle>,

    last_tick: Instant,
    catchup: f64,
}

struct ControlWindow {
    surface: WindowSurface,
    id: WindowId,
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    ui_visible: bool,
}

struct OutputWindow {
    surface: WindowSurface,
    id: WindowId,
    /// Cached selection that drives both startup and the picker in the UI.
    selected_monitor: usize,
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop, cli: &Cli) -> Result<Self> {
        let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
        let selected_monitor = resolve_monitor_index(cli.output_monitor, &monitors, event_loop);
        let target_monitor = monitors.get(selected_monitor).cloned();

        // Build both windows up front. Output gets opened on the chosen
        // monitor (and fullscreened if requested); control stays floating
        // so the user can position it on their laptop.
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

        // Bootstrap GPU: create the instance, the *control* surface, then
        // ask for an adapter compatible with it. Subsequent surfaces will
        // sit on the same adapter automatically.
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
            // Should not happen on a single adapter on macOS / Linux.
            // Both windows share the pipeline, which is per-format.
            warn!(
                "control and output surface formats differ ({:?} vs {:?}); pipeline targets control format",
                control_surface.format(),
                output_surface.format(),
            );
        }

        let pipeline = VideoPipeline::new(&gpu.device, control_surface.format());
        let video = VideoTexture::placeholder(&gpu.device);

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
            ui_visible: true,
        };
        let output = OutputWindow {
            id: output_window.id(),
            surface: output_surface,
            selected_monitor,
        };

        let mut library = Library::new();
        for path in &cli.clips {
            library.add(path.clone());
        }

        let mut state = Self {
            gpu,
            pipeline,
            video,
            control,
            output,
            library,
            deck: None,
            monitors,
            last_tick: Instant::now(),
            catchup: 0.0,
        };

        // Auto-activate the first clip so the user sees something
        // immediately rather than a black output.
        if !state.library.clips.is_empty() {
            state.activate_clip(0);
        }

        info!(
            "ready — {} clip(s), output on monitor [{}]",
            state.library.clips.len(),
            state.output.selected_monitor,
        );
        Ok(state)
    }

    fn handle_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if window_id == self.control.id {
            // Forward to egui first; it returns whether it consumed the
            // event but we still want to react to resize / close / keys.
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
                    // Closing the output window for now also exits — V1
                    // doesn't yet support recreating the output surface.
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
                let idx = self.library.add(path.clone());
                info!("loaded clip [{idx}] {}", path.display());
                if self.library.active.is_none() {
                    self.activate_clip(idx);
                }
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
                if let Some(deck) = self.deck.as_mut() {
                    deck.transport.toggle_play();
                }
            }
            KeyCode::KeyR => {
                if let Some(deck) = self.deck.as_mut() {
                    deck.restart();
                    self.catchup = 0.0;
                }
            }
            KeyCode::KeyF => self.toggle_output_fullscreen(),
            KeyCode::KeyM => self.cycle_output_monitor(),
            KeyCode::KeyH if !on_output => {
                self.control.ui_visible = !self.control.ui_visible;
            }
            KeyCode::Escape => {
                // On the output window, Esc unfullscreens (browser
                // convention). On the control window, Esc quits the app.
                if on_output && self.is_output_fullscreen() {
                    self.set_output_fullscreen(false);
                } else if !on_output {
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    /// Run the per-tick decoder pump. Called from `about_to_wait`, so it
    /// happens once per main-loop iteration regardless of how many windows
    /// will redraw afterwards.
    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f64().min(MAX_CATCHUP_SECS);
        self.last_tick = now;

        if let Some(deck) = self.deck.as_mut()
            && deck.transport.playing {
                self.catchup += dt;
                let period = deck.period_at_speed();
                while self.catchup >= period {
                    self.catchup -= period;
                    match deck.pull_next() {
                        Ok(Some(frame)) => {
                            self.video.upload(&self.gpu.device, &self.gpu.queue, &frame);
                        }
                        Ok(None) => {
                            self.catchup = 0.0;
                            break;
                        }
                        Err(e) => {
                            error!("decode error: {e:#}");
                            deck.transport.playing = false;
                            self.catchup = 0.0;
                            break;
                        }
                    }
                }
            }

        self.control.surface.window().request_redraw();
        self.output.surface.window().request_redraw();
    }

    fn activate_clip(&mut self, index: usize) {
        let Some(slot) = self.library.clips.get(index).cloned() else {
            return;
        };
        match Deck::open(&slot.path) {
            Ok(mut deck) => {
                // Decode + upload one frame so the output is never black.
                if let Ok(Some(frame)) = deck.decoder.next_frame() {
                    deck.transport.position = frame.pts;
                    self.video.upload(&self.gpu.device, &self.gpu.queue, &frame);
                }
                self.library.active = Some(index);
                self.deck = Some(deck);
                self.catchup = 0.0;
                info!("activated [{index}] {}", slot.name);
            }
            Err(e) => {
                error!("failed to open {}: {e:#}", slot.path.display());
            }
        }
    }

    fn remove_clip(&mut self, index: usize) {
        if index >= self.library.clips.len() {
            return;
        }
        let was_active = self.library.is_active(index);
        self.library.clips.remove(index);
        match self.library.active {
            Some(active) if active == index => {
                self.library.active = None;
                self.deck = None;
            }
            Some(active) if active > index => self.library.active = Some(active - 1),
            _ => {}
        }
        if was_active && !self.library.clips.is_empty() {
            let next = index.min(self.library.clips.len() - 1);
            self.activate_clip(next);
        }
    }

    fn refresh_monitors(&mut self) {
        self.monitors = self
            .control
            .surface
            .window()
            .available_monitors()
            .collect();
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

    fn render_output(&mut self) -> Result<()> {
        self.output
            .surface
            .prepare_video(&self.gpu, &self.pipeline, &self.video);

        let acquired = self.output.surface.acquire(&self.gpu).context("acquire output surface")?;
        let surface_tex = match acquired {
            AcquiredFrame::Ready(t) => t,
            AcquiredFrame::Skip => return Ok(()),
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

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
            self.output.surface.draw_video(&mut rpass, &self.pipeline);
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
        let ui_visible = self.control.ui_visible;

        // `begin_pass`/`end_pass` lets us drive egui without a closure,
        // so multiple panels' `.show(...)` callbacks can each capture
        // immutable borrows of our state without fighting the borrow
        // checker for `&mut Deck`.
        self.control.egui_ctx.begin_pass(raw_input);

        let mut actions = UiActions::default();
        if ui_visible {
            let deck_view = self.deck.as_ref().map(|d| DeckView {
                playing: d.transport.playing,
                looping: d.transport.looping,
                speed: d.transport.speed,
                position: d.transport.position,
                info: &d.info,
            });
            let ui_ctx = UiContext {
                library: &self.library,
                deck: deck_view,
                monitors: self.monitors.as_slice(),
                selected_monitor: self.output.selected_monitor,
                output_fullscreen: self.output.surface.window().fullscreen().is_some(),
            };
            actions = ui::draw_control(&self.control.egui_ctx, ui_ctx);
        }

        let full_output = self.control.egui_ctx.end_pass();
        self.control.egui_winit.handle_platform_output(
            self.control.surface.window().as_ref(),
            full_output.platform_output.clone(),
        );

        if ui_visible {
            self.render_control_with_egui(full_output)?;
        } else {
            self.render_control_blank()?;
        }

        self.apply_actions(actions);
        Ok(())
    }

    fn render_control_with_egui(&mut self, full_output: egui::FullOutput) -> Result<()> {
        self.control.surface.prepare_video(&self.gpu, &self.pipeline, &self.video);

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

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
            self.control.surface.draw_video(&mut rpass, &self.pipeline);
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
        Ok(())
    }

    fn render_control_blank(&mut self) -> Result<()> {
        self.control.surface.prepare_video(&self.gpu, &self.pipeline, &self.video);
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
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("avengine.control.blank"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("avengine.control.blank.pass"),
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
            self.control.surface.draw_video(&mut rpass, &self.pipeline);
        }
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        surface_tex.present();
        Ok(())
    }

    fn apply_actions(&mut self, actions: UiActions) {
        if actions.toggle_play
            && let Some(deck) = self.deck.as_mut() {
                deck.transport.toggle_play();
            }
        if actions.restart
            && let Some(deck) = self.deck.as_mut() {
                deck.restart();
                self.catchup = 0.0;
            }
        if let Some(v) = actions.set_looping
            && let Some(deck) = self.deck.as_mut() {
                deck.transport.looping = v;
            }
        if let Some(v) = actions.set_speed
            && let Some(deck) = self.deck.as_mut() {
                deck.transport.speed = v;
            }
        if let Some(idx) = actions.activate_clip {
            self.activate_clip(idx);
        }
        if let Some(idx) = actions.remove_clip {
            self.remove_clip(idx);
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
        if actions.open_files {
            // V1: log a hint. Native file dialogs need `rfd`; drag-and-drop
            // is the supported flow for now.
            info!("file picker not wired yet — drag files onto the control window");
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
