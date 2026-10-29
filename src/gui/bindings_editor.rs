//! Editing what each mouse button does on the host.
//!
//! This is the GUI over `actions.rs`: one row per physical button, an action
//! and a mode each, saved to `actions::config_path()`. Nothing here opens a
//! device or /dev/uinput. The two dangerous halves stay where they belong —
//! synthesising input is uinput's, and the 0x8110 remapping table is the
//! device worker's — so this module only ever *asks*, through `Cmd` and
//! `Request`.
//!
//! Two bits of care are deliberate and should survive editing:
//!
//!   * Replace silences a button in the mouse's own remapping table, which the
//!     mouse keeps across unplugging. So Replace on every button at once is
//!     refused outright, Replace on button 1 needs a second click, and
//!     `Request::RestoreIdentity` is always one button away. Every save sends
//!     the complete silenced set, including the empty one, because that is
//!     what `buttonspy::silence` takes and the only thing that lets a button
//!     moved off Replace get its click back without the factory table.
//!   * The config file is written only when the user asks. No autosave on
//!     keystroke: the file is read by the service, and half-typed bindings
//!     would be applied.

use super::theme;
use crate::actions::{self, Action, BindMode, Binding, Bindings, FnKeymap, Keymap, MAX_BUTTON};
use crate::device::{Cmd, CmdId, CmdKind, Snapshot};
use crate::uinput;
use eframe::egui;

/// The device-side half of an edit, queued for the integrator to send.
///
/// One `device::Cmd` variant each, and the host that drains `take_requests`
/// maps them straight across. Kept as its own type so this module can be read
/// and tested without the device layer: the semantics below are the contract,
/// and they are `buttonspy`'s, not this module's to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Silence exactly these buttons in the mouse's own remapping table,
    /// and give every other button its own click back.
    ///
    /// Exact, not additive, and the UI copy has to say so. `buttonspy::silence`
    /// builds the table from a healed snapshot of the device's own, so a button
    /// dropped from this list *is* given its click back, and an empty list puts
    /// the whole table back — which is how a Replace is undone by saving. The
    /// whole set has to be sent every time for that to hold. It does not start
    /// button reporting, though — the service does that — so a Replace applied
    /// with no service running is a dead button.
    ApplyBindings { silence: Vec<u8> },
    /// Write the factory identity remapping table. The recovery path for a
    /// mouse whose buttons were silenced with nothing listening.
    RestoreIdentity,
}

/// The escape hatch, for a mouse too dead to click this app's own button.
/// Named once so the UI copy and the integrator cannot drift apart.
///
/// This has to be a command that actually exists: it is what someone whose
/// left click is silenced types. `main.rs` dispatches `bind` to `cmd_bind`,
/// whose `reset` arm writes the factory identity table.
pub const RESTORE_CLI: &str = "logi bind reset";

/// Header written above the generated bindings, so someone who does find the
/// file knows what rewrites it.
const SAVE_HEADER: &str = "\
# logi button bindings, written by the Buttons editor in the app.
#
# `button N [replace] = ACTION`. Augment, the default, leaves the button's own
# click working and fires the action as well. `replace` silences the physical
# button in the mouse's remapping table.
#
# Hand edits survive until the next save from the app, which rewrites the file.

";

/// What kind of action a row holds. One per `Action` variant, with `None`
/// standing for `Action::Nothing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    Key,
    Text,
    Mouse,
    Command,
}

impl Kind {
    const ALL: &'static [Kind] = &[Kind::None, Kind::Key, Kind::Text, Kind::Mouse, Kind::Command];

    fn label(self) -> &'static str {
        match self {
            Kind::None => "Nothing",
            Kind::Key => "Key or chord",
            Kind::Text => "Typed text",
            Kind::Mouse => "Mouse button",
            Kind::Command => "Shell command",
        }
    }
}

/// One button's pending edit. Every field is kept across a kind change, so
/// clicking through the menu to look at the options does not lose what was
/// typed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    button: u8,
    kind: Kind,
    mode: BindMode,
    /// The chord as written in the config: "ctrl+c", "f5".
    key: String,
    text: String,
    command: String,
    mouse: u16,
}

impl Row {
    fn blank(button: u8) -> Self {
        Row {
            button,
            kind: Kind::None,
            mode: BindMode::Augment,
            key: String::new(),
            text: String::new(),
            command: String::new(),
            // Middle: the one button most people rebind something to.
            mouse: 0x112,
        }
    }

    /// Whether this row says anything. A row with no action that still leaves
    /// the button alone is not written to the file; `replace = nothing` is,
    /// because disabling a button is a real instruction.
    fn is_set(&self) -> bool {
        self.kind != Kind::None || self.mode == BindMode::Replace
    }

    /// The action, or why it cannot be built yet. Errors are what the row
    /// shows the user, so they say what to do rather than what failed.
    fn action(&self, keys: &dyn Keymap) -> Result<Action, String> {
        match self.kind {
            Kind::None => Ok(Action::Nothing),
            Kind::Key => {
                let t = self.key.trim();
                if t.is_empty() {
                    return Err("no key yet: press Capture and hit a key, or type one".into());
                }
                // Parsed through actions, not resolved here, so the GUI and
                // the config file can never disagree about what a chord means.
                Action::parse(&format!("key {t}"), keys)
            }
            Kind::Text => {
                let t = self.text.trim();
                if t.is_empty() {
                    return Err("nothing to type yet".into());
                }
                if t.contains('\n') {
                    return Err("one line only: the config file is a line per button".into());
                }
                Ok(Action::Text(t.to_string()))
            }
            Kind::Mouse => Ok(Action::MouseButton(self.mouse)),
            Kind::Command => {
                let t = self.command.trim();
                if t.is_empty() {
                    return Err("no command yet".into());
                }
                if t.contains('\n') {
                    return Err("one line only: the config file is a line per button".into());
                }
                Ok(Action::Command(t.to_string()))
            }
        }
    }

    /// Fill a row from a binding that was read back out of the config file.
    fn from_binding(b: &Binding, keys: &dyn Keymap) -> Self {
        let mut r = Row::blank(b.button);
        r.mode = b.mode;
        match &b.action {
            Action::Nothing => r.kind = Kind::None,
            Action::Key { .. } => {
                r.kind = Kind::Key;
                // `describe` writes "key ctrl+c"; the row holds the chord only.
                r.key = b
                    .action
                    .describe(keys)
                    .strip_prefix("key ")
                    .unwrap_or_default()
                    .to_string();
            }
            Action::Text(t) => {
                r.kind = Kind::Text;
                r.text = t.clone();
            }
            Action::MouseButton(c) => {
                r.kind = Kind::Mouse;
                r.mouse = *c;
            }
            Action::Command(c) => {
                r.kind = Kind::Command;
                r.command = c.clone();
            }
        }
        r
    }
}

/// The key-name tables live in uinput; `FnKeymap` is the seam.
fn keymap() -> impl Keymap {
    FnKeymap::new(uinput::key_code, |c| uinput::key_name(c).map(Into::into))
}

/// Would every button the device has end up on Replace?
///
/// That is the state to refuse: a mouse whose every button only runs host
/// actions has no click left to fix it with if the host side is not running.
fn all_replaced(rows: &[Row], count: u8) -> bool {
    if count == 0 {
        return false;
    }
    let n = count.min(MAX_BUTTON);
    (1..=n).all(|b| {
        rows.iter()
            .any(|r| r.button == b && r.mode == BindMode::Replace)
    })
}

/// egui's key name to the driver's. Most names agree once lowercased, so this
/// only carries the handful that do not; uinput's own alias table does the
/// rest ("escape", "period", "quote", and so on).
fn driver_key_name(egui_name: &str) -> String {
    let n = egui_name.to_ascii_lowercase();
    // Key::Num3 is the digit row's 3, not the keypad's.
    if let Some(d) = n.strip_prefix("num") {
        if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() {
            return d.to_string();
        }
    }
    match n.as_str() {
        // egui's `Key::name` calls the arrows "Left" and "Right", and uinput
        // reads a bare "left" or "right" as the mouse button. Passed through,
        // capturing the arrow key bound a click: `key left` parses to
        // BTN_LEFT, so every press of the button would left-click.
        "left" => "arrowleft".into(),
        "right" => "arrowright".into(),
        "up" => "arrowup".into(),
        "down" => "arrowdown".into(),
        "openbracket" | "opencurlybracket" => "leftbrace".into(),
        "closebracket" | "closecurlybracket" => "rightbrace".into(),
        "pipe" => "backslash".into(),
        "questionmark" => "slash".into(),
        "colon" => "semicolon".into(),
        "exclamationmark" => "1".into(),
        _ => n,
    }
}

