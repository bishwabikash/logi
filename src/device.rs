//! Worker thread that owns the device.
//!
//! The GUI must never touch HID++ directly: a call takes milliseconds and can
//! time out for the better part of a second, which would stall the frame loop
//! and freeze the window on unplug. So the device lives on its own thread and
//! the UI talks to it over channels.

use crate::dpi::{Capability, Dpi};
use crate::hidpp::Device;
use crate::led::Leds;
use crate::profile_blob::Profile;
use crate::profiles::{OnboardProfiles, MODE_ONBOARD};
use crate::rate::ReportRate;
use crate::{discover, effects, profile_io};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Cmd {
    Refresh,
    SetDpi(u16),
    SetRate(u16),
    SetProfile(u8),
    SetMode(u8),
    LedFixed { zone: u8, color: [u8; 3] },
    LedBreathing { zone: u8, color: [u8; 3], period: u16, waveform: u8, intensity: u8 },
    LedCycle { zone: u8, period: u16, intensity: u8 },
    LedOff { zone: u8 },
    /// Store the current lighting into the active onboard profile so it
    /// survives unplugging.
    PersistLighting { leds: [crate::profile_blob::Led; 2] },
    StartEffect { name: String, color: [u8; 3], period_ms: u32 },
    StopEffect,
    Backup(String),
    WriteProfile { index: u8, profile: Box<Profile> },
    SetButton { index: u8, button: u8, binding: crate::profile_blob::Button },
    /// Silence exactly these physical buttons and un-silence every other one.
    /// Empty means "silence nothing", which is how Replace is undone.
    ApplyBindings { silence: Vec<u8> },
    /// Write the factory identity remapping table. The recovery path for a
    /// mouse whose buttons were silenced with nothing listening.
    RestoreIdentity,
}

/// Identifies one command for whoever sent it.
///
/// Assigned by `Handle::send`, which hands it back, and carried through to the
/// `Outcome` recorded in the snapshot. Monotonic for the life of the handle.
/// Zero is never issued, so it reads as "nothing recorded yet".
pub type CmdId = u64;

/// Which command an `Outcome` belongs to.
///
/// The id alone would do for a caller that keeps what `send` returned, but the
/// GUI funnels several panels' commands through one dispatch loop that drops
/// it. So the kind is what lets a panel recognise its own answer, and the
/// per-profile commands carry the index they touch for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdKind {
    Refresh,
    SetDpi,
    SetRate,
    SetProfile,
    SetMode,
    Led,
    PersistLighting,
    StartEffect,
    StopEffect,
    Backup,
    WriteProfile(u8),
    SetButton(u8),
    ApplyBindings,
    RestoreIdentity,
}

impl Cmd {
    pub fn kind(&self) -> CmdKind {
        match self {
            Cmd::Refresh => CmdKind::Refresh,
            Cmd::SetDpi(_) => CmdKind::SetDpi,
            Cmd::SetRate(_) => CmdKind::SetRate,
            Cmd::SetProfile(_) => CmdKind::SetProfile,
            Cmd::SetMode(_) => CmdKind::SetMode,
            Cmd::LedFixed { .. }
            | Cmd::LedBreathing { .. }
            | Cmd::LedCycle { .. }
            | Cmd::LedOff { .. } => CmdKind::Led,
            Cmd::PersistLighting { .. } => CmdKind::PersistLighting,
            Cmd::StartEffect { .. } => CmdKind::StartEffect,
            Cmd::StopEffect => CmdKind::StopEffect,
            Cmd::Backup(_) => CmdKind::Backup,
            Cmd::WriteProfile { index, .. } => CmdKind::WriteProfile(*index),
            Cmd::SetButton { index, .. } => CmdKind::SetButton(*index),
            Cmd::ApplyBindings { .. } => CmdKind::ApplyBindings,
            Cmd::RestoreIdentity => CmdKind::RestoreIdentity,
        }
    }

    /// True when running this leaves the decoded profiles in the snapshot
    /// stale, so they have to be read again. `Refresh` is in here because
    /// that re-read is its whole job.
    fn rereads_profiles(&self) -> bool {
        matches!(
            self,
            Cmd::PersistLighting { .. }
                | Cmd::WriteProfile { .. }
                | Cmd::SetButton { .. }
                | Cmd::Refresh
        )
    }
}

