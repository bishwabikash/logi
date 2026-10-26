//! 0x2201 ADJUSTABLE_DPI.

use crate::features;
use crate::hidpp::{Device, Error, Result};

const GET_SENSOR_COUNT: u8 = 0x00;
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
                let steps = ((dpi - min) as f32 / *step as f32).round() as u16;
                Ok(min + steps * step)
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

    pub fn sensor_count(&self, dev: &mut Device) -> Result<u8> {
        Ok(dev.call(self.idx, GET_SENSOR_COUNT, &[])?.first().copied().unwrap_or(0))
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

    pub fn default(&self, dev: &mut Device, sensor: u8) -> Result<u16> {
        let p = dev.call(self.idx, GET_SENSOR_DPI, &[sensor])?;
        if p.len() < 5 {
            return Err(Error::Protocol("short getSensorDpi reply".into()));
        }
        Ok(u16::from_be_bytes([p[3], p[4]]))
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
