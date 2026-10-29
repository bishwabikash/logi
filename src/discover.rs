//! Find Logitech devices that speak HID++ 2.0.
//!
//! Deliberately not a model whitelist. Any Logitech device whose hidraw node
//! answers the root feature is usable, and what it can actually do is then
//! read from its own feature table. A device nobody has ever tested gets the
//! controls it reports and nothing else.

use crate::capabilities::Capabilities;
use crate::deviceinfo;
use crate::hidpp::{Device, Error, Result};
use std::fs;
use std::path::PathBuf;

pub const VENDOR_LOGITECH: u16 = 0x046D;

/// Names for devices whose DEVICE_NAME feature is missing or unreadable.
/// Only a fallback: the device's own name is preferred.
pub fn fallback_name(pid: u16) -> &'static str {
    match pid {
        0xC08B => "G502 HERO / SE HERO",
        0xC08D => "G502 LIGHTSPEED (wired)",
        0xC332 => "G502 Proteus Spectrum",
        0xC07D => "G502 Proteus Core",
        0xC539 | 0xC53A | 0xC541 | 0xC545 => "LIGHTSPEED receiver",
        0xC52B | 0xC532 | 0xC534 => "Unifying receiver",
        _ => "Logitech HID++ device",
    }
}

fn read_hex(path: &std::path::Path) -> Option<u16> {
    u16::from_str_radix(fs::read_to_string(path).ok()?.trim(), 16).ok()
}

/// Walk up from a hidraw node to the usb_device carrying idVendor/idProduct.
fn sysfs_ids(node: &str) -> Option<(u16, u16)> {
    let mut cur = fs::canonicalize(format!("/sys/class/hidraw/{node}/device")).ok()?;
    for _ in 0..6 {
        cur = cur.parent()?.to_path_buf();
        let v = cur.join("idVendor");
        let p = cur.join("idProduct");
        if v.exists() && p.exists() {
            return Some((read_hex(&v)?, read_hex(&p)?));
        }
    }
    None
}

/// A device that answered HID++, with what it says about itself.
pub struct Found {
    pub device: Device,
    pub pid: u16,
    pub path: PathBuf,
    pub name: String,
    pub kind: String,
    pub protocol: (u8, u8),
    pub caps: Capabilities,
}

impl Found {
    pub fn id(&self) -> String {
        format!("046d:{:04x}", self.pid)
    }
}

/// Every Logitech hidraw node on the system, before probing.
fn candidates() -> Vec<(PathBuf, u16)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    let mut nodes: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    nodes.sort_by_key(|n| n.trim_start_matches("hidraw").parse::<u32>().unwrap_or(u32::MAX));
    for node in nodes {
        if let Some((vid, pid)) = sysfs_ids(&node) {
            if vid == VENDOR_LOGITECH {
                out.push((PathBuf::from(format!("/dev/{node}")), pid));
            }
        }
    }
    out
}

fn interrogate(mut dev: Device, pid: u16, path: PathBuf) -> Option<Found> {
    let protocol = dev.protocol_version().ok()?;
    let caps = Capabilities::probe(&mut dev);
    // A node that answers the root feature but exposes nothing is a sibling
    // interface of a device we already have; skip it rather than listing a
    // device the user cannot do anything with.
    if caps.features.len() <= 2 {
        return None;
    }
    let name = deviceinfo::name(&mut dev)
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name(pid).to_string());
    let kind = deviceinfo::device_type(&mut dev)
        .map(deviceinfo::device_type_name)
        .unwrap_or("unknown")
        .to_string();
    Some(Found { device: dev, pid, path, name, kind, protocol, caps })
}

/// Probe every Logitech node and return each distinct device that answers.
///
/// A mouse exposes several hidraw nodes; only the one carrying the vendor
/// collection replies, so probing rather than guessing is what makes this
/// work across models.
pub fn list_devices() -> Vec<Found> {
    let mut found = Vec::new();
    for (path, pid) in candidates() {
        // One entry per physical device, not per interface. A device already
        // found is not opened again through its sibling nodes: each one that
        // does not answer costs a full reply timeout, and each is a write to
        // an interface that was never meant to receive HID++.
        if found.iter().any(|e: &Found| e.pid == pid) {
            continue;
        }
        let Ok(dev) = Device::open(&path) else { continue };
        if let Some(f) = interrogate(dev, pid, path) {
            found.push(f);
        }
    }
    found
}

/// Strip `prefix` from the front of `s`, ignoring case. `s` is returned
/// unchanged when it does not start with it.
fn strip_ci<'a>(s: &'a str, prefix: &str) -> &'a str {
    match s.get(..prefix.len()) {
        Some(head) if head.eq_ignore_ascii_case(prefix) => &s[prefix.len()..],
        _ => s,
    }
}

