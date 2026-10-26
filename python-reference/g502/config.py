"""Configuration persistence: $XDG_CONFIG_HOME/g502/config.json.

Falls back to ~/.config/g502/config.json. JSON, not TOML: the stdlib can only
read TOML, and a file the daemon and GUI both write has to be machine-writable.

Two properties matter more here than schema elegance, because the watcher
daemon reads this file at every start and rewrites it whenever a setting
changes:

  * load() never raises. A truncated, empty or hand-mangled file degrades to
    defaults plus a list of warnings, so one bad edit cannot stop the daemon
    coming up.
  * save() is atomic -- temp file in the same directory, fsync, os.replace --
    so a crash or a full disk mid-write leaves the previous config intact.

Unknown keys survive a load/save round trip instead of being dropped, and a
file written by a newer schema is reported and refused rather than quietly
downgraded.

Appwatch rules stay plain dicts, the shape appwatch.Rule.from_dict() takes, and
Config is itself a Mapping, so appwatch.RuleSet.from_config(cfg) and the
daemon's cfg.get(...) work on a loaded config directly. Not importing appwatch
here keeps the rule schema in one place and keeps this module free of the
device stack, so a GUI can read a config without touching hardware.
"""
from __future__ import annotations

import contextlib
import json
import os
import tempfile
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path

SCHEMA_VERSION = 1

APP_DIR = "g502"
FILE_NAME = "config.json"

# The firmware implements only these four LED modes; see led.py.
LED_MODES = ("off", "fixed", "cycle", "breathing")

FPS_MIN, FPS_MAX = 1, 120  # matches effects.Engine's own clamp


class ConfigError(Exception):
    """Raised by save() only. load() reports problems in Config.warnings."""


def config_path() -> Path:
    """Resolve the config location per XDG, re-reading the environment.

    Resolved per call rather than at import so a test (or a service file) can
    point XDG_CONFIG_HOME somewhere else. Per the XDG basedir spec an unset,
    empty or relative value is ignored.
    """
    base = os.environ.get("XDG_CONFIG_HOME") or ""
    root = Path(base) if base.startswith("/") else Path.home() / ".config"
    return root / APP_DIR / FILE_NAME


DEFAULT_PATH = config_path()  # convenience constant; prefer config_path()


# -- validation helpers ----------------------------------------------------
#
# Every coercer takes the warning list and falls back to a default instead of
# raising: a single bad value must not cost the user the whole file.

def _split(data: Mapping, known: tuple[str, ...]) -> tuple[dict, dict]:
    """Partition a mapping into recognised and unrecognised keys."""
    recognised, rest = {}, {}
    for key, val in data.items():
        (recognised if key in known else rest)[str(key)] = val
    return recognised, rest


def _as_int(val, default: int, name: str, warn: list[str],
            lo: int | None = None, hi: int | None = None) -> int:
    if isinstance(val, bool) or not isinstance(val, int):
        if isinstance(val, float) and val.is_integer():
            val = int(val)  # JSON editors happily turn 1600 into 1600.0
        else:
            warn.append(f"{name}: expected an integer, got {val!r}; "
                        f"using {default}")
            return default
    if (lo is not None and val < lo) or (hi is not None and val > hi):
        warn.append(f"{name}: {val} out of range {lo}..{hi}; using {default}")
        return default
    return val


def _as_opt_int(val, name: str, warn: list[str],
                lo: int | None = None) -> int | None:
    """None means "leave the device alone", so it is a real value here."""
    if val is None:
        return None
    out = _as_int(val, -1, name, warn, lo=lo)
    return None if out == -1 else out


def _as_bool(val, default: bool, name: str, warn: list[str]) -> bool:
    if isinstance(val, bool):
        return val
    warn.append(f"{name}: expected true/false, got {val!r}; using {default}")
    return default


def _as_str(val, default: str, name: str, warn: list[str],
            allowed: tuple[str, ...] | None = None) -> str:
    if not isinstance(val, str):
        warn.append(f"{name}: expected a string, got {val!r}; using {default}")
        return default
    if allowed is not None and val not in allowed:
        warn.append(f"{name}: {val!r} is not one of {allowed}; "
                    f"using {default}")
        return default
    return val


def _as_dict(val, name: str, warn: list[str]) -> dict:
    if isinstance(val, dict):
        return dict(val)
    warn.append(f"{name}: expected an object, got {val!r}; ignored")
    return {}


