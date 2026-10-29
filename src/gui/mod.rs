//! egui front end. All device work goes through `device::Handle`, which talks
//! to a worker thread, so nothing here can block the frame loop.

use crate::device::{self, Cmd, Snapshot};
use crate::dpi::Capability;
use crate::led;
use crate::profile_blob::{Button, Led, BTN_HID, HID_MOUSE, SPECIALS};
use eframe::egui;

pub mod bindings_editor;
pub mod profile_editor;
pub mod rules_editor;
pub mod theme;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sensor,
    Lighting,
    Profiles,
    Buttons,
    Apps,
    Service,
    About,
}

pub struct App {
    dev: device::Handle,
    tab: Tab,
    snap: Snapshot,

    // Sensor
    dpi: u16,

    // Lighting
    zone_sel: usize,
    all_zones: bool,
    mode: u8,
    color: [f32; 3],
    period_ms: u16,
    waveform: usize,
    intensity: u8,

    // Effects
    effect: usize,
    effect_period_ms: u32,

    profile_sel: usize,
    backup_path: String,
    /// Paths a backup was sent to from this tab. The file is written later, on
    /// the device thread, so it may not exist yet when the next click looks.
    backup_sent: Vec<String>,

    /// Pending button edits, keyed by button index, applied on click.
    btn_action: std::collections::HashMap<u8, String>,
    /// Which profile the pending edits above were chosen against. A choice
    /// made for one profile must not stay armed over another: Apply writes
    /// flash.
    btn_profile: Option<u8>,
    /// The newest flash write (or profile switch) sent from here that the
    /// worker has not yet answered, and when it was sent.
    ///
    /// The panels decide what to offer from the snapshot, and the snapshot only
    /// catches up once the worker has written and read back. A second write
    /// armed in that window is judged against sectors that are already out of
    /// date: two button changes that each leave a left click behind can,
    /// together, leave none. So writes wait for the previous one's answer.
    flash_inflight: Option<(device::CmdId, std::time::Instant)>,

    themed: bool,
    profile_editor: profile_editor::ProfileEditor,
    rules: rules_editor::RulesEditor,
    bindings: bindings_editor::BindingsEditor,
    service: crate::service::Watcher,
    /// Editors read their config files once, off the first paint.
    loaded: bool,
    /// None when there is no StatusNotifierWatcher, which is normal on a
    /// desktop without a tray host. The app works either way.
    tray: Option<crate::tray::Tray>,
    tray_tried: bool,
    /// Registration in flight. `Tray::spawn` blocks on a session-bus round
    /// trip, and where the watcher name is activatable but never comes up it
    /// blocks for the 25s D-Bus reply timeout, so it runs on its own thread
    /// and the handle arrives through here.
    tray_rx: Option<std::sync::mpsc::Receiver<Result<crate::tray::Tray, String>>>,
    /// True while the DPI slider is being dragged, so the live readout is not
    /// overwritten by the device's own value mid-drag.
    dpi_touched: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            dev: device::spawn(),
            tab: Tab::Sensor,
            snap: Snapshot::default(),
            dpi: 1600,
            zone_sel: 0,
            all_zones: true,
            mode: led::FIXED,
            color: [0.0, 1.0, 1.0],
            period_ms: 4000,
            waveform: 0,
            intensity: 100,
            effect: 0,
            effect_period_ms: 4000,
            profile_sel: 0,
            backup_path: default_backup_path(),
            backup_sent: Vec::new(),
            btn_action: std::collections::HashMap::new(),
            btn_profile: None,
            flash_inflight: None,
            themed: false,
            profile_editor: profile_editor::ProfileEditor::new(),
            rules: rules_editor::RulesEditor::new(),
            bindings: bindings_editor::BindingsEditor::new(),
            service: crate::service::Watcher::new(),
            loaded: false,
            tray: None,
            tray_tried: false,
            tray_rx: None,
            dpi_touched: false,
        }
    }

    /// Send a command, and remember it if it writes flash. See `flash_inflight`.
    fn send_tracked(&mut self, cmd: Cmd) {
        let flash = holds_the_panels(&cmd);
        let id = self.dev.send(cmd);
        if flash {
            self.flash_inflight = Some((id, std::time::Instant::now()));
        }
    }

    /// Send a panel's command, whichever panel it came from.
    ///
    /// A backup is the way back from a flash write, so none may land on a file
    /// that is already there. Three panels send one: this tab, the profile
    /// editor and the host-bindings editor, whose "Back up" button used to send
    /// the same default path on every press. Each numbers for itself only
    /// against what it has sent, so a path another panel had just sent (the
    /// file is written later, on the device thread) or one it did not know
    /// about could be sent twice, and the second dump, taken after a bad
    /// write, replaced the good one taken before it. Choosing the path here,
    /// where every panel's backup passes, is what makes the check shared.
    fn dispatch(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Backup(path) => {
                self.send_backup(path);
            }
            other => self.send_tracked(other),
        }
    }

    /// Send a backup to `path`, or to the first numbered path beside it that
    /// nothing is at and nothing has been sent to. Returns where it went.
    fn send_backup(&mut self, path: String) -> String {
        // Trimmed here, where every panel's backup passes: the enable checks
        // already ignore surrounding blanks, but the path kept them, so a
        // pasted path with a trailing space made a file whose name ends in
        // one, which nothing later looks for.
        let path = path.trim().to_string();
        let sent = &self.backup_sent;
        let target = free_backup_path(&path, |p| {
            sent.iter().any(|s| s == p) || std::path::Path::new(p).exists()
        });
        self.backup_sent.push(target.clone());
        self.dev.send(Cmd::Backup(target.clone()));
        target
    }

    fn rgb(&self) -> [u8; 3] {
        [
            (self.color[0] * 255.0).round() as u8,
            (self.color[1] * 255.0).round() as u8,
            (self.color[2] * 255.0).round() as u8,
        ]
    }

    fn zones(&self) -> Vec<u8> {
        if self.all_zones {
            self.snap.zones.iter().map(|z| z.index).collect()
        } else {
            self.snap
                .zones
                .get(self.zone_sel)
                .map(|z| vec![z.index])
                .unwrap_or_default()
        }
    }
}

/// True if this binding presses the left mouse button.
fn presses_left(b: &Button) -> bool {
    b.0[0] == BTN_HID && b.0[1] == HID_MOUSE && u16::from_be_bytes([b.0[2], b.0[3]]) & 1 != 0
}

/// Whether replacing `buttons[index]` with `new` would take away the only
/// left click the first `count` buttons have.
///
/// A mouse that cannot click survives unplugging and looks exactly like broken
/// hardware, and this grid writes flash one button at a time with no staging
/// step like the profile editor's. A profile that already has no left click is
/// not made worse by a change, so it stays editable: that is the way back.
fn strands_left_click(buttons: &[Button], count: usize, index: usize, new: Button) -> bool {
    let had = buttons.iter().take(count).any(presses_left);
    let has = buttons
        .iter()
        .take(count)
        .enumerate()
        .any(|(i, b)| presses_left(if i == index { &new } else { b }));
    had && !has
}

/// How long a flash write is waited on before the panels stop holding for it.
///
/// A write that is read back takes seconds, not minutes. The bound is for a
/// worker that never answers: the buttons must not stay grey for good.
const FLASH_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Commands that write an onboard profile sector.
fn writes_flash(c: &Cmd) -> bool {
    matches!(
        c,
        Cmd::PersistLighting { .. } | Cmd::WriteProfile { .. } | Cmd::SetButton { .. }
    )
}

/// Commands the panels wait out before offering another flash write.
///
/// The sector writes themselves, and a profile switch: `PersistLighting` is
/// addressed by whichever profile is active on the device when it runs, so
/// saving lighting against a switch the snapshot has not caught up with would
/// merge one profile's other zone into another profile's sector.
fn holds_the_panels(c: &Cmd) -> bool {
    writes_flash(c) || matches!(c, Cmd::SetProfile(_))
}

/// The label a button's menu shows: the user's pending choice if there is one,
/// otherwise what the device holds.
///
/// Only a choice the user made is remembered. Remembering the device's own
/// description as a default froze it, and when the table then changed
/// underneath (a write from this grid, the profile editor, a Refresh) the
/// frozen label no longer matched: Apply came up armed with a change nobody
/// chose, one that would write the old binding back to flash. A choice that
/// now matches the device has been applied, so it is dropped.
fn chosen_label(
    pending: &mut std::collections::HashMap<u8, String>,
    index: u8,
    device: &str,
) -> String {
    if pending.get(&index).is_some_and(|s| s == device) {
        pending.remove(&index);
    }
    pending.get(&index).cloned().unwrap_or_else(|| device.to_string())
}

