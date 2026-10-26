//! Panel icon, over the StatusNotifierItem spec.
//!
//! The point of this module is that the service can be seen and steered
//! without opening the window and without a terminal: the icon itself says
//! whether a mouse is attached and whether the background service is running,
//! and the context menu switches profile and DPI.
//!
//! A tray is optional, not assumed. StatusNotifierItem needs a host on the
//! session bus (`org.kde.StatusNotifierWatcher`); plain GNOME has none until
//! the AppIndicator extension is installed. So [`Tray::spawn`] returns an
//! error rather than panicking, the caller is expected to carry on without
//! one, and the error text names the extension because that is the fix.
//!
//! Threading: ksni runs its own event loop on its own thread, and
//! `ksni::blocking::Handle::update` waits for that loop to answer. Waiting is
//! fine, but not on the frame loop, so [`Tray::update`] hands the status to a
//! small thread of ours and returns immediately. Menu clicks come back the
//! other way, over a channel that [`Tray::poll`] drains.

use crate::gui::theme;
use crate::service::State;

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use ksni::blocking::TrayMethods;
use ksni::menu::{Disposition, MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Category, Icon, ToolTip};

/// What the user chose in the menu. The GUI turns these into device commands
/// and service calls; nothing here acts on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Show the main window.
    Open,
    /// Start the service if it is not running, stop it if it is.
    ToggleService,
    RestartService,
    SetProfile(u8),
    SetDpi(u16),
    Quit,
}

/// Everything the icon and its menu render. The GUI builds one of these from
/// its snapshot and the service state, and hands it to [`Tray::update`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// Model name, `None` when no device is attached.
    pub device: Option<String>,
    pub service: State,
    pub active_profile: u8,
    pub profile_count: u8,
    pub dpi: u16,
    pub dpi_presets: Vec<u16>,
}

/// Which of the three looks the icon wears. Named, because "the icon encodes
/// state" is only true if the states have names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Device attached, service running: a solid mouse.
    Live,
    /// Device attached, service not running: the same mouse as an outline.
    Idle,
    /// No device: the outline with a slash through it.
    Absent,
}

const NO_HOST: &str = "This desktop has no panel icon area: nothing provides \
    org.kde.StatusNotifierWatcher on the session bus. On GNOME, install the \
    \"AppIndicator and KStatusNotifierItem Support\" extension (package \
    gnome-shell-extension-appindicator), enable it, then log out and back in. \
    Every feature of this app works without it.";

impl Status {
    fn present(&self) -> bool {
        self.device.as_deref().is_some_and(|m| !m.is_empty())
    }