/// What happened to one command the worker executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: CmdId,
    pub kind: CmdKind,
    pub ok: bool,
    /// The notice on success, the error on failure. `None` only for a command
    /// that succeeded with nothing to say.
    pub message: Option<String>,
}

impl Outcome {
    pub fn ok(id: CmdId, kind: CmdKind, message: Option<String>) -> Self {
        Outcome { id, kind, ok: true, message }
    }

    pub fn failed(id: CmdId, kind: CmdKind, message: impl Into<String>) -> Self {
        Outcome { id, kind, ok: false, message: Some(message.into()) }
    }
}

/// How many outcomes the snapshot keeps.
///
/// A panel polls at frame rate, so one would nearly always do. The ring is
/// here so a burst of commands from other panels cannot push an answer out
/// before the panel waiting on it has had a frame to look.
const OUTCOME_HISTORY: usize = 16;

/// A snapshot of device state for the UI to render. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub model: String,
    pub kind: String,
    pub has_dpi: bool,
    pub has_rate: bool,
    pub has_leds: bool,
    pub has_profiles: bool,
    pub has_button_spy: bool,
    pub profile_format: u8,
    pub path: String,
    pub firmware: String,
    pub protocol: String,
    pub dpi: u16,
    pub dpi_cap: Option<Capability>,
    pub rate: u16,
    pub rates: Vec<u16>,
    pub profile_count: u8,
    pub active_profile: u8,
    pub button_count: u8,
    pub zones: Vec<ZoneInfo>,
    pub profiles: Vec<Profile>,
    pub mode: u8,
    pub effect_running: Option<String>,
    /// Most recent error, for the status bar.
    pub error: Option<String>,
    pub notice: Option<String>,
    /// The last few executed commands and how each went, oldest first.
    ///
    /// `error` and `notice` are the newest of these flattened for the status
    /// bar, which is all most panels need. This is what a panel reads when it
    /// has to know whether its *own* command landed: an error in the status
    /// bar may belong to anyone.
    pub outcomes: Vec<Outcome>,
}

impl Snapshot {
    /// Record one executed command, newest last.
    ///
    /// Also updates `error` and `notice`, so every panel that only reads
    /// those keeps working unchanged.
    pub fn record(&mut self, o: Outcome) {
        if o.ok {
            self.notice = o.message.clone();
            self.error = None;
        } else {
            self.error = o.message.clone();
            self.notice = None;
        }
        if self.outcomes.len() >= OUTCOME_HISTORY {
            let over = self.outcomes.len() + 1 - OUTCOME_HISTORY;
            self.outcomes.drain(..over);
        }
        self.outcomes.push(o);
    }

    /// Highest outcome id recorded, or 0 if none.
    ///
    /// A panel reads this *before* it sends, so it can tell its own answer
    /// from one that was already on the books.
    pub fn newest_outcome_id(&self) -> CmdId {
        self.outcomes.iter().map(|o| o.id).max().unwrap_or(0)
    }

    /// The outcome for `id`, if it is still in the ring.
    pub fn outcome(&self, id: CmdId) -> Option<&Outcome> {
        self.outcomes.iter().find(|o| o.id == id)
    }

    /// The oldest outcome newer than `after` for `kind`.
    ///
    /// Ids are handed out in send order and the worker executes in that same
    /// order, so "newer than the id I saw before I sent" plus "the kind I
    /// sent" identifies a panel's own command without the panel having to
    /// keep the id `send` returned — which the GUI's dispatch loop discards.
    pub fn outcome_after(&self, after: CmdId, kind: CmdKind) -> Option<&Outcome> {
        self.outcomes.iter().find(|o| o.id > after && o.kind == kind)
    }
}

#[derive(Debug, Clone)]
pub struct ZoneInfo {
    pub index: u8,
    pub location: String,
    /// Every mode the zone has an effect record for.
    pub modes: Vec<u8>,
    /// The subset this firmware will actually set over the live path.
    pub live_modes: Vec<u8>,
}

pub struct Handle {
    tx: Sender<(CmdId, Cmd)>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Next command id. Shared so a clone of the handle could not hand out
    /// the same id twice.
    next_id: Arc<AtomicU64>,
    /// Set while the worker holds the device in host mode, so shutdown knows
    /// whether it has to hand the mouse back.
    host_held: Arc<std::sync::atomic::AtomicBool>,
}

