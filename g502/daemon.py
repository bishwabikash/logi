"""g502d — the background daemon.

Ties config + appwatch + effects together: it polls for running processes,
switches the onboard profile and DPI that the matching rule asks for, and
streams a host-side lighting effect for as long as that rule is active.

One thread owns the device. The HID++ transport matches replies on a single
file descriptor, so a second thread issuing calls would consume the first
thread's answers; the frame pump, the process poll and signal handling
therefore all share one loop instead of living in threads.

The daemon must always leave the device in onboard mode. In host mode the
LEDs are driven entirely by this process, so a daemon that exits still
holding host mode leaves the mouse dark until it is replugged — that is the
one property worth sacrificing anything else for, hence the unconditional
restore in _shutdown().
"""
from __future__ import annotations

import argparse
import contextlib
import errno
import logging
import os
import select
import signal
import socket
import sys
import time
from dataclasses import dataclass
from typing import Iterator

from . import appwatch
from . import config as cfgmod
from . import discover
from .dpi import Dpi
from .effects import EFFECTS, Frame
from .hidpp import HidppError, HidppTimeout
from .led import Leds, parse_color
from .profiles import MODE_HOST, MODE_ONBOARD, OnboardProfiles

LOG = logging.getLogger("g502d")

# The device went away, refused us, or stopped answering. Anything in here
# means "drop the link and try to reopen it later", never "crash".
LINK_ERRORS = (HidppError, HidppTimeout, OSError)

DEFAULT_INTERVAL = 2.0
DEFAULT_FPS = 45          # ~ what a HID++ round trip per frame sustains
BACKOFF_START = 1.0
BACKOFF_MAX = 30.0


# -- config shaping --------------------------------------------------------

def effect_spec(raw: object) -> dict | None:
    """Normalise a config "effect" value to a dict or None.

    Accepts a bare name ("rainbow"), a dict ({"name": "wave", ...}), or any
    of None/"none"/"off" meaning "no host effect, let the stored profile
    light the mouse".
    """
    if raw is None:
        return None
    if isinstance(raw, str):
        raw = {"name": raw}
    if not isinstance(raw, dict):
        raise ValueError(f"effect must be a name or an object, got {type(raw).__name__}")
    name = raw.get("name")
    if not name or name in ("none", "off"):
        return None
    if name not in EFFECTS:
        raise ValueError(f"unknown effect {name!r}; have {', '.join(sorted(EFFECTS))}")
    spec = dict(raw)
    spec["name"] = name
    # Validate colours here so a typo is reported once at load rather than
    # on every poll.
    for key in ("color", "color2"):
        if spec.get(key):
            parse_color(str(spec[key]))
    return spec


def build_frames(spec: dict, zones: int) -> Iterator[Frame]:
    """Instantiate an effect generator from a config spec.

    Mirrors the argument mapping g502ctl uses, including periods in ms
    (the generators themselves take seconds).
    """
    name = spec["name"]
    kwargs: dict = {}
    if name in ("solid", "breathe", "gradient", "wave", "audio"):
        kwargs["color"] = parse_color(str(spec.get("color", "00ffff")))
    if name == "gradient":
        kwargs["a"] = kwargs.pop("color")
        kwargs["b"] = parse_color(str(spec.get("color2", "ff00ff")))
    if name in ("rainbow", "breathe", "gradient", "wave"):
        kwargs["period"] = float(spec.get("period", 4000)) / 1000.0
    if name in ("rainbow", "wave", "audio"):
        kwargs["zones"] = zones
    if name == "breathe":
        kwargs["curve"] = str(spec.get("waveform", "sine"))
    if name == "audio":
        if spec.get("source"):
            kwargs["source"] = str(spec["source"])
        if spec.get("sensitivity"):
            kwargs["sensitivity"] = float(spec["sensitivity"])
    return EFFECTS[name](**kwargs)


@dataclass
class Target:
    """The device state the current rule asks for.

    Compared as a whole against what is already applied, so a rule change
    that only swaps the effect is still noticed — keying the comparison on
    the profile alone would miss it.
    """
    profile: int
    dpi: int | None = None
    effect: dict | None = None


class _Link:
    """One open device plus the feature wrappers bound to it."""

    def __init__(self, dev):
        self.dev = dev
        self.profiles = OnboardProfiles(dev)
        self.leds = Leds(dev)
        self.mode = self.profiles.mode()
        self.entry_profile = self.profiles.current()
        self._dpi: Dpi | None = None

    @property
    def dpi(self) -> Dpi:
        # Built on demand: most configs never set a DPI, and the lookup is a
        # round trip.
        if self._dpi is None:
            self._dpi = Dpi(self.dev)
        return self._dpi


# -- single instance -------------------------------------------------------

