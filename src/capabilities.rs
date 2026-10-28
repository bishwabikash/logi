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

/// The hidden (0x40), engineering (0x20) and manufacturing (0x10) bits of a
/// feature's type byte. See `features::type_flags`.
const RESTRICTED_BITS: u8 = 0x70;

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
        // A feature the firmware flags hidden, engineering or manufacturing is
        // one the vendor does not mean generic software to drive, and
        // PROTOCOL.md records that writing those can brick the device. So a
        // flagged feature is listed in `restricted` and goes no further: it
        // never becomes a control. Nothing is flagged on the tested G502, so
        // this is the arbitrary-device case, which is the whole premise here.
        c.dpi = c.usable(F::ADJUSTABLE_DPI);
        c.report_rate = c.usable(F::REPORT_RATE);
        c.color_leds = c.usable(F::COLOR_LED_EFFECTS);
        c.onboard_profiles = c.usable(F::ONBOARD_PROFILES);
        c.button_spy = c.usable(F::MOUSE_BUTTON_SPY);
        c.device_name = c.usable(F::DEVICE_NAME);
        c.firmware = c.usable(F::DEVICE_FW_VERSION);
        c
    }

    /// Whether the device reports `id` at all, flagged or not.
    pub fn supports(&self, id: u16) -> bool {
        self.features.iter().any(|(_, f, _)| *f == id)
    }

    /// Whether `id` is reported *and* unflagged, so this app may drive it.
    pub fn usable(&self, id: u16) -> bool {
        self.features
            .iter()
            .any(|(_, f, t)| *f == id && t & RESTRICTED_BITS == 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(entries: &[(u16, u8)]) -> Capabilities {
        let mut c = Capabilities {
            features: entries.iter().enumerate().map(|(i, (id, t))| (i as u8, *id, *t)).collect(),
            ..Default::default()
        };
        for (id, t) in entries {
            if t & RESTRICTED_BITS != 0 {
                c.restricted.push(*id);
            }
        }
        c
    }

    #[test]
    fn a_plain_feature_is_usable() {
        let c = caps(&[(F::ADJUSTABLE_DPI, 0x00)]);
        assert!(c.supports(F::ADJUSTABLE_DPI));
        assert!(c.usable(F::ADJUSTABLE_DPI));
    }

    #[test]
    fn a_flagged_feature_is_reported_but_never_usable() {
        // PROTOCOL.md: writing the engineering-flagged features can brick the
        // device. `info` still has to be able to say they exist.
        for flag in [0x10u8, 0x20, 0x40] {
            let c = caps(&[(F::MOUSE_BUTTON_SPY, flag)]);
            assert!(c.supports(F::MOUSE_BUTTON_SPY), "flag 0x{flag:02x}");
            assert!(!c.usable(F::MOUSE_BUTTON_SPY), "flag 0x{flag:02x} became a control");
            assert_eq!(c.restricted, vec![F::MOUSE_BUTTON_SPY]);
        }
    }

    #[test]
    fn obsolete_alone_does_not_restrict_a_feature() {
        // Bit 7 says superseded, not dangerous, and is outside the mask.
        let c = caps(&[(F::REPORT_RATE, 0x80)]);
        assert!(c.usable(F::REPORT_RATE));
    }

    #[test]
    fn a_feature_the_device_never_reported_is_neither() {
        let c = caps(&[(F::ADJUSTABLE_DPI, 0x00)]);
        assert!(!c.supports(F::COLOR_LED_EFFECTS));
        assert!(!c.usable(F::COLOR_LED_EFFECTS));
    }

    #[test]
    fn the_summary_names_only_what_is_set() {
        let mut c = Capabilities::default();
        assert_eq!(c.summary(), "no supported features");
        c.dpi = true;
        c.onboard_profiles = true;
        assert_eq!(c.summary(), "dpi, profiles");
    }
}
