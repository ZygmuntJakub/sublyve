use avengine_core::BlendMode;
use avengine_playback::StreamInfo;
use winit::monitor::MonitorHandle;

use crate::library::Library;

/// Which inspector the tabbed bottom panel currently shows. Auto-
/// switches based on the user's last meaningful action (cue → Clip,
/// trigger / select_layer → Layer); manual tab clicks override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum BottomTab {
    #[default]
    Layer,
    Clip,
}

/// Which section the right (settings) panel currently shows. Manual
/// switching only — config-style tabs, no auto-switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum RightTab {
    #[default]
    Preview,
    Video,
    Audio,
    Project,
}


/// Read-only snapshot of one layer that the UI renders.
#[derive(Debug, Clone, Copy)]
pub struct LayerView<'a> {
    pub index: usize,
    pub blend_mode: BlendMode,
    pub opacity: f32,
    /// Layer master fade (0.0..=1.0); applied on top of opacity + audio
    /// gain. Drag to 0 to fade the whole layer out at once.
    pub master: f32,
    pub mute: bool,
    pub playing: bool,
    pub looping: bool,
    pub speed: f64,
    pub position: f64,
    /// Active column on this layer (`None` = layer is empty).
    pub active_col: Option<usize>,
    /// Stream metadata for the active clip (size, fps, duration).
    pub info: Option<StreamInfo>,
    /// Display name for the active clip, if any.
    pub active_name: Option<&'a str>,
    /// Per-layer audio gain (1.0 = unity).
    pub audio_gain: f32,
}

#[derive(Debug, Default, Clone)]
pub struct UiActions {
    /// Cell hovered this frame (used by drag-drop targeting).
    pub hovered_cell: Option<(usize, usize)>,
    /// Plain click on a cell — load + play that clip on its layer.
    pub trigger_cell: Option<(usize, usize)>,
    /// Shift+click on a cell — load it into the Cue pane without
    /// triggering, ready for the user to TAKE on their own beat.
    pub cue_cell: Option<(usize, usize)>,
    /// Right-click — stop the layer that owns the cell.
    pub stop_layer_at: Option<(usize, usize)>,
    /// Take the cued clip (Take button or Enter).
    pub take: bool,
    /// Click on a layer's row label in the grid — show this layer in the
    /// right-hand inspector.
    pub select_layer: Option<usize>,
    /// Per-layer mutations.
    pub set_layer_mute: Option<(usize, bool)>,
    pub set_layer_blend: Option<(usize, BlendMode)>,
    pub set_layer_opacity: Option<(usize, f32)>,
    pub set_layer_looping: Option<(usize, bool)>,
    pub set_layer_speed: Option<(usize, f64)>,
    pub toggle_layer_play: Option<usize>,
    pub restart_layer: Option<usize>,
    /// Composition-wide.
    pub toggle_composition_play: bool,
    pub restart_composition: bool,
    /// Output window controls.
    pub set_output_monitor: Option<usize>,
    pub set_output_fullscreen: Option<bool>,
    pub refresh_monitors: bool,
    /// Browse… button on the bottom panel when the cue is parked on an
    /// empty cell — opens a native file dialog and imports into (r, c).
    pub browse_for_cell: Option<(usize, usize)>,
    /// Per-clip default-setting edits from the bottom inspector.
    pub set_clip_default_loop: Option<((usize, usize), bool)>,
    pub set_clip_default_speed: Option<((usize, usize), f64)>,
    pub set_clip_default_blend: Option<((usize, usize), BlendMode)>,
    /// Master audio volume (0.0..=2.0).
    pub set_master_volume: Option<f32>,
    /// Per-layer audio gain (0.0..=2.0).
    pub set_layer_audio_gain: Option<(usize, f32)>,
    /// Switch the active output audio device to the named one.
    pub set_audio_device: Option<String>,
    /// `💾 Save…` button — prompt for a path and save the project.
    pub save_project: bool,
    /// `📂 Open…` button — prompt for a path and load a project.
    pub open_project: bool,
    /// X button on the per-row quick-controls strip — clear the
    /// layer's active clip (drops decoder, kills audio + video).
    pub clear_layer: Option<usize>,
    /// Per-layer master fade slider (0.0..=1.0) on the quick-controls
    /// strip. Multiplies into both the visual opacity uniform and the
    /// audio mix gain.
    pub set_layer_master: Option<(usize, f32)>,
    /// Layer scrub bar — seek the layer's decoder to the given
    /// position in seconds. Right-click resets to 0 (= restart).
    pub seek_layer: Option<(usize, f64)>,
    /// Composition resize buttons.
    pub add_layer: bool,
    pub remove_layer: bool,
    pub add_column: bool,
    pub remove_column: bool,
    /// Bottom panel tab change from a manual header click. Auto-
    /// switches (`cue` / `trigger` / `select_layer`) write directly
    /// to `AppState::bottom_tab` and don't go through this.
    pub set_bottom_tab: Option<BottomTab>,
    /// Right (settings) panel tab change from a manual header click.
    pub set_right_tab: Option<RightTab>,
}