def single_instance(name: str | None = None) -> socket.socket:
    """Claim an abstract-namespace unix socket as the run lock.

    The kernel releases an abstract name when the last holder's fd closes,
    including on SIGKILL, so there is no stale lock to reap the way a
    pidfile needs. Per-uid, because this runs as a user unit.
    """
    name = name or f"g502d.{os.getuid()}"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.bind("\0" + name)
    except OSError as e:
        s.close()
        if e.errno == errno.EADDRINUSE:
            raise SystemExit(
                f"g502d: already running (lock {name!r} is held). "
                f"Stop it with: systemctl --user stop g502d")
        raise
    return s


class Daemon:
    def __init__(self, cfg_path=None, interval: float | None = None,
                 fps: int | None = None):
        self.cfg_path = cfg_path
        self._interval_override = interval
        self._fps_override = fps
        self.cfg: dict = {}
        self.interval = DEFAULT_INTERVAL
        self.frame_interval = 1.0 / DEFAULT_FPS
        self.link: _Link | None = None
        self.applied: Target | None = None
        self.effect: Iterator[Frame] | None = None
        self.effect_name = ""
        self.frames = 0
        # Effects that failed at runtime (e.g. audio with no parec). Kept so
        # a rule does not restart a known-broken effect every poll.
        self.failed: set[str] = set()
        self._stop = False
        self._reload = False
        self._absent_logged = False
        self._wake_r = -1
        self._wake_w = -1

    # -- config ------------------------------------------------------------
    def load_config(self, first: bool = False) -> None:
        try:
            cfg = cfgmod.load(self.cfg_path)
        except SystemExit as e:
            # A typo in the config must not take down a running daemon; on
            # the first load it should still be loud.
            if first:
                raise
            LOG.error("reload failed, keeping previous config: %s", e)
            return
        # Validate every effect once, here, so the poll loop cannot raise.
        for holder, where in [(cfg, "top level")] + [
                (r, f"rule {i}") for i, r in enumerate(cfg.get("rules", []))
                if isinstance(r, dict)]:
            if "effect" not in holder:
                continue
            try:
                holder["effect"] = effect_spec(holder["effect"])
            except ValueError as e:
                LOG.error("%s: ignoring bad effect: %s", where, e)
                holder["effect"] = None
        self.cfg = cfg
        self.interval = max(0.25, float(
            self._interval_override
            if self._interval_override is not None
            else cfg.get("poll_interval", DEFAULT_INTERVAL)))
        fps = int(self._fps_override if self._fps_override is not None
                  else cfg.get("fps", DEFAULT_FPS))
        self.frame_interval = 1.0 / max(1, min(fps, 120))
        self.failed.clear()
        self.applied = None  # re-apply against the new rules
        LOG.info("config: %d rule(s), poll %.2fs, %d fps",
                 len(cfg.get("rules", [])), self.interval, fps)

    def resolve(self, rule: dict | None) -> Target:
        src = rule or {}
        effect = (src.get("effect") if "effect" in src
                  else self.cfg.get("effect"))
        dpi = src.get("dpi", self.cfg.get("dpi"))
        return Target(
            profile=int(src.get("profile", self.cfg.get("default_profile", 0))),
            dpi=int(dpi) if dpi else None,
            effect=effect if isinstance(effect, dict) else None,
        )

    # -- signals -----------------------------------------------------------
    def install_signals(self) -> None:
        """Flag-setting handlers plus a wakeup pipe.

        The handlers never touch the device: a signal can land in the middle
        of a HID++ request, and issuing another from the handler would
        interleave with the reply the main loop is waiting for. The pipe is
        what makes an idle sleep return immediately instead of after a whole
        poll interval.
        """
        r, w = os.pipe()
        os.set_blocking(r, False)
        os.set_blocking(w, False)
        self._wake_r, self._wake_w = r, w
        signal.set_wakeup_fd(w)
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, self._on_stop)
        signal.signal(signal.SIGHUP, self._on_hup)

    def _on_stop(self, signum, _frame) -> None:
        LOG.info("signal %s: shutting down", signal.Signals(signum).name)
        self._stop = True

    def _on_hup(self, _signum, _frame) -> None:
        self._reload = True

    def sleep(self, timeout: float) -> None:
        if timeout <= 0:
            return
        try:
            ready, _, _ = select.select([self._wake_r], [], [], timeout)
        except (InterruptedError, OSError):
            return
        if ready:
            with contextlib.suppress(OSError):
                os.read(self._wake_r, 512)

    # -- device link -------------------------------------------------------
    def connect(self) -> bool:
        try:
            dev = discover.open_device()
        except (RuntimeError, OSError) as e:
            # Unplugged is the normal case, not an error; say it once and
            # then keep quiet so journald is not flooded while it is away.
            LOG.log(logging.WARNING if not self._absent_logged else logging.DEBUG,
                    "device unavailable: %s", e)
            self._absent_logged = True
            return False
        try:
            self.link = _Link(dev)
        except LINK_ERRORS as e:
            LOG.warning("device opened but did not answer: %s", e)
            with contextlib.suppress(OSError):
                dev.close()
            self.link = None
            return False
        self._absent_logged = False
        self.applied = None  # a fresh link knows nothing about what is set
        LOG.info("connected: %s at %s (mode %d, profile %d)",
                 getattr(dev, "model", "?"), dev.path,
                 self.link.mode, self.link.entry_profile)
        return True

    def drop_link(self, reason: str) -> None:
        LOG.warning("device lost: %s", reason)
        self.stop_effect()
        if self.link is not None:
            # Best effort only — the usual reason we are here is that the
            # device is already gone.
            with contextlib.suppress(*LINK_ERRORS):
                self.link.profiles.set_mode(MODE_ONBOARD)
            with contextlib.suppress(OSError):
                self.link.dev.close()
        self.link = None
        self.applied = None

    def set_mode(self, mode: int) -> None:
        if self.link is None or self.link.mode == mode:
            return
        self.link.profiles.set_mode(mode)
        self.link.mode = mode
        LOG.debug("device mode -> %d", mode)

    # -- effects -----------------------------------------------------------
    def start_effect(self, spec: dict) -> None:
        name = spec["name"]
        if name in self.failed:
            LOG.debug("effect %s is disabled after an earlier failure", name)
            return
        assert self.link is not None
        frames = build_frames(spec, self.link.leds.zone_count)
        self.set_mode(MODE_HOST)  # live 0x8070 writes are inert in onboard mode
        self.effect = frames
        self.effect_name = name
        self.frames = 0
        LOG.info("effect -> %s", name)

    def stop_effect(self) -> None:
        if self.effect is None:
            return
        gen, self.effect = self.effect, None
        # close() runs the generator's finally blocks, which is what stops
        # the audio effect's parec child.
        with contextlib.suppress(OSError, RuntimeError):
            gen.close()
        if self.link is not None:
            with contextlib.suppress(*LINK_ERRORS):
                self.link.dev.flush()  # replies left by send_nowait

    def push_frame(self) -> bool:
        """Push one animation frame. False means the link was dropped."""
        assert self.effect is not None and self.link is not None
        try:
            frame = next(self.effect)
        except StopIteration:
            LOG.info("effect %s ended after %d frames", self.effect_name, self.frames)
            self.stop_effect()
            self._safe_onboard()
            return True
        except RuntimeError as e:
            # A generator can refuse at runtime — audio does when parec is
            # missing. Disable that effect and carry on; the mouse falls back
            # to its stored profile lighting.
            LOG.error("effect %s unavailable: %s", self.effect_name, e)
            self.failed.add(self.effect_name)
            self.stop_effect()
            self._safe_onboard()
            return True
        try:
            for z in range(self.link.leds.zone_count):
                r, g, b = frame[z % len(frame)]
                self.link.leds.fixed(z, r, g, b, nowait=True)
            self.frames += 1
            if self.frames % 32 == 0:
                self.link.dev.flush()  # drop accumulated replies
        except LINK_ERRORS as e:
            self.drop_link(f"lighting write failed: {e}")
            return False
        return True

    def _safe_onboard(self) -> None:
        try:
            self.set_mode(MODE_ONBOARD)
        except LINK_ERRORS as e:
            self.drop_link(f"mode restore failed: {e}")

    # -- apply -------------------------------------------------------------
    def apply(self, target: Target) -> None:
        link = self.link
        assert link is not None
        prev = self.applied
        want_profile = prev is None or target.profile != prev.profile
        want_dpi = bool(target.dpi) and (prev is None or target.dpi != prev.dpi)
        want_effect = prev is None or target.effect != prev.effect

        if want_profile or want_dpi:
            # Profile and DPI are onboard-mode concerns: the device is being
            # told which stored profile to run. Drop out of host mode first so
            # the switch is not fighting a live effect, then put the effect
            # back afterwards.
            self.stop_effect()
            self.set_mode(MODE_ONBOARD)
            if want_profile:
                link.profiles.set_current(target.profile)
                LOG.info("profile -> %d", target.profile)
            if want_dpi:
                LOG.info("dpi -> %d", link.dpi.set(int(target.dpi)))
            want_effect = True  # the effect was torn down above

        if want_effect:
            self.stop_effect()
            if target.effect:
                self.start_effect(target.effect)
            else:
                self.set_mode(MODE_ONBOARD)
        self.applied = target

    def poll(self) -> bool:
        """Evaluate the rules once. False means the link was dropped."""
        rule = appwatch.match_rule(self.cfg, appwatch.running_processes())
        target = self.resolve(rule)
        if target == self.applied:
            return True
        try:
            self.apply(target)
        except LINK_ERRORS as e:
            self.drop_link(f"apply failed: {e}")
            return False
        except (ValueError, KeyError) as e:
            # A rule asking for profile 9 or an impossible DPI: complain once
            # and mark it applied so it is not retried every poll.
            LOG.error("cannot apply rule %s: %s", rule or "<default>", e)
            self.applied = target
        return True

    # -- lifecycle ---------------------------------------------------------
    def loop(self) -> None:
        backoff = BACKOFF_START
        next_poll = 0.0
        while not self._stop:
            if self._reload:
                self._reload = False
                LOG.info("SIGHUP: reloading config")
                self.load_config()
            if self.link is None:
                if not self.connect():
                    self.sleep(backoff)
                    backoff = min(backoff * 2, BACKOFF_MAX)
                    continue
                backoff = BACKOFF_START
            now = time.monotonic()
            if now >= next_poll:
                next_poll = now + self.interval
                if not self.poll():
                    continue
            if self.effect is not None:
                if not self.push_frame():
                    continue
                self.sleep(self.frame_interval)
            else:
                # Nothing to animate: idle until the next poll is due.
                self.sleep(max(0.05, next_poll - time.monotonic()))

    def shutdown(self) -> None:
        """Always leave the device in onboard mode.

        Host mode makes the LEDs depend on this process, so exiting while
        holding it leaves the mouse dark until a replug. The write is issued
        unconditionally rather than only when we believe we hold host mode: a
        lost reply could have left link.mode wrong, and a redundant write
        costs one round trip.
        """
        self.stop_effect()
        if self.link is not None:
            try:
                self.link.profiles.set_mode(MODE_ONBOARD)
                self.link.mode = MODE_ONBOARD
                LOG.info("restored onboard mode")
            except LINK_ERRORS as e:
                LOG.warning("could not restore onboard mode: %s", e)
            if self.cfg.get("restore_on_exit", True):
                with contextlib.suppress(*LINK_ERRORS, ValueError):
                    self.link.profiles.set_current(self.link.entry_profile)
                    LOG.info("restored profile %d", self.link.entry_profile)
            with contextlib.suppress(OSError):
                self.link.dev.close()
            self.link = None
        if self._wake_w >= 0:
            signal.set_wakeup_fd(-1)
            for fd in (self._wake_r, self._wake_w):
                with contextlib.suppress(OSError):
                    os.close(fd)
            self._wake_r = self._wake_w = -1

    def run(self) -> int:
        self.load_config(first=True)
        self.install_signals()
        LOG.info("g502d started (pid %d)", os.getpid())
        try:
            self.loop()
        finally:
            self.shutdown()
        LOG.info("g502d stopped")
        return 0


