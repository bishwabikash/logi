//! Host-side animation.
//!
//! The firmware implements only off/fixed/breathing/cycle and has no frame
//! streaming mode (CUSTOM, 0x0c, is absent on this device), so anything richer
//! has to be a loop of fixed-colour writes from here. That means an effect
//! lives only as long as this process: it is not stored on the mouse.
//!
//! The device is held in host mode for the duration and put back on exit,
//! including on error, because otherwise its lighting would stay dependent on
//! a process that is no longer running.

use crate::capabilities::Capabilities;
use crate::features;
use crate::led::{self, Leds};
use crate::profiles::{OnboardProfiles, MODE_HOST, MODE_ONBOARD};
use crate::{discover, hidpp};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const EFFECTS: &[(&str, &str)] = &[
    ("static", "one unchanging colour"),
    ("breathe", "smooth host-side fade, independent of the hardware waveform"),
    ("rainbow", "hue sweep through the full wheel"),
    ("pulse", "sharp attack, slow decay"),
    ("wave", "hue sweep with the two zones phase-offset"),
];

pub fn is_effect(name: &str) -> bool {
    EFFECTS.iter().any(|(n, _)| *n == name)
}

/// Target frame rate. Blocking writes measured ~19 fps; fire-and-forget is
/// far quicker, but there is no point outrunning what the eye resolves on two
/// LEDs, and every frame is still two USB transfers.
const FPS: u32 = 30;