pub struct UiContext<'a> {
    pub library: &'a Library,
    pub layers: &'a [LayerView<'a>],
    pub cued: Option<(usize, usize)>,
    pub composition_playing: bool,
    pub output_texture: Option<egui::TextureId>,
    pub output_aspect: f32,
    pub cue_texture: Option<egui::TextureId>,
    pub cue_aspect: f32,
    /// Index of the layer the right inspector should display.
    pub selected_layer: Option<usize>,
    pub monitors: &'a [MonitorHandle],
    pub selected_monitor: usize,
    pub output_fullscreen: bool,
    pub audio_devices: &'a [String],
    pub current_audio_device: Option<&'a str>,
    pub master_volume: f32,
    /// Hard limits for the +/- buttons in the Composition section so
    /// the UI can disable them at the boundaries (which match
    /// `MAX_LAYERS` / `MAX_COLUMNS` in main.rs).
    pub max_layers: usize,
    pub max_columns: usize,
    /// Which tab the bottom panel should render this frame.
    pub bottom_tab: BottomTab,
    /// Which tab the right (settings) panel should render this frame.
    pub right_tab: RightTab,
}

pub fn draw_control(ctx: &egui::Context, ui_ctx: UiContext<'_>) -> UiActions {
    let mut actions = UiActions::default();

    egui::TopBottomPanel::top("avengine.transport")
        .resizable(false)
        .frame(panel_frame(255))
        .show(ctx, |ui| transport_bar(ui, &ui_ctx, &mut actions));

    // Settings panel on the right hosts the previously-left-panel
    // content (Output preview + Cue + TAKE + Output settings + Audio +
    // Composition) — one place for all the per-session config.
    egui::SidePanel::right("avengine.settings")
        .resizable(true)
        .default_width(300.0)
        .min_width(240.0)
        .frame(panel_frame(255))
        .show(ctx, |ui| settings_panel(ui, &ui_ctx, &mut actions));

    // Tabbed bottom panel: Layer inspector + Clip inspector share
    // the space, switched by the tab header. Default ~240 px is
    // enough for the Layer tab's transport / scrub / audio sections
    // without scrolling on a typical control window.
    egui::TopBottomPanel::bottom("avengine.bottom")
        .resizable(true)
        .default_height(240.0)
        .min_height(160.0)
        .frame(panel_frame(255))
        .show(ctx, |ui| bottom_tabs(ui, &ui_ctx, &mut actions));

    egui::CentralPanel::default()
        .frame(panel_frame(255))
        .show(ctx, |ui| grid_panel(ui, &ui_ctx, &mut actions));

    actions
}

/// `M:SS` (or `H:MM:SS` past an hour). Used by the layer inspector's
/// scrub bar so position / duration read like a normal media player.
fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Slider convention: right-click resets to a sensible default. Given
/// the slider's response and a `&mut value`, returns the value to
/// commit if anything changed (drag *or* right-click reset), or
/// `None` if the slider was untouched.
///
/// `egui::Slider` allocates its `Response` with `Sense::click_and_drag()`
/// which only tracks the *primary* mouse button — `resp.secondary_clicked()`
/// always returns `false` on a slider. We instead read the secondary
/// click directly from the input layer and gate it on `resp.hovered()`,
/// which gives us the "right-click on this slider" semantic. Reset
/// happens *after* the slider's own drag logic has already moved the
/// value, so we overwrite that to the default.
fn slider_value_after<F: Copy>(
    resp: egui::Response,
    value: &mut F,
    default: F,
) -> Option<F> {
    let right_clicked_on_slider = resp.hovered()
        && resp.ctx.input(|i| i.pointer.secondary_clicked());
    if right_clicked_on_slider {
        *value = default;
        return Some(default);
    }
    if resp.changed() {
        Some(*value)
    } else {
        None
    }
}

fn panel_frame(alpha: u8) -> egui::Frame {
    egui::Frame::default()
        .fill(egui::Color32::from_black_alpha(alpha))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
}

