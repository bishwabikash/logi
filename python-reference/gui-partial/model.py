"""Pure logic shared by the pages: no gi, no device access.

Everything here is deterministic and unit-testable, which is the only way to
get coverage on a project whose other half needs hardware. The pages stay
thin wrappers that move values between widgets and these dataclasses.
"""
from __future__ import annotations

from dataclasses import dataclass, field

from g502 import led as L
from g502 import profile_blob as pb

RGB = tuple[int, int, int]

# Modes this firmware implements, in the order the UI should offer them.
# The real list always comes from Leds.supports(); this is only the ordering.
MODE_ORDER = (L.OFF, L.FIXED, L.BREATHING, L.CYCLE)
WAVEFORM_ORDER = ("default", "sine", "square", "triangle", "sawtooth",
                  "sharkfin", "exponential")


def rgb_to_hex(rgb: RGB) -> str:
    return "#%02x%02x%02x" % rgb


def hex_to_rgb(text: str) -> RGB:
    """Parse a colour the same way the CLI does, so names work too."""
    return L.parse_color(text)


def snap_dpi(value: int, cap: dict) -> int:
    """Clamp and quantise a DPI to what the sensor can represent.

    Dpi.set() does this too, but the slider needs the same answer before the
    write so the widget does not fight the device over a rounded value.
    """
    if cap.get("kind") != "range":
        values = cap.get("values") or [value]
        return min(values, key=lambda v: abs(v - value))
    lo, hi, step = cap["min"], cap["max"], max(1, cap["step"])
    value = max(lo, min(hi, value))
    return lo + round((value - lo) / step) * step


def mode_label(mode: int) -> str:
    return L.MODE_NAMES.get(mode, f"0x{mode:02x}").capitalize()


# -- lighting ---------------------------------------------------------------

@dataclass
class ZoneSetting:
    """One zone's desired lighting, independent of how it gets applied.

    The same value object drives the live 0x8070 write and the Led record
    stored in profile flash, so the two paths cannot drift apart.
    """
    mode: int = L.FIXED
    color: RGB = (0, 255, 255)
    period_ms: int = 4000
    intensity: int = 100
    waveform: str = "default"

    @property
    def wants_color(self) -> bool:
        return self.mode in (L.FIXED, L.BREATHING)

    @property
    def wants_period(self) -> bool:
        return self.mode in (L.BREATHING, L.CYCLE)

    @property
    def wants_waveform(self) -> bool:
        return self.mode == L.BREATHING

    def apply_live(self, leds, zone: int) -> None:
        """Write through 0x8070. Only takes effect in host mode."""
        r, g, b = self.color
        if self.mode == L.OFF:
            leds.off(zone)
        elif self.mode == L.FIXED:
            leds.fixed(zone, r, g, b)
        elif self.mode == L.BREATHING:
            leds.breathing(zone, r, g, b, self.period_ms, self.waveform,
                           self.intensity)
        elif self.mode == L.CYCLE:
            leds.cycle(zone, self.period_ms, self.intensity)
        else:
            raise ValueError(f"cannot apply mode 0x{self.mode:02x}")

    def led_record(self) -> pb.Led:
        """The 11-byte record for a profile sector."""
        r, g, b = self.color
        if self.mode == L.OFF:
            return pb.Led.off()
        if self.mode == L.FIXED:
            return pb.Led.fixed(r, g, b)
        if self.mode == L.BREATHING:
            return pb.Led.breathing(r, g, b, self.period_ms,
                                    L.WAVEFORMS.get(self.waveform, 0),
                                    self.intensity)
        if self.mode == L.CYCLE:
            return pb.Led.cycle(self.period_ms, self.intensity)
        raise ValueError(f"cannot store mode 0x{self.mode:02x}")

    @classmethod
    def from_led(cls, led: pb.Led) -> "ZoneSetting":
        """Recover a setting from a stored record, so the UI opens showing
        what the device is actually doing rather than a default."""
        raw = led.raw
        m = raw[0]
        if m == L.FIXED:
            return cls(L.FIXED, (raw[1], raw[2], raw[3]))
        if m == L.BREATHING:
            inv = {v: k for k, v in L.WAVEFORMS.items()}
            return cls(L.BREATHING, (raw[1], raw[2], raw[3]),
                       (raw[4] << 8) | raw[5], raw[7] or 100,
                       inv.get(raw[6], "default"))
        if m == L.CYCLE:
            return cls(L.CYCLE, (0, 255, 255), (raw[6] << 8) | raw[7],
                       raw[8] or 100)
        return cls(L.OFF)