def _as_list(val, name: str, warn: list[str]) -> list:
    if isinstance(val, list):
        return list(val)
    warn.append(f"{name}: expected a list, got {val!r}; ignored")
    return []


# -- schema ----------------------------------------------------------------

@dataclass
class LedPref:
    """Wanted state of one RGB zone (zone 0 = logo, zone 1 = DPI indicator)."""

    mode: str = "fixed"
    color: str = "#ff0000"       # as led.parse_color() accepts
    period_ms: int = 5000        # breathing/cycle only
    waveform: str = "default"    # breathing only; see led.WAVEFORMS
    intensity: int = 100
    extra: dict = field(default_factory=dict, repr=False)

    KNOWN = ("mode", "color", "period_ms", "waveform", "intensity")

    @classmethod
    def from_dict(cls, data: Mapping, name: str,
                  warn: list[str]) -> "LedPref":
        known, extra = _split(data, cls.KNOWN)
        d = cls()
        return cls(
            mode=_as_str(known.get("mode", d.mode), d.mode, f"{name}.mode",
                         warn, allowed=LED_MODES),
            color=_as_str(known.get("color", d.color), d.color,
                          f"{name}.color", warn),
            period_ms=_as_int(known.get("period_ms", d.period_ms),
                              d.period_ms, f"{name}.period_ms", warn, lo=1),
            waveform=_as_str(known.get("waveform", d.waveform), d.waveform,
                             f"{name}.waveform", warn),
            intensity=_as_int(known.get("intensity", d.intensity),
                              d.intensity, f"{name}.intensity", warn,
                              lo=0, hi=100),
            extra=extra,
        )

    def to_dict(self) -> dict:
        out = dict(self.extra)
        out.update(mode=self.mode, color=self.color,
                   period_ms=self.period_ms, waveform=self.waveform,
                   intensity=self.intensity)
        return out


@dataclass
class ProfilePrefs:
    """Host-side preferences for one onboard profile slot.

    DPI and rate are left unvalidated against the hardware's own limits: dpi.py
    and rate.py query the device and are the authority, so a value is only
    rejected here if it cannot be a setting at all.
    """

    dpi: int | None = None
    rate: int | None = None                                  # Hz
    leds: list[LedPref] = field(default_factory=list)         # index = zone
    extra: dict = field(default_factory=dict, repr=False)

    KNOWN = ("dpi", "rate", "leds")

    @classmethod
    def from_dict(cls, data: Mapping, name: str,
                  warn: list[str]) -> "ProfilePrefs":
        known, extra = _split(data, cls.KNOWN)
        leds = []
        for i, item in enumerate(_as_list(known.get("leds", []),
                                          f"{name}.leds", warn)):
            if isinstance(item, dict):
                leds.append(LedPref.from_dict(item, f"{name}.leds[{i}]", warn))
            else:
                warn.append(f"{name}.leds[{i}]: expected an object, "
                            f"got {item!r}; ignored")
        return cls(
            dpi=_as_opt_int(known.get("dpi"), f"{name}.dpi", warn, lo=1),
            rate=_as_opt_int(known.get("rate"), f"{name}.rate", warn, lo=1),
            leds=leds,
            extra=extra,
        )

    def to_dict(self) -> dict:
        out = dict(self.extra)
        if self.dpi is not None:
            out["dpi"] = self.dpi
        if self.rate is not None:
            out["rate"] = self.rate
        if self.leds:
            out["leds"] = [led.to_dict() for led in self.leds]
        return out