fn transport_bar(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.horizontal(|ui| {
        let label = if ctx.composition_playing { "⏸  Pause all" } else { "▶  Play all" };
        if ui.button(label).clicked() {
            actions.toggle_composition_play = true;
        }
        if ui.button("⏮  Restart all").clicked() {
            actions.restart_composition = true;
        }
        ui.separator();
        if ui.button("📂  Open…").clicked() {
            actions.open_project = true;
        }
        if ui.button("💾  Save…").clicked() {
            actions.save_project = true;
        }
        ui.separator();
        ui.label(format!(
            "{} layer{} · {} active",
            ctx.layers.len(),
            if ctx.layers.len() == 1 { "" } else { "s" },
            ctx.layers.iter().filter(|l| l.active_col.is_some()).count(),
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(
                    "Click=trigger · Shift+click=cue · Right-click=stop · Enter=Take",
                )
                .small()
                .weak(),
            );
        });
    });
}

fn settings_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    let mut chosen = ctx.right_tab;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut chosen, RightTab::Preview, "Preview");
        ui.selectable_value(&mut chosen, RightTab::Video, "Video");
        ui.selectable_value(&mut chosen, RightTab::Audio, "Audio");
        ui.selectable_value(&mut chosen, RightTab::Project, "Project");
    });
    if chosen != ctx.right_tab {
        actions.set_right_tab = Some(chosen);
    }
    ui.separator();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| match ctx.right_tab {
            RightTab::Preview => right_preview_tab(ui, ctx, actions),
            RightTab::Video => right_video_tab(ui, ctx, actions),
            RightTab::Audio => right_audio_tab(ui, ctx, actions),
            RightTab::Project => right_project_tab(ui, ctx, actions),
        });
}

/// Header (tab buttons) + body dispatch for the tabbed bottom panel.
/// Manual tab clicks emit `UiActions::set_bottom_tab`; auto-switches
/// on `cue` / `trigger` / `select_layer` happen on the AppState side.
fn bottom_tabs(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    let mut chosen = ctx.bottom_tab;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut chosen, BottomTab::Layer, "Layer");
        ui.selectable_value(&mut chosen, BottomTab::Clip, "Clip");
    });
    if chosen != ctx.bottom_tab {
        actions.set_bottom_tab = Some(chosen);
    }
    ui.separator();
    match ctx.bottom_tab {
        BottomTab::Layer => layer_inspector_tab(ui, ctx, actions),
        BottomTab::Clip => clip_inspector_tab(ui, ctx, actions),
    }
}

/// Preview tab: live Output preview, Cue preview, and the TAKE button —
/// the cluster the performer watches while triggering clips.
fn right_preview_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Output");
    if let Some(id) = ctx.output_texture {
        thumb_image(ui, id, ctx.output_aspect, ui.available_width());
    } else {
        thumb_placeholder(ui, ui.available_width(), 16.0 / 9.0, "no output yet");
    }
    ui.add_space(10.0);

    ui.heading("Cue");
    if let Some(id) = ctx.cue_texture {
        thumb_image(ui, id, ctx.cue_aspect, ui.available_width());
    } else {
        thumb_placeholder(
            ui,
            ui.available_width(),
            16.0 / 9.0,
            "shift+click a cell to cue",
        );
    }
    ui.add_space(8.0);

    let take_enabled = ctx.cued.is_some();
    let take_btn = egui::Button::new(egui::RichText::new("⟶  TAKE").strong().size(18.0))
        .min_size(egui::vec2(ui.available_width(), 36.0));
    if ui.add_enabled(take_enabled, take_btn).clicked() {
        actions.take = true;
    }
}

/// Video tab: where the output composition is rendered (monitor +
/// fullscreen). Audio routing has its own tab.
fn right_video_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    output_settings_section(ui, ctx, actions);
}

/// Audio tab: device selection + master volume.
fn right_audio_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    audio_settings_section(ui, ctx, actions);
}

/// Project tab: composition setup (layer count, column count,
/// composition resolution). Project file Save/Open still live in the
/// top transport bar.
fn right_project_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    composition_section(ui, ctx, actions);
}

fn composition_section(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Composition");
    ui.add_space(4.0);

    let layer_count = ctx.layers.len();
    let column_count = ctx.library.columns();

    composition_row(ui, "Layers", layer_count, ctx.max_layers, |which| match which {
        ResizeButton::Add => actions.add_layer = true,
        ResizeButton::Remove => actions.remove_layer = true,
    });
    composition_row(ui, "Columns", column_count, ctx.max_columns, |which| {
        match which {
            ResizeButton::Add => actions.add_column = true,
            ResizeButton::Remove => actions.remove_column = true,
        }
    });

    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(
            "Removing a layer drops the topmost row; removing a column \
             drops the rightmost. Clips in dropped cells are lost.",
        )
        .small()
        .weak(),
    );
}

enum ResizeButton {
    Add,
    Remove,
}

