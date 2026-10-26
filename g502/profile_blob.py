"""Codec for the 256-byte onboard profile sector (profile_format_id 2).

Layout was derived by reading this device's sectors and cross-checking against
libratbag; see PROTOCOL.md. A sector is only accepted by the device if its
trailing CRC is right, so always serialize through this module.

    0x00        report rate, in milliseconds
    0x01        default DPI index
    0x02        shift (sniper) DPI index
    0x03-0x0c   dpi[5], uint16 little-endian, 0 = slot disabled
    0x20-0x9f   buttons[32], 4 bytes each (this device uses 11)
    0xa0-0xcf   name, 48 bytes UTF-16LE
    0xd0-0xfb   leds[2] then alt_leds[2], 11 bytes each
    0xfe-0xff   CRC-16/CCITT-FALSE over 0x00..0xfd, big-endian
"""
from __future__ import annotations

import struct
from dataclasses import dataclass, field

SECTOR_SIZE = 256
OFF_RATE = 0x00
OFF_DEFAULT_DPI = 0x01
OFF_SHIFT_DPI = 0x02
OFF_DPI = 0x03
N_DPI = 5
OFF_BUTTONS = 0x20
N_BUTTON_SLOTS = 32
OFF_NAME = 0xA0
NAME_LEN = 48
OFF_LEDS = 0xD0
LED_RECORD = 11
OFF_CRC = 0xFE

# button binding types
BTN_HID = 0x80
BTN_SPECIAL = 0x90
BTN_MACRO = 0x00
BTN_DISABLED = 0xFF

HID_NOOP = 0x00
HID_MOUSE = 0x01
HID_KEYBOARD = 0x02
HID_CONSUMER = 0x03

SPECIALS = {
    0x01: "wheel-left", 0x02: "wheel-right",
    0x03: "dpi-up", 0x04: "dpi-down", 0x05: "dpi-cycle",
    0x06: "dpi-default", 0x07: "dpi-shift",
    0x08: "profile-up", 0x09: "profile-down", 0x0A: "profile-cycle",
    0x0B: "second-mode",
}
SPECIALS_REV = {v: k for k, v in SPECIALS.items()}


def crc_ccitt(data: bytes) -> int:
    """CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection."""
    crc = 0xFFFF
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


@dataclass
class Button:
    raw: bytes

    @property
    def kind(self) -> str:
        t = self.raw[0]
        if t == BTN_SPECIAL:
            return "special"
        if t == BTN_HID:
            return {HID_NOOP: "disabled", HID_MOUSE: "mouse",
                    HID_KEYBOARD: "key", HID_CONSUMER: "consumer"}.get(
                        self.raw[1], "hid")
        if t == BTN_DISABLED:
            return "unused"
        if t == BTN_MACRO:
            return "macro"
        return f"type-0x{t:02x}"

    def describe(self) -> str:
        k = self.kind
        if k == "special":
            return SPECIALS.get(self.raw[1], f"special-0x{self.raw[1]:02x}")
        if k == "mouse":
            mask = (self.raw[2] << 8) | self.raw[3]
            n = mask.bit_length() if mask and not (mask & (mask - 1)) else None
            return f"button {n}" if n else f"buttons mask 0x{mask:04x}"
        if k == "key":
            mods, key = self.raw[2], self.raw[3]
            return f"key 0x{key:02x}" + (f" mods 0x{mods:02x}" if mods else "")
        if k == "consumer":
            return f"consumer 0x{(self.raw[2] << 8) | self.raw[3]:04x}"
        if k == "macro":
            return f"macro page {self.raw[1]} offset {self.raw[3]}"
        return k

    # constructors
    @staticmethod
    def mouse(n: int) -> "Button":
        mask = 1 << (n - 1)
        return Button(bytes([BTN_HID, HID_MOUSE, (mask >> 8) & 0xFF, mask & 0xFF]))

    @staticmethod
    def special(name: str) -> "Button":
        if name not in SPECIALS_REV:
            raise ValueError(f"unknown special {name!r}; "
                             f"have {', '.join(sorted(SPECIALS_REV))}")
        return Button(bytes([BTN_SPECIAL, SPECIALS_REV[name], 0xFF, 0xFF]))

    @staticmethod
    def key(code: int, modifiers: int = 0) -> "Button":
        return Button(bytes([BTN_HID, HID_KEYBOARD, modifiers & 0xFF, code & 0xFF]))

    @staticmethod
    def disabled() -> "Button":
        return Button(bytes([BTN_HID, HID_NOOP, 0x00, 0x00]))

    @staticmethod
    def unused() -> "Button":
        return Button(b"\xff\xff\xff\xff")


