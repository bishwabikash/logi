//! Visual design.
//!
//! The subject is a piece of configurable hardware, so the interface is built
//! like an instrument panel: a readout first, controls under it, and chrome
//! only where something is genuinely a separate object.
//!
//! Neutrals carry a slight blue-violet bias rather than being pure grey, which
//! is what stops a dark interface reading as a default. The accent is the
//! device's own logo cyan, used only for live values and the current
//! selection; semantic colours for good/warning/bad are deliberately separate
//! from it, so state never competes with selection.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

// Neutrals: graphite with a blue-violet bias (hue ~235), not neutral grey.
pub const BG: Color32 = Color32::from_rgb(0x15, 0x16, 0x1c);
pub const RAISED: Color32 = Color32::from_rgb(0x1b, 0x1d, 0x25);
pub const CONTROL: Color32 = Color32::from_rgb(0x23, 0x26, 0x30);
pub const CONTROL_HI: Color32 = Color32::from_rgb(0x2c, 0x30, 0x3c);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x38);

pub const TEXT: Color32 = Color32::from_rgb(0xe4, 0xe6, 0xed);
pub const DIM: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xb0);
pub const FAINT: Color32 = Color32::from_rgb(0x6b, 0x71, 0x82);

/// The device's own logo colour. Reserved for the current selection and live
/// values, so it always means "this one" or "right now".
pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0xc8, 0xd4);
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x1e, 0x5c, 0x64);

// Semantic, kept off the accent hue so status never reads as selection.
pub const GOOD: Color32 = Color32::from_rgb(0x72, 0xc2, 0x87);
pub const WARN: Color32 = Color32::from_rgb(0xd8, 0xa5, 0x5c);
pub const BAD: Color32 = Color32::from_rgb(0xdd, 0x7b, 0x7b);

pub fn apply(ctx: &egui::Context) {
    // Pin the preference before writing anything. egui keeps a separate style
    // for dark and for light, and `set_visuals`/`set_style` only write the one
    // belonging to whichever theme is active *now*; the default preference is
    // System. So on a desktop that switches to light mid-session, winit sends
    // ThemeChanged, egui swaps to the other slot, and the whole design — this
    // palette, the type scale, the spacing — is replaced by stock egui. The
    // caller applies this once, so nothing would ever put it back. There is no
    // light variant of this palette, so the honest fix is to ask for dark and
    // stay there.
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(0x10, 0x11, 0x16);
    v.faint_bg_color = RAISED;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT_FILL;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let r = CornerRadius::same(6);
    v.widgets.noninteractive.bg_fill = RAISED;
    v.widgets.noninteractive.weak_bg_fill = RAISED;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.corner_radius = r;

    // Interactive things get a visible fill so they read as operable.
    v.widgets.inactive.bg_fill = CONTROL;
    v.widgets.inactive.weak_bg_fill = CONTROL;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.corner_radius = r;

    v.widgets.hovered.bg_fill = CONTROL_HI;
    v.widgets.hovered.weak_bg_fill = CONTROL_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, FAINT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.corner_radius = r;

    v.widgets.active.bg_fill = ACCENT_FILL;
    v.widgets.active.weak_bg_fill = ACCENT_FILL;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.active.corner_radius = r;

    v.widgets.open.bg_fill = CONTROL;
    v.widgets.open.corner_radius = r;
    ctx.set_visuals(v);

    let mut s = (*ctx.style()).clone();
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Heading, FontId::new(17.0, Proportional)),
        (TextStyle::Body, FontId::new(14.0, Proportional)),
        (TextStyle::Button, FontId::new(13.5, Proportional)),
        (TextStyle::Small, FontId::new(11.5, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, egui::FontFamily::Monospace)),
    ]
    .into();
    s.spacing.item_spacing = egui::vec2(9.0, 8.0);
    s.spacing.button_padding = egui::vec2(11.0, 6.0);
    s.spacing.slider_width = 300.0;
    s.spacing.slider_rail_height = 5.0;
    s.spacing.combo_width = 150.0;
    s.spacing.interact_size.y = 27.0;
    s.visuals.indent_has_left_vline = false;
    ctx.set_style(s);
}

/// A section: a small label, a hairline, then content.
///
/// Deliberately not a card. Boxing every group in the same border and radius
/// makes a page of equal-weight objects with no hierarchy; the eye needs
/// somewhere to land first.
pub fn section<R>(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(label.to_uppercase())
            .color(FAINT)
            .size(10.5)
            .strong(),
    );
    ui.add_space(5.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(
        r.x_range(),
        ui.cursor().top() - 2.0,
        Stroke::new(1.0_f32, LINE),
    );
    ui.add_space(9.0);
    let out = body(ui);
    ui.add_space(18.0);
    out
}