fn composition_row(
    ui: &mut egui::Ui,
    label: &str,
    count: usize,
    max: usize,
    mut emit: impl FnMut(ResizeButton),
) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        // Numeric value, fixed-width so the buttons don't dance
        // when the count goes from 1 → 2 → 9 → 10.
        ui.add_sized(
            egui::vec2(28.0, 18.0),
            egui::Label::new(egui::RichText::new(count.to_string()).strong())
                .selectable(false),
        );
        if ui
            .add_enabled(count > 1, egui::Button::new("−").min_size(egui::vec2(24.0, 22.0)))
            .clicked()
        {
            emit(ResizeButton::Remove);
        }
        if ui
            .add_enabled(count < max, egui::Button::new("+").min_size(egui::vec2(24.0, 22.0)))
            .clicked()
        {
            emit(ResizeButton::Add);
        }
    });
}

fn audio_settings_section(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Audio");
    ui.add_space(4.0);

    let current = ctx.current_audio_device.unwrap_or("(no device)").to_owned();
    let mut new_selected: Option<String> = None;
    egui::ComboBox::from_label("Output")
        .selected_text(current)
        .width((ui.available_width() - 70.0).max(160.0))
        .show_ui(ui, |ui| {
            if ctx.audio_devices.is_empty() {
                ui.label(egui::RichText::new("(no devices)").weak());
            }
            for name in ctx.audio_devices {
                let is_current = ctx.current_audio_device == Some(name.as_str());
                if ui.selectable_label(is_current, name).clicked() && !is_current {
                    new_selected = Some(name.clone());
                }
            }
        });
    if let Some(name) = new_selected {
        actions.set_audio_device = Some(name);
    }

    let mut master = ctx.master_volume;
    let resp = ui.add(
        egui::Slider::new(&mut master, 0.0..=2.0)
            .text("Master")
            .fixed_decimals(2),
    );
    if let Some(v) = slider_value_after(resp, &mut master, 1.0) {
        actions.set_master_volume = Some(v);
    }
}

fn output_settings_section(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    ui.heading("Output settings");
    ui.add_space(4.0);

    let current = ctx
        .monitors
        .get(ctx.selected_monitor)
        .map(|m| monitor_label(ctx.selected_monitor, m))
        .unwrap_or_else(|| "(no monitors)".to_owned());

    let mut new_selected = ctx.selected_monitor;
    egui::ComboBox::from_label("Monitor")
        .selected_text(current)
        .width((ui.available_width() - 70.0).max(140.0))
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

    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("On output window: F = fullscreen · Esc = leave fullscreen · M = next monitor")
            .small()
            .weak(),
    );
}

fn thumb_image(ui: &mut egui::Ui, id: egui::TextureId, aspect: f32, max_width: f32) {
    let w = max_width;
    let h = w / aspect.max(0.01);
    ui.image(egui::load::SizedTexture::new(id, egui::vec2(w, h)));
}

fn thumb_placeholder(ui: &mut egui::Ui, width: f32, aspect: f32, label: &str) {
    let h = width / aspect;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(20, 20, 26));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        egui::Color32::from_gray(120),
    );
}

fn layer_inspector_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| layer_inspector_body(ui, ctx, actions));
}

