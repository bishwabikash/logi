//! 0x8070 COLOR_LED_EFFECTS — both RGB zones.
//!
//! Byte layouts follow libratbag's implementation of this device; see
//! PROTOCOL.md. The subtleties that bite:
//!
//!   * setZoneEffect payload is `[zone, effect_index, <10 param bytes>]`,
//!     twelve bytes in total. Byte 1 is the zone's **effect index**, not the
//!     mode id. They coincide for FIXED (both 1) and differ for everything
//!     else: BREATHING is mode 0x0a at index 2. Sending the mode id gets
//!     "invalid argument". Resolve the index from the zone's effect list.
//!   * Only FIXED and BREATHING are settable live on this device. Indices 0
//!     (off) and 3 (cycle) are rejected under every parameter layout tested.
//!     Live "off" is therefore a fixed black, and live cycling has to be
//!     streamed from the host. Both modes ARE storable in a profile sector,
//!     which uses mode ids rather than indices.
//!   * This device reports ext_caps 0x0000, so HAS_ZONE_EFFECT is clear and
//!     getZoneEffect is unimplemented: it answers with zeros rather than an
//!     error. Never infer a failed write from a zero readback.
//!   * 0x8070 writes only take effect in host mode (see profiles).

use crate::features;
use crate::hidpp::{Device, Error, Result};

const GET_INFO: u8 = 0x00;
const GET_ZONE_INFO: u8 = 0x01;
const GET_ZONE_EFFECT_INFO: u8 = 0x02;
pub const SET_ZONE_EFFECT: u8 = 0x04;

// enum hidpp20_led_mode
pub const OFF: u8 = 0x00;
pub const FIXED: u8 = 0x01;
pub const CYCLE: u8 = 0x03;
pub const WAVE: u8 = 0x04;
pub const STARLIGHT: u8 = 0x05;
pub const BREATHING: u8 = 0x0A;
pub const RIPPLE: u8 = 0x0B;
pub const CUSTOM: u8 = 0x0C;

pub fn mode_name(m: u8) -> &'static str {
    match m {
        OFF => "off",
        FIXED => "fixed",
        CYCLE => "cycle",
        WAVE => "wave",
        STARLIGHT => "starlight",
        BREATHING => "breathing",
        RIPPLE => "ripple",
        CUSTOM => "custom",
        _ => "?",
    }
}

/// enum hidpp20_led_waveform, BREATHING only.
pub const WAVEFORMS: &[(&str, u8)] = &[
    ("default", 0x00),
    ("sine", 0x01),
    ("square", 0x02),
    ("triangle", 0x03),
    ("sawtooth", 0x04),
    ("sharkfin", 0x05),
    ("exponential", 0x06),
];

pub fn waveform_id(name: &str) -> Option<u8> {
    WAVEFORMS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

pub fn waveform_name(id: u8) -> &'static str {
    WAVEFORMS.iter().find(|(_, v)| *v == id).map(|(n, _)| *n).unwrap_or("?")
}

pub fn zone_location_name(loc: u16) -> &'static str {
    match loc {
        0x0001 => "primary/logo",
        0x0002 => "logo",
        0x0003 => "left side",
        0x0004 => "right side",
        0x0005 => "combined",
        0x0006 => "primary-1",
        0x0007 => "primary-2",
        0x0008 => "DPI indicator",
        0x0009 => "edge",
        0x000A => "scroll wheel",
        _ => "unknown",
    }
}

// ext_caps bits
pub const EXT_CAP_HAS_ZONE_EFFECT: u16 = 0x01;

