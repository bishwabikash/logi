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

    /// True when running this can leave the device in host mode.
    ///
    /// The flag shutdown reads is set from this *before* the command runs,
    /// not from the mode read back after it, because a `setOnboardMode` whose
    /// reply was lost may still have landed. Handing the mouse back when it
    /// did not need it costs one HID++ call at exit; not handing it back
    /// leaves a mouse that ignores its own stored profile until something
    /// else sets the mode, which is not something the user can click.
    fn takes_host_mode(&self) -> bool {
        match self {
            Cmd::SetMode(m) => *m == crate::profiles::MODE_HOST,
            // Live lighting only takes effect in host mode, so each of these
            // switches into it first; see `ensure_host`.
            Cmd::LedFixed { .. }
            | Cmd::LedBreathing { .. }
            | Cmd::LedCycle { .. }
            | Cmd::LedOff { .. }
            | Cmd::StartEffect { .. } => true,
            _ => false,
        }
    }

    /// True when running this can change the onboard mode without taking host
    /// mode: the ones that switch back to onboard. After a failure of one of
    /// these the mode has to be read again, because the switch may have landed
    /// without its reply.
    fn touches_mode(&self) -> bool {
        matches!(self, Cmd::SetMode(_) | Cmd::PersistLighting { .. })
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
    /// Which DPI stage is active, as an index into the profile's stages.
    pub dpi_index: Option<u8>,
    pub dpi_cap: Option<Capability>,
    pub rate: u16,
    pub rates: Vec<u16>,
    pub profile_count: u8,
    pub active_profile: u8,
    pub button_count: u8,
    pub zones: Vec<ZoneInfo>,
    pub profiles: Vec<Profile>,
    pub mode: u8,
    /// How many times the mouse has announced a change on its own since this
    /// connection. Zero for a long time while the buttons are being pressed
    /// means this mouse does not announce them, and the slow re-read is what
    /// keeps the app current.
    pub events_seen: u64,
    /// What the last announcement was, for the diagnostics line.
    pub last_event: Option<String>,
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
        self.record_silent(o);
    }

    /// Record an outcome in the ring only, leaving the status bar's `error`
    /// and `notice` as they are.
    ///
    /// For a command that was answered without being run, such as a Refresh
    /// folded into the one just executed: it needs an answer so a panel waiting
    /// on it is not left waiting, but it has nothing to say, and `record` would
    /// wipe the notice of the command that did the work.
    pub fn record_silent(&mut self, o: Outcome) {
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
    /// The enabled DPI stages of the active profile, in slot order, as the
    /// mouse stores them. These are what its DPI button cycles through.
    ///
    /// Read off the sector, never assumed: a fixed list of "common" values
    /// has no 2400 in it, and a mouse whose fourth stage is 2400 then lit
    /// nothing at that level.
    pub fn dpi_stages(&self) -> Vec<u16> {
        self.profiles
            .get(self.active_profile as usize)
            .map(|p| p.dpi.iter().copied().filter(|d| *d != 0).collect())
            .unwrap_or_default()
    }

    /// The active DPI stage as (position, total) among the enabled stages,
    /// counting from one: "stage 2 of 4".
    ///
    /// None when the index is unknown, names a disabled slot or one past the
    /// end, or when the DPI is not that stage's value. The last matters
    /// because the stage index and the sensor DPI are two registers that do
    /// not always change together: a read taken between the two updates, or a
    /// DPI set from the host that is not any stage, would otherwise be shown
    /// as a stage it is not.
    pub fn dpi_stage(&self) -> Option<(usize, usize)> {
        let i = self.dpi_index? as usize;
        let stages = &self.profiles.get(self.active_profile as usize)?.dpi;
        let here = stages.get(i).copied().unwrap_or(0);
        if here == 0 || here != self.dpi {
            return None;
        }
        let pos = stages[..=i].iter().filter(|d| **d != 0).count();
        let total = stages.iter().filter(|d| **d != 0).count();
        Some((pos, total))
    }

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

/// How the exit-time restore gets the device to itself.
///
/// The restore opens its own handle on the node, and so does everything the
/// worker does. Both speak with the same software id, so two handles talking
/// at once can each take the other's reply for their own, and a worker still
/// working through a queue would put the mouse back in host mode (a live
/// lighting command switches into it first) right after the restore took it
/// out, with the process ending before anything noticed.
///
/// So the restore closes the gate and waits for the worker to be between
/// pieces of work. The worker claims the gate before it touches the device and
/// is refused once it is closed. Both sides use `SeqCst`, which is what makes
/// "I set `busy`, then saw `closing` clear" and "I set `closing`, then saw
/// `busy` clear" impossible together.
struct Gate {
    closing: std::sync::atomic::AtomicBool,
    busy: std::sync::atomic::AtomicBool,
}

/// Held by the worker for as long as it may be touching the device.
struct Busy<'a>(&'a Gate);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

