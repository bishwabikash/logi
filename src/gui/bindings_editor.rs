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
//!     `Request::RestoreIdentity` is always one button away.
//!   * The config file is written only when the user asks. No autosave on
//!     keystroke: the file is read by the service, and half-typed bindings
//!     would be applied.

use super::theme;
use crate::actions::{self, Action, BindMode, Binding, Bindings, FnKeymap, Keymap, MAX_BUTTON};
use crate::device::{Cmd, Snapshot};
use crate::uinput;
use eframe::egui;

/// Something the editor needs from the device layer that `device::Cmd` cannot
/// express yet. One new `Cmd` variant each; see the integration notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Start button reporting, run the host side from the saved config, and
    /// silence exactly these buttons — every button *not* listed goes back to
    /// its physical function, which is how Replace is undone. Empty is
    /// meaningful: it means "silence nothing".
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
        "openbracket" => "leftbrace".into(),
        "closebracket" => "rightbrace".into(),
        "pipe" => "backslash".into(),
        "questionmark" => "slash".into(),
        "colon" => "semicolon".into(),
        "exclamationmark" => "1".into(),
        _ => n,
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
    s.push_str(&driver_key_name(key_name));
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
    /// Button waiting for a second click to confirm Replace.
    confirm: Option<u8>,
    /// Why the last Replace was refused outright.
    refused: Option<String>,
    backup_path: String,
    backed_up: bool,
    requests: Vec<Request>,
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
            confirm: None,
            refused: None,
            backup_path: default_backup_path(),
            backed_up: false,
            requests: Vec::new(),
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
        std::fs::write(&tmp, &text)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
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
    fn poll_capture(&mut self, ui: &egui::Ui) {
        let Some(button) = self.capturing else { return };
        let got = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } => Some((key.name(), *modifiers)),
                _ => None,
            })
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

    /// Ask for Replace on one button, with the two refusals that matter.
    fn want_replace(&mut self, button: u8, count: u8) {
        let mut probe = self.rows.clone();
        if let Some(r) = probe.iter_mut().find(|r| r.button == button) {
            r.mode = BindMode::Replace;
        }
        if all_replaced(&probe, count) {
            self.refused = Some(format!(
                "Button {button} is the last one still clicking. Replace on every \
                 button would leave the mouse with no physical click at all."
            ));
            return;
        }
        self.refused = None;
        if button == 1 {
            // Left click. Needs a second, deliberate click.
            self.confirm = Some(button);
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

        // An unknown device gets the full set of rows, so the file can be
        // written before the mouse is plugged in.
        let count = if snap.button_count == 0 {
            MAX_BUTTON
        } else {
            snap.button_count.min(MAX_BUTTON)
        };
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
                 until something puts the table back, including across unplugging.",
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
                                 visible to this window, so type super+l for that one.",
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
                                self.set_replace(button);
                            }
                        });
                    });
                }
            });
        });

        if self.rows[idx] != row {
            self.rows[idx] = row;
        }
    }

    fn draw_safety(
        &mut self,
        ui: &mut egui::Ui,
        snap: &Snapshot,
        silence: &[u8],
        _count: u8,
        cmds: &mut Vec<Cmd>,
    ) {
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

        if silence.is_empty() {
            return;
        }

        theme::section(ui, "Replace safety", |ui| {
            let list: Vec<String> = silence.iter().map(|b| b.to_string()).collect();
            theme::row(ui, "silenced", &list.join(", "));
            ui.add_space(8.0);
            theme::hint(
                ui,
                "These buttons stop working physically once the bindings are applied. \
                 Every other button is put back to its own function at the same time.",
            );

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Restore physical buttons").clicked() {
                    self.requests.push(Request::RestoreIdentity);
                }
                if !self.backed_up {
                    theme::pill(ui, "no backup taken", theme::WARN);
                }
            });
            ui.add_space(6.0);
            theme::hint(
                ui,
                &format!(
                    "Restore writes the factory remapping table, so every button clicks \
                     again. It is also {RESTORE_CLI} from a terminal, for a mouse too \
                     dead to press this button with."
                ),
            );

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 90.0).max(160.0);
                ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                if ui
                    .add_enabled(snap.has_profiles, egui::Button::new("Back up"))
                    .clicked()
                {
                    cmds.push(Cmd::Backup(self.backup_path.clone()));
                    self.backed_up = true;
                }
            });
            ui.add_space(6.0);
            theme::hint(
                ui,
                "A backup covers the stored profiles, which is what a mis-set binding \
                 is most likely to send you back to. The remapping table is not in it; \
                 Restore above is that one's escape.",
            );
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
                            let n = silence.len();
                            self.requests.push(Request::ApplyBindings { silence });
                            self.status = Some((
                                format!(
                                    "saved to {}; {n} button(s) to silence",
                                    actions::config_path().display()
                                ),
                                true,
                            ));
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
                    "Saving writes the bindings file and applies it: the mouse starts \
                     reporting presses and the Replace buttons are silenced."
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
