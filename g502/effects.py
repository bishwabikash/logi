"""Host-side lighting animation.

The firmware on this device implements only off/fixed/breathing/cycle, and
notably *not* CUSTOM (0x0C) direct-frame streaming. So anything richer than
those four is produced here: we hold the device in host mode and push FIXED
colours frame by frame. That is also how Logitech's own software does its
"software mode" effects.

Frame rate is bounded by HID++ round trips. Writes go out via send_nowait and
replies are drained periodically, which lifts throughput from roughly 20 fps
to the 60+ range.
"""
from __future__ import annotations

import colorsys
import math
import shutil
import subprocess
import time
from typing import Callable, Iterator

from .led import Leds
from .profiles import OnboardProfiles

Frame = list[tuple[int, int, int]]  # one colour per zone


def _clamp8(v: float) -> int:
    return 0 if v < 0 else 255 if v > 255 else int(v)


class Engine:
    """Drives an effect generator at a target frame rate."""

    def __init__(self, dev, fps: int = 60):
        self.dev = dev
        self.leds = Leds(dev)
        self.profiles = OnboardProfiles(dev)
        self.fps = max(1, min(fps, 120))
        self.zone_count = self.leds.zone_count

    def run(self, effect: Iterator[Frame], duration: float | None = None) -> int:
        """Play frames until the generator ends, duration elapses, or Ctrl-C.

        Always restores the previous device mode on the way out, so an
        interrupted animation cannot leave the mouse dependent on this
        process.
        """
        interval = 1.0 / self.fps
        frames = 0
        deadline = None if duration is None else time.monotonic() + duration
        with self.profiles.host_mode():
            try:
                for frame in effect:
                    for z in range(self.zone_count):
                        r, g, b = frame[z % len(frame)]
                        self.leds.fixed(z, r, g, b, nowait=True)
                    frames += 1
                    if frames % 32 == 0:
                        self.leds.dev.flush()  # drop accumulated replies
                    if deadline and time.monotonic() >= deadline:
                        break
                    time.sleep(interval)
            except KeyboardInterrupt:
                pass
        return frames


# -- effect generators -----------------------------------------------------

def solid(color: tuple[int, int, int]) -> Iterator[Frame]:
    while True:
        yield [color]


def rainbow(period: float = 5.0, zone_offset: float = 0.25,
            zones: int = 2) -> Iterator[Frame]:
    """Hue sweep; zone_offset staggers the zones so it reads as motion."""
    t0 = time.monotonic()
    while True:
        base = ((time.monotonic() - t0) / period) % 1.0
        yield [tuple(_clamp8(c * 255) for c in
                     colorsys.hsv_to_rgb((base + i * zone_offset) % 1.0, 1, 1))
               for i in range(zones)]


def breathe(color: tuple[int, int, int], period: float = 4.0,
            curve: str = "sine", floor: float = 0.05) -> Iterator[Frame]:
    """Host-side breathing, so the curve isn't limited to the firmware's set."""
    r, g, b = color
    t0 = time.monotonic()
    shapes: dict[str, Callable[[float], float]] = {
        "sine": lambda p: (math.sin(p * math.tau - math.pi / 2) + 1) / 2,
        "triangle": lambda p: 1 - abs(2 * p - 1),
        "sawtooth": lambda p: p,
        "square": lambda p: 1.0 if p < 0.5 else 0.0,
        "exponential": lambda p: (math.exp(3 * (1 - abs(2 * p - 1))) - 1) / (math.exp(3) - 1),
    }
    shape = shapes.get(curve, shapes["sine"])
    while True:
        p = ((time.monotonic() - t0) / period) % 1.0
        k = floor + (1 - floor) * shape(p)
        yield [(_clamp8(r * k), _clamp8(g * k), _clamp8(b * k))]


def gradient(a: tuple[int, int, int], b: tuple[int, int, int],
             period: float = 3.0) -> Iterator[Frame]:
    t0 = time.monotonic()
    while True:
        p = ((time.monotonic() - t0) / period) % 1.0
        k = (math.sin(p * math.tau - math.pi / 2) + 1) / 2
        yield [tuple(_clamp8(a[i] + (b[i] - a[i]) * k) for i in range(3))]


def wave(color: tuple[int, int, int], period: float = 1.5,
         zones: int = 2) -> Iterator[Frame]:
    """Brightness travelling across the zones."""
    t0 = time.monotonic()
    while True:
        p = ((time.monotonic() - t0) / period) % 1.0
        out = []
        for i in range(zones):
            phase = (p - i / max(1, zones)) % 1.0
            k = max(0.0, 1 - abs(2 * phase - 1) * 2)
            out.append(tuple(_clamp8(c * (0.08 + 0.92 * k)) for c in color))
        yield out


def audio(color: tuple[int, int, int] = (0, 255, 128),
          source: str | None = None, zones: int = 2,
          sensitivity: float = 2.5) -> Iterator[Frame]:
    """Level meter driven by the default output monitor.

    This is the equivalent of the vendor software's audio visualiser. It
    reads a PulseAudio/PipeWire monitor source through parec; if parec is
    missing the generator refuses rather than silently idling.
    """
    if not shutil.which("parec"):
        raise RuntimeError(
            "parec not found (install pulseaudio-utils) — audio effect needs "
            "a PulseAudio/PipeWire monitor source")
    src = source
    if src is None:
        try:
            default = subprocess.run(
                ["pactl", "get-default-sink"], capture_output=True,
                text=True, timeout=5).stdout.strip()
            src = f"{default}.monitor" if default else None
        except (OSError, subprocess.SubprocessError):
            src = None
    cmd = ["parec", "--format=s16le", "--rate=22050", "--channels=1"]
    if src:
        cmd += ["-d", src]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL)
    chunk = 1024  # 512 samples
    smoothed = 0.0
    try:
        while True:
            raw = proc.stdout.read(chunk)
            if not raw or len(raw) < chunk:
                break
            peak = 0
            for i in range(0, len(raw), 64):  # decimate; we only need a level
                v = int.from_bytes(raw[i:i + 2], "little", signed=True)
                peak = max(peak, abs(v))
            level = min(1.0, (peak / 32768.0) * sensitivity)
            smoothed = max(level, smoothed * 0.82)  # fast attack, slow decay
            out = []
            for i in range(zones):
                k = max(0.04, smoothed - i * 0.12)
                out.append(tuple(_clamp8(c * k) for c in color))
            yield out
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            proc.kill()


EFFECTS = {
    "solid": solid, "rainbow": rainbow, "breathe": breathe,
    "gradient": gradient, "wave": wave, "audio": audio,
}