fn layer_inspector_body(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    let Some(idx) = ctx.selected_layer else {
        ui.heading("Layer");
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Click a row label (L0, L1, …) in the grid to inspect that layer.",
            )
            .weak(),
        );
        return;
    };
    let Some(layer) = ctx.layers.get(idx) else {
        return;
    };

    ui.heading(format!("Layer L{}", layer.index));
    if let Some(name) = layer.active_name {
        ui.label(egui::RichText::new(name).strong());
    } else {
        ui.label(egui::RichText::new("(empty)").weak());
    }
    if let Some(info) = layer.info {
        ui.label(
            egui::RichText::new(format!(
                "{}×{} @ {:.2}fps · {:.2}s",
                info.width, info.height, info.frame_rate, info.duration
            ))
            .small()
            .weak(),
        );
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    // Compositing controls (always available, even on empty layers, so
    // the user can preset a blend / opacity before triggering a clip).
    let mut mute = layer.mute;
    if ui.checkbox(&mut mute, "Mute").changed() {
        actions.set_layer_mute = Some((layer.index, mute));
    }

    let mut blend = layer.blend_mode;
    egui::ComboBox::from_label("Blend")
        .selected_text(layer.blend_mode.label())
        .width((ui.available_width() - 70.0).max(120.0))
        .show_ui(ui, |ui| {
            for &m in BlendMode::ALL {
                ui.selectable_value(&mut blend, m, m.label());
            }
        });
    if blend != layer.blend_mode {
        actions.set_layer_blend = Some((layer.index, blend));
    }

    let mut opacity = layer.opacity;
    let resp = ui.add(
        egui::Slider::new(&mut opacity, 0.0..=1.0)
            .text("Opacity")
            .fixed_decimals(2),
    );
    if let Some(v) = slider_value_after(resp, &mut opacity, 1.0) {
        actions.set_layer_opacity = Some((layer.index, v));
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    ui.heading("Transport");
    let has_clip = layer.active_col.is_some();
    ui.horizontal(|ui| {
        let play_label = if layer.playing { "⏸  Pause" } else { "▶  Play" };
        if ui
            .add_enabled(has_clip, egui::Button::new(play_label))
            .clicked()
        {
            actions.toggle_layer_play = Some(layer.index);
        }
        if ui
            .add_enabled(has_clip, egui::Button::new("⏮  Restart"))
            .clicked()
        {
            actions.restart_layer = Some(layer.index);
        }
    });

    let mut looping = layer.looping;
    if ui
        .add_enabled(has_clip, egui::Checkbox::new(&mut looping, "Loop"))
        .changed()
    {
        actions.set_layer_looping = Some((layer.index, looping));
    }

    let mut speed = layer.speed;
    let resp = ui.add_enabled(
        has_clip,
        egui::Slider::new(&mut speed, 0.1..=4.0)
            .text("Speed")
            .logarithmic(true),
    );
    if let Some(v) = slider_value_after(resp, &mut speed, 1.0) {
        actions.set_layer_speed = Some((layer.index, v));
    }

    ui.add_space(8.0);

    // Scrub bar: a normal-player-style timeline. The slider is bound
    // to a local mut that starts at the layer's current position; egui
    // moves it to the cursor while the user clicks / drags, and we
    // emit a seek action on every change. Right-click jumps to 0
    // (== restart-but-don't-touch-playing-state). On empty layers we
    // disable it instead of hiding it so the inspector layout doesn't
    // jump when a clip is loaded.
    if let Some(info) = layer.info {
        let duration = info.duration.max(0.001);
        let mut pos = layer.position.clamp(0.0, duration);
        let resp = ui.add_enabled(
            has_clip,
            egui::Slider::new(&mut pos, 0.0..=duration).show_value(false),
        );
        if let Some(v) = slider_value_after(resp, &mut pos, 0.0) {
            actions.seek_layer = Some((layer.index, v));
        }
        ui.label(format!("{} / {}", format_time(layer.position), format_time(info.duration)));
    } else {
        let mut zero = 0.0_f64;
        ui.add_enabled(false, egui::Slider::new(&mut zero, 0.0..=1.0).show_value(false));
        ui.label(egui::RichText::new("— / —").weak());
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    ui.heading("Audio");
    let mut gain = layer.audio_gain;
    let resp = ui.add(
        egui::Slider::new(&mut gain, 0.0..=2.0)
            .text("Gain")
            .fixed_decimals(2),
    );
    if let Some(v) = slider_value_after(resp, &mut gain, 1.0) {
        actions.set_layer_audio_gain = Some((layer.index, v));
    }
    ui.label(
        egui::RichText::new("Mute (above) silences both video and audio for this layer.")
            .small()
            .weak(),
    );

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    ui.heading("Master");
    let mut master = layer.master;
    let resp = ui.add(
        egui::Slider::new(&mut master, 0.0..=1.0)
            .text("Master")
            .fixed_decimals(2),
    );
    if let Some(v) = slider_value_after(resp, &mut master, 1.0) {
        actions.set_layer_master = Some((layer.index, v));
    }
    ui.label(
        egui::RichText::new(
            "Master fades both video opacity and audio gain at once \
             — same control as the rightmost slider in the row's quick \
             strip. Right-click any slider to reset.",
        )
        .small()
        .weak(),
    );
}

const CELL_FOOTER_H: f32 = 20.0;
const CELL_GAP: f32 = 4.0;
const ROW_LABEL_W: f32 = 28.0;
const MIN_CELL_W: f32 = 96.0;
/// Total width of the per-row quick-controls strip (X button + 3
/// vertical sliders + their labels). Tuned to host three vertical
/// sliders side-by-side without crowding.
const QUICK_STRIP_W: f32 = 132.0;
const QUICK_SLIDER_W: f32 = 32.0;

fn clip_inspector_tab(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| clip_inspector_body(ui, ctx, actions));
}

fn clip_inspector_body(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    match ctx.cued {
        None => clip_inspector_hint(ui),
        Some((r, c)) => match ctx.library.cell(r, c) {
            Some(slot) => clip_metadata_inspector(ui, ctx, actions, r, c, slot),
            None => empty_slot_inspector(ui, actions, r, c),
        },
    }
}

fn clip_inspector_hint(ui: &mut egui::Ui) {
    ui.heading("Clip");
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(
            "Shift+click any cell in the grid to inspect that clip — \
             empty cells let you browse for a file, filled cells show \
             metadata and per-clip defaults.",
        )
        .weak(),
    );
}

