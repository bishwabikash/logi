"""Per-application profile switching.

Polls process names and selects a profile from the first matching rule. This
mirrors how the vendor software does "on launch" detection: it keys off a
running process, not the focused window. That also means it behaves the same
under X11, Wayland and a bare TTY, since no window-manager API is involved.
"""
from __future__ import annotations

import os
import time

from .profiles import OnboardProfiles
from .hidpp import HidppError, HidppTimeout


def running_processes() -> set[str]:
    """Process names currently running, lowercased.

    Reads comm rather than the full cmdline: it is one short read per pid and
    is what rule authors actually write down.
    """
    names = set()
    try:
        entries = os.listdir("/proc")
    except OSError:
        return names
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/comm", "rb") as fh:
                names.add(fh.read().strip().decode("utf-8", "replace").lower())
        except (OSError, UnicodeDecodeError):
            continue  # process exited between listdir and open
    return names


def match_rule(cfg: dict, processes: set[str]) -> dict | None:
    for rule in cfg.get("rules", []):
        wanted = rule.get("match") or []
        if isinstance(wanted, str):
            wanted = [wanted]
        for name in wanted:
            n = name.lower()
            if n in processes or any(n in p for p in processes):
                return rule
    return None


class Watcher:
    def __init__(self, dev, cfg: dict, interval: float = 2.0,
                 verbose: bool = False):
        self.dev = dev
        self.cfg = cfg
        self.interval = max(0.25, interval)
        self.verbose = verbose
        self.profiles = OnboardProfiles(dev)
        self.applied: int | None = None
        self.entry_profile = self.profiles.current()

    def _log(self, msg: str) -> None:
        if self.verbose:
            print(f"[g502-watch] {msg}", flush=True)

    def apply(self, profile: int, dpi: int | None = None) -> None:
        if profile == self.applied:
            return
        try:
            self.profiles.set_current(profile)
            self.applied = profile
            self._log(f"profile -> {profile}")
            if dpi:
                from .dpi import Dpi
                actual = Dpi(self.dev).set(dpi)
                self._log(f"dpi -> {actual}")
        except (HidppError, HidppTimeout, ValueError) as e:
            self._log(f"failed to apply profile {profile}: {e}")

    def tick(self) -> None:
        procs = running_processes()
        rule = match_rule(self.cfg, procs)
        if rule:
            self.apply(int(rule.get("profile", 0)), rule.get("dpi"))
        else:
            self.apply(int(self.cfg.get("default_profile", 0)))

    def run(self) -> None:
        self._log(f"watching; {len(self.cfg.get('rules', []))} rule(s), "
                  f"every {self.interval}s")
        try:
            while True:
                self.tick()
                time.sleep(self.interval)
        except KeyboardInterrupt:
            pass
        finally:
            if self.cfg.get("restore_on_exit", True):
                with __import__("contextlib").suppress(
                        HidppError, HidppTimeout, ValueError):
                    self.profiles.set_current(self.entry_profile)
                    self._log(f"restored profile {self.entry_profile}")
