"""Host-side lighting animation — the equivalent of G HUB's "software mode".

The firmware implements only off/fixed/cycle/breathing and has no frame
streaming mode (CUSTOM 0x0c is absent), so anything richer than those four is
generated here: hold the device in host mode and push FIXED colours frame by
frame. Live 0x8070 writes are inert in onboard mode, which is why every run is
wrapped in OnboardProfiles.host_mode().

Two layers:

  * Effect.tick(t) returns one RGB per zone, t being seconds since the effect
    started. The arithmetic effects are pure functions of t, so they test
    without a device; the three sampling effects (dpi, audio, screen) take an
    injectable source callable for the same reason.
  * EffectRunner owns the device: host mode, frame pacing, periodic draining
    of the replies send_nowait leaves queued, and restore on signal, exception
    or normal exit.

Throughput is bounded by the HID++ control endpoint. Blocking writes measured
~19 fps; send_nowait removes the round trip and lifts that well past the 30 fps
default. MAX_FPS caps requests at 120 because each frame costs one write per
zone and send_nowait drops a frame when the kernel queue is full — asking for
more than the endpoint can carry just loses writes silently.
"""
from __future__ import annotations

import colorsys
import contextlib
import math
import os
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from array import array
from dataclasses import dataclass, field
from typing import Callable, Iterable, Iterator

from .hidpp import HidppError, HidppTimeout
from .led import Leds
from .profiles import OnboardProfiles

Frame = list[tuple[int, int, int]]  # one colour per zone

ZONES = 2            # hardware fact: zone 0 primary/logo, zone 1 DPI indicator
DEFAULT_FPS = 30
MAX_FPS = 120        # see module docstring
FLUSH_EVERY = 16     # frames between Device.flush() calls


# -- colour helpers --------------------------------------------------------

def _clamp8(v: float) -> int:
    return 0 if v < 0 else 255 if v > 255 else int(v)


def _rgb(c: Iterable[float]) -> tuple[int, int, int]:
    r, g, b = c
    return _clamp8(r), _clamp8(g), _clamp8(b)


def _scale(c: tuple[int, int, int], k: float) -> tuple[int, int, int]:
    return _clamp8(c[0] * k), _clamp8(c[1] * k), _clamp8(c[2] * k)


def _lerp(a: tuple[int, int, int], b: tuple[int, int, int],
          f: float) -> tuple[int, int, int]:
    f = 0.0 if f < 0 else 1.0 if f > 1 else f
    return _rgb(a[i] + (b[i] - a[i]) * f for i in range(3))


def _hsv(h: float, s: float = 1.0, v: float = 1.0) -> tuple[int, int, int]:
    return _rgb(x * 255 for x in colorsys.hsv_to_rgb(h % 1.0, s, v))


def boost_saturation(c: tuple[int, int, int],
                     factor: float) -> tuple[int, int, int]:
    """Push a colour's saturation. Screen averages are washed out; the LEDs
    read better with the hue exaggerated."""
    h, s, v = colorsys.rgb_to_hsv(c[0] / 255, c[1] / 255, c[2] / 255)
    return _hsv(h, min(1.0, s * factor), v)


# Brightness shapes for Breathe. Each maps phase 0..1 to 0..1. Named to match
# the firmware waveform list where the shapes coincide, so --waveform can be
# passed through to either path; unknown names fall back to sine rather than
# failing, because that list has shapes we do not model.
CURVES: dict[str, Callable[[float], float]] = {
    "sine": lambda p: (math.sin(p * math.tau - math.pi / 2) + 1) / 2,
    "default": lambda p: (math.sin(p * math.tau - math.pi / 2) + 1) / 2,
    "triangle": lambda p: 1 - abs(2 * p - 1),
    "sawtooth": lambda p: p,
    "square": lambda p: 1.0 if p < 0.5 else 0.0,
    "exponential": lambda p: (math.exp(3 * (1 - abs(2 * p - 1))) - 1)
                             / (math.exp(3) - 1),
    # fast rise, slow fall — the firmware's "sharkfin", mirrored
    "sharkfin": lambda p: math.sin(p * math.pi / 2) if p < 0.5
                          else 1 - (2 * p - 1) ** 0.5,
}


