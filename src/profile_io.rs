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

/// Whether a sector's trailing CRC matches the rest of it.
fn sector_crc_ok(data: &[u8]) -> bool {
    data.len() == SECTOR_SIZE
        && u16::from_be_bytes([data[profile_blob::OFF_CRC], data[profile_blob::OFF_CRC + 1]])
            == profile_blob::crc_ccitt(&data[..profile_blob::OFF_CRC])
}

/// Whether a sector holds a profile in the layout this codec decodes. Profile
/// N is sector N + 1; sector 0 is the directory and the sectors past the
/// profiles hold macros, neither of which is decoded here.
fn is_profile_sector(profile_count: u8, sector: u16) -> bool {
    sector >= 1 && sector <= profile_count as u16
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
///
/// `data` goes down exactly as given, CRC included. That is deliberate: this
/// is also the path a backup is restored through, and restamping the last two
/// bytes of a sector that came off the device would turn a corruption nobody
/// noticed into one the device accepts. Callers that build a sector stamp it
/// themselves; `write_profile` does.
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
            profiles.info.sector_count.saturating_sub(1)
        )));
    }
    if data.len() != SECTOR_SIZE {
        return Err(Error::Protocol(format!(
            "sector must be {SECTOR_SIZE} bytes, got {}",
            data.len()
        )));
    }
    let mut payload = [0u8; SECTOR_SIZE];
    payload.copy_from_slice(data);
    let idx = profiles.feature_index();

    // Flash has finite endurance, so a write that asks for what is already
    // stored must not cost an erase cycle. The comparison is against the
    // device and not against whatever the caller last read, because every
    // write funnels through here and some callers arrive with a sector they
    // staged earlier or with a button the user pressed twice. A read that
    // fails tells us nothing, so it falls through and writes.
    if profiles.read_sector(dev, sector, None).ok().as_deref() == Some(payload.as_slice()) {
        return Ok(payload);
    }

    let n = payload.len() as u16;
    let mut head = Vec::with_capacity(6);
    head.extend_from_slice(&sector.to_be_bytes());
    head.extend_from_slice(&0u16.to_be_bytes());
    head.extend_from_slice(&n.to_be_bytes());
    dev.call(idx, MEMORY_ADDR_WRITE, &head)?;

    // Past the addr_write the device has an open write window, so a failure
    // from here on may already have put bytes into flash. Say so: "it timed
    // out" reads as "nothing happened", and acting on that is how a sector
    // ends up half old and half new. The window is deliberately left
    // unclosed, as libratbag does, rather than committing a partial buffer.
    let partial = |e: Error| {
        Error::Protocol(format!(
            "sector {sector} write stopped part-way ({e}); it may now hold a \
             mix of old and new bytes. Write it again, or restore a backup, \
             before trusting it."
        ))
    };
    for chunk in payload.chunks(CHUNK) {
        dev.call(idx, MEMORY_WRITE, chunk).map_err(partial)?;
    }
    // Short report, matching libratbag's write_end.
    dev.call_short(idx, MEMORY_WRITE_END, &[]).map_err(partial)?;

    std::thread::sleep(SETTLE);
    let back = profiles.read_sector(dev, sector, None)?;
    if back.len() != payload.len() {
        return Err(Error::Protocol(format!(
            "sector {sector} read back {} bytes, expected {}",
            back.len(),
            payload.len()
        )));
    }
    if back != payload {
        let at = back
            .iter()
            .zip(payload.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        return Err(Error::Protocol(format!(
            "sector {sector} readback differs at offset 0x{at:02x}: wrote 0x{:02x}, read 0x{:02x}",
            payload[at], back[at]
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
            profiles.info.profile_count.saturating_sub(1)
        )));
    }
    // `serialize` passes the sector's unmodelled bytes straight through, so a
    // sector that did not check out when it was read would go back with its
    // corruption blessed by a freshly stamped CRC — which is how one
    // interrupted write becomes a permanent one, button table included.
    if !profile.crc_ok() {
        return Err(Error::Protocol(format!(
            "profile {index} was read from a sector whose stored CRC does not \
             match its contents, so writing it back would make that permanent. \
             Restore a backup first."
        )));
    }
    // write_sector writes verbatim, so the CRC is stamped here, over the
    // sector this codec just built.
    write_sector(dev, profiles, index as u16 + 1, &stamp_crc(&profile.serialize())?)
}

/// Every readable sector, for a backup. Unreadable sectors are recorded as
/// absent rather than aborting the dump.
pub fn dump_all(dev: &mut Device, profiles: &OnboardProfiles) -> Vec<(u16, Option<Vec<u8>>)> {
    (0..profiles.info.sector_count as u16)
        .map(|s| (s, profiles.read_sector(dev, s, None).ok()))
        .collect()
}

