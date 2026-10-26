//! Editing one onboard profile from the GUI.
//!
//! Every control here edits a staged copy held in memory. Nothing reaches the
//! mouse until the user presses Save to mouse, because writing a profile is a
//! flash write with finite endurance: a write per keystroke would both wear
//! the sector out and leave half-built profiles on the device while the user
//! is still typing.
//!
//! Two other rules are load-bearing rather than stylistic:
//!
//!   * A profile is always built by cloning the one that was read off the
//!     device, so `Profile::serialize` can pass through the bytes this codec
//!     does not model. Building one from nothing would zero them.
//!   * A profile with no left click leaves a mouse that cannot be clicked,
//!     which has happened on this project. Save refuses such a profile.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;

use super::theme;
use crate::device::{Cmd, Snapshot};
use crate::dpi::Capability;
use crate::led;
use crate::profile_blob::{
    Button, Led, Profile, BTN_HID, HID_MOUSE, LED_RECORD, NAME_LEN, N_DPI, SPECIALS,
};
use crate::rate::ReportRate;

/// The name field is 48 bytes of UTF-16LE, so 24 code units.
pub const NAME_UNITS: usize = NAME_LEN / 2;

/// How long the "writing to flash" marker stays up before giving up on seeing
/// the device catch up. The worker re-reads every sector after a write, which
/// takes about a second.
const WRITE_SETTLE: Duration = Duration::from_secs(4);

/// Mouse buttons this editor offers. The report has sixteen bits, but a
/// profile binding past the fifth physical button is not something a user can
/// press without remapping the mouse first.
const MOUSE_BUTTONS: std::ops::RangeInclusive<u8> = 1..=5;

/// Lighting modes storable in a profile sector. The flash path takes mode ids
/// rather than the live path's effect indices; `Led` handles that difference,
/// so these are `led` mode ids and go through the `Led` constructors.
const LED_MODES: &[u8] = &[led::OFF, led::FIXED, led::BREATHING, led::CYCLE];

const PALETTE: &[[u8; 3]] = &[
    [0x2d, 0xd4, 0xd4], [0xff, 0xff, 0xff], [0xe8, 0x3b, 0x3b], [0xff, 0x7a, 0x1f],
    [0xff, 0xd4, 0x2a], [0x5a, 0xd1, 0x5a], [0x3b, 0x8c, 0xff], [0x9b, 0x5a, 0xff],
    [0xff, 0x4d, 0x9e],
];

/// The stock layout for this mouse family, used by "Reset to stock layout".
/// Buttons 0-4 are the physical mouse buttons; the rest match what the device
/// ships with.
const STOCK_SPECIALS: &[&str] = &[
    "dpi-shift", "dpi-down", "dpi-up", "profile-cycle", "wheel-right", "wheel-left",
];

/// Truncate a name to what the 48-byte UTF-16LE field holds.
///
/// Counted in UTF-16 code units, not chars: an emoji is a surrogate pair and
/// costs two of the twenty-four.
pub fn truncate_name(s: &str) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let n = c.len_utf16();
        if used + n > NAME_UNITS {
            break;
        }
        used += n;
        out.push(c);
    }
    out
}

/// UTF-16 code units a name occupies, for the counter next to the field.
pub fn name_units(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Snap a DPI stage to a value the sensor can actually represent.
///
/// 0 means "stage disabled" and is left alone. With no reported capability the
/// value is passed through: inventing a step for an unknown sensor would be a
/// guess, and the device snaps on its own anyway.
pub fn snap_stage(cap: Option<&Capability>, v: u16) -> u16 {
    if v == 0 {
        return 0;
    }
    match cap {
        Some(Capability::Range { min, max, step }) => {
            let step = (*step).max(1);
            let v = v.clamp(*min, *max);
            let steps = ((v - min) as f32 / step as f32).round() as u16;
            (min + steps * step).min(*max)
        }
        Some(Capability::List(list)) if !list.is_empty() => *list
            .iter()
            .min_by_key(|c| (**c as i32 - v as i32).abs())
            .unwrap_or(&v),
        _ => v,
    }
}

/// Report rates to offer, in Hz. The device's own list when we have it.
fn rate_options(snap: &Snapshot, current_ms: u8) -> Vec<u16> {
    let mut v = if snap.rates.is_empty() {
        vec![125, 250, 500, 1000]
    } else {
        snap.rates.clone()
    };
    // Never hide the value already stored: the user has to be able to see
    // where the profile currently sits even if the list disagrees.
    if current_ms != 0 {
        let hz = 1000 / current_ms as u16;
        if !v.contains(&hz) {
            v.push(hz);
        }
    }
    v.sort_unstable();
    v
}

/// True if this binding presses the left mouse button.
fn is_left_click(b: &Button) -> bool {
    b.0[0] == BTN_HID
        && b.0[1] == HID_MOUSE
        && u16::from_be_bytes([b.0[2], b.0[3]]) & 0x0001 != 0
}

/// A binding as this editor can express it. `Other` covers keys, consumer
/// controls and macros, which are readable but not editable here, so picking
/// it leaves the bytes exactly as the device had them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    Disabled,
    Mouse(u8),
    Special(u8),
    Other,
}

impl Bind {
    fn of(b: &Button) -> Bind {
        match b.kind() {
            "mouse" => {
                let mask = u16::from_be_bytes([b.0[2], b.0[3]]);
                if mask != 0 && mask & (mask - 1) == 0 {
                    let n = (mask.trailing_zeros() + 1) as u8;
                    if MOUSE_BUTTONS.contains(&n) {
                        return Bind::Mouse(n);
                    }
                }
                Bind::Other
            }
            "special" => {
                if SPECIALS.iter().any(|(c, _)| *c == b.0[1]) {
                    Bind::Special(b.0[1])
                } else {
                    Bind::Other
                }
            }
            "disabled" | "unused" => Bind::Disabled,
            _ => Bind::Other,
        }
    }