pub struct Running {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Running {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// hue in [0,1) -> rgb, full saturation and value.
fn hue(h: f32) -> [u8; 3] {
    let h = (h.fract() + 1.0).fract() * 6.0;
    let i = h.floor() as i32;
    let f = h - h.floor();
    let (r, g, b) = match i {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

fn scale(c: [u8; 3], k: f32) -> [u8; 3] {
    let k = k.clamp(0.0, 1.0);
    [
        (c[0] as f32 * k) as u8,
        (c[1] as f32 * k) as u8,
        (c[2] as f32 * k) as u8,
    ]
}

/// One frame: the colour each zone should show at time `t` seconds.
pub fn frame(name: &str, t: f32, color: [u8; 3], period_s: f32, zones: usize) -> Vec<[u8; 3]> {
    let p = if period_s <= 0.0 { 1.0 } else { period_s };
    let phase = t / p;
    (0..zones)
        .map(|z| match name {
            "static" => color,
            "breathe" => {
                // raised cosine: 0 -> 1 -> 0, never fully dark
                let k = 0.15 + 0.85 * (0.5 - 0.5 * (phase * std::f32::consts::TAU).cos());
                scale(color, k)
            }
            "rainbow" => hue(phase),
            "pulse" => {
                let x = phase.fract();
                // fast attack, exponential decay
                let k = if x < 0.1 { x / 0.1 } else { (-(x - 0.1) * 4.0).exp() };
                scale(color, 0.1 + 0.9 * k)
            }
            "wave" => hue(phase + z as f32 / zones.max(1) as f32 * 0.5),
            _ => color,
        })
        .collect()
}

/// Refuse a feature the firmware flags hidden, engineering or manufacturing.
/// One the device never reported is left to the lookup that follows, which
/// names it.
fn require_drivable(caps: &Capabilities, id: u16) -> hidpp::Result<()> {
    if caps.supports(id) && !caps.usable(id) {
        return Err(hidpp::Error::Protocol(format!(
            "this device flags feature 0x{id:04x} as hidden or engineering, which \
             this app does not drive"
        )));
    }
    Ok(())
}

/// The device, held in host mode for as long as an effect is running.
///
/// Opening it, taking host mode and handing the mouse back all live here so
/// there is exactly one exit path. Dropping a `Session` restores the previous
/// mode, so a thread that never started, returned early or unwound still gives
/// the mouse back instead of leaving its lighting owned by a dead process.
struct Session {
    dev: hidpp::Device,
    leds: Leds,
    profiles: OnboardProfiles,
    /// The mode the device was in before we touched it.
    restore: u8,
    released: bool,
}

impl Session {
    fn open() -> hidpp::Result<Self> {
        let found = discover::open_device()?;
        // An effect drives 0x8070 and flips 0x8100's mode, so both are held to
        // the same rule every other control is: a feature the firmware flags
        // hidden, engineering or manufacturing is listed and never driven.
        require_drivable(&found.caps, features::COLOR_LED_EFFECTS)?;
        require_drivable(&found.caps, features::ONBOARD_PROFILES)?;
        let mut dev = found.device;
        let leds = Leds::new(&mut dev)?;
        if leds.zone_count == 0 {
            return Err(hidpp::Error::Protocol(
                "this device reports no lighting zones, so there is nothing to animate".into(),
            ));
        }
        // Every frame is a FIXED write to every zone. A zone whose firmware has
        // no fixed mode fails on the first one, and by then host mode has been
        // taken and the caller is holding a handle for an effect that will never
        // draw anything. Rule it out here, before the device is touched at all.
        if let Some(z) = (0..leds.zone_count).find(|z| !leds.supports(*z, led::FIXED)) {
            return Err(hidpp::Error::Protocol(format!(
                "zone {z} of this device has no fixed-colour mode, so a host-driven \
                 effect cannot drive its lighting"
            )));
        }
        let profiles = OnboardProfiles::new(&mut dev)?;

        let previous = profiles.mode(&mut dev).unwrap_or(MODE_ONBOARD);
        let restore = if previous == MODE_HOST { MODE_HOST } else { MODE_ONBOARD };
        let mut me = Session { dev, leds, profiles, restore, released: false };
        if previous != MODE_HOST {
            if let Err(e) = me.profiles.set_mode(&mut me.dev, MODE_HOST) {
                // The switch may have landed even though the reply did not, so
                // put the device back before giving up. Reporting the failure
                // and leaving the mouse in host mode is the one outcome worth
                // ruling out: nothing would ever switch it back.
                me.release();
                return Err(e);
            }
        }
        Ok(me)
    }

    /// Hand the mouse back. Idempotent, so Drop after an explicit call is free.
    ///
    /// A mouse left in host mode keeps whatever colour was last pushed and
    /// ignores its stored profile until something switches it back, so one
    /// timed-out reply is not allowed to be the end of it. A setter that timed
    /// out may well have landed, so the mode is read back before trying again.
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        for _ in 0..2 {
            match self.profiles.set_mode(&mut self.dev, self.restore) {
                Ok(()) => return,
                // The endpoint is gone; another try is against nothing.
                Err(hidpp::Error::Io(_)) => return,
                Err(_) => {
                    if matches!(self.profiles.mode(&mut self.dev), Ok(m) if m == self.restore) {
                        return;
                    }
                }
            }
        }
    }

    fn run(
        &mut self,
        name: &str,
        color: [u8; 3],
        period_ms: u32,
        stop: &AtomicBool,
    ) -> hidpp::Result<()> {
        let period_s = period_ms as f32 / 1000.0;
        let interval = Duration::from_micros(1_000_000 / FPS as u64);
        let zones = self.leds.zone_count as usize;
        let t0 = Instant::now();
        let mut frames: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            let t = t0.elapsed().as_secs_f32();
            let colors = frame(name, t, color, period_s, zones);
            for (z, c) in colors.iter().enumerate() {
                self.leds.fixed_nowait(&mut self.dev, z as u8, *c)?;
            }
            frames += 1;
            // send_nowait leaves replies queued; drain periodically or they
            // accumulate in the kernel buffer.
            if frames % 32 == 0 {
                self.dev.drain();
            }
            std::thread::sleep(interval);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.release();
    }
}

/// Open the device and run `name` until stopped.
///
/// The device is opened and switched to host mode here rather than on the
/// effect thread, so a device that is missing, busy or has no lighting is an
/// error the caller sees instead of a `Running` handle for an effect that
/// never started.
pub fn start(name: String, color: [u8; 3], period_ms: u32) -> Result<Running, String> {
    if !is_effect(&name) {
        return Err(format!("unknown effect {name:?}"));
    }
    let mut session = Session::open().map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("logi-effect".into())
        .spawn(move || {
            if let Err(e) = session.run(&name, color, period_ms, &flag) {
                eprintln!("logi: effect stopped: {e}");
            }
            // Always hand the mouse back, even if the loop failed. Dropping
            // the session would do this too; doing it here keeps the restore
            // visible at the exit it belongs to.
            session.release();
        })
        // The thread never started, so `session` is dropped here and its Drop
        // is what restores the mode.
        .map_err(|e| e.to_string())?;
    Ok(Running { stop, handle: Some(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_wheel_hits_primaries() {
        assert_eq!(hue(0.0), [255, 0, 0]);
        assert_eq!(hue(1.0 / 3.0)[1], 255);
        assert_eq!(hue(2.0 / 3.0)[2], 255);
    }

    #[test]
    fn hue_wraps() {
        assert_eq!(hue(0.0), hue(1.0));
        assert_eq!(hue(0.25), hue(1.25));
    }

    #[test]
    fn hue_handles_negative_input() {
        // t can go negative if a caller offsets phase; must not panic.
        let _ = hue(-0.3);
        assert_eq!(hue(-1.0), hue(0.0));
    }

    #[test]
    fn static_effect_is_constant() {
        let c = [10, 20, 30];
        for t in [0.0, 0.5, 7.3] {
            assert_eq!(frame("static", t, c, 2.0, 2), vec![c, c]);
        }
    }

    #[test]
    fn breathe_never_goes_fully_dark() {
        for i in 0..100 {
            let t = i as f32 * 0.05;
            for c in frame("breathe", t, [255, 255, 255], 2.0, 1) {
                assert!(c[0] > 0, "breathe went black at t={t}");
            }
        }
    }

    #[test]
    fn breathe_spans_most_of_the_range() {
        let vals: Vec<u8> = (0..100)
            .map(|i| frame("breathe", i as f32 * 0.02, [255, 255, 255], 2.0, 1)[0][0])
            .collect();
        assert!(*vals.iter().max().unwrap() > 240);
        assert!(*vals.iter().min().unwrap() < 60);
    }

    #[test]
    fn frame_returns_one_colour_per_zone() {
        assert_eq!(frame("rainbow", 1.0, [0; 3], 2.0, 2).len(), 2);
        assert_eq!(frame("rainbow", 1.0, [0; 3], 2.0, 1).len(), 1);
    }

    #[test]
    fn wave_offsets_the_second_zone() {
        let f = frame("wave", 0.0, [0; 3], 2.0, 2);
        assert_ne!(f[0], f[1]);
    }

    #[test]
    fn zero_period_does_not_divide_by_zero() {
        let f = frame("breathe", 1.0, [255; 3], 0.0, 1);
        assert_eq!(f.len(), 1);
    }

    fn caps(entries: &[(u16, u8)]) -> Capabilities {
        Capabilities {
            features: entries.iter().enumerate().map(|(i, (id, t))| (i as u8, *id, *t)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_flagged_lighting_or_mode_feature_is_not_driven() {
        for id in [features::COLOR_LED_EFFECTS, features::ONBOARD_PROFILES] {
            for flag in [0x10u8, 0x20, 0x40] {
                assert!(
                    require_drivable(&caps(&[(id, flag)]), id).is_err(),
                    "0x{id:04x} flagged 0x{flag:02x} was driven"
                );
            }
        }
    }

    #[test]
    fn an_unflagged_or_unreported_feature_is_left_to_the_lookup() {
        let id = features::COLOR_LED_EFFECTS;
        assert!(require_drivable(&caps(&[(id, 0x00)]), id).is_ok());
        assert!(require_drivable(&caps(&[(id, 0x80)]), id).is_ok());
        assert!(require_drivable(&caps(&[]), id).is_ok());
    }

    #[test]
    fn unknown_effect_is_rejected() {
        assert!(!is_effect("nonsense"));
        assert!(is_effect("rainbow"));
    }
}
