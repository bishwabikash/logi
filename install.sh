#!/usr/bin/env bash
# Install g502ctl, the udev rule, and optionally the watch service.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-/usr/local}"

usage() { sed -n '2,6p' "$0" | sed 's/^# \?//'; exit 1; }

case "${1:-all}" in
  udev)
    sudo install -m 0644 "$HERE/packaging/70-g502.rules" /etc/udev/rules.d/70-g502.rules
    sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=hidraw
    echo "udev rule installed. Replug the mouse (or re-login) to pick it up."
    ;;
  bin)
    sudo install -d "$PREFIX/lib/g502"
    sudo cp -r "$HERE/g502" "$PREFIX/lib/g502/"
    sudo install -m 0755 "$HERE/g502ctl" "$PREFIX/bin/g502ctl"
    # point the installed script at the installed package
    sudo sed -i "s|sys.path.insert(0, str(Path(__file__).resolve().parent))|sys.path.insert(0, \"$PREFIX/lib/g502\")|" "$PREFIX/bin/g502ctl"
    echo "installed $PREFIX/bin/g502ctl"
    ;;
  service)
    mkdir -p "$HOME/.config/systemd/user"
    install -m 0644 "$HERE/packaging/g502-watch.service" \
      "$HOME/.config/systemd/user/g502-watch.service"
    systemctl --user daemon-reload
    echo "installed. Enable with: systemctl --user enable --now g502-watch"
    ;;
  all) "$0" udev; "$0" bin; echo "run './install.sh service' to add the watcher" ;;
  *) usage ;;
esac