fn empty_slot_inspector(
    ui: &mut egui::Ui,
    actions: &mut UiActions,
    row: usize,
    col: usize,
) {
    ui.heading(format!("Empty slot · L{row} · C{col}"));
    ui.add_space(8.0);

    let btn = egui::Button::new(egui::RichText::new("📂  Browse…").strong().size(16.0))
        .min_size(egui::vec2(180.0, 36.0));
    if ui.add(btn).clicked() {
        actions.browse_for_cell = Some((row, col));
    }

    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("…or drop a video file directly onto this cell.")
            .small()
            .weak(),
    );
}

fn clip_metadata_inspector(
    ui: &mut egui::Ui,
    _ctx: &UiContext<'_>,
    actions: &mut UiActions,
    row: usize,
    col: usize,
    slot: &crate::library::ClipSlot,
) {
    ui.horizontal_top(|ui| {
        // Left column: thumbnail (or placeholder if not yet decoded).
        let thumb_w = 240.0;
        let thumb_h = thumb_w * 9.0 / 16.0;
        ui.allocate_ui_with_layout(
            egui::vec2(thumb_w, thumb_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                if let Some(id) = slot.thumbnail_id {
                    ui.image(egui::load::SizedTexture::new(id, egui::vec2(thumb_w, thumb_h)));
                } else {
                    thumb_placeholder(ui, thumb_w, 16.0 / 9.0, "no thumbnail");
                }
            },
        );

        ui.add_space(14.0);

        // Right column: metadata + defaults editors.
        ui.vertical(|ui| {
            ui.heading(&slot.name);
            ui.label(
                egui::RichText::new(slot.path.display().to_string())
                    .small()
                    .weak(),
            );
            if let Some(thumb) = slot.thumbnail.as_ref() {
                let (w, h) = thumb.size();
                ui.label(
                    egui::RichText::new(format!("source thumbnail {}×{}", w, h))
                        .small()
                        .weak(),
                );
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);

            ui.label(
                egui::RichText::new("Defaults applied on every trigger")
                    .small()
                    .weak(),
            );

            // Default loop.
            let mut looping = slot.defaults.looping;
            if ui.checkbox(&mut looping, "Loop").changed() {
                actions.set_clip_default_loop = Some(((row, col), looping));
            }

            // Default speed.
            let mut speed = slot.defaults.speed;
            let resp = ui.add(
                egui::Slider::new(&mut speed, 0.1..=4.0)
                    .text("Speed")
                    .logarithmic(true),
            );
            if let Some(v) = slider_value_after(resp, &mut speed, 1.0) {
                actions.set_clip_default_speed = Some(((row, col), v));
            }

            // Default blend.
            let mut blend = slot.defaults.blend;
            egui::ComboBox::from_label("Blend")
                .selected_text(slot.defaults.blend.label())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for &m in BlendMode::ALL {
                        ui.selectable_value(&mut blend, m, m.label());
                    }
                });
            if blend != slot.defaults.blend {
                actions.set_clip_default_blend = Some(((row, col), blend));
            }
        });
    });
}

fn grid_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    let cols = ctx.library.columns();
    let rows = ctx.library.layers();
    if cols == 0 || rows == 0 {
        return;
    }

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let avail = ui.available_width()
                - ROW_LABEL_W
                - QUICK_STRIP_W
                - (cols as f32 + 1.0) * CELL_GAP;
            let cell_w = (avail / cols as f32).max(MIN_CELL_W);
            // 16:9 thumbnail area + a small footer for the badges and name.
            let cell_h = cell_w * 9.0 / 16.0 + CELL_FOOTER_H;

            // Layer 0 sits at the bottom — front layers float to the top,
            // matching the layer inspector ordering and Resolume's
            // back-to-front Z stack.
            for row in (0..rows).rev() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = CELL_GAP;
                    row_label(ui, ctx, actions, row, cell_h);
                    quick_controls_strip(ui, ctx, actions, row, cell_h);
                    for col in 0..cols {
                        cell_widget(ui, ctx, actions, row, col, cell_w, cell_h);
                    }
                });
                ui.add_space(CELL_GAP);
            }
        });
}