def curve(name: str) -> Callable[[float], float]:
    return CURVES.get(str(name).lower(), CURVES["sine"])


# -- effect base -----------------------------------------------------------

class Effect:
    """One animation. Subclasses implement tick()."""

    name = "effect"

    def __init__(self, zones: int = ZONES):
        self.zones = max(1, int(zones))

    def tick(self, t: float) -> Frame:
        """Colours for time t (seconds since start), one entry per zone."""
        raise NotImplementedError

    def bind(self, runner: "EffectRunner") -> None:
        """Adopt the device's real zone count and open any sampling source.

        The runner calls this once before the first frame. Sources are opened
        here, not in __init__, so constructing an effect — or listing the
        registry — never spawns a process or touches the device.
        """
        self.zones = runner.zone_count

    def close(self) -> None:
        """Release any source. Called by the runner; must be idempotent."""

    def __iter__(self) -> Iterator[Frame]:
        # An effect is also a plain frame iterator, which is the shape the
        # older generator-style entry points handed to the runner.
        t0 = time.monotonic()
        while True:
            yield self.tick(time.monotonic() - t0)


# -- pure effects ----------------------------------------------------------

class Static(Effect):
    name = "static"

    def __init__(self, color: tuple[int, int, int] = (0, 255, 255),
                 zones: int = ZONES):
        super().__init__(zones)
        self.color = _rgb(color)

    def tick(self, t: float) -> Frame:
        return [self.color] * self.zones


class Breathe(Effect):
    """Host-side breathing, so the shape is not limited to the firmware's
    waveform set and the floor can be non-zero."""

    name = "breathe"

    def __init__(self, color: tuple[int, int, int] = (0, 255, 255),
                 period: float = 4.0, curve_name: str = "sine",
                 floor: float = 0.05, zones: int = ZONES):
        super().__init__(zones)
        self.color = _rgb(color)
        self.period = max(1e-3, float(period))
        self.curve_name = curve_name
        self.shape = curve(curve_name)
        self.floor = min(1.0, max(0.0, float(floor)))

    def tick(self, t: float) -> Frame:
        p = (t / self.period) % 1.0
        k = self.floor + (1 - self.floor) * self.shape(p)
        return [_scale(self.color, k)] * self.zones


class Pulse(Effect):
    """Sharp attack, exponential decay — reads as a heartbeat rather than
    breathing's symmetric swell."""

    name = "pulse"

    def __init__(self, color: tuple[int, int, int] = (255, 0, 102),
                 period: float = 1.0, floor: float = 0.04,
                 decay: float = 5.0, zones: int = ZONES):
        super().__init__(zones)
        self.color = _rgb(color)
        self.period = max(1e-3, float(period))
        self.floor = min(1.0, max(0.0, float(floor)))
        self.decay = max(0.0, float(decay))

    def tick(self, t: float) -> Frame:
        p = (t / self.period) % 1.0
        k = self.floor + (1 - self.floor) * math.exp(-self.decay * p)
        return [_scale(self.color, k)] * self.zones


class Rainbow(Effect):
    """Hue sweep. zone_offset staggers the zones so it reads as motion
    instead of two lamps changing in lockstep."""

    name = "rainbow"

    def __init__(self, period: float = 5.0, zone_offset: float = 0.25,
                 zones: int = ZONES, saturation: float = 1.0,
                 value: float = 1.0):
        super().__init__(zones)
        self.period = max(1e-3, float(period))
        self.zone_offset = float(zone_offset)
        self.saturation = min(1.0, max(0.0, float(saturation)))
        self.value = min(1.0, max(0.0, float(value)))

    def tick(self, t: float) -> Frame:
        base = (t / self.period) % 1.0
        return [_hsv(base + i * self.zone_offset, self.saturation, self.value)
                for i in range(self.zones)]


