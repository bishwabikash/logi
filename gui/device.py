"""Device access for the GUI: one worker thread owns the hidraw fd.

HID++ is strict request/response over a single file descriptor and
Device.call() drains stale input before writing, so two threads issuing calls
would read each other's replies. Everything therefore funnels through one
worker thread and results come back to the GTK main loop via GLib.idle_add.
The main loop never touches the device — a call that times out costs 800 ms,
which would be a visible freeze.
"""
from __future__ import annotations

import contextlib
import errno
import queue
import threading
from dataclasses import dataclass, field
from typing import Callable

from gi.repository import GLib, GObject

from . import optional

UDEV_HINT = ("The mouse is there but this user cannot open it. Install the "
             "udev rule and replug:\n\n    sudo ./install.sh udev")
MISSING_HINT = ("Plug in a supported Logitech mouse (G502 HERO, 046d:c08b). "
                "If it is plugged in, check that it is not claimed by "
                "another process.")

# Errno values that mean the endpoint is gone rather than unhappy.
_LOST = frozenset({errno.ENODEV, errno.ENXIO, errno.EIO, errno.EBADF,
                   errno.ENOENT, errno.EPIPE, errno.ESHUTDOWN})


@dataclass(slots=True)
class ZoneInfo:
    index: int
    location_name: str
    modes: list[int]


@dataclass(slots=True)
class ProfileEntry:
    index: int
    name: str
    crc_ok: bool
    profile: object | None = None      # profile_blob.Profile
    error: str | None = None


@dataclass(slots=True)
class Snapshot:
    """Everything the UI needs, read in one worker job.

    A snapshot is immutable as far as the pages are concerned: they render it
    and ask the worker for a new one after any write, rather than tracking
    device state themselves.
    """
    model: str = "unknown device"
    path: str = ""
    pid: int = 0
    protocol: tuple[int, int] = (0, 0)
    dpi: int = 0
    dpi_default: int = 0
    dpi_cap: dict = field(default_factory=dict)
    rate: int = 0
    rates: list[int] = field(default_factory=list)
    mode: int = 0
    host_held: bool = False
    profile_count: int = 0
    button_count: int = 0
    active_profile: int = 0
    dpi_index: int = 0
    zones: list[ZoneInfo] = field(default_factory=list)
    profiles: list[ProfileEntry] = field(default_factory=list)
    features: dict[str, bool] = field(default_factory=dict)

    def profile_name(self, index: int) -> str:
        from .model import profile_label
        for p in self.profiles:
            if p.index == index:
                return profile_label(index, p.name)
        return f"Profile {index + 1}"

    @property
    def profile_names(self) -> list[str]:
        return [self.profile_name(i) for i in range(self.profile_count)]


class Session:
    """The open device plus its feature wrappers. Worker thread only."""

    def __init__(self, dev):
        from g502.dpi import Dpi
        from g502.led import Leds
        from g502.profiles import OnboardProfiles
        from g502.rate import ReportRate

        self.dev = dev
        self.dpi = _optional(lambda: Dpi(dev))
        self.rate = _optional(lambda: ReportRate(dev))
        self.leds = _optional(lambda: Leds(dev))
        self.profiles = _optional(lambda: OnboardProfiles(dev))
        # Host mode is entered through the library's context manager even
        # though the GUI holds it open across many jobs: the ExitStack keeps
        # the restore-on-exit guarantee that a bare set_mode() would lose.
        self._host = contextlib.ExitStack()
        self.host_held = False

    def hold_host_mode(self) -> None:
        """Required before live 0x8070 lighting has any visible effect."""
        if self.host_held or self.profiles is None:
            return
        self._host.enter_context(self.profiles.host_mode())
        self.host_held = True

    def release_host_mode(self) -> None:
        if self.host_held:
            self.host_held = False
            with contextlib.suppress(Exception):
                self._host.close()

    def close(self) -> None:
        self.release_host_mode()
        with contextlib.suppress(Exception):
            self.dev.close()


def _optional(build):
    """Feature wrappers raise if the device lacks the feature; a missing
    feature disables a page, it is not a fatal error."""
    try:
        return build()
    except Exception:  # noqa: BLE001 - KeyError, HidppError, HidppTimeout, OSError
        return None


