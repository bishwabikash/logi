//! Visual styling.
//!
//! egui's defaults are a developer-tool look: cramped spacing, small text, grey
//! on grey. This is a hardware control panel someone opens to change one
//! setting, so it gets a dark surface, one accent colour, and enough breathing
//! room to scan quickly.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

/// Near-black page, one step up for cards, so elevation reads without borders.
pub const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1a);
pub const SURFACE: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x25);
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x24, 0x28, 0x30);
pub const LINE: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3d);

pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe9, 0xef);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa3);

/// The mouse's own default logo colour, so the app and the hardware agree.
pub const ACCENT: Color32 = Color32::from_rgb(0x2d, 0xd4, 0xd4);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x1c, 0x7d, 0x7d);
pub const GOOD: Color32 = Color32::from_rgb(0x5a, 0xd1, 0x8b);
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xb0, 0x4d);
pub const BAD: Color32 = Color32::from_rgb(0xe8, 0x6b, 0x6b);

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(0x0e, 0x10, 0x13);
    v.faint_bg_color = SURFACE;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    let r = CornerRadius::same(7);
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.corner_radius = r;

    v.widgets.inactive.bg_fill = SURFACE_HI;
    v.widgets.inactive.weak_bg_fill = SURFACE_HI;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.corner_radius = r;

    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2e, 0x34, 0x3e);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2e, 0x34, 0x3e);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_DIM);
    v.widgets.hovered.corner_radius = r;

    v.widgets.active.bg_fill = ACCENT_DIM;
    v.widgets.active.weak_bg_fill = ACCENT_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.corner_radius = r;

    v.widgets.open.bg_fill = SURFACE_HI;
    v.widgets.open.corner_radius = r;
    ctx.set_visuals(v);

    let mut s = (*ctx.style()).clone();
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, Proportional)),
        (TextStyle::Body, FontId::new(14.0, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, egui::FontFamily::Monospace)),
    ]
    .into();
    s.spacing.item_spacing = egui::vec2(10.0, 9.0);
    s.spacing.button_padding = egui::vec2(12.0, 7.0);
    s.spacing.slider_width = 300.0;
    s.spacing.slider_rail_height = 6.0;
    s.spacing.combo_width = 160.0;
    s.spacing.interact_size.y = 28.0;
    s.visuals.indent_has_left_vline = false;
    ctx.set_style(s);
}

/// A titled card. Everything in the body sits on a raised surface.
pub fn card<R>(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            // Every card spans the column, otherwise each one shrinks to its
            // own content and the stack looks ragged.
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(
                    egui::RichText::new(title.to_uppercase())
                        .color(MUTED)
                        .size(11.0)
                        .strong(),
                );
                ui.add_space(8.0);
            }
            body(ui)
        })
        .inner
}

/// A small status pill, for connected/mode indicators.
pub fn pill(ui: &mut egui::Ui, text: &str, colour: Color32) {
    egui::Frame::new()
        .fill(colour.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, colour.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(99))
        .inner_margin(egui::Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(colour).size(12.0).strong());
        });
}

/// Key/value row, label muted and right-aligned against a fixed gutter.
pub fn row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2(96.0, 18.0),
            egui::Label::new(egui::RichText::new(key).color(MUTED).size(12.5)),
        );
        ui.add(egui::Label::new(egui::RichText::new(value).size(13.5)).wrap());
    });
}

/// Helper text under a control.
pub fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(egui::RichText::new(text).color(MUTED).size(12.0))
            .wrap(),
    );
}

/// A clickable colour chip.
pub fn swatch(ui: &mut egui::Ui, rgb: [u8; 3], selected: bool) -> egui::Response {
    let size = egui::vec2(26.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), c);
    let stroke = if selected {
        Stroke::new(2.0, TEXT)
    } else if resp.hovered() {
        Stroke::new(1.5, MUTED)
    } else {
        Stroke::new(1.0, LINE)
    };
    painter.rect_stroke(rect, CornerRadius::same(6), stroke, egui::StrokeKind::Inside);
    resp
}