class Gradient(Effect):
    """Colour-cycle through a palette. `period` is one full loop of the ring,
    so a two-colour palette runs a -> b -> a in `period` seconds."""

    name = "gradient"

    def __init__(self, palette: list[tuple[int, int, int]] | None = None,
                 period: float = 3.0, zones: int = ZONES,
                 smooth: bool = True):
        super().__init__(zones)
        self.palette = [_rgb(c) for c in (palette or [(0, 255, 255),
                                                      (255, 0, 255)])]
        if not self.palette:
            raise ValueError("gradient needs at least one colour")
        self.period = max(1e-3, float(period))
        self.smooth = bool(smooth)

    def tick(self, t: float) -> Frame:
        n = len(self.palette)
        seg = ((t / self.period) % 1.0) * n
        i = int(seg) % n
        f = seg - math.floor(seg)
        if self.smooth:
            f = f * f * (3 - 2 * f)  # smoothstep: ease the handover
        return [_lerp(self.palette[i], self.palette[(i + 1) % n], f)] \
            * self.zones


class Wave(Effect):
    """Brightness travelling across the zones. Zone i is phase-offset by
    i/zones, which on this two-zone device reads as logo -> DPI indicator."""

    name = "wave"

    def __init__(self, color: tuple[int, int, int] = (0, 255, 136),
                 period: float = 1.5, zones: int = ZONES,
                 floor: float = 0.08, width: float = 0.5):
        super().__init__(zones)
        self.color = _rgb(color)
        self.period = max(1e-3, float(period))
        self.floor = min(1.0, max(0.0, float(floor)))
        self.width = min(1.0, max(1e-3, float(width)))

    def tick(self, t: float) -> Frame:
        p = (t / self.period) % 1.0
        out: Frame = []
        for i in range(self.zones):
            phase = (p - i / self.zones) % 1.0
            crest = max(0.0, 1.0 - abs(2 * phase - 1) / self.width)
            out.append(_scale(self.color,
                              self.floor + (1 - self.floor) * crest))
        return out


# -- DPI reactive ----------------------------------------------------------

# Stops are interpolated in log space: DPI is a continuous 100..25600 range on
# this sensor and doubling it is what feels like "one step", so a linear ramp
# would leave the whole low end looking the same colour.
DPI_STOPS: list[tuple[int, tuple[int, int, int]]] = [
    (400, (0, 64, 255)),
    (800, (0, 255, 255)),
    (1600, (0, 255, 64)),
    (3200, (255, 255, 0)),
    (6400, (255, 102, 0)),
    (12800, (255, 0, 0)),
    (25600, (255, 0, 255)),
]


def dpi_color(dpi: float,
              stops: list[tuple[int, tuple[int, int, int]]] | None = None
              ) -> tuple[int, int, int]:
    """Colour for a DPI value, clamped to the ends of the stop list."""
    pts = sorted(stops or DPI_STOPS)
    if dpi <= pts[0][0]:
        return _rgb(pts[0][1])
    if dpi >= pts[-1][0]:
        return _rgb(pts[-1][1])
    for (lo, lo_c), (hi, hi_c) in zip(pts, pts[1:]):
        if lo <= dpi <= hi:
            f = (math.log(dpi) - math.log(lo)) / (math.log(hi) - math.log(lo))
            return _lerp(_rgb(lo_c), _rgb(hi_c), f)
    return _rgb(pts[-1][1])  # unreachable for a sorted list


class DpiReactive(Effect):
    """Colour tracks the active DPI.

    The device is polled at `poll` seconds rather than every frame: each read
    is a blocking HID++ round trip and 30 of them a second would eat the
    budget the animation needs. Changes cross-fade so a DPI tap is visible as
    a sweep instead of a jump.
    """

    name = "dpi"

    def __init__(self, read_dpi: Callable[[], int] | None = None,
                 stops: list[tuple[int, tuple[int, int, int]]] | None = None,
                 poll: float = 0.25, fade: float = 0.35,
                 zones: int = ZONES):
        super().__init__(zones)
        self._read = read_dpi
        self.stops = stops or DPI_STOPS
        self.poll = max(0.0, float(poll))
        self.fade = max(0.0, float(fade))
        self.dpi: int | None = None
        self._from = (0, 0, 0)
        self._to = (0, 0, 0)
        self._fade_start = 0.0
        self._last_poll: float | None = None

    def bind(self, runner: "EffectRunner") -> None:
        super().bind(runner)
        if self._read is None:
            from .dpi import Dpi  # imported here: constructing must be inert

            sensor = Dpi(runner.dev)
            self._read = sensor.get

    def _current(self, t: float) -> tuple[int, int, int]:
        if self.fade <= 0:
            return self._to
        f = (t - self._fade_start) / self.fade
        return _lerp(self._from, self._to, f)

    def tick(self, t: float) -> Frame:
        if self._read is not None and (self._last_poll is None
                                       or t - self._last_poll >= self.poll):
            self._last_poll = t
            try:
                dpi = int(self._read())
            except (HidppError, HidppTimeout, OSError, ValueError, TypeError):
                dpi = self.dpi  # keep the last good reading rather than blink
            if dpi is not None and dpi != self.dpi:
                target = dpi_color(dpi, self.stops)
                # first reading lands directly; later ones fade from whatever
                # is on screen right now
                self._from = target if self.dpi is None else self._current(t)
                self._to = target
                self._fade_start = t
                self.dpi = dpi
        return [self._current(t)] * self.zones