/// Reduce a device id to the bare four hex digits of the product id.
///
/// The vendor prefix and an `0x` are both optional, and neither case nor
/// surrounding whitespace is significant. The comparison against the device's
/// own id is case-insensitive, so the prefix has to be matched the same way:
/// `Found::id` hands out the lowercase form, but someone typing an id back
/// into `$G502_DEVICE` has no reason to know which case it was in.
fn wanted_pid(want: &str) -> &str {
    strip_ci(strip_ci(want.trim(), "046d:"), "0x")
}

/// Open one device: the only one present, or the one whose id matches `want`
/// (e.g. "046d:c08b").
pub fn open_device_matching(want: Option<&str>) -> Result<Found> {
    let cands = candidates();
    if cands.is_empty() {
        return Err(Error::NotFound(
            "no Logitech device found. Is it plugged in?".into(),
        ));
    }
    // Reported if nothing answers. Taken from sysfs, not from a probe, so it
    // is true even for the nodes this call never opens.
    let mut present: Vec<String> = Vec::new();
    for (_, pid) in &cands {
        let id = format!("046d:{pid:04x}");
        if !present.contains(&id) {
            present.push(id);
        }
    }
    let mut permission_problem = None;
    let target = want.map(wanted_pid);

    for (path, pid) in cands {
        if let Some(w) = target {
            if !format!("{pid:04x}").eq_ignore_ascii_case(w) {
                continue;
            }
        }
        match Device::open(&path) {
            Ok(dev) => {
                // Stop at the first node that answers. The siblings of a
                // device we already have add nothing, and every node that
                // does not answer costs a full reply timeout.
                if let Some(f) = interrogate(dev, pid, path) {
                    return Ok(f);
                }
            }
            Err(Error::Permission(m)) => permission_problem = Some(m),
            Err(_) => {}
        }
    }

    if let Some(m) = permission_problem {
        return Err(Error::Permission(m));
    }
    match want {
        Some(w) => Err(Error::NotFound(format!(
            "no Logitech device matching {w:?} answered HID++. Present: {}",
            present.join(", ")
        ))),
        None => Err(Error::NotFound(
            "found Logitech hardware, but no HID++ endpoint answered. A plain \
             mouse with no configurable features will not appear here."
                .into(),
        )),
    }
}

/// The id in `$G502_DEVICE`, if one is set and not blank.
fn device_from_env(var: Option<String>) -> Option<String> {
    var.filter(|s| !s.trim().is_empty())
}

/// Open the device the user named in `$G502_DEVICE`, or the only one.
///
/// The CLI commands honour that variable, and so must everything that opens a
/// device without being told which: with two Logitech devices attached, an
/// effect or the GUI worker that ignored it would put the *other* device into
/// host mode and light that one instead.
pub fn open_device() -> Result<Found> {
    open_device_matching(device_from_env(std::env::var("G502_DEVICE").ok()).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_id_is_matched_whatever_case_it_is_typed_in() {
        // `Found::id` hands out the lowercase form, but the id also appears in
        // lsusb output and in the user's own notes. Every spelling of the same
        // device has to reduce to the same product id.
        for w in ["046d:c08b", "046D:C08B", "046d:C08B", "c08b", "C08B", "0xc08b", " 046d:c08b "]
        {
            assert!(
                wanted_pid(w).eq_ignore_ascii_case("c08b"),
                "{w:?} did not reduce to the product id, so it would match no device"
            );
        }
    }

    #[test]
    fn a_blank_device_variable_means_no_choice() {
        assert_eq!(device_from_env(None), None);
        assert_eq!(device_from_env(Some(String::new())), None);
        assert_eq!(device_from_env(Some("  \t".into())), None);
        assert_eq!(device_from_env(Some("046d:c08b".into())).as_deref(), Some("046d:c08b"));
    }

    #[test]
    fn a_non_logitech_prefix_is_left_alone() {
        // Only 046d is ever discovered, so another vendor's prefix must not be
        // silently stripped and read as a product id.
        assert_eq!(wanted_pid("1532:c08b"), "1532:c08b");
    }

    #[test]
    fn stripping_a_prefix_does_not_split_a_character() {
        // The id comes from the environment, so it can be arbitrary text.
        assert_eq!(wanted_pid("\u{00e9}\u{00e9}\u{00e9}"), "\u{00e9}\u{00e9}\u{00e9}");
        assert_eq!(wanted_pid("0\u{00e9}"), "0\u{00e9}");
    }

    #[test]
    fn fallback_names_are_only_a_last_resort() {
        // An untested device must still get a usable label rather than an
        // empty string, and no model entry may claim to be the generic one.
        assert_eq!(fallback_name(0x0000), "Logitech HID++ device");
        assert_ne!(fallback_name(0xC08B), fallback_name(0x0000));
    }
}
