"""Config file handling: ~/.config/g502/config.json"""
from __future__ import annotations

import json
import os
from pathlib import Path

DEFAULT_PATH = Path(
    os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")
) / "g502" / "config.json"

DEFAULT_CONFIG = {
    "default_profile": 0,
    "restore_on_exit": True,
    # First matching rule wins, so list the specific ones first.
    "rules": [
        {"match": ["csgo_linux64", "cs2"], "profile": 1,
         "comment": "example: a game by process name"},
        {"match": ["blender"], "profile": 2, "dpi": 1200,
         "comment": "example: also force a DPI"},
    ],
}


def load(path: Path | None = None) -> dict:
    p = Path(path) if path else DEFAULT_PATH
    if not p.exists():
        return dict(DEFAULT_CONFIG)
    try:
        with open(p) as fh:
            cfg = json.load(fh)
    except (OSError, json.JSONDecodeError) as e:
        raise SystemExit(f"g502: cannot read config {p}: {e}")
    if not isinstance(cfg, dict):
        raise SystemExit(f"g502: config {p} must be a JSON object")
    merged = dict(DEFAULT_CONFIG)
    merged.update(cfg)
    return merged


def save(cfg: dict, path: Path | None = None) -> Path:
    p = Path(path) if path else DEFAULT_PATH
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(".tmp")
    with open(tmp, "w") as fh:
        json.dump(cfg, fh, indent=2)
        fh.write("\n")
    tmp.replace(p)  # atomic: never leave a half-written config
    return p