    /// `None` for `Other`: there is nothing to build, the original stands.
    fn to_button(self) -> Option<Button> {
        match self {
            Bind::Disabled => Some(Button::disabled()),
            Bind::Mouse(n) => Some(Button::mouse(n)),
            Bind::Special(c) => crate::profile_blob::special_name(c).and_then(Button::special),
            Bind::Other => None,
        }
    }

    fn label(self, original: &Button) -> String {
        match self {
            Bind::Disabled => "disabled".into(),
            Bind::Mouse(n) => format!("button {n}"),
            Bind::Special(c) => crate::profile_blob::special_name(c)
                .map(str::to_string)
                .unwrap_or_else(|| format!("special-0x{c:02x}")),
            Bind::Other => format!("{} (leave as is)", original.describe()),
        }
    }
}

/// One zone's lighting, in editable form.
///
/// The record as read is kept and handed straight back while `edited` is
/// false, so a mode this editor does not model — wave, starlight — survives a
/// save of some other field untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct LedDraft {
    pub mode: u8,
    pub color: [u8; 3],
    pub period_ms: u16,
    pub waveform: u8,
    pub intensity: u8,
    raw: [u8; LED_RECORD],
    edited: bool,
}

impl LedDraft {
    pub fn from_led(l: &Led) -> Self {
        let r = l.0;
        let mut d = LedDraft {
            mode: r[0],
            color: [r[1], r[2], r[3]],
            period_ms: 4000,
            waveform: 0,
            intensity: 100,
            raw: r,
            edited: false,
        };
        match r[0] {
            led::BREATHING => {
                d.period_ms = u16::from_be_bytes([r[4], r[5]]);
                d.waveform = r[6];
                // The record stores 0 for full intensity.
                d.intensity = if r[7] == 0 { 100 } else { r[7] };
            }
            led::CYCLE => {
                d.period_ms = u16::from_be_bytes([r[6], r[7]]);
                d.intensity = if r[8] == 0 { 100 } else { r[8] };
            }
            _ => {}
        }
        if d.period_ms == 0 {
            d.period_ms = 4000;
        }
        d
    }

    pub fn to_led(&self) -> Led {
        if !self.edited {
            return Led(self.raw);
        }
        match self.mode {
            led::OFF => Led::off(),
            led::FIXED => Led::fixed(self.color),
            led::BREATHING => {
                Led::breathing(self.color, self.period_ms, self.waveform, self.intensity)
            }
            led::CYCLE => Led::cycle(self.period_ms, self.intensity),
            // A mode with no constructor is left exactly as it was read.
            _ => Led(self.raw),
        }
    }

    fn mark(&mut self) {
        self.edited = true;
    }
}

/// Staged edits to one profile. Compared field by field against the profile
/// read off the device to decide whether anything is unsaved.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub name: String,
    pub rate_ms: u8,
    pub dpi: [u16; N_DPI],
    pub default_dpi: u8,
    pub shift_dpi: u8,
    pub leds: [LedDraft; 2],
    /// The second-mode records as read. Mirrored from `leds` only for a zone
    /// the user actually edited.
    alt_raw: [Led; 2],
    pub buttons: Vec<Button>,
    /// How many of `buttons` the device actually has.
    editable: usize,
}

impl Draft {
    pub fn from_profile(p: &Profile, editable: usize) -> Self {
        Draft {
            name: p.name.clone(),
            rate_ms: p.report_rate_ms,
            dpi: p.dpi,
            default_dpi: p.default_dpi,
            shift_dpi: p.shift_dpi,
            leds: [LedDraft::from_led(&p.leds[0]), LedDraft::from_led(&p.leds[1])],
            alt_raw: p.alt_leds,
            buttons: p.buttons.clone(),
            editable: editable.min(p.buttons.len()),
        }
    }

    /// Build the profile to write, starting from the one read off the device
    /// so unmodelled bytes are preserved.
    pub fn build(&self, original: &Profile) -> Profile {
        let mut p = original.clone();
        p.name = truncate_name(&self.name);
        p.report_rate_ms = self.rate_ms;
        p.dpi = self.dpi;
        p.default_dpi = self.default_dpi;
        p.shift_dpi = self.shift_dpi;
        for (i, l) in self.leds.iter().enumerate() {
            p.leds[i] = l.to_led();
            // The second-mode record gets the same value, so holding the
            // sniper button does not change the lighting. Same thing the
            // Lighting tab's Save to mouse does.
            p.alt_leds[i] = if l.edited { l.to_led() } else { self.alt_raw[i] };
        }
        p.buttons = self.buttons.clone();
        p
    }

    pub fn report_rate_hz(&self) -> u16 {
        if self.rate_ms == 0 { 0 } else { 1000 / self.rate_ms as u16 }
    }

    pub fn enabled_stages(&self) -> Vec<usize> {
        (0..N_DPI).filter(|i| self.dpi[*i] != 0).collect()
    }

    /// Keep the default and shift indices pointing at a stage that exists.
    /// A disabled stage as the default is how a profile ends up booting at
    /// 0 DPI.
    pub fn fix_indices(&mut self) {
        let on = self.enabled_stages();
        let Some(&first) = on.first() else { return };
        if !on.contains(&(self.default_dpi as usize)) {
            self.default_dpi = first as u8;
        }
        if !on.contains(&(self.shift_dpi as usize)) {
            self.shift_dpi = first as u8;
        }
    }