/// Fold one frame's edited copy of a row back into the model.
///
/// `mode` is deliberately taken from the model and not from the copy. The copy
/// is cloned at the top of the frame, before any widget runs, while Augment
/// and Replace are clicked part-way through it and go straight to the model
/// through `set_augment` / `want_replace` — they have to, because only those
/// run the all-Replace refusal and the left-click confirmation. Writing the
/// copy's stale mode back over the top would undo the click in the same frame,
/// in both directions: Replace would never take, and — far worse — a button
/// already silenced could never be put back to Augment, so the one-click way
/// off Replace would be gone.
fn merge_row(model: &mut Row, mut edited: Row) {
    edited.mode = model.mode;
    if *model != edited {
        *model = edited;
    }
}

/// A captured key press as config syntax: "ctrl+shift+f5".
///
/// egui has no Super modifier on Linux, so a Super chord has to be typed. The
/// UI says so rather than silently dropping the key.
fn chord_text(key_name: &str, m: egui::Modifiers) -> String {
    let mut s = String::new();
    if m.ctrl || m.command {
        s.push_str("ctrl+");
    }
    if m.shift {
        s.push_str("shift+");
    }
    if m.alt {
        s.push_str("alt+");
    }
    // uinput's "plus" is the keypad's. Shift+= is what produces a '+' on the
    // main block, and egui reports that as Plus with Shift held.
    if key_name == "Plus" && m.shift {
        s.push_str("equal");
    } else {
        s.push_str(&driver_key_name(key_name));
    }
    s
}

/// In-app editor for host-side button bindings.
pub struct BindingsEditor {
    rows: Vec<Row>,
    /// The rows as last loaded or saved, so `dirty` has something to compare.
    saved: Vec<Row>,
    /// Lines the config file could not parse. Kept visible: a binding that
    /// silently vanished is worse than one that complains.
    problems: Vec<String>,
    /// Last save or load outcome: (message, went well).
    status: Option<(String, bool)>,
    /// Button whose next key press is being captured.
    capturing: Option<u8>,
    /// The frame this panel last ran in. Capture is armed on a click and
    /// lives in this struct, but only a frame that draws this panel can take
    /// the key; a gap means the user left the tab, and coming back to a
    /// capture still armed would eat the first key they typed there.
    last_ui_frame: u64,
    /// Button waiting for a second click to confirm Replace.
    confirm: Option<u8>,
    /// Why the last Replace was refused outright.
    refused: Option<String>,
    backup_path: String,
    /// Whether a backup is known to have been written. Set from the device
    /// layer's own outcome, never from the click: this flag silences a safety
    /// warning, so a backup that failed must not be able to clear it.
    backed_up: bool,
    requests: Vec<Request>,
    /// A Save's `ApplyBindings` is on its way: the newest outcome id the
    /// snapshot held when it was queued, so only an answer to it counts. The
    /// status line must not claim a button was given back before the device
    /// has said so — a failed write leaves the remapping table as it was.
    awaiting_apply: Option<CmdId>,
    /// How many `ApplyBindings` are queued and not yet answered. Two Saves
    /// can be in flight at once, and the first one's answer is also "newer
    /// than the id seen before the second was sent", so matching on the id
    /// alone settles the status on the wrong command and never hears the
    /// second one fail.
    pending_applies: usize,
    /// The newest Save was not sent to the device (none connected, or it
    /// cannot report presses), so an earlier apply that answers later is not
    /// news about the file as it now stands.
    tail_unapplied: bool,
    loaded: bool,
}

impl Default for BindingsEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl BindingsEditor {
    pub fn new() -> Self {
        BindingsEditor {
            rows: (1..=MAX_BUTTON).map(Row::blank).collect(),
            saved: (1..=MAX_BUTTON).map(Row::blank).collect(),
            problems: Vec::new(),
            status: None,
            capturing: None,
            last_ui_frame: 0,
            confirm: None,
            refused: None,
            backup_path: default_backup_path(),
            backed_up: false,
            requests: Vec::new(),
            awaiting_apply: None,
            pending_applies: 0,
            tail_unapplied: false,
            loaded: false,
        }
    }

    /// Read the bindings file. A missing file is the normal first run, not an
    /// error; an unreadable one is reported.
    pub fn load(&mut self) {
        self.loaded = true;
        let path = actions::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.set_from_text(&text);
                let n = self.rows.iter().filter(|r| r.is_set()).count();
                self.status = Some((
                    format!("{n} binding(s) loaded from {}", path.display()),
                    true,
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.set_from_text("");
                self.status = Some((
                    format!("no bindings yet; Save writes {}", path.display()),
                    true,
                ));
            }
            Err(e) => {
                self.set_from_text("");
                self.status = Some((format!("cannot read {}: {e}", path.display()), false));
            }
        }
    }

    /// Whether there are unsaved edits.
    pub fn dirty(&self) -> bool {
        self.rows != self.saved
    }

    /// Requests the integrator still has to turn into commands. Drains.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Replace the edit model from config text. The parse half of the model,
    /// split out so it can be tested without a filesystem.
    fn set_from_text(&mut self, text: &str) {
        let keys = keymap();
        let (bindings, problems) = Bindings::parse(text, &keys);
        self.rows = (1..=MAX_BUTTON).map(Row::blank).collect();
        for b in &bindings.list {
            if let Some(slot) = self.rows.iter_mut().find(|r| r.button == b.button) {
                *slot = Row::from_binding(b, &keys);
            }
        }
        self.saved = self.rows.clone();
        self.problems = problems;
        self.capturing = None;
        self.confirm = None;
        self.refused = None;
    }

    /// The whole model as a `Bindings`, plus any row that cannot be built.
    fn to_bindings(&self, keys: &dyn Keymap) -> (Bindings, Vec<String>) {
        let mut out = Bindings::default();
        let mut errs = Vec::new();
        for r in &self.rows {
            if !r.is_set() {
                continue;
            }
            match r.action(keys) {
                Ok(action) => out.list.push(Binding {
                    button: r.button,
                    action,
                    mode: r.mode,
                }),
                Err(e) => errs.push(format!("button {}: {e}", r.button)),
            }
        }
        (out, errs)
    }

    /// The file text, and the buttons that will be silenced.
    fn render(&self, keys: &dyn Keymap) -> Result<(String, Vec<u8>), Vec<String>> {
        let (bindings, errs) = self.to_bindings(keys);
        if !errs.is_empty() {
            return Err(errs);
        }
        let mut text = String::from(SAVE_HEADER);
        text.push_str(&bindings.render(keys));
        Ok((text, bindings.to_silence()))
    }

