"""Locate the HID++ control endpoint for a supported Logitech device."""
from __future__ import annotations

import glob
import os

from .hidpp import Device, HidppError, HidppTimeout

VENDOR_LOGITECH = 0x046D

# Devices this project drives. PID -> friendly name.
SUPPORTED = {
    0xC08B: "G502 HERO / SE HERO Gaming Mouse",
    0xC08D: "G502 LIGHTSPEED (wired mode)",
    0xC332: "G502 Proteus Spectrum",
    0xC07D: "G502 Proteus Core",
}


def _sysfs_ids(hidraw: str) -> tuple[int, int] | None:
    """Read idVendor/idProduct for the USB device behind a hidraw node."""
    dev = os.path.realpath(f"/sys/class/hidraw/{hidraw}/device")
    cur = dev
    for _ in range(6):  # walk up to the usb_device that carries the id files
        cur = os.path.dirname(cur)
        v = os.path.join(cur, "idVendor")
        p = os.path.join(cur, "idProduct")
        if os.path.exists(v) and os.path.exists(p):
            try:
                return int(open(v).read().strip(), 16), int(open(p).read().strip(), 16)
            except (OSError, ValueError):
                return None
    return None


def candidates() -> list[tuple[str, int, int]]:
    """Every hidraw node belonging to a supported Logitech device."""
    out = []
    for path in sorted(glob.glob("/sys/class/hidraw/hidraw*")):
        node = os.path.basename(path)
        ids = _sysfs_ids(node)
        if not ids:
            continue
        vid, pid = ids
        if vid == VENDOR_LOGITECH and pid in SUPPORTED:
            out.append((f"/dev/{node}", vid, pid))
    return out


def open_device() -> Device:
    """Open the first hidraw node that actually answers HID++.

    A G502 exposes two interfaces; only the one carrying the ff00 vendor
    collection responds to the root feature, so probe rather than guess.
    """
    found = candidates()
    if not found:
        raise RuntimeError(
            "no supported Logitech device found. Is it plugged in? "
            "(looked for " + ", ".join(f"046d:{p:04x}" for p in SUPPORTED) + ")"
        )
    errors = []
    for path, vid, pid in found:
        try:
            dev = Device(path)
        except PermissionError:
            errors.append(f"{path}: permission denied (need root or a udev rule)")
            continue
        try:
            dev.protocol_version()  # does it speak HID++ at all?
            dev.pid = pid
            dev.model = SUPPORTED[pid]
            return dev
        except (HidppError, HidppTimeout, OSError) as e:
            errors.append(f"{path}: {e}")
            dev.close()
    raise RuntimeError("found the device but no HID++ endpoint answered:\n  " +
                       "\n  ".join(errors))