/// Whether the command sent as `id` is still unanswered.
///
/// The worker runs commands in the order they were sent and records an
/// outcome for each, so an outcome with a higher id means this one's has been
/// recorded too, even if the ring has since pushed it out.
fn still_waiting(id: device::CmdId, newest_outcome: device::CmdId, waited: std::time::Duration) -> bool {
    newest_outcome < id && waited < FLASH_WAIT
}

/// Whether a tab has nothing to show without a mouse attached.
///
/// The service, the app rules and the host bindings are not the device's: the
/// rules and the bindings are config files and the service is a systemd unit.
/// With no mouse found, the service is often the very thing to look at (it may
/// be failing, or be what the user needs to restart), so the whole window must
/// not collapse to "no device". The bindings editor is written for exactly this
/// case: it lists every possible button and says the file can still be edited
/// and saved with no mouse, which the window never let anyone reach.
fn tab_needs_device(t: Tab) -> bool {
    !matches!(t, Tab::Apps | Tab::Service | Tab::Buttons)
}

/// Every label the per-button menu offers.
///
/// Each one has to survive `action_to_button` and come back out of
/// `Button::describe` spelled the same way: the Apply button is armed by
/// comparing the chosen label with the device's own description, so a label
/// that does not round-trip would stay armed after a successful write and
/// invite another flash write for no change.
fn button_choices() -> Vec<String> {
    let mut choices: Vec<String> = SPECIALS.iter().map(|(_, n)| n.to_string()).collect();
    for n in 1..=5u8 {
        choices.push(format!("button {n}"));
    }
    choices.push("disabled".to_string());
    choices
}

/// Map a menu label back to a binding. Labels come from the same tables that
/// produced them, so an unknown one means a bug rather than user input.
fn action_to_button(label: &str) -> Option<Button> {
    if label == "disabled" {
        return Some(Button::disabled());
    }
    if let Some(rest) = label.strip_prefix("button ") {
        return rest.trim().parse::<u8>().ok().map(Button::mouse);
    }
    Button::special(label)
}

/// Whether a lighting mode is one a profile record can encode.
///
/// `profile_blob::Led` has a constructor for exactly these four. Anything else
/// a zone reports — wave, starlight, ripple, the frame-streaming mode — would
/// be written as a fixed colour, which is not what the panel is showing.
fn storable_mode(m: u8) -> bool {
    matches!(m, led::OFF | led::FIXED | led::BREATHING | led::CYCLE)
}

/// The profile record for a lighting mode, or `None` when no record encodes it.
///
/// One place decides both "can this be stored" and "what is stored", so the
/// button cannot be enabled for a mode the record then misstates.
fn lighting_record(mode: u8, color: [u8; 3], period_ms: u16, waveform: u8, intensity: u8) -> Option<Led> {
    match mode {
        led::OFF => Some(Led::off()),
        led::FIXED => Some(Led::fixed(color)),
        led::BREATHING => Some(Led::breathing(color, period_ms, waveform, intensity)),
        led::CYCLE => Some(Led::cycle(period_ms, intensity)),
        _ => None,
    }
}

/// The modes every selected zone offers, in the first zone's order.
///
/// Read per zone off the device, and this app has no model whitelist, so zones
/// are not assumed alike. Offering what only the first one has meant "All"
/// could send a mode another zone has no effect for. `live` picks the modes
/// the live setter takes instead of the ones the zone has a record for.
fn common_modes(zones: &[&device::ZoneInfo], live: bool) -> Vec<u8> {
    let of = |z: &device::ZoneInfo| if live { z.live_modes.clone() } else { z.modes.clone() };
    let Some((first, rest)) = zones.split_first() else {
        return Vec::new();
    };
    let mut out = of(first);
    out.retain(|m| rest.iter().all(|z| of(z).contains(m)));
    out
}

/// The zones the panel is currently aimed at: all of them, or the chosen one.
fn selected_zones(zones: &[device::ZoneInfo], all_zones: bool, zone_sel: usize) -> Vec<&device::ZoneInfo> {
    if all_zones {
        zones.iter().collect()
    } else {
        zones.get(zone_sel).into_iter().collect()
    }
}

/// Which of the profile's two LED records a save would replace.
///
/// Record slot and zone position line up, the same pairing the profile editor
/// uses. "All zones" replaces only as many slots as the device has zones: on a
/// device that reports one, the second record is not this panel's to touch.
/// A selection with no slot behind it (a third zone, or none at all) replaces
/// nothing.
fn lighting_slots(all_zones: bool, zone_sel: usize, zone_count: usize) -> Vec<usize> {
    if all_zones {
        (0..zone_count.min(2)).collect()
    } else if zone_sel < zone_count && zone_sel < 2 {
        vec![zone_sel]
    } else {
        Vec::new()
    }
}

/// The two records to write, or `None` when writing them would have to guess.
///
/// The command carries both slots, so any slot not being replaced has to come
/// from what the mouse already stores. If that is not known the answer is
/// `None`, never a copy of the new record: that copy is the silent loss of the
/// other zone's lighting.
fn merged_leds(slots: &[usize], rec: Led, stored: Option<[Led; 2]>) -> Option<[Led; 2]> {
    if slots.is_empty() {
        return None;
    }
    if (0..2).all(|s| slots.contains(&s)) {
        return Some([rec, rec]);
    }
    let mut keep = stored?;
    for &s in slots {
        *keep.get_mut(s)? = rec;
    }
    Some(keep)
}

/// Why storing lighting into the onboard profile is not on offer, or `None`
/// when it is.
///
/// Every one of these is a reason a write would land as something other than
/// what the panel is showing, and the write is flash, so the button is
/// disabled and this says why rather than letting it fail afterwards.
fn store_blocked_reason(storable: bool, format: u8, slot_known: bool) -> Option<&'static str> {
    if !storable {
        return Some(
            "A profile record can hold off, fixed, breathing or cycle. This mode is not \
             one of them, so there is nothing to write that would mean what it says.",
        );
    }
    if format != 2 {
        return Some(
            "Saving reads the profile sector and writes it back, which needs the format 2 \
             layout this app decodes. This device reports another one, so nothing is \
             written.",
        );
    }
    if !slot_known {
        return Some(
            "The active profile has not been read off the mouse yet, so the other zone's \
             stored lighting is unknown and saving this one would overwrite it. Press \
             Refresh.",
        );
    }
    None
}

/// What a tray service item should do right now, or nothing.
fn tray_service_action(
    toggle: bool,
    known: bool,
    state: crate::service::State,
    busy: bool,
) -> Option<crate::service::Action> {
    use crate::service::{can_control, Action, State};
    if !known || busy || !can_control(state) {
        return None;
    }
    Some(match (toggle, state) {
        (false, _) => Action::Restart,
        (true, State::Active) => Action::Stop,
        (true, _) => Action::Start,
    })
}

/// `path` with `-n` before its extension: `a/b.json` becomes `a/b-2.json`.
///
/// The profile editor numbers its backups the same way; the two tabs share one
/// default path, so they have to agree on what "beside it" means.
fn numbered_path(path: &str, n: u32) -> String {
    let name_at = path.rfind('/').map_or(0, |i| i + 1);
    match path[name_at..].rfind('.') {
        // A leading dot is part of the name, not the start of an extension.
        Some(dot) if dot > 0 => {
            let at = name_at + dot;
            format!("{}-{n}{}", &path[..at], &path[at..])
        }
        _ => format!("{path}-{n}"),
    }
}

/// The first of `path`, `path-2`, `path-3`, ... that `taken` does not claim.
///
/// A backup is the way back from a flash write, so one taken after a bad write
/// must not replace the good one taken before it. The profile editor already
/// refuses to; this tab used to replace whatever was at the path, and both
/// default to the same file.
fn free_backup_path(path: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(path) {
        return path.to_string();
    }
    for n in 2..1000 {
        let candidate = numbered_path(path, n);
        if !taken(&candidate) {
            return candidate;
        }
    }
    numbered_path(path, 1000)
}

fn default_backup_path() -> String {
    backup_path_in(std::env::var("HOME").ok(), std::env::current_dir().ok())
}

/// The default backup file inside `home`, or inside `cwd` when there is no
/// usable home, so the path is absolute either way.
///
/// A bare `.` fallback wrote into whatever directory the app happened to be
/// started from, which for a desktop launcher is not somewhere a person looks
/// for the one file that undoes a flash write.
fn backup_path_in(home: Option<String>, cwd: Option<std::path::PathBuf>) -> String {
    let dir = home
        .filter(|h| h.starts_with('/'))
        .map(std::path::PathBuf::from)
        .or(cwd)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    dir.join("logi-backup.json").to_string_lossy().into_owned()
}