@dataclass
class Config(Mapping):
    version: int = SCHEMA_VERSION
    default_profile: int = 0
    restore_on_exit: bool = True
    effect: str | None = None            # key into effects.EFFECTS
    effect_params: dict = field(default_factory=dict)  # kwargs for it
    effect_fps: int = 60
    profiles: dict[int, ProfilePrefs] = field(default_factory=dict)
    rules: list[dict] = field(default_factory=list)      # appwatch rules
    extra: dict = field(default_factory=dict, repr=False)

    # Not persisted, and excluded from equality so a round trip compares equal.
    path: Path | None = field(default=None, compare=False, repr=False)
    warnings: list[str] = field(default_factory=list, compare=False,
                                repr=False)

    KNOWN = ("version", "default_profile", "restore_on_exit", "effect",
             "effect_params", "effect_fps", "profiles", "rules")

    @classmethod
    def from_dict(cls, data: Mapping, path: Path | None = None,
                  warn: list[str] | None = None) -> "Config":
        warn = [] if warn is None else warn
        known, extra = _split(data, cls.KNOWN)
        d = cls()

        profiles: dict[int, ProfilePrefs] = {}
        for key, val in _as_dict(known.get("profiles", {}), "profiles",
                                 warn).items():
            try:
                idx = int(key)  # JSON object keys are strings
            except (TypeError, ValueError):
                warn.append(f"profiles[{key!r}]: not a profile index; ignored")
                continue
            if isinstance(val, dict):
                profiles[idx] = ProfilePrefs.from_dict(
                    val, f"profiles[{idx}]", warn)
            else:
                warn.append(f"profiles[{idx}]: expected an object, "
                            f"got {val!r}; ignored")

        rules = []
        for i, rule in enumerate(_as_list(known.get("rules", []), "rules",
                                          warn)):
            if isinstance(rule, dict):
                rules.append(dict(rule))
            else:
                warn.append(f"rules[{i}]: expected an object, "
                            f"got {rule!r}; ignored")

        effect = known.get("effect")
        if effect is not None and not isinstance(effect, str):
            warn.append(f"effect: expected a name, got {effect!r}; ignored")
            effect = None

        return cls(
            version=_as_int(known.get("version", SCHEMA_VERSION),
                            SCHEMA_VERSION, "version", warn, lo=1),
            # No upper bound: the device exposes ROM profiles above the five
            # onboard slots, and profiles.py knows the real count.
            default_profile=_as_int(known.get("default_profile",
                                              d.default_profile),
                                    d.default_profile, "default_profile",
                                    warn, lo=0),
            restore_on_exit=_as_bool(known.get("restore_on_exit",
                                               d.restore_on_exit),
                                     d.restore_on_exit, "restore_on_exit",
                                     warn),
            effect=effect,
            effect_params=_as_dict(known.get("effect_params", {}),
                                   "effect_params", warn),
            effect_fps=_as_int(known.get("effect_fps", d.effect_fps),
                               d.effect_fps, "effect_fps", warn,
                               lo=FPS_MIN, hi=FPS_MAX),
            profiles=profiles,
            rules=rules,
            extra=extra,
            path=path,
            warnings=warn,
        )

    def to_dict(self) -> dict:
        """Serialisable form. Unknown keys go in first so known ones win."""
        out = dict(self.extra)
        out.update({
            "version": self.version,
            "default_profile": self.default_profile,
            "restore_on_exit": self.restore_on_exit,
            "effect_params": self.effect_params,
            "effect_fps": self.effect_fps,
            "profiles": {str(i): p.to_dict()
                         for i, p in sorted(self.profiles.items())},
            "rules": [dict(r) for r in self.rules],
        })
        if self.effect is not None:
            out["effect"] = self.effect
        return out

    # The daemon, appwatch and g502ctl all read the config as a mapping
    # (cfg.get(...), RuleSet.from_config(cfg)), so Config *is* one: a Config
    # goes wherever a dict used to, unknown keys included. Dataclass equality
    # still wins over Mapping's, so a round trip compares by field.
    def __getitem__(self, key: str):
        return self.to_dict()[key]

    def __iter__(self):
        return iter(self.to_dict())

    def __len__(self) -> int:
        return len(self.to_dict())

    def get(self, key: str, default=None):
        return self.to_dict().get(key, default)

    def prefs_for(self, profile: int) -> ProfilePrefs:
        """Preferences for a profile slot, creating empty ones on demand."""
        return self.profiles.setdefault(profile, ProfilePrefs())

    @property
    def is_future(self) -> bool:
        return self.version > SCHEMA_VERSION


DEFAULT_CONFIG = Config().to_dict()

# A starter file for `g502ctl config --init`: the rules and prefs double as the
# documentation for the format, which a bare default cannot do.
EXAMPLE_CONFIG = Config(
    # First matching rule wins, so list the specific ones first.
    rules=[{"match": ["csgo_linux64", "cs2"], "profile": 1},
           {"match": ["blender"], "profile": 2, "dpi": 1200}],
    profiles={0: ProfilePrefs(dpi=1600, rate=1000,
                              leds=[LedPref(color="#00aaff"),
                                    LedPref(mode="off")])},
).to_dict()