# -- audio visualiser ------------------------------------------------------

AUDIO_RATE = 22050   # plenty for a level meter, and a small pipe
AUDIO_CHUNK = 8192   # bytes read per tick at most (~186 ms at the above rate)


def pcm_level(raw: bytes) -> tuple[float, float]:
    """(rms, balance) for signed 16-bit little-endian mono samples.

    rms is 0..1 of full scale. balance is a crude high-frequency share from
    the mean first difference: 0 for DC/bass, 1 at Nyquist. Both are cheap
    enough to run per frame in pure Python, which an FFT would not be.
    """
    n = len(raw) // 2
    if n < 2:
        return 0.0, 0.0
    s = array("h")
    s.frombytes(raw[:n * 2])
    if array("h").itemsize != 2:  # not seen on CPython, but the cast is only
        return 0.0, 0.0           # valid when 'h' really is 16 bits
    import sys
    if sys.byteorder == "big":
        s.byteswap()  # the capture tools emit s16le regardless of host
    energy = 0
    lf = 0
    hf = 0
    prev = s[0]
    for v in s:
        energy += v * v
        lf += abs(v)
        hf += abs(v - prev)
        prev = v
    rms = min(1.0, math.sqrt(energy / n) / 32768.0)
    lf /= n
    hf /= 2 * n
    balance = 0.0 if lf + hf <= 0 else min(1.0, 2.0 * hf / (lf + hf))
    return rms, balance


def audio_frame(level: float, balance: float, zones: int = ZONES,
                base_hue: float = 0.5, hue_span: float = 0.4,
                floor: float = 0.03, taper: float = 0.15) -> Frame:
    """Level and spectral balance to colour: brightness follows level, hue
    slides from base_hue toward the treble end of the span."""
    level = 0.0 if level < 0 else 1.0 if level > 1 else level
    hue = base_hue + hue_span * (0.0 if balance < 0 else
                                 1.0 if balance > 1 else balance)
    out: Frame = []
    for i in range(max(1, zones)):
        v = floor + (1 - floor) * max(0.0, level - i * taper)
        out.append(_hsv(hue, 1.0, min(1.0, v)))
    return out