impl eframe::App for App {
    /// Hand the mouse back when the window closes.
    ///
    /// Host mode does not affect buttons, but it does stop the stored profile
    /// being applied, so a mouse left in it keeps whatever lighting this app
    /// last pushed and ignores its own profile. Restoring on exit means
    /// closing the app always returns the mouse to driving itself.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // An effect streams frames from a thread of its own, and the process
        // ending would cut that thread off wherever it was. Asking the worker
        // to stop it first is what lets it hand the mouse back and be done
        // before the restore below, rather than racing it. Bounded: the app
        // is closing, and a worker that never answers must not hold the
        // window open.
        if self.dev.snapshot().effect_running.is_some() {
            self.dev.send(Cmd::StopEffect);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while self.dev.snapshot().effect_running.is_some()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        // A flash write the worker is partway through must not be cut off by
        // the process ending: the sector is erased and rewritten in steps, and
        // an interrupted one is a profile the mouse cannot read back. The
        // restore below also talks to the device, so it waits for the write
        // too. Bounded for the same reason as above.
        if let Some((id, _)) = self.flash_inflight {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while self.dev.snapshot().newest_outcome_id() < id
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        self.dev.restore_onboard_blocking();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.themed {
            theme::apply(ctx);
            self.themed = true;
        }
        if !self.loaded {
            // File IO, so once rather than every frame.
            self.rules.load();
            self.bindings.load();
            self.loaded = true;
        }
        // Before the tray, not after: the tray renders this snapshot, and
        // reading it second had every icon update describing the frame before.
        self.snap = self.dev.snapshot();
        if !self.snap.connected {
            // A choice armed against a mouse that has gone is not one for
            // whatever comes back: Apply writes flash.
            self.btn_action.clear();
            self.btn_profile = None;
            // A drag in progress when the mouse went is never finished: the
            // slider is not drawn while there is no device, so it never sees
            // the release that clears this. Left set, it would keep the slider
            // on the dragged value after the replug instead of the mouse's own.
            self.dpi_touched = false;
        }
        if let Some((id, since)) = self.flash_inflight {
            if !still_waiting(id, self.snap.newest_outcome_id(), since.elapsed()) {
                self.flash_inflight = None;
            }
        }
        self.sync_tray(ctx);
        // The worker polls for the device, so keep repainting to pick up a
        // replug without the user having to click anything.
        // The worker can publish a change at any time, and nothing wakes the
        // window when it does, so this is the longest a change can wait to be
        // drawn. Cheap: the frame is small and nothing else is animating.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        self.draw_sidebar(ctx);
        self.draw_status(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(22, 18)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Cap the reading width, but never beyond what the panel
                    // actually has: forcing 640 here clipped every card.
                    let w = ui.available_width().min(660.0);
                    ui.set_max_width(w);
                    match self.tab {
                        t if !self.snap.connected && tab_needs_device(t) => {
                            self.draw_disconnected(ui)
                        }
                        Tab::Sensor => self.draw_sensor(ui),
                        Tab::Lighting => self.draw_lighting(ui),
                        Tab::Profiles => self.draw_profiles(ui),
                        Tab::Buttons => self.draw_buttons(ui),
                        Tab::Apps => {
                            let snap = self.snap.clone();
                            self.rules.ui(ui, &snap);
                        }
                        Tab::Service => self.draw_service(ui),
                        Tab::About => self.draw_about(ui),
                    }
                });
            });
    }
}

const PALETTE: &[[u8; 3]] = &[
    [0x2d, 0xd4, 0xd4], [0xff, 0xff, 0xff], [0xe8, 0x3b, 0x3b], [0xff, 0x7a, 0x1f],
    [0xff, 0xd4, 0x2a], [0x5a, 0xd1, 0x5a], [0x3b, 0x8c, 0xff], [0x9b, 0x5a, 0xff],
    [0xff, 0x4d, 0x9e],
];

impl App {
    /// Register the tray once, then keep it in step and act on its menu.
    fn sync_tray(&mut self, ctx: &egui::Context) {
        use crate::tray::{Event, Status};

        // Build the real status first: registering with Status::default()
        // made the first icon claim a service state nothing had looked at yet.
        let status = Status {
            device: self.snap.connected.then(|| self.snap.model.clone()),
            service: self.service.state(),
            active_profile: self.snap.active_profile,
            profile_count: self.snap.profile_count,
            dpi: self.snap.dpi,
            // The device's own range, not a fixed list: with no mouse attached
            // there is nothing to offer, and offering it anyway produced a DPI
            // menu that could not work.
            dpi_presets: match (self.snap.connected, &self.snap.dpi_cap) {
                (true, Some(Capability::Range { min, max, .. })) => {
                    [400u16, 800, 1600, 3200, 6400]
                        .into_iter()
                        .filter(|p| p >= min && p <= max)
                        .collect()
                }
                (true, Some(Capability::List(v))) => v.clone(),
                _ => Vec::new(),
            },
        };

        if !self.tray_tried {
            self.tray_tried = true;
            // Registering blocks: it opens the session bus and waits for the
            // watcher, which on a desktop where the name is activatable but
            // never comes up is the full 25s D-Bus timeout. On the frame loop
            // that is a window that never paints, so this goes on a thread of
            // its own, exactly as `Tray::spawn` says it must.
            let (tx, rx) = std::sync::mpsc::channel();
            let initial = status.clone();
            match std::thread::Builder::new()
                .name("logi-tray-register".into())
                .spawn(move || {
                    let _ = tx.send(crate::tray::Tray::spawn(initial));
                }) {
                Ok(_) => self.tray_rx = Some(rx),
                Err(e) => eprintln!("logi: no tray icon: {e}"),
            }
        }
        // Non-blocking: nothing here waits on the registration.
        match self.tray_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(Ok(t))) => {
                self.tray = Some(t);
                self.tray_rx = None;
            }
            Some(Ok(Err(e))) => {
                eprintln!("logi: no tray icon: {e}");
                self.tray_rx = None;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => self.tray_rx = None,
            _ => {}
        }
        let Some(tray) = &self.tray else { return };

        // update() is a no-op when nothing changed, so this is cheap per frame.
        tray.update(status);

        for ev in tray.poll() {
            match ev {
                Event::Open => {
                    // Un-minimise first. Focus does not restore a minimised
                    // window on most compositors, and a minimised window is
                    // the usual reason someone reaches for the tray at all,
                    // so without this the one menu item that has to work did
                    // nothing.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Event::ToggleService | Event::RestartService => {
                    // The same gate the Service tab puts on its buttons. The
                    // menu has no greyed state of its own, so without it a
                    // click before the first poll acted on a default, one
                    // during an action queued a second systemctl, and one
                    // with no unit or no systemd could only produce an error.
                    if let Some(a) = tray_service_action(
                        matches!(ev, Event::ToggleService),
                        self.service.known(),
                        self.service.state(),
                        self.service.busy().is_some(),
                    ) {
                        self.service.request(a);
                    }
                }
                Event::SetProfile(p) => {
                    self.send_tracked(Cmd::SetProfile(p));
                }
                Event::SetDpi(d) => {
                    self.dev.send(Cmd::SetDpi(d));
                }
                Event::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }

    fn draw_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(186.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::RAISED)
                    .inner_margin(egui::Margin::symmetric(14, 16)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Logi").size(21.0).strong().color(theme::TEXT));
                });
                ui.add_space(2.0);
                if self.snap.connected {
                    ui.label(
                        egui::RichText::new(self.snap.model.replace(" Gaming Mouse", ""))
                            .size(11.5)
                            .color(theme::DIM),
                    );
                    ui.add_space(9.0);
                    ui.horizontal_wrapped(|ui| {
                        theme::pill(ui, "connected", theme::GOOD);
                        let m = crate::profiles::mode_name(self.snap.mode);
                        let c = if self.snap.mode == crate::profiles::MODE_HOST {
                            theme::WARN
                        } else {
                            theme::DIM
                        };
                        theme::pill(ui, m, c);
                    });
                } else {
                    ui.add_space(9.0);
                    theme::pill(ui, "no device", theme::BAD);
                }

                ui.add_space(18.0);
                for (t, label) in [
                    (Tab::Sensor, "Sensor"),
                    (Tab::Lighting, "Lighting"),
                    (Tab::Profiles, "Profiles"),
                    (Tab::Buttons, "Buttons"),
                    (Tab::Apps, "Apps"),
                    (Tab::Service, "Service"),
                    (Tab::About, "About"),
                ] {
                    let on = self.tab == t;
                    let txt = egui::RichText::new(label)
                        .size(14.5)
                        .color(if on { theme::TEXT } else { theme::DIM });
                    let resp = ui.add_sized(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::Button::selectable(on, txt),
                    );
                    if resp.clicked() {
                        self.tab = t;
                    }
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.dev.send(Cmd::Refresh);
                    }
                    // Memory mode is the onboard-profiles feature. With no
                    // mouse, or one that reports none, the toggle could only
                    // send a command the worker refuses.
                    if !(self.snap.connected && self.snap.has_profiles) {
                        return;
                    }
                    ui.add_space(14.0);

