use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use avengine_compositor::Engine;
use avengine_compositor::engine::AcquiredFrame;
use avengine_playback::{Decoder, StreamInfo, Transport};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{WindowAttributes, WindowId};

/// Cap on wall-clock catch-up after a stall (e.g. window hidden).
/// Without this, an unfocused app accumulates `dt` and then bursts a
/// huge backlog of decodes the moment it resumes.
const MAX_CATCHUP_SECS: f64 = 0.25;

fn main() -> Result<()> {
    init_tracing();

    let path = std::env::args().nth(1).map(PathBuf::from).ok_or_else(|| {
        anyhow!("usage: avengine <video-file>")
    })?;

    let event_loop = EventLoop::new().context("creating winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App { state: None, video_path: path };
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
    video_path: PathBuf,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match AppState::new(event_loop, &self.video_path) {
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
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(state) = self.state.as_mut() {
            state.handle_window_event(event_loop, event);
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            state.engine.window().request_redraw();
        }
    }
}

struct AppState {
    engine: Engine,
    decoder: Decoder,
    stream: StreamInfo,
    transport: Transport,
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,

    last_tick: Instant,
    /// Wall-clock seconds owed to the decoder. Decremented by one
    /// `frame_period` each time we successfully pull a frame.
    catchup: f64,
    frame_period: f64,
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop, path: &std::path::Path) -> Result<Self> {
        let window = event_loop
            .create_window(WindowAttributes::default().with_title("avengine"))
            .context("creating window")?;

        let mut decoder =
            Decoder::open(path).with_context(|| format!("opening {}", path.display()))?;
        let stream = decoder.info();
        let frame_period = 1.0 / stream.frame_rate;

        let mut engine = pollster::block_on(Engine::new(window))?;
        let mut transport = Transport::new();

        // Show the first frame immediately so the window isn't black on launch.
        if let Some(frame) = decoder.next_frame()? {
            transport.position = frame.pts;
            engine.upload_frame(&frame);
        }

        let egui_ctx = egui::Context::default();
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::viewport::ViewportId::ROOT,
            engine.window(),
            None,
            None,
            None,
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(engine.device(), engine.surface_format(), None, 1, false);

        info!(
            "loaded {}x{} @ {:.2}fps, {:.1}s",
            stream.width, stream.height, stream.frame_rate, stream.duration
        );

        Ok(Self {
            engine,
            decoder,
            stream,
            transport,
            egui_ctx,
            egui_winit,
            egui_renderer,
            last_tick: Instant::now(),
            catchup: 0.0,
            frame_period,
        })
    }

    fn handle_window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        let _consumed = self.egui_winit.on_window_event(self.engine.window(), &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.engine.resize(size.width, size.height),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => match code {
                KeyCode::Space => self.transport.toggle_play(),
                KeyCode::KeyR => self.restart(),
                KeyCode::Escape => event_loop.exit(),
                _ => {}
            },
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.render() {
                    error!("render error: {e:#}");
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn restart(&mut self) {
        if let Err(e) = self.decoder.seek(0.0) {
            warn!("seek failed: {e}");
        }
        self.transport.position = 0.0;
        self.transport.playing = true;
        self.catchup = 0.0;
    }

    fn pump_frames(&mut self) {
        if !self.transport.playing {
            return;
        }
        let period = self.frame_period / self.transport.speed.max(1e-3);
        while self.catchup >= period {
            self.catchup -= period;
            match self.decoder.next_frame() {
                Ok(Some(frame)) => {
                    self.transport.position = frame.pts;
                    self.engine.upload_frame(&frame);
                }
                Ok(None) => {
                    if self.transport.looping {
                        if let Err(e) = self.decoder.seek(0.0) {
                            warn!("seek-to-loop failed: {e}");
                            self.transport.playing = false;
                        }
                        self.transport.position = 0.0;
                    } else {
                        self.transport.playing = false;
                    }
                    self.catchup = 0.0;
                    break;
                }
                Err(e) => {
                    error!("decode error: {e}");
                    self.transport.playing = false;
                    break;
                }
            }
        }
    }

    fn render(&mut self) -> Result<()> {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f64().min(MAX_CATCHUP_SECS);
        self.last_tick = now;
        self.catchup += dt;
        self.pump_frames();

        let raw_input = self.egui_winit.take_egui_input(self.engine.window());
        // `egui::Context` is internally Arc-shared; cloning the handle
        // sidesteps the double-borrow of `self` inside the closure.
        let egui_ctx = self.egui_ctx.clone();
        let full_output = egui_ctx.run(raw_input, |ctx| self.draw_ui(ctx));
        self.egui_winit
            .handle_platform_output(self.engine.window(), full_output.platform_output);

        let pixels_per_point = self.engine.window().scale_factor() as f32;
        let paint_jobs = self.egui_ctx.tessellate(full_output.shapes, pixels_per_point);
        let (sw, sh) = self.engine.surface_size();
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [sw, sh],
            pixels_per_point,
        };

        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(self.engine.device(), self.engine.queue(), *id, delta);
        }

        let acquired = self.engine.acquire().context("acquire surface")?;
        let surface_tex = match acquired {
            AcquiredFrame::Ready(tex) => tex,
            AcquiredFrame::Skip => return Ok(()),
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder =
            self.engine
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("avengine.frame"),
                });

        let extra_cmds = self.egui_renderer.update_buffers(
            self.engine.device(),
            self.engine.queue(),
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        {
            // egui-wgpu 0.30 requires `&mut RenderPass<'static>`; the
            // safe `forget_lifetime` upgrade does what the original
            // scaffold did with `mem::transmute`.
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("avengine.main_pass"),
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
            self.engine.draw_video(&mut rpass);
            self.egui_renderer.render(&mut rpass, &paint_jobs, &screen_descriptor);
        }

        let mut cmd_buffers: Vec<wgpu::CommandBuffer> = Vec::with_capacity(1 + extra_cmds.len());
        cmd_buffers.push(encoder.finish());
        cmd_buffers.extend(extra_cmds);
        self.engine.queue().submit(cmd_buffers);
        surface_tex.present();

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        Ok(())
    }

    fn draw_ui(&mut self, ctx: &egui::Context) {
        // Floating top bar so the video quad behind it stays visible
        // (the original scaffold used a CentralPanel which painted its
        // own opaque background over the video).
        egui::TopBottomPanel::top("transport")
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::from_black_alpha(160))
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.transport.playing { "Pause" } else { "Play" };
                    if ui.button(label).clicked() {
                        self.transport.toggle_play();
                    }
                    if ui.button("Restart").clicked() {
                        self.restart();
                    }
                    ui.checkbox(&mut self.transport.looping, "Loop");
                    ui.add(
                        egui::Slider::new(&mut self.transport.speed, 0.1..=4.0)
                            .text("speed")
                            .logarithmic(true),
                    );
                    ui.label(format!(
                        "{:.2}s / {:.2}s   {}x{} @ {:.2}fps",
                        self.transport.position,
                        self.stream.duration,
                        self.stream.width,
                        self.stream.height,
                        self.stream.frame_rate,
                    ));
                });
            });
    }
}
