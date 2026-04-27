use std::time::Instant;

use avengine_compositor::Engine;
use avengine_core::VideoFrame;
use avengine_playback::{Decoder, Transport};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::WindowId;

struct AppState {
    engine: Engine,
    decoder: Decoder,
    transport: Transport,
    current_frame: Option<VideoFrame>,
    egui_ctx: egui::Context,
    egui_winit_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    last_time: Instant,
    frame_duration: f64,
    accumulated_time: f64,
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop, path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let attrs = winit::window::WindowAttributes::default().with_title("avengine");
        let window = event_loop.create_window(attrs)?;

        let mut decoder = Decoder::open(path)?;
        let frame_duration = 1.0 / 30.0;

        let mut engine = pollster::block_on(Engine::new(window));

        let mut first_frame = None;
        if let Ok(Some(frame)) = decoder.next_frame() {
            engine.update_texture(&frame);
            first_frame = Some(frame);
        }

        let egui_ctx = egui::Context::default();

        let egui_winit_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::viewport::ViewportId::ROOT,
            engine.window(),
            None,
            None,
            None,
        );

        let egui_renderer =
            egui_wgpu::Renderer::new(engine.device(), engine.config().format, None, 1, false);

        Ok(Self {
            engine,
            decoder,
            transport: Transport::new(),
            current_frame: first_frame,
            egui_ctx,
            egui_winit_state,
            egui_renderer,
            last_time: Instant::now(),
            frame_duration,
            accumulated_time: 0.0,
        })
    }

    fn handle_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
    ) {
        let _ = self
            .egui_winit_state
            .on_window_event(self.engine.window(), &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.engine.resize(size.width, size.height);
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(keycode),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match keycode {
                KeyCode::Space => {
                    self.transport.toggle_play();
                }
                KeyCode::Escape => event_loop.exit(),
                _ => {}
            },
            WindowEvent::RedrawRequested => {
                self.render();
            }
            _ => {}
        }
    }

    fn render(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_time).as_secs_f64();
        self.last_time = now;

        let duration = self.decoder.duration();

        if self.transport.position >= duration && duration > 0.0 {
            if self.transport.loop_enabled {
                self.transport.position = 0.0;
                let _ = self.decoder.seek(0.0);
                self.current_frame = None;
            } else {
                self.transport.playing = false;
                self.transport.position = duration;
            }
        }

        if self.transport.playing {
            self.accumulated_time += dt;
            while self.accumulated_time >= self.frame_duration {
                self.accumulated_time -= self.frame_duration;
                match self.decoder.next_frame() {
                    Ok(Some(frame)) => {
                        self.engine.update_texture(&frame);
                        self.current_frame = Some(frame);
                    }
                    Ok(None) => {
                        if self.transport.loop_enabled {
                            let _ = self.decoder.seek(0.0);
                            self.transport.position = 0.0;
                            self.current_frame = None;
                        } else {
                            self.transport.playing = false;
                        }
                        break;
                    }
                    Err(e) => {
                        eprintln!("decode error: {e}");
                        self.transport.playing = false;
                        break;
                    }
                }
            }
        }
        self.transport.advance(dt, 1.0);

        // Build egui UI
        let raw_input = self.egui_winit_state.take_egui_input(self.engine.window());
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.transport.playing {
                        "‖ Pause"
                    } else {
                        "▶ Play"
                    };
                    if ui.button(label).clicked() {
                        self.transport.toggle_play();
                    }

                    if ui.button("⏮ Restart").clicked() {
                        self.transport.position = 0.0;
                        self.current_frame = None;
                        let _ = self.decoder.seek(0.0);
                        self.transport.playing = true;
                    }

                    ui.checkbox(&mut self.transport.loop_enabled, "Loop");
                });

                ui.label(format!(
                    "{:.1}s / {:.1}s",
                    self.transport.position,
                    self.decoder.duration()
                ));

                if let Some(ref frame) = self.current_frame {
                    ui.label(format!("{}x{}", frame.width, frame.height));
                }
            });
        });
        self.egui_winit_state
            .handle_platform_output(self.engine.window(), full_output.platform_output);

        let pixels_per_point = self.engine.window().scale_factor() as f32;
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [
                self.engine.config().width,
                self.engine.config().height,
            ],
            pixels_per_point,
        };

        // Upload egui textures
        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(self.engine.device(), self.engine.queue(), *id, delta);
        }

        // Create encoder and upload buffers
        let mut encoder =
            self.engine
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("main encoder"),
                });

        let extra_cmds = self.egui_renderer.update_buffers(
            self.engine.device(),
            self.engine.queue(),
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        // Render
        let output = self.engine.surface().get_current_texture().unwrap();
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Single render pass: video quad first, then egui on top
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.engine.render_video_pass(&mut rpass);
            self.egui_renderer.render(
                unsafe { std::mem::transmute(&mut rpass) },
                &paint_jobs,
                &screen_descriptor,
            );
        }

        let mut cmds: Vec<wgpu::CommandBuffer> =
            Vec::with_capacity(1 + extra_cmds.len());
        cmds.push(encoder.finish());
        cmds.extend(extra_cmds);
        self.engine.queue().submit(cmds);
        output.present();

        // Free egui textures
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
    }
}

struct App {
    state: Option<AppState>,
    path: String,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            match AppState::new(event_loop, &self.path) {
                Ok(state) => self.state = Some(state),
                Err(e) => {
                    eprintln!("failed to initialize: {e}");
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(ref mut state) = self.state {
            state.handle_window_event(event_loop, event);
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(ref state) = self.state {
            state.engine.window().request_redraw();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: avengine <video.mp4>");
        std::process::exit(1);
    });

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        state: None,
        path,
    };

    event_loop.run_app(&mut app)?;

    Ok(())
}
