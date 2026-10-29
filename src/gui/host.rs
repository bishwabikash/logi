//! What outlives the window: the panel icon and the service watcher.
//!
//! Closing the window used to end the process, and the icon with it. Hiding a
//! window is not an option where it matters most: Wayland has no way for an
//! application to hide its own window, so a "hidden" one just stays on screen
//! or sits minimised with its icon and title in the dock. So the window really
//! closes, and this is what stays behind it. `run` in `gui` keeps the icon up
//! between windows and opens a new one when the icon asks.

use std::cell::RefCell;
use std::sync::mpsc;

use crate::device::{Cmd, Snapshot};
use crate::dpi::Capability;
use crate::service;
use crate::tray::{Event, Status, Tray};

/// What the panel icon asked the window to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Show the window.
    Open,
    /// Leave altogether, icon included.
    Quit,
}

#[derive(Default)]
struct TrayCell {
    tried: bool,
    /// Registration in flight. `Tray::spawn` blocks on a session-bus round trip,
    /// and where the watcher name is activatable but never comes up it blocks
    /// for the 25s D-Bus reply timeout, so it runs on its own thread and the
    /// handle arrives through here.
    rx: Option<mpsc::Receiver<Result<Tray, String>>>,
    /// None when there is no StatusNotifierWatcher, which is normal on a
    /// desktop without a tray host. The app works either way.
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

    /// Whether there is an icon to come back to. Without one, closing the
    /// window has to end the program: nothing would be left to reopen it from.
    pub fn has_tray(&self) -> bool {
        self.tray.borrow().tray.is_some()
    }

    /// Take the registration's answer if it has arrived.
    fn collect(cell: &mut TrayCell) {
        match cell.rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(Ok(t))) => {
                cell.tray = Some(t);
                cell.rx = None;
            }
            Some(Ok(Err(e))) => {
                eprintln!("logi: no tray icon: {e}");
                cell.rx = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => cell.rx = None,
            _ => {}
        }
    }

    /// Wait, for a short while, for a registration that is still in flight.
    ///
    /// A window closed within a second or two of opening would otherwise find
    /// no icon yet, conclude there is none, and end the program, when the icon
    /// was about to appear.
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

    /// Register the icon once, keep it in step with `snap`, and act on its
    /// menu. `send` is how a command reaches the mouse, which differs between
    /// the window (it tracks flash writes) and the background loop.
    pub fn sync(&self, snap: &Snapshot, send: &mut dyn FnMut(Cmd)) -> Option<Signal> {
        // Build the real status first: registering with Status::default()
        // made the first icon claim a service state nothing had looked at yet.
        let status = status_for(snap, self.service.state());
        let mut cell = self.tray.borrow_mut();

        if !cell.tried {
            cell.tried = true;
            // Registering blocks, so it goes on a thread of its own, exactly
            // as `Tray::spawn` says it must.
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
                    // The same gate the Service tab puts on its buttons. The
                    // menu has no greyed state of its own, so without it a
                    // click before the first poll acted on a default, one
                    // during an action queued a second systemctl, and one
                    // with no unit or no systemd could only produce an error.
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

/// What the icon renders for this mouse and service.
pub fn status_for(snap: &Snapshot, service: service::State) -> Status {
    Status {
        device: snap.connected.then(|| snap.model.clone()),
        service,
        active_profile: snap.active_profile,
        profile_count: snap.profile_count,
        dpi: snap.dpi,
        // The device's own range, not a fixed list: with no mouse attached
        // there is nothing to offer, and offering it anyway produced a DPI
        // menu that could not work.
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