class _AudioCapture:
    """Raw s16le mono from the default output monitor via a capture tool.

    parec is preferred over pw-record because asking for the *monitor* of the
    default sink — what you can hear, not the microphone — is a documented
    device name there (@DEFAULT_MONITOR@), while pw-record needs a property
    blob. Reads are non-blocking: a frame with no new audio decays instead of
    stalling the loop.
    """

    def __init__(self, rate: int = AUDIO_RATE, source: str | None = None):
        self.rate = int(rate)
        self.source = source
        self.proc: subprocess.Popen | None = None
        self.cmd: list[str] = []

    def _command(self) -> list[str]:
        if shutil.which("parec"):
            return ["parec", "--format=s16le", f"--rate={self.rate}",
                    "--channels=1", "-d", self.source or "@DEFAULT_MONITOR@"]
        if shutil.which("pw-record"):
            cmd = ["pw-record", "--format=s16", f"--rate={self.rate}",
                   "--channels=1",
                   "--properties=stream.capture.sink=true", "-"]
            if self.source:
                cmd[-1:-1] = ["--target", self.source]
            return cmd
        raise RuntimeError(
            "no PipeWire/PulseAudio capture tool found: install "
            "pulseaudio-utils (parec) or pipewire-bin (pw-record)")

    def start(self) -> None:
        if self.proc is not None:
            return
        self.cmd = self._command()
        try:
            self.proc = subprocess.Popen(
                self.cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        except OSError as e:
            raise RuntimeError(f"cannot run {self.cmd[0]}: {e}") from None
        for stream in (self.proc.stdout, self.proc.stderr):
            os.set_blocking(stream.fileno(), False)

    def read(self) -> bytes:
        """Whatever the tool has produced since the last call, or b''."""
        if self.proc is None:
            self.start()
        assert self.proc is not None and self.proc.stdout is not None
        try:
            data = os.read(self.proc.stdout.fileno(), AUDIO_CHUNK)
        except BlockingIOError:
            return b""
        except OSError as e:
            raise RuntimeError(f"{self.cmd[0]} read failed: {e}") from None
        if not data and self.proc.poll() is not None:
            raise RuntimeError(
                f"{self.cmd[0]} exited with {self.proc.returncode}: "
                f"{self._stderr() or 'no output'}")
        return data

    def _stderr(self) -> str:
        if self.proc is None or self.proc.stderr is None:
            return ""
        try:
            return (os.read(self.proc.stderr.fileno(), 4096)
                    .decode("utf-8", "replace").strip())
        except (BlockingIOError, OSError):
            return ""

    def stop(self) -> None:
        if self.proc is None:
            return
        proc, self.proc = self.proc, None
        proc.terminate()
        try:
            proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            proc.kill()
        for stream in (proc.stdout, proc.stderr):
            with contextlib.suppress(OSError):
                if stream:
                    stream.close()


class AudioVisualiser(Effect):
    """Level meter driven by the default output monitor.

    With `color` set, that colour is dimmed by the level; with color=None the
    hue follows the spectral balance as well. Attack is instant and decay is
    exponential, which is what makes a meter look like it is following music
    rather than flickering.
    """

    name = "audio"

    def __init__(self, color: tuple[int, int, int] | None = None,
                 zones: int = ZONES, sensitivity: float = 2.5,
                 decay: float = 0.82, floor: float = 0.03,
                 taper: float = 0.15, source: str | None = None,
                 read_pcm: Callable[[], bytes] | None = None):
        super().__init__(zones)
        self.color = None if color is None else _rgb(color)
        self.sensitivity = max(0.0, float(sensitivity))
        self.decay = min(1.0, max(0.0, float(decay)))
        self.floor = min(1.0, max(0.0, float(floor)))
        self.taper = float(taper)
        self.level = 0.0
        self.balance = 0.0
        self._read = read_pcm
        self._cap = None if read_pcm else _AudioCapture(source=source)

    def bind(self, runner: "EffectRunner") -> None:
        super().bind(runner)
        self._ensure()

    def _ensure(self) -> None:
        if self._read is None and self._cap is not None:
            self._cap.start()           # raises if no capture tool exists
            self._read = self._cap.read

    def tick(self, t: float) -> Frame:
        self._ensure()
        raw = self._read() if self._read else b""
        if raw:
            rms, balance = pcm_level(raw)
            level = min(1.0, rms * self.sensitivity)
            self.level = max(level, self.level * self.decay)
            self.balance += (balance - self.balance) * 0.3
        else:
            self.level *= self.decay
        if self.color is None:
            return audio_frame(self.level, self.balance, self.zones,
                               floor=self.floor, taper=self.taper)
        return [_scale(self.color,
                       self.floor + (1 - self.floor)
                       * max(0.0, self.level - i * self.taper))
                for i in range(self.zones)]

    def close(self) -> None:
        if self._cap is not None:
            self._cap.stop()
            self._read = None


# -- screen sampler --------------------------------------------------------

def average_ppm(data: bytes) -> tuple[int, int, int]:
    """Mean colour of a binary PPM (P6).

    Parsed by hand rather than through an image library so the sampler stays
    dependency-free; PPM is why grim is asked for -t ppm in the first place.
    """
    if not data.startswith(b"P6"):
        raise ValueError("not a binary PPM (P6)")
    fields: list[int] = []
    pos = 2
    while len(fields) < 3:
        while pos < len(data) and data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b"#":           # comment to end of line
            while pos < len(data) and data[pos] != 0x0A:
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos:pos + 1].isspace():
            pos += 1
        if start == pos:
            raise ValueError("truncated PPM header")
        fields.append(int(data[start:pos]))
    pos += 1  # exactly one whitespace byte separates header from raster
    width, height, maxval = fields
    if maxval != 255:
        raise ValueError(f"only 8-bit PPM supported, maxval {maxval}")
    px = data[pos:pos + width * height * 3]
    n = len(px) // 3
    if n < 1:
        raise ValueError("empty PPM raster")
    step = max(1, n // 4096) * 3  # subsample: a mean needs no more than this
    r = g = b = count = 0
    for i in range(0, n * 3 - 2, step):
        r += px[i]
        g += px[i + 1]
        b += px[i + 2]
        count += 1
    return r // count, g // count, b // count


class _ScreenCapture:
    """Average screen colour, sampled on a background thread.

    grim is the Wayland tool of choice, but it speaks wlr-screencopy, which
    Mutter does not implement — so grim works on sway/Hyprland and fails on
    GNOME. The second backend covers GNOME by going through gnome-screenshot
    and letting ImageMagick do the downscale and the PNG decode.

    A capture costs tens of milliseconds, far too long to do inside the frame
    loop, hence the thread. The first sample is taken synchronously so a
    missing tool is reported to the caller instead of dying in a thread.
    """

    def __init__(self, rate: float = 2.0, scale: float = 0.02):
        self.interval = 1.0 / max(0.1, float(rate))
        self.scale = float(scale)
        self.color = (0, 0, 0)
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._backend: str | None = None

    def _pick_backend(self) -> str:
        if shutil.which("grim"):
            return "grim"
        if shutil.which("gnome-screenshot") and self._magick():
            return "gnome"
        raise RuntimeError(
            "no screen capture tool found: install grim (sway/Hyprland and "
            "other wlroots compositors) or gnome-screenshot plus imagemagick "
            "(GNOME, whose Mutter does not implement wlr-screencopy)")

    @staticmethod
    def _magick() -> str | None:
        return shutil.which("magick") or shutil.which("convert")

    def _grab(self) -> bytes:
        if self._backend == "grim":
            out = subprocess.run(
                ["grim", "-s", f"{self.scale:g}", "-t", "ppm", "-"],
                capture_output=True, timeout=5)
            if out.returncode != 0 or not out.stdout:
                raise RuntimeError(
                    "grim failed: "
                    + (out.stderr.decode("utf-8", "replace").strip()
                       or "no output (a wlroots compositor is required)"))
            return out.stdout
        magick = self._magick()
        if not magick:
            raise RuntimeError("imagemagick disappeared mid-run")
        fd, path = tempfile.mkstemp(suffix=".png", prefix="g502-screen-")
        os.close(fd)
        try:
            shot = subprocess.run(["gnome-screenshot", "-f", path],
                                  capture_output=True, timeout=10)
            if shot.returncode != 0:
                raise RuntimeError(
                    "gnome-screenshot failed: "
                    + (shot.stderr.decode("utf-8", "replace").strip()
                       or "no output"))
            conv = subprocess.run([magick, path, "-resize", "32x18!",
                                   "ppm:-"], capture_output=True, timeout=10)
            if conv.returncode != 0 or not conv.stdout:
                raise RuntimeError(
                    "imagemagick failed: "
                    + (conv.stderr.decode("utf-8", "replace").strip()
                       or "no output"))
            return conv.stdout
        finally:
            with contextlib.suppress(OSError):
                os.unlink(path)

    def sample(self) -> tuple[int, int, int]:
        if self._backend is None:
            self._backend = self._pick_backend()
        return average_ppm(self._grab())

    def start(self) -> None:
        if self._thread is not None:
            return
        self.color = self.sample()  # synchronous: surfaces a missing tool
        self._stop.clear()
        self._thread = threading.Thread(target=self._loop, daemon=True,
                                        name="g502-screen")
        self._thread.start()

    def _loop(self) -> None:
        while not self._stop.wait(self.interval):
            try:
                self.color = self.sample()
            except (RuntimeError, ValueError, OSError,
                    subprocess.SubprocessError):
                continue  # a transient failure must not kill the animation

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=2)
            self._thread = None