# -- entry point -----------------------------------------------------------

_PRIORITY = {logging.DEBUG: 7, logging.INFO: 6, logging.WARNING: 4,
             logging.ERROR: 3, logging.CRITICAL: 2}


class _JournalFormatter(logging.Formatter):
    """Prefix lines with a syslog priority.

    systemd reads these off stderr (SyslogLevelPrefix defaults to yes), so
    `journalctl -p warning` filters this daemon properly instead of seeing
    every line as plain info.
    """

    def format(self, record: logging.LogRecord) -> str:
        return f"<{_PRIORITY.get(record.levelno, 6)}>{super().format(record)}"


def setup_logging(level: str) -> None:
    handler = logging.StreamHandler(sys.stderr)
    if sys.stderr.isatty():
        handler.setFormatter(logging.Formatter(
            "%(asctime)s %(levelname)-7s %(message)s", "%H:%M:%S"))
    else:
        handler.setFormatter(_JournalFormatter("g502d: %(message)s"))
    logging.basicConfig(level=getattr(logging, level.upper(), logging.INFO),
                        handlers=[handler], force=True)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        prog="g502d",
        description="Background daemon for the Logitech G502 HERO: "
                    "per-application profiles and host-side lighting.")
    ap.add_argument("--config", default=None, help="config path")
    ap.add_argument("--interval", type=float, default=None,
                    help="process poll interval in seconds")
    ap.add_argument("--fps", type=int, default=None, help="effect frame rate")
    ap.add_argument("--log-level", default=os.environ.get("G502D_LOG_LEVEL", "info"),
                    choices=["debug", "info", "warning", "error"])
    ap.add_argument("--check", action="store_true",
                    help="validate the config and exit without touching the device")
    a = ap.parse_args(argv)

    setup_logging(a.log_level)
    d = Daemon(a.config, interval=a.interval, fps=a.fps)
    if a.check:
        d.load_config(first=True)
        print(f"config ok: {len(d.cfg.get('rules', []))} rule(s)")
        return 0
    lock = single_instance()  # held for the process lifetime
    try:
        return d.run()
    finally:
        lock.close()


if __name__ == "__main__":
    raise SystemExit(main())