def read_snapshot(s: Session) -> Snapshot:
    """One pass over the device. Roughly 100 round trips, hence off-thread."""
    from g502.profile_blob import Profile

    snap = Snapshot(
        model=getattr(s.dev, "model", "Logitech mouse"),
        path=getattr(s.dev, "path", ""),
        pid=getattr(s.dev, "pid", 0),
        host_held=s.host_held,
    )
    with contextlib.suppress(Exception):
        snap.protocol = s.dev.protocol_version()

    snap.features = {"dpi": s.dpi is not None, "rate": s.rate is not None,
                     "leds": s.leds is not None, "profiles": s.profiles is not None}

    if s.dpi is not None:
        with contextlib.suppress(Exception):
            snap.dpi_cap = s.dpi.capability()
            snap.dpi = s.dpi.get()
            snap.dpi_default = s.dpi.default()
    if s.rate is not None:
        with contextlib.suppress(Exception):
            snap.rates = s.rate.supported()
            snap.rate = s.rate.get()
    if s.leds is not None:
        snap.zones = [ZoneInfo(z.index, z.location_name, list(z.modes))
                      for z in s.leds.zones]
    if s.profiles is not None:
        info = s.profiles.info
        snap.profile_count = info.profile_count
        snap.button_count = info.button_count
        with contextlib.suppress(Exception):
            snap.mode = s.profiles.mode()
        with contextlib.suppress(Exception):
            snap.active_profile = s.profiles.current()
        with contextlib.suppress(Exception):
            snap.dpi_index = s.profiles.dpi_index()
        for i in range(info.profile_count):
            # Profile N lives in sector N+1; sector 0 is the directory.
            try:
                blob = s.profiles.read_sector(i + 1)
                p = Profile.parse(blob)
                snap.profiles.append(
                    ProfileEntry(i, p.name, p.crc_ok(), p))
            except Exception as e:  # noqa: BLE001
                snap.profiles.append(
                    ProfileEntry(i, "", False, None, f"{type(e).__name__}: {e}"))
    return snap


def diagnose() -> tuple[str, str]:
    """Why there is no device: (reason, actionable hint)."""
    import os

    try:
        from g502 import discover
    except Exception as e:  # noqa: BLE001
        return (f"cannot import g502.discover: {e}",
                "Run the app from the project checkout.")
    try:
        found = discover.candidates()
    except Exception as e:  # noqa: BLE001
        return (f"cannot enumerate hidraw devices: {e}", MISSING_HINT)
    if not found:
        return ("No supported Logitech mouse found", MISSING_HINT)
    blocked = [p for p, _, _ in found if not os.access(p, os.R_OK | os.W_OK)]
    if blocked:
        return (f"Cannot open {', '.join(blocked)}", UDEV_HINT)
    return ("Found the mouse but no interface answered HID++",
            "Unplug and replug it. Another process may hold the endpoint.")


def _device_lost(e: BaseException) -> bool:
    if isinstance(e, PermissionError):
        return True
    if isinstance(e, OSError):
        return e.errno in _LOST
    return False


