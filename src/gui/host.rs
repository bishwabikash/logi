//! What outlives the window: the panel icon and the service watcher.
//! The window really closes (Wayland cannot hide it); this stays behind and `run` in `gui` reopens it.

use std::cell::RefCell;
use std::sync::mpsc;

use crate::device::{Cmd, Snapshot};
use crate::dpi::Capability;
use crate::service;
use crate::tray::{Event, Status, Tray};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Open,
    Quit,
}

/// Retry timing for a failed registration: at login the panel's icon host often comes up after the service.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_ATTEMPTS: u32 = 100;

#[derive(Default)]
struct TrayCell {
    tried: bool,
    attempts: u32,
    failed_at: Option<std::time::Instant>,
    /// Registration in flight: `Tray::spawn` blocks on D-Bus (up to 25s), so it runs on its own thread.
    rx: Option<mpsc::Receiver<Result<Tray, String>>>,
    /// None when there is no StatusNotifierWatcher (a desktop without a tray host); the app works either way.
    tray: Option<Tray>,
}

pub struct Host {
    pub service: service::Watcher,
    tray: RefCell<TrayCell>,
    quit: std::sync::atomic::AtomicBool,
}

impl Host {
    pub fn new() -> Self {
        Host {
            service: service::Watcher::new(),
            tray: RefCell::new(TrayCell::default()),
            quit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether there is an icon to come back to; without one, closing the window must end the program.
    pub fn has_tray(&self) -> bool {
        self.tray.borrow().tray.is_some()
    }

    fn collect(cell: &mut TrayCell) {
        match cell.rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(Ok(t))) => {
                cell.tray = Some(t);
                cell.rx = None;
            }
            Some(Ok(Err(e))) => {
                // Said once, not on every retry.
                if cell.attempts == 1 {
                    eprintln!("logi: no tray icon yet: {e}");
                }
                cell.rx = None;
                cell.failed_at = Some(std::time::Instant::now());
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                cell.rx = None;
                cell.failed_at = Some(std::time::Instant::now());
            }
            _ => {}
        }
    }

    /// Keep trying for up to `limit`, for a start with no window to drive retries. True once there is an icon.
    pub fn await_tray(&self, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        loop {
            self.sync(&Snapshot::default(), &mut |_| {});
            if self.has_tray() {
                return true;
            }
            if std::time::Instant::now() >= deadline || self.gave_up() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    fn gave_up(&self) -> bool {
        let cell = self.tray.borrow();
        cell.tray.is_none() && cell.rx.is_none() && cell.attempts >= MAX_ATTEMPTS
    }

    /// Briefly wait for a registration in flight, so a window closed right after opening does not conclude there is no icon.
    pub fn wait_for_tray(&self, limit: std::time::Duration) {
        let deadline = std::time::Instant::now() + limit;
        loop {
            {
                let mut cell = self.tray.borrow_mut();
                Self::collect(&mut cell);
                if cell.rx.is_none() || std::time::Instant::now() >= deadline {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    pub fn request_quit(&self) {
        self.quit.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn quitting(&self) -> bool {
        self.quit.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Register the icon once, keep it in step with `snap`, and act on its menu. `send` differs between
    /// the window (tracks flash writes) and the background loop.
    pub fn sync(&self, snap: &Snapshot, send: &mut dyn FnMut(Cmd)) -> Option<Signal> {
        // Build the real status first: Status::default() would claim a service state nothing has looked at.
        let status = status_for(snap, self.service.state());
        let mut cell = self.tray.borrow_mut();

        // A failed attempt is tried again after a pause, up to a limit.
        if cell.tried
            && cell.tray.is_none()
            && cell.rx.is_none()
            && cell.attempts < MAX_ATTEMPTS
            && cell.failed_at.is_some_and(|t| t.elapsed() >= RETRY_EVERY)
        {
            cell.tried = false;
        }
        if !cell.tried {
            cell.tried = true;
            cell.attempts += 1;
            // Registering blocks, so it gets its own thread.
            let (tx, rx) = mpsc::channel();
            let initial = status.clone();
            match std::thread::Builder::new().name("logi-tray-register".into()).spawn(move || {
                let _ = tx.send(Tray::spawn(initial));
            }) {
                Ok(_) => cell.rx = Some(rx),
                Err(e) => eprintln!("logi: no tray icon: {e}"),
            }
        }
        // Non-blocking: nothing here waits on the registration.
        Self::collect(&mut cell);
        let tray = cell.tray.as_ref()?;

        // update() is a no-op when nothing changed, so this is cheap per call.
        tray.update(status);

        let mut signal = None;
        for ev in tray.poll() {
            match ev {
                Event::Open => signal = signal.or(Some(Signal::Open)),
                Event::Quit => signal = Some(Signal::Quit),
                Event::ToggleService | Event::RestartService => {
                    // Same gate as the Service tab's buttons: the menu has no greyed state, so clicks before the first poll,
                    // mid-action, or with no systemd must be refused.
                    if let Some(a) = super::tray_service_action(
                        matches!(ev, Event::ToggleService),
                        self.service.known(),
                        self.service.state(),
                        self.service.busy().is_some(),
                    ) {
                        self.service.request(a);
                    }
                }
                Event::SetProfile(p) => send(Cmd::SetProfile(p)),
                Event::SetDpi(d) => send(Cmd::SetDpi(d)),
            }
        }
        signal
    }
}

pub fn status_for(snap: &Snapshot, service: service::State) -> Status {
    Status {
        device: snap.connected.then(|| snap.model.clone()),
        service,
        active_profile: snap.active_profile,
        profile_count: snap.profile_count,
        dpi: snap.dpi,
        // The device's own range, not a fixed list: with no mouse there is nothing to offer.
        dpi_presets: match (snap.connected, &snap.dpi_cap) {
            (true, Some(Capability::Range { min, max, .. })) => [400u16, 800, 1600, 3200, 6400]
                .into_iter()
                .filter(|p| p >= min && p <= max)
                .collect(),
            (true, Some(Capability::List(v))) => v.clone(),
            _ => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mouse_means_no_dpi_menu() {
        let s = Snapshot { dpi_cap: Some(Capability::Range { min: 100, max: 25600, step: 50 }), ..Default::default() };
        let st = status_for(&s, service::State::Active);
        assert!(st.device.is_none());
        assert!(st.dpi_presets.is_empty());
    }

    #[test]
    fn presets_come_from_the_range_the_mouse_reported() {
        let s = Snapshot {
            connected: true,
            model: "G502".into(),
            dpi_cap: Some(Capability::Range { min: 100, max: 3200, step: 50 }),
            ..Default::default()
        };
        let st = status_for(&s, service::State::Active);
        assert_eq!(st.dpi_presets, vec![400, 800, 1600, 3200]);
        assert_eq!(st.device.as_deref(), Some("G502"));
    }

    #[test]
    fn a_new_host_is_neither_quitting_nor_holding_an_icon() {
        let h = Host::new();
        assert!(!h.quitting());
        assert!(!h.has_tray());
        h.request_quit();
        assert!(h.quitting());
    }
}