    /// The header line: the device, or that there is not one.
    pub fn device_label(&self) -> String {
        match self.device.as_deref() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => "No device".to_string(),
        }
    }

    /// The service state in words.
    pub fn service_label(&self) -> String {
        format!("Service: {}", service_words(&self.service))
    }

    /// Label for the single start/stop item, which flips rather than showing
    /// two items where one of them never applies.
    pub fn toggle_label(&self) -> &'static str {
        if self.service == State::Active {
            "Stop service"
        } else {
            "Start service"
        }
    }

    /// False where starting or stopping could not work, so those items are
    /// shown greyed instead of erroring when clicked.
    pub fn service_controls_enabled(&self) -> bool {
        matches!(self.service, State::Active | State::Inactive | State::Failed)
    }

    /// What to do about an unhappy service, said in the menu so the user is
    /// never sent to a terminal to find out.
    pub fn service_note(&self) -> Option<&'static str> {
        match self.service {
            State::NotInstalled => Some("Install it from the Service panel in the app."),
            State::Failed => Some("Read the log in the Service panel in the app."),
            State::NoSystemd => Some("systemd did not answer. Open the Service panel."),
            State::Active | State::Inactive => None,
        }
    }

    /// Profile menu labels, one per onboard profile. Empty when the device has
    /// no profiles, in which case the submenu is left out entirely.
    pub fn profile_labels(&self) -> Vec<String> {
        // Shown 1-based: "Profile 0" reads as a bug to anyone who has used
        // Logitech's own software. The wire index stays 0-based.
        (0..self.profile_count).map(|i| format!("Profile {}", i + 1)).collect()
    }

    /// Index of the checked profile, or an out-of-range value when the active
    /// profile is not one of them (which ksni renders as nothing checked).
    pub fn profile_selected(&self) -> usize {
        if self.active_profile < self.profile_count {
            self.active_profile as usize
        } else {
            usize::MAX
        }
    }

    pub fn dpi_labels(&self) -> Vec<String> {
        self.dpi_presets.iter().map(|d| format!("{d} DPI")).collect()
    }

    /// The DPI is continuous on this hardware, so it is often between presets.
    /// Then nothing is checked, which is the truth.
    pub fn dpi_selected(&self) -> usize {
        self.dpi_presets.iter().position(|d| *d == self.dpi).unwrap_or(usize::MAX)
    }

    /// Which look the icon wears.
    pub fn shape(&self) -> Shape {
        if !self.present() {
            Shape::Absent
        } else if self.service == State::Active {
            Shape::Live
        } else {
            Shape::Idle
        }
    }

    /// One line of context for the pointer hover.
    pub fn tool_tip_text(&self) -> String {
        let mut s = self.service_label();
        if self.present() {
            if self.profile_count > 0 {
                s.push_str(&format!(
                    " \u{b7} profile {} of {}",
                    self.active_profile as u16 + 1,
                    self.profile_count
                ));
            }
            if self.dpi > 0 {
                s.push_str(&format!(" \u{b7} {} DPI", self.dpi));
            }
        }
        s
    }

    /// True when `other` would change anything the tray shows.
    ///
    /// [`Tray::update`] is called every frame; without this each one would be
    /// a D-Bus round trip and a redraw of the panel for no reason.
    pub fn differs_from(&self, other: &Status) -> bool {
        self != other
    }

    /// The whole menu as plain text, in order, with `[*]` on checked items and
    /// `(disabled)` on dead ones.
    ///
    /// Exists so the menu model can be tested with no session bus. Built from
    /// the same label helpers `menu()` uses; keep the order in step with it.
    pub fn menu_outline(&self) -> Vec<String> {
        let dis = |on: bool| if on { "" } else { " (disabled)" };
        let mut out = vec![self.device_label(), "--".into(), self.service_label()];
        let can = self.service_controls_enabled();
        out.push(format!("{}{}", self.toggle_label(), dis(can)));
        out.push(format!("Restart service{}", dis(can)));
        if let Some(n) = self.service_note() {
            out.push(n.to_string());
        }
        out.push("--".into());
        let profiles = self.profile_labels();
        if !profiles.is_empty() {
            out.push("Profile >".into());
            let sel = self.profile_selected();
            for (i, l) in profiles.iter().enumerate() {
                out.push(format!("  {l}{}", if i == sel { " [*]" } else { "" }));
            }
        }
        let dpis = self.dpi_labels();
        if !dpis.is_empty() {
            out.push("DPI >".into());
            let sel = self.dpi_selected();
            for (i, l) in dpis.iter().enumerate() {
                out.push(format!("  {l}{}", if i == sel { " [*]" } else { "" }));
            }
        }
        out.push("--".into());
        out.push("Open Logi".into());
        out.push("Quit".into());
        out
    }
}

/// The service state in words. The one place this module looks inside
/// `service::State`.
fn service_words(s: &State) -> &'static str {
    match s {
        State::Active => "running",
        State::Inactive => "stopped",
        State::Failed => "failed",
        State::NotInstalled => "not installed",
        State::NoSystemd => "unknown",
    }
}

/// The ksni side: holds the status it renders and a way back to the GUI.
struct Model {
    status: Status,
    events: Sender<Event>,
}