/// Per-row Resolume-style quick controls: an `X` clear button and
/// three vertical faders for **V**olume, **O**pacity, and **M**aster.
/// All three are live: drag-down on Master fades both audio and video
/// at once. The right-panel inspector keeps the same controls with
/// numeric labels for fine adjustment; the strip is for live use.
fn quick_controls_strip(
    ui: &mut egui::Ui,
    ctx: &UiContext<'_>,
    actions: &mut UiActions,
    row: usize,
    height: f32,
) {
    let layer = ctx.layers.get(row).copied();

    ui.allocate_ui_with_layout(
        egui::vec2(QUICK_STRIP_W, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;

            // Clear button: small red X, top-aligned to the strip.
            let has_clip = layer.is_some_and(|l| l.active_col.is_some());
            ui.vertical(|ui| {
                ui.add_space(2.0);
                let btn = egui::Button::new(
                    egui::RichText::new("✕")
                        .strong()
                        .color(egui::Color32::from_rgb(220, 80, 80)),
                )
                .min_size(egui::vec2(20.0, 22.0));
                if ui.add_enabled(has_clip, btn)
                    .on_hover_text("Clear this layer (stops audio + video)")
                    .clicked()
                {
                    actions.clear_layer = Some(row);
                }
            });

            let Some(layer) = layer else { return };

            // Three vertical sliders: V, O, M. Each takes the full
            // strip height minus a small label footer. egui's
            // `Slider::vertical()` does the orientation; `text("X")`
            // would print on the side, so we render labels manually
            // with `Painter` after the slider.
            let slider_h = (height - 18.0).max(60.0);

            // Volume.
            let mut gain = layer.audio_gain;
            ui.vertical(|ui| {
                let resp = ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, slider_h),
                    egui::Slider::new(&mut gain, 0.0..=2.0)
                        .vertical()
                        .show_value(false),
                );
                if let Some(v) = slider_value_after(resp, &mut gain, 1.0) {
                    actions.set_layer_audio_gain = Some((row, v));
                }
                ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, 12.0),
                    egui::Label::new(egui::RichText::new("Vol").small().weak())
                        .selectable(false),
                );
            });

            // Opacity.
            let mut opacity = layer.opacity;
            ui.vertical(|ui| {
                let resp = ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, slider_h),
                    egui::Slider::new(&mut opacity, 0.0..=1.0)
                        .vertical()
                        .show_value(false),
                );
                if let Some(v) = slider_value_after(resp, &mut opacity, 1.0) {
                    actions.set_layer_opacity = Some((row, v));
                }
                ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, 12.0),
                    egui::Label::new(egui::RichText::new("Opa").small().weak())
                        .selectable(false),
                );
            });

            // Master.
            let mut master = layer.master;
            ui.vertical(|ui| {
                let resp = ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, slider_h),
                    egui::Slider::new(&mut master, 0.0..=1.0)
                        .vertical()
                        .show_value(false),
                );
                if let Some(v) = slider_value_after(resp, &mut master, 1.0) {
                    actions.set_layer_master = Some((row, v));
                }
                ui.add_sized(
                    egui::vec2(QUICK_SLIDER_W, 12.0),
                    egui::Label::new(
                        egui::RichText::new("Mst").small().strong(),
                    )
                    .selectable(false),
                );
            });
        },
    );
}

fn row_label(
    ui: &mut egui::Ui,
    ctx: &UiContext<'_>,
    actions: &mut UiActions,
    row: usize,
    height: f32,
) {
    let selected = ctx.selected_layer == Some(row);
    let text = if selected {
        egui::RichText::new(format!("L{row}"))
            .strong()
            .color(egui::Color32::from_rgb(80, 220, 140))
    } else {
        egui::RichText::new(format!("L{row}")).small().weak()
    };
    let response = ui.add_sized(
        egui::vec2(ROW_LABEL_W, height),
        egui::Label::new(text)
            .selectable(false)
            .sense(egui::Sense::click()),
    );
    if response.clicked() {
        actions.select_layer = Some(row);
    }
}

