//! Codec for the 256-byte onboard profile sector (profile_format_id 2).
//!
//! Layout derived by reading this device's sectors and cross-checked against
//! libratbag; see PROTOCOL.md. A sector is only accepted by the device if its
//! trailing CRC is right, so always build sectors through `serialize`.
//!
//! ```text
//! 0x00        report rate, in milliseconds
//! 0x01        default DPI index
//! 0x02        shift (sniper) DPI index
//! 0x03-0x0c   dpi[5], u16 LITTLE-endian, 0 = slot disabled
//! 0x20-0x9f   buttons[32], 4 bytes each (this device uses 11)
//! 0xa0-0xcf   name, 48 bytes UTF-16LE
//! 0xd0-0xfb   leds[2] then alt_leds[2], 11 bytes each
//! 0xfe-0xff   CRC-16/CCITT-FALSE over 0x00..0xfd, BIG-endian
//! ```

use crate::led;

pub const SECTOR_SIZE: usize = 256;
pub const OFF_RATE: usize = 0x00;
pub const OFF_DEFAULT_DPI: usize = 0x01;
pub const OFF_SHIFT_DPI: usize = 0x02;
pub const OFF_DPI: usize = 0x03;
pub const N_DPI: usize = 5;
pub const OFF_BUTTONS: usize = 0x20;
pub const N_BUTTON_SLOTS: usize = 32;
pub const OFF_NAME: usize = 0xA0;
pub const NAME_LEN: usize = 48;
pub const OFF_LEDS: usize = 0xD0;
pub const LED_RECORD: usize = 11;
pub const OFF_CRC: usize = 0xFE;

// Button binding types.
pub const BTN_HID: u8 = 0x80;
pub const BTN_SPECIAL: u8 = 0x90;
pub const BTN_MACRO: u8 = 0x00;
pub const BTN_UNUSED: u8 = 0xFF;

pub const HID_NOOP: u8 = 0x00;
pub const HID_MOUSE: u8 = 0x01;
pub const HID_KEYBOARD: u8 = 0x02;
pub const HID_CONSUMER: u8 = 0x03;

pub const SPECIALS: &[(u8, &str)] = &[
    (0x01, "wheel-left"),
    (0x02, "wheel-right"),
    (0x03, "dpi-up"),
    (0x04, "dpi-down"),
    (0x05, "dpi-cycle"),
    (0x06, "dpi-default"),
    (0x07, "dpi-shift"),
    (0x08, "profile-up"),
    (0x09, "profile-down"),
    (0x0A, "profile-cycle"),
    (0x0B, "second-mode"),
];

pub fn special_name(code: u8) -> Option<&'static str> {
    SPECIALS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

