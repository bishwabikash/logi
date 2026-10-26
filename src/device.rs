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

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
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
}

/// A snapshot of device state for the UI to render. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub model: String,
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
    tx: Sender<Cmd>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Set while the worker holds the device in host mode, so shutdown knows
    /// whether it has to hand the mouse back.
    host_held: Arc<std::sync::atomic::AtomicBool>,
}

impl Handle {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
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
    let (tx, rx) = mpsc::channel::<Cmd>();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let shared = Arc::clone(&snapshot);
    let host_held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = Arc::clone(&host_held);
    std::thread::Builder::new()
        .name("g502-device".into())
        .spawn(move || worker(rx, shared, held))
        .expect("spawn device thread");
    Handle { tx, snapshot, host_held }
}

/// Take host mode only if we are not already in it. Each switch costs a
/// 150ms settle, which is very noticeable when it happens on every click.
fn ensure_host(l: &mut Live) -> crate::hidpp::Result<()> {
    if l.profiles.mode(&mut l.dev).unwrap_or(0) == crate::profiles::MODE_HOST {
        return Ok(());
    }
    l.profiles.set_mode(&mut l.dev, crate::profiles::MODE_HOST)
}

struct Live {
    dev: Device,
    model: &'static str,
    leds: Leds,
    dpi: Dpi,
    rate: ReportRate,
    profiles: OnboardProfiles,
}

fn connect() -> Result<Live, String> {
    let found = discover::open_device().map_err(|e| e.to_string())?;
    let mut dev = found.device;
    let leds = Leds::new(&mut dev).map_err(|e| e.to_string())?;
    let dpi = Dpi::new(&mut dev).map_err(|e| e.to_string())?;
    let rate = ReportRate::new(&mut dev).map_err(|e| e.to_string())?;
    let profiles = OnboardProfiles::new(&mut dev).map_err(|e| e.to_string())?;
    Ok(Live { dev, model: found.model, leds, dpi, rate, profiles })
}

/// Cheap state: about seven HID++ round trips.
///
/// Kept separate from the profile sectors deliberately. Reading all five
/// profiles is 80 round trips, which is over a second of latency; doing that
/// after every click made the UI feel hung.
fn read_state(l: &mut Live, into: &mut Snapshot) {
    into.connected = true;
    into.model = l.model.to_string();
    into.path = l.dev.path.clone();
    if let Ok((a, b)) = l.dev.protocol_version() {
        into.protocol = format!("{a}.{b}");
    }
    into.dpi = l.dpi.get(&mut l.dev, 0).unwrap_or(into.dpi);
    if into.dpi_cap.is_none() {
        into.dpi_cap = l.dpi.capability(&mut l.dev, 0).ok();
    }
    into.rate = l.rate.get(&mut l.dev).unwrap_or(into.rate);
    if into.rates.is_empty() {
        into.rates = l.rate.supported(&mut l.dev).unwrap_or_default();
    }
    into.profile_count = l.profiles.info.profile_count;
    into.button_count = l.profiles.info.button_count;
    into.active_profile = l.profiles.current(&mut l.dev).unwrap_or(into.active_profile);
    into.mode = l.profiles.mode(&mut l.dev).unwrap_or(into.mode);
    if into.zones.is_empty() {
        into.zones = l
            .leds
            .zones
            .iter()
            .map(|z| ZoneInfo {
                index: z.index,
                location: z.location_name().to_string(),
                modes: z.modes.clone(),
                live_modes: l.leds.live_modes(z.index),
            })
            .collect();
    }
}

/// The expensive part: every profile sector, 80 round trips. Only worth doing
/// on an explicit refresh or after something wrote flash.
fn read_profiles(l: &mut Live, into: &mut Snapshot) {
    let mut out = Vec::new();
    for i in 0..into.profile_count {
        if let Ok(blob) = l.profiles.read_sector(&mut l.dev, i as u16 + 1, None) {
            if let Ok(p) = Profile::parse(&blob) {
                out.push(p);
            }
        }
    }
    if !out.is_empty() {
        into.profiles = out;
    }
}

fn read_snapshot(l: &mut Live) -> Snapshot {
    let mut s = Snapshot {
        connected: true,
        model: l.model.to_string(),
        path: l.dev.path.clone(),
        ..Default::default()
    };
    if let Ok((a, b)) = l.dev.protocol_version() {
        s.protocol = format!("{a}.{b}");
    }
    s.dpi = l.dpi.get(&mut l.dev, 0).unwrap_or(0);
    s.dpi_cap = l.dpi.capability(&mut l.dev, 0).ok();
    s.rate = l.rate.get(&mut l.dev).unwrap_or(0);
    s.rates = l.rate.supported(&mut l.dev).unwrap_or_default();
    s.profile_count = l.profiles.info.profile_count;
    s.button_count = l.profiles.info.button_count;
    s.active_profile = l.profiles.current(&mut l.dev).unwrap_or(0);
    s.mode = l.profiles.mode(&mut l.dev).unwrap_or(0);
    s.zones = l
        .leds
        .zones
        .iter()
        .map(|z| ZoneInfo {
            index: z.index,
            location: z.location_name().to_string(),
            modes: z.modes.clone(),
            live_modes: l.leds.live_modes(z.index),
        })
        .collect();
    for i in 0..s.profile_count {
        if let Ok(blob) = l.profiles.read_sector(&mut l.dev, i as u16 + 1, None) {
            if let Ok(p) = Profile::parse(&blob) {
                s.profiles.push(p);
            }
        }
    }
    s
}