/// The model name comes off the device, so it is not trusted to be JSON-safe.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Serialise a dump as JSON without pulling in a serde dependency.
pub fn dump_to_json(model: &str, dump: &[(u16, Option<Vec<u8>>)]) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"model\": \"{}\",\n", json_escape(model)));
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
        // Taken byte by byte rather than by slicing the &str: one stray
        // multi-byte character in a hand-edited or half-copied backup puts
        // `hex[i..i + 2]` inside a character, and the panic that follows is a
        // crash where the user should have been told the file is bad.
        let hex = val.trim_matches('"').as_bytes();
        if hex.len() % 2 != 0 {
            return Err(Error::Protocol(format!("sector {sector}: odd hex length")));
        }
        let bytes: Result<Vec<u8>> = hex
            .chunks(2)
            .map(|pair| {
                match ((pair[0] as char).to_digit(16), (pair[1] as char).to_digit(16)) {
                    (Some(hi), Some(lo)) => Ok((hi * 16 + lo) as u8),
                    _ => Err(Error::Protocol(format!("sector {sector}: bad hex"))),
                }
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
        // A profile sector carries a CRC over its own contents, so a mismatch
        // means the file is not what the device handed out: a flipped byte in
        // the button table reads as a perfectly valid binding and leaves a
        // mouse that moves but does not click. Checked before anything is
        // written, so a bad file costs nothing.
        if is_profile_sector(profiles.info.profile_count, *sector) && !sector_crc_ok(data) {
            return Err(Error::Protocol(format!(
                "sector {sector} of this backup does not match its own stored CRC, so \
                 it is not the sector the mouse saved. Nothing was written. Use a \
                 backup that checks out."
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
    fn a_stamped_sector_matches_its_own_crc() {
        assert!(sector_crc_ok(&stamp_crc(&[0x5Au8; SECTOR_SIZE]).unwrap()));
    }

    #[test]
    fn one_flipped_byte_fails_the_crc_check() {
        let mut bad = stamp_crc(&[0x5Au8; SECTOR_SIZE]).unwrap();
        bad[profile_blob::OFF_BUTTONS] ^= 0x01;
        assert!(!sector_crc_ok(&bad), "a corrupt button table must not pass");
        assert!(!sector_crc_ok(&bad[..200]), "a short sector has no CRC to match");
    }

    #[test]
    fn profile_sectors_are_the_ones_after_the_directory() {
        assert!(!is_profile_sector(5, 0), "sector 0 is the directory");
        assert!(is_profile_sector(5, 1));
        assert!(is_profile_sector(5, 5));
        assert!(!is_profile_sector(5, 6), "past the profiles is macro space");
        assert!(!is_profile_sector(0, 1), "a device claiming no profiles has none");
    }

    #[test]
    fn json_parse_rejects_empty() {
        assert!(dump_from_json("{}").is_err());
    }

    #[test]
    fn json_parse_rejects_odd_hex() {
        assert!(dump_from_json("{\n\"0\": \"abc\"\n}").is_err());
    }

    #[test]
    fn json_parse_rejects_non_ascii_hex_without_panicking() {
        // An even byte length whose second byte is inside a character: this
        // used to panic on a non-boundary &str slice.
        assert!(dump_from_json("{\n\"0\": \"a\u{0905}\"\n}").is_err());
        assert!(dump_from_json("{\n\"1\": \"\u{00ff}\u{00ff}\"\n}").is_err());
    }

    #[test]
    fn json_parse_rejects_a_signed_byte() {
        // from_str_radix accepted "+f"; the sector bytes are plain hex.
        assert!(dump_from_json("{\n\"0\": \"+f\"\n}").is_err());
    }
}

/// Whether writing `p` would actually change the sector it was read from.
///
/// Flash has finite endurance, so a save that asks for what is already stored
/// should cost nothing. Saving the same profile twice is an ordinary thing for
/// a user to do.
fn changed(p: &Profile, blob: &[u8]) -> bool {
    p.serialize().as_slice() != blob
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
    p.report_rate_ms = ms;
    if !changed(&p, &blob) {
        return Ok(hz);
    }
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
    // The second bound is the sector's own: the table has 32 slots whatever a
    // device claims, and indexing past it would panic.
    let limit = profiles.info.button_count.min(profile_blob::N_BUTTON_SLOTS as u8);
    if button >= limit {
        return Err(Error::Protocol(format!(
            "button {button} out of range 0..{}",
            limit.saturating_sub(1)
        )));
    }
    let blob = profiles.read_sector(dev, index as u16 + 1, None)?;
    let mut p = Profile::parse(&blob).map_err(Error::Protocol)?;
    p.buttons[button as usize] = binding;
    if !changed(&p, &blob) {
        return Ok(());
    }
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
    if !changed(&p, &blob) {
        return Ok(());
    }
    write_profile(dev, profiles, index, &p)?;
    Ok(())
}
