"""HID++ 2.0 feature ids."""

ROOT                      = 0x0000
FEATURE_SET               = 0x0001
FEATURE_INFO              = 0x0002
DEVICE_FW_VERSION         = 0x0003
DEVICE_NAME               = 0x0005
CONFIG_CHANGE             = 0x0020
DFU_CONTROL               = 0x00C2
LED_CONTROL               = 0x1300
LED_TEST                  = 0x18A1
ENABLE_HIDDEN_FEATURES    = 0x1E00
SPI_DIRECT_ACCESS         = 0x1E22
TDE_ACCESS                = 0x1EB0
DEVICE_RESET              = 0x1802
CONFIG_DEVICE_PROPS       = 0x1806
ADJUSTABLE_DPI            = 0x2201
COLOR_LED_EFFECTS         = 0x8070
REPORT_RATE               = 0x8060
ONBOARD_PROFILES          = 0x8100
MOUSE_BUTTON_SPY          = 0x8110

NAMES = {
    0x0000: "ROOT", 0x0001: "FEATURE_SET", 0x0002: "FEATURE_INFO",
    0x0003: "DEVICE_FW_VERSION", 0x0005: "DEVICE_NAME",
    0x0020: "CONFIG_CHANGE", 0x00C2: "DFU_CONTROL",
    0x1300: "LED_CONTROL", 0x1801: "MANUFACTURING_MODE",
    0x1802: "DEVICE_RESET", 0x1806: "CONFIG_DEVICE_PROPS",
    0x18A1: "LED_TEST", 0x18B1: "STATIC_MONITOR_MODE",
    0x1E00: "ENABLE_HIDDEN_FEATURES", 0x1E22: "SPI_DIRECT_ACCESS",
    0x1EB0: "TDE_ACCESS", 0x2201: "ADJUSTABLE_DPI",
    0x8060: "REPORT_RATE", 0x8070: "COLOR_LED_EFFECTS",
    0x8100: "ONBOARD_PROFILES", 0x8110: "MOUSE_BUTTON_SPY",
}

# Feature-type flag bits returned by FEATURE_SET.getFeatureId
TYPE_FLAGS = [
    (0x80, "obsolete"),
    (0x40, "hidden"),
    (0x20, "engineering"),
    (0x10, "manufacturing"),
]


def type_flags(t: int) -> str:
    return ",".join(n for bit, n in TYPE_FLAGS if t & bit) or "-"


def name(fid: int) -> str:
    return NAMES.get(fid, f"unknown-0x{fid:04x}")
