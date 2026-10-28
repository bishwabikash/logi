//! 0x8100 ONBOARD_PROFILES — mode, profile switching, profile memory.
//!
//! Function 1 is setOnboardMode, a *setter*. Calling it expecting a read will
//! write the mode. Reading is function 2.

use crate::features;
use crate::hidpp::{Device, Error, Result};

const GET_INFO: u8 = 0x00;
const SET_MODE: u8 = 0x01;
const GET_MODE: u8 = 0x02;
const SET_CURRENT_PROFILE: u8 = 0x03;
const GET_CURRENT_PROFILE: u8 = 0x04;
pub const MEMORY_READ: u8 = 0x05;
pub const MEMORY_ADDR_WRITE: u8 = 0x06;
pub const MEMORY_WRITE: u8 = 0x07;
pub const MEMORY_WRITE_END: u8 = 0x08;
const GET_CURRENT_DPI_INDEX: u8 = 0x0B;
const SET_CURRENT_DPI_INDEX: u8 = 0x0C;

/// Device drives itself from stored profiles.
pub const MODE_ONBOARD: u8 = 0x01;
/// Host drives it. Required for live 0x8070 lighting.
pub const MODE_HOST: u8 = 0x02;

pub fn mode_name(m: u8) -> &'static str {
    match m {
        MODE_ONBOARD => "onboard",
        MODE_HOST => "host",
        _ => "invalid",
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub memory_model_id: u8,
    pub profile_format_id: u8,
    pub macro_format_id: u8,
    pub profile_count: u8,
    pub profile_count_oob: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
    pub mechanical_layout: u8,
    pub various_info: u8,
}

pub struct OnboardProfiles {
    idx: u8,
    pub info: Info,
}

impl OnboardProfiles {
    pub fn new(dev: &mut Device) -> Result<Self> {
        let idx = dev.feature_index(features::ONBOARD_PROFILES)?;
        let p = dev.call(idx, GET_INFO, &[])?;
        if p.len() < 10 {
            return Err(Error::Protocol("short 0x8100 getInfo reply".into()));
        }
        Ok(OnboardProfiles {
            idx,
            info: Info {
                memory_model_id: p[0],
                profile_format_id: p[1],
                macro_format_id: p[2],
                profile_count: p[3],
                profile_count_oob: p[4],
                button_count: p[5],
                sector_count: p[6],
                sector_size: u16::from_be_bytes([p[7], p[8]]),
                mechanical_layout: p[9],
                various_info: p.get(10).copied().unwrap_or(0),
            },
        })
    }

    pub fn feature_index(&self) -> u8 {
        self.idx
    }

    pub fn mode(&self, dev: &mut Device) -> Result<u8> {
        Ok(dev.call(self.idx, GET_MODE, &[])?.first().copied().unwrap_or(0))
    }

    pub fn set_mode(&self, dev: &mut Device, mode: u8) -> Result<()> {
        if mode != MODE_ONBOARD && mode != MODE_HOST {
            return Err(Error::Protocol("mode must be onboard(1) or host(2)".into()));
        }
        dev.call(self.idx, SET_MODE, &[mode])?;
        // The firmware needs a moment before it honours writes in the new
        // mode. 50ms was marginal: a lighting write issued immediately after
        // the switch intermittently came back "invalid argument".
        std::thread::sleep(std::time::Duration::from_millis(150));
        Ok(())
    }

    /// Hold the device in host mode for the duration of `f`, then put it back.
    ///
    /// Leaving the mouse in host mode makes its lighting depend on this process
    /// still running, so the restore runs even when `f` returns an error. A
    /// panic is not covered — release builds abort — so `logi mode onboard` is
    /// the way back if the process dies holding host mode.
    pub fn with_host_mode<T>(
        &self,
        dev: &mut Device,
        f: impl FnOnce(&mut Device) -> Result<T>,
    ) -> Result<T> {
        let previous = self.mode(dev).unwrap_or(MODE_ONBOARD);
        if previous != MODE_HOST {
            self.set_mode(dev, MODE_HOST)?;
        }
        let out = f(dev);
        let restore = if previous == MODE_HOST { MODE_HOST } else { MODE_ONBOARD };
        let _ = self.set_mode(dev, restore);
        out
    }

    /// Active profile, 0-based.
    pub fn current(&self, dev: &mut Device) -> Result<u8> {
        let p = dev.call(self.idx, GET_CURRENT_PROFILE, &[])?;
        // Byte 0 is the memory bank, byte 1 the 1-based profile index.
        Ok(p.get(1).copied().unwrap_or(1).saturating_sub(1))
    }

    pub fn set_current(&self, dev: &mut Device, index: u8) -> Result<()> {
        if index >= self.info.profile_count {
            return Err(Error::Protocol(format!(
                "profile {index} out of range 0..{}",
                self.info.profile_count.saturating_sub(1)
            )));
        }
        dev.call(self.idx, SET_CURRENT_PROFILE, &[0x00, index + 1])?;
        Ok(())
    }

    pub fn dpi_index(&self, dev: &mut Device) -> Result<u8> {
        Ok(dev.call(self.idx, GET_CURRENT_DPI_INDEX, &[])?.first().copied().unwrap_or(0))
    }

    pub fn set_dpi_index(&self, dev: &mut Device, i: u8) -> Result<()> {
        dev.call(self.idx, SET_CURRENT_DPI_INDEX, &[i])?;
        Ok(())
    }

    /// Read one profile sector, 16 bytes per long report.
    pub fn read_sector(&self, dev: &mut Device, sector: u16, length: Option<usize>) -> Result<Vec<u8>> {
        let length = length.unwrap_or(self.info.sector_size as usize);
        let mut out = Vec::with_capacity(length);
        let mut offset = 0usize;
        while offset < length {
            let p = dev.call(
                self.idx,
                MEMORY_READ,
                &[
                    (sector >> 8) as u8, (sector & 0xFF) as u8,
                    (offset >> 8) as u8, (offset & 0xFF) as u8,
                ],
            )?;
            // Each reply carries its own 16 bytes and the next one starts 16
            // further on, so a reply that came up short cannot just be
            // appended: every byte after it would sit at the wrong offset,
            // and the sector would decode as plausible nonsense.
            let want = (length - offset).min(16);
            if p.len() < want {
                return Err(Error::Protocol(format!(
                    "short memoryRead reply for sector {sector} at offset {offset}: \
                     {} bytes, wanted {want}",
                    p.len()
                )));
            }
            out.extend_from_slice(&p[..want]);
            offset += 16;
        }
        Ok(out)
    }
}
