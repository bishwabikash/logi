//! 0x0005 DEVICE_NAME and 0x0003 DEVICE_FW_VERSION.
//!
//! Lets the app name a device from the device itself rather than a hardcoded
//! table, which is what makes supporting an arbitrary Logitech mouse possible.

use crate::features;
use crate::hidpp::{Device, Result};

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

pub fn firmware(dev: &mut Device) -> Result<Vec<Firmware>> {
    let idx = dev.feature_index(features::DEVICE_FW_VERSION)?;
    let n = dev.call(idx, FW_COUNT, &[])?.first().copied().unwrap_or(0);
    let mut out = Vec::new();
    for e in 0..n {
        let Ok(p) = dev.call(idx, FW_INFO, &[e]) else { continue };
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