class ScreenSampler(Effect):
    """Ambient lighting: the LEDs follow the screen's average colour.

    Smoothing is time-based rather than per-frame so the chase speed does not
    change with fps.
    """

    name = "screen"

    def __init__(self, read_color: Callable[[], tuple[int, int, int]] | None
                 = None, zones: int = ZONES, saturate: float = 1.4,
                 tau: float = 0.25, rate: float = 2.0):
        super().__init__(zones)
        self.saturate = float(saturate)
        self.tau = max(1e-3, float(tau))
        self._read = read_color
        self._cap = None if read_color else _ScreenCapture(rate=rate)
        self._color = (0.0, 0.0, 0.0)
        self._last_t: float | None = None

    def bind(self, runner: "EffectRunner") -> None:
        super().bind(runner)
        self._ensure()

    def _ensure(self) -> None:
        if self._read is None and self._cap is not None:
            self._cap.start()            # raises if no capture tool exists
            self._read = lambda: self._cap.color  # type: ignore[union-attr]

    def tick(self, t: float) -> Frame:
        self._ensure()
        target = _rgb(self._read()) if self._read else (0, 0, 0)
        dt = 0.0 if self._last_t is None else max(0.0, t - self._last_t)
        self._last_t = t
        alpha = 1.0 if dt <= 0 and self._color == (0.0, 0.0, 0.0) \
            else 1 - math.exp(-dt / self.tau)
        self._color = tuple(self._color[i] + (target[i] - self._color[i])
                            * alpha for i in range(3))
        out = boost_saturation(_rgb(self._color), self.saturate)
        return [out] * self.zones

    def close(self) -> None:
        if self._cap is not None:
            self._cap.stop()
            self._read = None