/// A bordered card. Reserved for something that really is a discrete object,
/// so the border still means something when it appears.
pub fn card<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(RAISED)
        .stroke(Stroke::new(1.0_f32, LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

/// The headline figure for a panel: large, tabular, with its unit set small
/// and dim so the number itself carries.
pub fn readout(ui: &mut egui::Ui, value: &str, unit: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(value)
                .size(32.0)
                .strong()
                .color(ACCENT)
                .monospace(),
        );
        ui.add_space(2.0);
        ui.label(egui::RichText::new(unit).size(12.0).color(DIM));
    });
}

/// A secondary figure. Smaller than `readout`, so one number per panel leads.
pub fn value(ui: &mut egui::Ui, value: &str, unit: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(value)
                .size(19.0)
                .strong()
                .color(TEXT)
                .monospace(),
        );
        ui.add_space(2.0);
        ui.label(egui::RichText::new(unit).size(11.5).color(DIM));
    });
}

pub fn pill(ui: &mut egui::Ui, text: &str, colour: Color32) {
    egui::Frame::new()
        .fill(colour.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0_f32, colour.gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(99))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(colour).size(11.0).strong());
        });
}

pub fn row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2(104.0, 17.0),
            egui::Label::new(egui::RichText::new(key).color(DIM).size(12.0)),
        );
        ui.add(egui::Label::new(egui::RichText::new(value).size(13.0)).wrap());
    });
}

pub fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(DIM).size(11.5)).wrap());
}

/// Shown where a control would be if the device supported the feature.
pub fn unsupported(ui: &mut egui::Ui, what: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(format!("This device does not report {what}."))
                .color(FAINT)
                .size(12.0)
                .italics(),
        )
        .wrap(),
    );
}

pub fn swatch(ui: &mut egui::Ui, rgb: [u8; 3], selected: bool) -> egui::Response {
    let size = egui::vec2(24.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(5), c);
    let stroke = if selected {
        Stroke::new(2.0_f32, TEXT)
    } else if resp.hovered() {
        Stroke::new(1.5_f32, DIM)
    } else {
        Stroke::new(1.0_f32, LINE)
    };
    p.rect_stroke(rect, CornerRadius::same(5), stroke, egui::StrokeKind::Inside);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egui keeps one style for dark and one for light and swaps the active
    /// one when the desktop theme changes. `apply` runs once, off the first
    /// paint, so if it wrote only the slot that happened to be active, a
    /// desktop switching to light mid-session would replace this palette, the
    /// type scale and the spacing with stock egui and nothing would put them
    /// back.
    #[test]
    fn the_design_survives_a_system_theme_switch() {
        let ctx = egui::Context::default();
        apply(&ctx);
        assert_eq!(ctx.style().visuals.panel_fill, BG);

        // Exactly what winit reports on a switch to light.
        let input = egui::RawInput {
            system_theme: Some(egui::Theme::Light),
            ..Default::default()
        };
        let _ = ctx.run(input, |_| {});

        assert_eq!(
            ctx.style().visuals.panel_fill,
            BG,
            "a light system theme dropped the design"
        );
        assert_eq!(
            ctx.style().text_styles[&egui::TextStyle::Body].size,
            14.0,
            "the type scale went with it"
        );
        assert_eq!(ctx.style().visuals.override_text_color, Some(TEXT));
    }

    /// The accent means "this one" or "right now", so status must never be
    /// drawn in it by accident.
    #[test]
    fn status_colours_stay_off_the_accent() {
        for c in [GOOD, WARN, BAD] {
            assert_ne!(c, ACCENT);
            assert_ne!(c, ACCENT_FILL);
        }
    }

    #[test]
    fn a_section_hands_back_what_its_body_returns() {
        let got: std::cell::RefCell<Option<i32>> = std::cell::RefCell::new(None);
        egui::__run_test_ui(|ui| {
            let v = section(ui, "sensor", |ui| {
                hint(ui, "a line");
                41 + 1
            });
            *got.borrow_mut() = Some(v);
        });
        assert_eq!(*got.borrow(), Some(42));
    }

    #[test]
    fn a_swatch_takes_the_space_it_claims_and_reports_no_click() {
        let out: std::cell::RefCell<Option<(egui::Vec2, bool)>> =
            std::cell::RefCell::new(None);
        egui::__run_test_ui(|ui| {
            let r = swatch(ui, [0x2d, 0xd4, 0xd4], true);
            *out.borrow_mut() = Some((r.rect.size(), r.clicked()));
        });
        // Painting is not clicking: a swatch that reported a click would
        // rewrite the chosen colour with no pointer anywhere near it.
        assert_eq!(*out.borrow(), Some((egui::vec2(24.0, 24.0), false)));
    }
}
