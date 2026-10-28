//! 0x8060 REPORT_RATE.
//!
//! `set` only works in host mode. In onboard mode the rate is taken from the
//! active profile and the device answers setReportRate with "invalid
//! argument" (verified on a real G502 HERO). A rate set live is also lost when
//! the device returns to onboard mode, so to make one stick you write byte
//! 0x00 of the profile sector instead — see `profile_io::set_report_rate`.

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

    /// Set the live rate. Requires host mode; see the module note.
    pub fn set(&self, dev: &mut Device, hz: u16) -> Result<()> {
        let ok = self.supported(dev)?;
        if !ok.contains(&hz) {
            return Err(Error::Protocol(format!("{hz} Hz unsupported; have {ok:?}")));
        }
        dev.call(self.idx, SET_RATE, &[(1000 / hz) as u8])?;
        Ok(())
    }

    /// The one-byte interval a profile sector stores for `hz`.
    ///
    /// Callers use this as the test of whether a rate can be persisted at all,
    /// so it has to refuse anything byte 0x00 of the sector cannot hold: below
    /// 4 Hz the interval is over 255 ms and a bare `as u8` would silently
    /// store a different rate than the one asked for (1 Hz -> 232 ms).
    pub fn interval_ms(hz: u16) -> Result<u8> {
        if hz == 0 || 1000 % hz != 0 {
            return Err(Error::Protocol(format!("{hz} Hz is not a whole-ms interval")));
        }
        let ms = 1000 / hz;
        if ms > u8::MAX as u16 {
            return Err(Error::Protocol(format!(
                "{hz} Hz is a {ms} ms interval, which does not fit the profile's one byte"
            )));
        }
        Ok(ms as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_ms_accepts_the_rates_this_device_offers() {
        for (hz, ms) in [(1000u16, 1u8), (500, 2), (250, 4), (125, 8)] {
            assert_eq!(ReportRate::interval_ms(hz).unwrap(), ms, "{hz} Hz");
        }
    }

    #[test]
    fn interval_ms_refuses_a_rate_the_sector_byte_cannot_hold() {
        // 1 Hz is 1000 ms and 2 Hz is 500 ms. Byte 0x00 of the profile sector
        // is one byte, so these used to truncate to 232 and 244 ms: a rate
        // written to flash that nobody asked for.
        assert!(ReportRate::interval_ms(1).is_err());
        assert!(ReportRate::interval_ms(2).is_err());
        assert_eq!(ReportRate::interval_ms(4).unwrap(), 250);
    }

    #[test]
    fn interval_ms_refuses_a_rate_that_is_not_a_whole_millisecond() {
        assert!(ReportRate::interval_ms(0).is_err());
        assert!(ReportRate::interval_ms(333).is_err());
        assert!(ReportRate::interval_ms(144).is_err());
    }
}