/// Parse `ff0066`, `#ff0066`, or a colour name.
pub fn parse_color(s: &str) -> Result<[u8; 3]> {
    let named = match s.trim().to_ascii_lowercase().as_str() {
        "off" | "black" => Some("000000"),
        "white" => Some("ffffff"),
        "red" => Some("ff0000"),
        "green" => Some("00ff00"),
        "blue" => Some("0000ff"),
        "cyan" => Some("00ffff"),
        "magenta" => Some("ff00ff"),
        "yellow" => Some("ffff00"),
        "orange" => Some("ff6600"),
        "purple" => Some("8000ff"),
        "pink" => Some("ff0080"),
        _ => None,
    };
    let t = named.map(|s| s.to_string()).unwrap_or_else(|| {
        s.trim().trim_start_matches('#').to_ascii_lowercase()
    });
    if t.len() != 6 {
        return Err(Error::Protocol(format!(
            "bad colour {s:?}: want rrggbb hex or a colour name"
        )));
    }
    let v = u32::from_str_radix(&t, 16)
        .map_err(|_| Error::Protocol(format!("bad colour {s:?}: not hex")))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

#[derive(Debug, Clone)]
pub struct Zone {
    pub index: u8,
    pub location: u16,
    pub num_effects: u8,
    pub persistency_caps: u8,
    /// Mode ids the firmware actually implements.
    pub modes: Vec<u8>,
}

impl Zone {
    pub fn location_name(&self) -> &'static str {
        zone_location_name(self.location)
    }
}

pub struct Leds {
    idx: u8,
    pub zone_count: u8,
    pub nv_caps: u16,
    pub ext_caps: u16,
    pub zones: Vec<Zone>,
}

impl Leds {
    pub fn new(dev: &mut Device) -> Result<Self> {
        let idx = dev.feature_index(features::COLOR_LED_EFFECTS)?;
        let info = dev.call(idx, GET_INFO, &[])?;
        if info.len() < 5 {
            return Err(Error::Protocol("short 0x8070 getInfo reply".into()));
        }
        // Reply is {u8 zone_count, u16 nv_caps, u16 ext_caps}.
        let mut me = Leds {
            idx,
            zone_count: info[0],
            nv_caps: u16::from_be_bytes([info[1], info[2]]),
            ext_caps: u16::from_be_bytes([info[3], info[4]]),
            zones: Vec::new(),
        };
        for z in 0..me.zone_count {
            me.zones.push(me.read_zone(dev, z)?);
        }
        Ok(me)
    }

    /// Whether getZoneEffect is implemented. False on this device.
    pub fn can_read_back(&self) -> bool {
        self.ext_caps & EXT_CAP_HAS_ZONE_EFFECT != 0
    }

    fn read_zone(&self, dev: &mut Device, z: u8) -> Result<Zone> {
        let p = dev.call(self.idx, GET_ZONE_INFO, &[z])?;
        if p.len() < 5 {
            return Err(Error::Protocol("short getZoneInfo reply".into()));
        }
        let mut zone = Zone {
            index: p[0],
            location: u16::from_be_bytes([p[1], p[2]]),
            num_effects: p[3],
            persistency_caps: p[4],
            modes: Vec::new(),
        };
        for e in 0..zone.num_effects {
            let q = dev.call(self.idx, GET_ZONE_EFFECT_INFO, &[z, e])?;
            // setZoneEffect addresses an effect by its position in this list,
            // so dropping an entry would shift every later effect's index and
            // quietly write the wrong effect. Refuse the zone instead.
            if q.len() < 4 {
                return Err(Error::Protocol(format!(
                    "short getZoneEffectInfo reply for zone {z} effect {e}"
                )));
            }
            zone.modes.push(u16::from_be_bytes([q[2], q[3]]) as u8);
        }
        Ok(zone)
    }

    pub fn supports(&self, zone: u8, mode: u8) -> bool {
        self.zones
            .get(zone as usize)
            .map(|z| z.modes.contains(&mode))
            .unwrap_or(false)
    }

    /// The zone's index for a mode id. setZoneEffect wants this, not the id.
    pub fn effect_index(&self, zone: u8, mode: u8) -> Option<u8> {
        self.zones
            .get(zone as usize)?
            .modes
            .iter()
            .position(|m| *m == mode)
            .map(|i| i as u8)
    }