    /// Write the config file. Returns the buttons to silence on success.
    fn save(&mut self, count: u8) -> Result<Vec<u8>, String> {
        // Belt and braces: the Replace control refuses this already, so
        // reaching it here would mean a bug, and a bug must not be able to
        // write a file that unmaps every button.
        if all_replaced(&self.rows, count) {
            return Err("every button is set to Replace; leave at least one clicking".into());
        }
        let keys = keymap();
        let (text, silence) = self.render(&keys).map_err(|errs| errs.join("; "))?;
        let path = actions::config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        // Through a temporary in the same directory, then renamed. A plain
        // write truncates first, so a crash or a full disk between the two
        // leaves half a file — and the service reads this file as the user's
        // bindings, with a half-read Replace line silencing a button.
        let tmp = path.with_extension("conf.tmp");
        // Flushed to the disk, not just to the page cache, before the rename.
        // The rename is what makes the swap atomic, but an unsynced temporary
        // can be renamed into place and still come back empty after a power
        // loss — and an empty bindings file is not "no bindings": it is a
        // button silenced in the mouse with nothing left on disk saying so.
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })();
        if let Err(e) = written {
            // A half-written temporary is never renamed, but leaving it there
            // is litter in the user's config directory.
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {e}", tmp.display()));
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot replace {}: {e}", path.display())
        })?;
        self.saved = self.rows.clone();
        self.problems.clear();
        Ok(silence)
    }

    /// Take the next key press out of egui's own event stream.
    ///
    /// Reading raw events rather than a focused widget is what makes a chord
    /// like ctrl+shift+f5 capturable: a TextEdit would eat some of those as
    /// shortcuts and never see the rest.
    ///
    /// Taken *out*, not just read. While capture is armed the key belongs to
    /// the capture and to nothing else, and leaving it in the queue hands the
    /// same press to whatever still holds keyboard focus. Capture is armed by
    /// clicking a focusable widget, so that widget is usually the one holding
    /// it: capturing Space or Enter activated it a second time and silently
    /// re-armed capture, which then ate the next key the user typed anywhere
    /// in the app and rewrote a row with it. The `Text` events the same press
    /// produces go with it, or the character is inserted into whichever
    /// TextEdit had focus.
    fn poll_capture(&mut self, ui: &egui::Ui) {
        let frame = ui.ctx().cumulative_frame_nr();
        let away = frame > self.last_ui_frame.saturating_add(1);
        self.last_ui_frame = frame;
        if away {
            self.capturing = None;
        }
        let Some(button) = self.capturing else { return };
        let got = ui.input_mut(|i| {
            // Ctrl+C, Ctrl+X and Ctrl+V never arrive as a `Key` event at all:
            // egui-winit turns them into Copy / Cut / Paste and returns before
            // pushing the key. The modifiers they were pressed with are the
            // ones egui currently holds.
            let held = i.modifiers;
            let mut first = None;
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } => {
                    first.get_or_insert((key.name(), *modifiers));
                    false
                }
                egui::Event::Copy => {
                    first.get_or_insert(("C", held));
                    false
                }
                egui::Event::Cut => {
                    first.get_or_insert(("X", held));
                    false
                }
                // Not left in the queue either: a focused TextEdit would
                // paste the clipboard into itself on the same press.
                egui::Event::Paste(_) => {
                    first.get_or_insert(("V", held));
                    false
                }
                // Releases and repeats go too: half a press left behind is
                // what makes a focused widget act on a key capture took.
                egui::Event::Key { .. } | egui::Event::Text(_) => false,
                _ => true,
            });
            first
        });
        let Some((name, mods)) = got else { return };
        self.capturing = None;
        if name == "Escape" && mods.is_none() {
            return;
        }
        let chord = chord_text(name, mods);
        if let Some(r) = self.rows.iter_mut().find(|r| r.button == button) {
            r.kind = Kind::Key;
            r.key = chord;
        }
    }

    /// Whether putting `button` on Replace would leave no button clicking. If
    /// so, records the refusal for the user and says so.
    fn refuse_if_last(&mut self, button: u8, count: u8) -> bool {
        let mut probe = self.rows.clone();
        if let Some(r) = probe.iter_mut().find(|r| r.button == button) {
            r.mode = BindMode::Replace;
        }
        if all_replaced(&probe, count) {
            self.refused = Some(format!(
                "Button {button} is the last one still clicking. Replace on every \
                 button would leave the mouse with no physical click at all."
            ));
            return true;
        }
        self.refused = None;
        false
    }

    /// Ask for Replace on one button, with the two refusals that matter.
    fn want_replace(&mut self, button: u8, count: u8) {
        if self.refuse_if_last(button, count) {
            return;
        }
        if button == 1 {
            // Left click. Needs a second, deliberate click.
            self.confirm = Some(button);
            return;
        }
        self.set_replace(button);
    }

    /// The second click on the left-click confirmation.
    ///
    /// The all-Replace refusal is checked again here, not trusted from when
    /// the card was opened. The card stays up while other rows are edited, so
    /// Replace on every other button in the meantime would otherwise let this
    /// click walk straight past the guard.
    fn confirm_replace(&mut self, button: u8, count: u8) {
        self.confirm = None;
        if self.refuse_if_last(button, count) {
            return;
        }
        self.set_replace(button);
    }

    fn set_replace(&mut self, button: u8) {
        self.confirm = None;
        if let Some(r) = self.rows.iter_mut().find(|r| r.button == button) {
            r.mode = BindMode::Replace;
        }
    }

    /// Drop back to Augment. Never refused: recovery has to be one click.
    fn set_augment(&mut self, button: u8) {
        self.confirm = None;
        self.refused = None;
        if let Some(r) = self.rows.iter_mut().find(|r| r.button == button) {
            r.mode = BindMode::Augment;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if !self.loaded {
            self.load();
        }
        self.poll_capture(ui);
        self.settle_apply(snap);

        // An unknown device gets the full set of rows, so the file can be
        // written before the mouse is plugged in.
        let count = if snap.button_count == 0 {
            MAX_BUTTON
        } else {
            snap.button_count.min(MAX_BUTTON)
        };
        // A capture armed on a row that is no longer drawn (a different mouse
        // with fewer buttons) has no "Press a key" cue and would still take
        // the next key typed anywhere in this panel.
        if self.capturing.is_some_and(|b| b > count) {
            self.capturing = None;
        }
        let keys = keymap();
        let (bindings, errs) = self.to_bindings(&keys);
        let silence = bindings.to_silence();

        theme::section(ui, "Host bindings", |ui| {
            theme::readout(ui, &bindings.list.len().to_string(), "bound");
            ui.add_space(6.0);
            theme::value(ui, &silence.len().to_string(), "replaced");
            ui.add_space(10.0);
            theme::hint(
                ui,
                "What a mouse button does on this computer, as opposed to what the \
                 mouse itself reports. Edits are staged here and written when you \
                 press Save; nothing reaches the mouse until then.",
            );
            if !snap.connected {
                ui.add_space(8.0);
                theme::hint(
                    ui,
                    "No mouse connected, so all 16 possible buttons are listed. \
                     Bindings can still be edited and saved.",
                );
            } else if !snap.has_button_spy {
                ui.add_space(8.0);
                theme::unsupported(ui, "button reporting, which host bindings need");
                ui.add_space(6.0);
                theme::hint(
                    ui,
                    "Without it the mouse cannot tell this app that a button was \
                     pressed, so neither Augment nor Replace can work. Bindings \
                     below can still be edited for another mouse.",
                );
            }
        });

        self.draw_rows(ui, count, &keys);
        self.draw_safety(ui, snap, &silence, count, &mut cmds);
        self.draw_save(ui, snap, &errs, count);
        self.draw_problems(ui);
        cmds
    }

    fn draw_rows(&mut self, ui: &mut egui::Ui, count: u8, keys: &dyn Keymap) {
        theme::section(ui, "Buttons", |ui| {
            theme::hint(
                ui,
                "Augment keeps the button's own job and fires the action as well. \
                 Replace silences the physical button in the mouse's remapping \
                 table, so it only fires the action — and the mouse keeps that \
                 across unplugging, until a Save without it or Restore puts it back.",
            );
            ui.add_space(12.0);
            for i in 0..count {
                let button = i + 1;
                self.draw_row(ui, button, count, keys);
                if button != count {
                    ui.add_space(6.0);
                    let r = ui.available_rect_before_wrap();
                    ui.painter().hline(
                        r.x_range(),
                        ui.cursor().top() - 2.0,
                        egui::Stroke::new(1.0, theme::LINE),
                    );
                    ui.add_space(6.0);
                }
            }

            // The file can bind a button this mouse does not have: the count
            // comes from the device, hand editing does not. Those rows get no
            // row above, which would make them invisible state — still written
            // back out on save, and a Replace among them makes the device side
            // refuse the whole set, since buttonspy only accepts 1..count.
            let hidden: Vec<String> = self
                .rows
                .iter()
                .filter(|r| r.button > count && r.is_set())
                .map(|r| r.button.to_string())
                .collect();
            if !hidden.is_empty() {
                ui.add_space(12.0);
                theme::hint(
                    ui,
                    &format!(
                        "The file also binds button {} — this mouse has {count}, so \
                         there is no row for that above. It stays in the file as it is; \
                         a Replace among these cannot be applied to this mouse, so Save \
                         leaves it out of what it sends.",
                        hidden.join(", ")
                    ),
                );
                ui.add_space(6.0);
                if ui.button("Forget those bindings").clicked() {
                    for r in self.rows.iter_mut().filter(|r| r.button > count) {
                        *r = Row::blank(r.button);
                    }
                }
            }
        });
    }

    fn draw_row(&mut self, ui: &mut egui::Ui, button: u8, count: u8, keys: &dyn Keymap) {
        // Edited on a copy, so one row's widgets cannot borrow the whole
        // editor and the write-back is the only place the model changes.
        let Some(idx) = self.rows.iter().position(|r| r.button == button) else { return };
        let mut row = self.rows[idx].clone();
        let replaced = row.mode == BindMode::Replace;

        ui.horizontal(|ui| {
            ui.add_sized(
                egui::vec2(74.0, 20.0),
                egui::Label::new(
                    egui::RichText::new(format!("Button {button}"))
                        .size(13.0)
                        .color(if button == 1 { theme::TEXT } else { theme::DIM }),
                ),
            );
            egui::ComboBox::from_id_salt(("bind-kind", button))
                .selected_text(row.kind.label())
                .width(136.0)
                .show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut row.kind, *k, k.label());
                    }
                });
            // Capture belongs to a Key row. Switched to another kind with it
            // still armed, the cue is gone but the next key press would turn
            // the row back into a Key binding behind the user's back.
            if row.kind != Kind::Key && self.capturing == Some(button) {
                self.capturing = None;
            }

            ui.add_space(4.0);
            // The mode lives on the same line as the action: the pair is the
            // thing people get wrong, so they are never read apart.
            if ui.selectable_label(!replaced, "Augment").clicked() && replaced {
                self.set_augment(button);
            }
            if ui.selectable_label(replaced, "Replace").clicked() && !replaced {
                // Against the device's own button count, the same number
                // draw_rows clamped the visible rows to. Counting the rows
                // instead gives MAX_BUTTON every time — there is always one
                // row per possible button — so on any mouse with fewer
                // buttons than that the all-Replace refusal could never fire.
                self.want_replace(button, count);
            }
            if button == 1 && replaced {
                theme::pill(ui, "left click silenced", theme::BAD);
            }
        });

        // The value editor, indented under its row.
        ui.horizontal(|ui| {
            ui.add_space(74.0);
            ui.vertical(|ui| {
                let w = (ui.available_width() - 110.0).max(140.0);
                match row.kind {
                    Kind::None => {
                        theme::hint(
                            ui,
                            if replaced {
                                "Disabled: the button does nothing at all."
                            } else {
                                "Unbound: the button keeps its normal job."
                            },
                        );
                    }
                    Kind::Key => {
                        ui.horizontal(|ui| {
                            let capturing = self.capturing == Some(button);
                            let label = if capturing { "Press a key" } else { "Capture" };
                            if ui.selectable_label(capturing, label).clicked() {
                                self.capturing = if capturing { None } else { Some(button) };
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut row.key)
                                    .hint_text("ctrl+c")
                                    .desired_width(w),
                            );
                        });
                        if self.capturing == Some(button) {
                            ui.add_space(4.0);
                            theme::hint(
                                ui,
                                "Waiting for a key press. Escape cancels. Super is not \
                                 visible to this window, so type super+l for that one; \
                                 ctrl+v with an empty clipboard sends this window \
                                 nothing at all, so type that one too.",
                            );
                        }
                    }
                    Kind::Text => {
                        ui.add(
                            egui::TextEdit::singleline(&mut row.text)
                                .hint_text("Hello there")
                                .desired_width(w),
                        );
                        ui.add_space(4.0);
                        theme::hint(
                            ui,
                            "Typed as key presses on a US layout; other layouts produce \
                             other characters.",
                        );
                    }
                    Kind::Mouse => {
                        egui::ComboBox::from_id_salt(("bind-mouse", button))
                            .selected_text(mouse_label(row.mouse))
                            .show_ui(ui, |ui| {
                                for (n, c) in actions::MOUSE_BUTTONS {
                                    ui.selectable_value(&mut row.mouse, *c, *n);
                                }
                            });
                    }
                    Kind::Command => {
                        ui.add(
                            egui::TextEdit::singleline(&mut row.command)
                                .hint_text("notify-send hi")
                                .desired_width(w),
                        );
                        ui.add_space(4.0);
                        theme::hint(
                            ui,
                            "Runs as you, with your environment: as trusted as ~/.profile. \
                             A pipe, a glob, quoting or $VAR sends it through /bin/sh.",
                        );
                    }
                }

                if let Err(e) = row.action(keys) {
                    if row.is_set() {
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(e).color(theme::WARN).size(11.5),
                            )
                            .wrap(),
                        );
                    }
                }

                // Confirmation for left click, inline where the click was.
                if self.confirm == Some(button) {
                    ui.add_space(8.0);
                    theme::card(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Replace left click?")
                                .color(theme::BAD)
                                .strong(),
                        );
                        ui.add_space(6.0);
                        theme::hint(
                            ui,
                            "Button 1 is left click. On Replace it stops clicking in the \
                             firmware and only fires this action, so if the host side is \
                             not running you cannot click anything — including the buttons \
                             in this window.",
                        );
                        ui.add_space(6.0);
                        theme::hint(
                            ui,
                            &format!("Recovery from a terminal: {RESTORE_CLI}"),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Keep left click").clicked() {
                                self.confirm = None;
                            }
                            if ui.button("Replace it anyway").clicked() {
                                self.confirm_replace(button, count);
                            }
                        });
                    });
                }
            });
        });

        merge_row(&mut self.rows[idx], row);
    }

    fn draw_safety(
        &mut self,
        ui: &mut egui::Ui,
        snap: &Snapshot,
        silence: &[u8],
        _count: u8,
        cmds: &mut Vec<Cmd>,
    ) {
        // The warning beside Restore is only worth anything if it tracks what
        // actually reached the disk, so it follows the newest backup outcome
        // rather than the click that asked for one.
        if let Some(ok) = backup_confirmed(snap) {
            self.backed_up = ok;
        }

        if let Some(msg) = self.refused.clone() {
            theme::section(ui, "Refused", |ui| {
                ui.horizontal(|ui| {
                    theme::pill(ui, "not allowed", theme::BAD);
                });
                ui.add_space(8.0);
                theme::hint(ui, &msg);
                ui.add_space(8.0);
                if ui.button("Understood").clicked() {
                    self.refused = None;
                }
            });
        }

        // Deliberately not gated on there being a Replace row. The rows are
        // this app's staged edit; the mouse's remapping table is whatever was
        // last written to it, and it survives unplugging. Someone whose
        // buttons are silenced and who then reloads the file, discards edits,
        // or starts the app fresh sees no Replace at all — and that is exactly
        // the person who needs Restore and the terminal command in front of
        // them.
        let silenced = !silence.is_empty();
        theme::section(ui, "Replace safety", |ui| {
            if silenced {
                let list: Vec<String> = silence.iter().map(|b| b.to_string()).collect();
                theme::row(ui, "silenced", &list.join(", "));
                ui.add_space(8.0);
                theme::hint(
                    ui,
                    "These buttons stop working physically once the bindings are applied, \
                     and stay that way across unplugging and after this app is closed. \
                     Each Save writes the whole set at once, so a button moved back to \
                     Augment gets its click back on the next Save. Restore below is the \
                     escape for when this window can no longer be clicked at all.",
                );
            } else {
                theme::hint(
                    ui,
                    "Nothing above is set to Replace. Saving in this state silences no \
                     button and gives back the click of any button silenced earlier. \
                     Restore below does the same without saving, by forcing the factory \
                     table.",
                );
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Restore physical buttons").clicked() {
                    self.requests.push(Request::RestoreIdentity);
                }
                if silenced && !self.backed_up {
                    theme::pill(ui, "no backup taken", theme::WARN);
                }
            });
            ui.add_space(6.0);
            theme::hint(
                ui,
                &format!(
                    "Restore writes the factory remapping table, so every button clicks \
                     again. It is also `{RESTORE_CLI}` from a terminal, which is the one \
                     to remember: a mouse whose left click is silenced cannot press the \
                     button above."
                ),
            );

            if silenced {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 90.0).max(160.0);
                    ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                    if ui
                        .add_enabled(snap.has_profiles, egui::Button::new("Back up"))
                        .clicked()
                    {
                        cmds.push(Cmd::Backup(self.backup_path.clone()));
                    }
                });
                ui.add_space(6.0);
                theme::hint(
                    ui,
                    "A backup covers the stored profiles, which is what a mis-set binding \
                     is most likely to send you back to. The remapping table is not in it; \
                     Restore above is that one's escape.",
                );
            }
        });
    }

    /// Queue the device-side half of a save that has just landed, and return
    /// the message to show for it.
    ///
    /// The empty list is queued too, and that is the whole point.
    /// `buttonspy::silence` writes exactly the set it is given and puts every
    /// button not in it back to its own click, so an empty set un-silences the
    /// lot. Swallowing it here left a button the user had just moved off
    /// Replace silenced in the mouse — across unplugging — with only the
    /// factory table to get it back, and told them so in the status line.
    fn queue_apply(&mut self, apply: bool, silence: Vec<u8>, after: CmdId) -> String {
        let path = actions::config_path();
        self.tail_unapplied = !apply;
        if !apply {
            // An apply already in flight keeps being tracked: if it fails, the
            // mouse's table is in an unknown state and that is still news.
            return format!(
                "saved to {}; not applied, because no connected mouse can report \
                 button presses",
                path.display()
            );
        }
        let n = silence.len();
        self.requests.push(Request::ApplyBindings { silence });
        // The baseline of the oldest unanswered apply stays: answers arrive in
        // send order, and a newer baseline would skip past the first one.
        if self.pending_applies == 0 || self.awaiting_apply.is_none() {
            self.awaiting_apply = Some(after);
            self.pending_applies = 0;
        }
        self.pending_applies += 1;
        if n == 0 {
            format!(
                "saved to {}; asking the mouse to let every physical button click again",
                path.display()
            )
        } else {
            format!(
                "saved to {}; asking the mouse to silence {n} button(s) and leave \
                 every other one clicking",
                path.display()
            )
        }
    }

    /// Replace the "asking the mouse" status with what the device answered.
    ///
    /// The file is already written by then, so a failure here means the file
    /// and the mouse's remapping table disagree, and the user needs to hear
    /// that rather than keep reading a success. Save is not offered again for
    /// the same edit (nothing is dirty), so the way out is named.
    fn settle_apply(&mut self, snap: &Snapshot) {
        let Some(after) = self.awaiting_apply else { return };
        let Some(o) = snap.outcome_after(after, CmdKind::ApplyBindings) else { return };
        self.pending_applies = self.pending_applies.saturating_sub(1);
        if self.pending_applies > 0 {
            // Not the last Save's answer. The last one writes the whole set
            // again, so it is the one that says where the table ended up.
            self.awaiting_apply = Some(o.id);
            return;
        }
        self.awaiting_apply = None;
        if o.ok && self.tail_unapplied {
            // The newest Save never reached the device; the status already
            // says so, and this answer is about an older set.
            return;
        }
        self.status = Some(if o.ok {
            (
                format!(
                    "saved; the mouse answered: {}",
                    o.message.as_deref().unwrap_or("done")
                ),
                true,
            )
        } else {
            (
                format!(
                    "saved to the file, but the mouse did not take it: {}. Its remapping \
                     table may still hold the old silenced set; Restore physical buttons \
                     puts every button back.",
                    o.message.as_deref().unwrap_or("no reason given")
                ),
                false,
            )
        });
    }

    fn draw_save(&mut self, ui: &mut egui::Ui, snap: &Snapshot, errs: &[String], count: u8) {
        theme::section(ui, "Save", |ui| {
            let blocked = all_replaced(&self.rows, count);
            let ok = errs.is_empty() && !blocked;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(ok && self.dirty(), egui::Button::new("Save bindings"))
                    .clicked()
                {
                    match self.save(count) {
                        Ok(silence) => {
                            // Only ask the device layer for anything when the
                            // mouse can actually report button presses. A
                            // Replace silences the physical button; doing that
                            // to a mouse that cannot tell us it was pressed
                            // leaves a button that does nothing at all, and it
                            // stays that way across unplugging. This is also
                            // what the hint at the bottom of the section
                            // promises, so the two must not drift apart.
                            let apply = snap.connected && snap.has_button_spy;
                            // Only buttons this mouse has. buttonspy refuses
                            // the whole set if one is past its count, which
                            // would also refuse the un-silencing of the others
                            // the user moved off Replace in the same edit.
                            let silence: Vec<u8> =
                                silence.into_iter().filter(|b| *b <= count).collect();
                            let after = snap.newest_outcome_id();
                            let msg = self.queue_apply(apply, silence, after);
                            self.status = Some((msg, true));
                        }
                        Err(e) => self.status = Some((e, false)),
                    }
                }
                if ui
                    .add_enabled(self.dirty(), egui::Button::new("Discard edits"))
                    .clicked()
                {
                    self.rows = self.saved.clone();
                    self.capturing = None;
                    self.confirm = None;
                    self.refused = None;
                }
                if ui.button("Reload file").clicked() {
                    self.load();
                }
                if self.dirty() {
                    theme::pill(ui, "unsaved", theme::WARN);
                }
            });

            for e in errs {
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(e).color(theme::BAD).size(12.0)).wrap(),
                );
            }
            if blocked {
                ui.add_space(6.0);
                theme::hint(
                    ui,
                    "Every button is on Replace. Put at least one back to Augment \
                     before saving, so the mouse keeps a working click.",
                );
            }

            if let Some((msg, good)) = self.status.clone() {
                ui.add_space(10.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(msg)
                            .color(if good { theme::GOOD } else { theme::BAD })
                            .size(12.0),
                    )
                    .wrap(),
                );
            }

            ui.add_space(10.0);
            theme::hint(
                ui,
                if snap.has_button_spy {
                    "Saving writes the bindings file and silences the Replace buttons in \
                     the mouse. It does not make anything act on the presses: that is the \
                     background service's job, so check the Service tab before silencing \
                     a button you need."
                } else {
                    "Saving writes the bindings file. Nothing is applied, because this \
                     device cannot report button presses."
                },
            );
        });
    }

    fn draw_problems(&mut self, ui: &mut egui::Ui) {
        if self.problems.is_empty() {
            return;
        }
        let problems = self.problems.clone();
        theme::section(ui, "Lines the file could not use", |ui| {
            for p in &problems {
                ui.add(
                    egui::Label::new(egui::RichText::new(p).color(theme::WARN).size(12.0)).wrap(),
                );
            }
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Those buttons are unbound until they are set here. Saving rewrites the \
                 file, which drops the bad lines.",
            );
        });
    }
}

