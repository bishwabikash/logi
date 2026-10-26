//! Onboard-profile flash writes (0x8100 memory functions).
//!
//! The sequence, confirmed against libratbag's
//! hidpp20_onboard_profiles_write_sector:
//!
//!   * MEMORY_ADDR_WRITE (6), long report, params `be16 sector | be16
//!     sub_address | be16 count`, opening a write window.
//!   * MEMORY_WRITE (7), long report, exactly 16 payload bytes and nothing
//!     else. The window carries the address, so chunks must go in order with
//!     no gaps.
//!   * MEMORY_WRITE_END (8), **short** report, no params. Unlike every other
//!     call here.
//!
//! These are persistent flash writes with finite endurance. Do not call them
//! in a loop; stream colours with the effects engine instead.

use crate::profile_blob::{self, Profile, SECTOR_SIZE};
use crate::profiles::{OnboardProfiles, MEMORY_ADDR_WRITE, MEMORY_WRITE, MEMORY_WRITE_END};
use crate::hidpp::{Device, Error, Result};
use std::time::Duration;

const CHUNK: usize = 16;
pub const DIRECTORY_SECTOR: u16 = 0;
/// Give the flash a moment before reading back.
const SETTLE: Duration = Duration::from_millis(120);

/// Formats this codec understands. Refuse anything else rather than write
/// a layout we cannot predict.
const KNOWN_PROFILE_FORMAT: u8 = 2;
const KNOWN_MEMORY_MODEL: u8 = 1;

fn check_writable(p: &OnboardProfiles) -> Result<()> {
    if p.info.profile_format_id != KNOWN_PROFILE_FORMAT {
        return Err(Error::Protocol(format!(
            "unsupported profile_format_id {} (this codec handles {})",
            p.info.profile_format_id, KNOWN_PROFILE_FORMAT
        )));
    }
    if p.info.memory_model_id != KNOWN_MEMORY_MODEL {
        return Err(Error::Protocol(format!(
            "unsupported memory_model_id {}",
            p.info.memory_model_id
        )));
    }
    if p.info.sector_size as usize != SECTOR_SIZE {
        return Err(Error::Protocol(format!(
            "device reports {}-byte sectors, this codec assumes {}",
            p.info.sector_size, SECTOR_SIZE
        )));
    }
    Ok(())
}

/// Stamp a correct CRC over the payload, ignoring whatever the caller left in
/// the last two bytes.
pub fn stamp_crc(data: &[u8]) -> Result<[u8; SECTOR_SIZE]> {
    if data.len() != SECTOR_SIZE {
        return Err(Error::Protocol(format!(
            "sector must be {SECTOR_SIZE} bytes, got {}",
            data.len()
        )));
    }
    let mut out = [0u8; SECTOR_SIZE];
    out.copy_from_slice(data);
    let crc = profile_blob::crc_ccitt(&out[..profile_blob::OFF_CRC]);
    out[profile_blob::OFF_CRC..].copy_from_slice(&crc.to_be_bytes());
    Ok(out)
}

