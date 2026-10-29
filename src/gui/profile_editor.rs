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
use crate::device::{Cmd, CmdId, CmdKind, Snapshot};
use crate::dpi::Capability;
use crate::led;
use crate::profile_blob::{
    Button, Led, Profile, BTN_HID, HID_MOUSE, LED_RECORD, NAME_LEN, N_DPI, SPECIALS,
};
use crate::rate::ReportRate;

/// The name field is 48 bytes of UTF-16LE, so 24 code units.
pub const NAME_UNITS: usize = NAME_LEN / 2;

/// How long to wait for the outcome of a write before giving up on ever
/// seeing one.
///
/// Not a guess at how long the write takes: the worker records an outcome for
/// every command it executes, the device-not-connected early return included,
/// so the answer normally arrives within a frame or two of the re-read. This
/// bound only covers an answer that never comes at all — the outcome ring
/// overflowing while this panel was not on screen, or a worker that died —
/// and that path keeps the edits.
const OUTCOME_WAIT: Duration = Duration::from_secs(20);

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
        Some(Capability::Range { min, max, step }) if *min <= *max => {
            // In u32 throughout: the capability comes off the wire, and
            // `min + steps * step` overflows a u16 for a device that reports a
            // large step near the top of the range.
            let step = (*step).max(1) as u32;
            let v = v.clamp(*min, *max) as u32;
            let min = *min as u32;
            let steps = ((v - min) as f32 / step as f32).round() as u32;
            (min + steps * step).min(*max as u32) as u16
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
    let mut v: Vec<u16> = if snap.rates.is_empty() {
        vec![125, 250, 500, 1000]
    } else {
        // The device's list comes from an interval bitmask, so it can hold
        // rates that are not a whole number of milliseconds: bit 2 means 3 ms,
        // which is 333 Hz. Byte 0 of the sector is an interval in whole
        // milliseconds, so such a rate cannot be stored, and offering it gave
        // a button that silently did nothing.
        snap.rates
            .iter()
            .copied()
            .filter(|hz| ReportRate::interval_ms(*hz).is_ok())
            .collect()
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
        // Never write an index that names a disabled stage, even one that came
        // off the device that way: that is how a profile ends up booting at 0
        // DPI, and the checkbox path is not the only way one gets here.
        p.default_dpi = self.valid_index(self.default_dpi);
        p.shift_dpi = self.valid_index(self.shift_dpi);
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

    /// `i` if it names an enabled stage, otherwise the first enabled one.
    ///
    /// Left alone when every stage is disabled: there is no index to pick, and
    /// that profile is refused by the save gate anyway.
    fn valid_index(&self, i: u8) -> u8 {
        let on = self.enabled_stages();
        match on.first() {
            Some(&first) if !on.contains(&(i as usize)) => first as u8,
            _ => i,
        }
    }

    /// Keep the default and shift indices pointing at a stage that exists.
    /// A disabled stage as the default is how a profile ends up booting at
    /// 0 DPI.
    pub fn fix_indices(&mut self) {
        self.default_dpi = self.valid_index(self.default_dpi);
        self.shift_dpi = self.valid_index(self.shift_dpi);
    }

    /// A profile with no left click leaves a mouse that cannot click.
    pub fn has_left_click(&self) -> bool {
        self.buttons.iter().take(self.editable).any(is_left_click)
    }

    /// Carry unsaved edits across a change in what the mouse holds.
    ///
    /// Three-way: a field the user has not touched (it still equals `old`, the
    /// reading the edits were staged against) takes the value from `new`; a
    /// field the user changed stays theirs. Without this a draft staged before
    /// another panel wrote this profile — the Lighting tab's Save to mouse,
    /// the per-button writes — carried the *old* bytes for everything the user
    /// never touched, and Save to mouse wrote them straight back over that
    /// write, silently undoing it in flash.
    fn rebase(&mut self, old: &Draft, new: &Draft) {
        fn keep<T: PartialEq + Clone>(mine: &mut T, old: &T, new: &T) {
            if *mine == *old {
                *mine = new.clone();
            }
        }
        keep(&mut self.name, &old.name, &new.name);
        keep(&mut self.rate_ms, &old.rate_ms, &new.rate_ms);
        for i in 0..N_DPI {
            keep(&mut self.dpi[i], &old.dpi[i], &new.dpi[i]);
        }
        keep(&mut self.default_dpi, &old.default_dpi, &new.default_dpi);
        keep(&mut self.shift_dpi, &old.shift_dpi, &new.shift_dpi);
        for z in 0..2 {
            // An edit that lands on exactly what the mouse now holds has
            // nothing left to write either.
            if self.leds[z] == old.leds[z] || self.leds[z].to_led() == new.leds[z].to_led() {
                self.leds[z] = new.leds[z].clone();
            }
        }
        // Never edited by the user, only mirrored from `leds` at build time.
        self.alt_raw = new.alt_raw;
        if self.buttons.len() != new.buttons.len() || old.buttons.len() != new.buttons.len() {
            self.buttons = new.buttons.clone();
        } else {
            for i in 0..self.buttons.len() {
                keep(&mut self.buttons[i], &old.buttons[i], &new.buttons[i]);
            }
        }
        self.editable = new.editable;
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

/// Where the last write this panel sent stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteState {
    /// Handed to the worker; no outcome recorded for it yet.
    Waiting,
    /// The worker answered, and it did not go through. Carries what it said.
    Failed(String),
    /// No outcome arrived inside `OUTCOME_WAIT`. Says nothing about whether
    /// the mouse took the write, so the edits are kept either way.
    Unanswered,
}

/// One write handed to the worker, and what came back.
///
/// Dropped outright on success, which is what lets the device's own reading
/// take over. Kept on every other result, because the staged edits are still
/// on screen and the user has to be told why.
#[derive(Debug, Clone)]
struct Write {
    /// Highest outcome id on the books when Save was pressed. The answer to
    /// this write is the first `WriteProfile(index)` outcome newer than it.
    ///
    /// The id `Handle::send` returns would be more direct, but this panel
    /// never sees it: `ui` returns commands and the host's dispatch loop is
    /// what sends them. Ids are handed out in send order and executed in that
    /// same order, so a watermark plus the kind — which carries the profile
    /// index — names this write's own outcome.
    after: CmdId,
    sent: Instant,
    /// Exactly the draft that was handed over. A success only drops the
    /// staged edits when they still match this: the fields stay live while
    /// the write is in flight, and the second it takes is long enough to
    /// retype a name in. Those keystrokes were never sent, so the device
    /// agreeing with the *earlier* draft says nothing about them.
    sent_draft: Draft,
    state: WriteState,
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
    /// Writes handed to the worker and not yet answered, keyed by the
    /// profile index each one touches. An entry leaves on success, which is
    /// what lets the device's own reading take over.
    ///
    /// Keyed per profile rather than held in one slot: saving profile 2 and
    /// then switching tabs and saving profile 3 inside the second the write
    /// takes would otherwise drop the first write's tracking, and edits the
    /// mouse had in fact taken would sit staged as unsaved for the rest of
    /// the session.
    writes: HashMap<u8, Write>,
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
            writes: HashMap::new(),
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

        // Settled before the staged entry is chosen: a write that has just
        // succeeded drops its staged entry, and that is what lets the reading
        // below take over.
        self.resolve_writes(snap);
        let state = self.writes.get(&index).map(|w| w.state.clone());
        let mut st = stage_for(self.staged.remove(&index), &base);

        self.draw_header(ui, &st, index, state.as_ref());
        self.draw_identity(ui, &mut st.draft, &base);
        self.draw_rate(ui, &mut st.draft, snap);
        self.draw_dpi(ui, &mut st.draft, snap);
        self.draw_lighting(ui, &mut st.draft, snap);
        self.draw_buttons(ui, &mut st.draft, &base, n_buttons);
        self.draw_copy_from(ui, &mut st.draft, snap, index, n_buttons);
        let reverted =
            self.draw_save(ui, &mut st, original, index, snap, state.as_ref(), &mut cmds);

        // Reverting drops the entry, which is what makes the device's own
        // reading show again. Everything else is kept: a write waiting on its
        // outcome still holds the edits that were sent, and one that failed or
        // went unanswered holds them so they can be retried. Only the outcome
        // for a write clears them, in `resolve_write`.
        if !reverted && (st.dirty() || st.base != base) {
            self.staged.insert(index, st);
        } else {
            // Nothing of that write is on screen any more — reverted, or
            // edited back to what the mouse holds — so a settled failure has
            // nothing left to explain and must stop flagging a profile that
            // now matches.
            self.drop_settled_write(index);
        }
        cmds
    }

    /// Settle a write against the outcome the worker recorded for it.
    ///
    /// Success is the only path that drops the staged edits, and it drops them
    /// knowing the device agrees: the worker re-reads every sector and records
    /// the outcome under one lock, so by the time the outcome is visible here
    /// `snap.profiles` is already the post-write reading.
    fn resolve_writes(&mut self, snap: &Snapshot) {
        // Collected rather than removed inside the loop: the map is borrowed
        // mutably for the walk.
        let mut through: Vec<(u8, Draft)> = Vec::new();
        for (index, w) in self.writes.iter_mut() {
            // A settled write is never re-examined, so a later outcome for
            // some other write cannot clear its edits.
            if w.state != WriteState::Waiting {
                continue;
            }
            match snap.outcome_after(w.after, CmdKind::WriteProfile(*index)) {
                // Through. The reading in this same snapshot is what the
                // device now holds, so the draft has nothing left to say.
                Some(o) if o.ok => through.push((*index, w.sent_draft.clone())),
                Some(o) => {
                    w.state = WriteState::Failed(
                        o.message
                            .clone()
                            .unwrap_or_else(|| "the mouse refused the write".into()),
                    );
                }
                None if w.sent.elapsed() > OUTCOME_WAIT => w.state = WriteState::Unanswered,
                // Still in flight.
                None => {}
            }
        }
        for (index, sent) in through {
            self.writes.remove(&index);
            // Only the edits that were actually sent are handed back to the
            // device's reading. Anything typed after Save was never in that
            // write, so dropping it here would lose it silently.
            let moved_on = self.staged.get(&index).is_some_and(|s| s.draft != sent);
            if !moved_on {
                self.staged.remove(&index);
                continue;
            }
            // Those later edits stay, re-based against what the device now
            // holds, so "unsaved" keeps meaning unsaved against the mouse
            // rather than against the reading from before the write.
            //
            // Three-way against what was sent, not just a new base: every
            // field still equal to `sent` is one the user has not touched since
            // Save, and takes the device's own reading. Replacing the base
            // alone left the edited lighting record (old raw bytes, edited
            // flag) and the mirrored second-mode records differing from the
            // reading for good, so the profile showed unsaved changes with
            // nothing to save.
            if let (Some(p), Some(s)) =
                (snap.profiles.get(index as usize), self.staged.get_mut(&index))
            {
                let now = Draft::from_profile(p, s.draft.editable);
                s.draft.rebase(&sent, &now);
                s.base = now;
            }
        }
    }

    /// Forget a write that has already been answered, for `index`.
    ///
    /// Called when the staged edits go away under a revert: with nothing left
    /// on screen from that write there is nothing left to explain, and the
    /// panel would otherwise keep flagging a failure for a profile that now
    /// matches the mouse. A write still in flight is left alone — its answer
    /// is still worth showing, whatever the user has done to the draft since.
    fn drop_settled_write(&mut self, index: u8) {
        if self.writes.get(&index).is_some_and(|w| w.state != WriteState::Waiting) {
            self.writes.remove(&index);
        }
    }

    fn draw_header(
        &self,
        ui: &mut egui::Ui,
        st: &Staged,
        index: u8,
        state: Option<&WriteState>,
    ) {
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
            match state {
                // In flight the edits are still unsaved — that is the honest
                // reading until the device answers — so one pill says so.
                Some(WriteState::Waiting) => theme::pill(ui, "writing to flash", theme::WARN),
                Some(WriteState::Failed(_)) => theme::pill(ui, "write failed", theme::BAD),
                Some(WriteState::Unanswered) => theme::pill(ui, "write unconfirmed", theme::BAD),
                None if st.dirty() => theme::pill(ui, "unsaved changes", theme::WARN),
                None => {}
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
                // Extrema, not the first and last entry: the list is built in
                // the order the device reported it, and a descending one
                // would hand the slider an inverted range.
                Some(Capability::List(v)) => (
                    v.iter().copied().min().unwrap_or(100),
                    v.iter().copied().max().unwrap_or(25600),
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
                                    // Edits, not the default Always: with
                                    // Always the slider rounds the *existing*
                                    // value to the step during the paint pass
                                    // and reports `changed()`, so a stored
                                    // stage the sensor's step does not land on
                                    // was rewritten in the draft by drawing
                                    // the panel, with no click anywhere.
                                    .clamping(egui::SliderClamping::Edits)
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
                        // Guarded on an actual change: marking the record
                        // edited is what makes the editor rebuild it instead
                        // of handing back the bytes it read, so re-picking
                        // the mode already set would flag the profile unsaved
                        // and offer a flash write that changes nothing.
                        if ui
                            .selectable_label(led_draft.mode == *m, led::mode_name(*m))
                            .clicked()
                            && led_draft.mode != *m
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
                            if theme::swatch(ui, *c, led_draft.color == *c).clicked()
                                && led_draft.color != *c
                            {
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
                                // Same reason as the DPI slider: a stored
                                // period outside this range is shown as it is
                                // rather than clamped behind the user's back.
                                .clamping(egui::SliderClamping::Edits)
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
                                .clamping(egui::SliderClamping::Edits)
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
                                        && led_draft.waveform != *v
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
                            // Staged, not selected and not live, so it takes
                            // the warning colour rather than the accent.
                            theme::pill(ui, &format!("was {}", was.describe()), theme::WARN);
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

    /// Returns true if the staged edits were reverted and should be dropped.
    fn draw_save(
        &mut self,
        ui: &mut egui::Ui,
        st: &mut Staged,
        original: &Profile,
        index: u8,
        snap: &Snapshot,
        state: Option<&WriteState>,
        cmds: &mut Vec<Cmd>,
    ) -> bool {
        let dirty = st.dirty();
        let waiting = state == Some(&WriteState::Waiting);
        // With no editable buttons the editor never touches the remapping
        // table — it is written back exactly as read — so there is nothing to
        // refuse, and refusing left the save button dead with no way out.
        let no_click = st.draft.editable > 0 && !st.draft.has_left_click();
        let blocked = no_click || st.draft.enabled_stages().is_empty();
        let mut reverted = false;

        theme::section(ui, "Save", |ui| {
            theme::hint(
                ui,
                if self.backup_asked {
                    "A backup was asked for this session. Taking another overwrites the \
                     file named below, so point it somewhere else to keep both."
                } else {
                    "Take a backup before the first write of this session. It dumps every \
                     sector, the profile directory included, so a bad profile can be put \
                     back. A file already at the path below is replaced, so point it \
                     somewhere new to keep an older backup."
                },
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 130.0).max(160.0);
                ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                // Kept available after the first click: the write happens on
                // the device thread, so a failure to write the file shows up
                // in the status bar afterwards, and hiding the control left no
                // way to retry the one thing that makes a bad write
                // recoverable.
                if ui.button("Save backup").clicked() {
                    cmds.push(Cmd::Backup(self.backup_path.clone()));
                    self.backup_asked = true;
                }
            });
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                // Not while one is in flight: a second click would be a second
                // flash write for the same edits.
                if ui
                    .add_enabled(dirty && !blocked && !waiting, egui::Button::new("Save to mouse"))
                    .clicked()
                {
                    cmds.push(Cmd::WriteProfile {
                        index,
                        profile: Box::new(st.draft.build(original)),
                    });
                    // Read before the command is sent — the host's dispatch
                    // loop sends it after `ui` returns — so every outcome on
                    // the books now is older than this write's own.
                    self.writes.insert(
                        index,
                        Write {
                            after: snap.newest_outcome_id(),
                            sent: Instant::now(),
                            sent_draft: st.draft.clone(),
                            state: WriteState::Waiting,
                        },
                    );
                    // The draft is deliberately left as it is. It is what was
                    // sent, and until the outcome says the device took it,
                    // adopting it as the truth would be a guess — the guess
                    // that used to discard a failed write's edits.
                }
                // Not while one is in flight either. Reverting would throw
                // away the very edits that were sent, and then the failure
                // text below — "your edits are still here" — would be a lie
                // with nothing left on screen to retry from.
                if ui.add_enabled(dirty && !waiting, egui::Button::new("Revert")).clicked() {
                    reverted = true; // dropping the staged entry is the revert
                    self.drop_settled_write(index);
                }
            });

            ui.add_space(8.0);
            match state {
                Some(WriteState::Waiting) => {
                    theme::hint(
                        ui,
                        "Written; waiting for the mouse to confirm. The worker re-reads every \
                         sector after a write, which takes about a second. Your edits stay \
                         here until it answers.",
                    );
                    ui.add_space(8.0);
                }
                Some(WriteState::Failed(msg)) => {
                    ui.label(
                        egui::RichText::new(format!("The write failed: {msg}"))
                            .color(theme::BAD)
                            .size(13.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    theme::hint(
                        ui,
                        "Your edits are still here, so nothing has been lost. Save again to \
                         retry, or Revert to go back to what the mouse holds. Press Refresh in \
                         the sidebar first if you want to see what that is now: a write that \
                         reports an error may still have changed the sector.",
                    );
                    ui.add_space(8.0);
                }
                Some(WriteState::Unanswered) => {
                    ui.label(
                        egui::RichText::new("No answer came back for this write.")
                            .color(theme::BAD)
                            .size(13.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    theme::hint(
                        ui,
                        "That says nothing about whether the mouse took it, so your edits are \
                         kept. Press Refresh in the sidebar to see what the mouse holds now, \
                         then save again or revert.",
                    );
                    ui.add_space(8.0);
                }
                None => {}
            }
            if !dirty {
                theme::hint(ui, "This profile matches what is on the mouse.");
            } else if blocked {
                theme::hint(
                    ui,
                    "Fix the problem flagged above before saving. Nothing has been written.",
                );
            } else if state.is_none() {
                theme::hint(
                    ui,
                    &format!(
                        "One flash write to profile {index}. Flash has finite endurance, so \
                         edits are collected here and written once, not as you type."
                    ),
                );
            }
        });
        reverted
    }
}

/// Which staged entry to draw this frame.
///
/// Edits are never adopted away here, write in flight or not. A write is
/// cleared only by its own outcome, in `ProfileEditor::resolve_writes`, which
/// removes the staged entry on success so the next frame falls through to the
/// device's reading. That is why this no longer needs to know about writes:
/// guessing from a timer is what used to put the pre-write values back under
/// the user's nose after a failed write.
fn stage_for(prev: Option<Staged>, base: &Draft) -> Staged {
    match prev {
        Some(s) if s.base == *base => s,
        // Nothing staged and the device has moved on: adopt the reading.
        Some(s) if !s.dirty() => Staged::new(base.clone()),
        // Edits staged and the device has moved on: keep the edits, but only
        // the ones the user made. See `Draft::rebase`.
        Some(mut s) => {
            s.draft.rebase(&s.base, base);
            s.base = base.clone();
            s
        }
        None => Staged::new(base.clone()),
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
    use crate::device::Outcome;
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

    #[test]
    fn a_rate_the_sector_cannot_store_is_not_offered() {
        // A device whose interval bitmask has bit 2 reports 333 Hz, which is
        // 3 ms: storable as a byte, but the control used to do nothing when
        // 1000/hz was not whole, so it must not be offered.
        let snap = Snapshot { rates: vec![125, 142, 166, 250, 333, 500, 1000], ..Default::default() };
        let offered = rate_options(&snap, 1);
        for hz in [333, 166, 142] {
            assert!(!offered.contains(&hz), "{hz} Hz is not a whole-ms interval");
        }
        assert_eq!(offered, vec![125, 250, 500, 1000]);
        for hz in &offered {
            assert!(ReportRate::interval_ms(*hz).is_ok());
        }
    }

    #[test]
    fn snapping_survives_a_capability_that_would_overflow_a_u16() {
        // Straight off the wire, so the arithmetic has to hold for values no
        // sensor would really report.
        let cap = Capability::Range { min: 1000, max: 65535, step: 8191 };
        assert_eq!(snap_stage(Some(&cap), 65535), 65535);
        assert_eq!(snap_stage(Some(&cap), 60000), 58337);
        // An inverted range is passed through rather than clamped backwards.
        let bad = Capability::Range { min: 25600, max: 100, step: 50 };
        assert_eq!(snap_stage(Some(&bad), 1600), 1600);
    }

    #[test]
    fn a_startup_index_on_a_disabled_stage_is_not_written() {
        let p = profile();
        let mut d = draft();
        // Stage 4 is disabled in the fixture; the device can hand us a profile
        // already pointing at it.
        d.default_dpi = 4;
        d.shift_dpi = 4;
        let back = Profile::parse(&d.build(&p).serialize()).unwrap();
        assert_eq!(back.default_dpi, 0);
        assert_eq!(back.shift_dpi, 0);
        assert_ne!(back.dpi[back.default_dpi as usize], 0);
        // With nothing enabled there is no index to pick, and the save gate
        // refuses that profile instead.
        let mut empty = draft();
        empty.dpi = [0; N_DPI];
        empty.default_dpi = 2;
        assert_eq!(empty.build(&p).default_dpi, 2);
        assert!(empty.enabled_stages().is_empty());
    }

    #[test]
    fn unsaved_edits_are_never_adopted_away_by_a_device_reading() {
        let base = draft();
        let mut st = Staged::new(base.clone());
        st.draft.rate_ms = 4;
        assert!(st.dirty());
        let kept = stage_for(Some(st), &base);
        assert_eq!(kept.draft.rate_ms, 4);
        assert!(kept.dirty());

        // A clean entry does follow the device: that is how another writer's
        // change shows up.
        let clean = Staged::new(base.clone());
        let mut moved = base.clone();
        moved.rate_ms = 8;
        assert_eq!(stage_for(Some(clean), &moved).draft.rate_ms, 8);
    }

    #[test]
    fn staged_edits_carry_over_a_change_another_writer_made() {
        // Edits staged against the reading as it was.
        let old = draft();
        let mut st = Staged::new(old.clone());
        st.draft.name = "Mine".into();
        st.draft.buttons[6] = Button::mouse(2);

        // Meanwhile another panel wrote this profile: new lighting, a new rate
        // and a rebound button the user never touched here.
        let mut p = profile();
        p.report_rate_ms = 4;
        p.leds[0] = Led::fixed([1, 2, 3]);
        p.alt_leds[0] = Led::fixed([1, 2, 3]);
        p.buttons[8] = Button::mouse(3);
        let now = Draft::from_profile(&p, 11);

        let kept = stage_for(Some(st), &now);
        assert_eq!(kept.base, now);
        // The user's own edits survive...
        assert_eq!(kept.draft.name, "Mine");
        assert_eq!(kept.draft.buttons[6], Button::mouse(2));
        assert!(kept.dirty());
        // ...and nothing they did not touch is put back to the old reading.
        assert_eq!(kept.draft.rate_ms, 4);
        assert_eq!(kept.draft.leds[0], now.leds[0]);
        assert_eq!(kept.draft.buttons[8], Button::mouse(3));
        let built = kept.draft.build(&p);
        assert_eq!(built.leds[0], Led::fixed([1, 2, 3]));
        assert_eq!(built.alt_leds[0], Led::fixed([1, 2, 3]));
        assert_eq!(built.report_rate_ms, 4);
        assert_eq!(built.buttons[8], Button::mouse(3));
    }

    #[test]
    fn edits_the_mouse_now_holds_are_no_longer_unsaved() {
        // A write that went through without its outcome being seen: after a
        // refresh the reading matches the draft, so nothing is left to save.
        let old = draft();
        let mut st = Staged::new(old.clone());
        st.draft.rate_ms = 4;
        st.draft.leds[1].mode = led::BREATHING;
        st.draft.leds[1].mark();
        let p = st.draft.build(&profile());
        let now = Draft::from_profile(&Profile::parse(&p.serialize()).unwrap(), 11);
        let kept = stage_for(Some(st), &now);
        assert!(!kept.dirty(), "the mouse already holds these edits");
        assert_eq!(kept.draft, now);
    }

    /// The editor mid-save: edits staged for `index`, the write handed over,
    /// and `after` standing for whatever was already on the books.
    fn mid_save(index: u8, after: CmdId) -> ProfileEditor {
        let mut ed = ProfileEditor::new();
        let mut st = Staged::new(draft());
        st.draft.name = "Gaming".into();
        st.draft.rate_ms = 4;
        let sent_draft = st.draft.clone();
        ed.staged.insert(index, st);
        ed.writes.insert(
            index,
            Write { after, sent: Instant::now(), sent_draft, state: WriteState::Waiting },
        );
        ed
    }

    /// Where the write for `index` stands, or `None` once it is forgotten.
    fn state_of(ed: &ProfileEditor, index: u8) -> Option<WriteState> {
        ed.writes.get(&index).map(|w| w.state.clone())
    }

    fn with_outcomes(outcomes: Vec<Outcome>) -> Snapshot {
        Snapshot { outcomes, ..Default::default() }
    }

    #[test]
    fn a_successful_outcome_hands_the_profile_back_to_the_device() {
        let mut ed = mid_save(2, 5);
        assert!(ed.dirty());
        let snap = with_outcomes(vec![Outcome::ok(
            6,
            CmdKind::WriteProfile(2),
            Some("profile 2 written to flash".into()),
        )]);
        ed.resolve_writes(&snap);
        // Nothing left waiting, and no staged entry, so the next frame stages
        // from the sector the worker re-read before recording this outcome.
        assert!(state_of(&ed, 2).is_none());
        assert!(!ed.dirty());
        assert!(ed.staged.is_empty());
    }

    #[test]
    fn a_failed_outcome_keeps_the_edits_and_carries_the_message() {
        let mut ed = mid_save(2, 5);
        let snap = with_outcomes(vec![Outcome::failed(
            6,
            CmdKind::WriteProfile(2),
            "readback did not match",
        )]);
        ed.resolve_writes(&snap);
        // The whole point: a failed write must not discard the edits.
        assert_eq!(ed.dirty_profiles(), vec![2]);
        assert_eq!(ed.staged[&2].draft.name, "Gaming");
        assert_eq!(state_of(&ed, 2), Some(WriteState::Failed("readback did not match".into())));
        // Saving again is a retry, and it starts waiting from a fresh
        // watermark so the failure just read is not taken for the answer.
        let mut ed2 = ed;
        let sent_draft = ed2.staged[&2].draft.clone();
        ed2.writes.insert(
            2,
            Write {
                after: snap.newest_outcome_id(),
                sent: Instant::now(),
                sent_draft,
                state: WriteState::Waiting,
            },
        );
        ed2.resolve_writes(&snap);
        assert_eq!(state_of(&ed2, 2), Some(WriteState::Waiting));
        assert!(ed2.dirty());
    }

    #[test]
    fn a_failed_outcome_with_nothing_to_say_still_reads_as_a_failure() {
        let mut ed = mid_save(0, 0);
        let snap = with_outcomes(vec![Outcome {
            id: 1,
            kind: CmdKind::WriteProfile(0),
            ok: false,
            message: None,
        }]);
        ed.resolve_writes(&snap);
        match state_of(&ed, 0) {
            Some(WriteState::Failed(m)) => assert!(!m.is_empty()),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(ed.dirty());
    }

    #[test]
    fn an_outcome_that_never_arrives_keeps_the_edits() {
        let mut ed = mid_save(2, 5);
        let empty = with_outcomes(Vec::new());

        // Inside the bound there is nothing to conclude yet.
        ed.resolve_writes(&empty);
        assert_eq!(state_of(&ed, 2), Some(WriteState::Waiting));
        assert!(ed.dirty());

        // Past it, the panel stops waiting — and keeps the edits, because an
        // answer that never came says nothing about what the mouse stored.
        if let Some(w) = ed.writes.get_mut(&2) {
            w.sent = Instant::now()
                .checked_sub(OUTCOME_WAIT + Duration::from_secs(1))
                .expect("a clock with twenty-one seconds behind it");
        }
        ed.resolve_writes(&empty);
        assert_eq!(state_of(&ed, 2), Some(WriteState::Unanswered));
        assert_eq!(ed.dirty_profiles(), vec![2]);
        assert_eq!(ed.staged[&2].draft.rate_ms, 4);

        // And it stays there: a settled write is not re-examined, so a later
        // outcome for some other write cannot clear these edits.
        ed.resolve_writes(&with_outcomes(vec![Outcome::ok(9, CmdKind::WriteProfile(2), None)]));
        assert_eq!(state_of(&ed, 2), Some(WriteState::Unanswered));
        assert!(ed.dirty());
    }

    #[test]
    fn only_this_writes_own_outcome_answers_it() {
        // An outcome already on the books when Save was pressed.
        let mut ed = mid_save(2, 5);
        ed.resolve_writes(&with_outcomes(vec![Outcome::ok(
            4,
            CmdKind::WriteProfile(2),
            None,
        )]));
        assert_eq!(state_of(&ed, 2), Some(WriteState::Waiting));

        // A newer write, to another profile. The kind carries the index for
        // exactly this reason.
        ed.resolve_writes(&with_outcomes(vec![Outcome::ok(
            6,
            CmdKind::WriteProfile(3),
            None,
        )]));
        assert_eq!(state_of(&ed, 2), Some(WriteState::Waiting));

        // Another panel's command, newer still.
        ed.resolve_writes(&with_outcomes(vec![Outcome::failed(
            7,
            CmdKind::SetButton(2),
            "no",
        )]));
        assert_eq!(state_of(&ed, 2), Some(WriteState::Waiting));
        assert!(ed.dirty());

        // The real answer, among the rest.
        ed.resolve_writes(&with_outcomes(vec![
            Outcome::ok(4, CmdKind::WriteProfile(2), None),
            Outcome::ok(6, CmdKind::WriteProfile(3), None),
            Outcome::failed(7, CmdKind::SetButton(2), "no"),
            Outcome::ok(8, CmdKind::WriteProfile(2), None),
        ]));
        assert!(state_of(&ed, 2).is_none());
        assert!(!ed.dirty());
    }

    #[test]
    fn a_write_the_device_was_not_there_for_is_answered_too() {
        // The worker records an outcome even on its device-not-connected
        // early return, so this path reaches the panel rather than sitting on
        // the fallback for twenty seconds.
        let mut ed = mid_save(1, 0);
        ed.resolve_writes(&with_outcomes(vec![Outcome::failed(
            1,
            CmdKind::WriteProfile(1),
            "device not connected",
        )]));
        assert_eq!(state_of(&ed, 1), Some(WriteState::Failed("device not connected".into())));
        assert!(ed.dirty());
    }

    #[test]
    fn a_second_save_does_not_orphan_the_first() {
        // Save profile 2, switch tabs inside the second the write takes, save
        // profile 3. Both are tracked, and each is answered on its own.
        let mut ed = mid_save(2, 5);
        let mut st = Staged::new(draft());
        st.draft.rate_ms = 2;
        let sent_draft = st.draft.clone();
        ed.staged.insert(3, st);
        ed.writes.insert(
            3,
            Write { after: 6, sent: Instant::now(), sent_draft, state: WriteState::Waiting },
        );

        ed.resolve_writes(&with_outcomes(vec![
            Outcome::ok(6, CmdKind::WriteProfile(2), None),
            Outcome::failed(7, CmdKind::WriteProfile(3), "readback did not match"),
        ]));

        // Profile 2 went through, so its edits are gone and the device's
        // reading takes over. Profile 3 failed, so its edits are still here.
        assert!(state_of(&ed, 2).is_none());
        assert!(!ed.staged.contains_key(&2));
        assert_eq!(state_of(&ed, 3), Some(WriteState::Failed("readback did not match".into())));
        assert_eq!(ed.dirty_profiles(), vec![3]);
        assert_eq!(ed.staged[&3].draft.rate_ms, 2);
    }

    #[test]
    fn edits_made_after_save_survive_the_write_going_through() {
        let mut ed = mid_save(2, 5);
        // The fields stay live while the write is in flight, and the write
        // plus the re-read of all five sectors takes about a second: long
        // enough to fix a typo in. Those keystrokes were never sent.
        ed.staged.get_mut(&2).expect("staged").draft.name = "Gaming 2".into();

        // The device took the write and the worker has re-read the sector.
        let mut on_device = profile();
        on_device.name = "Gaming".into();
        on_device.report_rate_ms = 4;
        let mut snap = with_outcomes(vec![Outcome::ok(6, CmdKind::WriteProfile(2), None)]);
        snap.profiles = vec![profile(), profile(), on_device];

        ed.resolve_writes(&snap);

        // The write is answered, but the later edit is not thrown away with
        // it: the mouse agreeing with the earlier draft says nothing about a
        // change that was never handed over.
        assert!(state_of(&ed, 2).is_none());
        assert_eq!(ed.staged[&2].draft.name, "Gaming 2");
        assert_eq!(ed.dirty_profiles(), vec![2]);
        // And it is now unsaved against what the mouse holds, not against the
        // reading from before the write.
        assert_eq!(ed.staged[&2].base.name, "Gaming");
        assert_eq!(ed.staged[&2].base.rate_ms, 4);
    }

    #[test]
    fn a_write_going_through_leaves_no_phantom_difference_behind() {
        // Lighting edited and saved, then a name typed while the write was in
        // flight. The lighting is on the mouse now, byte for byte, so the only
        // thing still unsaved is the name. Re-basing by overwriting the
        // reading alone left the staged lighting (marked edited, holding the
        // old raw record) and the mirrored second-mode records differing from
        // the new reading for good: "unsaved changes" with nothing to save,
        // and a flash write offered that would change nothing.
        let mut ed = ProfileEditor::new();
        let mut st = Staged::new(draft());
        st.draft.leds[0].mode = led::BREATHING;
        st.draft.leds[0].color = [0xff, 0x00, 0x80];
        st.draft.leds[0].mark();
        st.draft.rate_ms = 4;
        let sent_draft = st.draft.clone();
        let on_device = Profile::parse(&sent_draft.build(&profile()).serialize()).unwrap();
        st.draft.name = "typed later".into();
        ed.staged.insert(2, st);
        ed.writes.insert(
            2,
            Write { after: 5, sent: Instant::now(), sent_draft, state: WriteState::Waiting },
        );
        let mut snap = with_outcomes(vec![Outcome::ok(6, CmdKind::WriteProfile(2), None)]);
        snap.profiles = vec![profile(), profile(), on_device];

        ed.resolve_writes(&snap);

        let s = &ed.staged[&2];
        assert_eq!(s.draft.name, "typed later");
        let mut rest = s.draft.clone();
        rest.name = s.base.name.clone();
        assert_eq!(rest, s.base, "only the name was typed after Save");
        // Putting the name back leaves nothing to save.
        let mut back = Staged { draft: rest, base: s.base.clone() };
        assert!(!back.dirty());
        back.draft.name = "typed later".into();
        assert!(back.dirty());
    }

    /// A device shaped like a real one, with `step` as its reported DPI step.
    fn paint_snapshot(step: u16) -> Snapshot {
        Snapshot {
            connected: true,
            has_profiles: true,
            has_dpi: true,
            has_rate: true,
            has_leds: true,
            profile_format: 2,
            profile_count: 1,
            button_count: 11,
            rates: vec![125, 250, 500, 1000],
            dpi_cap: Some(Capability::Range { min: 100, max: 25600, step }),
            profiles: vec![profile()],
            ..Default::default()
        }
    }

    #[test]
    fn painting_the_panel_stages_nothing_and_sends_nothing() {
        use std::cell::RefCell;
        // A step of 400 from a minimum of 100 lands on 100, 500, 900 and so
        // on, so none of the fixture's stages (800, 1600, 3200, 6400) sit on
        // one. egui's slider rounds an *existing* value to the step during
        // the paint pass and reports that as a change, which rewrote the
        // staged DPI with no click anywhere and left the profile reading as
        // unsaved. Nothing here may touch the draft, and nothing may queue a
        // flash write.
        let snap = paint_snapshot(400);
        let ed = RefCell::new(ProfileEditor::new());
        let cmds: RefCell<Vec<Cmd>> = RefCell::new(Vec::new());
        egui::__run_test_ui(|ui| {
            let mut e = ed.borrow_mut();
            let out = e.ui(ui, &snap, 0);
            cmds.borrow_mut().extend(out);
        });
        let e = ed.borrow();
        assert!(cmds.borrow().is_empty(), "painting queued {:?}", cmds.borrow());
        assert!(!e.dirty(), "painting made the editor think the profile had edits");
        assert!(e.dirty_profiles().is_empty());
    }

    #[test]
    fn reverting_forgets_an_answered_write_but_not_one_in_flight() {
        // Revert drops the edits; a failure with nothing left on screen would
        // otherwise keep flagging a profile that now matches the mouse.
        let mut ed = mid_save(2, 5);
        ed.resolve_writes(&with_outcomes(vec![Outcome::failed(
            6,
            CmdKind::WriteProfile(2),
            "readback did not match",
        )]));
        ed.drop_settled_write(2);
        assert!(state_of(&ed, 2).is_none());

        // One still in flight stays tracked: its answer is still worth
        // showing, and it has to be able to clear itself.
        let mut ed = mid_save(2, 5);
        ed.drop_settled_write(2);
        assert_eq!(state_of(&ed, 2), Some(WriteState::Waiting));
    }
}