fn cell_widget(
    ui: &mut egui::Ui,
    ctx: &UiContext<'_>,
    actions: &mut UiActions,
    row: usize,
    col: usize,
    width: f32,
    height: f32,
) {
    // `allocate_exact_size` is the only reliable way to enforce a uniform
    // cell footprint regardless of inner content. Painting through `Painter`
    // (rather than nested widgets) avoids egui's auto-grow behaviour.
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());

    let clip = ctx.library.cell(row, col);
    let active = ctx.layers.get(row).is_some_and(|l| l.active_col == Some(col));
    let cued = ctx.cued == Some((row, col));

    let (bg, stroke_color, stroke_w) = cell_palette(active, cued, clip.is_some());
    let painter = ui.painter().with_clip_rect(rect);
    let rounding = egui::Rounding::same(4.0);
    painter.rect_filled(rect, rounding, bg);
    painter.rect_stroke(rect, rounding, egui::Stroke::new(stroke_w, stroke_color));

    if let Some(c) = clip {
        let footer_y = rect.max.y - CELL_FOOTER_H;
        let thumb_area = egui::Rect::from_min_max(
            rect.min + egui::vec2(3.0, 3.0),
            egui::pos2(rect.max.x - 3.0, footer_y - 1.0),
        );

        // Solid black under the thumbnail so any letterbox bars look
        // intentional rather than seeing the cell background through them.
        painter.rect_filled(thumb_area, egui::Rounding::same(2.0), egui::Color32::BLACK);

        if let Some(id) = c.thumbnail_id {
            let aspect = c
                .thumbnail
                .as_ref()
                .map_or(16.0 / 9.0, avengine_compositor::Thumbnail::aspect_ratio);
            let fit = letterbox_inside(thumb_area, aspect);
            painter.image(
                id,
                fit,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.text(
                thumb_area.center(),
                egui::Align2::CENTER_CENTER,
                "…",
                egui::FontId::proportional(13.0),
                egui::Color32::from_gray(140),
            );
        }

        // Footer: badges followed by the name. Painted through the
        // already-clipped Painter, so an over-long name is cut at the
        // cell edge instead of shoving the cell wider.
        let baseline = egui::pos2(rect.min.x + 6.0, rect.max.y - CELL_FOOTER_H * 0.5);
        let mut x = baseline.x;
        let font = egui::FontId::proportional(11.0);
        if active {
            x = paint_badge(&painter, egui::pos2(x, baseline.y), "▶", &font,
                egui::Color32::from_rgb(80, 220, 140));
        }
        if cued {
            x = paint_badge(&painter, egui::pos2(x, baseline.y), "★", &font,
                egui::Color32::from_rgb(240, 220, 80));
        }
        painter.text(
            egui::pos2(x, baseline.y),
            egui::Align2::LEFT_CENTER,
            &c.name,
            font,
            egui::Color32::from_gray(220),
        );
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "+",
            egui::FontId::proportional(20.0),
            egui::Color32::from_gray(80),
        );
    }

    if response.hovered() {
        actions.hovered_cell = Some((row, col));
    }

    // Click semantics:
    //   - Plain click on a filled cell  → trigger immediately.
    //   - Shift+click on any cell       → cue. On a filled cell this
    //     parks the clip on the preview deck; on an empty cell it
    //     parks the cue on the slot so the bottom inspector flips to
    //     "Browse…" mode.
    //   - Right-click on a filled cell  → stop the layer.
    if response.clicked() || response.double_clicked() {
        let shift = ui.input(|i| i.modifiers.shift);
        if shift {
            actions.cue_cell = Some((row, col));
        } else if clip.is_some() {
            actions.trigger_cell = Some((row, col));
        }
    }
    if clip.is_some() && response.secondary_clicked() {
        actions.stop_layer_at = Some((row, col));
    }
}

fn paint_badge(
    painter: &egui::Painter,
    pos: egui::Pos2,
    glyph: &str,
    font: &egui::FontId,
    color: egui::Color32,
) -> f32 {
    let galley = painter.text(pos, egui::Align2::LEFT_CENTER, glyph, font.clone(), color);
    galley.right() + 4.0
}

fn cell_palette(
    active: bool,
    cued: bool,
    has_clip: bool,
) -> (egui::Color32, egui::Color32, f32) {
    if active {
        (
            egui::Color32::from_rgb(40, 100, 60),
            egui::Color32::from_rgb(80, 220, 140),
            2.0,
        )
    } else if cued {
        (
            egui::Color32::from_rgb(80, 80, 30),
            egui::Color32::from_rgb(220, 200, 80),
            2.0,
        )
    } else if has_clip {
        (
            egui::Color32::from_rgb(28, 30, 38),
            egui::Color32::from_gray(60),
            1.0,
        )
    } else {
        (
            egui::Color32::from_rgb(18, 18, 22),
            egui::Color32::from_gray(45),
            1.0,
        )
    }
}

/// Fit a `aspect`-ratio image into `area`, centring it. Adds letterbox
/// bars on the long axis instead of stretching.
fn letterbox_inside(area: egui::Rect, aspect: f32) -> egui::Rect {
    if area.width() <= 0.0 || area.height() <= 0.0 || aspect <= 0.0 {
        return area;
    }
    let area_aspect = area.width() / area.height();
    if aspect > area_aspect {
        // Source is wider than area → fit to width, letterbox top/bottom.
        let h = area.width() / aspect;
        let y = area.center().y - h / 2.0;
        egui::Rect::from_min_size(egui::pos2(area.min.x, y), egui::vec2(area.width(), h))
    } else {
        // Source is taller than area → fit to height, pillarbox left/right.
        let w = area.height() * aspect;
        let x = area.center().x - w / 2.0;
        egui::Rect::from_min_size(egui::pos2(x, area.min.y), egui::vec2(w, area.height()))
    }
}

fn monitor_label(index: usize, m: &MonitorHandle) -> String {
    let name = m.name().unwrap_or_else(|| format!("Monitor {index}"));
    let size = m.size();
    format!("[{index}] {} — {}×{}", name, size.width, size.height)
}