fn mouse_label(code: u16) -> String {
    actions::MOUSE_BUTTONS
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(n, _)| (*n).to_string())
        .unwrap_or_else(|| format!("code {code}"))
}

/// Whether the newest backup the device layer reported actually landed.
///
/// Newest first, so a failed one is not masked by an older good one. Any
/// panel's backup counts, because the file it writes is the same safety net
/// wherever it was asked for.
fn backup_confirmed(snap: &Snapshot) -> Option<bool> {
    snap.outcomes
        .iter()
        .rev()
        .find(|o| o.kind == CmdKind::Backup)
        .map(|o| o.ok)
}

fn default_backup_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/logi-backup.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action type, both modes, in config syntax.
    const SRC: &str = "button 1 = key ctrl+c\n\
                       button 2 = text Hello there\n\
                       button 3 replace = mouse middle\n\
                       button 4 replace = command notify-send hi\n\
                       button 5 replace = nothing\n";

    fn editor(src: &str) -> BindingsEditor {
        let mut e = BindingsEditor::new();
        e.set_from_text(src);
        e.loaded = true;
        e
    }

    #[test]
    fn a_blank_editor_has_one_row_per_possible_button() {
        let e = BindingsEditor::new();
        assert_eq!(e.rows.len(), MAX_BUTTON as usize);
        assert_eq!(e.rows[0].button, 1);
        assert_eq!(e.rows[MAX_BUTTON as usize - 1].button, MAX_BUTTON);
        assert!(!e.dirty());
        assert!(e.rows.iter().all(|r| !r.is_set()));
    }

    #[test]
    fn the_edit_model_loads_every_action_type() {
        let e = editor(SRC);
        assert!(e.problems.is_empty(), "{:?}", e.problems);
        let k = |b: u8| e.rows.iter().find(|r| r.button == b).unwrap();
        assert_eq!(k(1).kind, Kind::Key);
        assert_eq!(k(1).key, "ctrl+c");
        assert_eq!(k(1).mode, BindMode::Augment);
        assert_eq!(k(2).kind, Kind::Text);
        assert_eq!(k(2).text, "Hello there");
        assert_eq!(k(3).kind, Kind::Mouse);
        assert_eq!(k(3).mouse, 0x112);
        assert_eq!(k(3).mode, BindMode::Replace);
        assert_eq!(k(4).kind, Kind::Command);
        assert_eq!(k(4).command, "notify-send hi");
        assert_eq!(k(5).kind, Kind::None);
        assert_eq!(k(5).mode, BindMode::Replace);
        // Untouched buttons stay unbound and are not written out.
        assert!(!k(6).is_set());
    }

    #[test]
    fn a_config_with_every_action_type_round_trips() {
        let e = editor(SRC);
        let keys = keymap();
        let (text, _) = e.render(&keys).expect("renders");
        assert!(text.starts_with('#'), "the header is kept");

        // The rendered file has to load back to the same model, and then to
        // the same bytes: that is what makes saving idempotent.
        let again = editor(&text);
        assert!(again.problems.is_empty(), "{:?}", again.problems);
        assert_eq!(e.rows, again.rows);
        let (text2, _) = again.render(&keys).expect("renders");
        assert_eq!(text, text2);

        // And it is the same set of bindings actions.rs would parse.
        let (a, _) = Bindings::parse(SRC, &keys);
        let (b, _) = Bindings::parse(&text, &keys);
        assert_eq!(a.list, b.list);
    }

    #[test]
    fn only_replace_rows_are_silenced() {
        let e = editor(SRC);
        let keys = keymap();
        let (_, silence) = e.render(&keys).unwrap();
        assert_eq!(silence, vec![3, 4, 5]);
        // Augmented buttons must never show up here: that would take a
        // working click away from someone who only asked to add to it.
        assert!(!silence.contains(&1));
        assert!(!silence.contains(&2));
    }

    #[test]
    fn an_unbound_augment_row_is_not_written_but_replace_nothing_is() {
        let mut e = editor("");
        assert_eq!(e.render(&keymap()).unwrap().0, SAVE_HEADER);
        e.set_replace(7);
        let (text, silence) = e.render(&keymap()).unwrap();
        assert!(text.contains("button 7 replace = nothing"), "{text}");
        assert_eq!(silence, vec![7]);
    }

    #[test]
    fn replace_on_every_button_is_refused() {
        let mut e = editor("");
        // A five-button mouse: four Replaces are fine, the fifth is not.
        for b in 1..=4 {
            e.set_replace(b);
        }
        assert!(!all_replaced(&e.rows, 5));
        e.want_replace(5, 5);
        assert!(e.refused.is_some(), "the fifth Replace must be refused");
        assert_eq!(
            e.rows.iter().find(|r| r.button == 5).unwrap().mode,
            BindMode::Augment
        );
        assert!(!all_replaced(&e.rows, 5));
        // And save refuses it too, even if the control somehow let it through.
        e.set_replace(5);
        assert!(all_replaced(&e.rows, 5));
        assert!(e.save(5).is_err());
    }

    /// The refusal is about the mouse in your hand, not about the editor's
    /// row list. There is one row per *possible* button, so counting rows
    /// always yields MAX_BUTTON and the refusal would never fire on a mouse
    /// with fewer buttons than that — which is every mouse this app has seen.
    #[test]
    fn the_all_replaced_refusal_counts_the_device_s_buttons_not_the_rows() {
        let mut e = editor("");
        // A five-button mouse with four of them already replaced.
        for b in 1..=4 {
            e.set_replace(b);
        }
        e.want_replace(5, 5);
        assert!(e.refused.is_some(), "the fifth of five must be refused");
        assert_eq!(
            e.rows.iter().find(|r| r.button == 5).unwrap().mode,
            BindMode::Augment
        );

        // The same state measured against MAX_BUTTON is not refused at all:
        // that is what the UI used to pass in, and the bug it caused.
        assert!(!all_replaced(&e.rows, MAX_BUTTON));
        e.refused = None;
        e.want_replace(5, MAX_BUTTON);
        assert!(
            e.refused.is_none(),
            "documents the wrong-count behaviour the UI must not have"
        );
    }

    #[test]
    fn replace_on_left_click_needs_confirming() {
        let mut e = editor("");
        e.want_replace(1, 5);
        // Staged, not applied: the row is still Augment until confirmed.
        assert_eq!(e.confirm, Some(1));
        assert_eq!(
            e.rows.iter().find(|r| r.button == 1).unwrap().mode,
            BindMode::Augment
        );
        e.set_replace(1);
        assert_eq!(e.confirm, None);
        assert_eq!(
            e.rows.iter().find(|r| r.button == 1).unwrap().mode,
            BindMode::Replace
        );
        // Going back is never refused.
        e.set_augment(1);
        assert_eq!(
            e.rows.iter().find(|r| r.button == 1).unwrap().mode,
            BindMode::Augment
        );
    }

    /// The confirmation card outlives the click that opened it. Replacing
    /// every other button while it is up must not let "Replace it anyway"
    /// finish the job the refusal exists to stop.
    #[test]
    fn confirming_left_click_rechecks_the_all_replaced_refusal() {
        let mut e = editor("");
        for b in 2..=4 {
            e.set_replace(b);
        }
        e.want_replace(1, 5);
        assert_eq!(e.confirm, Some(1));
        // Meanwhile the last other button is replaced: 1 is now the only one
        // still clicking.
        e.want_replace(5, 5);
        assert!(e.refused.is_none());
        e.confirm_replace(1, 5);
        assert!(e.refused.is_some(), "the confirm click bypassed the guard");
        assert_eq!(e.confirm, None);
        assert!(!all_replaced(&e.rows, 5));
        assert_eq!(
            e.rows.iter().find(|r| r.button == 1).unwrap().mode,
            BindMode::Augment
        );

        // And with room to spare it goes through.
        e.set_augment(5);
        e.want_replace(1, 5);
        e.confirm_replace(1, 5);
        assert_eq!(
            e.rows.iter().find(|r| r.button == 1).unwrap().mode,
            BindMode::Replace
        );
    }

    #[test]
    fn an_unknown_key_name_surfaces_a_useful_message() {
        let mut e = editor("");
        {
            let r = e.rows.iter_mut().find(|r| r.button == 9).unwrap();
            r.kind = Kind::Key;
            r.key = "ctrl+nosuchkey".into();
        }
        let keys = keymap();
        let errs = e.render(&keys).expect_err("must not render");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("button 9"), "{errs:?}");
        assert!(errs[0].contains("nosuchkey"), "{errs:?}");
        assert!(errs[0].contains("unknown key name"), "{errs:?}");

        // An empty key field says what to do rather than what failed.
        e.rows.iter_mut().find(|r| r.button == 9).unwrap().key.clear();
        let errs = e.render(&keys).expect_err("must not render");
        assert!(errs[0].contains("Capture"), "{errs:?}");
    }

    #[test]
    fn a_bad_line_in_the_file_is_reported_and_the_rest_loads() {
        let e = editor("button 1 = key ctrl+c\nnonsense\nbutton 2 = key f5\n");
        assert_eq!(e.problems.len(), 1, "{:?}", e.problems);
        assert_eq!(e.rows.iter().filter(|r| r.is_set()).count(), 2);
    }

    #[test]
    fn dirty_tracks_edits_against_what_was_loaded() {
        let mut e = editor(SRC);
        assert!(!e.dirty());
        e.set_replace(9);
        assert!(e.dirty());
        e.set_augment(9);
        assert!(!e.dirty(), "back to the loaded state is not dirty");
    }

    #[test]
    fn captured_chords_become_config_syntax() {
        let plain = egui::Modifiers::NONE;
        assert_eq!(chord_text("F5", plain), "f5");
        assert_eq!(chord_text("A", plain), "a");
        assert_eq!(chord_text("Num3", plain), "3");
        assert_eq!(chord_text("ArrowUp", plain), "arrowup");
        assert_eq!(chord_text("OpenBracket", plain), "leftbrace");
        let cs = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(chord_text("F5", cs), "ctrl+shift+f5");

        // And whatever capture produces has to parse as an action.
        let keys = keymap();
        for name in ["F5", "A", "Num3", "ArrowUp", "OpenBracket", "Period", "Quote"] {
            let chord = chord_text(name, cs);
            Action::parse(&format!("key {chord}"), &keys)
                .unwrap_or_else(|e| panic!("{name} -> {chord}: {e}"));
        }
    }

    /// Every key egui can report has to come out of capture as the keyboard
    /// key it was, or as an error the row shows — never as another key, and
    /// above all never as a mouse button. `Key::name` is egui's own text, not
    /// the driver's, and the two disagree: its arrows are "Left" and "Right",
    /// which uinput reads as BTN_LEFT and BTN_RIGHT.
    #[test]
    fn no_captured_key_becomes_a_mouse_button_or_the_wrong_key() {
        let keys = keymap();
        let code_of = |name: &str, m: egui::Modifiers| -> Result<u16, String> {
            match Action::parse(&format!("key {}", chord_text(name, m)), &keys)? {
                Action::Key { code, .. } => Ok(code),
                o => panic!("{name}: {o:?}"),
            }
        };
        let none = egui::Modifiers::NONE;
        for &k in egui::Key::ALL {
            if let Ok(code) = code_of(k.name(), none) {
                assert!(code < 0x100, "{:?} was captured as a mouse button ({code})", k);
            }
        }
        assert_eq!(code_of("Left", none), Ok(105));
        assert_eq!(code_of("Right", none), Ok(106));
        assert_eq!(code_of("Up", none), Ok(103));
        assert_eq!(code_of("Down", none), Ok(108));
        assert_eq!(code_of("OpenCurlyBracket", none), Ok(26));
        assert_eq!(code_of("CloseCurlyBracket", none), Ok(27));
        // Shift+= is the main block's plus; a bare Plus is the keypad's.
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        assert_eq!(code_of("Plus", shift), Ok(13));
        assert_eq!(code_of("Plus", none), Ok(78));
        assert_eq!(chord_text("Left", shift), "shift+arrowleft");
    }

    /// The frame's write-back must not be able to undo a mode click.
    ///
    /// `draw_row` clones the row before any widget runs, then Augment and
    /// Replace write straight to the model part-way through the frame, because
    /// only `set_augment` / `want_replace` run the refusals. Copying the stale
    /// clone's mode back over the model afterwards reverted every such click
    /// in the same frame — including the click back off Replace, which is the
    /// one-step way out of a silenced button.
    #[test]
    fn the_frame_write_back_cannot_revert_a_mode_click() {
        // Replace clicked mid-frame, on a copy taken while it was Augment.
        let stale = Row::blank(1);
        let mut model = Row::blank(1);
        model.mode = BindMode::Replace;
        merge_row(&mut model, stale.clone());
        assert_eq!(model.mode, BindMode::Replace, "the Replace click was reverted");

        // And the way back out, which matters more: Augment clicked on a row
        // the frame cloned while it was Replace.
        let mut stale_replaced = Row::blank(2);
        stale_replaced.mode = BindMode::Replace;
        let mut model = stale_replaced.clone();
        model.mode = BindMode::Augment;
        merge_row(&mut model, stale_replaced);
        assert_eq!(model.mode, BindMode::Augment, "the Augment click was reverted");
    }

    /// Everything that is not the mode still comes from the frame's copy:
    /// that is how the text fields and the kind menu get written back at all.
    #[test]
    fn the_frame_write_back_still_carries_what_the_widgets_edited() {
        let mut model = Row::blank(3);
        model.mode = BindMode::Replace;
        let mut edited = Row::blank(3);
        edited.kind = Kind::Text;
        edited.text = "hello".into();
        merge_row(&mut model, edited);
        assert_eq!(model.kind, Kind::Text);
        assert_eq!(model.text, "hello");
        assert_eq!(model.mode, BindMode::Replace, "the mode is the model's");
    }

    /// The pill next to Restore says whether there is a backup to fall back
    /// on, so it has to follow what the device layer reported, not the click.
    #[test]
    fn a_failed_backup_does_not_count_as_a_backup() {
        use crate::device::Outcome;
        let mut snap = Snapshot::default();
        assert_eq!(backup_confirmed(&snap), None, "nothing asked for yet");

        snap.record(Outcome::ok(1, CmdKind::Backup, None));
        assert_eq!(backup_confirmed(&snap), Some(true));

        // A later failure is what the user is standing in, not the older win.
        snap.record(Outcome::failed(2, CmdKind::Backup, "read-only"));
        assert_eq!(backup_confirmed(&snap), Some(false));

        // Another panel's unrelated failure must not read as a bad backup.
        snap.record(Outcome::failed(3, CmdKind::SetDpi, "nope"));
        assert_eq!(backup_confirmed(&snap), Some(false));
    }

    /// Painting must not be able to edit the model. Rows are cloned, edited
    /// and merged back every frame, so a paint with no input at all has to
    /// leave the editor exactly where it was — including not-dirty.
    #[test]
    fn painting_the_rows_changes_nothing() {
        use std::cell::RefCell;
        let ed = RefCell::new(editor(SRC));
        egui::__run_test_ui(|ui| {
            let mut e = ed.borrow_mut();
            let keys = keymap();
            e.draw_rows(ui, 11, &keys);
        });
        let e = ed.borrow();
        assert!(!e.dirty(), "painting made the editor think it had edits");
        assert_eq!(e.rows, editor(SRC).rows);
    }

    /// Saving with nothing on Replace still has to reach the device.
    ///
    /// `buttonspy::silence` writes exactly the set it is handed and returns
    /// every other button to its own click, so the empty set is the un-silence
    /// — not a no-op. Dropping it here left a button the user had just moved
    /// off Replace dead in the mouse, across unplugging, with only the factory
    /// table to dig it out.
    #[test]
    fn saving_sends_the_whole_silenced_set_including_the_empty_one() {
        let mut e = editor("");

        let msg = e.queue_apply(true, Vec::new(), 0);
        assert_eq!(
            e.take_requests(),
            vec![Request::ApplyBindings { silence: Vec::new() }],
            "an empty set must still be applied: it is what gives a click back"
        );
        assert!(msg.contains("click again"), "{msg}");

        // A non-empty one carries exactly the Replace rows, nothing more.
        let msg = e.queue_apply(true, vec![3, 4], 0);
        assert_eq!(
            e.take_requests(),
            vec![Request::ApplyBindings { silence: vec![3, 4] }]
        );
        assert!(msg.contains("silence 2 button(s)"), "{msg}");

        // And nothing at all is asked of a mouse that cannot report presses:
        // silencing a button there is silencing it with nothing listening.
        let msg = e.queue_apply(false, vec![3], 0);
        assert!(e.take_requests().is_empty(), "nothing may be applied");
        assert!(msg.contains("not applied"), "{msg}");
    }

    /// The status line may not say a button was given back until the device
    /// says so, and a failed apply has to turn it into a warning.
    #[test]
    fn the_status_follows_what_the_device_answered() {
        use crate::device::Outcome;
        let mut e = editor("");
        let mut snap = Snapshot::default();
        snap.record(Outcome::ok(1, CmdKind::ApplyBindings, Some("old".into())));
        let after = snap.newest_outcome_id();
        e.queue_apply(true, vec![3], after);
        assert_eq!(e.awaiting_apply, Some(after));

        // Before any answer, and with an older answer on the books: unchanged.
        e.settle_apply(&snap);
        assert_eq!(e.awaiting_apply, Some(after));
        // Someone else's command is not ours.
        snap.record(Outcome::failed(2, CmdKind::SetDpi, "nope"));
        e.settle_apply(&snap);
        assert_eq!(e.awaiting_apply, Some(after));

        snap.record(Outcome::failed(3, CmdKind::ApplyBindings, "device unplugged"));
        e.settle_apply(&snap);
        assert_eq!(e.awaiting_apply, None);
        let (msg, good) = e.status.clone().unwrap();
        assert!(!good, "{msg}");
        assert!(msg.contains("device unplugged") && msg.contains("Restore"), "{msg}");

        // And a success is reported as the device's own words.
        e.queue_apply(true, Vec::new(), snap.newest_outcome_id());
        snap.record(Outcome::ok(
            4,
            CmdKind::ApplyBindings,
            Some("all physical buttons restored".into()),
        ));
        e.settle_apply(&snap);
        let (msg, good) = e.status.clone().unwrap();
        assert!(good && msg.contains("all physical buttons restored"), "{msg}");
    }

    /// Two Saves can be in flight together. The first one's answer is also
    /// newer than the id seen before the second was sent, so the status has to
    /// wait for the last answer: a failure of the newest Save must not be
    /// shadowed by the older one's success.
    #[test]
    fn two_saves_in_flight_settle_on_the_last_answer() {
        use crate::device::Outcome;
        let mut e = editor("");
        let mut snap = Snapshot::default();
        let base = snap.newest_outcome_id();
        e.queue_apply(true, vec![3], base);
        // The second Save is clicked before the first is answered.
        e.queue_apply(true, Vec::new(), snap.newest_outcome_id());
        assert_eq!(e.pending_applies, 2);

        snap.record(Outcome::ok(1, CmdKind::ApplyBindings, Some("first".into())));
        e.settle_apply(&snap);
        assert_eq!(e.pending_applies, 1);
        assert!(e.awaiting_apply.is_some(), "still waiting on the second");
        let still_asking = e.status.clone();
        assert!(still_asking.is_none(), "the first answer must not settle the status");

        snap.record(Outcome::failed(2, CmdKind::ApplyBindings, "device unplugged"));
        e.settle_apply(&snap);
        assert_eq!(e.awaiting_apply, None);
        let (msg, good) = e.status.clone().unwrap();
        assert!(!good && msg.contains("device unplugged"), "{msg}");
    }

    /// A Save with no mouse to send it to is not undone by an older apply
    /// answering afterwards.
    #[test]
    fn an_older_apply_does_not_overwrite_a_save_that_was_never_sent() {
        use crate::device::Outcome;
        let mut e = editor("");
        let mut snap = Snapshot::default();
        e.queue_apply(true, vec![3], 0);
        let msg = e.queue_apply(false, vec![3], 0);
        e.status = Some((msg, true));
        snap.record(Outcome::ok(1, CmdKind::ApplyBindings, Some("done".into())));
        e.settle_apply(&snap);
        let (msg, _) = e.status.clone().unwrap();
        assert!(msg.contains("not applied"), "{msg}");

        // A failure of that older apply is still news: the table is unknown.
        let mut e = editor("");
        let mut snap = Snapshot::default();
        e.queue_apply(true, vec![3], 0);
        e.queue_apply(false, vec![3], 0);
        snap.record(Outcome::failed(1, CmdKind::ApplyBindings, "timed out"));
        e.settle_apply(&snap);
        assert!(!e.status.clone().unwrap().1);
    }

    /// Capture armed, then the user leaves the tab: coming back must not find
    /// it still armed and eat the first key typed there.
    #[test]
    fn capture_does_not_survive_leaving_the_tab() {
        let ctx = egui::Context::default();
        let mut e = editor("");
        let mut frame = |e: &mut BindingsEditor, draws_panel: bool| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if draws_panel {
                        e.poll_capture(ui);
                    }
                });
            });
        };
        frame(&mut e, true);
        e.capturing = Some(4);
        frame(&mut e, true);
        assert_eq!(e.capturing, Some(4), "consecutive frames keep it armed");
        frame(&mut e, false);
        frame(&mut e, false);
        frame(&mut e, true);
        assert_eq!(e.capturing, None, "a gap in frames means the tab was left");
    }

    /// A captured key belongs to the capture and to nothing else.
    ///
    /// Capture is armed by clicking a focusable widget, so that widget still
    /// holds keyboard focus. Leaving the press in the queue let it act on the
    /// same key: Space or Enter clicked the Capture control a second time and
    /// re-armed capture, which then ate the next key typed anywhere in the app.
    #[test]
    fn a_captured_key_is_taken_out_of_the_frame() {
        use std::cell::RefCell;
        let ed = RefCell::new(editor(""));
        let left = RefCell::new(usize::MAX);
        egui::__run_test_ui(|ui| {
            let mut e = ed.borrow_mut();
            e.capturing = Some(4);
            ui.input_mut(|i| {
                i.events.push(egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
                // The same press on a printable key: this is what a focused
                // TextEdit would have inserted.
                i.events.push(egui::Event::Text(" ".into()));
                i.events.push(egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            });
            e.poll_capture(ui);
            *left.borrow_mut() = ui.input(|i| i.events.len());
        });
        let e = ed.borrow();
        assert_eq!(e.capturing, None, "the capture did not finish");
        assert_eq!(e.rows.iter().find(|r| r.button == 4).unwrap().key, "space");
        assert_eq!(e.rows.iter().find(|r| r.button == 4).unwrap().kind, Kind::Key);
        assert_eq!(
            *left.borrow(),
            0,
            "the press was left for the focused widget to act on as well"
        );
    }

    /// Ctrl+C is the very chord the key field's hint shows, and egui-winit
    /// never delivers it as a key press: it becomes `Event::Copy`. Capture
    /// used to wait for a `Key` that could not come, stay armed, and leave the
    /// Copy / Paste for a focused TextEdit to act on.
    #[test]
    fn clipboard_chords_are_captured_not_lost() {
        use std::cell::RefCell;
        for (event, want) in [
            (egui::Event::Copy, "ctrl+c"),
            (egui::Event::Cut, "ctrl+x"),
            (egui::Event::Paste("clip".into()), "ctrl+shift+v"),
        ] {
            let ed = RefCell::new(editor(""));
            let left = RefCell::new(usize::MAX);
            let shift = want.contains("shift");
            egui::__run_test_ui(|ui| {
                let mut e = ed.borrow_mut();
                e.capturing = Some(4);
                ui.input_mut(|i| {
                    i.modifiers = egui::Modifiers {
                        ctrl: true,
                        command: true,
                        shift,
                        ..Default::default()
                    };
                    i.events.push(event.clone());
                });
                e.poll_capture(ui);
                *left.borrow_mut() = ui.input(|i| i.events.len());
            });
            let e = ed.borrow();
            assert_eq!(e.capturing, None, "{want}: capture stayed armed");
            let row = e.rows.iter().find(|r| r.button == 4).unwrap();
            assert_eq!(row.key, want);
            assert_eq!(row.kind, Kind::Key);
            assert_eq!(*left.borrow(), 0, "{want}: left for a focused widget");
        }
    }

    /// Escape cancels, and is swallowed with everything else: a cancel that
    /// also closes whatever else listens for Escape is not a cancel.
    #[test]
    fn escape_cancels_the_capture_and_leaves_the_row_alone() {
        use std::cell::RefCell;
        let ed = RefCell::new(editor(""));
        let left = RefCell::new(usize::MAX);
        egui::__run_test_ui(|ui| {
            let mut e = ed.borrow_mut();
            e.capturing = Some(4);
            ui.input_mut(|i| {
                i.events.push(egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            });
            e.poll_capture(ui);
            *left.borrow_mut() = ui.input(|i| i.events.len());
        });
        let e = ed.borrow();
        assert_eq!(e.capturing, None);
        assert!(e.rows.iter().find(|r| r.button == 4).unwrap().key.is_empty());
        assert_eq!(e.rows.iter().find(|r| r.button == 4).unwrap().kind, Kind::None);
        assert_eq!(*left.borrow(), 0);
    }

    #[test]
    fn a_multi_line_value_is_refused_rather_than_truncated() {
        let mut e = editor("");
        {
            let r = e.rows.iter_mut().find(|r| r.button == 4).unwrap();
            r.kind = Kind::Command;
            r.command = "echo one\necho two".into();
        }
        let errs = e.render(&keymap()).expect_err("must not render");
        assert!(errs[0].contains("one line"), "{errs:?}");
    }
}