@dataclass
class Led:
    raw: bytes  # 11 bytes: mode + 10 params

    @property
    def mode(self) -> int:
        return self.raw[0]

    def describe(self) -> str:
        from .led import MODE_NAMES, WAVEFORMS
        m = self.raw[0]
        name = MODE_NAMES.get(m, f"0x{m:02x}")
        if m == 0x01:
            return f"fixed #{self.raw[1]:02x}{self.raw[2]:02x}{self.raw[3]:02x}"
        if m == 0x0A:
            period = (self.raw[4] << 8) | self.raw[5]
            wf = {v: k for k, v in WAVEFORMS.items()}.get(self.raw[6], self.raw[6])
            inten = self.raw[7] or 100
            return (f"breathing #{self.raw[1]:02x}{self.raw[2]:02x}{self.raw[3]:02x}"
                    f" {period}ms {wf} {inten}%")
        if m == 0x03:
            period = (self.raw[6] << 8) | self.raw[7]
            return f"cycle {period}ms {self.raw[8] or 100}%"
        return name

    @staticmethod
    def fixed(r: int, g: int, b: int) -> "Led":
        return Led(bytes([0x01, r, g, b]) + b"\x00" * 7)

    @staticmethod
    def breathing(r: int, g: int, b: int, period_ms: int = 4000,
                  waveform: int = 0, intensity: int = 100) -> "Led":
        return Led(bytes([0x0A, r, g, b, (period_ms >> 8) & 0xFF,
                          period_ms & 0xFF, waveform,
                          0 if intensity >= 100 else intensity]) + b"\x00" * 3)

    @staticmethod
    def cycle(period_ms: int = 8000, intensity: int = 100) -> "Led":
        return Led(bytes([0x03, 0, 0, 0, 0, 0, (period_ms >> 8) & 0xFF,
                          period_ms & 0xFF,
                          0 if intensity >= 100 else intensity]) + b"\x00" * 2)

    @staticmethod
    def off() -> "Led":
        return Led(b"\x00" * 11)


@dataclass
class Profile:
    report_rate_ms: int = 1
    default_dpi: int = 1
    shift_dpi: int = 0
    dpi: list[int] = field(default_factory=lambda: [800, 1600, 3200, 6400, 0])
    buttons: list[Button] = field(default_factory=list)
    name: str = ""
    leds: list[Led] = field(default_factory=list)
    alt_leds: list[Led] = field(default_factory=list)
    _original: bytes | None = None

    @property
    def report_rate_hz(self) -> int:
        return 1000 // self.report_rate_ms if self.report_rate_ms else 0

    @classmethod
    def parse(cls, blob: bytes) -> "Profile":
        if len(blob) != SECTOR_SIZE:
            raise ValueError(f"profile sector must be {SECTOR_SIZE} bytes, got {len(blob)}")
        p = cls()
        p._original = blob
        p.report_rate_ms = blob[OFF_RATE]
        p.default_dpi = blob[OFF_DEFAULT_DPI]
        p.shift_dpi = blob[OFF_SHIFT_DPI]
        p.dpi = list(struct.unpack_from("<5H", blob, OFF_DPI))
        p.buttons = [Button(blob[OFF_BUTTONS + i * 4: OFF_BUTTONS + i * 4 + 4])
                     for i in range(N_BUTTON_SLOTS)]
        raw_name = blob[OFF_NAME:OFF_NAME + NAME_LEN]
        p.name = raw_name.decode("utf-16-le", "replace").split("\x00")[0]
        recs = [Led(blob[OFF_LEDS + i * LED_RECORD: OFF_LEDS + (i + 1) * LED_RECORD])
                for i in range(4)]
        p.leds, p.alt_leds = recs[:2], recs[2:]
        return p

    def crc_ok(self) -> bool:
        if not self._original:
            return False
        stored = struct.unpack_from(">H", self._original, OFF_CRC)[0]
        return stored == crc_ccitt(self._original[:OFF_CRC])

    def serialize(self) -> bytes:
        """Rebuild the sector, preserving unknown bytes from the original.

        Fields this codec does not model are copied through rather than
        zeroed, so a round trip never silently drops firmware state.
        """
        buf = bytearray(self._original if self._original else b"\xff" * SECTOR_SIZE)
        buf[OFF_RATE] = self.report_rate_ms
        buf[OFF_DEFAULT_DPI] = self.default_dpi
        buf[OFF_SHIFT_DPI] = self.shift_dpi
        struct.pack_into("<5H", buf, OFF_DPI, *(list(self.dpi) + [0] * N_DPI)[:N_DPI])
        for i, b in enumerate(self.buttons[:N_BUTTON_SLOTS]):
            buf[OFF_BUTTONS + i * 4: OFF_BUTTONS + i * 4 + 4] = b.raw
        nm = self.name.encode("utf-16-le")[:NAME_LEN]
        buf[OFF_NAME:OFF_NAME + NAME_LEN] = nm + b"\x00" * (NAME_LEN - len(nm))
        for i, led in enumerate((list(self.leds) + list(self.alt_leds))[:4]):
            buf[OFF_LEDS + i * LED_RECORD: OFF_LEDS + (i + 1) * LED_RECORD] = led.raw
        struct.pack_into(">H", buf, OFF_CRC, crc_ccitt(bytes(buf[:OFF_CRC])))
        return bytes(buf)

    def describe(self, n_buttons: int = 11) -> str:
        out = [f"report rate : {self.report_rate_hz} Hz ({self.report_rate_ms} ms)",
               f"name        : {self.name!r}" if self.name else "name        : (unset)",
               f"dpi stages  : {[d for d in self.dpi if d]}  "
               f"default=idx{self.default_dpi} shift=idx{self.shift_dpi}"]
        out.append("buttons:")
        for i, b in enumerate(self.buttons[:n_buttons]):
            out.append(f"  {i:2d}  {b.describe():<24} raw={b.raw.hex(' ')}")
        for i, led in enumerate(self.leds):
            out.append(f"led {i}       : {led.describe()}")
        for i, led in enumerate(self.alt_leds):
            out.append(f"alt led {i}   : {led.describe()}")
        return "\n".join(out)
