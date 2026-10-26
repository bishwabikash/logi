"""Defensive access to the g502 modules that may be absent or half-written.

effects, config, appwatch and profile_io are written independently of this
app. A GUI that refuses to start because one of them is mid-edit is useless,
so every one of them is imported through here: the import failure is recorded
and the matching page degrades to a message instead of crashing.
"""
from __future__ import annotations

import importlib
import json
from typing import Any

_cache: dict[str, Any] = {}
_errors: dict[str, str] = {}


def module(name: str):
    """Import g502.<name> once, returning None if it cannot be imported.

    Catches Exception rather than ImportError on purpose: a module being
    edited right now can fail with a SyntaxError or raise at import time.
    """
    if name in _cache:
        return _cache[name]
    try:
        mod = importlib.import_module(f"g502.{name}")
    except Exception as e:  # noqa: BLE001 - see docstring
        _errors[name] = f"{type(e).__name__}: {e}"
        mod = None
    _cache[name] = mod
    return mod


def missing() -> dict[str, str]:
    """Modules that failed to import, name -> reason."""
    return dict(_errors)


def unavailable(name: str) -> str:
    reason = _errors.get(name, "not available")
    return f"g502/{name}.py is unavailable ({reason})"


# -- config -----------------------------------------------------------------

_FALLBACK_CONFIG = {"default_profile": 0, "restore_on_exit": True, "rules": []}


def load_config() -> tuple[dict, str | None]:
    """(config, error). Never raises: config.load() exits the process on a
    bad file, which a GUI must not do."""
    cfg = module("config")
    if cfg is None:
        return dict(_FALLBACK_CONFIG), unavailable("config")
    try:
        return dict(cfg.load()), None
    except BaseException as e:  # config.load raises SystemExit on bad JSON
        return dict(_FALLBACK_CONFIG), f"cannot read config: {e}"


def save_config(data: dict) -> tuple[str | None, str | None]:
    """(path, error)."""
    cfg = module("config")
    if cfg is None:
        return None, unavailable("config")
    try:
        return str(cfg.save(data)), None
    except Exception as e:  # noqa: BLE001
        return None, f"cannot write config: {e}"


def config_path() -> str:
    cfg = module("config")
    return str(getattr(cfg, "DEFAULT_PATH", "~/.config/g502/config.json"))


# -- appwatch ---------------------------------------------------------------

def running_processes() -> set[str]:
    aw = module("appwatch")
    if aw is None:
        return set()
    try:
        return set(aw.running_processes())
    except Exception:  # noqa: BLE001
        return set()


def match_rule(cfg: dict, processes: set[str]) -> dict | None:
    aw = module("appwatch")
    if aw is None:
        return None
    try:
        return aw.match_rule(cfg, processes)
    except Exception:  # noqa: BLE001
        return None


# -- effects ----------------------------------------------------------------

def effect_names() -> list[str]:
    fx = module("effects")
    return sorted(getattr(fx, "EFFECTS", {})) if fx else []


def effect_engine(dev, fps: int):
    fx = module("effects")
    if fx is None or not hasattr(fx, "Engine"):
        raise RuntimeError(unavailable("effects"))
    return fx.Engine(dev, fps=fps)


def effect_generator(name: str, **kwargs):
    """Build a generator, passing only the keywords it actually accepts.

    The effect signatures differ (colour, two colours, zones, curve), and
    they are not this app's to change, so filter by introspection rather
    than hardcoding which effect takes what.
    """
    import inspect

    fx = module("effects")
    if fx is None:
        raise RuntimeError(unavailable("effects"))
    factory = getattr(fx, "EFFECTS", {}).get(name)
    if factory is None:
        raise RuntimeError(f"effect {name!r} is not available")
    try:
        accepted = set(inspect.signature(factory).parameters)
    except (TypeError, ValueError):
        accepted = set(kwargs)
    return factory(**{k: v for k, v in kwargs.items() if k in accepted})


# -- profile_io -------------------------------------------------------------
#
# profile_io owns the export format. Its public names are not fixed yet, so
# probe for the conventional ones and fall back to a raw-sector document that
# this app can always read back. The fallback is tagged so a later
# profile_io can recognise or reject it.

_EXPORT_NAMES = ("export_profile", "to_dict", "profile_to_dict", "dumps")
_IMPORT_NAMES = ("import_profile", "from_dict", "profile_from_dict", "loads")
FALLBACK_FORMAT = "g502-profile-raw/1"


def _entry(names: tuple[str, ...]):
    pio = module("profile_io")
    if pio is None:
        return None
    for n in names:
        fn = getattr(pio, n, None)
        if callable(fn):
            return fn
    return None


def have_profile_io() -> bool:
    return _entry(_EXPORT_NAMES) is not None and _entry(_IMPORT_NAMES) is not None


def export_json(profile, index: int) -> str:
    """Serialise a parsed Profile to JSON text."""
    fn = _entry(_EXPORT_NAMES)
    if fn is not None:
        try:
            out = fn(profile)
        except TypeError:
            out = fn(profile, index)  # some signatures take the slot too
        if isinstance(out, str):
            return out
        return json.dumps(out, indent=2) + "\n"

    blob = profile.serialize()
    doc = {
        "format": FALLBACK_FORMAT,
        "note": "written by gui/ because g502/profile_io.py was unavailable; "
                "sector is the source of truth, the rest is for reading",
        "slot": index,
        "name": profile.name,
        "report_rate_hz": profile.report_rate_hz,
        "dpi": list(profile.dpi),
        "default_dpi_index": profile.default_dpi,
        "shift_dpi_index": profile.shift_dpi,
        "buttons": [b.raw.hex() for b in profile.buttons],
        "leds": [led.raw.hex() for led in profile.leds],
        "alt_leds": [led.raw.hex() for led in profile.alt_leds],
        "sector": blob.hex(),
    }
    return json.dumps(doc, indent=2) + "\n"


def import_profile(text: str):
    """Parse exported JSON back into a Profile. Raises ValueError on junk."""
    fn = _entry(_IMPORT_NAMES)
    if fn is not None:
        try:
            return fn(text)
        except (TypeError, json.JSONDecodeError):
            return fn(json.loads(text))

    from g502.profile_blob import SECTOR_SIZE, Profile

    try:
        doc = json.loads(text)
    except json.JSONDecodeError as e:
        raise ValueError(f"not valid JSON: {e}") from None
    if not isinstance(doc, dict) or "sector" not in doc:
        raise ValueError("no 'sector' field: this file was not written by "
                         "this app's fallback exporter")
    try:
        blob = bytes.fromhex(doc["sector"])
    except (TypeError, ValueError):
        raise ValueError("'sector' is not hex") from None
    if len(blob) != SECTOR_SIZE:
        raise ValueError(f"sector is {len(blob)} bytes, need {SECTOR_SIZE}")
    return Profile.parse(blob)