/// Write one sector and verify the readback. Returns the bytes written.
pub fn write_sector(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    sector: u16,
    data: &[u8],
) -> Result<[u8; SECTOR_SIZE]> {
    check_writable(profiles)?;
    if sector >= profiles.info.sector_count as u16 {
        return Err(Error::Protocol(format!(
            "sector {sector} out of range 0..{}",
            profiles.info.sector_count - 1
        )));
    }
    let payload = stamp_crc(data)?;
    let idx = profiles.feature_index();

    let n = payload.len() as u16;
    let mut head = Vec::with_capacity(6);
    head.extend_from_slice(&sector.to_be_bytes());
    head.extend_from_slice(&0u16.to_be_bytes());
    head.extend_from_slice(&n.to_be_bytes());
    dev.call(idx, MEMORY_ADDR_WRITE, &head)?;

    for chunk in payload.chunks(CHUNK) {
        dev.call(idx, MEMORY_WRITE, chunk)?;
    }
    // Short report, matching libratbag's write_end.
    dev.call_short(idx, MEMORY_WRITE_END, &[])?;

    std::thread::sleep(SETTLE);
    let back = profiles.read_sector(dev, sector, None)?;
    if back != payload {
        let at = back
            .iter()
            .zip(payload.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        return Err(Error::Protocol(format!(
            "sector {sector} readback differs at offset 0x{at:02x}: wrote 0x{:02x}, read 0x{:02x}",
            payload[at],
            back.get(at).copied().unwrap_or(0)
        )));
    }
    Ok(payload)
}

/// Profile N lives in sector N+1; sector 0 is the directory.
pub fn write_profile(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    index: u8,
    profile: &Profile,
) -> Result<[u8; SECTOR_SIZE]> {
    if index >= profiles.info.profile_count {
        return Err(Error::Protocol(format!(
            "profile {index} out of range 0..{}",
            profiles.info.profile_count - 1
        )));
    }
    write_sector(dev, profiles, index as u16 + 1, &profile.serialize())
}

/// Every readable sector, for a backup. Unreadable sectors are recorded as
/// absent rather than aborting the dump.
pub fn dump_all(dev: &mut Device, profiles: &OnboardProfiles) -> Vec<(u16, Option<Vec<u8>>)> {
    (0..profiles.info.sector_count as u16)
        .map(|s| (s, profiles.read_sector(dev, s, None).ok()))
        .collect()
}

/// Serialise a dump as JSON without pulling in a serde dependency.
pub fn dump_to_json(model: &str, dump: &[(u16, Option<Vec<u8>>)]) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"model\": \"{model}\",\n"));
    s.push_str("  \"sectors\": {\n");
    let entries: Vec<String> = dump
        .iter()
        .map(|(n, d)| match d {
            Some(bytes) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                format!("    \"{n}\": \"{hex}\"")
            }
            None => format!("    \"{n}\": null"),
        })
        .collect();
    s.push_str(&entries.join(",\n"));
    s.push_str("\n  }\n}\n");
    s
}

/// Parse a dump written by `dump_to_json`.
pub fn dump_from_json(text: &str) -> Result<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    // Deliberately minimal: the only producer is dump_to_json, and pulling in
    // a JSON crate for one flat object of hex strings is not worth it.
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('"') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let key = k.trim().trim_matches('"');
        let val = v.trim();
        let Ok(sector) = key.parse::<u16>() else { continue };
        if val == "null" {
            continue;
        }
        let hex = val.trim_matches('"');
        if hex.len() % 2 != 0 {
            return Err(Error::Protocol(format!("sector {sector}: odd hex length")));
        }
        let bytes: Result<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&hex[i..i + 2], 16)
                    .map_err(|_| Error::Protocol(format!("sector {sector}: bad hex")))
            })
            .collect();
        out.push((sector, bytes?));
    }
    if out.is_empty() {
        return Err(Error::Protocol("backup contains no sectors".into()));
    }
    Ok(out)
}

