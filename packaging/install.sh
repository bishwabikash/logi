#!/usr/bin/env bash
# install.sh — install the udev rule and the g502d user service.
#
# Two separate things, either of which can be skipped:
#   * /etc/udev/rules.d/99-logitech-g502.rules   (needs sudo) so g502ctl and
#     g502d can open the mouse without root
#   * ~/.config/systemd/user/g502d.service       (no sudo) the daemon
#
# Safe to re-run: identical files are left alone, and anything you have
# edited is never overwritten — a .new copy is written beside it instead.
#
# Usage: ./install.sh [--no-udev] [--no-service] [--force] [--help]
#   --force   overwrite an edited unit file (the udev rule too)
# Env:
#   G502_ROOT   directory containing the g502/ package (default: autodetected)
#   PREFIX      where a system-wide install lives (default: /usr/local)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
PREFIX="${PREFIX:-/usr/local}"

RULE_NAME="99-logitech-g502.rules"
RULE_DEST="/etc/udev/rules.d/$RULE_NAME"
UNIT_NAME="g502d.service"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_DEST="$UNIT_DIR/$UNIT_NAME"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/g502/config.json"

DO_UDEV=1 DO_SERVICE=1 FORCE=0
for arg in "$@"; do
  case "$arg" in
    --no-udev)    DO_UDEV=0 ;;
    --no-service) DO_SERVICE=0 ;;
    --force)      FORCE=1 ;;
    -h|--help)    sed -n '2,18p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "install.sh: unknown option $arg (try --help)" >&2; exit 2 ;;
  esac
done

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# -- where the package lives ------------------------------------------------
# A system-wide copy is preferred over the checkout, so the daemon keeps
# working if this directory is moved or deleted.
if [[ -n "${G502_ROOT:-}" ]]; then
  ROOT="$G502_ROOT"
elif [[ -f "$PREFIX/lib/g502/g502/daemon.py" ]]; then
  ROOT="$PREFIX/lib/g502"
elif [[ -f "$REPO/g502/daemon.py" ]]; then
  ROOT="$REPO"
else
  die "cannot find the g502 package (looked in $PREFIX/lib/g502 and $REPO)"
fi

PYTHON="$(command -v python3 || true)"
[[ -n "$PYTHON" ]] || die "python3 not found"

say "g502 install"
say "  package  : $ROOT"
say "  python   : $PYTHON"
[[ $DO_UDEV    == 1 ]] && say "  udev rule: $RULE_DEST (needs sudo)"
[[ $DO_SERVICE == 1 ]] && say "  service  : $UNIT_DEST"

# -- udev -------------------------------------------------------------------
if [[ $DO_UDEV == 1 ]]; then
  step "udev rule"
  command -v sudo >/dev/null || die "sudo not found; copy $HERE/$RULE_NAME to $RULE_DEST as root yourself"
  if [[ -f "$RULE_DEST" ]] && cmp -s "$HERE/$RULE_NAME" "$RULE_DEST"; then
    say "already installed and identical; nothing to do"
  else
    if [[ -f "$RULE_DEST" && $FORCE == 0 ]]; then
      warn "$RULE_DEST differs from the shipped rule; replacing it (pass --no-udev to skip)"
    fi
    say "About to run, as root:"
    say "  install -m 0644 '$HERE/$RULE_NAME' '$RULE_DEST'"
    say "  udevadm control --reload-rules"
    say "  udevadm trigger --subsystem-match=hidraw"
    sudo install -m 0644 "$HERE/$RULE_NAME" "$RULE_DEST"
    sudo udevadm control --reload-rules
    sudo udevadm trigger --subsystem-match=hidraw
    say "installed $RULE_DEST"
  fi

  # The older rule shipped with this repo does the same job. Two rules that
  # both grant access are harmless, but say so rather than leave it a mystery.
  if [[ -f /etc/udev/rules.d/70-g502.rules ]]; then
    warn "/etc/udev/rules.d/70-g502.rules is also installed and grants the same access."
    warn "Keeping both is harmless; remove that one if you want a single rule."
  fi

  if getent group plugdev >/dev/null 2>&1; then
    if id -nG "$USER" | tr ' ' '\n' | grep -qx plugdev; then
      say "group plugdev: you are a member"
    else
      warn "you are not in the plugdev group. uaccess should still cover your"
      warn "local session; if not: sudo usermod -aG plugdev \"$USER\" (then log out and back in)"
    fi
  else
    warn "no plugdev group on this system; the rule falls back to uaccess only"
  fi
fi

# -- user service -----------------------------------------------------------
if [[ $DO_SERVICE == 1 ]]; then
  step "systemd user unit"
  [[ -f "$HERE/$UNIT_NAME" ]] || die "missing $HERE/$UNIT_NAME"
  mkdir -p "$UNIT_DIR"
  rendered="$(mktemp)"; trap 'rm -f "$rendered"' EXIT
  # Documentation= is a URI, so a path with spaces has to be escaped there.
  doc_uri="${REPO// /%20}"
  sed -e "s|@G502_ROOT@|$ROOT|g" \
      -e "s|@G502_DOC@|$doc_uri|g" \
      -e "s|@PYTHON@|$PYTHON|g" \
      "$HERE/$UNIT_NAME" > "$rendered"
  if grep -q '@G502_ROOT@\|@PYTHON@' "$rendered"; then
    die "token substitution failed; not installing a broken unit"
  fi

  if [[ -f "$UNIT_DEST" ]] && cmp -s "$rendered" "$UNIT_DEST"; then
    say "already installed and identical; nothing to do"
  elif [[ -f "$UNIT_DEST" && $FORCE == 0 ]]; then
    install -m 0644 "$rendered" "$UNIT_DEST.new"
    warn "$UNIT_DEST exists and differs — your copy was left alone."
    warn "The new version is at $UNIT_DEST.new; diff it, or re-run with --force."
  else
    install -m 0644 "$rendered" "$UNIT_DEST"
    say "installed $UNIT_DEST"
  fi
  systemctl --user daemon-reload || warn "systemctl --user daemon-reload failed (no user session?)"
fi

# -- config -----------------------------------------------------------------
step "config"
if [[ -f "$CONFIG" ]]; then
  say "$CONFIG exists; left untouched"
else
  if PYTHONPATH="$ROOT" "$PYTHON" -c 'from g502 import config; print(config.save(config.DEFAULT_CONFIG))' 2>/dev/null; then
    say "wrote a starter config (example rules only; edit it before enabling)"
  else
    warn "could not write $CONFIG; create it with: g502ctl config --init"
  fi
fi
# Parses the config without touching the device.
PYTHONPATH="$ROOT" "$PYTHON" -m g502.daemon --check || warn "config did not validate; fix it before starting the daemon"

# -- what next --------------------------------------------------------------
cat <<NEXT

== next steps

1. Unplug and replug the mouse. The udev rule is only applied on an add
   event, so an already-connected device keeps its old permissions.

2. Check it works without sudo:
     g502ctl info
   If that says permission denied, see the comments at the top of
   $HERE/$RULE_NAME (your G502 may put HID++ on a different interface).

3. Edit your rules — the installed config only contains examples:
     \$EDITOR $CONFIG

4. Try the daemon in the foreground first:
     PYTHONPATH='$ROOT' $PYTHON -m g502.daemon --log-level debug

5. Then enable it for your session:
     systemctl --user enable --now $UNIT_NAME
     journalctl --user -u $UNIT_NAME -f

   After editing the config:  systemctl --user reload $UNIT_NAME
   To remove everything:      $HERE/uninstall.sh
NEXT
