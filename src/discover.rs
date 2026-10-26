//! Locate the HID++ control endpoint for a supported Logitech device.

use crate::hidpp::{Device, Error, Result};
use std::fs;
use std::path::PathBuf;

pub const VENDOR_LOGITECH: u16 = 0x046D;

/// Devices this project drives.
pub const SUPPORTED: &[(u16, &str)] = &[
    (0xC08B, "G502 HERO / SE HERO Gaming Mouse"),
    (0xC08D, "G502 LIGHTSPEED (wired mode)"),
    (0xC332, "G502 Proteus Spectrum"),
    (0xC07D, "G502 Proteus Core"),
];

pub fn model_name(pid: u16) -> Option<&'static str> {
    SUPPORTED.iter().find(|(p, _)| *p == pid).map(|(_, n)| *n)
}

/// Walk up from a hidraw node to the usb_device that carries idVendor/idProduct.
fn sysfs_ids(node: &str) -> Option<(u16, u16)> {
    let mut cur = fs::canonicalize(format!("/sys/class/hidraw/{node}/device")).ok()?;
    for _ in 0..6 {
        cur = cur.parent()?.to_path_buf();
        let v = cur.join("idVendor");
        let p = cur.join("idProduct");
        if v.exists() && p.exists() {
            let vs = fs::read_to_string(&v).ok()?;
            let ps = fs::read_to_string(&p).ok()?;
            return Some((
                u16::from_str_radix(vs.trim(), 16).ok()?,
                u16::from_str_radix(ps.trim(), 16).ok()?,
            ));
        }
    }
    None
}

pub struct Candidate {
    pub path: PathBuf,
    pub pid: u16,
}

/// Every hidraw node belonging to a supported Logitech device.
pub fn candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    let mut nodes: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    nodes.sort();
    for node in nodes {
        if let Some((vid, pid)) = sysfs_ids(&node) {
            if vid == VENDOR_LOGITECH && model_name(pid).is_some() {
                out.push(Candidate { path: PathBuf::from(format!("/dev/{node}")), pid });
            }
        }
    }
    out
}

pub struct Found {
    pub device: Device,
    pub pid: u16,
    pub model: &'static str,
}

/// Open the first hidraw node that actually answers HID++.
///
/// A G502 exposes two interfaces; only the one carrying the ff00 vendor
/// collection responds to the root feature, so probe rather than guess.
pub fn open_device() -> Result<Found> {
    let found = candidates();
    if found.is_empty() {
        let ids: Vec<String> = SUPPORTED.iter().map(|(p, _)| format!("046d:{p:04x}")).collect();
        return Err(Error::NotFound(format!(
            "no supported Logitech device found. Is it plugged in? (looked for {})",
            ids.join(", ")
        )));
    }
    let mut permission_problem = None;
    let mut errors = Vec::new();
    for c in found {
        match Device::open(&c.path) {
            Ok(mut dev) => {
                // Does it speak HID++ at all?
                match dev.protocol_version() {
                    Ok(_) => {
                        return Ok(Found {
                            device: dev,
                            pid: c.pid,
                            model: model_name(c.pid).unwrap_or("unknown"),
                        })
                    }
                    Err(e) => errors.push(format!("{}: {e}", c.path.display())),
                }
            }
            Err(Error::Permission(m)) => permission_problem = Some(m),
            Err(e) => errors.push(format!("{}: {e}", c.path.display())),
        }
    }
    // Permission is the overwhelmingly common cause and has a specific fix, so
    // report it in preference to a generic "nothing answered".
    if let Some(m) = permission_problem {
        return Err(Error::Permission(m));
    }
    Err(Error::Protocol(format!(
        "found the device but no HID++ endpoint answered:\n  {}",
        errors.join("\n  ")
    )))
}
