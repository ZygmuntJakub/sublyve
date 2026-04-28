use avengine_core::BlendMode;
use avengine_playback::StreamInfo;
use winit::monitor::MonitorHandle;

use crate::library::Library;

/// Read-only snapshot of one layer that the UI renders.
#[derive(Debug, Clone, Copy)]
pub struct LayerView<'a> {
    pub index: usize,
    pub blend_mode: BlendMode,
    pub opacity: f32,
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
}

pub fn draw_control(ctx: &egui::Context, ui_ctx: UiContext<'_>) -> UiActions {
    let mut actions = UiActions::default();

    egui::TopBottomPanel::top("avengine.transport")
        .resizable(false)
        .frame(panel_frame(255))
        .show(ctx, |ui| transport_bar(ui, &ui_ctx, &mut actions));

    egui::SidePanel::left("avengine.left")
        .resizable(true)
        .default_width(300.0)
        .min_width(240.0)
        .frame(panel_frame(255))
        .show(ctx, |ui| left_panel(ui, &ui_ctx, &mut actions));

    egui::SidePanel::right("avengine.right")
        .resizable(true)
        .default_width(280.0)
        .min_width(220.0)
        .frame(panel_frame(255))
        .show(ctx, |ui| right_panel(ui, &ui_ctx, &mut actions));

    egui::TopBottomPanel::bottom("avengine.clip_inspector")
        .resizable(true)
        .default_height(170.0)
        .min_height(120.0)
        .frame(panel_frame(255))
        .show(ctx, |ui| clip_inspector(ui, &ui_ctx, &mut actions));

    egui::CentralPanel::default()
        .frame(panel_frame(255))
        .show(ctx, |ui| grid_panel(ui, &ui_ctx, &mut actions));

    actions
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

fn left_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
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

    ui.add_space(14.0);
    ui.separator();
    ui.add_space(6.0);

    output_settings_section(ui, ctx, actions);
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

fn right_panel(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
    let Some(idx) = ctx.selected_layer else {
        ui.heading("Layer");
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Click a row label (L0, L1, …) on the left of the grid to inspect that layer.",
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
    if ui
        .add(
            egui::Slider::new(&mut opacity, 0.0..=1.0)
                .text("Opacity")
                .fixed_decimals(2),
        )
        .changed()
    {
        actions.set_layer_opacity = Some((layer.index, opacity));
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
    if ui
        .add_enabled(
            has_clip,
            egui::Slider::new(&mut speed, 0.1..=4.0)
                .text("Speed")
                .logarithmic(true),
        )
        .changed()
    {
        actions.set_layer_speed = Some((layer.index, speed));
    }

    ui.add_space(8.0);
    if let Some(info) = layer.info {
        ui.label(format!("{:>6.2}s / {:>6.2}s", layer.position, info.duration));
    }
}

const CELL_FOOTER_H: f32 = 20.0;
const CELL_GAP: f32 = 4.0;
const ROW_LABEL_W: f32 = 28.0;
const MIN_CELL_W: f32 = 96.0;

fn clip_inspector(ui: &mut egui::Ui, ctx: &UiContext<'_>, actions: &mut UiActions) {
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
            if ui
                .add(
                    egui::Slider::new(&mut speed, 0.1..=4.0)
                        .text("Speed")
                        .logarithmic(true),
                )
                .changed()
            {
                actions.set_clip_default_speed = Some(((row, col), speed));
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
            let avail = ui.available_width() - ROW_LABEL_W - (cols as f32 + 1.0) * CELL_GAP;
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
                    for col in 0..cols {
                        cell_widget(ui, ctx, actions, row, col, cell_w, cell_h);
                    }
                });
                ui.add_space(CELL_GAP);
            }
        });
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