fn worker(
    rx: Receiver<Cmd>,
    shared: Arc<Mutex<Snapshot>>,
    host_held: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut live: Option<Live> = None;
    let mut effect: Option<effects::Running> = None;
    let mut pending: std::collections::VecDeque<Cmd> = std::collections::VecDeque::new();

    loop {
        // Reconnect lazily so unplug/replug is handled without crashing.
        if live.is_none() && effect.is_none() {
            match connect() {
                Ok(mut l) => {
                    let snap = read_snapshot(&mut l);
                    if let Ok(mut g) = shared.lock() {
                        *g = snap;
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
        let cmd = match pending.pop_front() {
            Some(c) => c,
            None => match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };

        // Any command other than StopEffect ends a running effect first: the
        // effect holds the device, and two owners would interleave writes.
        if !matches!(cmd, Cmd::StopEffect) {
            if let Some(r) = effect.take() {
                r.stop();
            }
        }

        let Some(l) = live.as_mut() else {
            if let Ok(mut g) = shared.lock() {
                g.error = Some("device not connected".into());
            }
            continue;
        };

        let wrote_flash = matches!(
            cmd,
            Cmd::PersistLighting { .. }
                | Cmd::WriteProfile { .. }
                | Cmd::SetButton { .. }
                | Cmd::Refresh
        );
        let result: crate::hidpp::Result<Option<String>> = match cmd {
            Cmd::Refresh => Ok(None),
            Cmd::SetDpi(v) => l.dpi.set(&mut l.dev, v, 0).map(|got| {
                Some(if got == v { format!("DPI {got}") } else { format!("DPI snapped to {got}") })
            }),
            Cmd::SetRate(hz) => l.rate.set(&mut l.dev, hz).map(|_| Some(format!("{hz} Hz"))),
            Cmd::SetMode(m) => l
                .profiles
                .set_mode(&mut l.dev, m)
                .map(|_| Some(format!("mode: {}", crate::profiles::mode_name(m)))),
            Cmd::SetProfile(i) => {
                l.profiles.set_current(&mut l.dev, i).map(|_| Some(format!("profile {i}")))
            }
            Cmd::LedFixed { zone, color } => ensure_host(l)
                .and_then(|_| l.leds.fixed(&mut l.dev, zone, color))
                .map(|_| Some("lighting applied live".into())),
            Cmd::LedBreathing { zone, color, period, waveform, intensity } => ensure_host(l)
                .and_then(|_| {
                    l.leds.breathing(&mut l.dev, zone, color, period, waveform, intensity)
                })
                .map(|_| Some("breathing applied live".into())),
            Cmd::LedCycle { zone, period, intensity } => ensure_host(l)
                .and_then(|_| l.leds.cycle(&mut l.dev, zone, period, intensity))
                .map(|_| Some("cycle applied live".into())),
            Cmd::LedOff { zone } => ensure_host(l)
                .and_then(|_| l.leds.off(&mut l.dev, zone))
                .map(|_| Some("lighting off".into())),
            Cmd::PersistLighting { leds } => {
                let idx = l.profiles.current(&mut l.dev).unwrap_or(0);
                l.profiles
                    .read_sector(&mut l.dev, idx as u16 + 1, None)
                    .and_then(|blob| {
                        let mut p = Profile::parse(&blob)
                            .map_err(crate::hidpp::Error::Protocol)?;
                        p.leds = leds;
                        p.alt_leds = leds;
                        profile_io::write_profile(&mut l.dev, &l.profiles, idx, &p)
                    })
                    .and_then(|_| l.profiles.set_mode(&mut l.dev, MODE_ONBOARD))
                    .map(|_| Some("stored to onboard profile (survives unplug)".into()))
            }
            Cmd::SetButton { index, button, binding } => {
                profile_io::set_button(&mut l.dev, &l.profiles, index, button, binding)
                    .map(|_| Some(format!(
                        "button {button} -> {} (profile {index}, flash)",
                        binding.describe()
                    )))
            }
            Cmd::WriteProfile { index, profile } => {
                profile_io::write_profile(&mut l.dev, &l.profiles, index, &profile)
                    .map(|_| Some(format!("profile {index} written to flash")))
            }
            Cmd::Backup(path) => {
                let dump = profile_io::dump_all(&mut l.dev, &l.profiles);
                let json = profile_io::dump_to_json(l.model, &dump);
                match std::fs::write(&path, json) {
                    Ok(()) => Ok(Some(format!("backup written to {path}"))),
                    Err(e) => Err(crate::hidpp::Error::Io(e)),
                }
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
                    g.notice = notice;
                    g.error = None;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                // A vanished device must drop the handle so the loop reconnects.
                let gone = matches!(e, crate::hidpp::Error::Io(_))
                    || matches!(e, crate::hidpp::Error::NotFound(_));
                if let Ok(mut g) = shared.lock() {
                    g.error = Some(msg);
                    g.notice = None;
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
            if matches!(next, Cmd::Refresh) {
                continue;
            }
            pending.push_back(next);
            break;
        }
    }
}