# -- registry --------------------------------------------------------------
# Factories, not classes: the CLI calls these with keyword arguments parsed
# from argv, and every one of them must be constructible with no arguments
# at all (the CLI only passes what a given effect is known to accept).

def static(color: tuple[int, int, int] = (0, 255, 255),
           zones: int = ZONES) -> Effect:
    return Static(color, zones)


def breathe(color: tuple[int, int, int] = (0, 255, 255), period: float = 4.0,
            curve: str = "sine", floor: float = 0.05,
            zones: int = ZONES) -> Effect:
    return Breathe(color, period, curve, floor, zones)


def pulse(color: tuple[int, int, int] = (255, 0, 102), period: float = 1.0,
          zones: int = ZONES) -> Effect:
    return Pulse(color, period, zones=zones)


def rainbow(period: float = 5.0, zone_offset: float = 0.25,
            zones: int = ZONES) -> Effect:
    return Rainbow(period, zone_offset, zones)


def gradient(a: tuple[int, int, int] = (0, 255, 255),
             b: tuple[int, int, int] = (255, 0, 255), period: float = 3.0,
             palette: list[tuple[int, int, int]] | None = None,
             zones: int = ZONES) -> Effect:
    return Gradient(palette or [a, b], period, zones)


def wave(color: tuple[int, int, int] = (0, 255, 136), period: float = 1.5,
         zones: int = ZONES) -> Effect:
    return Wave(color, period, zones)


def dpi_reactive(zones: int = ZONES,
                 read_dpi: Callable[[], int] | None = None,
                 poll: float = 0.25, fade: float = 0.35) -> Effect:
    return DpiReactive(read_dpi, poll=poll, fade=fade, zones=zones)


def audio_visualiser(color: tuple[int, int, int] | None = None,
                     zones: int = ZONES, sensitivity: float = 2.5,
                     source: str | None = None) -> Effect:
    return AudioVisualiser(color, zones, sensitivity, source=source)


def screen_sampler(zones: int = ZONES, saturate: float = 1.4,
                   rate: float = 2.0) -> Effect:
    return ScreenSampler(zones=zones, saturate=saturate, rate=rate)


REGISTRY: dict[str, Callable[..., Effect]] = {
    "solid": static,      # legacy spelling; the CLI passes a colour to this
    "static": static,
    "breathe": breathe,
    "pulse": pulse,
    "rainbow": rainbow,
    "gradient": gradient,
    "wave": wave,
    "dpi": dpi_reactive,
    "audio": audio_visualiser,
    "screen": screen_sampler,
}

EFFECTS = REGISTRY  # name the CLI imports


def list_effects() -> list[str]:
    return sorted(REGISTRY)