    /// A profile with no left click leaves a mouse that cannot click.
    pub fn has_left_click(&self) -> bool {
        self.buttons.iter().take(self.editable).any(is_left_click)
    }

    fn set_bind(&mut self, i: usize, b: Bind) {
        if let Some(button) = b.to_button() {
            if let Some(slot) = self.buttons.get_mut(i) {
                *slot = button;
            }
        }
    }

    /// Everything except the name, which stays so two copies of a profile are
    /// still tellable apart.
    fn copy_settings_from(&mut self, other: &Draft) {
        self.rate_ms = other.rate_ms;
        self.dpi = other.dpi;
        self.default_dpi = other.default_dpi;
        self.shift_dpi = other.shift_dpi;
        for (i, l) in other.leds.iter().enumerate() {
            let mut l = l.clone();
            l.mark();
            self.leds[i] = l;
        }
        let n = self.editable.min(other.buttons.len());
        for i in 0..n {
            self.buttons[i] = other.buttons[i];
        }
        self.fix_indices();
    }

    fn reset_stock_buttons(&mut self) {
        for i in 0..self.editable {
            let b = if let Some(n) = u8::try_from(i + 1).ok().filter(|n| MOUSE_BUTTONS.contains(n)) {
                Button::mouse(n)
            } else {
                match STOCK_SPECIALS.get(i - 5).and_then(|n| Button::special(n)) {
                    Some(b) => b,
                    None => continue,
                }
            };
            self.buttons[i] = b;
        }
    }
}

#[derive(Debug, Clone)]
struct Staged {
    draft: Draft,
    /// The profile as last read off the device, to compare against.
    base: Draft,
}

impl Staged {
    fn new(d: Draft) -> Self {
        Staged { base: d.clone(), draft: d }
    }

    fn dirty(&self) -> bool {
        self.draft != self.base
    }
}

/// In-app profile editing. One instance holds the staged edits for every
/// profile the user has touched.
pub struct ProfileEditor {
    staged: HashMap<u8, Staged>,
    backup_path: String,
    /// Set once a backup has been asked for this session, so the offer stops
    /// after it has been taken.
    backup_asked: bool,
    copy_from: u8,
    /// Index and time of the last write, so the UI can say a write is in
    /// flight while the worker re-reads the sectors.
    writing: Option<(u8, Instant)>,
}

impl Default for ProfileEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfileEditor {
    pub fn new() -> Self {
        ProfileEditor {
            staged: HashMap::new(),
            backup_path: default_backup_path(),
            backup_asked: false,
            copy_from: 0,
            writing: None,
        }
    }

    /// True when any profile has unsaved edits, so the host can warn on exit.
    pub fn dirty(&self) -> bool {
        self.staged.values().any(Staged::dirty)
    }

    /// Which profiles have unsaved edits, for a more specific warning.
    pub fn dirty_profiles(&self) -> Vec<u8> {
        let mut v: Vec<u8> =
            self.staged.iter().filter(|(_, s)| s.dirty()).map(|(i, _)| *i).collect();
        v.sort_unstable();
        v
    }

    pub fn discard(&mut self) {
        self.staged.clear();
    }

