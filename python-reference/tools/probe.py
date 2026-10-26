#!/usr/bin/env python3
"""Enumerate everything a connected G502 reports about itself. Read-only."""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from g502 import features as F
from g502.discover import open_device
from g502.hidpp import HidppError, HidppTimeout

EFFECT_NAMES = {
    0x00: "off / disabled",
    0x01: "fixed (solid colour)",
    0x02: "pulsing / breathing",
    0x03: "cycling (rainbow)",
    0x04: "colour wave",
    0x05: "starlight",
    0x06: "light on press",
    0x07: "audio visualiser",
    0x08: "boot-up",
    0x09: "demo",
    0x0A: "breathing (alt)",
    0x0B: "ripple",
}
ZONE_LOC = {
    0x0000: "unknown", 0x0001: "primary / logo", 0x0002: "logo",
    0x0003: "left side", 0x0004: "right side", 0x0005: "combined",
    0x0006: "primary-1", 0x0007: "primary-2", 0x0008: "DPI indicator",
    0x0009: "edge", 0x000A: "scroll wheel",
}

dev = open_device()
print(f"device   : {dev.model}  (046d:{dev.pid:04x})")
print(f"endpoint : {dev.path}")
maj, mnr = dev.protocol_version()
print(f"HID++    : {maj}.{mnr}")

# ---- feature table -------------------------------------------------------
fs = dev.feature_index(F.FEATURE_SET)
count = dev.call(fs, 0x00)[0]
print(f"\n=== feature table ({count} + root) ===")
print(f"{'idx':>4}  {'id':>6}  {'flags':<22} name")
print("-" * 68)
present = {}
for i in range(count + 1):
    try:
        p = dev.call(fs, 0x01, bytes([i]))
    except (HidppError, HidppTimeout) as e:
        print(f"{i:4d}  {'?':>6}  {e}")
        continue
    fid = (p[0] << 8) | p[1]
    print(f"{i:4d}  0x{fid:04x}  {F.type_flags(p[2]):<22} {F.name(fid)}")
    present[fid] = i

# ---- firmware ------------------------------------------------------------
if F.DEVICE_FW_VERSION in present:
    fw = present[F.DEVICE_FW_VERSION]
    n = dev.call(fw, 0x00)[0]
    print(f"\n=== firmware ({n} entities) ===")
    for e in range(n):
        try:
            p = dev.call(fw, 0x01, bytes([e]))
        except (HidppError, HidppTimeout) as ex:
            print(f"  entity {e}: {ex}")
            continue
        kind = {0: "main app", 1: "bootloader", 2: "hardware", 3: "touchpad"}.get(p[0] & 0x0F, f"type{p[0]}")
        name = bytes(p[1:4]).decode("ascii", "replace")
        print(f"  {kind:<11} {name} v{p[4]:02x}.{p[5]:02x} build {(p[6]<<8)|p[7]:04x}")

# ---- RGB effect engine ---------------------------------------------------
if F.COLOR_LED_EFFECTS in present:
    led = present[F.COLOR_LED_EFFECTS]
    info = dev.call(led, 0x00)
    zones = info[0]
    print(f"\n=== 0x8070 COLOR_LED_EFFECTS ===")
    print(f"  getInfo raw   : {info[:8].hex(' ')}")
    print(f"  zone count    : {zones}")
    for z in range(zones):
        zi = dev.call(led, 0x01, bytes([z]))
        loc = (zi[1] << 8) | zi[2]
        n_eff = zi[3]
        print(f"\n  zone {z}: location=0x{loc:04x} ({ZONE_LOC.get(loc,'?')})  "
              f"effects={n_eff}   raw={zi[:6].hex(' ')}")
        for e in range(n_eff):
            try:
                ei = dev.call(led, 0x02, bytes([z, e]))
            except (HidppError, HidppTimeout) as ex:
                print(f"    effect {e}: {ex}")
                continue
            eid = (ei[2] << 8) | ei[3]
            print(f"    effect {e}: id=0x{eid:04x}  "
                  f"{EFFECT_NAMES.get(eid, 'UNKNOWN'):<22} caps={ei[4:10].hex(' ')}")

# ---- DPI ----------------------------------------------------------------
if F.ADJUSTABLE_DPI in present:
    d = present[F.ADJUSTABLE_DPI]
    nsensors = dev.call(d, 0x00)[0]
    print(f"\n=== 0x2201 ADJUSTABLE_DPI ({nsensors} sensor) ===")
    for s in range(nsensors):
        p = dev.call(d, 0x01, bytes([s]))
        print(f"  sensor {s} dpi list raw: {p[1:14].hex(' ')}")
        vals, i = [], 1
        while i + 1 < 14:
            v = (p[i] << 8) | p[i+1]
            if v == 0: break
            vals.append(v); i += 2
        # 0xe000 marks a range step rather than a discrete list
        print(f"  decoded: {vals}")
        cur = dev.call(d, 0x02, bytes([s]))
        print(f"  current dpi: {(cur[1]<<8)|cur[2]}")

# ---- report rate --------------------------------------------------------
if F.REPORT_RATE in present:
    r = present[F.REPORT_RATE]
    p = dev.call(r, 0x00)
    mask = p[1]
    rates = [1000 // (1 << b) for b in range(8) if mask & (1 << b)]
    print(f"\n=== 0x8060 REPORT_RATE ===")
    print(f"  supported mask=0x{mask:02x} -> {sorted(rates)} Hz")
    print(f"  current: {1000 // (1 << (dev.call(r, 0x01)[0] - 1)) if dev.call(r,0x01)[0] else '?'} Hz"
          f"   (raw {dev.call(r, 0x01)[0]})")

# ---- onboard profiles ---------------------------------------------------
if F.ONBOARD_PROFILES in present:
    o = present[F.ONBOARD_PROFILES]
    p = dev.call(o, 0x00)
    print(f"\n=== 0x8100 ONBOARD_PROFILES ===")
    print(f"  getInfo raw: {p[:10].hex(' ')}")
    print(f"  memory model={p[0]} profile fmt={p[1]} macro fmt={p[2]} "
          f"profile count={p[3]} (+{p[4]} ROM) buttons={p[5]} sectors={p[6]}")
    mode = dev.call(o, 0x01)[0]
    print(f"  mode: {mode} ({'onboard' if mode==1 else 'host/software' if mode==2 else '?'})")
    cur = dev.call(o, 0x05)
    print(f"  active profile raw: {cur[:4].hex(' ')}")

dev.close()
print("\n(read-only probe; nothing was written)")
