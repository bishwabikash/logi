"""HID keyboard usage codes for the button-rebinding dialog.

These are USB HID Usage Table 0x07 values, not device-specific — the profile
stores the usage code verbatim, so the same table serves any keyboard
binding. Only the keys worth binding to a mouse button are listed; the dialog
also takes a raw code for anything missing.
"""
from __future__ import annotations

# Modifier bits in the profile's modifier byte (HID boot-protocol order).
MODIFIERS = (
    ("Ctrl", 0x01), ("Shift", 0x02), ("Alt", 0x04), ("Super", 0x08),
    ("Right Ctrl", 0x10), ("Right Shift", 0x20), ("Right Alt", 0x40),
    ("Right Super", 0x80),
)

_KEYS: list[tuple[str, int]] = [
    ("Enter", 0x28), ("Escape", 0x29), ("Backspace", 0x2A), ("Tab", 0x2B),
    ("Space", 0x2C), ("Minus", 0x2D), ("Equal", 0x2E),
    ("Left bracket", 0x2F), ("Right bracket", 0x30), ("Backslash", 0x31),
    ("Semicolon", 0x33), ("Apostrophe", 0x34), ("Grave", 0x35),
    ("Comma", 0x36), ("Period", 0x37), ("Slash", 0x38), ("Caps Lock", 0x39),
    ("Print Screen", 0x46), ("Scroll Lock", 0x47), ("Pause", 0x48),
    ("Insert", 0x49), ("Home", 0x4A), ("Page Up", 0x4B), ("Delete", 0x4C),
    ("End", 0x4D), ("Page Down", 0x4E),
    ("Right", 0x4F), ("Left", 0x50), ("Down", 0x51), ("Up", 0x52),
    ("Num Lock", 0x53), ("Menu", 0x65),
]
_KEYS += [(chr(ord("A") + i), 0x04 + i) for i in range(26)]
_KEYS += [(str(d), 0x1E + d - 1) for d in range(1, 10)] + [("0", 0x27)]
_KEYS += [(f"F{n}", 0x3A + n - 1) for n in range(1, 13)]
_KEYS += [(f"F{n}", 0x68 + n - 13) for n in range(13, 25)]
_KEYS += [(f"Keypad {d}", 0x59 + d - 1) for d in range(1, 10)]
_KEYS += [("Keypad 0", 0x62), ("Keypad Enter", 0x58), ("Keypad Plus", 0x57),
          ("Keypad Minus", 0x56), ("Keypad Multiply", 0x55),
          ("Keypad Divide", 0x54), ("Keypad Period", 0x63)]

# Sorted by code so the dropdown order is stable and predictable.
KEYS: tuple[tuple[str, int], ...] = tuple(sorted(set(_KEYS), key=lambda kv: kv[1]))
BY_CODE = {code: name for name, code in KEYS}


def key_name(code: int) -> str:
    return BY_CODE.get(code, f"usage 0x{code:02x}")


def modifier_label(mask: int) -> str:
    return "+".join(name for name, bit in MODIFIERS if mask & bit)


def describe(code: int, modifiers: int = 0) -> str:
    mods = modifier_label(modifiers)
    return f"{mods}+{key_name(code)}" if mods else key_name(code)


def modifier_mask(names: list[str]) -> int:
    lookup = {n: b for n, b in MODIFIERS}
    return sum(lookup.get(n, 0) for n in names)