    /// Draw the editor for one profile. Returns the commands to dispatch.
    pub fn ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot, index: u8) -> Vec<Cmd> {
        let mut cmds: Vec<Cmd> = Vec::new();

        if !snap.has_profiles {
            theme::unsupported(ui, "onboard profiles, so there is nothing to edit");
            return cmds;
        }
        // Writing a layout this codec cannot decode would corrupt it, so the
        // editor offers nothing at all rather than a control that errors.
        if snap.profile_format != 2 {
            theme::unsupported(ui, "a profile layout this app can decode");
            ui.add_space(6.0);
            theme::hint(
                ui,
                &format!(
                    "The mouse reports profile format {}, and this app only knows format 2. \
                     Switching profiles still works; editing is off because writing a \
                     layout it cannot decode would corrupt the profile.",
                    snap.profile_format
                ),
            );
            return cmds;
        }
        let Some(original) = snap.profiles.get(index as usize) else {
            theme::hint(
                ui,
                "This profile has not been read off the mouse yet. Press Refresh in the \
                 sidebar; reading all five sectors takes about a second.",
            );
            return cmds;
        };

        let n_buttons = (snap.button_count as usize).min(original.buttons.len());
        let base = Draft::from_profile(original, n_buttons);
        let mut st = match self.staged.remove(&index) {
            // Nothing staged and the device has moved on: adopt the reading.
            Some(s) if !s.dirty() && s.base != base => Staged::new(base.clone()),
            Some(s) => s,
            None => Staged::new(base.clone()),
        };

        // The write marker clears itself; a failed write shows up as an error
        // in the status bar, which the host owns.
        if self.writing.map(|(_, t)| t.elapsed() > WRITE_SETTLE).unwrap_or(false) {
            self.writing = None;
        }
        let writing = self.writing.map(|(i, _)| i) == Some(index);

        self.draw_header(ui, &st, index, writing);
        self.draw_identity(ui, &mut st.draft, &base);
        self.draw_rate(ui, &mut st.draft, snap);
        self.draw_dpi(ui, &mut st.draft, snap);
        self.draw_lighting(ui, &mut st.draft, snap);
        self.draw_buttons(ui, &mut st.draft, &base, n_buttons);
        self.draw_copy_from(ui, &mut st.draft, snap, index, n_buttons);
        let committed = self.draw_save(ui, &st, original, index, &mut cmds);

        // A committed profile drops out of the staging map: the device is the
        // source of truth, and the worker re-reads every sector after a write.
        if !committed && (st.dirty() || st.base != base) {
            self.staged.insert(index, st);
        }
        cmds
    }

    fn draw_header(&self, ui: &mut egui::Ui, st: &Staged, index: u8, writing: bool) {
        let lead = st.draft.dpi.get(st.draft.default_dpi as usize).copied().unwrap_or(0);
        theme::readout(ui, &lead.to_string(), "DPI on this profile");
        ui.add_space(2.0);
        theme::value(ui, &st.draft.report_rate_hz().to_string(), "Hz report rate");
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let name = if st.draft.name.is_empty() {
                format!("Profile {index}")
            } else {
                st.draft.name.clone()
            };
            ui.label(egui::RichText::new(name).size(15.0).strong());
            if st.dirty() {
                theme::pill(ui, "unsaved changes", theme::WARN);
            }
            if writing {
                theme::pill(ui, "writing to flash", theme::WARN);
            }
        });
        ui.add_space(14.0);
    }

    fn draw_identity(&self, ui: &mut egui::Ui, d: &mut Draft, base: &Draft) {
        theme::section(ui, "Name", |ui| {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 70.0).max(160.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut d.name)
                        .desired_width(w)
                        .hint_text("unnamed"),
                );
                if resp.changed() {
                    // Enforced here rather than at save, so the field cannot
                    // show text that will not survive the write.
                    d.name = truncate_name(&d.name);
                }
                let used = name_units(&d.name);
                let c = if used >= NAME_UNITS { theme::WARN } else { theme::DIM };
                ui.label(
                    egui::RichText::new(format!("{used}/{NAME_UNITS}")).color(c).size(12.0),
                );
            });
            ui.add_space(6.0);
            theme::hint(
                ui,
                "Stored in the profile as 48 bytes of UTF-16, which is 24 characters. \
                 Windows shows this name too.",
            );
            if d.name != base.name {
                ui.add_space(4.0);
                theme::hint(ui, &format!("Was: {}", display_name(&base.name)));
            }
        });
    }

    fn draw_rate(&self, ui: &mut egui::Ui, d: &mut Draft, snap: &Snapshot) {
        theme::section(ui, "Report rate", |ui| {
            let options = rate_options(snap, d.rate_ms);
            let current = d.report_rate_hz();
            ui.horizontal_wrapped(|ui| {
                for hz in options {
                    if ui.selectable_label(current == hz, format!("{hz} Hz")).clicked() {
                        // interval_ms refuses a rate that is not a whole
                        // number of milliseconds, which the sector cannot
                        // store; such a value is simply not offered.
                        if let Ok(ms) = ReportRate::interval_ms(hz) {
                            d.rate_ms = ms;
                        }
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Byte 0 of the profile is the polling interval in milliseconds, so the \
                 rate holds with nothing running. The live setter is refused in onboard \
                 mode.",
            );
        });
    }

    fn draw_dpi(&self, ui: &mut egui::Ui, d: &mut Draft, snap: &Snapshot) {
        let cap = snap.dpi_cap.clone();
        theme::section(ui, "DPI stages", |ui| {
            let (min, max, step) = match &cap {
                Some(Capability::Range { min, max, step }) => (*min, *max, *step),
                Some(Capability::List(v)) => (
                    v.first().copied().unwrap_or(100),
                    v.last().copied().unwrap_or(25600),
                    50,
                ),
                None => (100, 25600, 50),
            };
            egui::Grid::new("profile-dpi-stages")
                .num_columns(3)
                .spacing(egui::vec2(12.0, 8.0))
                .show(ui, |ui| {
                    for i in 0..N_DPI {
                        let mut on = d.dpi[i] != 0;
                        ui.label(
                            egui::RichText::new(format!("stage {i}"))
                                .color(theme::DIM)
                                .size(12.5),
                        );
                        if ui.checkbox(&mut on, "").changed() {
                            // A disabled stage is stored as 0; re-enabling one
                            // starts from a value the sensor can hit.
                            d.dpi[i] = if on { snap_stage(cap.as_ref(), 1600) } else { 0 };
                            d.fix_indices();
                        }
                        if d.dpi[i] == 0 {
                            ui.label(
                                egui::RichText::new("disabled").color(theme::FAINT).size(12.5),
                            );
                        } else {
                            let mut v = d.dpi[i];
                            let resp = ui.add(
                                egui::Slider::new(&mut v, min..=max)
                                    .step_by(step as f64)
                                    .suffix(" DPI"),
                            );
                            if resp.changed() {
                                d.dpi[i] = snap_stage(cap.as_ref(), v);
                            }
                        }
                        ui.end_row();
                    }
                });

            ui.add_space(10.0);
            let on = d.enabled_stages();
            if on.is_empty() {
                ui.label(
                    egui::RichText::new("Every stage is disabled.")
                        .color(theme::BAD)
                        .size(12.5),
                );
                ui.add_space(4.0);
                theme::hint(ui, "Enable at least one stage: the profile needs a DPI to use.");
            } else {
                theme::row(ui, "startup stage", "");
                ui.horizontal_wrapped(|ui| {
                    for i in &on {
                        let label = format!("{} ({})", d.dpi[*i], i);
                        if ui
                            .selectable_label(d.default_dpi as usize == *i, label)
                            .clicked()
                        {
                            d.default_dpi = *i as u8;
                        }
                    }
                });
                ui.add_space(8.0);
                theme::row(ui, "sniper stage", "");
                ui.horizontal_wrapped(|ui| {
                    for i in &on {
                        let label = format!("{} ({})", d.dpi[*i], i);
                        if ui.selectable_label(d.shift_dpi as usize == *i, label).clicked() {
                            d.shift_dpi = *i as u8;
                        }
                    }
                });
                ui.add_space(8.0);
                theme::hint(
                    ui,
                    "The startup stage is what the profile applies when it loads, and what \
                     dpi-default returns to. The sniper stage is where a button bound to \
                     dpi-shift takes the mouse while it is held.",
                );
            }
            ui.add_space(6.0);
            match &cap {
                Some(Capability::Range { min, max, step }) => theme::hint(
                    ui,
                    &format!("Snapped to the sensor's own steps: {min}-{max} by {step}."),
                ),
                Some(Capability::List(_)) => {
                    theme::hint(ui, "Snapped to one of the values the sensor reports.")
                }
                None => theme::hint(
                    ui,
                    "The mouse has not reported its DPI steps, so values are stored as \
                     typed and the sensor will round them itself.",
                ),
            }
        });
    }

    fn draw_lighting(&self, ui: &mut egui::Ui, d: &mut Draft, snap: &Snapshot) {
        theme::section(ui, "Lighting", |ui| {
            for z in 0..2usize {
                let label = snap
                    .zones
                    .get(z)
                    .map(|zi| zi.location.clone())
                    .unwrap_or_else(|| format!("zone {z}"));
                ui.label(egui::RichText::new(label).size(13.5).strong());
                ui.add_space(6.0);

                let led_draft = &mut d.leds[z];
                ui.horizontal_wrapped(|ui| {
                    for m in LED_MODES {
                        if ui
                            .selectable_label(led_draft.mode == *m, led::mode_name(*m))
                            .clicked()
                        {
                            led_draft.mode = *m;
                            led_draft.mark();
                        }
                    }
                });

                if led_draft.mode == led::FIXED || led_draft.mode == led::BREATHING {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        for c in PALETTE {
                            if theme::swatch(ui, *c, led_draft.color == *c).clicked() {
                                led_draft.color = *c;
                                led_draft.mark();
                            }
                        }
                        ui.add_space(6.0);
                        let mut rgb = [
                            led_draft.color[0] as f32 / 255.0,
                            led_draft.color[1] as f32 / 255.0,
                            led_draft.color[2] as f32 / 255.0,
                        ];
                        if ui.color_edit_button_rgb(&mut rgb).changed() {
                            led_draft.color = [
                                (rgb[0] * 255.0).round() as u8,
                                (rgb[1] * 255.0).round() as u8,
                                (rgb[2] * 255.0).round() as u8,
                            ];
                            led_draft.mark();
                        }
                    });
                }
                if led_draft.mode == led::BREATHING || led_draft.mode == led::CYCLE {
                    ui.add_space(8.0);
                    let mut period = led_draft.period_ms;
                    if ui
                        .add(
                            egui::Slider::new(&mut period, 500..=20000)
                                .suffix(" ms")
                                .text("period"),
                        )
                        .changed()
                    {
                        led_draft.period_ms = period;
                        led_draft.mark();
                    }
                    let mut inten = led_draft.intensity;
                    if ui
                        .add(
                            egui::Slider::new(&mut inten, 1..=100)
                                .suffix(" %")
                                .text("intensity"),
                        )
                        .changed()
                    {
                        led_draft.intensity = inten;
                        led_draft.mark();
                    }
                }
                if led_draft.mode == led::BREATHING {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("waveform").color(theme::DIM).size(12.5));
                        let current = led::waveform_name(led_draft.waveform);
                        egui::ComboBox::from_id_salt(("profile-wf", z))
                            .selected_text(current)
                            .show_ui(ui, |ui| {
                                for (n, v) in led::WAVEFORMS {
                                    if ui
                                        .selectable_label(led_draft.waveform == *v, *n)
                                        .clicked()
                                    {
                                        led_draft.waveform = *v;
                                        led_draft.mark();
                                    }
                                }
                            });
                    });
                }
                if !LED_MODES.contains(&led_draft.mode) {
                    ui.add_space(6.0);
                    theme::hint(
                        ui,
                        &format!(
                            "Stored as mode {} ({}), which this editor does not build. It is \
                             left untouched unless you pick a mode above.",
                            led_draft.mode,
                            led::mode_name(led_draft.mode)
                        ),
                    );
                }
                ui.add_space(14.0);
            }
            theme::hint(
                ui,
                "Stored in the profile, so the mouse runs it with nothing running. Cycling \
                 works this way even though the firmware refuses to start it live. Each \
                 zone's second-mode record gets the same value.",
            );
        });
    }

    fn draw_buttons(&self, ui: &mut egui::Ui, d: &mut Draft, base: &Draft, n: usize) {
        theme::section(ui, "Buttons", |ui| {
            if n == 0 {
                theme::hint(ui, "The mouse reports no remappable buttons.");
                return;
            }
            let mut options: Vec<Bind> = vec![Bind::Disabled];
            for m in MOUSE_BUTTONS {
                options.push(Bind::Mouse(m));
            }
            for (c, _) in SPECIALS {
                options.push(Bind::Special(*c));
            }

            egui::Grid::new("profile-buttons")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 8.0))
                .show(ui, |ui| {
                    for i in 0..n {
                        let current = d.buttons[i];
                        let was = base.buttons[i];
                        let cur_bind = Bind::of(&current);
                        ui.label(
                            egui::RichText::new(format!("{i}")).color(theme::DIM).size(12.5),
                        );
                        ui.label(egui::RichText::new(current.describe()).size(13.0));

                        egui::ComboBox::from_id_salt(("profile-btn", i))
                            .selected_text(cur_bind.label(&current))
                            .show_ui(ui, |ui| {
                                // Only offered when that is what the device
                                // has: there is no "other" to build.
                                if cur_bind == Bind::Other
                                    && ui
                                        .selectable_label(true, Bind::Other.label(&current))
                                        .clicked()
                                {
                                    // Nothing to do; the bytes already stand.
                                }
                                for o in &options {
                                    if ui
                                        .selectable_label(*o == cur_bind, o.label(&current))
                                        .clicked()
                                    {
                                        d.set_bind(i, *o);
                                    }
                                }
                            });

                        if current != was {
                            theme::pill(ui, &format!("was {}", was.describe()), theme::ACCENT);
                        } else {
                            ui.label("");
                        }
                        ui.end_row();
                    }
                });

            ui.add_space(10.0);
            if !d.has_left_click() {
                ui.label(
                    egui::RichText::new("No button is bound to left click.")
                        .color(theme::BAD)
                        .size(13.0)
                        .strong(),
                );
                ui.add_space(4.0);
                theme::hint(
                    ui,
                    "Bind \"button 1\" to one of the buttons above. Saving is blocked until \
                     then: a profile with no left click leaves a mouse that cannot click.",
                );
                ui.add_space(8.0);
            }
            if ui.button("Reset to stock layout").clicked() {
                d.reset_stock_buttons();
            }
            ui.add_space(6.0);
            theme::hint(
                ui,
                "Stock layout is buttons 1-5 on the five mouse buttons, then sniper, DPI \
                 down, DPI up, profile cycle and the two wheel tilts.",
            );
        });
    }

    fn draw_copy_from(
        &mut self,
        ui: &mut egui::Ui,
        d: &mut Draft,
        snap: &Snapshot,
        index: u8,
        n_buttons: usize,
    ) {
        let others: Vec<u8> = (0..snap.profiles.len() as u8).filter(|i| *i != index).collect();
        if others.is_empty() {
            return;
        }
        theme::section(ui, "Copy from another profile", |ui| {
            if !others.contains(&self.copy_from) {
                self.copy_from = others[0];
            }
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("profile-copy-src")
                    .selected_text(profile_label(snap, self.copy_from))
                    .show_ui(ui, |ui| {
                        for o in &others {
                            ui.selectable_value(&mut self.copy_from, *o, profile_label(snap, *o));
                        }
                    });
                if ui.button("Copy into this draft").clicked() {
                    if let Some(src) = snap.profiles.get(self.copy_from as usize) {
                        d.copy_settings_from(&Draft::from_profile(src, n_buttons));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Copies the report rate, DPI stages, lighting and buttons into the staged \
                 edits here. The name is left alone, and nothing is written until you save.",
            );
        });
    }

    /// Returns true if a write was dispatched for this profile.
    fn draw_save(
        &mut self,
        ui: &mut egui::Ui,
        st: &Staged,
        original: &Profile,
        index: u8,
        cmds: &mut Vec<Cmd>,
    ) -> bool {
        let dirty = st.dirty();
        let blocked = !st.draft.has_left_click() || st.draft.enabled_stages().is_empty();
        let mut committed = false;

        theme::section(ui, "Save", |ui| {
            if !self.backup_asked {
                theme::hint(
                    ui,
                    "Take a backup before the first write of this session. It dumps every \
                     sector, the profile directory included, so a bad profile can be put \
                     back.",
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 130.0).max(160.0);
                    ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                    if ui.button("Save backup").clicked() {
                        cmds.push(Cmd::Backup(self.backup_path.clone()));
                        self.backup_asked = true;
                    }
                });
                ui.add_space(12.0);
            }

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(dirty && !blocked, egui::Button::new("Save to mouse"))
                    .clicked()
                {
                    cmds.push(Cmd::WriteProfile {
                        index,
                        profile: Box::new(st.draft.build(original)),
                    });
                    self.writing = Some((index, Instant::now()));
                    committed = true;
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                    committed = true; // dropping the staged entry is the revert
                }
            });

            ui.add_space(8.0);
            if !dirty {
                theme::hint(ui, "This profile matches what is on the mouse.");
            } else if blocked {
                theme::hint(
                    ui,
                    "Fix the problem flagged above before saving. Nothing has been written.",
                );
            } else {
                theme::hint(
                    ui,
                    &format!(
                        "One flash write to profile {index}. Flash has finite endurance, so \
                         edits are collected here and written once, not as you type."
                    ),
                );
            }
        });
        committed
    }
}

