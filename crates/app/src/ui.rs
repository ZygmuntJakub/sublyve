use avengine_playback::StreamInfo;
use winit::monitor::MonitorHandle;

use crate::library::Library;

/// Snapshot of the active deck for the UI to read. We deliberately don't
/// hand the UI a `&mut Deck` because egui's nested `.show(...)` panels
/// each take a `FnOnce` closure that would each want its own `&mut`.
/// Instead, controls bind to scratch locals and emit `UiActions` if the
/// user changed them.
#[derive(Debug, Clone, Copy)]
pub struct DeckView<'a> {
    pub playing: bool,
    pub looping: bool,
    pub speed: f64,
    pub position: f64,
    pub info: &'a StreamInfo,
}

/// Per-frame intent emitted by the UI. The render loop applies these
/// after the egui pass so we don't tangle UI mutation with engine state.
#[derive(Debug, Default, Clone)]
pub struct UiActions {
    pub activate_clip: Option<usize>,
    pub remove_clip: Option<usize>,
    pub toggle_play: bool,
    pub restart: bool,
    pub set_looping: Option<bool>,
    pub set_speed: Option<f64>,
    pub set_output_monitor: Option<usize>,
    pub set_output_fullscreen: Option<bool>,
    pub refresh_monitors: bool,
    pub open_files: bool,
}

pub struct UiContext<'a> {
    pub library: &'a Library,
    pub deck: Option<DeckView<'a>>,
    pub monitors: &'a [MonitorHandle],
    pub selected_monitor: usize,
    pub output_fullscreen: bool,
}

pub fn draw_control(ctx: &egui::Context, ui_ctx: UiContext<'_>) -> UiActions {
    let mut actions = UiActions::default();

    egui::TopBottomPanel::top("avengine.transport")
        .resizable(false)
        .frame(panel_frame(220))
        .show(ctx, |ui| transport_bar(ui, &ui_ctx, &mut actions));

    egui::SidePanel::left("avengine.library")
        .resizable(true)
        .default_width(240.0)
        .min_width(180.0)
        .frame(panel_frame(220))
        .show(ctx, |ui| library_panel(ui, &ui_ctx, &mut actions));

    egui::SidePanel::right("avengine.output")
        .resizable(true)
        .default_width(260.0)
        .min_width(200.0)
        .frame(panel_frame(220))
        .show(ctx, |ui| output_panel(ui, &ui_ctx, &mut actions));

    egui::TopBottomPanel::bottom("avengine.status")
        .resizable(false)
        .frame(panel_frame(180))
        .show(ctx, |ui| status_bar(ui, &ui_ctx));

    // The center stays unpainted so the video preview shows through.
    egui::CentralPanel::default()
        .frame(egui::Frame::none())
        .show(ctx, |_ui| {});

    actions
}

fn panel_frame(alpha: u8) -> egui::Frame {
    egui::Frame::default()
        .fill(egui::Color32::from_black_alpha(alpha))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
}

fn transport_bar(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.horizontal(|ui| {
        let has_deck = ctx.deck.is_some();
        let playing = ctx.deck.is_some_and(|d| d.playing);

        let label = if playing { "⏸  Pause" } else { "▶  Play" };
        if ui
            .add_enabled(has_deck, egui::Button::new(label))
            .clicked()
        {
            actions.toggle_play = true;
        }
        if ui
            .add_enabled(has_deck, egui::Button::new("⏮  Restart"))
            .clicked()
        {
            actions.restart = true;
        }

        ui.separator();

        let mut looping = ctx.deck.is_some_and(|d| d.looping);
        if ui
            .add_enabled(has_deck, egui::Checkbox::new(&mut looping, "Loop"))
            .changed()
        {
            actions.set_looping = Some(looping);
        }

        let mut speed = ctx.deck.map_or(1.0, |d| d.speed);
        let resp = ui.add_enabled(
            has_deck,
            egui::Slider::new(&mut speed, 0.1..=4.0)
                .text("speed")
                .logarithmic(true),
        );
        if resp.changed() {
            actions.set_speed = Some(speed);
        }

        ui.separator();
        if let Some(d) = ctx.deck {
            ui.label(format!("{:>6.2}s / {:>6.2}s", d.position, d.info.duration));
        } else {
            ui.label(egui::RichText::new("no clip loaded").weak());
        }
    });
}

fn library_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Library");
    ui.horizontal(|ui| {
        if ui.button("➕  Open files…").clicked() {
            actions.open_files = true;
        }
        ui.label(
            egui::RichText::new("(or drag files in)")
                .small()
                .weak(),
        );
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if ctx.library.clips.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Library is empty.\nDrop a video file onto the window.")
                        .weak(),
                );
                return;
            }
            for (i, clip) in ctx.library.clips.iter().enumerate() {
                let active = ctx.library.is_active(i);
                ui.horizontal(|ui| {
                    let label = if active {
                        egui::RichText::new(format!("▶  {}", clip.name)).strong()
                    } else {
                        egui::RichText::new(format!("    {}", clip.name))
                    };
                    if ui
                        .selectable_label(active, label)
                        .on_hover_text(clip.path.display().to_string())
                        .clicked()
                    {
                        actions.activate_clip = Some(i);
                    }
                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                        actions.remove_clip = Some(i);
                    }
                });
            }
        });
}

fn output_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Output");
    ui.add_space(4.0);

    let current = ctx
        .monitors
        .get(ctx.selected_monitor)
        .map(|m| monitor_label(ctx.selected_monitor, m))
        .unwrap_or_else(|| "(no monitors)".to_owned());

    let mut new_selected = ctx.selected_monitor;
    egui::ComboBox::from_label("Monitor")
        .selected_text(current)
        .width(220.0)
        .show_ui(ui, |ui| {
            for (i, m) in ctx.monitors.iter().enumerate() {
                ui.selectable_value(&mut new_selected, i, monitor_label(i, m));
            }
            ui.separator();
            if ui.button("Refresh").clicked() {
                actions.refresh_monitors = true;
            }
        });
    if new_selected != ctx.selected_monitor {
        actions.set_output_monitor = Some(new_selected);
    }

    let mut fs = ctx.output_fullscreen;
    if ui.checkbox(&mut fs, "Fullscreen").changed() {
        actions.set_output_fullscreen = Some(fs);
    }

    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "Shortcuts on output window:\n  F = fullscreen · Esc = exit fullscreen",
        )
        .small()
        .weak(),
    );
}

fn status_bar(ui: &mut egui::Ui, ctx: &UiContext<'_>) {
    ui.horizontal(|ui| {
        ui.label(format!("Clips: {}", ctx.library.clips.len()));
        ui.separator();
        if let Some(d) = ctx.deck {
            ui.label(format!(
                "{}×{} @ {:.2}fps",
                d.info.width, d.info.height, d.info.frame_rate
            ));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("Space play/pause · R restart · H hide UI")
                    .small()
                    .weak(),
            );
        });
    });
}

fn monitor_label(index: usize, m: &MonitorHandle) -> String {
    let name = m.name().unwrap_or_else(|| format!("Monitor {index}"));
    let size = m.size();
    format!("[{index}] {} — {}×{}", name, size.width, size.height)
}
