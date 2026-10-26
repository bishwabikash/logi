"""GTK4 / libadwaita dependency probe.

Deliberately free of `gi` imports at module scope: the whole point is to be
importable on a machine that lacks PyGObject so the entry point can print an
apt command instead of an ImportError traceback.
"""
from __future__ import annotations

from dataclasses import dataclass

# Debian / Kali package names.
APT_PACKAGES = ("python3-gi", "gir1.2-gtk-4.0", "gir1.2-adw-1")
APT_COMMAND = "sudo apt install " + " ".join(APT_PACKAGES)

# Minimums are set by the widgets actually used, not by taste:
#   Gtk.ColorDialogButton, Gtk.FileDialog          -> GTK 4.10
#   Adw.ToolbarView, NavigationSplitView, SpinRow  -> libadwaita 1.4
#   Adw.ButtonRow                                  -> libadwaita 1.6
MIN_GTK = (4, 10)
MIN_ADW = (1, 4)
ADW_BUTTON_ROW = (1, 6)


@dataclass(frozen=True, slots=True)
class DepStatus:
    ok: bool
    gtk: tuple[int, int] | None = None
    adw: tuple[int, int] | None = None
    problem: str | None = None

    @property
    def has_button_row(self) -> bool:
        return bool(self.adw and self.adw >= ADW_BUTTON_ROW)

    def report(self) -> str:
        if self.ok:
            return (f"GTK {self.gtk[0]}.{self.gtk[1]}, "
                    f"libadwaita {self.adw[0]}.{self.adw[1]}")
        lines = [f"g502-gui: {self.problem}", "", "Install the dependencies:",
                 f"    {APT_COMMAND}"]
        if self.gtk:
            lines.append(f"\n(found GTK {self.gtk[0]}.{self.gtk[1]}, "
                         f"need {MIN_GTK[0]}.{MIN_GTK[1]}+)")
        if self.adw:
            lines.append(f"(found libadwaita {self.adw[0]}.{self.adw[1]}, "
                         f"need {MIN_ADW[0]}.{MIN_ADW[1]}+)")
        return "\n".join(lines)


def check() -> DepStatus:
    """Probe for PyGObject, GTK4 and libadwaita without raising."""
    try:
        import gi
    except ImportError:
        return DepStatus(False, problem="PyGObject is not installed")

    try:
        gi.require_version("Gtk", "4.0")
    except ValueError:
        return DepStatus(False, problem="GTK 4 GObject bindings are not installed")
    try:
        gi.require_version("Adw", "1")
    except ValueError:
        return DepStatus(False, problem="libadwaita GObject bindings are not installed")

    try:
        from gi.repository import Adw, Gtk
    except ImportError as e:
        return DepStatus(False, problem=f"cannot load the GTK4/libadwaita bindings: {e}")

    gtk = (Gtk.MAJOR_VERSION, Gtk.MINOR_VERSION)
    adw = (Adw.MAJOR_VERSION, Adw.MINOR_VERSION)
    if gtk < MIN_GTK:
        return DepStatus(False, gtk, adw, "GTK is too old")
    if adw < MIN_ADW:
        return DepStatus(False, gtk, adw, "libadwaita is too old")
    return DepStatus(True, gtk, adw)
