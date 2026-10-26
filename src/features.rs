#![allow(dead_code)]
//! HID++ 2.0 feature ids. The full table is kept as a protocol reference.

pub const ROOT: u16 = 0x0000;
pub const FEATURE_SET: u16 = 0x0001;
pub const DEVICE_FW_VERSION: u16 = 0x0003;
pub const DEVICE_NAME: u16 = 0x0005;
pub const LED_CONTROL: u16 = 0x1300;
pub const ADJUSTABLE_DPI: u16 = 0x2201;
pub const REPORT_RATE: u16 = 0x8060;
pub const COLOR_LED_EFFECTS: u16 = 0x8070;
pub const ONBOARD_PROFILES: u16 = 0x8100;
pub const MOUSE_BUTTON_SPY: u16 = 0x8110;

pub fn name(id: u16) -> &'static str {
    match id {
        0x0000 => "ROOT",
        0x0001 => "FEATURE_SET",
        0x0002 => "FEATURE_INFO",
        0x0003 => "DEVICE_FW_VERSION",
        0x0005 => "DEVICE_NAME",
        0x0020 => "CONFIG_CHANGE",
        0x00c2 => "DFU_CONTROL",
        0x1300 => "LED_CONTROL",
        0x1801 => "MANUFACTURING_MODE",
        0x1802 => "DEVICE_RESET",
        0x1806 => "CONFIG_DEVICE_PROPS",
        0x18a1 => "LED_TEST",
        0x18b1 => "STATIC_MONITOR_MODE",
        0x1e00 => "ENABLE_HIDDEN_FEATURES",
        0x1e22 => "SPI_DIRECT_ACCESS",
        0x1eb0 => "TDE_ACCESS",
        0x2201 => "ADJUSTABLE_DPI",
        0x8060 => "REPORT_RATE",
        0x8070 => "COLOR_LED_EFFECTS",
        0x8100 => "ONBOARD_PROFILES",
        0x8110 => "MOUSE_BUTTON_SPY",
        _ => "unknown",
    }
}

/// Feature-type flag bits from FEATURE_SET.getFeatureId.
pub fn type_flags(t: u8) -> String {
    let mut v = Vec::new();
    if t & 0x80 != 0 { v.push("obsolete") }
    if t & 0x40 != 0 { v.push("hidden") }
    if t & 0x20 != 0 { v.push("engineering") }
    if t & 0x10 != 0 { v.push("manufacturing") }
    if v.is_empty() { "-".into() } else { v.join(",") }
}
