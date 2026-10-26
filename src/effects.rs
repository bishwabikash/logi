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

use crate::led::Leds;
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

/// Open the device and run `name` until stopped.
pub fn start(name: String, color: [u8; 3], period_ms: u32) -> Result<Running, String> {
    if !is_effect(&name) {
        return Err(format!("unknown effect {name:?}"));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("g502-effect".into())
        .spawn(move || {
            if let Err(e) = run(&name, color, period_ms, &flag) {
                eprintln!("g502: effect stopped: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Running { stop, handle: Some(handle) })
}

fn run(name: &str, color: [u8; 3], period_ms: u32, stop: &AtomicBool) -> hidpp::Result<()> {
    let found = discover::open_device()?;
    let mut dev = found.device;
    let leds = Leds::new(&mut dev)?;
    let profiles = OnboardProfiles::new(&mut dev)?;

    let previous = profiles.mode(&mut dev).unwrap_or(MODE_ONBOARD);
    profiles.set_mode(&mut dev, MODE_HOST)?;

    let period_s = period_ms as f32 / 1000.0;
    let interval = Duration::from_micros(1_000_000 / FPS as u64);
    let t0 = Instant::now();
    let mut frames: u64 = 0;

    let outcome = (|| -> hidpp::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let t = t0.elapsed().as_secs_f32();
            let colors = frame(name, t, color, period_s, leds.zone_count as usize);
            for (z, c) in colors.iter().enumerate() {
                leds.fixed_nowait(&mut dev, z as u8, *c)?;
            }
            frames += 1;
            // send_nowait leaves replies queued; drain periodically or they
            // accumulate in the kernel buffer.
            if frames % 32 == 0 {
                dev.drain();
            }
            std::thread::sleep(interval);
        }
        Ok(())
    })();

    // Always hand the mouse back, even if the loop failed.
    let restore = if previous == MODE_HOST { MODE_HOST } else { MODE_ONBOARD };
    let _ = profiles.set_mode(&mut dev, restore);
    outcome
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

    #[test]
    fn unknown_effect_is_rejected() {
        assert!(!is_effect("nonsense"));
        assert!(is_effect("rainbow"));
    }
}
