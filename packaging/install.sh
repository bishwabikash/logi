#!/usr/bin/env bash
# Install logi: the binary, the udev rules, the desktop entry and the user
# service.
#
# Three parts, each skippable:
#   /etc/udev/rules.d/...        (sudo) so the mouse and /dev/uinput are
#                                reachable without root
#   $PREFIX/bin/logi             the binary and its desktop entry
#   ~/.config/systemd/user/      the background service, enabled and started
#
# Safe to re-run.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DO_UDEV=1; DO_BIN=1; DO_SERVICE=1

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \?//'
    cat <<USAGE

Usage: packaging/install.sh [--no-udev] [--no-bin] [--no-service] [--help]
Env:
  PREFIX   where the binary and desktop file go (default: \$HOME/.local)
USAGE
}

for arg in "$@"; do
    case "$arg" in
        --no-udev)    DO_UDEV=0 ;;
        --no-bin)     DO_BIN=0 ;;
        --no-service) DO_SERVICE=0 ;;
        -h|--help)    usage; exit 0 ;;
        *) echo "install.sh: unknown option $arg" >&2; usage; exit 2 ;;
    esac
done

# A source checkout builds into target/release; a release tarball ships the
# binary next to packaging/.
BIN="$ROOT/target/release/logi"
[ -x "$BIN" ] || BIN="$ROOT/logi"
if { [ "$DO_BIN" = 1 ] || [ "$DO_SERVICE" = 1 ]; } && [ ! -x "$BIN" ]; then
    echo "install.sh: no logi binary found. Build one with:" >&2
    echo "    cargo build --release" >&2
    echo "or run this from an extracted release tarball." >&2
    exit 1
fi

# ---------------------------------------------------------------- udev -----
if [ "$DO_UDEV" = 1 ]; then
    changed=0
    # An older layout used different filenames; remove them so one device does
    # not end up governed by two rules.
    for stale in 99-logitech-g502.rules 99-g502-uinput.rules 70-g502.rules; do
        [ -f "/etc/udev/rules.d/$stale" ] && { sudo rm -f "/etc/udev/rules.d/$stale"; changed=1; }
    done
    for r in 99-logitech-hidpp.rules 99-logi-uinput.rules; do
        dest="/etc/udev/rules.d/$r"
        if cmp -s "$HERE/$r" "$dest" 2>/dev/null; then
            echo "udev rule current: $dest"
        else
            echo "Installing $dest (needs sudo)."
            sudo install -m 0644 "$HERE/$r" "$dest"
            changed=1
        fi
    done

    # /dev/uinput does not exist until the module is loaded, so the rule has
    # nothing to act on before that. Binding buttons to keystrokes needs it.
    if ! lsmod | grep -q '^uinput'; then
        echo "Loading the uinput module (needed to synthesise key presses)."
        sudo modprobe uinput || echo "  could not load uinput; button bindings will not work"
    fi
    if [ ! -f /etc/modules-load.d/uinput.conf ]; then
        echo uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null
        echo "uinput will load at boot."
    fi

    if [ "$changed" = 1 ]; then
        sudo udevadm control --reload-rules
        sudo udevadm trigger --subsystem-match=hidraw
        sudo udevadm trigger --subsystem-match=misc
        echo
        echo "  >>> UNPLUG AND REPLUG THE MOUSE. <<<"
        echo "  udevadm trigger re-runs the rules, but a node that is already open"
        echo "  keeps its old permissions, so the replug is what matters."
        echo
    fi

    if ! id -nG | tr ' ' '\n' | grep -qx plugdev; then
        echo "  Note: you are not in the plugdev group, which the rules rely on:"
        echo "      sudo usermod -aG plugdev \$USER     (then log out and back in)"
    fi
fi

# ----------------------------------------------------------------- bin -----
if [ "$DO_BIN" = 1 ]; then
    ICONS="$PREFIX/share/icons/hicolor/scalable/apps"
    install -d "$PREFIX/bin" "$PREFIX/share/applications" "$ICONS"
    install -m 0755 "$BIN" "$PREFIX/bin/logi"
    install -m 0644 "$HERE/logi.desktop" "$PREFIX/share/applications/logi.desktop"
    install -m 0644 "$HERE/logi.svg" "$ICONS/logi.svg"
    echo "Installed $PREFIX/bin/logi, its desktop entry and its icon."
    command -v gtk-update-icon-cache >/dev/null 2>&1 &&
        gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    command -v update-desktop-database >/dev/null 2>&1 &&
        update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) echo "  Note: $PREFIX/bin is not on your PATH. Add it to use 'logi' directly." ;;
    esac
fi

# ------------------------------------------------------------- service -----
if [ "$DO_SERVICE" = 1 ]; then
    UNIT_DIR="$HOME/.config/systemd/user"
    install -d "$UNIT_DIR"
    # The unit runs the installed binary where there is one, so upgrading the
    # install upgrades the service; otherwise it runs it from the build tree.
    TARGET="$PREFIX/bin/logi"
    [ -x "$TARGET" ] || TARGET="$BIN"
    sed "s|@BIN@|$TARGET|" "$HERE/logi.service" > "$UNIT_DIR/logi.service.new"

    if [ -f "$UNIT_DIR/logi.service" ] && \
       ! cmp -s "$UNIT_DIR/logi.service.new" "$UNIT_DIR/logi.service"; then
        # Never clobber a unit someone has edited; leave the new one beside it.
        if systemctl --user cat logi >/dev/null 2>&1 && \
           [ -n "$(find "$UNIT_DIR/logi.service" -newer "$HERE/logi.service" 2>/dev/null)" ]; then
            mv "$UNIT_DIR/logi.service.new" "$UNIT_DIR/logi.service.new"
            echo "Your logi.service looks edited; wrote $UNIT_DIR/logi.service.new instead."
        else
            mv "$UNIT_DIR/logi.service.new" "$UNIT_DIR/logi.service"
        fi
    else
        mv "$UNIT_DIR/logi.service.new" "$UNIT_DIR/logi.service"
    fi

    if ! systemctl --user show-environment >/dev/null 2>&1; then
        echo "No systemd user session here, so the service was installed but not started."
        echo "  Start it from a desktop session with: systemctl --user enable --now logi"
    else
        systemctl --user daemon-reload
        systemctl --user enable --now logi 2>&1 | sed 's/^/  /' || true
        sleep 1
        state=$(systemctl --user is-active logi 2>/dev/null || true)
        echo "Service: $state"
        if [ "$state" != "active" ]; then
            echo "  It did not stay running. Recent log:"
            journalctl --user -u logi -n 8 --no-pager 2>/dev/null | sed 's/^/    /' || true
        fi
    fi
fi

echo
echo "Done. Run 'logi' for the app, 'logi --help' for the command line,"
echo "or 'systemctl --user status logi' for the service."