def make(name: str, **kwargs) -> Effect:
    try:
        factory = REGISTRY[name]
    except KeyError:
        raise ValueError(
            f"unknown effect {name!r}; have {', '.join(list_effects())}"
        ) from None
    return factory(**kwargs)


# -- runner ----------------------------------------------------------------

@dataclass
class RunStats:
    frames: int = 0
    seconds: float = 0.0

    @property
    def fps(self) -> float:
        return self.frames / self.seconds if self.seconds > 0 else 0.0


class EffectRunner:
    """Plays an effect on the device.

    Holds host mode for the duration (0x8070 is inert otherwise), paces frames
    against a monotonic deadline, and drains the replies send_nowait leaves
    queued every FLUSH_EVERY frames — without that they accumulate. SIGINT and
    SIGTERM stop the loop rather than unwinding through it, so the mode restore
    and the effect's own cleanup always run.
    """

    def __init__(self, dev, fps: int = DEFAULT_FPS,
                 flush_every: int = FLUSH_EVERY):
        self.dev = dev
        self.leds = Leds(dev)
        self.profiles = OnboardProfiles(dev)
        self.fps = max(1, min(int(fps), MAX_FPS))
        self.flush_every = max(1, int(flush_every))
        self.zone_count = self.leds.zone_count
        self.stats = RunStats()
        self._stop = False
        self._old_handlers: dict[int, object] = {}

    # -- signals -----------------------------------------------------------
    def _on_signal(self, signum, _frame) -> None:
        self._stop = True

    def _install_signals(self) -> None:
        self._old_handlers.clear()
        for sig in (signal.SIGINT, signal.SIGTERM):
            # signal.signal only works on the main thread; a runner on a
            # worker thread just relies on stop()/KeyboardInterrupt instead.
            with contextlib.suppress(ValueError, OSError):
                self._old_handlers[sig] = signal.signal(sig, self._on_signal)

    def _restore_signals(self) -> None:
        for sig, handler in self._old_handlers.items():
            with contextlib.suppress(ValueError, OSError, TypeError):
                signal.signal(sig, handler)  # type: ignore[arg-type]
        self._old_handlers.clear()

    def stop(self) -> None:
        """Ask a running loop to finish after the current frame."""
        self._stop = True

    # -- frames ------------------------------------------------------------
    def _frames(self, effect) -> Iterator[Frame]:
        if hasattr(effect, "tick"):
            t0 = time.monotonic()
            while True:
                yield effect.tick(time.monotonic() - t0)
        else:
            yield from effect  # a plain iterable of frames

    def _push(self, frame: Frame) -> None:
        if not frame:
            return
        for z in range(self.zone_count):
            # Effects may emit fewer colours than the device has zones; the
            # last-resort behaviour is to repeat, not to go dark.
            r, g, b = frame[z % len(frame)]
            self.leds.fixed(z, r, g, b, nowait=True)

    def run(self, effect: "Effect | Iterable[Frame]",
            duration: float | None = None) -> int:
        """Play until the effect ends, duration elapses, or we are signalled.

        Returns the number of frames pushed; richer numbers land in .stats.
        """
        interval = 1.0 / self.fps
        self._stop = False
        self._install_signals()
        frames = 0
        started = time.monotonic()
        try:
            with self.profiles.host_mode():
                if hasattr(effect, "bind"):
                    effect.bind(self)
                due = time.monotonic()
                for frame in self._frames(effect):
                    self._push(frame)
                    frames += 1
                    if frames % self.flush_every == 0:
                        self.dev.flush()
                    now = time.monotonic()
                    if self._stop:
                        break
                    if duration is not None and now - started >= duration:
                        break
                    due += interval
                    delay = due - now
                    if delay > 0:
                        time.sleep(delay)
                    elif delay < -interval:
                        due = now  # fell behind: resync instead of catching up
                self.dev.flush()
        except KeyboardInterrupt:
            pass  # a Ctrl-C that beat the handler still exits cleanly
        finally:
            self._restore_signals()
            if hasattr(effect, "close"):
                with contextlib.suppress(Exception):
                    effect.close()
            self.stats = RunStats(frames, time.monotonic() - started)
        return frames


Engine = EffectRunner  # the name the CLI already imports
