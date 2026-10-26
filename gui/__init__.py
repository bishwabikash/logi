"""GTK4 + libadwaita management app for the Logitech G502 HERO.

Layout of this package:

    deps.py      dependency probe; importable without gi so a missing
                 PyGObject produces an apt command, not a traceback
    model.py     pure logic (no gi, no hardware) — settings dataclasses,
                 rule formatting, DPI snapping. This is what the unit
                 tests cover.
    optional.py  defensive wrappers around the g502 modules that may be
                 absent or half-written
    device.py    the worker thread that owns the hidraw fd
    window.py    the window, sidebar and status page
    pages/       one Adw.PreferencesPage per sidebar entry
    fake.py      a fake device session so the widget tree can be built
                 and exercised without hardware
"""
from __future__ import annotations

import sys
from pathlib import Path

__version__ = "0.1.0"
APP_ID = "dev.g502.Manager"

# The app is normally run straight out of the checkout ("python3 -m gui"),
# where the g502 package sits next to this one but is not installed. Mirror
# what g502ctl does rather than requiring a PYTHONPATH.
_ROOT = Path(__file__).resolve().parent.parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))