                    // Memory mode. Host does not affect buttons or movement
                    // (measured: input is identical in both modes). What it
                    // does change is that the stored profile is not applied,
                    // so lighting is whatever this app last pushed.
                    let host = self.snap.mode == crate::profiles::MODE_HOST;
                    theme::hint(
                        ui,
                        if host {
                            "Host: this app drives the lighting. The stored profile is \
                             not applied, so lighting reverts when the app exits. \
                             Buttons and movement are unaffected."
                        } else {
                            "Onboard: the mouse drives itself from its stored profile, \
                             with nothing running."
                        },
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.selectable_label(!host, "Onboard").clicked() && host {
                            self.dev.send(Cmd::SetMode(crate::profiles::MODE_ONBOARD));
                        }
                        if ui.selectable_label(host, "Host").clicked() && !host {
                            self.dev.send(Cmd::SetMode(crate::profiles::MODE_HOST));
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("MEMORY MODE")
                            .color(theme::DIM)
                            .size(11.0)
                            .strong(),
                    );
                });
            });
    }

    fn draw_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::RAISED)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(e) = &self.snap.error {
                        ui.label(egui::RichText::new("!").color(theme::BAD).strong());
                        ui.label(egui::RichText::new(e).color(theme::BAD).size(12.5));
                    } else if let Some(n) = &self.snap.notice {
                        ui.label(egui::RichText::new("OK").color(theme::GOOD).size(12.0).strong());
                        ui.label(egui::RichText::new(n).size(12.5));
                    } else {
                        ui.label(egui::RichText::new("Ready").color(theme::DIM).size(12.5));
                    }
                    if let Some(r) = &self.snap.effect_running {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            theme::pill(ui, &format!("{r} running"), theme::ACCENT);
                        });
                    }
                });
            });
    }

    fn draw_disconnected(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        let err = self.snap.error.clone().unwrap_or_default();
        theme::card(ui, |ui| {
            // No model whitelist: this app drives any Logitech device that
            // answers HID++, so naming one model here misreads as "the wrong
            // mouse is plugged in".
            ui.label(egui::RichText::new("No Logitech device found").size(18.0).strong());
            ui.add_space(10.0);
            if err.contains("permission") {
                theme::hint(
                    ui,
                    "The mouse is attached, but this user cannot open its hidraw node.",
                );
                ui.add_space(10.0);
                ui.label("Install the udev rule, then unplug and replug the mouse:");
                ui.add_space(6.0);
                ui.code("packaging/install.sh");
                ui.add_space(10.0);
                theme::hint(
                    ui,
                    "udevadm trigger re-runs the rules, but a node that is already open \
                     keeps its old permissions, so the replug is what matters.",
                );
            } else {
                theme::hint(ui, "Plug in a Logitech mouse that speaks HID++ over USB.");
                if !err.is_empty() {
                    ui.add_space(8.0);
                    theme::hint(ui, &err);
                }
            }
        });
    }

    /// "stage 2 of 4", under the DPI figure, once the mouse says which stage is
    /// active. It is what changes when the DPI button on the mouse is pressed.
    fn stage_hint(&self, ui: &mut egui::Ui) {
        if let Some((pos, total)) = self.snap.dpi_stage() {
            theme::hint(
                ui,
                &format!("stage {pos} of {total} in profile {}", self.snap.active_profile),
            );
        }
    }

    fn draw_sensor(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_dpi && !self.snap.has_rate {
            theme::unsupported(ui, "an adjustable sensor");
            return;
        }
        let (min, max, step) = match self.snap.dpi_cap {
            Some(Capability::Range { min, max, step }) => (min, max, step),
            _ => (100, 25600, 50),
        };
        // A list sensor accepts only its own values: `Dpi::set` rejects
        // anything else outright. A slider over an invented continuous range
        // would offer nothing but commands the device refuses.
        let stages: Option<Vec<u16>> = match &self.snap.dpi_cap {
            Some(Capability::List(v)) => Some(v.clone()),
            _ => None,
        };
        if !self.dpi_touched && self.snap.dpi != 0 {
            self.dpi = self.snap.dpi;
        }

        if !self.snap.has_dpi {
            theme::section(ui, "Sensor", |ui| theme::unsupported(ui, "adjustable DPI"));
        } else if let Some(stages) = stages {
            theme::section(ui, "Sensor", |ui| {
                theme::readout(ui, &self.snap.dpi.to_string(), "DPI");
                self.stage_hint(ui);
                ui.add_space(10.0);
                if stages.is_empty() {
                    theme::hint(ui, "The sensor reported no DPI values, so there is \
                                     nothing to choose from here.");
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    for v in &stages {
                        if ui.selectable_label(self.snap.dpi == *v, format!("{v}")).clicked() {
                            self.dpi = *v;
                            self.dpi_touched = false;
                            self.dev.send(Cmd::SetDpi(*v));
                        }
                    }
                });
                ui.add_space(8.0);
                theme::hint(ui, "This sensor reports fixed stages rather than a range, \
                                 so these are the only values it accepts.");
            });
        } else if self.snap.dpi_cap.is_none() {
            // The range read failed or has not landed. Drawing a slider over
            // numbers invented here, with a hint stating them as the device's,
            // would be a claim about hardware nobody asked.
            theme::section(ui, "Sensor", |ui| {
                theme::readout(ui, &self.snap.dpi.to_string(), "DPI");
                self.stage_hint(ui);
                ui.add_space(10.0);
                theme::hint(
                    ui,
                    "The sensor has not reported which DPI values it accepts, so there \
                     is nothing to choose from here. Press Refresh.",
                );
            });
        } else {
        theme::section(ui, "Sensor", |ui| {
            theme::readout(ui, &self.snap.dpi.to_string(), "DPI");
                self.stage_hint(ui);
            ui.add_space(10.0);
            let resp = ui.add(
                egui::Slider::new(&mut self.dpi, min..=max)
                    .step_by(step as f64)
                    // Not the default `Always`: that clamps and step-snaps the
                    // existing value during the paint pass and reports it as
                    // changed, which the code below would send to the device.
                    // A mouse whose DPI is off this grid would be rewritten by
                    // nothing more than the window being drawn.
                    .clamping(egui::SliderClamping::Edits)
                    .show_value(false),
            );
            if resp.changed() {
                self.dpi_touched = true;
            }
            // Only a value the user moved to is sent. A press and release on
            // the handle with no movement used to send `self.dpi` anyway,
            // which is the device's own value, or the 1600 placeholder when it
            // has not reported one.
            if (resp.drag_stopped() && self.dpi_touched)
                || (resp.changed() && !resp.dragged())
            {
                self.dev.send(Cmd::SetDpi(self.dpi));
                self.dpi_touched = false;
            } else if resp.drag_stopped() {
                self.dpi_touched = false;
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for p in [400u16, 800, 1600, 3200, 6400, 12800, 25600] {
                    if p < min || p > max {
                        continue;
                    }
                    if ui.selectable_label(self.snap.dpi == p, format!("{p}")).clicked() {
                        self.dpi = p;
                        self.dpi_touched = false;
                        self.dev.send(Cmd::SetDpi(p));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                &format!("Continuous: {min}-{max} in steps of {step}. Not fixed stages."),
            );
        });
        }

        if !self.snap.has_rate {
            theme::section(ui, "Report rate", |ui| {
                theme::unsupported(ui, "an adjustable report rate")
            });
            return;
        }
        theme::section(ui, "Report rate", |ui| {
            theme::value(ui, &self.snap.rate.to_string(), "Hz");
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for hz in self.snap.rates.clone() {
                    if ui
                        .selectable_label(self.snap.rate == hz, format!("{hz} Hz"))
                        .clicked()
                    {
                        self.dev.send(Cmd::SetRate(hz));
                    }
                }
            });
            ui.add_space(8.0);
            // What this button actually sends is `setReportRate`, the live
            // setter. It does not touch the profile, and PROTOCOL.md has it
            // measured: in onboard mode the device answers invalid argument,
            // and a rate set live is lost the moment it goes back. The old
            // wording here promised the opposite, which is the profile
            // editor's job, not this one's.
            theme::hint(
                ui,
                "Sets the rate live. The mouse only accepts that in host mode, and forgets \
                 it on the way back to onboard. To make a rate stick, set it on the \
                 Profiles tab: that writes byte 0 of the profile sector.",
            );
        });
    }

    fn draw_lighting(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_leds {
            theme::unsupported(ui, "RGB lighting");
            return;
        }
        let zones = self.snap.zones.clone();
        // A zone list that shrank (a different mouse, a replug) must not leave
        // the selection pointing past its end.
        if self.zone_sel >= zones.len() {
            self.zone_sel = 0;
        }
        // What the panel is aimed at decides what it offers: the modes every
        // selected zone has, and the ones every selected zone takes live. Off
        // is always live, as a fixed black.
        let aimed = selected_zones(&zones, self.all_zones, self.zone_sel);
        let live_modes = common_modes(&aimed, true);
        let live_ok = |m: u8| live_modes.contains(&m) || m == led::OFF;
        let offered = common_modes(&aimed, false);
        // A mode carried over from before the selection changed that the zones
        // do not have would otherwise stay armed behind a list that does not
        // show it, and Save would write it to flash.
        if !offered.is_empty() && !offered.contains(&self.mode) {
            self.mode = offered[0];
        }

        theme::section(ui, "Zone", |ui| {
            ui.horizontal_wrapped(|ui| {
                // "All", not "Both": the zone list is read off the device and
                // this app has no model whitelist, so the number of zones is
                // not something the label gets to claim.
                if ui.selectable_label(self.all_zones, "All").clicked() {
                    self.all_zones = true;
                }
                for (i, z) in zones.iter().enumerate() {
                    let on = !self.all_zones && self.zone_sel == i;
                    if ui.selectable_label(on, &z.location).clicked() {
                        self.all_zones = false;
                        self.zone_sel = i;
                    }
                }
            });
        });

        ui.add_space(14.0);
        theme::section(ui, "Mode", |ui| {
            let available: Vec<u8> = if zones.is_empty() {
                vec![led::OFF, led::FIXED, led::BREATHING, led::CYCLE]
            } else {
                offered.clone()
            };
            ui.horizontal_wrapped(|ui| {
                for m in &available {
                    let label = if live_ok(*m) {
                        led::mode_name(*m).to_string()
                    } else if storable_mode(*m) {
                        format!("{} (store only)", led::mode_name(*m))
                    } else {
                        // Neither path can carry it: the live setter refuses
                        // it and no profile record encodes it. Saying "store
                        // only" here would point at a button that is off.
                        format!("{} (not supported here)", led::mode_name(*m))
                    };
                    if ui.selectable_label(self.mode == *m, label).clicked() {
                        self.mode = *m;
                    }
                }
            });

            if self.mode == led::FIXED || self.mode == led::BREATHING {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    for c in PALETTE {
                        let sel = self.rgb() == *c;
                        if theme::swatch(ui, *c, sel).clicked() {
                            self.color = [
                                c[0] as f32 / 255.0,
                                c[1] as f32 / 255.0,
                                c[2] as f32 / 255.0,
                            ];
                        }
                    }
                    ui.add_space(6.0);
                    ui.color_edit_button_rgb(&mut self.color);
                });
            }
            if self.mode == led::BREATHING || self.mode == led::CYCLE {
                ui.add_space(10.0);
                ui.add(
                    egui::Slider::new(&mut self.period_ms, 500..=20000)
                        .suffix(" ms")
                        .text("period"),
                );
                ui.add(
                    egui::Slider::new(&mut self.intensity, 1..=100)
                        .suffix(" %")
                        .text("intensity"),
                );
            }
            if self.mode == led::BREATHING {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("waveform").color(theme::DIM).size(12.5));
                    egui::ComboBox::from_id_salt("wf")
                        .selected_text(led::WAVEFORMS[self.waveform].0)
                        .show_ui(ui, |ui| {
                            for (i, (n, _)) in led::WAVEFORMS.iter().enumerate() {
                                ui.selectable_value(&mut self.waveform, i, *n);
                            }
                        });
                });
            }

            // Storing goes through the format-2 profile codec and writes
            // flash, so three things have to hold before the button is live.
            //
            // The record has to be able to say what was picked. `Led` encodes
            // off, fixed, breathing and cycle; for a zone that reports wave
            // or starlight `lighting_record` has no answer, so nothing is
            // stored rather than a plain colour nobody chose.
            let rec = lighting_record(
                self.mode,
                self.rgb(),
                self.period_ms,
                led::WAVEFORMS[self.waveform].1,
                self.intensity,
            );
            // The other zone's record has to be known, because the command
            // carries both slots. Writing this record into both when the
            // mouse's own lighting has not been read is the silent loss of
            // stored lighting that the slot pairing below exists to prevent,
            // and it is reachable whenever the sectors are undecoded.
            let stored = self
                .snap
                .profiles
                .get(self.snap.active_profile as usize)
                .map(|p| p.leds);
            let slots = lighting_slots(self.all_zones, self.zone_sel, zones.len());
            let to_store = rec.and_then(|r| merged_leds(&slots, r, stored));
            // And the sector has to be one this app can decode at all: the
            // worker parses it with the format-2 codec before writing it back.
            let blocked = if slots.is_empty() {
                Some(
                    "The selected zone has no record in the profile, so there is nothing \
                     to write for it.",
                )
            } else {
                store_blocked_reason(
                    rec.is_some(),
                    self.snap.profile_format,
                    to_store.is_some(),
                )
            };
            // One flash write at a time: `stored` above is the snapshot, and
            // a write still in flight has not reached it yet.
            let writing = self.flash_inflight.is_some();
            let can_store = blocked.is_none() && !writing;

            ui.add_space(14.0);
            if !live_ok(self.mode) {
                theme::hint(
                    ui,
                    &format!(
                        "This firmware will not set {} live. Store it so the mouse runs \
                         it itself, or stream the rainbow effect below.",
                        led::mode_name(self.mode)
                    ),
                );
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        live_ok(self.mode) && storable_mode(self.mode),
                        egui::Button::new("Apply now"),
                    )
                    .clicked()
                {
                    let c = self.rgb();
                    let wf = led::WAVEFORMS[self.waveform].1;
                    for z in self.zones() {
                        self.dev.send(match self.mode {
                            led::OFF => Cmd::LedOff { zone: z },
                            led::FIXED => Cmd::LedFixed { zone: z, color: c },
                            led::BREATHING => Cmd::LedBreathing {
                                zone: z, color: c, period: self.period_ms,
                                waveform: wf, intensity: self.intensity,
                            },
                            led::CYCLE => Cmd::LedCycle {
                                zone: z, period: self.period_ms, intensity: self.intensity,
                            },
                            // Nothing else is sent: the button is only
                            // enabled for the four modes above.
                            _ => continue,
                        });
                    }
                }
                if ui
                    .add_enabled(can_store, egui::Button::new("Save to mouse"))
                    .clicked()
                {
                    // This writes flash. With one zone selected the other is
                    // kept exactly as the mouse already stores it (see
                    // `merged_leds`); `can_store` is only true when that
                    // answer exists.
                    if let Some(leds) = to_store {
                        self.send_tracked(Cmd::PersistLighting { leds });
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Apply now needs host mode and is lost on unplug. Save to mouse writes \
                 onboard flash, so it persists with nothing running, and into Windows.",
            );
            if let Some(why) = blocked {
                ui.add_space(6.0);
                theme::hint(ui, why);
            } else if writing {
                ui.add_space(6.0);
                theme::hint(ui, "Waiting for the mouse to confirm the last write.");
            }
        });

        ui.add_space(14.0);
        theme::section(ui, "Effects", |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("fx")
                    .selected_text(crate::effects::EFFECTS[self.effect].0)
                    .show_ui(ui, |ui| {
                        for (i, (n, _)) in crate::effects::EFFECTS.iter().enumerate() {
                            ui.selectable_value(&mut self.effect, i, *n);
                        }
                    });
                if ui.button("Start").clicked() {
                    self.dev.send(Cmd::StartEffect {
                        name: crate::effects::EFFECTS[self.effect].0.to_string(),
                        color: self.rgb(),
                        period_ms: self.effect_period_ms,
                    });
                }
                if ui
                    .add_enabled(
                        self.snap.effect_running.is_some(),
                        egui::Button::new("Stop"),
                    )
                    .clicked()
                {
                    self.dev.send(Cmd::StopEffect);
                }
            });
            ui.add_space(6.0);
            theme::hint(ui, crate::effects::EFFECTS[self.effect].1);
            ui.add_space(8.0);
            ui.add(
                egui::Slider::new(&mut self.effect_period_ms, 250..=20000)
                    .suffix(" ms")
                    .text("period"),
            );
            ui.add_space(6.0);
            theme::hint(
                ui,
                "The firmware has no frame mode, so animation is streamed from here and \
                 stops when it stops.",
            );
        });
    }

    fn draw_profiles(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_profiles {
            theme::unsupported(ui, "onboard profiles");
            return;
        }
        if self.snap.profile_format != 2 && self.snap.profiles.is_empty() {
            theme::section(ui, "Active profile", |ui| {
                theme::hint(ui, &format!(
                    "This device stores profiles in format {}, which this app cannot \
                     decode. Switching between them still works; their contents are \
                     not shown, and nothing is written.",
                    self.snap.profile_format
                ));
            });
        }
        theme::section(ui, "Active profile", |ui| {
            ui.horizontal_wrapped(|ui| {
                for i in 0..self.snap.profile_count {
                    let on = self.snap.active_profile == i;
                    if ui.selectable_label(on, format!("{i}")).clicked() {
                        self.send_tracked(Cmd::SetProfile(i));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                &format!(
                    "{} profiles live on the mouse. The active one applies with no \
                     software running.",
                    self.snap.profile_count
                ),
            );
        });

        if !self.snap.profiles.is_empty() {
            ui.add_space(14.0);
            self.profile_sel = self.profile_sel.min(self.snap.profiles.len() - 1);
            theme::section(ui, "Contents", |ui| {
                ui.horizontal_wrapped(|ui| {
                    for i in 0..self.snap.profiles.len() {
                        if ui
                            .selectable_label(self.profile_sel == i, format!("{i}"))
                            .clicked()
                        {
                            self.profile_sel = i;
                        }
                    }
                });
                ui.add_space(10.0);
                let p = &self.snap.profiles[self.profile_sel];
                theme::row(ui, "report rate", &format!("{} Hz", p.report_rate_hz()));
                let stages: Vec<String> =
                    p.dpi.iter().filter(|d| **d != 0).map(|d| d.to_string()).collect();
                theme::row(ui, "dpi stages", &stages.join(", "));
                theme::row(ui, "logo", &p.leds[0].describe());
                theme::row(ui, "dpi light", &p.leds[1].describe());
                theme::row(
                    ui,
                    "name",
                    if p.name.is_empty() { "(unset)" } else { &p.name },
                );
            });
        }

        if self.snap.has_profiles && self.snap.profile_format == 2 {
            ui.add_space(14.0);
            let snap = self.snap.clone();
            let sel = self.profile_sel as u8;
            // The editor builds what it writes from the snapshot, which does
            // not yet include a flash write sent from the Buttons or Lighting
            // tab and not answered. Saving then would write the old sector
            // back over that change. So it waits, as the button grid does.
            let writing = self.flash_inflight.is_some();
            let cmds = ui
                .add_enabled_ui(!writing, |ui| self.profile_editor.ui(ui, &snap, sel))
                .inner;
            if writing {
                ui.add_space(6.0);
                theme::hint(ui, "Waiting for the mouse to confirm the last write.");
            }
            for cmd in cmds {
                self.dispatch(cmd);
            }
        }

        ui.add_space(14.0);
        theme::section(ui, "Backup", |ui| {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 90.0).max(160.0);
                ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                if ui
                    .add_enabled(!self.backup_path.trim().is_empty(), egui::Button::new("Save"))
                    .clicked()
                {
                    // Never onto a file that is already there. The field shows
                    // where it actually went.
                    self.backup_path = self.send_backup(self.backup_path.clone());
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Dumps all 16 sectors including the profile directory. Take one before \
                 changing buttons or saving lighting. A file already at this path is \
                 never replaced: the new backup is written beside it with a number on \
                 the end.",
            );
        });
    }

    fn draw_buttons(&mut self, ui: &mut egui::Ui) {
        // Host bindings first: they need no flash write and are what most
        // people are looking for.
        let snap = self.snap.clone();
        for cmd in self.bindings.ui(ui, &snap) {
            self.dispatch(cmd);
        }
        // The editor also queues things Cmd could not express when it was
        // written. Draining this is what makes Save actually reach the device.
        for req in self.bindings.take_requests() {
            self.dev.send(match req {
                bindings_editor::Request::ApplyBindings { silence } => {
                    Cmd::ApplyBindings { silence }
                }
                bindings_editor::Request::RestoreIdentity => Cmd::RestoreIdentity,
            });
        }
        ui.add_space(18.0);
        // The grid below is the mouse's own table. With no mouse there is
        // nothing to read, and "does not report onboard profiles" would be a
        // claim about a device that is not there.
        if !self.snap.connected {
            theme::section(ui, "Buttons", |ui| {
                theme::hint(ui, "No mouse is connected, so its stored bindings cannot be shown.");
            });
            return;
        }
        if !self.snap.has_profiles {
            theme::unsupported(ui, "onboard profiles, so bindings cannot be read");
            return;
        }
        if self.snap.profiles.is_empty() {
            theme::section(ui, "Buttons", |ui| {
                theme::hint(ui, "No profile read yet. Press Refresh.");
            });
            return;
        }
        let idx = self.snap.active_profile;
        let Some(p) = self.snap.profiles.get(idx as usize).cloned() else {
            theme::section(ui, "Buttons", |ui| {
                theme::hint(ui, "Active profile not readable.");
            });
            return;
        };

        // A pending choice belongs to the profile it was made against. Left
        // over across a switch it shows up as a change to this profile that
        // the user never made, with Apply already armed — and Apply writes
        // flash.
        if self.btn_profile != Some(idx) {
            self.btn_action.clear();
            self.btn_profile = Some(idx);
        }

        let choices = button_choices();

        // The sector's table has 32 slots whatever a device claims, and
        // `set_button` refuses anything past them.
        let n_buttons = self.snap.button_count.min(crate::profile_blob::N_BUTTON_SLOTS as u8);
        let mut pending: Option<(u8, Button)> = None;
        let mut stranded = false;
        // The strand check below reads the snapshot's table, which does not
        // include a write that has been sent and not yet answered.
        let writing = self.flash_inflight.is_some();

        theme::section(ui, &format!("Buttons, profile {idx}"), |ui| {
            egui::Grid::new("btns")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 8.0))
                .show(ui, |ui| {
                    for i in 0..n_buttons {
                        let cur = p.buttons.get(i as usize).copied().unwrap_or(Button::disabled());
                        let cur_desc = cur.describe();
                        ui.label(egui::RichText::new(format!("{i}")).color(theme::DIM).size(12.5));
                        ui.label(egui::RichText::new(&cur_desc).size(13.0));

                        let sel = chosen_label(&mut self.btn_action, i, &cur_desc);
                        egui::ComboBox::from_id_salt(("btn", i))
                            .selected_text(&sel)
                            .show_ui(ui, |ui| {
                                for c in &choices {
                                    let mut s = sel.clone();
                                    if ui.selectable_value(&mut s, c.clone(), c).clicked() {
                                        self.btn_action.insert(i, c.clone());
                                    }
                                }
                            });

                        let changed = sel != cur_desc;
                        let chosen = if changed { action_to_button(&sel) } else { None };
                        let strands = chosen.is_some_and(|b| {
                            strands_left_click(&p.buttons, n_buttons as usize, i as usize, b)
                        });
                        stranded |= strands;
                        if ui
                            .add_enabled(
                                changed && !strands && !writing,
                                egui::Button::new("Apply"),
                            )
                            .clicked()
                        {
                            if let Some(b) = chosen {
                                pending = Some((i, b));
                            }
                        }
                        ui.end_row();
                    }
                });
            if stranded {
                ui.add_space(8.0);
                theme::hint(
                    ui,
                    "That change would leave this profile with no left click, and a mouse \
                     with none cannot click, even after unplugging. Bind left click to \
                     another button first.",
                );
            }
            if writing {
                ui.add_space(8.0);
                theme::hint(ui, "Waiting for the mouse to confirm the last write.");
            }
            ui.add_space(10.0);
            theme::hint(
                ui,
                "Changing a binding writes onboard flash, so it persists with nothing \
                 running. Take a backup from the Profiles tab first.",
            );
        });

        if let Some((button, binding)) = pending {
            // The choice stays: once the write reads back it matches the
            // device and `chosen_label` drops it, and if the write fails it
            // is still there to retry. Apply is held until then.
            self.send_tracked(Cmd::SetButton { index: idx, button, binding });
        }
    }

    fn draw_service(&mut self, ui: &mut egui::Ui) {
        use crate::service::{self as svc, Action, State};
        let st = self.service.state();
        // The first poll runs journalctl, which can outlast the first frames.
        // Until it lands the cached state is only a default, and printing it
        // tells the user the unit is installed and stopped without anything
        // having looked. Saying so is what `Watcher::known` is for.
        let known = self.service.known();
        let colour = match st {
            State::Active => theme::GOOD,
            State::Failed => theme::BAD,
            State::Inactive => theme::WARN,
            _ => theme::FAINT,
        };

        theme::section(ui, "Background service", |ui| {
            ui.horizontal(|ui| {
                if known {
                    theme::pill(ui, svc::label(st), colour);
                } else {
                    theme::pill(ui, "checking", theme::FAINT);
                }
                if known && self.service.is_enabled() {
                    theme::pill(ui, "starts at login", theme::DIM);
                }
                if let Some(a) = self.service.busy() {
                    theme::pill(ui, a.describe(), theme::ACCENT);
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                if known { svc::describe(st) } else { "Asking systemd what the unit is doing." },
            );
            ui.add_space(12.0);

            let can = known && svc::can_control(st) && self.service.busy().is_none();
            ui.horizontal(|ui| {
                if ui.add_enabled(can && st != State::Active, egui::Button::new("Start")).clicked() {
                    self.service.request(Action::Start);
                }
                if ui.add_enabled(can && st == State::Active, egui::Button::new("Stop")).clicked() {
                    self.service.request(Action::Stop);
                }
                if ui.add_enabled(can, egui::Button::new("Restart")).clicked() {
                    self.service.request(Action::Restart);
                }
                let enabled = self.service.is_enabled();
                let label = if enabled { "Don't start at login" } else { "Start at login" };
                if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                    self.service.request(if enabled { Action::Disable } else { Action::Enable });
                }
            });

            if st == State::NotInstalled {
                ui.add_space(10.0);
                theme::hint(
                    ui,
                    "The unit file is not installed. Run packaging/install.sh once; it \
                     needs sudo for the udev rules, which is why the app cannot do it.",
                );
            }
            if let Some(e) = self.service.last_error() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(e).color(theme::BAD).size(12.5));
            }
            if let Some(n) = self.service.last_notice() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(n).color(theme::GOOD).size(12.5));
            }
        });

        theme::section(ui, "Recent log", |ui| {
            let log = self.service.log();
            if log.is_empty() {
                theme::hint(ui, "Nothing logged yet.");
            } else {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for line in log {
                        ui.label(egui::RichText::new(line).monospace().size(11.5).color(theme::DIM));
                    }
                });
            }
            ui.add_space(8.0);
            if ui.button("Refresh").clicked() {
                self.service.refresh();
            }
        });
    }

    fn draw_about(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Device", |ui| {
            theme::row(ui, "model", &self.snap.model);
            theme::row(ui, "node", &self.snap.path);
            theme::row(ui, "HID++", &self.snap.protocol);
            theme::row(ui, "mode", crate::profiles::mode_name(self.snap.mode));
        });
        ui.add_space(14.0);
        theme::section(ui, "Following the mouse", |ui| {
            theme::row(ui, "announced", &self.snap.events_seen.to_string());
            if let Some(last) = &self.snap.last_event {
                theme::row(ui, "last", last);
            }
            ui.add_space(4.0);
            theme::hint(
                ui,
                "The window follows changes made on the mouse itself, such as the \
                 DPI and profile buttons. A mouse that announces them is picked up \
                 at once; either way the device is re-read about every second and a \
                 half. If the DPI buttons are pressed and this stays at zero, this \
                 mouse does not announce them and that re-read is doing the work.",
            );
        });
        ui.add_space(14.0);
        theme::section(ui, "What this hardware can and cannot do", |ui| {
            // Read off the attached device, not written in. These lines claim
            // facts about the mouse in front of the user, and this app has no
            // model whitelist, so a fixed list of numbers would be a lie on
            // anything but the one it was written for.
            let modes: Vec<&str> = self
                .snap
                .zones
                .first()
                .map(|z| z.modes.iter().map(|m| led::mode_name(*m)).collect())
                .unwrap_or_default();
            let lighting = if modes.is_empty() {
                "This device reports no lighting zones.".to_string()
            } else {
                format!("The firmware implements {} lighting modes: {}.",
                        modes.len(), modes.join(", "))
            };
            // Which of those the live setter takes is also the device's to say
            // (`live_modes`), so it is read the same way. A fixed sentence
            // naming fixed and breathing is true of one firmware.
            let live = match self.snap.zones.first() {
                Some(z) if !z.live_modes.is_empty() => format!(
                    "Set live: {}. Any other mode has to be stored or streamed.",
                    z.live_modes.iter().map(|m| led::mode_name(*m)).collect::<Vec<_>>().join(", ")
                ),
                Some(_) => "None of its lighting modes can be set live; they have to be \
                            stored or streamed."
                    .to_string(),
                None => String::new(),
            };
            let dpi = match &self.snap.dpi_cap {
                Some(Capability::Range { min, max, step }) =>
                    format!("DPI is continuous, {min}-{max} in steps of {step}."),
                Some(Capability::List(v)) if !v.is_empty() => format!(
                    "DPI comes in fixed stages: {}.",
                    v.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", ")
                ),
                _ => "This device did not report what DPI values it accepts.".to_string(),
            };
            for line in [
                lighting.as_str(),
                live.as_str(),
                "Wave, starlight, ripple and frame streaming are in HID++ but may be absent.",
                "Animated effects are streamed from this app and stop when it stops.",
                dpi.as_str(),
                "The live report rate is only accepted in host mode. To make one stick, \
                 set it on the Profiles tab, which writes it to the profile.",
                "Surface tuning and lift-off distance are out of scope: they sit behind \
                 engineering-flagged features that can brick the mouse.",
            ] {
                if !line.is_empty() {
                    theme::hint(ui, line);
                }
            }
            ui.add_space(8.0);
            theme::hint(ui, "Protocol detail is in PROTOCOL.md.");
        });
    }
}

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([940.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Logi"),
        ..Default::default()
    };
    eframe::run_native("logi", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Apply button in the Buttons grid is armed by comparing the chosen
    /// menu label with `Button::describe` of what the device holds. If a label
    /// did not come back out spelled the same way, Apply would stay armed
    /// after a successful write, and Apply writes flash.
    #[test]
    fn every_menu_label_round_trips_through_describe() {
        for label in button_choices() {
            let b = action_to_button(&label)
                .unwrap_or_else(|| panic!("{label} is offered but maps to no binding"));
            assert_eq!(b.describe(), label, "{label} does not come back as itself");
        }
    }

    #[test]
    fn a_label_from_nowhere_sends_nothing() {
        // No binding means `pending` stays None, so no command is queued.
        assert_eq!(action_to_button("nonsense"), None);
        assert_eq!(action_to_button("button"), None);
        assert_eq!(action_to_button("button x"), None);
        // Past the 16-bit mask, so `Button::mouse` would clamp and the write
        // would not be the button that was asked for.
        assert_eq!(action_to_button("button 900"), None);
    }

    #[test]
    fn describe_of_an_unmodelled_binding_is_not_offered_as_a_choice() {
        // A key binding, written by the CLI or the profile editor. The grid
        // shows it, but it must not be reachable from the menu, or choosing it
        // back would be a write that means something else.
        let key = Button::key(0x1e, 0x02);
        assert!(!button_choices().contains(&key.describe()));
        assert_eq!(action_to_button(&key.describe()), None);
    }

    #[test]
    fn storable_modes_are_exactly_the_ones_led_can_encode() {
        for m in [led::OFF, led::FIXED, led::BREATHING, led::CYCLE] {
            assert!(storable_mode(m), "{} has an Led constructor", led::mode_name(m));
        }
        for m in [led::WAVE, led::STARLIGHT, led::RIPPLE, led::CUSTOM] {
            assert!(!storable_mode(m), "{} has no Led constructor", led::mode_name(m));
        }
        // And what we do store for each really is that mode, so the record
        // cannot say something other than the panel.
        assert_eq!(Led::off().mode(), led::OFF);
        assert_eq!(Led::fixed([1, 2, 3]).mode(), led::FIXED);
        assert_eq!(Led::breathing([1, 2, 3], 4000, 0, 100).mode(), led::BREATHING);
        assert_eq!(Led::cycle(4000, 100).mode(), led::CYCLE);
    }

    #[test]
    fn storing_is_refused_until_the_write_can_mean_what_it_shows() {
        // A mode no record encodes.
        assert!(store_blocked_reason(false, 2, true).is_some());
        // A sector layout this app does not decode: the worker parses it with
        // the format-2 codec before writing it back.
        assert!(store_blocked_reason(true, 1, true).is_some());
        assert!(store_blocked_reason(true, 0, true).is_some());
        // One zone selected with the stored records unknown. The command
        // carries both slots, so writing would overwrite the other zone's
        // stored lighting with a copy of this one.
        assert!(store_blocked_reason(true, 2, false).is_some());
        // All three hold.
        assert_eq!(store_blocked_reason(true, 2, true), None);
    }

    #[test]
    fn a_record_exists_only_for_the_modes_a_profile_can_hold() {
        for m in [led::OFF, led::FIXED, led::BREATHING, led::CYCLE] {
            let r = lighting_record(m, [1, 2, 3], 4000, 0, 100).expect("storable");
            assert_eq!(r.mode(), m, "the record says what the panel shows");
        }
        for m in [led::WAVE, led::STARLIGHT, led::RIPPLE, led::CUSTOM] {
            assert_eq!(lighting_record(m, [1, 2, 3], 4000, 0, 100), None);
        }
    }

    #[test]
    fn saving_one_zone_never_copies_it_over_the_other() {
        let a = Led::fixed([1, 1, 1]);
        let b = Led::fixed([2, 2, 2]);
        let new = Led::fixed([9, 9, 9]);
        // Selecting the second zone keeps the first as stored.
        let slots = lighting_slots(false, 1, 2);
        assert_eq!(merged_leds(&slots, new, Some([a, b])), Some([a, new]));
        let slots = lighting_slots(false, 0, 2);
        assert_eq!(merged_leds(&slots, new, Some([a, b])), Some([new, b]));
        // The stored records are unknown: no write, not a copy into both.
        assert_eq!(merged_leds(&slots, new, None), None);
        // Both zones selected replaces both, and needs nothing stored.
        let slots = lighting_slots(true, 0, 2);
        assert_eq!(merged_leds(&slots, new, None), Some([new, new]));
    }

    #[test]
    fn the_mode_list_is_what_every_selected_zone_has() {
        let zone = |index: u8, modes: &[u8], live: &[u8]| device::ZoneInfo {
            index,
            location: String::new(),
            modes: modes.to_vec(),
            live_modes: live.to_vec(),
        };
        let a = zone(0, &[led::OFF, led::FIXED, led::BREATHING, led::CYCLE], &[led::FIXED, led::BREATHING]);
        let b = zone(1, &[led::OFF, led::FIXED], &[led::FIXED]);
        let zones = [a, b];
        // One zone: its own list.
        let one = selected_zones(&zones, false, 0);
        assert_eq!(common_modes(&one, false), vec![led::OFF, led::FIXED, led::BREATHING, led::CYCLE]);
        // All: only what both have, so "All" cannot send a mode the second
        // zone has no effect for.
        let all = selected_zones(&zones, true, 0);
        assert_eq!(common_modes(&all, false), vec![led::OFF, led::FIXED]);
        assert_eq!(common_modes(&all, true), vec![led::FIXED]);
        // Nothing selected offers nothing.
        assert!(selected_zones(&zones, false, 5).is_empty());
        assert!(common_modes(&selected_zones(&zones, false, 5), false).is_empty());
    }

    #[test]
    fn a_zone_with_no_record_slot_writes_nothing() {
        // A third zone has no record behind it, and neither does a stale
        // selection on a device that reports none.
        assert!(lighting_slots(false, 2, 3).is_empty());
        assert!(lighting_slots(false, 0, 0).is_empty());
        assert!(lighting_slots(true, 0, 0).is_empty());
        let new = Led::off();
        assert_eq!(merged_leds(&[], new, Some([new, new])), None);
    }

    #[test]
    fn all_zones_on_a_one_zone_device_leaves_the_other_record_alone() {
        let a = Led::fixed([1, 1, 1]);
        let b = Led::fixed([2, 2, 2]);
        let new = Led::fixed([9, 9, 9]);
        let slots = lighting_slots(true, 0, 1);
        assert_eq!(slots, vec![0]);
        assert_eq!(merged_leds(&slots, new, Some([a, b])), Some([new, b]));
        assert_eq!(merged_leds(&slots, new, None), None);
    }

    #[test]
    fn the_service_and_the_rules_do_not_wait_for_a_mouse() {
        assert!(!tab_needs_device(Tab::Service));
        assert!(!tab_needs_device(Tab::Apps));
        // The host bindings are a file, and their editor is built to be used
        // with no mouse.
        assert!(!tab_needs_device(Tab::Buttons));
        for t in [Tab::Sensor, Tab::Lighting, Tab::Profiles, Tab::About] {
            assert!(tab_needs_device(t));
        }
    }

    #[test]
    fn a_tray_service_click_acts_only_when_the_panel_would_let_it() {
        use crate::service::{Action, State};
        // Nothing has looked at systemd yet: the state is a default.
        assert_eq!(tray_service_action(true, false, State::Inactive, false), None);
        // Something is already in flight.
        assert_eq!(tray_service_action(true, true, State::Active, true), None);
        // Nothing to control.
        assert_eq!(tray_service_action(true, true, State::NotInstalled, false), None);
        assert_eq!(tray_service_action(false, true, State::NoSystemd, false), None);
        // And when it can: toggle flips, restart restarts.
        assert_eq!(tray_service_action(true, true, State::Active, false), Some(Action::Stop));
        assert_eq!(tray_service_action(true, true, State::Failed, false), Some(Action::Start));
        assert_eq!(tray_service_action(false, true, State::Active, false), Some(Action::Restart));
    }

    #[test]
    fn the_grid_will_not_remove_the_only_left_click() {
        let left = Button::mouse(1);
        let right = Button::mouse(2);
        let table = [left, right, Button::disabled()];
        // Disabling or rebinding the only left click strands the mouse.
        assert!(strands_left_click(&table, 3, 0, Button::disabled()));
        assert!(strands_left_click(&table, 3, 0, right));
        // Anything else is fine, including keeping left click where it is.
        assert!(!strands_left_click(&table, 3, 1, Button::disabled()));
        assert!(!strands_left_click(&table, 3, 0, Button::mouse(1)));
        // Moving it to another button first is the way to change the original.
        let moved = [left, left, Button::disabled()];
        assert!(!strands_left_click(&moved, 3, 0, Button::disabled()));
        // A profile already without one can always be edited back.
        let broken = [right, Button::disabled()];
        assert!(!strands_left_click(&broken, 2, 0, Button::disabled()));
        assert!(!strands_left_click(&broken, 2, 1, left));
        // A left click beyond the buttons this device has does not count.
        let hidden = [right, right, left];
        assert!(!strands_left_click(&hidden, 2, 0, Button::disabled()));
    }

    #[test]
    fn a_flash_write_holds_the_next_until_it_is_answered() {
        use std::time::Duration;
        let soon = Duration::from_secs(1);
        // Sent as 7, nothing newer recorded: still in flight.
        assert!(still_waiting(7, 0, soon));
        assert!(still_waiting(7, 6, soon));
        // Its own outcome, or a later one if the ring moved on.
        assert!(!still_waiting(7, 7, soon));
        assert!(!still_waiting(7, 12, soon));
        // A worker that never answers must not grey the buttons for good.
        assert!(!still_waiting(7, 0, FLASH_WAIT));
    }

    #[test]
    fn a_profile_switch_holds_the_panels_too() {
        assert!(holds_the_panels(&Cmd::SetProfile(1)));
        assert!(holds_the_panels(&Cmd::SetButton { index: 0, button: 1, binding: Button::disabled() }));
        assert!(!holds_the_panels(&Cmd::SetDpi(800)));
        assert!(!holds_the_panels(&Cmd::Refresh));
    }

    #[test]
    fn only_a_choice_the_user_made_is_remembered() {
        let mut m = std::collections::HashMap::new();
        // Nothing chosen: the device's own label, and nothing stored.
        assert_eq!(chosen_label(&mut m, 3, "button 1"), "button 1");
        assert!(m.is_empty());
        // The table changed underneath: no stale label to read as a change.
        assert_eq!(chosen_label(&mut m, 3, "button 2"), "button 2");
        // A chosen label is kept while the device differs...
        m.insert(3, "disabled".to_string());
        assert_eq!(chosen_label(&mut m, 3, "button 2"), "disabled");
        // ...and dropped once the device holds it, so it cannot come back as
        // a pending change after something else rewrites that button.
        assert_eq!(chosen_label(&mut m, 3, "disabled"), "disabled");
        assert!(m.is_empty());
        assert_eq!(chosen_label(&mut m, 3, "button 2"), "button 2");
    }

    #[test]
    fn only_sector_writes_are_held_for() {
        assert!(writes_flash(&Cmd::SetButton { index: 0, button: 1, binding: Button::disabled() }));
        assert!(writes_flash(&Cmd::PersistLighting { leds: [Led::off(), Led::off()] }));
        assert!(!writes_flash(&Cmd::Refresh));
        assert!(!writes_flash(&Cmd::SetDpi(800)));
        assert!(!writes_flash(&Cmd::Backup(String::new())));
    }

    #[test]
    fn a_backup_never_lands_on_one_already_there() {
        let taken = |p: &str| p == "/h/logi-backup.json" || p == "/h/logi-backup-2.json";
        assert_eq!(free_backup_path("/h/other.json", taken), "/h/other.json");
        assert_eq!(free_backup_path("/h/logi-backup.json", taken), "/h/logi-backup-3.json");
        // No extension, and a dot in a directory name is not one.
        assert_eq!(numbered_path("/h.d/backup", 2), "/h.d/backup-2");
        assert_eq!(numbered_path("/h/.hidden", 2), "/h/.hidden-2");
        // Bounded.
        assert_eq!(free_backup_path("/h/a.json", |_| true), "/h/a-1000.json");
    }

    #[test]
    fn a_backup_path_is_absolute_even_with_no_home() {
        assert_eq!(
            backup_path_in(Some("/home/u".into()), Some("/tmp".into())),
            "/home/u/logi-backup.json"
        );
        // `HOME` unset, empty or relative is not worth a panic, and a bare
        // filename would write into whatever directory the app was started
        // from.
        for home in [None, Some(String::new()), Some("relative".to_string())] {
            assert_eq!(
                backup_path_in(home, Some("/work".into())),
                "/work/logi-backup.json"
            );
        }
        assert_eq!(backup_path_in(None, None), "/logi-backup.json");
        assert!(default_backup_path().starts_with('/'));
    }
}
