"""0x8070 COLOR_LED_EFFECTS — both RGB zones.

Byte layouts and the mode/waveform enums follow libratbag's implementation of
this device; see PROTOCOL.md. The important details:

  * setZoneEffect payload is [zone, mode, <10 param bytes>]
  * CYCLE has five unused bytes before its period, which is the usual reason
    a hand-built cycling write is rejected with "invalid argument"
  * this device reports ext_caps 0x0000, so HAS_ZONE_EFFECT is clear and
    getZoneEffect is unsupported: it answers with zeros rather than an
    error. That is not a failed write.
  * 0x8070 writes only take effect in host mode; see profiles.OnboardProfiles
"""
from __future__ import annotations

from . import features as F

GET_INFO = 0x00
GET_ZONE_INFO = 0x01
GET_ZONE_EFFECT_INFO = 0x02
GET_ZONE_EFFECT = 0x03
SET_ZONE_EFFECT = 0x04

# enum hidpp20_led_mode
OFF = 0x00
FIXED = 0x01
CYCLE = 0x03
WAVE = 0x04
STARLIGHT = 0x05
BREATHING = 0x0A
RIPPLE = 0x0B
CUSTOM = 0x0C

MODE_NAMES = {
    OFF: "off", FIXED: "fixed", CYCLE: "cycle", WAVE: "wave",
    STARLIGHT: "starlight", BREATHING: "breathing", RIPPLE: "ripple",
    CUSTOM: "custom",
}

# enum hidpp20_led_waveform (BREATHING only)
WAVEFORMS = {
    "default": 0x00, "sine": 0x01, "square": 0x02, "triangle": 0x03,
    "sawtooth": 0x04, "sharkfin": 0x05, "exponential": 0x06,
}

ZONE_LOCATIONS = {
    0x0000: "unknown", 0x0001: "primary/logo", 0x0002: "logo",
    0x0003: "left side", 0x0004: "right side", 0x0005: "combined",
    0x0006: "primary-1", 0x0007: "primary-2", 0x0008: "DPI indicator",
    0x0009: "edge", 0x000A: "scroll wheel",
}

# ext_caps bits
EXT_CAP_HAS_ZONE_EFFECT = 0x01
EXT_CAP_NO_GET_EFFECT_SETTINGS = 0x02
EXT_CAP_HAS_SET_LED_BIN_INFO = 0x04
EXT_CAP_MONOCHROME_ONLY = 0x08


def parse_color(s: str) -> tuple[int, int, int]:
    """Accept 'ff0066', '#ff0066', or 'red'."""
    named = {
        "off": "000000", "black": "000000", "white": "ffffff",
        "red": "ff0000", "green": "00ff00", "blue": "0000ff",
        "cyan": "00ffff", "magenta": "ff00ff", "yellow": "ffff00",
        "orange": "ff6600", "purple": "8000ff", "pink": "ff0080",
    }
    t = named.get(s.strip().lower(), s.strip().lstrip("#"))
    if len(t) != 6:
        raise ValueError(f"bad colour {s!r}: want rrggbb hex or a colour name")
    try:
        v = int(t, 16)
    except ValueError:
        raise ValueError(f"bad colour {s!r}: not hex") from None
    return (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF


class Zone:
    def __init__(self, index: int, location: int, num_effects: int,
                 persistency_caps: int):
        self.index = index
        self.location = location
        self.num_effects = num_effects
        self.persistency_caps = persistency_caps
        self.modes: list[int] = []  # mode ids the firmware actually implements

    @property
    def location_name(self) -> str:
        return ZONE_LOCATIONS.get(self.location, f"0x{self.location:04x}")

    def __repr__(self) -> str:
        names = ",".join(MODE_NAMES.get(m, hex(m)) for m in self.modes)
        return f"<Zone {self.index} {self.location_name} [{names}]>"


class Leds:
    def __init__(self, dev):
        self.dev = dev
        self.idx = dev.feature_index(F.COLOR_LED_EFFECTS)
        info = self.dev.call(self.idx, GET_INFO)
        self.zone_count = info[0]
        self.nv_caps = (info[1] << 8) | info[2]
        self.ext_caps = (info[3] << 8) | info[4]
        self.zones = [self._zone(z) for z in range(self.zone_count)]

    @property
    def can_read_back(self) -> bool:
        """Whether getZoneEffect is implemented.

        This device reports ext_caps 0x0000, so HAS_ZONE_EFFECT is clear and
        reading the current effect is unsupported — it answers with zeros
        rather than an error. Never infer a failed write from that.
        """
        return bool(self.ext_caps & EXT_CAP_HAS_ZONE_EFFECT)

    def _zone(self, z: int) -> Zone:
        p = self.dev.call(self.idx, GET_ZONE_INFO, bytes([z]))
        zone = Zone(p[0], (p[1] << 8) | p[2], p[3], p[4])
        for e in range(zone.num_effects):
            q = self.dev.call(self.idx, GET_ZONE_EFFECT_INFO, bytes([z, e]))
            zone.modes.append((q[2] << 8) | q[3])
        return zone

    def supports(self, zone: int, mode: int) -> bool:
        return mode in self.zones[zone].modes

    # -- writes ------------------------------------------------------------
    def _set(self, zone: int, mode: int, params: bytes, nowait: bool = False):
        if not self.supports(zone, mode):
            have = ",".join(MODE_NAMES.get(m, hex(m)) for m in self.zones[zone].modes)
            raise ValueError(
                f"zone {zone} has no mode {MODE_NAMES.get(mode, hex(mode))}; "
                f"firmware implements: {have}"
            )
        payload = bytes([zone, mode]) + params
        payload += b"\x00" * (11 - len(payload))
        if nowait:
            self.dev.send_nowait(self.idx, SET_ZONE_EFFECT, payload)
        else:
            self.dev.call(self.idx, SET_ZONE_EFFECT, payload)

    def off(self, zone: int) -> None:
        self._set(zone, OFF, b"")

    def fixed(self, zone: int, r: int, g: int, b: int,
              nowait: bool = False) -> None:
        """Solid colour. This is also the primitive the animation engine
        drives, hence the nowait path."""
        self._set(zone, FIXED, bytes([r & 0xFF, g & 0xFF, b & 0xFF, 0x00]),
                  nowait=nowait)

    def breathing(self, zone: int, r: int, g: int, b: int,
                  period_ms: int = 4000, waveform: str = "default",
                  intensity: int = 100) -> None:
        wf = WAVEFORMS[waveform] if isinstance(waveform, str) else waveform
        self._set(zone, BREATHING, bytes([
            r & 0xFF, g & 0xFF, b & 0xFF,
            (period_ms >> 8) & 0xFF, period_ms & 0xFF,
            wf & 0xFF,
            0 if intensity >= 100 else max(1, intensity) & 0xFF,
        ]))

    def cycle(self, zone: int, period_ms: int = 8000,
              intensity: int = 100) -> None:
        # five unused bytes come before the period here
        self._set(zone, CYCLE, bytes([
            0, 0, 0, 0, 0,
            (period_ms >> 8) & 0xFF, period_ms & 0xFF,
            0 if intensity >= 100 else max(1, intensity) & 0xFF,
        ]))

    def all_fixed(self, r: int, g: int, b: int, nowait: bool = False) -> None:
        for z in range(self.zone_count):
            self.fixed(z, r, g, b, nowait=nowait)

    def all_off(self) -> None:
        for z in range(self.zone_count):
            self.off(z)