# -- buttons ----------------------------------------------------------------

BUTTON_KINDS = ("mouse", "special", "key", "disabled")
KIND_LABELS = {"mouse": "Mouse button", "special": "Special action",
               "key": "Keyboard key", "disabled": "Disabled"}


@dataclass
class ButtonSpec:
    kind: str = "mouse"
    mouse_n: int = 1
    special: str = "dpi-up"
    key_code: int = 0x04
    modifiers: int = 0

    def to_button(self) -> pb.Button:
        if self.kind == "mouse":
            return pb.Button.mouse(self.mouse_n)
        if self.kind == "special":
            return pb.Button.special(self.special)
        if self.kind == "key":
            return pb.Button.key(self.key_code, self.modifiers)
        if self.kind == "disabled":
            return pb.Button.disabled()
        raise ValueError(f"unknown button kind {self.kind!r}")

    @classmethod
    def from_button(cls, button: pb.Button) -> "ButtonSpec":
        raw, kind = button.raw, button.kind
        if kind == "mouse":
            mask = (raw[2] << 8) | raw[3]
            n = mask.bit_length() if mask and not (mask & (mask - 1)) else 1
            return cls("mouse", mouse_n=n)
        if kind == "special":
            return cls("special", special=pb.SPECIALS.get(raw[1], "dpi-up"))
        if kind == "key":
            return cls("key", key_code=raw[3], modifiers=raw[2])
        return cls("disabled")


# -- appwatch rules ---------------------------------------------------------

@dataclass
class Rule:
    """One config.json rule, in the shape the editor dialog works with."""
    match: list[str] = field(default_factory=list)
    profile: int = 0
    dpi: int | None = None
    comment: str = ""

    @classmethod
    def from_dict(cls, d: dict) -> "Rule":
        raw = d.get("match") or []
        if isinstance(raw, str):
            raw = [raw]
        dpi = d.get("dpi")
        return cls([str(m) for m in raw], int(d.get("profile", 0)),
                   int(dpi) if dpi else None, str(d.get("comment", "")))

    def to_dict(self) -> dict:
        out: dict = {"match": list(self.match), "profile": self.profile}
        if self.dpi:
            out["dpi"] = self.dpi
        if self.comment:
            out["comment"] = self.comment
        return out

    def summary(self, profile_name: str | None = None) -> str:
        target = profile_name or f"profile {self.profile}"
        extra = f", DPI {self.dpi}" if self.dpi else ""
        return f"{target}{extra}"

    @property
    def title(self) -> str:
        return ", ".join(self.match) if self.match else "(no process names)"


def parse_matches(text: str) -> list[str]:
    """Split the editor's comma/whitespace separated process-name field."""
    out = []
    for chunk in text.replace("\n", ",").replace(" ", ",").split(","):
        name = chunk.strip().lower()
        if name and name not in out:
            out.append(name)
    return out


def rules_from_config(cfg: dict) -> list[Rule]:
    return [Rule.from_dict(r) for r in cfg.get("rules", [])
            if isinstance(r, dict)]


def rules_into_config(cfg: dict, rules: list[Rule]) -> dict:
    out = dict(cfg)
    out["rules"] = [r.to_dict() for r in rules]
    return out


# -- profile naming ---------------------------------------------------------

# 48 bytes of UTF-16LE. Non-BMP characters take two code units, so measure
# the encoding rather than len(str) or the device truncates mid-character.
NAME_MAX_BYTES = pb.NAME_LEN


def name_fits(name: str) -> bool:
    return len(name.encode("utf-16-le")) <= NAME_MAX_BYTES


def truncate_name(name: str) -> str:
    while name and not name_fits(name):
        name = name[:-1]
    return name


def profile_label(index: int, name: str) -> str:
    return name.strip() or f"Profile {index + 1}"
