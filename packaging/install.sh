#!/usr/bin/env bash
# Install the g502 binary, the udev rule and the desktop entry.
#
# Two separate things, either of which can be skipped:
#   /etc/udev/rules.d/99-logitech-g502.rules   (needs sudo) so g502 can open
#                                              the mouse without root
#   $PREFIX/bin/g502 + a .desktop entry        (sudo only if PREFIX needs it)
#
# Safe to re-run.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DO_UDEV=1
DO_BIN=1

usage() {
    sed -n '2,9p' "$0" | sed 's/^# \?//'
    cat <<USAGE

Usage: packaging/install.sh [--no-udev] [--no-bin] [--help]
Env:
  PREFIX   where the binary and desktop file go (default: \$HOME/.local)
USAGE
}

for arg in "$@"; do
    case "$arg" in
        --no-udev) DO_UDEV=0 ;;
        --no-bin)  DO_BIN=0 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option $arg" >&2; usage; exit 2 ;;
    esac
done

BIN="$ROOT/target/release/g502"
if [ "$DO_BIN" = 1 ] && [ ! -x "$BIN" ]; then
    echo "install.sh: $BIN not built yet. Run:" >&2
    echo "    cargo build --release" >&2
    exit 1
fi

if [ "$DO_UDEV" = 1 ]; then
    RULE=/etc/udev/rules.d/99-logitech-g502.rules
    if cmp -s "$HERE/99-logitech-g502.rules" "$RULE" 2>/dev/null; then
        echo "udev rule already current: $RULE"
    else
        echo "Installing the udev rule to $RULE (needs sudo)."
        sudo install -m 0644 "$HERE/99-logitech-g502.rules" "$RULE"
        sudo udevadm control --reload-rules
        sudo udevadm trigger --subsystem-match=hidraw
        echo "udev rule installed."
        echo
        echo "  >>> NOW UNPLUG AND REPLUG THE MOUSE. <<<"
        echo "  udevadm trigger re-runs the rules, but a node that is already"
        echo "  open keeps its old permissions, so the replug is what matters."
        echo
    fi
fi

if [ "$DO_BIN" = 1 ]; then
    install -d "$PREFIX/bin" "$PREFIX/share/applications"
    install -m 0755 "$BIN" "$PREFIX/bin/g502"
    install -m 0644 "$HERE/g502.desktop" "$PREFIX/share/applications/g502.desktop"
    echo "Installed $PREFIX/bin/g502 and the desktop entry."
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) echo "Note: $PREFIX/bin is not on your PATH." ;;
    esac
    command -v update-desktop-database >/dev/null 2>&1 &&
        update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
fi

echo
echo "Done. Run 'g502' for the app, or 'g502 --help' for the command line."