# -- migration -------------------------------------------------------------

# Keyed on the version being left, so each step only knows its own neighbour.
# Version 1 is the first schema, hence no entries yet.
MIGRATIONS: dict[int, Callable[[dict], tuple[dict, list[str]]]] = {}


def migrate(data: Mapping, from_version: int | None = None
            ) -> tuple[dict, list[str]]:
    """Step a raw mapping up to SCHEMA_VERSION, returning it and any notes.

    A newer-than-known version is passed through untouched: reading it with
    today's rules is fine, rewriting it is not (save() refuses).
    """
    out = dict(data)
    notes: list[str] = []
    version = from_version
    if version is None:
        version = out.get("version", SCHEMA_VERSION)
        if not isinstance(version, int) or isinstance(version, bool):
            notes.append(f"version: expected an integer, got {version!r}; "
                         f"assuming {SCHEMA_VERSION}")
            version = SCHEMA_VERSION
    if version > SCHEMA_VERSION:
        notes.append(f"config schema v{version} is newer than the supported "
                     f"v{SCHEMA_VERSION}: unrecognised settings are kept, and "
                     f"saving is refused unless forced")
        return out, notes
    while version < SCHEMA_VERSION:
        step = MIGRATIONS.get(version)
        if step is None:
            notes.append(f"no migration from schema v{version}; reading "
                         f"whatever is still recognisable")
            break
        out, step_notes = step(out)
        notes += step_notes
        version += 1
    out["version"] = version  # never claim a version we did not reach
    return out, notes


# -- load / save -----------------------------------------------------------

def load(path: str | os.PathLike | None = None) -> Config:
    """Read the config, falling back to defaults for anything unusable.

    Never raises: the caller inspects Config.warnings to see what was ignored.
    """
    p = Path(path) if path else config_path()
    warn: list[str] = []
    try:
        raw = p.read_text(encoding="utf-8")
    except FileNotFoundError:
        return Config(path=p)  # not an error: first run
    except (OSError, UnicodeDecodeError) as e:
        warn.append(f"{p}: unreadable ({e}); using defaults")
        return Config(path=p, warnings=warn)

    if not raw.strip():
        warn.append(f"{p}: empty; using defaults")
        return Config(path=p, warnings=warn)

    try:
        data = json.loads(raw)
    except (json.JSONDecodeError, RecursionError) as e:
        warn.append(f"{p}: invalid JSON ({e}); using defaults. The file was "
                    f"left as it is -- fix or delete it")
        return Config(path=p, warnings=warn)

    if not isinstance(data, dict):
        warn.append(f"{p}: top level is {type(data).__name__}, expected an "
                    f"object; using defaults")
        return Config(path=p, warnings=warn)

    # Last-resort guard: a config must not be able to stop the daemon starting,
    # whatever shape its contents turn out to have.
    try:
        data, notes = migrate(data)
        warn += notes
        return Config.from_dict(data, path=p, warn=warn)
    except Exception as e:  # noqa: BLE001 - deliberately broad; see above
        warn.append(f"{p}: could not be interpreted ({e!r}); using defaults")
        return Config(path=p, warnings=warn)


def save(cfg: Config | Mapping, path: str | os.PathLike | None = None,
         force: bool = False) -> Path:
    """Write the config atomically and return the path written.

    Atomic because the daemon rewrites this file while other processes read it:
    a reader sees either the old file or the new one, never a half-written one.
    """
    if isinstance(cfg, Config):
        data = cfg.to_dict()
        target = Path(path) if path else (cfg.path or config_path())
    else:
        data = dict(cfg)
        target = Path(path) if path else config_path()

    version = data.get("version", SCHEMA_VERSION)
    if isinstance(version, int) and version > SCHEMA_VERSION and not force:
        raise ConfigError(
            f"{target}: was written by schema v{version}, newer than the "
            f"supported v{SCHEMA_VERSION}; refusing to overwrite it and lose "
            f"settings (pass force=True to insist)")

    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=target.parent, prefix=target.name + ".",
                               suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(data, fh, indent=2)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())  # replace() is atomic but not durable alone
        # mkstemp gives 0600; the tool is often run under sudo and the config
        # has to stay readable by the user who owns the session.
        os.chmod(tmp, 0o644)
        os.replace(tmp, target)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp)  # leave no debris, and never a partial config
        raise
    return target
