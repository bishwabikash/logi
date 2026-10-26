# g502 — a Linux driver and management app for the Logitech G502 HERO

Userspace HID++ 2.0 driver plus a CLI (`g502ctl`) and a desktop front end for
the Logitech G502 HERO (`046d:c08b`). It was written from scratch: every byte
layout here was either read off a real device or cross-checked against
libratbag, and the findings are written up in [PROTOCOL.md](PROTOCOL.md).

It talks to `/dev/hidraw*` directly. No kernel module, no daemon required for
anything that gets stored in the mouse.

### How this relates to the alternatives

[libratbag](https://github.com/libratbag/libratbag) and its GUI,
[Piper](https://github.com/libratbag/piper), already drive this mouse, and for
DPI stages, report rate, button remapping and the four firmware lighting
effects they are the better-tested choice — they are packaged, maintained, and
cover dozens of devices. If that is all you need, use them.

This project exists because a few things sit outside libratbag's device model:

* **Continuous DPI.** The sensor accepts any value from 100 to 25600 in steps
  of 50; libratbag exposes five discrete stages.
* **Full breathing parameterisation.** Period, waveform and intensity, not
  just a colour.
* **Host-side effects.** Rainbow, gradient, travelling wave and an
  audio-reactive meter, generated here and streamed to the device.
* **Per-application profile switching**, which libratbag does not do at all
  (it is a configuration library, not a running agent).
* **Profile sector import/export**, on top of a codec that round-trips all
  five stored profiles byte-identically.

It is a single-device hobby project, not a general-purpose daemon, and it is
newer and far less tested than libratbag. Nothing here is endorsed by or
affiliated with Logitech.

## Status

As of this commit, per feature:

| Feature | Status |
|---|---|
| Endpoint discovery / HID++ transport | working |
| DPI, continuous 100–25600 step 50 | working |
| Report rate 125 / 250 / 500 / 1000 Hz | working |
| Switching between the 5 onboard profiles | working |
| Decoding a stored profile sector (`show`) | working |
| Lighting stored to flash: off / fixed / breathing / cycle (`persist`) | working |
| Live lighting in host mode (`led`) | working |
| Host-side animation (`effect`) | working |
| Audio-reactive effect | working, needs `parec` |
| Button remapping: mouse button, HID key, special, disabled | working |
| Per-application profile switching (`watch`) | working |
| GUI | ships as `g502-gui` — see [GUI](#gui) |
| Onboard macro recording / playback | not implemented (macro bindings are decoded, not authored) |
| Profile export/import to a file | not implemented in `g502ctl`; the codec supports it |
| G502 LIGHTSPEED, Proteus Spectrum, Proteus Core | untested |
| `getZoneEffect` lighting readback | unsupported by this firmware |
| Surface tuning, lift-off distance, angle snapping | out of scope — see [Limitations](#limitations) |
| Firmware update (DFU) | out of scope |

## Hardware support

| USB id | Device | Status |
|---|---|---|
| `046d:c08b` | G502 HERO / SE HERO | **verified** — firmware `U1 v27.03`, HID++ 4.2 |
| `046d:c08d` | G502 LIGHTSPEED (wired mode) | untested |
| `046d:c332` | G502 Proteus Spectrum | untested |
| `046d:c07d` | G502 Proteus Core | untested |

Only `c08b` has been exercised against hardware. The other three are listed in
`g502/discover.py` because they are the same family and speak the same
features, but nobody has run this code against one: expect a different zone
count, a different effect set, or a different profile format, and treat any
write as unverified. The shipped udev rule covers `c08b` and `c08d` only — add
a line for the others if you try them.

What the verified device reports: 2 RGB zones (zone 0 primary/logo, zone 1 the
DPI indicator), 11 buttons, 5 onboard profiles plus 5 read-only ROM profiles,
16 sectors of 256 bytes, and exactly four firmware lighting effects.

## Install

Requirements:

* Python 3.13, standard library only. No pip packages.
* `udev` and `systemd` for the optional convenience bits below.
* Optional: `pulseaudio-utils` (for `parec`/`pactl`) if you want the
  audio-reactive effect. Without it, that one effect refuses to start and
  everything else is unaffected.

```
packaging/install.sh                 # udev rule (needs sudo) + g502d user service
packaging/install.sh --no-service    # just the udev rule
packaging/install.sh --no-udev       # just the user service
packaging/install.sh --force         # overwrite a unit file you have edited
```

It is safe to re-run: identical files are left alone, and anything you have
edited is never overwritten — a `.new` copy is written beside it instead.
`G502_ROOT` overrides where the `g502/` package lives (autodetected by
default); `PREFIX` sets the system-wide install prefix (default `/usr/local`).

You do not have to install anything to use the project — `./g502ctl` runs from
the checkout. The udev rule is what removes the need for `sudo`.

**udev.** `packaging/99-logitech-g502.rules` tags the device's hidraw nodes
with `uaccess`, handing the node to the active local session via
systemd-logind, and also sets group `plugdev` for the cases uaccess does not
cover (ssh sessions, system services). After installing the rule you must
**unplug and replug the mouse** — `udevadm trigger` re-runs the rules, but a
node that is already open keeps its old permissions.

The rule matches on vendor and product only, which grants both of the G502's
hidraw nodes rather than just the HID++ one. That is deliberate and explained
in the rule file: udev requires every `ATTRS{}` key in a rule to resolve on the
same parent device, and `bInterfaceNumber` lives on the usb_interface while
`idVendor`/`idProduct` live on the usb_device, so an interface-narrowed rule
silently matches nothing.

**systemd user unit.** `packaging/g502-watch.service` runs
`g502ctl watch --quiet` in your session:

```
systemctl --user enable --now g502-watch
```

It is a user unit, not a system one — it reads `/proc` and one hidraw node,
and runs with `NoNewPrivileges`, `ProtectSystem=strict` and
`ProtectHome=read-only`.

You can also just run everything out of a checkout; `./g502ctl` works in place.

## Usage

```
g502ctl info                      # capabilities, mode, zones, current state
g502ctl show 0                    # decode stored profile 0 (sector 1)

g502ctl dpi                       # read
g502ctl dpi 1600                  # any value 100-25600; off-step values snap
g502ctl rate 1000                 # 125 / 250 / 500 / 1000 Hz
g502ctl profile 2                 # switch active onboard profile
```

### Lighting: stored vs live

Two routes, and the difference is the single most important thing to
understand about this device:

```
g502ctl persist solid ff0066      # written to profile flash: survives reboot
g502ctl led solid ff0066 --hold   # live, only while this command runs
```

`persist` writes one of the four firmware effects into the profile sector. The
mouse keeps it with no host software at all, including when you boot Windows.

`led` drives the device live over `0x8070`, which only has any effect while the
mouse is in **host mode**. The command holds host mode for as long as it runs
and restores the previous mode on the way out, so without `--hold` you will see
the colour applied and then immediately revert to the stored profile.

Both accept the four firmware effects:

```
g502ctl persist off
g502ctl persist breathing 00ffff --period 3000 --waveform sharkfin
g502ctl persist cycle --period 8000 --intensity 60
g502ctl persist solid red --index 3      # into profile 3 rather than the active one
```

Colours are `rrggbb`, `#rrggbb`, or a name (`red green blue cyan magenta
yellow orange purple pink white off`). Firmware breathing waveforms:
`default sine square triangle sawtooth sharkfin exponential`.

### Animated effects

Anything beyond those four is computed on the host and streamed as solid
colours, one HID++ write per zone per frame:

```
g502ctl effect rainbow --period 5000
g502ctl effect breathe ff0066 --waveform exponential
g502ctl effect gradient 00ffff --color2 ff00ff
g502ctl effect wave 00ff88 --fps 90
g502ctl effect audio 00ff88 --duration 30
```

Effects: `solid rainbow breathe gradient wave audio`. Host-side `breathe`
curves are `sine triangle sawtooth square exponential` — a different set from
the firmware's waveform list above, because these are generated here.

`--fps` defaults to 60 and is clamped to 1–120; `--duration` is in seconds and
defaults to running until Ctrl-C. The device is put back into its previous mode
when the effect stops, so the stored profile's lighting comes back.

### Button remapping

Written into profile flash, so it persists:

```
g502ctl button 6 dpi-cycle
g502ctl button 10 key:0x1d         # HID keyboard usage code
g502ctl button 4 button:5          # mouse button number
g502ctl button 9 none              # bound to nothing
g502ctl button 6 dpi-up --profile 2
```

Special actions: `wheel-left wheel-right dpi-up dpi-down dpi-cycle dpi-default
dpi-shift profile-up profile-down profile-cycle second-mode`.

Writes refuse to proceed if the profile's existing CRC is already bad, and the
sector is read back and compared after every write.

### Per-application profiles

```
g502ctl config --init              # writes ~/.config/g502/config.json
g502ctl config                     # print the effective config
g502ctl watch                      # foreground, logs every switch
g502ctl watch --interval 1 --quiet
```

Rules are matched against the names of running processes, first match wins:

```json
{
  "default_profile": 0,
  "restore_on_exit": true,
  "rules": [
    { "match": ["cs2", "csgo_linux64"], "profile": 1 },
    { "match": ["blender"], "profile": 2, "dpi": 1200 }
  ]
}
```

Matching on process presence — not on the focused window — is deliberate; see
[Limitations](#limitations). It behaves identically under Wayland, X11 and a
bare TTY.

### GUI

`g502-gui` is a desktop front end over the same `g502/` modules: device
information, DPI and report rate, both lighting zones, profile inspection and
the per-app rules. It needs its toolkit's Python bindings installed. Every
operation it offers is also available from `g502ctl`, which is the reference
interface — if the GUI and the CLI disagree, the CLI is right.

## Architecture

```
g502ctl          CLI (argparse; one cmd_* per subcommand)
g502-gui         desktop front end
g502/            the driver
tools/probe.py   read-only dump of everything the device reports about itself
packaging/       udev rule + systemd user unit
PROTOCOL.md      the protocol reference this is built on
tests/           offline unit tests (see tests/README.md)
```

| Module | What it does |
|---|---|
| `g502/hidpp.py` | Transport. Short (`0x10`) and long (`0x11`) vendor reports over hidraw, software-id tagging so replies are not confused with event notifications, `call()` / `send_nowait()` / `flush()`, feature-index resolution via the root feature. |
| `g502/discover.py` | Finds the device in sysfs and probes its hidraw nodes for the one that actually answers HID++ (a G502 exposes two interfaces; only one carries the `ff00` vendor collections). |
| `g502/features.py` | Feature-id constants and names, including the `hidden,engineering` ones that exist but are deliberately not used. |
| `g502/dpi.py` | `0x2201 ADJUSTABLE_DPI`. Decodes the descriptor's range form (a word with `0xE000` set is a step, not a DPI) and snaps values to the step. |
| `g502/rate.py` | `0x8060 REPORT_RATE`. Bitmask of supported intervals in, Hz out. |
| `g502/led.py` | `0x8070 COLOR_LED_EFFECTS`. Zone enumeration, per-mode parameter packing, colour parsing, and a `supports()` check so an unimplemented mode fails with a readable error instead of a protocol one. |
| `g502/profiles.py` | `0x8100 ONBOARD_PROFILES`. Mode read/write with a `host_mode()` context manager that always restores, profile selection, and sector read/write with a pre-write CRC check and post-write verify. |
| `g502/profile_blob.py` | Codec for the 256-byte profile sector: report rate, DPI stages, 32 button slots, UTF-16 name, 4 LED records, CRC-16/CCITT-FALSE. Unmodelled bytes are copied through on serialize, so a round trip never drops firmware state. |
| `g502/effects.py` | Host-side animation. Holds host mode, runs a frame generator, pushes `FIXED` colours with `send_nowait` and drains replies periodically. |
| `g502/config.py` | `~/.config/g502/config.json` load/save (atomic write). |
| `g502/appwatch.py` | Reads `/proc/*/comm`, matches rules, applies a profile (and optionally a DPI), restores the entry profile on exit. |

## Limitations

These are properties of the device and the protocol, not a to-do list.

**The firmware implements four lighting effects, and that is all.** `off`
(0x00), `fixed` (0x01), `cycle` (0x03) and `breathing` (0x0a). `WAVE`,
`STARLIGHT`, `RIPPLE` and `CUSTOM` are defined in HID++ but absent here — the
device enumerates four effects per zone and rejects the rest. In particular
there is **no hardware frame-streaming mode** (`CUSTOM` 0x0C is what that would
be), so everything under `g502ctl effect` is the host pushing solid colours in
a loop. Those effects stop the moment the process stops.

**Live lighting requires host mode.** In onboard mode the LEDs follow the
stored profile and `0x8070` writes are inert. So: anything written with
`persist` (lighting, buttons, DPI stages, report rate) lives in the mouse and
needs no host; anything driven live (`led`, `effect`) lasts exactly as long as
the process holding host mode. Leaving the mouse in host mode would make its
lighting depend on your login session, which is why every code path restores
the previous mode on exit, including on Ctrl-C.

**Frame rate is bounded by HID++ round trips.** A blocking write per frame
measures about **19 fps**. Fire-and-forget writes with periodic draining go
higher — the engine's default target is 60 fps and it accepts up to 120 — but
the ceiling is the USB interrupt endpoint, not the code, and dropped frames are
normal at the top of that range. Two zones is also the hardware limit; there is
nothing per-LED to animate.

**There is no lighting readback.** This device reports `ext_caps 0x0000`, so
`HAS_ZONE_EFFECT` is clear and `getZoneEffect` is not implemented. It answers
the request with zeros rather than an error, which means a zero readback tells
you nothing about whether your write landed. Verify lighting by eye.

**Surface tuning, lift-off distance and angle snapping are not implemented,
deliberately.** They are not standalone HID++ features on this device — the
feature table has no entry for them. G HUB reaches them through the
`engineering`-flagged features `TDE_ACCESS` (0x1eb0) and `SPI_DIRECT_ACCESS`
(0x1e22), writing sensor registers directly. A wrong write there is not
recoverable from userspace, i.e. it can brick the mouse. No open-source
implementation exists to copy, and guessing is not an acceptable risk. Out of
scope.

**Firmware update is out of scope.** `DFU_CONTROL` (0x00c2) is present on the
device and untouched here.

**Per-app switching keys off running processes, not the focused window.**
Wayland has no portable API for "which window has focus" — it is restricted by
design, and the GNOME shell extension route would mean shipping an extension.
So the watcher matches process presence, the same thing the vendor software's
"on launch" detection does. Practical consequence: with two matching
applications open at once, the first matching rule wins regardless of which one
you are actually using.

**Onboard macros are not authored.** `show` decodes a macro binding as a page
and offset, but nothing here records or writes macro sectors.

**One device at a time.** `open_device()` returns the first endpoint that
answers; there is no selection flag for two G502s on one machine.

## Troubleshooting

* *`no supported Logitech device found`* — the mouse is not plugged in, or its
  USB id is not in `g502/discover.py`.
* *`permission denied`* — the udev rule is not installed or the mouse has not
  been replugged since.
* *`needs root for raw HID access`* — this is `g502ctl`'s own euid check
  (`need_root()`), not a device permission. The udev rule covers the device
  node; this check is the remaining reason to use `sudo`.
* *`found the device but no HID++ endpoint answered`* — something else holds
  the device, or the probe timed out. Stop other configuration software
  (libratbag's `ratbagd`, a running Piper) and retry.
* *Lighting commands print success but nothing changes* — you are probably
  expecting `led` to persist. It does not; use `persist`.
* *`parec not found`* — install `pulseaudio-utils`; only the `audio` effect
  needs it.

## Tests

All tests are offline and never touch hardware:

```
python3 -m unittest discover tests
```

See [tests/README.md](tests/README.md) for what that does and does not cover.

## Credits

[**libratbag**](https://github.com/libratbag/libratbag) — the LED mode and
waveform enums, the `setZoneEffect` parameter layouts, the onboard-profile
function numbering, the 256-byte profile sector layout and the CRC-16/CCITT
choice were all confirmed against its implementation of this device. Where this
project's observations and libratbag's code agree, PROTOCOL.md says so; where
something is only from hardware probing, it is marked `[probed]`. This work
would have taken far longer without it.

The protocol reference is in [PROTOCOL.md](PROTOCOL.md), including which
findings came from the device and which from libratbag. It is not duplicated
here.