impl Handle {
    /// Queue a command and return its id, which shows up in
    /// `Snapshot::outcomes` once the worker has run it.
    ///
    /// Deliberately not `#[must_use]`: nearly every caller fires and forgets,
    /// and only one that waits for the result needs the id.
    pub fn send(&self, c: Cmd) -> CmdId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send((id, c));
        id
    }

    /// The id the next `send` will hand out.
    ///
    /// For a caller that cannot keep what `send` returned — the GUI dispatches
    /// several panels' commands through one loop — this is the other way to
    /// bound the search of `Snapshot::outcomes` from below.
    pub fn next_id(&self) -> CmdId {
        self.next_id.load(Ordering::Relaxed)
    }

    pub fn host_held(&self) -> bool {
        self.host_held.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Put the mouse back into onboard mode, synchronously.
    ///
    /// Host mode leaves the stored profile unapplied, so the mouse would keep
    /// whatever lighting this app last pushed after it exits. Done here rather
    /// than as a command the worker might never reach.
    pub fn restore_onboard_blocking(&self) {
        if !self.host_held() {
            return;
        }
        if let Ok(f) = crate::discover::open_device() {
            let mut dev = f.device;
            if let Ok(p) = crate::profiles::OnboardProfiles::new(&mut dev) {
                let _ = p.set_mode(&mut dev, crate::profiles::MODE_ONBOARD);
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Spawn the device worker. Returns immediately; the first snapshot arrives
/// once the worker has probed the device.
pub fn spawn() -> Handle {
    let (tx, rx) = mpsc::channel::<(CmdId, Cmd)>();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let shared = Arc::clone(&snapshot);
    let host_held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = Arc::clone(&host_held);
    std::thread::Builder::new()
        .name("logi-device".into())
        .spawn(move || worker(rx, shared, held))
        .expect("spawn device thread");
    // Ids start at 1 so that 0 can mean "no outcome recorded yet".
    Handle { tx, snapshot, next_id: Arc::new(AtomicU64::new(1)), host_held }
}

/// Take host mode only if we are not already in it. Each switch costs a
/// 150ms settle, which is very noticeable when it happens on every click.
fn ensure_host(l: &mut Live) -> crate::hidpp::Result<()> {
    let p = l.profiles.take().ok_or_else(|| missing("onboard profiles"))?;
    let r = if p.mode(&mut l.dev).unwrap_or(0) == crate::profiles::MODE_HOST {
        Ok(())
    } else {
        p.set_mode(&mut l.dev, crate::profiles::MODE_HOST)
    };
    l.profiles = Some(p);
    r
}

struct Live {
    dev: Device,
    name: String,
    kind: String,
    caps: crate::capabilities::Capabilities,
    // Each is present only if the device's own feature table reports it, so an
    // arbitrary Logitech mouse gets exactly the controls it supports.
    leds: Option<Leds>,
    dpi: Option<Dpi>,
    rate: Option<ReportRate>,
    profiles: Option<OnboardProfiles>,
}

/// Fail a command cleanly when the device lacks the feature it needs.
fn missing(what: &str) -> crate::hidpp::Error {
    crate::hidpp::Error::Protocol(format!("this device has no {what}"))
}

/// Borrow one optional feature and the device at the same time.
///
/// `l.dpi()?.set(&mut l.dev, ..)` cannot work: the accessor borrows all of
/// `l`. Destructuring borrows the fields disjointly, which is what this does.
macro_rules! feat {
    ($l:expr, $field:ident, $what:expr) => {{
        let Live { dev, $field, .. } = $l;
        match $field.as_ref() {
            Some(f) => Ok((f, dev)),
            None => Err(missing($what)),
        }
    }};
}

impl Live {
    fn leds(&self) -> crate::hidpp::Result<&Leds> {
        self.leds.as_ref().ok_or_else(|| missing("RGB lighting"))
    }
    fn dpi(&self) -> crate::hidpp::Result<&Dpi> {
        self.dpi.as_ref().ok_or_else(|| missing("adjustable DPI"))
    }
    fn rate(&self) -> crate::hidpp::Result<&ReportRate> {
        self.rate.as_ref().ok_or_else(|| missing("adjustable report rate"))
    }
    fn profiles(&self) -> crate::hidpp::Result<&OnboardProfiles> {
        self.profiles.as_ref().ok_or_else(|| missing("onboard profiles"))
    }
}

fn connect() -> Result<Live, String> {
    let found = discover::open_device().map_err(|e| e.to_string())?;
    let mut dev = found.device;
    let caps = found.caps;
    // Build only what the device advertises. A failure to construct a feature
    // it claims is not fatal either; the control just stays unavailable.
    let leds = caps.color_leds.then(|| Leds::new(&mut dev).ok()).flatten();
    let dpi = caps.dpi.then(|| Dpi::new(&mut dev).ok()).flatten();
    let rate = caps.report_rate.then(|| ReportRate::new(&mut dev).ok()).flatten();
    let profiles = caps
        .onboard_profiles
        .then(|| OnboardProfiles::new(&mut dev).ok())
        .flatten();
    Ok(Live { dev, name: found.name, kind: found.kind, caps, leds, dpi, rate, profiles })
}

/// Cheap state: about seven HID++ round trips.
///
/// Kept separate from the profile sectors deliberately. Reading all five
/// profiles is 80 round trips, which is over a second of latency; doing that
/// after every click made the UI feel hung.
fn read_state(l: &mut Live, into: &mut Snapshot) {
    into.connected = true;
    into.model = l.name.clone();
    into.kind = l.kind.clone();
    into.path = l.dev.path.clone();
    into.has_dpi = l.dpi.is_some();
    into.has_rate = l.rate.is_some();
    into.has_leds = l.leds.is_some();
    into.has_profiles = l.profiles.is_some();
    into.has_button_spy = l.caps.button_spy;
    if let Ok((a, b)) = l.dev.protocol_version() {
        into.protocol = format!("{a}.{b}");
    }
    if let Some(d) = l.dpi.take() {
        into.dpi = d.get(&mut l.dev, 0).unwrap_or(into.dpi);
        if into.dpi_cap.is_none() {
            into.dpi_cap = d.capability(&mut l.dev, 0).ok();
        }
        l.dpi = Some(d);
    }
    if let Some(r) = l.rate.take() {
        into.rate = r.get(&mut l.dev).unwrap_or(into.rate);
        if into.rates.is_empty() {
            into.rates = r.supported(&mut l.dev).unwrap_or_default();
        }
        l.rate = Some(r);
    }
    if let Some(p) = l.profiles.take() {
        into.profile_count = p.info.profile_count;
        into.button_count = p.info.button_count;
        into.profile_format = p.info.profile_format_id;
        into.active_profile = p.current(&mut l.dev).unwrap_or(into.active_profile);
        into.mode = p.mode(&mut l.dev).unwrap_or(into.mode);
        l.profiles = Some(p);
    }
    if into.zones.is_empty() {
        if let Some(leds) = l.leds.take() {
            into.zones = leds
                .zones
                .iter()
                .map(|z| ZoneInfo {
                    index: z.index,
                    location: z.location_name().to_string(),
                    modes: z.modes.clone(),
                    live_modes: leds.live_modes(z.index),
                })
                .collect();
            l.leds = Some(leds);
        }
    }
}

/// The expensive part: every profile sector, 80 round trips. Only worth doing
/// on an explicit refresh or after something wrote flash.
fn read_profiles(l: &mut Live, into: &mut Snapshot) {
    let Some(p) = l.profiles.take() else { return };
    // Only the 256-byte format-2 layout is decodable; another device may use a
    // different one, and guessing at it would print nonsense.
    if p.info.profile_format_id == 2 && p.info.sector_size as usize == crate::profile_blob::SECTOR_SIZE
    {
        let mut out = Vec::new();
        for i in 0..into.profile_count {
            if let Ok(blob) = p.read_sector(&mut l.dev, i as u16 + 1, None) {
                if let Ok(parsed) = Profile::parse(&blob) {
                    out.push(parsed);
                }
            }
        }
        if !out.is_empty() {
            into.profiles = out;
        }
    }
    l.profiles = Some(p);
}

fn worker(
    rx: Receiver<(CmdId, Cmd)>,
    shared: Arc<Mutex<Snapshot>>,
    host_held: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut live: Option<Live> = None;
    let mut effect: Option<effects::Running> = None;
    let mut pending: std::collections::VecDeque<(CmdId, Cmd)> =
        std::collections::VecDeque::new();

    loop {
        // Reconnect lazily so unplug/replug is handled without crashing.
        if live.is_none() && effect.is_none() {
            match connect() {
                Ok(mut l) => {
                    if let Ok(mut g) = shared.lock() {
                        *g = Snapshot::default();
                        read_state(&mut l, &mut g);
                        read_profiles(&mut l, &mut g);
                    }
                    live = Some(l);
                }
                Err(e) => {
                    if let Ok(mut g) = shared.lock() {
                        g.connected = false;
                        g.error = Some(e);
                    }
                    // Back off; a missing device is the normal idle state.
                    std::thread::sleep(Duration::from_millis(1500));
                }
            }
        }

        // Anything queued by the previous iteration runs before we block.
        let (id, cmd) = match pending.pop_front() {
            Some(c) => c,
            None => match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let kind = cmd.kind();

        // Any command other than StopEffect ends a running effect first: the
        // effect holds the device, and two owners would interleave writes.
        if !matches!(cmd, Cmd::StopEffect) {
            if let Some(r) = effect.take() {
                r.stop();
            }
        }

        let Some(l) = live.as_mut() else {
            // Still an outcome: a panel waiting on this id must be told it
            // failed, or it waits for an answer that is never coming.
            if let Ok(mut g) = shared.lock() {
                let msg = "device not connected".to_string();
                g.error = Some(msg.clone());
                g.record(Outcome::failed(id, kind, msg));
            }
            continue;
        };

        let name_for_backup = l.name.clone();
        let wrote_flash = cmd.rereads_profiles();
        let result: crate::hidpp::Result<Option<String>> = match cmd {
            Cmd::Refresh => Ok(None),
            Cmd::SetDpi(v) => feat!(l, dpi, "adjustable DPI").and_then(|(d, dev)| {
                d.set(dev, v, 0).map(|got| {
                    Some(if got == v {
                        format!("DPI {got}")
                    } else {
                        format!("DPI snapped to {got}")
                    })
                })
            }),
            Cmd::SetRate(hz) => feat!(l, rate, "adjustable report rate")
                .and_then(|(r, dev)| r.set(dev, hz).map(|_| Some(format!("{hz} Hz")))),
            Cmd::SetMode(m) => feat!(l, profiles, "onboard profiles").and_then(|(p, dev)| {
                p.set_mode(dev, m)
                    .map(|_| Some(format!("mode: {}", crate::profiles::mode_name(m))))
            }),
            Cmd::SetProfile(i) => feat!(l, profiles, "onboard profiles")
                .and_then(|(p, dev)| p.set_current(dev, i).map(|_| Some(format!("profile {i}")))),
            Cmd::LedFixed { zone, color } => ensure_host(l)
                .and_then(|_| feat!(l, leds, "RGB lighting"))
                .and_then(|(leds, dev)| leds.fixed(dev, zone, color))
                .map(|_| Some("lighting applied live".into())),
            Cmd::LedBreathing { zone, color, period, waveform, intensity } => ensure_host(l)
                .and_then(|_| feat!(l, leds, "RGB lighting"))
                .and_then(|(leds, dev)| {
                    leds.breathing(dev, zone, color, period, waveform, intensity)
                })
                .map(|_| Some("breathing applied live".into())),
            Cmd::LedCycle { zone, period, intensity } => ensure_host(l)
                .and_then(|_| feat!(l, leds, "RGB lighting"))
                .and_then(|(leds, dev)| leds.cycle(dev, zone, period, intensity))
                .map(|_| Some("cycle applied live".into())),
            Cmd::LedOff { zone } => ensure_host(l)
                .and_then(|_| feat!(l, leds, "RGB lighting"))
                .and_then(|(leds, dev)| leds.off(dev, zone))
                .map(|_| Some("lighting off".into())),
            Cmd::PersistLighting { leds: recs } => {
                feat!(l, profiles, "onboard profiles").and_then(|(p, dev)| {
                    let idx = p.current(dev).unwrap_or(0);
                    let blob = p.read_sector(dev, idx as u16 + 1, None)?;
                    let mut prof =
                        Profile::parse(&blob).map_err(crate::hidpp::Error::Protocol)?;
                    prof.leds = recs;
                    prof.alt_leds = recs;
                    profile_io::write_profile(dev, p, idx, &prof)?;
                    p.set_mode(dev, MODE_ONBOARD)?;
                    Ok(Some("stored to onboard profile (survives unplug)".into()))
                })
            }
            Cmd::SetButton { index, button, binding } => {
                feat!(l, profiles, "onboard profiles").and_then(|(p, dev)| {
                    profile_io::set_button(dev, p, index, button, binding).map(|_| {
                        Some(format!(
                            "button {button} -> {} (profile {index}, flash)",
                            binding.describe()
                        ))
                    })
                })
            }
            Cmd::ApplyBindings { silence } => {
                // Only the table is written here. Acting on the presses is the
                // background service's job, so say so rather than implying the
                // bindings are live.
                match crate::buttonspy::ButtonSpy::new(&mut l.dev) {
                    Err(e) => Err(e),
                    Ok(spy) => spy.silence(&mut l.dev, &silence).map(|_| {
                        Some(if silence.is_empty() {
                            "all physical buttons restored".to_string()
                        } else {
                            format!(
                                "{} button(s) silenced; the service acts on them",
                                silence.len()
                            )
                        })
                    }),
                }
            }
            Cmd::RestoreIdentity => match crate::buttonspy::ButtonSpy::new(&mut l.dev) {
                Err(e) => Err(e),
                Ok(spy) => spy
                    .restore_identity(&mut l.dev)
                    .map(|_| Some("factory button mapping restored".into())),
            },
            Cmd::WriteProfile { index, profile } => {
                feat!(l, profiles, "onboard profiles").and_then(|(p, dev)| {
                    profile_io::write_profile(dev, p, index, &profile)
                        .map(|_| Some(format!("profile {index} written to flash")))
                })
            }
            Cmd::Backup(path) => {
                feat!(l, profiles, "onboard profiles").and_then(|(p, dev)| {
                    let dump = profile_io::dump_all(dev, p);
                    let json = profile_io::dump_to_json(&name_for_backup, &dump);
                    std::fs::write(&path, json)
                        .map(|_| Some(format!("backup written to {path}")))
                        .map_err(crate::hidpp::Error::Io)
                })
            }
            Cmd::StartEffect { name, color, period_ms } => {
                match effects::start(name.clone(), color, period_ms) {
                    Ok(r) => {
                        effect = Some(r);
                        // The effect thread now owns the device.
                        live = None;
                        if let Ok(mut g) = shared.lock() {
                            g.effect_running = Some(name);
                            g.notice = Some("effect running; it stops when you stop it".into());
                            g.error = None;
                        }
                        continue;
                    }
                    Err(e) => Err(crate::hidpp::Error::Protocol(e)),
                }
            }
            Cmd::StopEffect => {
                if let Some(r) = effect.take() {
                    r.stop();
                }
                if let Ok(mut g) = shared.lock() {
                    g.effect_running = None;
                }
                Ok(Some("effect stopped".into()))
            }
        };

        match result {
            Ok(notice) => {
                if let Ok(mut g) = shared.lock() {
                    read_state(l, &mut g);
                    host_held.store(
                        g.mode == crate::profiles::MODE_HOST,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    if wrote_flash || g.profiles.is_empty() {
                        read_profiles(l, &mut g);
                    }
                    g.notice = notice.clone();
                    g.error = None;
                    g.record(Outcome::ok(id, kind, notice));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                // A vanished device must drop the handle so the loop reconnects.
                let gone = matches!(e, crate::hidpp::Error::Io(_))
                    || matches!(e, crate::hidpp::Error::NotFound(_));
                if let Ok(mut g) = shared.lock() {
                    g.error = Some(msg.clone());
                    g.notice = None;
                    // Recorded before the handle is dropped, so a panel waiting
                    // on this id learns the write failed rather than timing out.
                    g.record(Outcome::failed(id, kind, msg));
                }
                if gone {
                    live = None;
                }
            }
        }

        // Collapse a run of queued Refreshes, but never drop a real command:
        // the previous version matched Ok(_) and broke, which silently threw
        // away whatever it had just received. Two quick clicks lost the second.
        while let Ok(next) = rx.try_recv() {
            if matches!(next.1, Cmd::Refresh) {
                continue;
            }
            pending.push_back(next);
            break;
        }
    }
}
