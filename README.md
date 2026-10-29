# logi

A driver and management app for Logitech mice on Linux, written against the
HID++ 2.0 protocol. One native binary: a window, a tray icon, a background
service and a command line. No Python and no runtime dependencies beyond the
system's own graphics libraries.

Run `logi` for the window, or `logi --help` for the command line.

> **Not affiliated with or endorsed by Logitech.** Logitech, G502 and G HUB are
> trademarks of their owners. This project talks to the mouse directly and uses
> none of Logitech's software.

> **Version 0.1, tested on one mouse (a G502 HERO).** Read
> [What has and has not been tested](#what-has-and-has-not-been-tested) before
> pointing it at anything else, or before using button bindings.

## What it does

* **DPI.** The G502 HERO's sensor is continuous, 100 to 25600 in steps of 50,
  not a handful of fixed stages. The window follows changes made outside it,
  including the DPI and profile buttons on the mouse, and shows which stage is
  active.
* **Report rate**, written to the active onboard profile.
* **Lighting.** Off, fixed colour and breathing, set live or stored into a
  profile so it survives without any software. A few host-driven effects
  (rainbow, wave, pulse) while the app runs.
* **Onboard profiles.** Read, edit and back up all five: name, DPI stages,
  report rate, lighting, and the eleven buttons. Edits stage in memory and are
  written by one explicit Save, then read back and verified.
* **Button bindings.** Bind a physical button to a key or chord, typed text, a
  command, or another mouse button, either alongside its normal job or in place
  of it.
* **Per-application profiles.** Switch profile while a given program is running.
* **A tray icon** showing the service state, with a menu.
* **No model whitelist.** Any Logitech device that answers HID++ is listed by
  `logi devices`, and gets exactly the controls its own feature table reports.

## How this relates to libratbag, Piper and Solaar

[libratbag](https://github.com/libratbag/libratbag) with
[Piper](https://github.com/libratbag/piper) already covers basic DPI, lighting
and button configuration for many mice, and [Solaar](https://github.com/pwr-Solaar/Solaar)
covers Logitech receivers and a wide range of devices. If that is all you need,
they are the mature choice.

This project goes further on one device, because the hardware exposes more than
their models do: continuous DPI, parameterised breathing, whole-device backup
and restore, host-driven effects, and live following of the mouse.

The LED, profile and button byte layouts and the sector CRC were confirmed
against libratbag's implementation of the same hardware, and the two
announcements the mouse sends were taken from Solaar's notification handling.
No code was copied from either; the protocol facts are written up in
[PROTOCOL.md](PROTOCOL.md).

## What has and has not been tested

Everything below was run against **one G502 HERO** (046d:c08b, firmware U1
v27.03), and every write was reverted and the device checked against a backup
taken first.

| Verified on the mouse | |
|---|---|
| Discovery, capability probing, naming from the device | yes |
| DPI across the full range, with snapping | yes |
| Report rate, live and written to a profile | yes |
| Lighting: off, fixed, breathing, both zones, live | yes |
| Host-driven effects | yes |
| Profile decode, backup, restore, flash writes with readback | yes |
| Button rebinding in flash | yes |
| Per-application profile switching | yes |
| The window following DPI and profile changes made by another process | yes |
| The tray icon registering with GNOME | yes |

| **Implemented and unit-tested, never run against hardware** | |
|---|---|
| Host button bindings: the spy, silencing a button, the virtual input device | **no** |
| The service acting on bindings | **no** |
| Saving a profile to flash from the editor panel | **no** (the same write path was verified from the CLI) |
| The tray menu's actions | **no** |
| A press of the mouse's own DPI or profile button showing up in the window | **not watched directly.** Expected, because the window re-reads the device about every second and a half; whether the mouse also announces the press is unknown |
| Any mouse other than the G502 HERO | **no** |

The suite has about 420 unit tests, none of which need hardware. They check the
logic, not the device.

**Silencing a button is the riskiest untested part.** It rewrites a table on the
mouse that survives unplugging, and it rests on how that table's values are
read. If a button stops responding, `logi bind reset` restores the factory
mapping.

## Install

**From a release.** Each tagged release on the
[Releases page](https://github.com/bishwabikash/logi/releases) carries an x86-64
Linux tarball and its checksum, built on Ubuntu 22.04 so it runs on older
distributions too. Check it, unpack it, and run the installer from inside:

```
sha256sum -c logi-*-x86_64-linux.tar.gz.sha256
tar xzf logi-*-x86_64-linux.tar.gz
cd logi-*-x86_64-linux
packaging/install.sh
```

**From source.** You need a Rust toolchain, 1.89 or newer. The build needs no `-dev` packages:
the windowing library loads X11, Wayland and xkbcommon at run time.

```
cargo build --release
packaging/install.sh
```

Then **unplug and replug the mouse.** The rules take effect when the device is
added, and a node that is already open keeps its old permissions.

`install.sh` does three independent things, each skippable with `--no-udev`,
`--no-bin` or `--no-service`:

* installs two udev rules to `/etc/udev/rules.d` so the mouse and `/dev/uinput`
  work without root, loads the `uinput` module and arranges for it at boot
  (this is the only part needing sudo, and the only one that cannot be done
  from inside the app);
* copies the binary and a desktop entry under `~/.local`;
* installs a systemd user service, enables it and starts it.

You can skip it entirely and run `./target/release/logi`; the rules are only
what removes the need for `sudo`. The rules grant access through your session
(`uaccess`) and the `plugdev` group, and match on vendor and product only. Why
not narrower is explained in the rule file.

To remove it: `systemctl --user disable --now logi`, then delete
`~/.config/systemd/user/logi.service`, `~/.local/bin/logi`,
`~/.local/share/applications/logi.desktop`, `~/.config/logi/`, the two
`/etc/udev/rules.d/99-logi*.rules` files, and `/etc/modules-load.d/uinput.conf`.

## Using it

The window has tabs for Sensor, Lighting, Profiles, Buttons, Apps, Service and
About. The sidebar has the **memory mode** switch. In onboard mode the mouse
drives itself from its stored profile; in host mode the app drives the
lighting, and the stored profile is not applied. Host mode does not affect
buttons or movement.

Some of the command line:

```
logi devices                 every Logitech HID++ device and what it can do
logi info                    capabilities and current state
logi state --follow          print a line each time the mouse's state changes
logi dpi 1600                set DPI, snapped to the sensor's step
logi rate 1000               written to the active profile; --live for host mode
logi profile 2               switch the active onboard profile
logi profile show 0          decode a stored profile
logi led purple              live lighting; also: breathing <colour>, off
logi effect rainbow          stream an animation until Ctrl-C
logi backup mouse.json       dump all sectors; `logi restore` writes them back
logi bind                    show button bindings; `bind run`, `bind reset`
logi watch                   per-application profiles (see below)
```

With more than one device attached, choose with `LOGI_DEVICE=046d:c08b`.

The service and `logi watch` read `~/.config/logi/rules.conf`, one
`pattern = profile[,dpi]` per line, first match wins:

```
csgo     = 1, 800
blender  = 2
fallback = 0
```

Matching is on whether a program is *running*, not whether it is focused,
because Wayland does not let an ordinary process see the active window.
Button bindings live in `~/.config/logi/bindings.conf`.

## Safety

* **Onboard profiles are flash with finite endurance.** The app never writes on
  a keystroke or a drag. Take a backup first (`logi backup`), and do not script
  profile writes in a loop.
* **The button remapping table survives unplugging.** Writing zeros to it leaves
  a mouse that moves but cannot click, and looks exactly like broken hardware.
  The code refuses that write; `logi bind reset` is the way back.
* **Do not probe unknown HID++ functions with empty parameters.** Some are
  setters. Doing exactly that is how both of the above were first found.
* **Surface tuning, lift-off distance and firmware update are deliberately not
  implemented.** See below.

## Limits of the hardware

These are properties of the G502 HERO's firmware, not things left to do.

* It implements four lighting modes: off, fixed, breathing and cycle. Wave,
  starlight, ripple and frame streaming are defined by HID++ but absent. So
  richer animation is streamed from the host and **stops when the app stops**.
* Only fixed and breathing can be set live. `setZoneEffect` addresses an effect
  by the zone's *effect index*, not its mode id, and the firmware rejects off
  and cycle at that call. Both can be stored in a profile, which takes mode ids.
* Live lighting and live report rate need host mode; onboard mode ignores them.
* `getZoneEffect` is not implemented, and answers with zeros rather than an
  error. A zero readback says nothing about whether a write landed.
* Surface tuning, lift-off distance and angle snapping are not standalone
  features here, and the likely route to them rewrites sensor registers in a way
  that cannot be undone from the host. Firmware update is out for the same
  reason.

## Layout

| Path | Role |
|---|---|
| `src/hidpp.rs` | HID++ 2.0 transport: reports, software ids, hangup detection |
| `src/discover.rs`, `capabilities.rs`, `deviceinfo.rs`, `features.rs` | Finding devices and reading what each can do |
| `src/dpi.rs`, `rate.rs`, `led.rs`, `profiles.rs` | One feature each |
| `src/profile_blob.rs`, `profile_io.rs` | The 256-byte profile sector and verified flash writes |
| `src/buttonspy.rs`, `uinput.rs`, `actions.rs` | Button bindings |
| `src/appwatch.rs`, `effects.rs` | Per-application rules and host-driven lighting |
| `src/live.rs` | Following changes the mouse announces |
| `src/device.rs` | The worker thread that owns the device |
| `src/service.rs`, `tray.rs` | The systemd user unit and the tray icon |
| `src/gui/` | The window |
| `packaging/` | udev rules, desktop entry, service unit, installer |
| `python-reference/` | The earlier Python implementation the Rust was ported from |

[PROTOCOL.md](PROTOCOL.md) documents the protocol, marking each fact as probed
from the device or taken from libratbag or Solaar. Start there rather than in
the code if you want to port this. [PROGRESS.md](PROGRESS.md) records what is
verified and what is not.

## Contributing

CI builds and tests every push and pull request. Releases are described in
[RELEASING.md](RELEASING.md). `cargo test` runs the suite and needs no hardware. If you have a Logitech mouse
other than the G502 HERO, the most useful thing is the output of `logi devices`
and `logi info` and a note of what worked. Be careful with anything that
writes: take `logi backup` first.

## License

MIT. See [LICENSE](LICENSE).