impl Model {
    /// Menu callbacks run on the tray's event loop, which must not block, so
    /// a click only posts an event. The GUI does the work.
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// A line of text, not a control.
    fn note(label: String) -> MenuItem<Self> {
        StandardItem {
            label,
            enabled: false,
            disposition: Disposition::Informative,
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Model {
    fn id(&self) -> String {
        "logi".into()
    }

    fn title(&self) -> String {
        "Logi".into()
    }

    /// This item is about a piece of hardware, which is what Hardware means.
    fn category(&self) -> Category {
        Category::Hardware
    }

    fn status(&self) -> ksni::Status {
        // A failed service is the one case worth the host emphasising.
        if self.status.service == State::Failed {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let shape = self.status.shape();
        icons(shape, tint(shape))
    }

    /// Worn only in the NeedsAttention state above.
    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        let [r, g, b, _] = theme::BAD.to_array();
        icons(Shape::Idle, [r, g, b])
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.status.device_label(),
            description: self.status.tool_tip_text(),
            ..Default::default()
        }
    }

    /// Left click opens the window, which is what a panel icon is expected to
    /// do. The menu is on right click.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.emit(Event::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.status;
        let mut items = vec![Model::note(s.device_label()), MenuItem::Separator];

        items.push(Model::note(s.service_label()));
        let can = s.service_controls_enabled();
        items.push(
            StandardItem {
                label: s.toggle_label().into(),
                enabled: can,
                activate: Box::new(|m: &mut Model| m.emit(Event::ToggleService)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Restart service".into(),
                enabled: can,
                activate: Box::new(|m: &mut Model| m.emit(Event::RestartService)),
                ..Default::default()
            }
            .into(),
        );
        if let Some(n) = s.service_note() {
            items.push(Model::note(n.to_string()));
        }
        items.push(MenuItem::Separator);

        // Left out rather than shown dead: a device with no onboard profiles
        // has nothing for this submenu to say.
        let profiles = s.profile_labels();
        if !profiles.is_empty() {
            items.push(
                SubMenu {
                    label: "Profile".into(),
                    submenu: vec![RadioGroup {
                        selected: s.profile_selected(),
                        // Option index is the profile index, 0-based on the wire.
                        select: Box::new(|m: &mut Model, i: usize| {
                            m.emit(Event::SetProfile(i as u8))
                        }),
                        options: profiles.into_iter().map(radio).collect(),
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
            );
        }

        let dpis = s.dpi_labels();
        if !dpis.is_empty() {
            let presets = s.dpi_presets.clone();
            items.push(
                SubMenu {
                    label: "DPI".into(),
                    submenu: vec![RadioGroup {
                        selected: s.dpi_selected(),
                        select: Box::new(move |m: &mut Model, i: usize| {
                            if let Some(v) = presets.get(i) {
                                m.emit(Event::SetDpi(*v));
                            }
                        }),
                        options: dpis.into_iter().map(radio).collect(),
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open Logi".into(),
                activate: Box::new(|m: &mut Model| m.emit(Event::Open)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|m: &mut Model| m.emit(Event::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

fn radio(label: String) -> RadioItem {
    RadioItem { label, ..Default::default() }
}

/// A registered panel icon.
pub struct Tray {
    /// Statuses out to our own thread, which owns the ksni handle.
    tx: Sender<Status>,
    /// Clicks back in. `poll` takes `&self`, hence the mutex.
    events: Mutex<Receiver<Event>>,
    /// Last status actually pushed, for change detection.
    last: Mutex<Status>,
}

impl Tray {
    /// Register with the StatusNotifierWatcher.
    ///
    /// `Err` when there is no tray host, which is the normal case on a plain
    /// GNOME install. The caller must stay usable without one.
    pub fn spawn(initial: Status) -> Result<Self, String> {
        let (ev_tx, ev_rx) = mpsc::channel();
        let model = Model { status: initial.clone(), events: ev_tx };
        let handle = model.spawn().map_err(describe)?;

        let (tx, rx) = mpsc::channel::<Status>();
        std::thread::Builder::new()
            .name("logi-tray".into())
            .spawn(move || {
                while let Ok(mut s) = rx.recv() {
                    // Only the newest status is worth a round trip; anything
                    // queued behind it is already out of date.
                    while let Ok(newer) = rx.try_recv() {
                        s = newer;
                    }
                    if handle.update(|m: &mut Model| m.status = s).is_none() {
                        break; // the tray service is gone
                    }
                }
                handle.shutdown();
            })
            .map_err(|e| format!("could not start the tray thread: {e}"))?;

        Ok(Tray { tx, events: Mutex::new(ev_rx), last: Mutex::new(initial) })
    }

    /// Push new state to the icon. Cheap and non-blocking; an unchanged status
    /// is dropped here rather than crossing D-Bus.
    pub fn update(&self, status: Status) {
        let Ok(mut last) = self.last.lock() else { return };
        if !last.differs_from(&status) {
            return;
        }
        *last = status.clone();
        let _ = self.tx.send(status);
    }

    /// Non-blocking; returns what the user chose since the last call.
    pub fn poll(&self) -> Vec<Event> {
        match self.events.lock() {
            Ok(rx) => rx.try_iter().collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Turn a ksni failure into something a user can act on.
fn describe(e: ksni::Error) -> String {
    match e {
        // No host, or a host that will not show us: same fix either way.
        ksni::Error::Watcher(_) | ksni::Error::WontShow => NO_HOST.to_string(),
        ksni::Error::Dbus(e) => format!(
            "No panel icon: the session D-Bus could not be reached ({e}). \
             This usually means the app is running outside a desktop session."
        ),
        // ksni::Error is #[non_exhaustive].
        other => format!("No panel icon: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Icon
//
// Drawn in code rather than shipped as files: three states times two sizes is
// six images to install and keep in step with the palette, for a glyph that is
// a rounded rectangle and a slot.

/// The icon is designed on a 22-unit grid, which is the panel size GNOME asks
/// for, and scaled from there.
const GRID: f32 = 22.0;

/// Mouse body: centre, half-width, half-height, corner radius.
const BODY: (f32, f32, f32, f32, f32) = (11.0, 11.5, 6.0, 8.5, 5.5);
/// Scroll wheel slot. Small, but it is what makes the silhouette read as a
/// mouse rather than a pill.
const WHEEL: (f32, f32, f32, f32, f32) = (11.0, 7.0, 0.85, 2.6, 0.85);
/// Outline weight, in grid units.
const STROKE: f32 = 1.1;

/// The palette is the app's own, so the panel and the window agree. GOOD and
/// WARN are the service's state; an absent device is not a fault, so it is
/// neutral rather than red.
fn tint(shape: Shape) -> [u8; 3] {
    let c = match shape {
        Shape::Live => theme::GOOD,
        Shape::Idle => theme::WARN,
        Shape::Absent => theme::DIM,
    };
    let [r, g, b, _] = c.to_array();
    [r, g, b]
}

/// Signed distance to a rounded rectangle; negative inside.
fn rrect(px: f32, py: f32, r: (f32, f32, f32, f32, f32)) -> f32 {
    let (cx, cy, hw, hh, rad) = r;
    let qx = (px - cx).abs() - (hw - rad);
    let qy = (py - cy).abs() - (hh - rad);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad
}

/// Signed distance to a thick line segment; negative inside.
fn bar(px: f32, py: f32, a: (f32, f32), b: (f32, f32), half: f32) -> f32 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let (wx, wy) = (px - a.0, py - a.1);
    let t = ((wx * vx + wy * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (dx, dy) = (wx - vx * t, wy - vy * t);
    (dx * dx + dy * dy).sqrt() - half
}

/// Is this point, in grid units, inked?
fn ink(shape: Shape, x: f32, y: f32) -> bool {
    let body = rrect(x, y, BODY);
    let wheel = rrect(x, y, WHEEL);
    match shape {
        // Solid, with the wheel slot cut out of it.
        Shape::Live => body < 0.0 && wheel > 0.0,
        // Hollow, with the wheel as a mark inside.
        Shape::Idle => body.abs() < STROKE || wheel < 0.0,
        Shape::Absent => {
            let slash = bar(x, y, (4.8, 4.8), (17.2, 17.2), 1.25);
            if slash < 0.0 {
                return true;
            }
            // A clear gap each side of the slash, or it merges with the
            // outline it crosses and both stop reading.
            if slash < STROKE {
                return false;
            }
            body.abs() < STROKE || wheel < 0.0
        }
    }
}

/// One ARGB32 pixmap.
pub fn render(shape: Shape, rgb: [u8; 3], size: i32) -> Icon {
    let scale = size as f32 / GRID;
    let mut data = Vec::with_capacity((size * size * 4).max(0) as usize);
    for y in 0..size {
        for x in 0..size {
            // 3x3 supersampling. At 22px the corner radius is most of the
            // glyph, and hard sampling makes it look chewed.
            let mut hits = 0u32;
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 3.0) / scale;
                    let py = (y as f32 + (sy as f32 + 0.5) / 3.0) / scale;
                    if ink(shape, px, py) {
                        hits += 1;
                    }
                }
            }
            // ARGB32, network byte order: alpha first, not premultiplied.
            let a = (hits * 255 / 9) as u8;
            data.extend_from_slice(&[a, rgb[0], rgb[1], rgb[2]]);
        }
    }
    Icon { width: size, height: size, data }
}

/// The panel size and a 2x variant, so a HiDPI panel has something to pick.
fn icons(shape: Shape, rgb: [u8; 3]) -> Vec<Icon> {
    [22, 44].iter().map(|&n| render(shape, rgb, n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g502() -> Status {
        Status {
            device: Some("G502 HERO".into()),
            service: State::Active,
            active_profile: 1,
            profile_count: 5,
            dpi: 1600,
            dpi_presets: vec![800, 1600, 3200],
        }
    }

    #[test]
    fn identical_statuses_are_not_worth_a_redraw() {
        let a = g502();
        assert!(!a.differs_from(&a.clone()));
    }

    #[test]
    fn every_rendered_field_counts_as_a_change() {
        let base = g502();
        let mut s = base.clone();
        s.dpi = 1601;
        assert!(base.differs_from(&s));
        let mut s = base.clone();
        s.active_profile = 2;
        assert!(base.differs_from(&s));
        let mut s = base.clone();
        s.service = State::Inactive;
        assert!(base.differs_from(&s));
        let mut s = base.clone();
        s.device = None;
        assert!(base.differs_from(&s));
        let mut s = base.clone();
        s.dpi_presets.push(6400);
        assert!(base.differs_from(&s));
    }

    #[test]
    fn menu_names_the_device_and_the_live_values() {
        let out = g502().menu_outline();
        assert_eq!(out[0], "G502 HERO");
        assert!(out.contains(&"Service: running".to_string()));
        assert!(out.contains(&"Stop service".to_string()));
        assert!(out.contains(&"  Profile 2 [*]".to_string()));
        assert!(out.contains(&"  1600 DPI [*]".to_string()));
        assert!(out.contains(&"Quit".to_string()));
        // Exactly one profile and one DPI checked.
        assert_eq!(out.iter().filter(|l| l.ends_with("[*]")).count(), 2);
    }

    #[test]
    fn no_device_still_offers_the_service_and_the_window() {
        let s = Status { service: State::Inactive, ..Default::default() };
        let out = s.menu_outline();
        assert_eq!(out[0], "No device");
        assert!(out.contains(&"Start service".to_string()));
        assert!(out.contains(&"Open Logi".to_string()));
        // Nothing to choose from, so neither submenu appears.
        assert!(!out.iter().any(|l| l.starts_with("Profile >")));
        assert!(!out.iter().any(|l| l.starts_with("DPI >")));
        assert_eq!(s.shape(), Shape::Absent);
    }

    #[test]
    fn a_failed_service_says_so_and_says_where_to_look() {
        let s = Status { service: State::Failed, ..g502() };
        let out = s.menu_outline();
        assert!(out.contains(&"Service: failed".to_string()));
        assert!(out.contains(&"Start service".to_string()));
        assert!(out.contains(&"Read the log in the Service panel in the app.".to_string()));
        // Failed is still a device present but not running: the idle look.
        assert_eq!(s.shape(), Shape::Idle);
    }

    #[test]
    fn an_uninstalled_service_greys_its_controls() {
        let s = Status { service: State::NotInstalled, ..g502() };
        let out = s.menu_outline();
        assert!(out.contains(&"Start service (disabled)".to_string()));
        assert!(out.contains(&"Restart service (disabled)".to_string()));
        assert!(out.contains(&"Install it from the Service panel in the app.".to_string()));
    }

    #[test]
    fn several_profiles_are_listed_one_based_and_switch_zero_based() {
        let s = Status { profile_count: 3, active_profile: 2, ..g502() };
        assert_eq!(s.profile_labels(), ["Profile 1", "Profile 2", "Profile 3"]);
        assert_eq!(s.profile_selected(), 2);
    }

    #[test]
    fn an_active_profile_past_the_count_checks_nothing() {
        let s = Status { profile_count: 2, active_profile: 7, ..g502() };
        assert_eq!(s.profile_selected(), usize::MAX);
        assert!(!s.menu_outline().iter().any(|l| l.starts_with("  Profile") && l.ends_with("[*]")));
    }

    #[test]
    fn a_dpi_between_presets_checks_nothing() {
        let s = Status { dpi: 1450, ..g502() };
        assert_eq!(s.dpi_selected(), usize::MAX);
    }

    #[test]
    fn running_with_a_device_is_the_live_look() {
        assert_eq!(g502().shape(), Shape::Live);
    }

    #[test]
    fn icons_are_square_argb_at_the_requested_size() {
        for size in [16, 22, 44] {
            let i = render(Shape::Live, [1, 2, 3], size);
            assert_eq!((i.width, i.height), (size, size));
            assert_eq!(i.data.len(), (size * size * 4) as usize);
            // The colour is in the R/G/B bytes, alpha first.
            assert!(i.data.chunks_exact(4).all(|p| p[1..] == [1, 2, 3]));
        }
    }

    #[test]
    fn the_three_states_look_different() {
        let alpha = |s| {
            render(s, [255, 255, 255], 22)
                .data
                .chunks_exact(4)
                .map(|p| p[0])
                .collect::<Vec<u8>>()
        };
        let live = alpha(Shape::Live);
        let idle = alpha(Shape::Idle);
        let absent = alpha(Shape::Absent);
        assert_ne!(live, idle);
        assert_ne!(idle, absent);
        assert_ne!(live, absent);
        // Solid covers more than an outline, which covers more than nothing.
        let ink: fn(&Vec<u8>) -> usize = |v| v.iter().filter(|a| **a > 128).count();
        assert!(ink(&live) > ink(&idle));
        assert!(ink(&idle) > 20);
        // Each state tints differently, or the colour would carry no meaning.
        assert_ne!(tint(Shape::Live), tint(Shape::Idle));
        assert_ne!(tint(Shape::Idle), tint(Shape::Absent));
    }

    #[test]
    fn the_glyph_stays_inside_its_box() {
        let i = render(Shape::Live, [255, 255, 255], 22);
        let at = |x: usize, y: usize| i.data[(y * 22 + x) * 4];
        for n in 0..22 {
            assert_eq!(at(n, 0), 0, "top edge at {n}");
            assert_eq!(at(n, 21), 0, "bottom edge at {n}");
            assert_eq!(at(0, n), 0, "left edge at {n}");
            assert_eq!(at(21, n), 0, "right edge at {n}");
        }
    }

    #[test]
    fn the_no_host_message_names_the_fix() {
        assert!(NO_HOST.contains("appindicator"));
        assert!(NO_HOST.contains("log out"));
    }

    #[test]
    #[ignore = "needs a live session bus with a StatusNotifierWatcher"]
    fn registers_and_reports_clicks() {
        let tray = Tray::spawn(g502()).expect("a tray host");
        tray.update(Status { dpi: 3200, ..g502() });
        assert!(tray.poll().is_empty());
    }
}
