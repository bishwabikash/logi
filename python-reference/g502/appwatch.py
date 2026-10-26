"""Per-application automatic profile switching.

GHub's headline feature, split in half. This module is the *deciding* half: it
samples what is running (and, where the desktop permits, what is focused),
picks the first matching rule and hands the resulting Action to a callback.
It never opens the device. The daemon owns the device and decides what an
Action means, which keeps everything here testable with synthetic input.

Foreground detection is best-effort by necessity. Wayland gives no client a
way to ask "which window has focus" — that is a deliberate property of the
protocol, not a missing feature — so the only routes on GNOME are side doors
through the shell itself. When none answers we fall back to process-presence
scanning over /proc, which always works (X11, Wayland, a bare TTY) and is
effectively what the vendor software keys off anyway.

Rules and the rule set serialise to and from plain dicts so config.py can
persist them; the dict shape is the one already in config.DEFAULT_CONFIG.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, field

PROFILE_COUNT = 5          # 5 onboard profiles on this device
DEFAULT_INTERVAL = 1.0     # one /proc walk a second is unobtrusive
DEFAULT_DEBOUNCE = 1.5     # a candidate must persist this long before it wins
PROBE_RETRY = 60.0         # leave a failed foreground probe alone this long
COMM_MAX = 15              # kernel truncates /proc/<pid>/comm to 15 chars


# -- what we matched against ----------------------------------------------

@dataclass(frozen=True)
class Process:
    """One observed process. All fields are lowercased at construction."""

    pid: int = 0
    comm: str = ""        # /proc/<pid>/comm, kernel-truncated
    exe: str = ""         # basename of the executable (or of argv[0])
    cmdline: str = ""     # full command line, NUL-separated args joined

    @classmethod
    def synthetic(cls, name: str, pid: int = 0) -> Process:
        """A process known only by name, e.g. a window's WM_CLASS.

        The foreground probes sometimes give a class with no pid; filling all
        three fields lets such a hit match the same rules as a real process.
        """
        n = name.strip().lower()
        return cls(pid=pid, comm=n[:COMM_MAX], exe=n, cmdline=n)


def read_process(pid: int, proc_root: str = "/proc") -> Process | None:
    """Read one pid, or None if it is gone or unreadable."""
    base = f"{proc_root}/{pid}"
    try:
        with open(f"{base}/comm", "rb") as fh:
            comm = fh.read().strip().decode("utf-8", "replace").lower()
    except OSError:
        return None  # exited between listdir and open; nothing to report
    argv: list[str] = []
    try:
        with open(f"{base}/cmdline", "rb") as fh:
            argv = [a.decode("utf-8", "replace")
                    for a in fh.read().split(b"\0") if a]
    except OSError:
        pass  # kernel threads have an empty cmdline; comm still identifies them
    exe = ""
    try:
        exe = os.path.basename(os.readlink(f"{base}/exe")).lower()
    except OSError:
        # /proc/<pid>/exe is only readable for our own processes (and root).
        # argv[0] is the next best thing, and it is what `ps` shows.
        if argv:
            exe = os.path.basename(argv[0]).lower()
    if exe.endswith(" (deleted)"):
        exe = exe[:-len(" (deleted)")]  # binary replaced by an update
    return Process(pid=pid, comm=comm, exe=exe,
                   cmdline=" ".join(argv).lower())


def snapshot_processes(proc_root: str = "/proc") -> list[Process]:
    """Every process we can see. The fallback that always works."""
    out: list[Process] = []
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return out
    for entry in entries:
        if not entry.isdigit():
            continue
        p = read_process(int(entry), proc_root)
        if p is not None:
            out.append(p)
    return out


# -- rules -----------------------------------------------------------------

def _tokens(value: object) -> tuple[str, ...]:
    """Accept a bare string or a list of them; normalise to lowercase."""
    if value is None:
        return ()
    if isinstance(value, str):
        value = [value]
    if not isinstance(value, (list, tuple)):
        raise ValueError(f"expected a string or list of strings, got {value!r}")
    out = []
    for v in value:
        if not isinstance(v, str):
            raise ValueError(f"pattern must be a string, got {v!r}")
        v = v.strip().lower()
        if v:
            out.append(v)
    return tuple(out)


def _name_hit(token: str, value: str) -> bool:
    """Exact name match, tolerating the 15-char comm truncation."""
    return bool(value) and (value == token or value == token[:COMM_MAX])


def _opt_int(value: object, what: str) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{what} must be an integer, got {value!r}")
    return value


@dataclass(frozen=True)
class Rule:
    """One match -> profile mapping.

    `match` is the loose form users write by hand: a hit on the process name,
    the executable basename, or a substring of the command line. `comm` and
    `cmdline` exist for when that is too blunt — e.g. telling two Electron
    apps apart needs the command line, since their comm is both "electron".
    """

    match: tuple[str, ...] = ()      # name OR exe basename OR cmdline substring
    comm: tuple[str, ...] = ()       # process name / exe basename, exact
    cmdline: tuple[str, ...] = ()    # substring of the full command line
    profile: int = 0
    dpi: int | None = None           # range checking is dpi.Dpi.set's job
    effect: str | None = None        # name from effects.EFFECTS
    name: str = ""                   # label, for logs only
    enabled: bool = True

    @property
    def patterns(self) -> tuple[str, ...]:
        return self.comm + self.match + self.cmdline

    @property
    def label(self) -> str:
        pats = self.patterns
        return self.name or (pats[0] if pats else "rule")

    def matches(self, p: Process) -> bool:
        """A rule with no patterns matches nothing — never everything."""
        if not self.enabled:
            return False
        for t in self.comm:
            if _name_hit(t, p.comm) or _name_hit(t, p.exe):
                return True
        for t in self.cmdline:
            if t in p.cmdline:
                return True
        for t in self.match:
            if (_name_hit(t, p.comm) or _name_hit(t, p.exe)
                    or t in p.cmdline):
                return True
        return False

    # -- persistence -------------------------------------------------------
    @classmethod
    def from_dict(cls, d: Mapping) -> Rule:
        if not isinstance(d, Mapping):
            raise ValueError(f"rule must be an object, got {d!r}")
        profile = _opt_int(d.get("profile", 0), "profile")
        if not 0 <= (profile or 0) < PROFILE_COUNT:
            raise ValueError(
                f"profile {profile} out of range 0..{PROFILE_COUNT - 1}")
        effect = d.get("effect")
        if effect is not None and not isinstance(effect, str):
            raise ValueError(f"effect must be a string, got {effect!r}")
        return cls(
            match=_tokens(d.get("match")),
            comm=_tokens(d.get("comm")),
            cmdline=_tokens(d.get("cmdline")),
            profile=profile or 0,
            dpi=_opt_int(d.get("dpi"), "dpi"),
            effect=effect,
            # "comment" is what the shipped example config uses for a label.
            name=str(d.get("name") or d.get("comment") or ""),
            enabled=bool(d.get("enabled", True)),
        )

    def to_dict(self) -> dict:
        """Only non-default keys, so a hand-written config stays readable."""
        d: dict = {}
        for key in ("match", "comm", "cmdline"):
            v = getattr(self, key)
            if v:
                d[key] = list(v)
        d["profile"] = self.profile
        if self.dpi is not None:
            d["dpi"] = self.dpi
        if self.effect is not None:
            d["effect"] = self.effect
        if self.name:
            d["name"] = self.name
        if not self.enabled:
            d["enabled"] = False
        return d


@dataclass(frozen=True)
class Action:
    """What the daemon should make the device look like.

    `rule` and `source` are excluded from equality: they are diagnostics, and
    two rules asking for the same profile must not cause a re-apply.
    """

    profile: int
    dpi: int | None = None
    effect: str | None = None
    rule: str = field(default="", compare=False)
    source: str = field(default="default", compare=False)  # foreground|process|default


@dataclass
class RuleSet:
    """Ordered rules plus the fallback applied when nothing matches."""

    rules: list[Rule] = field(default_factory=list)
    default_profile: int = 0
    default_dpi: int | None = None
    default_effect: str | None = None

    @classmethod
    def from_config(cls, cfg: Mapping) -> RuleSet:
        if not isinstance(cfg, Mapping):
            raise ValueError(f"config must be an object, got {cfg!r}")
        profile = _opt_int(cfg.get("default_profile", 0), "default_profile")
        if not 0 <= (profile or 0) < PROFILE_COUNT:
            raise ValueError(
                f"default_profile {profile} out of range "
                f"0..{PROFILE_COUNT - 1}")
        raw = cfg.get("rules") or []
        if not isinstance(raw, (list, tuple)):
            raise ValueError("rules must be a list")
        effect = cfg.get("default_effect")
        if effect is not None and not isinstance(effect, str):
            raise ValueError(f"default_effect must be a string, got {effect!r}")
        return cls(
            rules=[Rule.from_dict(r) for r in raw],
            default_profile=profile or 0,
            default_dpi=_opt_int(cfg.get("default_dpi"), "default_dpi"),
            default_effect=effect,
        )

    def to_config(self) -> dict:
        d: dict = {"default_profile": self.default_profile,
                   "rules": [r.to_dict() for r in self.rules]}
        if self.default_dpi is not None:
            d["default_dpi"] = self.default_dpi
        if self.default_effect is not None:
            d["default_effect"] = self.default_effect
        return d

    def default_action(self) -> Action:
        return Action(profile=self.default_profile, dpi=self.default_dpi,
                      effect=self.default_effect, rule="", source="default")

    def _action(self, rule: Rule, source: str) -> Action:
        # Unset dpi/effect fall back to the defaults rather than keeping
        # whatever the previous app left behind.
        return Action(
            profile=rule.profile,
            dpi=self.default_dpi if rule.dpi is None else rule.dpi,
            effect=self.default_effect if rule.effect is None else rule.effect,
            rule=rule.label, source=source)

    def match(self, processes: Iterable[Process],
              foreground: Process | None = None) -> Action:
        """First matching rule wins; focus beats mere presence.

        Checking the focused app first means a rule for a game applies while
        the game is in front, even though a background launcher also matches
        an earlier rule. When focus is unknown (the usual Wayland case) this
        degrades to presence, so rule order is all that decides.
        """
        if foreground is not None:
            for rule in self.rules:
                if rule.matches(foreground):
                    return self._action(rule, "foreground")
        procs = list(processes)
        for rule in self.rules:
            for p in procs:
                if rule.matches(p):
                    return self._action(rule, "process")
        return self.default_action()


def _coerce_ruleset(rules: object) -> RuleSet:
    if rules is None:
        return RuleSet()
    if isinstance(rules, RuleSet):
        return rules
    if isinstance(rules, Mapping):
        return RuleSet.from_config(rules)
    if isinstance(rules, Sequence):
        return RuleSet(rules=[r if isinstance(r, Rule) else Rule.from_dict(r)
                              for r in rules])
    raise ValueError(f"cannot read rules from {rules!r}")


# -- foreground detection --------------------------------------------------

class _ProbeUnavailable(Exception):
    """This mechanism is not usable here (not merely "nothing focused")."""


@dataclass
class _Strategy:
    name: str
    fn: Callable[[], Process | None]
    next_try: float = 0.0


def _first_json(text: str, prefer: str = "object") -> object | None:
    """Pull the JSON payload out of gdbus's `(true, '...')` tuple syntax.

    gdbus prints GVariant, not JSON, so the payload is sliced out rather than
    parsed. `prefer` decides which bracket is tried first: a one-element array
    slices into a valid object, and an array of several objects slices into
    invalid JSON, so the caller has to say which shape it expects.
    """
    pairs = (("[", "]"), ("{", "}")) if prefer == "array" else \
            (("{", "}"), ("[", "]"))
    for opener, closer in pairs:
        i, j = text.find(opener), text.rfind(closer)
        if 0 <= i < j:
            try:
                return json.loads(text[i:j + 1])
            except json.JSONDecodeError:
                continue
    return None


# The shell's own JS: the only exact answer available under Wayland.
_EVAL_JS = (
    "const w = global.get_window_actors().map(a => a.meta_window)"
    ".find(w => w.has_focus());"
    "w ? JSON.stringify({pid: w.get_pid(), wm_class: w.get_wm_class()}) : ''"
)


class ForegroundProbe:
    """Best-effort "what has focus right now", tried in order:

      1. org.gnome.Shell.Eval — exact, but GNOME 41+ refuses it unless
         unsafe-mode is on, so it normally fails and that is fine.
      2. A cooperating shell extension: Window Calls (ickyicky) or
         focused-window-dbus (flexagoon). Only present if the user installed
         one; their bus names are fixed, so probing is cheap.
      3. Nothing. The watcher then runs on process presence alone. That is
         the documented limit of Wayland, not a defect to work around.

    A mechanism that reports itself unavailable is not retried for
    `retry` seconds — the usual case is "permanently unavailable", and
    shelling out to gdbus every tick would be pure waste.
    """

    def __init__(self, retry: float = PROBE_RETRY, timeout: float = 1.0,
                 proc_root: str = "/proc",
                 clock: Callable[[], float] = time.monotonic):
        self.retry = retry
        self.timeout = timeout
        self.proc_root = proc_root
        self.clock = clock
        self.mechanism: str | None = None  # last one that answered
        self._strategies = [
            _Strategy("gnome-shell-eval", self._via_eval),
            _Strategy("gnome-window-calls", self._via_window_calls),
            _Strategy("gnome-focused-window", self._via_focused_window),
        ]

    @property
    def available(self) -> bool:
        """True once some mechanism has actually answered."""
        return self.mechanism is not None

    def poll(self) -> Process | None:
        now = self.clock()
        for s in self._strategies:
            if now < s.next_try:
                continue
            try:
                p = s.fn()
            except _ProbeUnavailable:
                s.next_try = now + self.retry
                continue
            except Exception:
                # These are side doors we do not control; a surprise from one
                # must not take down profile switching.
                s.next_try = now + self.retry
                continue
            s.next_try = 0.0
            self.mechanism = s.name
            return p  # may be None: it answered, nothing is focused
        return None

    # -- mechanisms --------------------------------------------------------
    def _gdbus(self, *args: str) -> str:
        try:
            r = subprocess.run(("gdbus", "call", "--session", *args),
                               capture_output=True, text=True,
                               timeout=self.timeout)
        except FileNotFoundError:
            raise _ProbeUnavailable("gdbus not installed (glib2 tools)")
        except (OSError, subprocess.SubprocessError):
            raise _ProbeUnavailable("gdbus call failed")
        if r.returncode != 0:
            raise _ProbeUnavailable(r.stderr.strip() or "gdbus returned error")
        return r.stdout

    def _via_eval(self) -> Process | None:
        out = self._gdbus("--dest", "org.gnome.Shell",
                          "--object-path", "/org/gnome/Shell",
                          "--method", "org.gnome.Shell.Eval", _EVAL_JS)
        # Eval answers (false, '...') when it is disabled; the call itself
        # still succeeds, so the success flag has to be read out of stdout.
        if not out.lstrip().startswith("(true"):
            raise _ProbeUnavailable("Shell.Eval disabled (no unsafe-mode)")
        d = _first_json(out)
        if not isinstance(d, Mapping):
            return None  # nothing focused
        return self._resolve(d.get("pid"), d.get("wm_class"))

    def _via_window_calls(self) -> Process | None:
        out = self._gdbus("--dest", "org.gnome.Shell",
                          "--object-path", "/org/gnome/Shell/Extensions/Windows",
                          "--method", "org.gnome.Shell.Extensions.Windows.List")
        windows = _first_json(out, prefer="array")
        if isinstance(windows, Mapping):
            windows = [windows]  # a lone window slices out as an object
        if not isinstance(windows, list):
            raise _ProbeUnavailable("Window Calls returned no window list")
        for w in windows:
            if isinstance(w, Mapping) and w.get("focus"):
                return self._resolve(w.get("pid"), w.get("wm_class"))
        return None

    def _via_focused_window(self) -> Process | None:
        out = self._gdbus(
            "--dest", "org.gnome.Shell",
            "--object-path", "/org/gnome/shell/extensions/FocusedWindow",
            "--method", "org.gnome.shell.extensions.FocusedWindow.Get")
        d = _first_json(out)
        if not isinstance(d, Mapping):
            raise _ProbeUnavailable("focused-window-dbus returned no object")
        # This extension reports no pid, only the window class.
        return self._resolve(d.get("pid"), d.get("wm_class") or d.get("class"))

    def _resolve(self, pid: object, wm_class: object) -> Process | None:
        if isinstance(pid, int) and pid > 0:
            p = read_process(pid, self.proc_root)
            if p is not None:
                return p
        if isinstance(wm_class, str) and wm_class.strip():
            return Process.synthetic(wm_class)
        return None


# -- applying --------------------------------------------------------------

class DeviceApplier:
    """Default Action -> device translation, for a caller that only has a dev.

    The daemon normally passes its own callback, because effects need a
    long-running engine it already owns. Imports are deferred so importing
    appwatch never pulls in device code.
    """

    def __init__(self, dev, verbose: bool = False):
        self.dev = dev
        self.verbose = verbose
        self._profiles = None
        self._entry: int | None = None
        self._warned_effect = False

    def _log(self, msg: str) -> None:
        if self.verbose:
            print(f"[g502-watch] {msg}", flush=True)

    def profiles(self):
        if self._profiles is None:
            from .profiles import OnboardProfiles
            self._profiles = OnboardProfiles(self.dev)
            self._entry = self._profiles.current()
        return self._profiles

    def __call__(self, action: Action) -> None:
        self.profiles().set_current(action.profile)
        if action.dpi:
            from .dpi import Dpi
            self._log(f"dpi -> {Dpi(self.dev).set(action.dpi)}")
        if action.effect and not self._warned_effect:
            self._warned_effect = True
            self._log(f"effect '{action.effect}' ignored: effects need the "
                      f"daemon's engine, not a one-shot write")

    def restore(self) -> None:
        """Put back the profile that was current when we started."""
        if self._profiles is None or self._entry is None:
            return
        self._profiles.set_current(self._entry)
        self._log(f"restored profile {self._entry}")


# -- the watcher -----------------------------------------------------------

class Watcher:
    """Polls, debounces, and calls `apply` when the wanted Action changes.

    `apply` is a callable taking one Action; passing an open device instead is
    supported for the CLI's sake and wraps it in DeviceApplier.
    """

    def __init__(self, apply: Callable[[Action], None] | object,
                 rules: object = None, *,
                 interval: float = DEFAULT_INTERVAL,
                 debounce: float = DEFAULT_DEBOUNCE,
                 verbose: bool = False,
                 snapshot: Callable[[], list[Process]] | None = None,
                 foreground: Callable[[], Process | None] | None = None,
                 use_foreground: bool = True,
                 on_exit: Callable[[], None] | None = None,
                 clock: Callable[[], float] = time.monotonic,
                 sleep: Callable[[float], None] = time.sleep):
        cfg = rules if isinstance(rules, Mapping) else {}
        self.rules = _coerce_ruleset(rules)
        if not callable(apply) and hasattr(apply, "call"):
            # Compat with `Watcher(dev, cfg)`: the CLI has a device, not a
            # policy. New callers pass a callback and keep this module clean.
            applier = DeviceApplier(apply, verbose=verbose)
            apply = applier
            if on_exit is None and cfg.get("restore_on_exit", True):
                on_exit = applier.restore
        if not callable(apply):
            raise ValueError("apply must be callable or an open device")
        self.apply = apply
        self.interval = max(0.1, float(interval))
        self.debounce = max(0.0, float(debounce))
        self.verbose = verbose
        self.on_exit = on_exit
        self.clock = clock
        self.sleep = sleep
        self._snapshot = snapshot or snapshot_processes
        self.probe = ForegroundProbe(clock=clock) if (
            foreground is None and use_foreground) else None
        self._foreground = foreground or (
            self.probe.poll if self.probe is not None else None)
        self.applied: Action | None = None
        self.pending: Action | None = None
        self.pending_since: float = 0.0

    def _log(self, msg: str) -> None:
        if self.verbose:
            print(f"[g502-watch] {msg}", flush=True)

    def wanted(self) -> Action:
        """The Action the current system state asks for."""
        fg = self._foreground() if self._foreground is not None else None
        return self.rules.match(self._snapshot(), fg)

    def tick(self) -> Action | None:
        """One poll. Returns the Action applied this tick, else None."""
        return self._settle(self.wanted())

    def _settle(self, want: Action) -> Action | None:
        now = self.clock()
        if want == self.applied:
            self.pending = None  # the status quo is still right
            return None
        if want != self.pending:
            # Hysteresis: a candidate has to survive `debounce` seconds, so a
            # process that flashes past (a build step, an installer helper)
            # cannot thrash the profile.
            self.pending, self.pending_since = want, now
        if now - self.pending_since < self.debounce:
            return None
        return self._fire(want)

    def _fire(self, want: Action) -> Action | None:
        try:
            self.apply(want)
        except Exception as e:
            # The callback owns the device, so its failures (unplugged mouse,
            # a timeout) land here. Leave `applied` alone and let the next
            # debounce window retry.
            self._log(f"apply failed for profile {want.profile}: {e}")
            self.pending, self.pending_since = want, self.clock()
            return None
        self.applied = want
        self.pending = None
        detail = "".join([f" dpi {want.dpi}" if want.dpi else "",
                          f" effect {want.effect}" if want.effect else ""])
        self._log(f"{want.source}:{want.rule or 'fallback'} -> "
                  f"profile {want.profile}{detail}")
        return want

    def run(self, stop: Callable[[], bool] | None = None,
            ticks: int | None = None) -> None:
        """Poll until `stop()` is true, `ticks` polls are done, or Ctrl-C."""
        mech = self.probe.mechanism if self.probe else None
        self._log(f"watching {len(self.rules.rules)} rule(s) every "
                  f"{self.interval}s, debounce {self.debounce}s, "
                  f"foreground: {mech or 'process-presence only'}")
        done = 0
        try:
            while not (stop is not None and stop()):
                self.tick()
                done += 1
                if ticks is not None and done >= ticks:
                    break
                self.sleep(self.interval)
        except KeyboardInterrupt:
            pass
        finally:
            if self.on_exit is not None:
                try:
                    self.on_exit()
                except Exception as e:
                    self._log(f"exit handler failed: {e}")