/// Push a backup back, skipping sectors that already match.
pub fn restore_all(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    backup: &[(u16, Vec<u8>)],
) -> Result<usize> {
    check_writable(profiles)?;
    for (sector, data) in backup {
        if data.len() != SECTOR_SIZE {
            return Err(Error::Protocol(format!(
                "sector {sector} in backup is {} bytes, expected {SECTOR_SIZE}",
                data.len()
            )));
        }
        if *sector >= profiles.info.sector_count as u16 {
            return Err(Error::Protocol(format!(
                "backup names sector {sector}, device has {}",
                profiles.info.sector_count
            )));
        }
    }
    let mut written = 0;
    // Profiles first, directory last: a half-restored device with an intact
    // old directory is recoverable, the reverse is not.
    let mut order: Vec<&(u16, Vec<u8>)> = backup.iter().collect();
    order.sort_by_key(|(s, _)| if *s == DIRECTORY_SECTOR { u16::MAX } else { *s });
    for (sector, data) in order {
        if profiles.read_sector(dev, *sector, None).ok().as_deref() == Some(data.as_slice()) {
            continue;
        }
        write_sector(dev, profiles, *sector, data)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_crc_makes_the_sector_self_consistent() {
        let out = stamp_crc(&[0u8; SECTOR_SIZE]).unwrap();
        assert_eq!(
            u16::from_be_bytes([out[profile_blob::OFF_CRC], out[profile_blob::OFF_CRC + 1]]),
            profile_blob::crc_ccitt(&out[..profile_blob::OFF_CRC])
        );
    }

    #[test]
    fn stamp_crc_overwrites_a_wrong_caller_crc() {
        let mut d = [0u8; SECTOR_SIZE];
        d[profile_blob::OFF_CRC] = 0xDE;
        d[profile_blob::OFF_CRC + 1] = 0xAD;
        let out = stamp_crc(&d).unwrap();
        assert_ne!(&out[profile_blob::OFF_CRC..], &[0xDE, 0xAD]);
    }

    #[test]
    fn stamp_crc_rejects_a_wrong_size() {
        assert!(stamp_crc(&[0u8; 10]).is_err());
    }

    #[test]
    fn json_dump_round_trips() {
        let dump = vec![
            (0u16, Some(vec![0xAAu8; SECTOR_SIZE])),
            (1u16, Some(vec![0x55u8; SECTOR_SIZE])),
            (2u16, None),
        ];
        let text = dump_to_json("test", &dump);
        let back = dump_from_json(&text).unwrap();
        // The unreadable sector is dropped, not invented.
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].0, 0);
        assert_eq!(back[0].1, vec![0xAA; SECTOR_SIZE]);
        assert_eq!(back[1].1, vec![0x55; SECTOR_SIZE]);
    }

    #[test]
    fn json_parse_rejects_empty() {
        assert!(dump_from_json("{}").is_err());
    }

    #[test]
    fn json_parse_rejects_odd_hex() {
        assert!(dump_from_json("{\n\"0\": \"abc\"\n}").is_err());
    }
}

/// Store a report rate into a profile's flash sector.
///
/// The live 0x8060 setter only works in host mode and is lost when the device
/// returns to onboard mode, so this is the path that actually makes a rate
/// stick. Byte 0x00 of the sector is the interval in milliseconds.
pub fn set_report_rate(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    index: u8,
    hz: u16,
) -> Result<u16> {
    let ms = crate::rate::ReportRate::interval_ms(hz)?;
    let blob = profiles.read_sector(dev, index as u16 + 1, None)?;
    let mut p = Profile::parse(&blob).map_err(Error::Protocol)?;
    if p.report_rate_ms == ms {
        return Ok(hz);
    }
    p.report_rate_ms = ms;
    write_profile(dev, profiles, index, &p)?;
    Ok(hz)
}

/// Store a button binding into a profile's flash sector.
pub fn set_button(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    index: u8,
    button: u8,
    binding: crate::profile_blob::Button,
) -> Result<()> {
    if button >= profiles.info.button_count {
        return Err(Error::Protocol(format!(
            "button {button} out of range 0..{}",
            profiles.info.button_count - 1
        )));
    }
    let blob = profiles.read_sector(dev, index as u16 + 1, None)?;
    let mut p = Profile::parse(&blob).map_err(Error::Protocol)?;
    p.buttons[button as usize] = binding;
    write_profile(dev, profiles, index, &p)?;
    Ok(())
}

/// Store lighting into a profile's flash sector, both zones and their alts.
pub fn set_lighting(
    dev: &mut Device,
    profiles: &OnboardProfiles,
    index: u8,
    leds: [crate::profile_blob::Led; 2],
) -> Result<()> {
    let blob = profiles.read_sector(dev, index as u16 + 1, None)?;
    let mut p = Profile::parse(&blob).map_err(Error::Protocol)?;
    p.leds = leds;
    p.alt_leds = leds;
    write_profile(dev, profiles, index, &p)?;
    Ok(())
}