class DeviceWorker(GObject.Object):
    """Serialises every device call onto one thread.

    Signals carry plain Python objects, so the pages receive the same
    Snapshot the worker built rather than a flattened copy.
    """

    __gtype_name__ = "G502DeviceWorker"
    __gsignals__ = {
        # snapshot
        "device-ready": (GObject.SignalFlags.RUN_FIRST, None, (object,)),
        # reason, hint
        "device-lost": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
        "busy-changed": (GObject.SignalFlags.RUN_FIRST, None, (bool,)),
        "message": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        "effect-changed": (GObject.SignalFlags.RUN_FIRST, None, (bool,)),
    }

    def __init__(self, fake_session=None):
        super().__init__()
        self._q: queue.Queue = queue.Queue()
        self._session: Session | None = fake_session
        self._fake = fake_session
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self._pending = 0
        self._effect_stop = threading.Event()
        self._effect_running = False

    # -- lifecycle ---------------------------------------------------------
    def start(self) -> None:
        self._thread = threading.Thread(target=self._loop, name="g502-io",
                                        daemon=True)
        self._thread.start()
        self.connect_device()

    def shutdown(self, timeout: float = 3.0) -> None:
        """Stop the worker and put the device back to onboard mode.

        Leaving host mode set would make the mouse's lighting depend on a
        process that is no longer running, so the release is not optional.
        """
        self.stop_effect()
        self._q.put(None)
        if self._thread is not None:
            self._thread.join(timeout)

    @property
    def connected(self) -> bool:
        return self._session is not None

    @property
    def effect_running(self) -> bool:
        return self._effect_running

    # -- submission --------------------------------------------------------
    def connect_device(self) -> None:
        """(Re)open the device and read a snapshot."""
        self._enqueue(self._connect_job, None, None, interrupt_effect=True)

    def submit(self, job: Callable[[Session], object],
               on_done: Callable[[object], None] | None = None,
               on_error: Callable[[str], None] | None = None,
               *, refresh: bool = False, message: str | None = None,
               interrupt_effect: bool = True) -> None:
        """Run `job(session)` on the worker thread.

        refresh re-reads the snapshot afterwards, which is what every write
        wants: the device is the source of truth, not the widgets.
        interrupt_effect stops a running animation first, because it holds
        the worker thread for as long as it plays.
        """
        def wrapped(session: Session):
            result = job(session)
            return read_snapshot(session) if refresh else result

        def done(result):
            if refresh and isinstance(result, Snapshot):
                self.emit("device-ready", result)
            elif on_done is not None:
                on_done(result)
            if message:
                self.emit("message", message)
            if refresh and on_done is not None:
                on_done(result)

        self._enqueue(wrapped, done, on_error, interrupt_effect)

    def refresh(self) -> None:
        self.submit(lambda s: None, refresh=True)

    def _enqueue(self, job, on_done, on_error, interrupt_effect: bool) -> None:
        if interrupt_effect and self._effect_running:
            self.stop_effect()
        with self._lock:
            self._pending += 1
            if self._pending == 1:
                self.emit("busy-changed", True)
        self._q.put((job, on_done, on_error))

    # -- effects -----------------------------------------------------------
    def run_effect(self, build: Callable[[Session], tuple],
                   on_stop: Callable[[], None] | None = None) -> None:
        """Play a host-side animation until stop_effect().

        `build(session)` returns (engine, generator); it runs on the worker
        thread because constructing an Engine talks to the device.
        """
        def job(session: Session):
            engine, gen = build(session)
            self._effect_stop.clear()
            return engine.run(_until_stopped(gen, self._effect_stop))

        def started():
            self._effect_running = True
            self.emit("effect-changed", True)

        def finished(frames):
            self._effect_running = False
            self.emit("effect-changed", False)
            if on_stop is not None:
                on_stop()

        def failed(msg):
            self._effect_running = False
            self.emit("effect-changed", False)
            self.emit("message", msg)
            if on_stop is not None:
                on_stop()

        started()
        self._enqueue(job, finished, failed, interrupt_effect=True)

    def stop_effect(self) -> None:
        """Safe from the main loop: only sets a flag the worker polls."""
        self._effect_stop.set()

    # -- worker thread -----------------------------------------------------
    def _connect_job(self, _session):
        if self._session is not None:
            return read_snapshot(self._session)
        from g502.discover import open_device

        self._session = Session(open_device())
        return read_snapshot(self._session)

    def _loop(self) -> None:
        try:
            while True:
                item = self._q.get()
                if item is None:
                    break
                self._run(item)
        finally:
            if self._session is not None and self._fake is None:
                self._session.close()
            self._session = None

    def _run(self, item) -> None:
        job, on_done, on_error = item
        try:
            if self._session is None and job is not self._connect_job:
                raise RuntimeError("device is not connected")
            result = job(self._session)
        except BaseException as e:  # noqa: BLE001 - must never kill the thread
            self._fail(e, on_error)
        else:
            if job is self._connect_job:
                self._main(self.emit, "device-ready", result)
            elif on_done is not None:
                self._main(on_done, result)
        finally:
            with self._lock:
                self._pending = max(0, self._pending - 1)
                idle = self._pending == 0
            if idle:
                self._main(self.emit, "busy-changed", False)

    def _fail(self, e: BaseException, on_error) -> None:
        from g502.hidpp import HidppError, HidppTimeout

        if _device_lost(e) or not self._healthy(e):
            reason, hint = self._teardown(e)
            self._main(self.emit, "device-lost", reason, hint)
            return
        if isinstance(e, HidppTimeout):
            msg = f"The mouse did not answer: {e}"
        elif isinstance(e, HidppError):
            msg = f"The mouse rejected that: {e}"
        elif isinstance(e, (ValueError, KeyError, RuntimeError)):
            msg = str(e) or type(e).__name__
        else:
            msg = f"{type(e).__name__}: {e}"
        if on_error is not None:
            self._main(on_error, msg)
        else:
            self._main(self.emit, "message", msg)

    def _healthy(self, e: BaseException) -> bool:
        """A timeout can mean a busy firmware or an unplugged mouse. Ask the
        root feature which it is rather than guessing."""
        from g502.hidpp import HidppTimeout

        if not isinstance(e, HidppTimeout) or self._session is None:
            return True
        try:
            self._session.dev.protocol_version()
            return True
        except Exception:  # noqa: BLE001
            return False

    def _teardown(self, e: BaseException) -> tuple[str, str]:
        if self._session is not None and self._fake is None:
            self._session.close()
        self._session = None
        if isinstance(e, RuntimeError) and not isinstance(e, OSError):
            return diagnose() if "no supported" in str(e).lower() else (str(e), MISSING_HINT)
        if isinstance(e, PermissionError):
            return (f"Permission denied opening the device: {e}", UDEV_HINT)
        return diagnose()

    @staticmethod
    def _main(fn, *args) -> None:
        """Hop to the GTK main loop. GObject emission is not thread-safe."""
        def once():
            fn(*args)
            return GLib.SOURCE_REMOVE

        GLib.idle_add(once)


def _until_stopped(gen, stop: threading.Event):
    """Wrap an effect generator so stop_effect() ends Engine.run().

    Engine.run() only exits when the generator is exhausted, its duration
    elapses, or KeyboardInterrupt — and a GUI has none of those. Ending the
    generator is the one lever that also runs the engine's host-mode restore.
    """
    for frame in gen:
        if stop.is_set():
            return
        yield frame


# -- effect construction ----------------------------------------------------

def build_effect(name: str, *, color, color2, period_ms: int, fps: int,
                 waveform: str):
    """A `build` callable for run_effect(), bound to the chosen parameters."""
    def build(session: Session):
        engine = optional.effect_engine(session.dev, fps)
        zones = getattr(engine, "zone_count", 2)
        gen = optional.effect_generator(
            name, color=color, a=color, b=color2,
            period=max(0.05, period_ms / 1000.0), zones=zones,
            curve=waveform)
        return engine, gen

    return build
