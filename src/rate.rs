//! 0x8060 REPORT_RATE.

use crate::features;
use crate::hidpp::{Device, Error, Result};

const GET_RATE_LIST: u8 = 0x00;
const GET_RATE: u8 = 0x01;
const SET_RATE: u8 = 0x02;

pub struct ReportRate {
    idx: u8,
}

impl ReportRate {
    pub fn new(dev: &mut Device) -> Result<Self> {
        Ok(ReportRate { idx: dev.feature_index(features::REPORT_RATE)? })
    }

    /// Bit N of the mask means an interval of (N+1) ms is supported.
    pub fn supported(&self, dev: &mut Device) -> Result<Vec<u16>> {
        let mask = dev.call(self.idx, GET_RATE_LIST, &[])?.first().copied().unwrap_or(0);
        let mut v: Vec<u16> = (0..8)
            .filter(|b| mask & (1 << b) != 0)
            .map(|b| 1000u16 / (b as u16 + 1))
            .collect();
        v.sort_unstable();
        Ok(v)
    }

    pub fn get(&self, dev: &mut Device) -> Result<u16> {
        let ms = dev.call(self.idx, GET_RATE, &[])?.first().copied().unwrap_or(0);
        Ok(if ms == 0 { 0 } else { 1000 / ms as u16 })
    }

    pub fn set(&self, dev: &mut Device, hz: u16) -> Result<()> {
        let ok = self.supported(dev)?;
        if !ok.contains(&hz) {
            return Err(Error::Protocol(format!("{hz} Hz unsupported; have {ok:?}")));
        }
        dev.call(self.idx, SET_RATE, &[(1000 / hz) as u8])?;
        Ok(())
    }
}
