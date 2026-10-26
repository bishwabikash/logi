# g502 — Linux driver and management tool for the Logitech G502 HERO

Talks HID++ 2.0 straight to the mouse, so it covers the things the vendor
software does on Windows: DPI, report rate, both RGB zones, onboard profiles,
button remapping, animated lighting and per-application profile switching.
Settings written to profile flash persist across reboots and into Windows.

Protocol details and how they were established are in [PROTOCOL.md](PROTOCOL.md).

## Install

    ./install.sh all          # udev rule + g502ctl
    ./install.sh service      # optional: per-app profile switching

The udev rule grants your logged-in session access to the device, so `g502ctl`
does not need `sudo` afterwards. Replug the mouse once after installing it.

## Use

    g502ctl info                      # capabilities and current state
    g502ctl show 0                    # decode stored profile 0

    g502ctl dpi 1600                  # any value 100-25600 in steps of 50
    g502ctl rate 1000                 # 125 / 250 / 500 / 1000 Hz
    g502ctl profile 2                 # switch onboard profile

### Lighting

Two routes, and the difference matters:

    g502ctl persist solid ff0066      # writes to flash: survives reboot, no daemon
    g502ctl led solid ff0066 --hold   # live, only while the command runs

`persist` stores one of the four effects the firmware implements. `led` and
`effect` drive the device live, which needs host mode — when the command exits
the mouse reverts to its stored profile.

    g502ctl persist breathing 00ffff --period 3000 --waveform sharkfin
    g502ctl persist cycle --period 8000

Waveforms: `default sine square triangle sawtooth sharkfin exponential`.

### Animated effects

Anything beyond the firmware's four effects is generated here and streamed as
solid colours at about 45 fps:

    g502ctl effect rainbow --period 5000
    g502ctl effect breathe ff0066 --waveform exponential
    g502ctl effect gradient 00ffff --color2 ff00ff
    g502ctl effect wave 00ff88
    g502ctl effect audio 00ff88        # needs: sudo apt install pulseaudio-utils

### Button remapping

Written into profile flash, so it persists:

    g502ctl button 6 dpi-cycle
    g502ctl button 10 key:0x1d         # HID usage code
    g502ctl button 4 button:5          # mouse button
    g502ctl button 9 none

Actions: `wheel-left wheel-right dpi-up dpi-down dpi-cycle dpi-default
dpi-shift profile-up profile-down profile-cycle second-mode`.

### Per-application profiles

    g502ctl config --init              # ~/.config/g502/config.json
    g502ctl watch                      # foreground, logs matches

Rules match on process name, first match wins:

```json
{
  "default_profile": 0,
  "rules": [
    { "match": ["cs2", "csgo_linux64"], "profile": 1 },
    { "match": ["blender"], "profile": 2, "dpi": 1200 }
  ]
}
```

Matching on a running process is what the vendor software does for "on launch"
detection, and it behaves the same under X11, Wayland and a TTY.

## What this cannot do

* **Surface tuning, lift-off distance, angle snapping.** Not exposed as HID++
  features on this device; the vendor software reaches them through
  engineering-flagged features that write sensor registers directly. A wrong
  write there is not recoverable from userspace.
* **Per-LED animation richer than two zones.** The hardware has exactly two
  colour zones.
* **Frame rates much above ~45 fps.** The firmware does not implement the
  CUSTOM (0x0C) direct-frame effect, so every frame is a separate HID++
  round trip.
* **Firmware updates.** `DFU_CONTROL` is present but deliberately untouched.

## Relationship to libratbag

libratbag/Piper already drive this mouse well. This project exists to reach
what its model does not expose: arbitrary DPI rather than five fixed stages,
breathing waveform and period, host-driven animation, and process-based
profile switching. The protocol work leans on libratbag's implementation,
which is credited throughout PROTOCOL.md.
