//! 0x0005 DEVICE_NAME and 0x0003 DEVICE_FW_VERSION.
//!
//! Lets the app name a device from the device itself rather than a hardcoded
//! table, which is what makes supporting an arbitrary Logitech mouse possible.

use crate::features;
use crate::hidpp::{Device, Error, Result};

const NAME_COUNT: u8 = 0x00;
const NAME_GET: u8 = 0x01;
const NAME_TYPE: u8 = 0x02;

const FW_COUNT: u8 = 0x00;
const FW_INFO: u8 = 0x01;

/// 0x0005 getDeviceType values.
pub fn device_type_name(t: u8) -> &'static str {
    match t {
        0 => "keyboard",
        1 => "remote control",
        2 => "numpad",
        3 => "mouse",
        4 => "touchpad",
        5 => "trackball",
        6 => "presenter",
        7 => "receiver",
        8 => "headset",
        9 => "webcam",
        10 => "steering wheel",
        11 => "joystick",
        12 => "gamepad",
        13 => "dock",
        14 => "speaker",
        15 => "microphone",
        16 => "illumination light",
        17 => "programmable controller",
        18 => "car simulator pedals",
        19 => "adapter",
        _ => "unknown",
    }
}

/// Read the device's own marketing name, e.g. "G502 HERO Gaming Mouse".
pub fn name(dev: &mut Device) -> Result<String> {
    let idx = dev.feature_index(features::DEVICE_NAME)?;
    let len = dev.call(idx, NAME_COUNT, &[])?.first().copied().unwrap_or(0) as usize;
    let mut out = String::new();
    let mut at = 0usize;
    // Returned 16 bytes at a time, ASCII, not NUL terminated.
    while at < len && out.len() < 128 {
        let chunk = dev.call(idx, NAME_GET, &[at as u8])?;
        let take = chunk.len().min(16).min(len - at);
        for &b in &chunk[..take] {
            if b.is_ascii_graphic() || b == b' ' {
                out.push(b as char);
            }
        }
        if take == 0 {
            break;
        }
        at += take;
    }
    Ok(out.trim().to_string())
}

pub fn device_type(dev: &mut Device) -> Result<u8> {
    let idx = dev.feature_index(features::DEVICE_NAME)?;
    Ok(dev.call(idx, NAME_TYPE, &[])?.first().copied().unwrap_or(0xFF))
}

#[derive(Debug, Clone)]
pub struct Firmware {
    pub kind: String,
    pub prefix: String,
    pub version: String,
    pub build: u16,
}

impl Firmware {
    /// "U1 27.03 (build 31)" for one entity.
    pub fn describe(&self) -> String {
        format!("{} {} (build {})", self.prefix, self.version, self.build)
    }
}

/// The firmware line shown for a device: the main entity, then any others
/// ("bootloader U2 ...") after it. Empty when the device reported none.
pub fn firmware_summary(list: &[Firmware]) -> String {
    let main = list.iter().find(|f| f.kind == "main");
    let rest = list.iter().filter(|f| f.kind != "main");
    main.map(|f| f.describe())
        .into_iter()
        .chain(rest.map(|f| format!("{} {}", f.kind, f.describe())))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn firmware(dev: &mut Device) -> Result<Vec<Firmware>> {
    let idx = dev.feature_index(features::DEVICE_FW_VERSION)?;
    let n = dev.call(idx, FW_COUNT, &[])?.first().copied().unwrap_or(0);
    let mut out = Vec::new();
    // `n` is the device's own claim, and an entity that does not answer costs a
    // full reply timeout. A vanished endpoint ends the walk at once, and a run
    // of silence ends it as `Capabilities::probe` does, instead of spending
    // minutes on a count of 255.
    let mut quiet = 0u8;
    for e in 0..n {
        let p = match dev.call(idx, FW_INFO, &[e]) {
            Ok(p) => p,
            Err(err @ Error::Io(_)) => return Err(err),
            Err(_) => {
                quiet += 1;
                if quiet >= 3 {
                    break;
                }
                continue;
            }
        };
        quiet = 0;
        if p.len() < 8 {
            continue;
        }
        let kind = match p[0] & 0x0F {
            0 => "main",
            1 => "bootloader",
            2 => "hardware",
            3 => "touchpad",
            _ => "other",
        };
        out.push(Firmware {
            kind: kind.into(),
            prefix: String::from_utf8_lossy(&p[1..4]).trim().to_string(),
            version: format!("{:02x}.{:02x}", p[4], p[5]),
            build: u16::from_be_bytes([p[6], p[7]]),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_firmware_line_leads_with_the_main_entity() {
        let f = |kind: &str, prefix: &str, version: &str, build| Firmware {
            kind: kind.into(), prefix: prefix.into(), version: version.into(), build,
        };
        let list = [f("bootloader", "BL", "01.02", 3), f("main", "U1", "27.03", 31)];
        assert_eq!(firmware_summary(&list), "U1 27.03 (build 31), bootloader BL 01.02 (build 3)");
        assert_eq!(firmware_summary(&[]), "");
    }
}
