//! 0x2201 ADJUSTABLE_DPI.

use crate::features;
use crate::hidpp::{Device, Error, Result};

const GET_SENSOR_DPI_LIST: u8 = 0x01;
const GET_SENSOR_DPI: u8 = 0x02;
const SET_SENSOR_DPI: u8 = 0x03;

/// A word with 0xE000 set is a step size, not a DPI value.
const RANGE_MARKER: u16 = 0xE000;

#[derive(Debug, Clone, PartialEq)]
pub enum Capability {
    /// Continuous range. The HERO sensor reports this form.
    Range { min: u16, max: u16, step: u16 },
    List(Vec<u16>),
}

impl Capability {
    /// Snap to the nearest value the sensor can actually represent.
    pub fn snap(&self, dpi: u16) -> Result<u16> {
        match self {
            Capability::Range { min, max, step } => {
                if dpi < *min || dpi > *max {
                    return Err(Error::Protocol(format!("dpi {dpi} outside {min}-{max}")));
                }
                // A zero step would divide to infinity (or NaN at the minimum)
                // and collapse every request onto `min`: a DPI nobody asked
                // for, written to the sensor.
                if *step == 0 {
                    return Err(Error::Protocol(
                        "device reported a DPI range with a zero step".into(),
                    ));
                }
                // In u32, and clamped: a device whose range is not a whole
                // number of steps would otherwise round the top of the range
                // up past max, and a wide range with a coarse step overflows
                // a u16 on the way.
                let steps = ((dpi - min) as f32 / *step as f32).round() as u32;
                let snapped = (*min as u32 + steps * *step as u32).min(*max as u32);
                Ok(snapped as u16)
            }
            Capability::List(v) => {
                if v.contains(&dpi) {
                    Ok(dpi)
                } else {
                    Err(Error::Protocol(format!("dpi {dpi} not in {v:?}")))
                }
            }
        }
    }
}

pub struct Dpi {
    idx: u8,
}

impl Dpi {
    pub fn new(dev: &mut Device) -> Result<Self> {
        Ok(Dpi { idx: dev.feature_index(features::ADJUSTABLE_DPI)? })
    }

    pub fn capability(&self, dev: &mut Device, sensor: u8) -> Result<Capability> {
        let p = dev.call(self.idx, GET_SENSOR_DPI_LIST, &[sensor])?;
        let mut values = Vec::new();
        let mut step = None;
        let mut i = 1;
        while i + 1 < p.len().min(15) {
            let w = u16::from_be_bytes([p[i], p[i + 1]]);
            if w == 0 {
                break;
            }
            if w & RANGE_MARKER == RANGE_MARKER {
                step = Some(w & 0x1FFF);
            } else {
                values.push(w);
            }
            i += 2;
        }
        match (step, values.len()) {
            (Some(s), n) if n >= 2 => Ok(Capability::Range {
                min: values[0],
                max: values[n - 1],
                step: s,
            }),
            _ => Ok(Capability::List(values)),
        }
    }

    pub fn get(&self, dev: &mut Device, sensor: u8) -> Result<u16> {
        let p = dev.call(self.idx, GET_SENSOR_DPI, &[sensor])?;
        if p.len() < 3 {
            return Err(Error::Protocol("short getSensorDpi reply".into()));
        }
        Ok(u16::from_be_bytes([p[1], p[2]]))
    }

    pub fn set(&self, dev: &mut Device, dpi: u16, sensor: u8) -> Result<u16> {
        let snapped = self.capability(dev, sensor)?.snap(dpi)?;
        dev.call(
            self.idx,
            SET_SENSOR_DPI,
            &[sensor, (snapped >> 8) as u8, (snapped & 0xFF) as u8],
        )?;
        Ok(snapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g502() -> Capability {
        Capability::Range { min: 100, max: 25600, step: 50 }
    }

    #[test]
    fn snap_rounds_to_the_step_and_stays_in_range() {
        assert_eq!(g502().snap(1000).unwrap(), 1000);
        assert_eq!(g502().snap(1020).unwrap(), 1000);
        assert_eq!(g502().snap(1030).unwrap(), 1050);
        assert_eq!(g502().snap(25600).unwrap(), 25600);
        assert!(g502().snap(99).is_err());
        assert!(g502().snap(25601).is_err());
    }

    #[test]
    fn snap_never_exceeds_max_when_the_range_is_not_whole_steps() {
        let c = Capability::Range { min: 100, max: 190, step: 100 };
        assert_eq!(c.snap(190).unwrap(), 190);
    }

    #[test]
    fn a_zero_step_is_an_error_not_the_minimum() {
        let c = Capability::Range { min: 100, max: 25600, step: 0 };
        assert!(c.snap(100).is_err());
        assert!(c.snap(5000).is_err());
    }

    #[test]
    fn a_list_only_accepts_its_own_values() {
        let c = Capability::List(vec![400, 800]);
        assert_eq!(c.snap(800).unwrap(), 800);
        assert!(c.snap(801).is_err());
    }
}