fn display_name(n: &str) -> String {
    if n.is_empty() { "(unset)".into() } else { n.to_string() }
}

fn profile_label(snap: &Snapshot, i: u8) -> String {
    match snap.profiles.get(i as usize) {
        Some(p) if !p.name.is_empty() => format!("{i} - {}", p.name),
        _ => format!("profile {i}"),
    }
}

/// Where a backup lands unless the user says otherwise.
pub fn default_backup_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/logi-backup.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_blob::{OFF_BUTTONS, OFF_CRC, OFF_LEDS, SECTOR_SIZE};

    /// A format-2 sector shaped like the one this device actually stores.
    fn fixture() -> [u8; SECTOR_SIZE] {
        let mut b = [0xFFu8; SECTOR_SIZE];
        b[0x00] = 0x01; // 1000 Hz
        b[0x01] = 0x01; // default stage 1
        b[0x02] = 0x00; // sniper stage 0
        b[0x03..0x0d].copy_from_slice(&[
            0x20, 0x03, 0x40, 0x06, 0x80, 0x0c, 0x00, 0x19, 0x00, 0x00,
        ]); // 800, 1600, 3200, 6400, disabled
        let btns: [[u8; 4]; 11] = [
            [0x80, 0x01, 0x00, 0x01], [0x80, 0x01, 0x00, 0x02],
            [0x80, 0x01, 0x00, 0x04], [0x80, 0x01, 0x00, 0x08],
            [0x80, 0x01, 0x00, 0x10], [0x90, 0x07, 0xff, 0xff],
            [0x90, 0x04, 0xff, 0xff], [0x90, 0x03, 0xff, 0xff],
            [0x90, 0x0a, 0xff, 0xff], [0x90, 0x02, 0xff, 0xff],
            [0x90, 0x01, 0xff, 0xff],
        ];
        for (i, bt) in btns.iter().enumerate() {
            b[OFF_BUTTONS + i * 4..OFF_BUTTONS + i * 4 + 4].copy_from_slice(bt);
        }
        for i in 0xa0..0xd0 {
            b[i] = 0x00;
        }
        for i in 0..4 {
            let o = OFF_LEDS + i * LED_RECORD;
            b[o..o + LED_RECORD]
                .copy_from_slice(&[0x01, 0x00, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0]);
        }
        b[0xfc] = 0xff;
        b[0xfd] = 0xff;
        let crc = crate::profile_blob::crc_ccitt(&b[..OFF_CRC]);
        b[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_be_bytes());
        b
    }

    fn profile() -> Profile {
        Profile::parse(&fixture()).unwrap()
    }

    fn draft() -> Draft {
        Draft::from_profile(&profile(), 11)
    }

    #[test]
    fn an_untouched_draft_rebuilds_the_sector_byte_for_byte() {
        let p = profile();
        assert_eq!(draft().build(&p).serialize(), fixture());
    }

    #[test]
    fn a_built_profile_round_trips_through_serialize_and_parse() {
        let p = profile();
        let mut d = draft();
        d.name = "Gaming".into();
        d.rate_ms = 2;
        d.dpi[0] = 1200;
        d.leds[0].mode = led::BREATHING;
        d.leds[0].color = [0xff, 0x00, 0x80];
        d.leds[0].period_ms = 3000;
        d.leds[0].waveform = 1;
        d.leds[0].intensity = 60;
        d.leds[0].mark();
        let built = d.build(&p);
        let back = Profile::parse(&built.serialize()).unwrap();
        assert_eq!(back.name, "Gaming");
        assert_eq!(back.report_rate_ms, 2);
        assert_eq!(back.report_rate_hz(), 500);
        assert_eq!(back.dpi, d.dpi);
        assert_eq!(back.default_dpi, d.default_dpi);
        assert_eq!(back.shift_dpi, d.shift_dpi);
        assert_eq!(back.buttons, built.buttons);
        assert_eq!(back.leds[0].describe(), "breathing #ff0080 3000ms sine 60%");
        // The second-mode record follows the edited zone.
        assert_eq!(back.alt_leds[0], back.leds[0]);
        // The untouched zone keeps the bytes that were read.
        assert_eq!(back.leds[1], profile().leds[1]);
        assert!(back.crc_ok());
    }

    #[test]
    fn staging_is_dirty_until_it_is_reverted() {
        let base = draft();
        let mut st = Staged::new(base.clone());
        assert!(!st.dirty());
        st.draft.name = "Changed".into();
        assert!(st.dirty());
        st.draft = st.base.clone();
        assert!(!st.dirty());
    }

    #[test]
    fn the_editor_reports_which_profiles_are_dirty() {
        let mut ed = ProfileEditor::new();
        assert!(!ed.dirty());
        assert!(ed.dirty_profiles().is_empty());
        let mut s = Staged::new(draft());
        s.draft.rate_ms = 8;
        ed.staged.insert(3, s);
        ed.staged.insert(1, Staged::new(draft()));
        assert!(ed.dirty());
        assert_eq!(ed.dirty_profiles(), vec![3]);
        ed.discard();
        assert!(!ed.dirty());
    }

    #[test]
    fn dpi_snapping_follows_the_sensors_step() {
        let cap = Capability::Range { min: 100, max: 25600, step: 50 };
        assert_eq!(snap_stage(Some(&cap), 1234), 1250);
        assert_eq!(snap_stage(Some(&cap), 1225), 1250);
        assert_eq!(snap_stage(Some(&cap), 30000), 25600);
        assert_eq!(snap_stage(Some(&cap), 10), 100);
        let list = Capability::List(vec![400, 800, 1600]);
        assert_eq!(snap_stage(Some(&list), 900), 800);
        assert_eq!(snap_stage(Some(&list), 1500), 1600);
        // No reported capability means no guessing.
        assert_eq!(snap_stage(None, 1234), 1234);
    }

    #[test]
    fn a_disabled_stage_stays_zero() {
        let cap = Capability::Range { min: 100, max: 25600, step: 50 };
        assert_eq!(snap_stage(Some(&cap), 0), 0);
        let p = profile();
        let mut d = draft();
        assert_eq!(d.dpi[4], 0);
        d.dpi[0] = snap_stage(Some(&cap), d.dpi[0]);
        let back = Profile::parse(&d.build(&p).serialize()).unwrap();
        assert_eq!(back.dpi[4], 0);
        assert_eq!(d.enabled_stages(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn disabling_the_startup_stage_repoints_it() {
        let mut d = draft();
        d.default_dpi = 3;
        d.shift_dpi = 3;
        d.dpi[3] = 0;
        d.fix_indices();
        assert_eq!(d.default_dpi, 0);
        assert_eq!(d.shift_dpi, 0);
        // With nothing enabled there is no index to pick, so it is left alone
        // rather than pointing somewhere invented.
        let mut empty = draft();
        empty.dpi = [0; N_DPI];
        empty.default_dpi = 2;
        empty.fix_indices();
        assert_eq!(empty.default_dpi, 2);
        assert!(empty.enabled_stages().is_empty());
    }

    #[test]
    fn a_name_truncates_at_the_byte_limit() {
        assert_eq!(truncate_name("short"), "short");
        let long = "a".repeat(40);
        assert_eq!(truncate_name(&long).len(), NAME_UNITS);
        // Twenty-four code units, and the twenty-fifth has to go.
        let mixed = format!("{}{}", "e".repeat(23), "\u{1F600}");
        assert_eq!(truncate_name(&mixed), "e".repeat(23));
        assert_eq!(name_units(&truncate_name(&mixed)), 23);
        // A surrogate pair fits when there is room for two units.
        let fits = format!("{}{}", "e".repeat(22), "\u{1F600}");
        assert_eq!(name_units(&truncate_name(&fits)), NAME_UNITS);
    }

    #[test]
    fn a_long_name_survives_the_write_as_stored() {
        let p = profile();
        let mut d = draft();
        d.name = "a".repeat(40);
        let back = Profile::parse(&d.build(&p).serialize()).unwrap();
        assert_eq!(back.name, "a".repeat(NAME_UNITS));
    }

    #[test]
    fn a_profile_with_no_left_click_is_refused() {
        let mut d = draft();
        assert!(d.has_left_click());
        d.set_bind(0, Bind::Special(0x03));
        assert!(!d.has_left_click());
        // And can be put back.
        d.set_bind(7, Bind::Mouse(1));
        assert!(d.has_left_click());
    }

    #[test]
    fn stock_layout_is_clickable() {
        let mut d = draft();
        for i in 0..d.editable {
            d.buttons[i] = Button::disabled();
        }
        assert!(!d.has_left_click());
        d.reset_stock_buttons();
        assert!(d.has_left_click());
        assert_eq!(d.buttons[0].describe(), "button 1");
        assert_eq!(d.buttons[4].describe(), "button 5");
        assert_eq!(d.buttons[5].describe(), "dpi-shift");
        assert_eq!(d.buttons[10].describe(), "wheel-left");
    }

    #[test]
    fn bindings_map_both_ways() {
        assert_eq!(Bind::of(&Button::mouse(3)), Bind::Mouse(3));
        assert_eq!(Bind::of(&Button::disabled()), Bind::Disabled);
        assert_eq!(Bind::of(&Button::special("dpi-up").unwrap()), Bind::Special(0x03));
        // A key binding is readable but not expressible here, so it is left be.
        let key = Button::key(0x1e, 0x02);
        assert_eq!(Bind::of(&key), Bind::Other);
        assert!(Bind::Other.to_button().is_none());
        let mut d = draft();
        d.buttons[6] = key;
        d.set_bind(6, Bind::Other);
        assert_eq!(d.buttons[6], key);
    }

    #[test]
    fn an_unedited_led_hands_back_the_bytes_it_was_given() {
        // Breathing with a byte this editor does not model set in the record.
        let mut raw = [0u8; LED_RECORD];
        raw[0] = led::BREATHING;
        raw[1..4].copy_from_slice(&[0x10, 0x20, 0x30]);
        raw[4] = 0x0f;
        raw[5] = 0xa0;
        raw[6] = 0x03;
        raw[7] = 0x50;
        raw[10] = 0x7f;
        let d = LedDraft::from_led(&Led(raw));
        assert_eq!(d.mode, led::BREATHING);
        assert_eq!(d.color, [0x10, 0x20, 0x30]);
        assert_eq!(d.period_ms, 0x0fa0);
        assert_eq!(d.waveform, 3);
        assert_eq!(d.intensity, 0x50);
        assert_eq!(d.to_led().0, raw);
    }

    #[test]
    fn an_unknown_led_mode_is_not_rewritten() {
        let mut raw = [0u8; LED_RECORD];
        raw[0] = led::STARLIGHT;
        raw[5] = 0x42;
        let mut d = LedDraft::from_led(&Led(raw));
        d.mark(); // edited, but still on a mode with no constructor
        assert_eq!(d.to_led().0, raw);
    }

    #[test]
    fn full_intensity_round_trips_as_one_hundred() {
        let l = Led::breathing([1, 2, 3], 2000, 1, 100);
        assert_eq!(LedDraft::from_led(&l).intensity, 100);
        let c = Led::cycle(8000, 100);
        let d = LedDraft::from_led(&c);
        assert_eq!(d.intensity, 100);
        assert_eq!(d.period_ms, 8000);
    }

    #[test]
    fn copying_a_profile_keeps_the_name_and_takes_the_rest() {
        let mut dst = draft();
        dst.name = "Desktop".into();
        let mut src = draft();
        src.name = "Gaming".into();
        src.rate_ms = 4;
        src.dpi = [400, 0, 0, 0, 0];
        src.default_dpi = 0;
        src.shift_dpi = 0;
        src.buttons[6] = Button::mouse(2);
        dst.copy_settings_from(&src);
        assert_eq!(dst.name, "Desktop");
        assert_eq!(dst.rate_ms, 4);
        assert_eq!(dst.dpi, [400, 0, 0, 0, 0]);
        assert_eq!(dst.buttons[6], Button::mouse(2));
        assert!(dst.has_left_click());
    }

    #[test]
    fn rate_options_always_include_what_is_stored() {
        let mut snap = Snapshot { rates: vec![125, 250, 500, 1000], ..Default::default() };
        assert_eq!(rate_options(&snap, 1), vec![125, 250, 500, 1000]);
        // 1000/3 = 333 Hz is stored but not offered by the device.
        assert!(rate_options(&snap, 3).contains(&333));
        snap.rates.clear();
        assert_eq!(rate_options(&snap, 0), vec![125, 250, 500, 1000]);
    }
}
