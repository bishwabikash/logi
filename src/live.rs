//! Following the device: what it announces on its own.
//!
//! A mouse with onboard profiles tells the host when its current profile or its
//! current DPI stage changes, which is what happens when someone presses the
//! profile or DPI button on it. Without listening for that, the app only learns
//! about a change when the user clicks something and a command happens to
//! re-read the state, so the window and the tray keep showing a stale value.
//!
//! Both events come from 0x8100. The event id is the high nibble of byte 3 and
//! the software id in the low nibble is zero, which is what tells an event from
//! the reply to somebody's request. Per Solaar's notification handler:
//!
//! | event | byte 3 | payload                                        |
//! |-------|--------|------------------------------------------------|
//! | 0     | `0x00` | profile changed: u16 big-endian profile sector |
//! | 1     | `0x10` | DPI stage changed: the stage *index*, not DPI  |
//!
//! The DPI event carries an index, so the value is read afterwards.
//!
//! This is the fast path, not the only one. Whether a given mouse sends these
//! in every mode is not something to assume, and other software or the
//! background service can change state too, so the worker also re-reads on a
//! slow timer. Events are what keep that from feeling slow.

use crate::hidpp::{Device, Error, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const EV_PROFILE_CHANGED: u8 = 0;
const EV_DPI_INDEX_CHANGED: u8 = 1;

/// How long the listener waits for a report before checking whether it has
/// been asked to stop. Also the longest a drop can take to join.
const TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveEvent {
    /// The active profile changed. Carries that profile's flash sector.
    ProfileChanged(u16),
    /// The active DPI stage changed. Carries the stage index.
    DpiIndexChanged(u8),
}

impl LiveEvent {
    /// A short phrase for the diagnostics line.
    pub fn describe(self) -> String {
        match self {
            // Profile N lives in sector N+1; sector 0 is the directory.
            LiveEvent::ProfileChanged(s) => format!("profile {}", s.saturating_sub(1)),
            LiveEvent::DpiIndexChanged(i) => format!("DPI stage {}", i + 1),
        }
    }
}

/// Read one input report as an announcement from the onboard-profiles feature.
///
/// `onboard_idx` is that feature's index on this device, not its id: indices
/// differ between models.
pub fn decode(report: &[u8], onboard_idx: u8) -> Option<LiveEvent> {
    if report.len() < 5 {
        return None;
    }
    if !matches!(report[0], 0x10 | 0x11 | 0x12) {
        return None;
    }
    if report[2] != onboard_idx {
        return None;
    }
    // A reply carries somebody's software id here; an event carries zero.
    if report[3] & 0x0F != 0 {
        return None;
    }
    match report[3] >> 4 {
        EV_PROFILE_CHANGED => {
            let sector = u16::from_be_bytes([*report.get(4)?, *report.get(5)?]);
            // Solaar ignores a zero sector, which is not a profile.
            (sector != 0).then_some(LiveEvent::ProfileChanged(sector))
        }
        EV_DPI_INDEX_CHANGED => Some(LiveEvent::DpiIndexChanged(report[4])),
        _ => None,
    }
}

fn pack(ev: LiveEvent) -> u32 {
    match ev {
        LiveEvent::ProfileChanged(s) => (1 << 16) | s as u32,
        LiveEvent::DpiIndexChanged(i) => (2 << 16) | i as u32,
    }
}

fn unpack(v: u32) -> Option<LiveEvent> {
    match v >> 16 {
        1 => Some(LiveEvent::ProfileChanged(v as u16)),
        2 => Some(LiveEvent::DpiIndexChanged(v as u8)),
        _ => None,
    }
}

/// What the listener thread hands the worker.
///
/// Atomics rather than a channel. The worker waits on the command channel, and
/// a sender held inside the worker would keep that channel open forever, so the
/// worker would never see the app close. Flags it checks on each idle tick cost
/// at most one tick of latency and none of that.
#[derive(Default)]
pub struct Pulse {
    dirty: AtomicBool,
    count: AtomicU64,
    last: AtomicU32,
}

impl Pulse {
    pub fn note(&self, ev: LiveEvent) {
        // Written before the flag, so a reader that sees the flag sees these.
        self.last.store(pack(ev), Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
    }

    /// True once per burst of events: reading it clears it.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// How many events the mouse has announced since this connection.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn last(&self) -> Option<LiveEvent> {
        unpack(self.last.load(Ordering::Relaxed))
    }
}

/// A thread reading the device's unsolicited reports on a handle of its own.
///
/// Its own handle, because `call` drains the control handle's queue before each
/// request, and sharing one would throw away an announcement as a side effect
/// of reading the DPI. hidraw queues per open file, so this one gets its own
/// copy of everything.
///
/// Dropping it stops the thread and waits for it, at most one tick.
pub struct Listener {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Listener {
    pub fn spawn(dev: &Device, onboard_idx: u8, pulse: Arc<Pulse>) -> Result<Self> {
        let mut listener = dev.open_listener()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("logi-events".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.read_event(TICK) {
                        Ok(Some(report)) => {
                            if let Some(ev) = decode(&report, onboard_idx) {
                                pulse.note(ev);
                            }
                        }
                        Ok(None) => {}
                        // The endpoint is gone. The worker finds that out for
                        // itself on its next probe and reconnects, which makes
                        // a new listener; there is nothing to report from here.
                        Err(_) => break,
                    }
                }
            })
            .map_err(Error::Io)?;
        Ok(Listener { stop, handle: Some(handle) })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long report as the device sends it: 0x11, wired index, feature index,
    /// then event/function nibble and software id, then payload.
    fn report(feature: u8, byte3: u8, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0x11, 0xFF, feature, byte3];
        r.extend_from_slice(payload);
        r.resize(20, 0);
        r
    }

    #[test]
    fn a_dpi_index_event_carries_the_stage_index() {
        let r = report(12, 0x10, &[2]);
        assert_eq!(decode(&r, 12), Some(LiveEvent::DpiIndexChanged(2)));
    }

    #[test]
    fn a_profile_event_carries_a_big_endian_sector() {
        // Sector 3, written big-endian: 0x0003, not 0x0300.
        let r = report(12, 0x00, &[0x00, 0x03]);
        assert_eq!(decode(&r, 12), Some(LiveEvent::ProfileChanged(3)));
        let r = report(12, 0x00, &[0x01, 0x00]);
        assert_eq!(decode(&r, 12), Some(LiveEvent::ProfileChanged(256)));
    }

    #[test]
    fn a_zero_sector_is_not_a_profile() {
        assert_eq!(decode(&report(12, 0x00, &[0, 0]), 12), None);
    }

    #[test]
    fn a_reply_is_not_an_event() {
        // The low nibble is somebody's software id. A getCurrentProfile reply
        // would otherwise read as a profile change every time anyone asked.
        assert_eq!(decode(&report(12, 0x48, &[0, 3]), 12), None);
        assert_eq!(decode(&report(12, 0x18, &[2]), 12), None);
        assert_eq!(decode(&report(12, 0x01, &[2]), 12), None);
    }

    #[test]
    fn another_feature_is_ignored() {
        // Index 13 is button spy on the G502; its presses must not look like a
        // DPI change just because the byte pattern matches.
        assert_eq!(decode(&report(13, 0x10, &[2]), 12), None);
    }

    #[test]
    fn the_feature_index_is_not_assumed() {
        // Indices are per device. 5 here is not 12.
        assert_eq!(decode(&report(5, 0x10, &[1]), 5), Some(LiveEvent::DpiIndexChanged(1)));
        assert_eq!(decode(&report(5, 0x10, &[1]), 12), None);
    }

    #[test]
    fn an_unknown_event_id_is_ignored() {
        assert_eq!(decode(&report(12, 0x20, &[1]), 12), None);
        assert_eq!(decode(&report(12, 0xF0, &[1]), 12), None);
    }

    #[test]
    fn junk_does_not_panic() {
        assert_eq!(decode(&[], 12), None);
        assert_eq!(decode(&[0x11], 12), None);
        assert_eq!(decode(&[0x11, 0xFF, 12, 0x00], 12), None);
        // A profile event too short to hold its sector.
        assert_eq!(decode(&[0x11, 0xFF, 12, 0x00, 0x00], 12), None);
        // Not a HID++ report at all.
        assert_eq!(decode(&[0x01, 0xFF, 12, 0x10, 2, 0, 0], 12), None);
    }

    #[test]
    fn a_short_report_still_decodes() {
        // 7 bytes, report id 0x10: the short form.
        let r = [0x10, 0xFF, 12, 0x10, 3, 0, 0];
        assert_eq!(decode(&r, 12), Some(LiveEvent::DpiIndexChanged(3)));
    }

    #[test]
    fn events_survive_packing() {
        for ev in [
            LiveEvent::ProfileChanged(1),
            LiveEvent::ProfileChanged(0xFFFF),
            LiveEvent::DpiIndexChanged(0),
            LiveEvent::DpiIndexChanged(4),
        ] {
            assert_eq!(unpack(pack(ev)), Some(ev));
        }
        assert_eq!(unpack(0), None, "an untouched pulse reports no event");
    }

    #[test]
    fn the_pulse_flags_once_per_burst_and_keeps_the_count() {
        let p = Pulse::default();
        assert!(!p.take_dirty());
        assert_eq!(p.last(), None);

        p.note(LiveEvent::DpiIndexChanged(1));
        p.note(LiveEvent::DpiIndexChanged(2));
        assert!(p.take_dirty());
        assert!(!p.take_dirty(), "reading the flag clears it");
        // The count is not cleared by reading: it is what the diagnostics show.
        assert_eq!(p.count(), 2);
        assert_eq!(p.last(), Some(LiveEvent::DpiIndexChanged(2)));
    }

    #[test]
    fn descriptions_name_the_profile_and_stage_people_see() {
        // Sector 1 is profile 0, and stages are counted from one on screen.
        assert_eq!(LiveEvent::ProfileChanged(1).describe(), "profile 0");
        assert_eq!(LiveEvent::ProfileChanged(5).describe(), "profile 4");
        assert_eq!(LiveEvent::DpiIndexChanged(0).describe(), "DPI stage 1");
    }
}
