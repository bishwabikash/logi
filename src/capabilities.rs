//! What a given device can actually do.
//!
//! Every feature here is discovered at runtime from the device's own feature
//! table, so an arbitrary Logitech mouse gets exactly the controls it supports
//! and nothing else. Nothing in this file is specific to one model.

use crate::features as F;
use crate::hidpp::Device;

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Every feature id the device reports, in table order.
    pub features: Vec<(u8, u16, u8)>,
    pub dpi: bool,
    pub report_rate: bool,
    pub color_leds: bool,
    pub onboard_profiles: bool,
    pub button_spy: bool,
    pub device_name: bool,
    pub firmware: bool,
    /// Features flagged hidden/engineering/manufacturing, which this app does
    /// not touch. Recorded so `info` can show they exist.
    pub restricted: Vec<u16>,
}

impl Capabilities {
    pub fn probe(dev: &mut Device) -> Self {
        let mut c = Capabilities::default();
        if let Ok(fs) = dev.feature_index(F::FEATURE_SET) {
            if let Ok(p) = dev.call(fs, 0x00, &[]) {
                // getCount excludes the root feature, so the valid indices
                // are 0..=count. A device that answers nothing for an index
                // costs a full reply timeout each time, and `count` is the
                // device's own claim: give up after a short run of silence
                // rather than spending minutes on a count of 255.
                let count = p.first().copied().unwrap_or(0);
                let mut quiet = 0u8;
                for i in 0..=count {
                    match dev.call(fs, 0x01, &[i]) {
                        Ok(q) if q.len() >= 3 => {
                            quiet = 0;
                            let id = u16::from_be_bytes([q[0], q[1]]);
                            c.features.push((i, id, q[2]));
                            if q[2] & 0x70 != 0 {
                                c.restricted.push(id);
                            }
                        }
                        _ => {
                            quiet += 1;
                            if quiet >= 3 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        let has = |c: &Capabilities, id: u16| c.features.iter().any(|(_, f, _)| *f == id);
        c.dpi = has(&c, F::ADJUSTABLE_DPI);
        c.report_rate = has(&c, F::REPORT_RATE);
        c.color_leds = has(&c, F::COLOR_LED_EFFECTS);
        c.onboard_profiles = has(&c, F::ONBOARD_PROFILES);
        c.button_spy = has(&c, F::MOUSE_BUTTON_SPY);
        c.device_name = has(&c, F::DEVICE_NAME);
        c.firmware = has(&c, F::DEVICE_FW_VERSION);
        c
    }

    pub fn supports(&self, id: u16) -> bool {
        self.features.iter().any(|(_, f, _)| *f == id)
    }

    /// A one-line summary for listings.
    pub fn summary(&self) -> String {
        let mut v = Vec::new();
        if self.dpi { v.push("dpi") }
        if self.report_rate { v.push("rate") }
        if self.color_leds { v.push("rgb") }
        if self.onboard_profiles { v.push("profiles") }
        if self.button_spy { v.push("buttons") }
        if v.is_empty() { "no supported features".into() } else { v.join(", ") }
    }
}