impl Gate {
    fn new() -> Self {
        Gate {
            closing: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Claim the device for one piece of work. `None` once the gate is closed,
    /// in which case the caller must leave the device alone.
    fn enter(&self) -> Option<Busy<'_>> {
        self.busy.store(true, Ordering::SeqCst);
        if self.closing.load(Ordering::SeqCst) {
            self.busy.store(false, Ordering::SeqCst);
            return None;
        }
        Some(Busy(self))
    }

    /// Refuse all further work, then wait up to `limit` for the piece in
    /// progress to finish. True if the worker is idle, false if it was still
    /// busy when the limit ran out.
    fn close_and_wait(&self, limit: Duration) -> bool {
        self.closing.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + limit;
        while self.busy.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
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
    gate: Arc<Gate>,
}

/// Put the mouse back in onboard mode and make sure it took.
///
/// A setter whose reply was lost may still have landed, so after a failure
/// the mode is read before another try. Stops at once on an Io error: the
/// endpoint is gone and another try is against nothing.
fn back_to_onboard(p: &OnboardProfiles, dev: &mut Device) -> crate::hidpp::Result<()> {
    const ATTEMPTS: u32 = 3;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match p.set_mode(dev, MODE_ONBOARD) {
            Ok(()) => return Ok(()),
            Err(e @ crate::hidpp::Error::Io(_)) => return Err(e),
            Err(e) => {
                if matches!(p.mode(dev), Ok(m) if m == MODE_ONBOARD) {
                    return Ok(());
                }
                last = Some(e);
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    match last {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Whether the mouse should be handed back at exit, given what is known.
///
/// `mode` is what was just read off the device, or `None` if the read failed;
/// a failed read must leave the earlier answer standing. It used to read as
/// "onboard" by way of a stale snapshot field, which cleared the flag exactly
/// when the device was misbehaving and a lost mode switch was most likely.
fn host_held_after(prev: bool, mode: Option<u8>, takes_host: bool) -> bool {
    match mode {
        Some(m) => m == crate::profiles::MODE_HOST || takes_host,
        None => prev || takes_host,
    }
}

impl Handle {
    /// Queue a command and return its id, which shows up in
    /// `Snapshot::outcomes` once the worker has run it.
    ///
    /// Deliberately not `#[must_use]`: nearly every caller fires and forgets,
    /// and only one that waits for the result needs the id.
    pub fn send(&self, c: Cmd) -> CmdId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kind = c.kind();
        if self.tx.send((id, c)).is_err() {
            // The worker thread is gone, so nothing will ever record an
            // outcome for this id and a panel waiting on its own answer would
            // wait for one that cannot arrive. Writing the snapshot from here
            // is safe in exactly the case it matters: the only other writer is
            // the worker, and it has stopped publishing.
            if let Ok(mut g) = self.snapshot.lock() {
                g.record(Outcome::failed(id, kind, "the device thread has stopped"));
            }
        }
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

    /// Put the mouse back into onboard mode, synchronously. For exit.
    ///
    /// Host mode leaves the stored profile unapplied, so the mouse would keep
    /// whatever lighting this app last pushed after it exits. Done here rather
    /// than as a command the worker might never reach.
    ///
    /// The worker is shut out first and given a moment to finish what it is
    /// doing; see `Gate`. That also makes `host_held` final by the time it is
    /// read. After this the worker refuses every command.
    pub fn restore_onboard_blocking(&self) {
        self.gate.close_and_wait(Duration::from_secs(5));
        if !self.host_held() {
            return;
        }
        // One lost reply must not be the end of it: the mouse would keep
        // ignoring its stored profile after the app is gone, and nothing is
        // left running to switch it back. So the whole sequence, opening the
        // device included, is tried again, not only the setter. This mirrors
        // the effect session's release, which hands the mouse back the same
        // way on its own exit.
        const ATTEMPTS: u32 = 3;
        for attempt in 0..ATTEMPTS {
            if hand_back_once() {
                return;
            }
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// One try at handing the mouse back on a handle of its own. True when the
/// job is done or cannot be done by trying again (the endpoint is gone).
fn hand_back_once() -> bool {
    let found = match crate::discover::open_device() {
        Ok(f) => f,
        Err(e) => return hand_back_is_final(&e),
    };
    let mut dev = found.device;
    let p = match OnboardProfiles::new(&mut dev) {
        Ok(p) => p,
        Err(e) => return hand_back_is_final(&e),
    };
    match back_to_onboard(&p, &mut dev) {
        Ok(()) => true,
        Err(e) => hand_back_is_final(&e),
    }
}

/// Whether another try at handing the mouse back could not change this
/// failure: the endpoint is gone, the device has no onboard mode to hand back
/// to, or the node cannot be opened at all. Anything else may be one lost
/// reply, which is exactly what the retries are for.
fn hand_back_is_final(e: &crate::hidpp::Error) -> bool {
    use crate::hidpp::Error;
    matches!(e, Error::Io(_) | Error::NoSuchFeature(_) | Error::Permission(_))
}

/// Spawn the device worker. Returns immediately; the first snapshot arrives
/// once the worker has probed the device.
pub fn spawn() -> Handle {
    let (tx, rx) = mpsc::channel::<(CmdId, Cmd)>();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let shared = Arc::clone(&snapshot);
    let host_held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = Arc::clone(&host_held);
    let gate = Arc::new(Gate::new());
    let worker_gate = Arc::clone(&gate);
    std::thread::Builder::new()
        .name("logi-device".into())
        .spawn(move || worker(rx, shared, held, worker_gate))
        .expect("spawn device thread");
    // Ids start at 1 so that 0 can mean "no outcome recorded yet".
    Handle { tx, snapshot, next_id: Arc::new(AtomicU64::new(1)), host_held, gate }
}

/// Which node an effect thread is holding open, as the filesystem names it.
type NodeId = (u64, u64);

fn node_id(path: &str) -> Option<NodeId> {
    use std::os::unix::fs::MetadataExt;
    // A stat, not an open: nothing here touches the device itself.
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// True when the node an effect thread opened is no longer the node at that
/// path: unplugged, or unplugged and plugged back in.
///
/// `effects::Running` has no way to ask whether its thread is still alive, and
/// that thread exits on its own when a frame write fails — which is what an
/// unplug does. The worker would go on believing in a running effect that is
/// not, and with `effect` still set the loop top never reconnects. A replug
/// recreates the node, so the identity is compared and not just its presence;
/// a bare `exists()` misses a replug that lands between two polls. `None`
/// means the identity could not be taken when the effect started, in which
/// case nothing is claimed.
fn effect_node_gone(started_on: Option<NodeId>, path: &str) -> bool {
    match started_on {
        Some(id) => node_id(path) != Some(id),
        None => false,
    }
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
    /// What the listener thread has seen. Present even without a listener, so
    /// the worker does not have to ask whether there is one.
    pulse: Arc<crate::live::Pulse>,
    /// Dropped with the connection, which stops and joins its thread.
    _listener: Option<crate::live::Listener>,
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
    // Only onboard profiles announce anything. A device without them, or one
    // whose second handle will not open, simply falls back on the slow re-read.
    let pulse = Arc::new(crate::live::Pulse::default());
    let listener = profiles.as_ref().and_then(|p| {
        crate::live::Listener::spawn(&dev, p.feature_index(), Arc::clone(&pulse)).ok()
    });
    Ok(Live {
        dev,
        name: found.name,
        kind: found.kind,
        caps,
        leds,
        dpi,
        rate,
        profiles,
        pulse,
        _listener: listener,
    })
}

/// Cheap state: about seven HID++ round trips.
///
/// Kept separate from the profile sectors deliberately. Reading all five
/// profiles is 80 round trips, which is over a second of latency; doing that
/// after every click made the UI feel hung.
///
/// Returns whether the onboard mode was read off the device this time, as
/// opposed to `into.mode` being left at whatever it held before.
fn read_state(l: &mut Live, into: &mut Snapshot) -> bool {
    let mut mode_read = false;
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
        // Kept on a failed read rather than cleared, so one dropped reply does
        // not make the stage indicator flicker away.
        if let Ok(i) = p.dpi_index(&mut l.dev) {
            into.dpi_index = Some(i);
        }
        if let Ok(m) = p.mode(&mut l.dev) {
            into.mode = m;
            mode_read = true;
        }
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
    mode_read
}

/// Read just the onboard mode into the snapshot. True if it was read.
fn read_mode(l: &mut Live, into: &mut Snapshot) -> bool {
    let Some(p) = l.profiles.take() else { return false };
    let read = p.mode(&mut l.dev);
    l.profiles = Some(p);
    match read {
        Ok(m) => {
            into.mode = m;
            true
        }
        Err(_) => false,
    }
}

/// Decode a run of sector reads into profiles, one slot per profile index.
///
/// Position in `Snapshot::profiles` *is* the profile index. It is the number
/// the editor labels, the slot the Lighting panel reads the stored LED records
/// out of before `Cmd::PersistLighting`, and the index the editor hands back
/// in `Cmd::WriteProfile`. So a sector that will not read or will not decode
/// has to end the run rather than be skipped: skipping it slid every later
/// profile down one slot, which showed profile 3 under "profile 2" and wrote
/// profile 3's edits into profile 2's sector. One transient `busy` or one
/// short memoryRead reply was enough. Stopping short leaves the tail absent
/// instead, which every panel already renders as "not read off the mouse yet;
/// press Refresh".
///
/// Lazy on purpose: the iterator only issues the next read when this asks for
/// one, so a device that has stopped answering is not ground through four more
/// profiles at 16 calls and 800ms apiece — close to a minute in which no
/// queued command runs.
fn decode_profiles(
    sectors: impl IntoIterator<Item = crate::hidpp::Result<Vec<u8>>>,
) -> Vec<Profile> {
    let mut out = Vec::new();
    for read in sectors {
        match read.map(|blob| Profile::parse(&blob)) {
            Ok(Ok(parsed)) => out.push(parsed),
            _ => break,
        }
    }
    out
}

/// Replace the snapshot's profiles with what a re-read found.
///
/// Always replaces, even with nothing. Keeping the old list when the re-read
/// came back empty left the pre-write profile standing after a failed flash
/// write, which is exactly the case the re-read exists for: the sector may have
/// changed, the device is not answering, and the editor would then edit and
/// write back a profile that is no longer what the mouse holds. An absent
/// profile is rendered as "not read off the mouse yet", which is true.
///
/// True if every profile that was expected came back.
fn adopt_profiles(into: &mut Vec<Profile>, read: Vec<Profile>, expected: usize) -> bool {
    let complete = read.len() >= expected;
    *into = read;
    complete
}

/// The expensive part: every profile sector, 80 round trips. Only worth doing
/// on an explicit refresh or after something wrote flash.
///
/// True if there is nothing left unread: every profile came back, or the
/// device has no layout this app decodes so there is nothing to read.
fn read_profiles(l: &mut Live, into: &mut Snapshot) -> bool {
    let Some(p) = l.profiles.take() else { return true };
    let mut complete = true;
    // Only the 256-byte format-2 layout is decodable; another device may use a
    // different one, and guessing at it would print nonsense.
    if p.info.profile_format_id == 2 && p.info.sector_size as usize == crate::profile_blob::SECTOR_SIZE
    {
        let count = into.profile_count;
        let dev = &mut l.dev;
        let out = decode_profiles((0..count).map(|i| p.read_sector(dev, i as u16 + 1, None)));
        complete = adopt_profiles(&mut into.profiles, out, count as usize);
    }
    l.profiles = Some(p);
    complete
}

/// Say so when a re-read came back short, whatever else the command had to
/// report. A Refresh that read two profiles of five used to answer with
/// nothing at all, which reads as success.
fn note_short_read(notice: Option<String>, got: usize, want: usize) -> Option<String> {
    Some(match notice {
        None => format!("read {got} of {want} profiles; the rest would not read"),
        Some(n) => format!("{n}; only {got} of {want} profiles could be read back"),
    })
}

/// Write a sector dump to `path`, or leave the file alone and say why.
///
/// Both guards here are the ones `logi backup` already carries, and the GUI's
/// Backup button needs them more: it defaults to one fixed path, so it is the
/// same file every time, and it sits next to the controls that write flash —
/// it is the way back from them. Being told "backup written" over a file that
/// no longer holds a usable backup is the worst outcome available.
///
/// `dump_all` records a sector it could not read as absent rather than
/// failing, so a dump can come back entirely empty without any error.
fn write_backup(
    path: &str,
    model: &str,
    dump: &[(u16, Option<Vec<u8>>)],
) -> crate::hidpp::Result<Option<String>> {
    let readable = dump.iter().filter(|(_, d)| d.is_some()).count();
    // A dump where every sector is absent is not a backup — `restore` rejects
    // one — so writing it can only destroy the good backup already there.
    if readable == 0 {
        return Err(crate::hidpp::Error::Protocol(format!(
            "no sector could be read, so {path} was left alone"
        )));
    }
    // Written beside the target and renamed into place. `fs::write` truncates
    // first, so a write that failed or was interrupted halfway left the user
    // with neither the old backup nor a usable new one.
    let tmp = format!("{path}.part");
    let wrote = std::fs::write(&tmp, profile_io::dump_to_json(model, dump))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        // Not Error::Io: that is the class the worker loop reads as "the
        // device vanished", so an unwritable backup path used to drop a
        // perfectly good handle and pay for a full reconnect and re-read.
        return Err(crate::hidpp::Error::Protocol(format!("writing {path}: {e}")));
    }
    // A partial backup is still worth keeping, but the user has to know it is
    // partial before they trust it as the way back from a flash write.
    Ok(Some(if readable < dump.len() {
        format!(
            "backup written to {path}; {} of {} sectors would not read and are \
             recorded as absent",
            dump.len() - readable,
            dump.len()
        )
    } else {
        format!("backup written to {path}")
    }))
}

/// Hand the worker's snapshot to the UI.
///
/// The worker keeps the authoritative copy and is its only writer, so every
/// HID++ read happens with no lock held and the lock is taken for exactly one
/// clone. It used to be held across `read_state` and `read_profiles` — the
/// latter is 80 round trips, over a second — while the frame loop blocked on
/// the same lock every frame, which is precisely the stall this worker thread
/// exists to keep out of the UI.
///
/// Published in one assignment, so the outcome of a flash write and the
/// re-read that followed it still become visible together: the profile editor
/// settles a write by reading both out of the same snapshot.
/// How often an idle worker looks up: for a nudge from the listener, for an
/// effect that has died, for a command. Also the most a nudge is delayed.
const IDLE_TICK: Duration = Duration::from_millis(150);

/// How often an idle worker re-reads the device whether or not anything
/// announced a change. The backstop for a mouse that does not announce, and for
/// changes made by other software or by the service in another process.
const RECONCILE: Duration = Duration::from_millis(1500);

/// The fields a re-read can change and a panel can show.
///
/// Compared before and after so a re-read that found nothing new does not
/// publish, which would clone the whole snapshot (profiles included) ten times
/// a second for no one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveKey {
    dpi: u16,
    dpi_index: Option<u8>,
    rate: u16,
    profile: u8,
    mode: u8,
    events: u64,
}

impl LiveKey {
    fn of(s: &Snapshot) -> Self {
        LiveKey {
            dpi: s.dpi,
            dpi_index: s.dpi_index,
            rate: s.rate,
            profile: s.active_profile,
            mode: s.mode,
            events: s.events_seen,
        }
    }
}

enum Resync {
    /// Nothing a panel shows has changed.
    Same,
    Changed,
    /// The endpoint is gone: unplugged, or the node was replaced.
    Gone,
}

/// Re-read the cheap state and say whether it moved.
fn resync(l: &mut Live, snap: &mut Snapshot) -> Resync {
    // A read that fails is indistinguishable from "unchanged" if every error is
    // swallowed, which is how an unplugged mouse used to read as healthy. Only
    // an I/O failure means the endpoint is gone; a timeout means busy, and the
    // next pass will try again.
    match l.dev.protocol_version() {
        Err(crate::hidpp::Error::Io(_)) | Err(crate::hidpp::Error::NotFound(_)) => {
            return Resync::Gone;
        }
        Err(_) => return Resync::Same,
        Ok(_) => {}
    }
    let before = LiveKey::of(snap);
    read_state(l, snap);
    snap.events_seen = l.pulse.count();
    snap.last_event = l.pulse.last().map(|e| e.describe());
    if LiveKey::of(snap) == before {
        Resync::Same
    } else {
        Resync::Changed
    }
}

fn publish(shared: &Mutex<Snapshot>, snap: &Snapshot) {
    if let Ok(mut g) = shared.lock() {
        *g = snap.clone();
    }
}

/// Drop everything read off a device, keeping the record of what was asked of
/// it.
///
/// Reconnecting used to assign a fresh `Snapshot::default()`, which threw the
/// outcome ring away with it. A panel that had just been told its write failed
/// because the mouse was unplugged had that answer erased by the replug, and
/// went on waiting for one that no longer existed.
fn reset_for_connect(snap: &mut Snapshot) {
    let outcomes = std::mem::take(&mut snap.outcomes);
    *snap = Snapshot { outcomes, ..Snapshot::default() };
}

fn worker(
    rx: Receiver<(CmdId, Cmd)>,
    shared: Arc<Mutex<Snapshot>>,
    host_held: Arc<std::sync::atomic::AtomicBool>,
    gate: Arc<Gate>,
) {
    let mut live: Option<Live> = None;
    let mut effect: Option<effects::Running> = None;
    // Identity of the device node the running effect opened; see
    // `effect_node_gone`.
    let mut effect_node: Option<NodeId> = None;
    let mut pending: std::collections::VecDeque<(CmdId, Cmd)> =
        std::collections::VecDeque::new();
    // The worker's own copy. Mutated freely here and published in one clone,
    // so no device read ever happens with the UI's lock held.
    let mut snap = Snapshot::default();
    // Why an effect ended, held over the reconnect that follows. Reconnecting
    // wipes `error` along with everything else read off the device, so a
    // message set at retirement was gone again before the UI could draw it.
    let mut carried_error: Option<String> = None;
    let mut last_resync = std::time::Instant::now();

    loop {
        // Reconnect lazily so unplug/replug is handled without crashing.
        //
        // Not once the gate is closed: the exit-time restore has the device to
        // itself, and probing it now would talk over that.
        let mut back_off = false;
        if live.is_none() && effect.is_none() {
            if let Some(_busy) = gate.enter() {
                match connect() {
                    Ok(mut l) => {
                        reset_for_connect(&mut snap);
                        read_state(&mut l, &mut snap);
                        read_profiles(&mut l, &mut snap);
                        snap.error = carried_error.take();
                        live = Some(l);
                        last_resync = std::time::Instant::now();
                        publish(&shared, &snap);
                    }
                    Err(e) => {
                        carried_error = None;
                        snap.connected = false;
                        snap.error = Some(e);
                        publish(&shared, &snap);
                        back_off = true;
                    }
                }
            }
        }
        if back_off {
            // A missing device is the normal idle state. Slept outside the
            // gate, so a restore at exit is not made to wait out the back-off.
            std::thread::sleep(Duration::from_millis(1500));
        }

        // Anything queued by the previous iteration runs before we block.
        let (id, cmd) = match pending.pop_front() {
            Some(c) => c,
            None => match rx.recv_timeout(IDLE_TICK) {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // An effect thread that died on its own (the mouse was
                    // unplugged mid-effect) leaves `effect` set and `live`
                    // empty, a state the reconnect guard above cannot leave.
                    // Retire it here so the next pass reconnects and the
                    // snapshot stops claiming an effect that is not running.
                    //
                    // The thread can also exit while the node stays in place,
                    // when a write keeps failing for a reason other than an
                    // unplug, so ask the thread itself as well as the node.
                    let finished = effect.as_ref().map_or(false, |r| r.is_finished());
                    let gone = effect.is_some() && effect_node_gone(effect_node, &snap.path);
                    if finished || gone {
                        if let Some(r) = effect.take() {
                            r.stop();
                        }
                        effect_node = None;
                        snap.effect_running = None;
                        if gone {
                            snap.connected = false;
                            snap.error = Some(
                                "the effect ended because the mouse was disconnected"
                                    .to_string(),
                            );
                            // Carried like the message below: if the mouse is
                            // already back by the time the loop top reconnects,
                            // the reconnect wipes `error`, and this was the
                            // only explanation for the effect having stopped.
                            carried_error = snap.error.clone();
                        } else {
                            // Still connected, so do not say otherwise. The
                            // loop top reconnects now that `effect` is clear.
                            snap.error = Some(
                                "the effect stopped on its own; start it again to retry"
                                    .to_string(),
                            );
                            carried_error = snap.error.clone();
                        }
                        publish(&shared, &snap);
                    }

                    // Follow the device. Nothing else is asking it anything, so
                    // this is where a change made on the mouse itself, or by
                    // other software, gets noticed. Not while an effect holds
                    // the device: it has its own handle, and `live` is empty.
                    if effect.is_none() && live.is_some() {
                        let nudged = live.as_ref().map_or(false, |l| l.pulse.take_dirty());
                        if nudged || last_resync.elapsed() >= RECONCILE {
                            last_resync = std::time::Instant::now();
                            // Closed once the exit-time restore has the device.
                            if let Some(_busy) = gate.enter() {
                                match live.as_mut().map(|l| resync(l, &mut snap)) {
                                    Some(Resync::Changed) => publish(&shared, &snap),
                                    Some(Resync::Gone) => {
                                        snap.connected = false;
                                        snap.error =
                                            Some("the mouse was disconnected".to_string());
                                        live = None;
                                        publish(&shared, &snap);
                                    }
                                    Some(Resync::Same) | None => {}
                                }
                            }
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let kind = cmd.kind();

        // From here until the end of the iteration the device may be touched.
        // Once the exit-time restore has closed the gate nothing more runs: a
        // command that put the mouse back in host mode after the restore took
        // it out would outlive the app.
        let Some(_busy) = gate.enter() else {
            snap.record(Outcome::failed(id, kind, "the app is closing"));
            publish(&shared, &snap);
            continue;
        };

        // Every command ends a running effect first: the effect thread holds
        // its own handle on the device, and two owners would interleave
        // writes. Stopping it joins that thread, which hands the mouse back
        // and releases the device for the loop top to open again.
        let was_running = effect.is_some();
        if let Some(r) = effect.take() {
            r.stop();
        }
        effect_node = None;
        if was_running {
            snap.effect_running = None;
        }

        // Stopping needs no handle of its own, and must not be made to wait
        // for one. This used to fall through to the handle check below, which
        // fails every command with "device not connected" while an effect
        // runs — `live` is None for the whole of one — so Stop reported a
        // failure, left `effect` in place, and with `effect` in place the loop
        // top would not reconnect either: every later command failed the same
        // way and the only way out of the effect was quitting the app.
        if matches!(cmd, Cmd::StopEffect) {
            let msg = if was_running { "effect stopped" } else { "no effect was running" };
            snap.record(Outcome::ok(id, kind, Some(msg.to_string())));
            publish(&shared, &snap);
            continue;
        }
        if was_running {
            // The device is free now but not yet open. Requeue rather than
            // fail the command for a handle the loop top is about to take.
            publish(&shared, &snap);
            pending.push_front((id, cmd));
            continue;
        }

        // Marked before the command runs, not after it succeeds: a mode
        // switch whose reply was lost may still have landed, and until this
        // flag is set nothing at exit will hand the mouse back. See
        // `Cmd::takes_host_mode`.
        let takes_host = cmd.takes_host_mode();
        let touches_mode = takes_host || cmd.touches_mode();

        let Some(l) = live.as_mut() else {
            // Still an outcome: a panel waiting on this id must be told it
            // failed, or it waits for an answer that is never coming. No
            // device was touched, so the host flag is left as it was: setting
            // it here made every exit with the mouse unplugged hunt for a
            // device to hand back.
            snap.record(Outcome::failed(id, kind, "device not connected"));
            publish(&shared, &snap);
            continue;
        };
        if takes_host {
            host_held.store(true, Ordering::Relaxed);
        }

        let name_for_backup = l.name.clone();
        let wrote_flash = cmd.rereads_profiles();
        let result: crate::hidpp::Result<Option<String>> = match cmd {
            // read_state swallows every read error, so on its own a Refresh
            // of an unplugged mouse "succeeded", kept the dead handle, and
            // left the snapshot saying connected. One real call first lets
            // a vanished device surface as the Io error that drops the handle.
            Cmd::Refresh => l.dev.protocol_version().map(|_| None),
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
                    // Not `unwrap_or(0)`: a read that timed out would store
                    // this lighting into profile 0's flash while a different
                    // profile is the one the mouse is running.
                    let idx = p.current(dev)?;
                    let blob = p.read_sector(dev, idx as u16 + 1, None)?;
                    let mut prof =
                        Profile::parse(&blob).map_err(crate::hidpp::Error::Protocol)?;
                    prof.leds = recs;
                    prof.alt_leds = recs;
                    profile_io::write_profile(dev, p, idx, &prof)?;
                    // The flash write is done by now, so a failure here must
                    // not read as a failed write. It does matter though: the
                    // mouse is still in host mode and not applying what was
                    // just stored.
                    back_to_onboard(p, dev).map_err(|e| match e {
                        io @ crate::hidpp::Error::Io(_) => io,
                        other => crate::hidpp::Error::Protocol(format!(
                            "the lighting was stored, but the mouse could not be \
                             switched back to onboard mode ({other}); choose Onboard \
                             under Memory mode"
                        )),
                    })?;
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
            Cmd::Backup(path) => feat!(l, profiles, "onboard profiles")
                .and_then(|(p, dev)| {
                    let dump = profile_io::dump_all(dev, p);
                    write_backup(&path, &name_for_backup, &dump)
                }),
            Cmd::StartEffect { name, color, period_ms } => {
                match effects::start(name.clone(), color, period_ms) {
                    Ok(r) => {
                        effect = Some(r);
                        effect_node = node_id(&snap.path);
                        // The effect thread now owns the device.
                        live = None;
                        snap.effect_running = Some(name);
                        // Recorded like any other command: the success path
                        // used to record nothing, so a caller waiting on this
                        // id waited for an answer that was never written.
                        snap.record(Outcome::ok(
                            id,
                            kind,
                            Some("effect running; it stops when you stop it".to_string()),
                        ));
                        publish(&shared, &snap);
                        continue;
                    }
                    Err(e) => Err(crate::hidpp::Error::Protocol(e)),
                }
            }
            // Handled above, before the device handle is required.
            Cmd::StopEffect => Ok(None),
        };

        // True once this command has left both the live state and the profile
        // sectors freshly read, which is what makes a queued Refresh redundant.
        let mut reread = false;
        match result {
            Ok(mut notice) => {
                let mode_read = read_state(l, &mut snap);
                if wrote_flash || snap.profiles.is_empty() {
                    // Only a complete re-read makes a queued Refresh
                    // redundant; one that came back short is worth asking for
                    // again, and worth saying so.
                    reread = read_profiles(l, &mut snap);
                    if !reread {
                        notice = note_short_read(
                            notice,
                            snap.profiles.len(),
                            snap.profile_count as usize,
                        );
                    }
                }
                // A mode that could not be read this time says nothing, and
                // must not clear the flag: the earlier answer stands.
                host_held.store(
                    host_held_after(
                        host_held.load(Ordering::Relaxed),
                        mode_read.then_some(snap.mode),
                        takes_host,
                    ),
                    Ordering::Relaxed,
                );
                // Recorded with the re-read in one publish, so a panel that
                // settles a write against this outcome sees the post-write
                // sectors in the same snapshot.
                snap.record(Outcome::ok(id, kind, notice));
                publish(&shared, &snap);
            }
            Err(e) => {
                // A vanished device must drop the handle so the loop reconnects.
                let gone = matches!(
                    e,
                    crate::hidpp::Error::Io(_) | crate::hidpp::Error::NotFound(_)
                );
                // A failed flash write says nothing about what the sector now
                // holds: a reply lost to the timeout hides a write that landed,
                // and a half-finished one leaves a mix. The snapshot still
                // shows the profile as it was before, which the editor would
                // then edit and write back as if it were current. Read it
                // again so the UI sees the sector as it is, and publish that
                // with the failure.
                if !gone
                    && matches!(
                        kind,
                        CmdKind::WriteProfile(_) | CmdKind::SetButton(_) | CmdKind::PersistLighting
                    )
                {
                    read_profiles(l, &mut snap);
                }
                // The same goes for the mode: a switch whose reply was lost
                // may have landed, and the snapshot would go on showing the
                // old one. The Onboard control only offers itself while the
                // snapshot says host, so a stale "onboard" here is a mouse the
                // user has no button to bring back. One read, since the
                // device may not be answering.
                if !gone && touches_mode {
                    let mode_read = read_mode(l, &mut snap);
                    host_held.store(
                        host_held_after(
                            host_held.load(Ordering::Relaxed),
                            mode_read.then_some(snap.mode),
                            takes_host,
                        ),
                        Ordering::Relaxed,
                    );
                }
                // Recorded before the handle is dropped, so a panel waiting
                // on this id learns the write failed rather than timing out.
                snap.record(Outcome::failed(id, kind, e.to_string()));
                if gone {
                    // Said before the reconnect, which can spend seconds
                    // probing nodes: the snapshot should not claim a live
                    // mouse for that long.
                    snap.connected = false;
                    // Carried over the reconnect like the effect's message:
                    // a mouse that is already back (a USB hiccup, a replug)
                    // is reconnected before the next frame, and the reconnect
                    // wipes `error`, so the click that was lost would leave
                    // nothing in the status bar.
                    carried_error = snap.error.clone();
                }
                publish(&shared, &snap);
                if gone {
                    live = None;
                }
            }
        }

        // Fold a run of queued Refreshes into the command that just re-read
        // everything, but never drop a real command: the previous version
        // matched Ok(_) and broke, which silently threw away whatever it had
        // just received. Two quick clicks lost the second.
        //
        // Only after a command that re-read the profile sectors too. A Refresh
        // queued behind a DPI change is the user asking for those sectors, and
        // folding it away dropped the request. A folded one is still answered,
        // so a panel waiting on its id is not left waiting for good.
        let mut folded = false;
        while let Ok(next) = rx.try_recv() {
            if reread && matches!(next.1, Cmd::Refresh) {
                snap.record_silent(Outcome::ok(next.0, CmdKind::Refresh, None));
                folded = true;
                continue;
            }
            pending.push_back(next);
            break;
        }
        if folded {
            publish(&shared, &snap);
        }
    }
}

#[cfg(test)]
mod tests {
    fn snap_with_stages(stages: [u16; 5], active: u8, index: Option<u8>) -> Snapshot {
        // A profile parsed from a real-shaped sector, with only the DPI slots set.
        let mut blob = [0xFFu8; crate::profile_blob::SECTOR_SIZE];
        for (n, d) in stages.iter().enumerate() {
            blob[3 + n * 2..5 + n * 2].copy_from_slice(&d.to_le_bytes());
        }
        let crc = crate::profile_blob::crc_ccitt(&blob[..crate::profile_blob::OFF_CRC]);
        blob[crate::profile_blob::OFF_CRC..].copy_from_slice(&crc.to_be_bytes());
        let mut s = Snapshot::default();
        s.profiles = vec![Profile::parse(&blob).unwrap()];
        s.active_profile = active;
        s.dpi_index = index;
        // The sensor reads what the stage says, which is the normal case.
        s.dpi = index.and_then(|i| stages.get(i as usize).copied()).unwrap_or(0);
        s
    }

    #[test]
    fn the_stage_counts_only_enabled_slots() {
        // Slot 1 is disabled, so slot 2 is the second stage, not the third.
        let s = snap_with_stages([800, 0, 1600, 3200, 0], 0, Some(2));
        assert_eq!(s.dpi_stage(), Some((2, 3)));
        let s = snap_with_stages([800, 0, 1600, 3200, 0], 0, Some(0));
        assert_eq!(s.dpi_stage(), Some((1, 3)));
    }

    #[test]
    fn a_stage_that_is_not_there_has_no_position() {
        // Disabled slot, past the end, no index, and no such profile.
        assert_eq!(snap_with_stages([800, 0, 1600, 0, 0], 0, Some(1)).dpi_stage(), None);
        assert_eq!(snap_with_stages([800, 0, 1600, 0, 0], 0, Some(9)).dpi_stage(), None);
        assert_eq!(snap_with_stages([800, 0, 1600, 0, 0], 0, None).dpi_stage(), None);
        assert_eq!(snap_with_stages([800, 0, 1600, 0, 0], 3, Some(0)).dpi_stage(), None);
    }

    #[test]
    fn the_stages_are_the_ones_stored_on_the_mouse() {
        // A fourth stage of 2400 is not in any list of common values.
        let s = snap_with_stages([400, 800, 1600, 2400, 0], 0, Some(3));
        assert_eq!(s.dpi_stages(), vec![400, 800, 1600, 2400]);
        // Disabled slots are skipped, and the order is the slot order.
        let s = snap_with_stages([800, 0, 3200, 0, 6400], 0, Some(0));
        assert_eq!(s.dpi_stages(), vec![800, 3200, 6400]);
        // Nothing read yet means nothing to offer, not a made-up list.
        assert!(Snapshot::default().dpi_stages().is_empty());
        assert!(snap_with_stages([400, 800, 0, 0, 0], 3, None).dpi_stages().is_empty());
    }

    #[test]
    fn a_stage_is_shown_only_when_the_dpi_is_that_stage() {
        let mut s = snap_with_stages([400, 800, 1600, 2400, 0], 0, Some(2));
        assert_eq!(s.dpi_stage(), Some((3, 4)));
        // The index moved and the sensor has not caught up yet: the two
        // registers do not change together, and this must not read as stage 3.
        s.dpi = 800;
        assert_eq!(s.dpi_stage(), None);
        // A DPI set from the host that is no stage at all.
        s.dpi = 3200;
        assert_eq!(s.dpi_stage(), None);
    }

    #[test]
    fn the_live_key_sees_every_field_a_panel_shows() {
        let base = Snapshot::default();
        let key = LiveKey::of(&base);
        let mut s = base.clone();
        s.dpi = 1600;
        assert_ne!(LiveKey::of(&s), key, "dpi");
        let mut s = base.clone();
        s.dpi_index = Some(1);
        assert_ne!(LiveKey::of(&s), key, "dpi stage");
        let mut s = base.clone();
        s.rate = 500;
        assert_ne!(LiveKey::of(&s), key, "rate");
        let mut s = base.clone();
        s.active_profile = 2;
        assert_ne!(LiveKey::of(&s), key, "profile");
        let mut s = base.clone();
        s.mode = 2;
        assert_ne!(LiveKey::of(&s), key, "mode");
        let mut s = base.clone();
        s.events_seen = 1;
        assert_ne!(LiveKey::of(&s), key, "event count");
        assert_eq!(LiveKey::of(&base.clone()), key, "an untouched copy is the same");
    }

    use super::*;
    use crate::profile_blob::{Button, Led, LED_RECORD, OFF_CRC, OFF_RATE, SECTOR_SIZE};

    fn ok(id: CmdId, kind: CmdKind) -> Outcome {
        Outcome::ok(id, kind, Some(format!("outcome {id}")))
    }

    /// A decodable format-2 sector, tagged in the report-rate byte so one
    /// profile can be told apart from another by the slot it lands in.
    fn sector(tag: u8) -> Vec<u8> {
        let mut b = vec![0xFFu8; SECTOR_SIZE];
        b[OFF_RATE] = tag;
        let crc = crate::profile_blob::crc_ccitt(&b[..OFF_CRC]);
        b[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_be_bytes());
        b
    }

    #[test]
    fn a_refused_sector_truncates_the_profiles_instead_of_shifting_them() {
        // Position is the profile index everywhere that matters: the label in
        // the editor, the slot the Lighting panel reads stored LED records
        // out of, and the index `Cmd::WriteProfile` carries back to flash.
        // Dropping the refused sector and carrying on slid 4 and 5 into slots
        // 3 and 4, so saving slot 3 wrote profile 4's edits over profile 3.
        let reads = vec![
            Ok(sector(1)),
            Ok(sector(2)),
            Err(crate::hidpp::Error::Device { code: 0x08, feature: 0x0f, function: 5 }),
            Ok(sector(4)),
            Ok(sector(5)),
        ];
        let got = decode_profiles(reads);
        assert_eq!(got.len(), 2, "the tail must be absent, not renumbered");
        assert_eq!(got[0].report_rate_ms, 1);
        assert_eq!(got[1].report_rate_ms, 2);
    }

    #[test]
    fn a_sector_that_will_not_decode_ends_the_run_too() {
        // A short memoryRead reply is the other way a slot goes missing, and
        // it shifts the tail exactly the same way.
        let reads = vec![Ok(sector(1)), Ok(vec![0u8; 17]), Ok(sector(3))];
        let got = decode_profiles(reads);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].report_rate_ms, 1);
    }

    #[test]
    fn decode_profiles_stops_reading_once_a_sector_fails() {
        // One sector is 16 calls at the 800ms default timeout, so asking a
        // device that has stopped answering for four more profiles is close to
        // a minute in which the worker runs nothing that is queued.
        let mut asked = 0usize;
        let got = decode_profiles((0..5).map(|i| {
            asked += 1;
            if i < 2 {
                Ok(sector(i as u8 + 1))
            } else {
                Err(crate::hidpp::Error::Timeout { feature: 0x0f, function: 5 })
            }
        }));
        assert_eq!(got.len(), 2);
        assert_eq!(asked, 3, "the sectors after the failure were still read");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("logi-device-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn full_dump() -> Vec<(u16, Option<Vec<u8>>)> {
        (0..6u16).map(|i| (i, Some(sector(i as u8)))).collect()
    }

    #[test]
    fn a_backup_with_no_readable_sector_leaves_the_old_one_alone() {
        // The Backup button defaults to one fixed path, so this is the same
        // file every time, and it is the way back from a flash write. Writing
        // a dump with nothing in it — `restore` refuses one — over a good
        // backup and reporting success is the worst outcome available.
        let dir = temp_dir("backup-empty");
        let path = dir.join("logi-backup.json");
        std::fs::write(&path, "the good backup").unwrap();
        let empty: Vec<(u16, Option<Vec<u8>>)> = (0..6u16).map(|i| (i, None)).collect();

        let err = write_backup(path.to_str().unwrap(), "G502", &empty).unwrap_err();
        assert!(
            matches!(err, crate::hidpp::Error::Protocol(_)),
            "a filesystem or dump problem must not read as a vanished device: {err:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the good backup");
    }

    #[test]
    fn a_backup_lands_whole_and_leaves_no_part_file() {
        let dir = temp_dir("backup-whole");
        let path = dir.join("logi-backup.json");
        let msg = write_backup(path.to_str().unwrap(), "G502", &full_dump())
            .unwrap()
            .unwrap();
        assert!(msg.contains("backup written"), "{msg}");
        assert!(!msg.contains("absent"), "a whole backup must not warn: {msg}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"model\": \"G502\""), "{text}");
        assert!(!dir.join("logi-backup.json.part").exists());
    }

    #[test]
    fn a_partial_backup_is_kept_but_says_so() {
        // Worth keeping — restoring it leaves the unread sectors as they are —
        // but a user about to write flash has to know it is not the whole
        // device before they treat it as the way back.
        let dir = temp_dir("backup-partial");
        let path = dir.join("logi-backup.json");
        let mut dump = full_dump();
        dump[3].1 = None;
        dump[4].1 = None;
        let msg = write_backup(path.to_str().unwrap(), "G502", &dump).unwrap().unwrap();
        assert!(msg.contains("2 of 6 sectors"), "{msg}");
        assert!(path.exists());
    }

    #[test]
    fn an_unwritable_path_is_not_reported_as_a_lost_device() {
        // The worker reads Error::Io as "the mouse vanished" and drops the
        // device handle, which costs a reconnect and an 80-round-trip re-read
        // and tells the user about a disconnect that never happened.
        let dir = temp_dir("backup-unwritable");
        let path = dir.join("no-such-directory/logi-backup.json");
        let err = write_backup(path.to_str().unwrap(), "G502", &full_dump()).unwrap_err();
        assert!(matches!(err, crate::hidpp::Error::Protocol(_)), "{err:?}");
        // The half-written temporary must not be left behind as litter that
        // looks like a backup.
        assert!(!dir.join("no-such-directory").exists());
    }

    #[test]
    fn an_effect_whose_node_disappeared_is_noticed() {
        let dir = temp_dir("effect-node");
        let node = dir.join("hidraw0");
        std::fs::write(&node, "").unwrap();
        let path = node.to_str().unwrap();
        let id = node_id(path);
        assert!(id.is_some());
        assert!(!effect_node_gone(id, path), "a node that is still there is not gone");
        std::fs::remove_file(&node).unwrap();
        assert!(effect_node_gone(id, path), "an unplug has to retire the effect");
    }

    #[test]
    fn an_effect_with_no_recorded_node_is_never_called_dead() {
        // If the identity could not be taken at the start, claiming the effect
        // died would tear down one that is running.
        assert!(!effect_node_gone(None, "/nonexistent/hidraw9"));
    }

    #[test]
    fn record_flattens_the_newest_outcome_into_the_status_bar() {
        let mut s = Snapshot::default();
        s.record(Outcome::failed(1, CmdKind::SetDpi, "nope"));
        assert_eq!(s.error.as_deref(), Some("nope"));
        assert_eq!(s.notice, None);
        s.record(Outcome::ok(2, CmdKind::SetDpi, Some("DPI 800".into())));
        // A success has to clear the previous error, or the status bar keeps
        // reporting a failure the device has since taken back.
        assert_eq!(s.error, None);
        assert_eq!(s.notice.as_deref(), Some("DPI 800"));
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let mut s = Snapshot::default();
        for i in 1..=(OUTCOME_HISTORY as u64 * 3) {
            s.record(ok(i, CmdKind::Refresh));
        }
        assert_eq!(s.outcomes.len(), OUTCOME_HISTORY);
        assert_eq!(s.newest_outcome_id(), OUTCOME_HISTORY as u64 * 3);
        // Oldest first, contiguous: the tail of what was recorded.
        let ids: Vec<CmdId> = s.outcomes.iter().map(|o| o.id).collect();
        assert_eq!(ids.first(), Some(&(OUTCOME_HISTORY as u64 * 2 + 1)));
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_folded_refresh_is_answered_without_touching_the_status_bar() {
        let mut s = Snapshot::default();
        s.record(Outcome::ok(1, CmdKind::SetDpi, Some("DPI 800".into())));
        s.record_silent(Outcome::ok(2, CmdKind::Refresh, None));
        // The Refresh was answered, so whoever waits on it can stop...
        assert_eq!(s.outcome_after(1, CmdKind::Refresh).unwrap().id, 2);
        // ...and the notice of the command that did the work is still up.
        assert_eq!(s.notice.as_deref(), Some("DPI 800"));
        assert_eq!(s.error, None);
        // Same ring, same bound.
        for i in 3..(3 + OUTCOME_HISTORY as u64 * 2) {
            s.record_silent(ok(i, CmdKind::Refresh));
        }
        assert_eq!(s.outcomes.len(), OUTCOME_HISTORY);
    }

    #[test]
    fn newest_outcome_id_is_zero_before_anything_runs() {
        // Panels bound their search with this before they send, so it has to
        // be a value no command will ever be issued: ids start at 1.
        assert_eq!(Snapshot::default().newest_outcome_id(), 0);
    }

    #[test]
    fn outcome_after_ignores_an_answer_that_predates_the_send() {
        let mut s = Snapshot::default();
        s.record(ok(1, CmdKind::ApplyBindings));
        let after = s.newest_outcome_id();
        // Someone else's command lands in between.
        s.record(ok(2, CmdKind::SetDpi));
        assert!(s.outcome_after(after, CmdKind::ApplyBindings).is_none());
        s.record(ok(3, CmdKind::ApplyBindings));
        assert_eq!(s.outcome_after(after, CmdKind::ApplyBindings).unwrap().id, 3);
    }

    #[test]
    fn outcome_after_takes_the_oldest_match_not_the_newest() {
        // Two of the same kind in flight: the first answer belongs to the
        // first send, and handing it the second would settle it on the wrong
        // result.
        let mut s = Snapshot::default();
        s.record(Outcome::failed(1, CmdKind::SetRate, "refused"));
        s.record(ok(2, CmdKind::SetRate));
        let got = s.outcome_after(0, CmdKind::SetRate).unwrap();
        assert_eq!(got.id, 1);
        assert!(!got.ok);
    }

    #[test]
    fn write_outcomes_are_told_apart_by_profile() {
        // The GUI drops the id `send` returned, so the index in the kind is
        // the only thing keeping one profile's answer off another's draft.
        let mut s = Snapshot::default();
        s.record(ok(1, CmdKind::WriteProfile(2)));
        assert!(s.outcome_after(0, CmdKind::WriteProfile(1)).is_none());
        assert_eq!(s.outcome_after(0, CmdKind::WriteProfile(2)).unwrap().id, 1);
    }

    #[test]
    fn a_burst_cannot_push_out_an_answer_before_it_is_read() {
        // What the ring is for: a panel polls once a frame, so one outcome
        // has to survive everything other panels can queue in that frame.
        let mut s = Snapshot::default();
        s.record(ok(1, CmdKind::WriteProfile(0)));
        for i in 2..=(OUTCOME_HISTORY as u64) {
            s.record(ok(i, CmdKind::Led));
        }
        assert!(s.outcome(1).is_some());
        assert_eq!(s.outcome_after(0, CmdKind::WriteProfile(0)).unwrap().id, 1);
    }

    #[test]
    fn reset_for_connect_clears_the_device_but_keeps_the_answers() {
        let mut s = Snapshot::default();
        s.connected = true;
        s.model = "G502".into();
        s.dpi = 1600;
        s.mode = crate::profiles::MODE_HOST;
        s.record(Outcome::failed(7, CmdKind::WriteProfile(1), "device disconnected"));
        reset_for_connect(&mut s);
        assert!(!s.connected);
        assert_eq!(s.model, "");
        assert_eq!(s.dpi, 0);
        assert_eq!(s.mode, 0);
        // The replug must not erase the answer the panel is still waiting to
        // read, or it waits for one that no longer exists.
        assert_eq!(s.outcome(7).unwrap().message.as_deref(), Some("device disconnected"));
    }

    #[test]
    fn every_live_lighting_command_is_marked_as_taking_host_mode() {
        // Live 0x8070 only lands in host mode, so each of these switches into
        // it. Miss one and the mouse is left ignoring its stored profile.
        for c in [
            Cmd::LedFixed { zone: 0, color: [1, 2, 3] },
            Cmd::LedBreathing { zone: 0, color: [1, 2, 3], period: 2000, waveform: 0, intensity: 100 },
            Cmd::LedCycle { zone: 0, period: 2000, intensity: 100 },
            Cmd::LedOff { zone: 0 },
            Cmd::StartEffect { name: "rainbow".into(), color: [1, 2, 3], period_ms: 2000 },
            Cmd::SetMode(crate::profiles::MODE_HOST),
        ] {
            assert!(c.takes_host_mode(), "{c:?} leaves the device in host mode");
        }
    }

    #[test]
    fn commands_that_end_in_onboard_mode_do_not_claim_host_mode() {
        // PersistLighting switches back itself, and a plain refresh or a
        // sensor change never leaves onboard mode at all. Claiming host mode
        // here would mean a pointless device open on every exit.
        for c in [
            Cmd::SetMode(crate::profiles::MODE_ONBOARD),
            Cmd::Refresh,
            Cmd::SetDpi(800),
            Cmd::SetRate(1000),
            Cmd::PersistLighting { leds: [Led([0; LED_RECORD]); 2] },
            Cmd::StopEffect,
        ] {
            assert!(!c.takes_host_mode(), "{c:?} does not take host mode");
        }
    }

    #[test]
    fn everything_that_writes_flash_forces_a_profile_re_read() {
        for c in [
            Cmd::PersistLighting { leds: [Led([0; LED_RECORD]); 2] },
            Cmd::SetButton { index: 0, button: 1, binding: Button::disabled() },
            Cmd::Refresh,
        ] {
            assert!(c.rereads_profiles(), "{c:?} leaves the decoded profiles stale");
        }
        for c in [Cmd::SetDpi(800), Cmd::LedOff { zone: 0 }, Cmd::ApplyBindings { silence: vec![] }] {
            assert!(!c.rereads_profiles(), "{c:?} does not touch a profile sector");
        }
    }

    #[test]
    fn the_four_lighting_commands_answer_under_one_kind() {
        // The Lighting panel waits on CmdKind::Led whichever one it sent.
        assert_eq!(Cmd::LedOff { zone: 1 }.kind(), CmdKind::Led);
        assert_eq!(Cmd::LedFixed { zone: 0, color: [0; 3] }.kind(), CmdKind::Led);
        assert_eq!(Cmd::SetButton { index: 3, button: 4, binding: Button::disabled() }.kind(), CmdKind::SetButton(3));
        assert_eq!(Cmd::SetMode(1).kind(), CmdKind::SetMode);
    }

    #[test]
    fn a_closed_gate_waits_for_the_worker_and_then_refuses_it() {
        let gate = Gate::new();
        let busy = gate.enter().expect("an open gate lets the worker in");
        // The worker is mid-piece: closing cannot report idle yet.
        assert!(!gate.close_and_wait(Duration::from_millis(30)));
        // It is shut out from now on, even while the earlier piece runs on.
        assert!(gate.enter().is_none());
        drop(busy);
        assert!(gate.close_and_wait(Duration::from_millis(30)));
        // A refused entry must not leave the gate looking busy, or the
        // restore would wait out its whole limit for a worker that is idle.
        assert!(gate.enter().is_none());
        assert!(gate.close_and_wait(Duration::from_millis(30)));
    }

    #[test]
    fn an_idle_gate_closes_at_once() {
        let gate = Gate::new();
        let t = std::time::Instant::now();
        assert!(gate.close_and_wait(Duration::from_secs(5)));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_mode_that_could_not_be_read_does_not_clear_the_host_flag() {
        use crate::profiles::{MODE_HOST, MODE_ONBOARD};
        // Read and onboard: handed back, flag clears.
        assert!(!host_held_after(true, Some(MODE_ONBOARD), false));
        // Read and host: the flag stands.
        assert!(host_held_after(false, Some(MODE_HOST), false));
        // Not read: whatever was known before stands, either way.
        assert!(host_held_after(true, None, false));
        assert!(!host_held_after(false, None, false));
        // A command that may have taken host mode sets it regardless, even
        // when the read says onboard: its reply may have been lost in flight.
        assert!(host_held_after(false, Some(MODE_ONBOARD), true));
        assert!(host_held_after(false, None, true));
    }

    #[test]
    fn a_re_read_that_finds_nothing_replaces_the_stale_profiles() {
        let parse = |tag: u8| Profile::parse(&sector(tag)).unwrap();
        let mut held = vec![parse(1), parse(2), parse(3)];
        // The device stopped answering after a failed write: the old list is
        // what the sector held before, which is no longer known to be true.
        assert!(!adopt_profiles(&mut held, Vec::new(), 3));
        assert!(held.is_empty());

        let mut held = vec![parse(9)];
        assert!(!adopt_profiles(&mut held, vec![parse(1), parse(2)], 3));
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].report_rate_ms, 1);

        assert!(adopt_profiles(&mut held, vec![parse(1), parse(2), parse(3)], 3));
        assert_eq!(held.len(), 3);
    }

    #[test]
    fn a_short_read_is_said_aloud_and_keeps_the_commands_own_notice() {
        let bare = note_short_read(None, 2, 5).unwrap();
        assert!(bare.contains("2 of 5"), "{bare}");
        let kept = note_short_read(Some("profile 1 written to flash".into()), 0, 5).unwrap();
        assert!(kept.starts_with("profile 1 written to flash"), "{kept}");
        assert!(kept.contains("0 of 5"), "{kept}");
    }

    #[test]
    fn only_a_failure_another_try_cannot_change_ends_the_hand_back() {
        use crate::hidpp::Error;
        // Nothing to hand back to, or nothing to hand back on.
        assert!(hand_back_is_final(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        ))));
        assert!(hand_back_is_final(&Error::NoSuchFeature(0x8100)));
        assert!(hand_back_is_final(&Error::Permission("no permission".into())));
        // One lost reply, a busy device, a node that has not reappeared yet:
        // the mouse may still be in host mode, so these are tried again.
        assert!(!hand_back_is_final(&Error::Timeout { feature: 0x0f, function: 1 }));
        assert!(!hand_back_is_final(&Error::Device { code: 0x08, feature: 0x0f, function: 1 }));
        assert!(!hand_back_is_final(&Error::NotFound("no Logitech device found".into())));
        assert!(!hand_back_is_final(&Error::Protocol("short getInfo reply".into())));
    }

    #[test]
    fn commands_that_can_change_the_mode_are_the_ones_re_read_after_a_failure() {
        assert!(Cmd::SetMode(crate::profiles::MODE_ONBOARD).touches_mode());
        assert!(Cmd::PersistLighting { leds: [Led([0; LED_RECORD]); 2] }.touches_mode());
        // Not worth a read on a device that may be wedged.
        assert!(!Cmd::SetDpi(800).touches_mode());
        assert!(!Cmd::Refresh.touches_mode());
    }
}