pub fn special_code(name: &str) -> Option<u8> {
    SPECIALS.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

/// CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection.
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Button(pub [u8; 4]);

impl Button {
    pub fn kind(&self) -> &'static str {
        match self.0[0] {
            BTN_SPECIAL => "special",
            BTN_HID => match self.0[1] {
                HID_NOOP => "disabled",
                HID_MOUSE => "mouse",
                HID_KEYBOARD => "key",
                HID_CONSUMER => "consumer",
                _ => "hid",
            },
            BTN_UNUSED => "unused",
            BTN_MACRO => "macro",
            _ => "other",
        }
    }

    pub fn describe(&self) -> String {
        match self.kind() {
            "special" => special_name(self.0[1])
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("special-0x{:02x}", self.0[1])),
            "mouse" => {
                let mask = u16::from_be_bytes([self.0[2], self.0[3]]);
                if mask != 0 && mask & (mask - 1) == 0 {
                    format!("button {}", mask.trailing_zeros() + 1)
                } else {
                    format!("buttons mask 0x{mask:04x}")
                }
            }
            "key" => {
                if self.0[2] != 0 {
                    format!("key 0x{:02x} mods 0x{:02x}", self.0[3], self.0[2])
                } else {
                    format!("key 0x{:02x}", self.0[3])
                }
            }
            "consumer" => format!("consumer 0x{:04x}", u16::from_be_bytes([self.0[2], self.0[3]])),
            "macro" => format!("macro page {} offset {}", self.0[1], self.0[3]),
            k => k.to_string(),
        }
    }

    /// Mouse button `n`, 1-based. The mask is 16 bits wide, so there is no
    /// such thing as button 17; clamping beats a shift overflow.
    pub fn mouse(n: u8) -> Self {
        let mask: u16 = 1 << (n.clamp(1, 16) - 1);
        Button([BTN_HID, HID_MOUSE, (mask >> 8) as u8, (mask & 0xFF) as u8])
    }

    pub fn special(name: &str) -> Option<Self> {
        special_code(name).map(|c| Button([BTN_SPECIAL, c, 0xFF, 0xFF]))
    }

    pub fn key(code: u8, modifiers: u8) -> Self {
        Button([BTN_HID, HID_KEYBOARD, modifiers, code])
    }

    pub fn disabled() -> Self {
        Button([BTN_HID, HID_NOOP, 0x00, 0x00])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Led(pub [u8; LED_RECORD]);

impl Led {
    pub fn mode(&self) -> u8 {
        self.0[0]
    }

    pub fn describe(&self) -> String {
        match self.0[0] {
            led::FIXED => format!("fixed #{:02x}{:02x}{:02x}", self.0[1], self.0[2], self.0[3]),
            led::BREATHING => {
                let period = u16::from_be_bytes([self.0[4], self.0[5]]);
                let inten = if self.0[7] == 0 { 100 } else { self.0[7] };
                format!(
                    "breathing #{:02x}{:02x}{:02x} {}ms {} {}%",
                    self.0[1], self.0[2], self.0[3], period,
                    led::waveform_name(self.0[6]), inten
                )
            }
            led::CYCLE => {
                let period = u16::from_be_bytes([self.0[6], self.0[7]]);
                let inten = if self.0[8] == 0 { 100 } else { self.0[8] };
                format!("cycle {period}ms {inten}%")
            }
            m => led::mode_name(m).to_string(),
        }
    }

    pub fn fixed(c: [u8; 3]) -> Self {
        let mut r = [0u8; LED_RECORD];
        r[0] = led::FIXED;
        r[1..4].copy_from_slice(&c);
        Led(r)
    }

    pub fn breathing(c: [u8; 3], period_ms: u16, waveform: u8, intensity: u8) -> Self {
        let mut r = [0u8; LED_RECORD];
        r[0] = led::BREATHING;
        r[1..4].copy_from_slice(&c);
        r[4] = (period_ms >> 8) as u8;
        r[5] = (period_ms & 0xFF) as u8;
        r[6] = waveform;
        r[7] = if intensity >= 100 { 0 } else { intensity };
        Led(r)
    }

    pub fn cycle(period_ms: u16, intensity: u8) -> Self {
        let mut r = [0u8; LED_RECORD];
        r[0] = led::CYCLE;
        r[6] = (period_ms >> 8) as u8;
        r[7] = (period_ms & 0xFF) as u8;
        r[8] = if intensity >= 100 { 0 } else { intensity };
        Led(r)
    }

    pub fn off() -> Self {
        Led([0u8; LED_RECORD])
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub report_rate_ms: u8,
    pub default_dpi: u8,
    pub shift_dpi: u8,
    pub dpi: [u16; N_DPI],
    pub buttons: Vec<Button>,
    pub name: String,
    pub leds: [Led; 2],
    pub alt_leds: [Led; 2],
    /// Kept so `serialize` can pass through bytes this codec does not model.
    original: Option<[u8; SECTOR_SIZE]>,
}

impl Profile {
    pub fn report_rate_hz(&self) -> u16 {
        if self.report_rate_ms == 0 { 0 } else { 1000 / self.report_rate_ms as u16 }
    }

    pub fn parse(blob: &[u8]) -> Result<Self, String> {
        if blob.len() != SECTOR_SIZE {
            return Err(format!("profile sector must be {SECTOR_SIZE} bytes, got {}", blob.len()));
        }
        let mut dpi = [0u16; N_DPI];
        for (i, slot) in dpi.iter_mut().enumerate() {
            let o = OFF_DPI + i * 2;
            *slot = u16::from_le_bytes([blob[o], blob[o + 1]]);
        }
        let buttons = (0..N_BUTTON_SLOTS)
            .map(|i| {
                let o = OFF_BUTTONS + i * 4;
                Button([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]])
            })
            .collect();
        let name_u16: Vec<u16> = blob[OFF_NAME..OFF_NAME + NAME_LEN]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let name = String::from_utf16_lossy(&name_u16);
        let mut rec = [Led::off(); 4];
        for (i, r) in rec.iter_mut().enumerate() {
            let o = OFF_LEDS + i * LED_RECORD;
            let mut b = [0u8; LED_RECORD];
            b.copy_from_slice(&blob[o..o + LED_RECORD]);
            *r = Led(b);
        }
        let mut orig = [0u8; SECTOR_SIZE];
        orig.copy_from_slice(blob);
        Ok(Profile {
            report_rate_ms: blob[OFF_RATE],
            default_dpi: blob[OFF_DEFAULT_DPI],
            shift_dpi: blob[OFF_SHIFT_DPI],
            dpi,
            buttons,
            name,
            leds: [rec[0], rec[1]],
            alt_leds: [rec[2], rec[3]],
            original: Some(orig),
        })
    }

    pub fn crc_ok(&self) -> bool {
        match &self.original {
            Some(o) => {
                u16::from_be_bytes([o[OFF_CRC], o[OFF_CRC + 1]]) == crc_ccitt(&o[..OFF_CRC])
            }
            None => false,
        }
    }

    /// Rebuild the sector, preserving unknown bytes from the original so a
    /// round trip never silently drops firmware state.
    pub fn serialize(&self) -> [u8; SECTOR_SIZE] {
        let mut buf = self.original.unwrap_or([0xFF; SECTOR_SIZE]);
        buf[OFF_RATE] = self.report_rate_ms;
        buf[OFF_DEFAULT_DPI] = self.default_dpi;
        buf[OFF_SHIFT_DPI] = self.shift_dpi;
        for (i, v) in self.dpi.iter().enumerate() {
            let o = OFF_DPI + i * 2;
            buf[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
        for (i, b) in self.buttons.iter().take(N_BUTTON_SLOTS).enumerate() {
            let o = OFF_BUTTONS + i * 4;
            buf[o..o + 4].copy_from_slice(&b.0);
        }
        let mut nm: Vec<u8> = self.name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        nm.truncate(NAME_LEN);
        nm.resize(NAME_LEN, 0);
        buf[OFF_NAME..OFF_NAME + NAME_LEN].copy_from_slice(&nm);
        for (i, l) in self.leds.iter().chain(self.alt_leds.iter()).enumerate() {
            let o = OFF_LEDS + i * LED_RECORD;
            buf[o..o + LED_RECORD].copy_from_slice(&l.0);
        }
        let crc = crc_ccitt(&buf[..OFF_CRC]);
        buf[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    pub fn describe(&self, n_buttons: usize) -> String {
        let mut out = Vec::new();
        out.push(format!("report rate : {} Hz ({} ms)", self.report_rate_hz(), self.report_rate_ms));
        out.push(if self.name.is_empty() {
            "name        : (unset)".to_string()
        } else {
            format!("name        : {:?}", self.name)
        });
        let stages: Vec<String> = self.dpi.iter().filter(|d| **d != 0).map(|d| d.to_string()).collect();
        out.push(format!(
            "dpi stages  : [{}]  default=idx{} shift=idx{}",
            stages.join(", "), self.default_dpi, self.shift_dpi
        ));
        out.push("buttons:".into());
        for (i, b) in self.buttons.iter().take(n_buttons).enumerate() {
            out.push(format!("  {i:2}  {:<24} raw={:02x} {:02x} {:02x} {:02x}",
                b.describe(), b.0[0], b.0[1], b.0[2], b.0[3]));
        }
        for (i, l) in self.leds.iter().enumerate() {
            out.push(format!("led {i}       : {}", l.describe()));
        }
        for (i, l) in self.alt_leds.iter().enumerate() {
            out.push(format!("alt led {i}   : {}", l.describe()));
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real profile 0 read off the device, used as a golden fixture.
    fn golden() -> [u8; SECTOR_SIZE] {
        let mut b = [0xFFu8; SECTOR_SIZE];
        // header: 1ms, default idx 1, shift idx 0, dpi 800/1600/3200/6400/off
        b[0x00] = 0x01; b[0x01] = 0x01; b[0x02] = 0x00;
        b[0x03..0x0d].copy_from_slice(&[
            0x20, 0x03, 0x40, 0x06, 0x80, 0x0c, 0x00, 0x19, 0x00, 0x00,
        ]);
        // 11 buttons then unused slots
        let btns: [[u8; 4]; 11] = [
            [0x80, 0x01, 0x00, 0x01], [0x80, 0x01, 0x00, 0x02],
            [0x80, 0x01, 0x00, 0x04], [0x80, 0x01, 0x00, 0x08],
            [0x80, 0x01, 0x00, 0x10], [0x90, 0x07, 0xff, 0xff],
            [0x90, 0x04, 0xff, 0xff], [0x90, 0x03, 0xff, 0xff],
            [0x90, 0x0a, 0xff, 0xff], [0x90, 0x02, 0xff, 0xff],
            [0x90, 0x01, 0xff, 0xff],
        ];
        for (i, bt) in btns.iter().enumerate() {
            b[OFF_BUTTONS + i * 4..OFF_BUTTONS + i * 4 + 4].copy_from_slice(bt);
        }
        for i in 0xa0..0xd0 { b[i] = 0x00 }
        // four LED records: fixed cyan
        for i in 0..4 {
            let o = OFF_LEDS + i * LED_RECORD;
            b[o..o + LED_RECORD].copy_from_slice(&[0x01, 0x00, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0]);
        }
        b[0xfc] = 0xff; b[0xfd] = 0xff;
        let crc = crc_ccitt(&b[..OFF_CRC]);
        b[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_be_bytes());
        b
    }

    #[test]
    fn crc_matches_the_value_the_device_stored() {
        // 0xb348 is what the real mouse had in profile 0 with this content.
        assert_eq!(crc_ccitt(&golden()[..OFF_CRC]), 0xb348);
    }

    #[test]
    fn parses_the_device_fixture() {
        let p = Profile::parse(&golden()).unwrap();
        assert_eq!(p.report_rate_hz(), 1000);
        assert_eq!(p.dpi, [800, 1600, 3200, 6400, 0]);
        assert_eq!(p.default_dpi, 1);
        assert!(p.crc_ok());
    }

    #[test]
    fn dpi_stages_are_little_endian() {
        // 0x0320 LE at offset 3 must read as 800, not 0x2003.
        let p = Profile::parse(&golden()).unwrap();
        assert_eq!(p.dpi[0], 800);
    }

    #[test]
    fn button_decoding() {
        let p = Profile::parse(&golden()).unwrap();
        assert_eq!(p.buttons[0].describe(), "button 1");
        assert_eq!(p.buttons[4].describe(), "button 5");
        assert_eq!(p.buttons[5].describe(), "dpi-shift");
        assert_eq!(p.buttons[8].describe(), "profile-cycle");
        assert_eq!(p.buttons[10].describe(), "wheel-left");
    }

    #[test]
    fn led_decoding() {
        let p = Profile::parse(&golden()).unwrap();
        assert_eq!(p.leds[0].describe(), "fixed #00ffff");
    }

    #[test]
    fn round_trip_is_byte_identical() {
        let g = golden();
        let p = Profile::parse(&g).unwrap();
        assert_eq!(p.serialize(), g);
    }

    #[test]
    fn serialize_restamps_the_crc() {
        let mut p = Profile::parse(&golden()).unwrap();
        p.dpi[0] = 1000;
        let out = p.serialize();
        assert_eq!(
            u16::from_be_bytes([out[OFF_CRC], out[OFF_CRC + 1]]),
            crc_ccitt(&out[..OFF_CRC])
        );
        assert_eq!(Profile::parse(&out).unwrap().dpi[0], 1000);
    }

    #[test]
    fn serialize_preserves_unmodelled_bytes() {
        let mut g = golden();
        g[0x1f] = 0x5A; // a byte this codec does not model
        let crc = crc_ccitt(&g[..OFF_CRC]);
        g[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_be_bytes());
        let p = Profile::parse(&g).unwrap();
        assert_eq!(p.serialize()[0x1f], 0x5A);
    }

    #[test]
    fn rejects_wrong_sized_sector() {
        assert!(Profile::parse(&[0u8; 128]).is_err());
    }

    #[test]
    fn button_constructors_round_trip() {
        assert_eq!(Button::mouse(3).describe(), "button 3");
        assert_eq!(Button::special("dpi-shift").unwrap().describe(), "dpi-shift");
        assert!(Button::special("nonsense").is_none());
    }

    #[test]
    fn led_constructors_round_trip() {
        assert_eq!(Led::fixed([0, 0xff, 0xff]).describe(), "fixed #00ffff");
        let b = Led::breathing([0xff, 0, 0], 4000, 1, 100);
        assert_eq!(b.describe(), "breathing #ff0000 4000ms sine 100%");
        assert_eq!(Led::cycle(8000, 100).describe(), "cycle 8000ms 100%");
    }
}