    /// `[zone, effect_index, 10 param bytes]` — twelve bytes.
    fn payload(zone: u8, effect_index: u8, params: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 12];
        p[0] = zone;
        p[1] = effect_index;
        let n = params.len().min(10);
        p[2..2 + n].copy_from_slice(&params[..n]);
        p
    }

    fn set(&self, dev: &mut Device, zone: u8, mode: u8, params: &[u8]) -> Result<()> {
        let Some(ei) = self.effect_index(zone, mode) else {
            let have: Vec<&str> = self
                .zones
                .get(zone as usize)
                .map(|z| z.modes.iter().map(|m| mode_name(*m)).collect())
                .unwrap_or_default();
            return Err(Error::Protocol(format!(
                "zone {zone} has no mode {}; firmware implements: {}",
                mode_name(mode),
                have.join(",")
            )));
        };
        dev.call(self.idx, SET_ZONE_EFFECT, &Self::payload(zone, ei, params))?;
        Ok(())
    }

    /// Live "off".
    ///
    /// Effect index 0 is rejected by this firmware, so this is a fixed black
    /// instead. Visually identical; to store a real off mode in a profile,
    /// write `profile_blob::Led::off()` to flash.
    pub fn off(&self, dev: &mut Device, zone: u8) -> Result<()> {
        self.fixed(dev, zone, [0, 0, 0])
    }

    pub fn fixed(&self, dev: &mut Device, zone: u8, c: [u8; 3]) -> Result<()> {
        self.set(dev, zone, FIXED, &[c[0], c[1], c[2], 0x00])
    }

    /// Fire-and-forget solid colour. The primitive the animation engine drives.
    pub fn fixed_nowait(&self, dev: &mut Device, zone: u8, c: [u8; 3]) -> Result<()> {
        let Some(ei) = self.effect_index(zone, FIXED) else {
            return Err(Error::Protocol(format!("zone {zone} has no fixed mode")));
        };
        dev.send_nowait(
            self.idx,
            SET_ZONE_EFFECT,
            &Self::payload(zone, ei, &[c[0], c[1], c[2], 0x00]),
        )
    }

    pub fn breathing(
        &self,
        dev: &mut Device,
        zone: u8,
        c: [u8; 3],
        period_ms: u16,
        waveform: u8,
        intensity: u8,
    ) -> Result<()> {
        self.set(
            dev,
            zone,
            BREATHING,
            &[
                c[0], c[1], c[2],
                (period_ms >> 8) as u8, (period_ms & 0xFF) as u8,
                waveform,
                if intensity >= 100 { 0 } else { intensity.max(1) },
            ],
        )
    }

    /// Hardware cycling, which this firmware will not set live.
    ///
    /// Every parameter layout at effect index 3 is answered with "invalid
    /// argument", so rather than emit a confusing protocol error this says
    /// what to do instead. Cycling IS storable in a profile sector.
    pub fn cycle(&self, _dev: &mut Device, _zone: u8, _period_ms: u16, _intensity: u8) -> Result<()> {
        Err(Error::Protocol(
            "this device will not set hardware cycling live (effect index 3 is \
             rejected). Store it into an onboard profile instead, or stream \
             `logi effect rainbow` from the host."
                .into(),
        ))
    }

    pub fn all_fixed(&self, dev: &mut Device, c: [u8; 3]) -> Result<()> {
        for z in 0..self.zone_count {
            self.fixed(dev, z, c)?;
        }
        Ok(())
    }

    pub fn all_off(&self, dev: &mut Device) -> Result<()> {
        for z in 0..self.zone_count {
            self.off(dev, z)?;
        }
        Ok(())
    }

    /// Modes this firmware accepts over the live path, as opposed to modes it
    /// merely has an effect record for.
    pub fn live_modes(&self, zone: u8) -> Vec<u8> {
        self.zones
            .get(zone as usize)
            .map(|z| {
                z.modes.iter().copied().filter(|m| *m == FIXED || *m == BREATHING).collect()
            })
            .unwrap_or_default()
    }
}
