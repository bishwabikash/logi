# g502

A driver and management app for the Logitech G502 HERO on Linux, written from
scratch against the device's HID++ 2.0 protocol. One native binary with a
graphical app and a command line in it; no daemon, no Python, no runtime
dependencies beyond the system's own graphics libraries.

Run it with no arguments for the app, or with a subcommand for the shell.

## How this relates to libratbag and Piper

libratbag and Piper already cover basic DPI, lighting and button configuration
for this mouse, and if that is all you need they are the mature choice. This
project exists because the hardware exposes more than libratbag's model does:

* **DPI is continuous**, 100–25600 in steps of 50. libratbag presents five
  discrete stages. The device's own descriptor says otherwise, and this app
  exposes the full range.
* **Breathing is fully parameterised** — period plus seven waveforms (sine,
  square, triangle, sawtooth, sharkfin, exponential, default) — rather than an
  on/off toggle.
* **Host-streamed effects** that the firmware cannot do on its own.
* **Whole-device backup and restore** of all 16 flash sectors, including the
  profile directory.

The LED, profile and button byte layouts and the sector CRC were confirmed
against libratbag's implementation of the same hardware; it is a far better
protocol reference than anything Logitech publishes. See
<https://github.com/libratbag/libratbag>.

## Hardware

Verified on **046d:c08b** (G502 HERO / SE HERO), firmware `U1 v27.03`,
HID++ 4.2.

Also matched but **untested**, because there was no hardware to try: `c08d`
(LIGHTSPEED, wired), `c332` (Proteus Spectrum), `c07d` (Proteus Core).

## Build and install

Needs a Rust toolchain. No `-dev` packages: winit dlopens X11, Wayland and
xkbcommon at runtime, so the build works on a machine with only the runtime
libraries present.

```
cargo build --release
packaging/install.sh
```

Then **unplug and replug the mouse**. `udevadm trigger` re-runs the rules, but
a node that is already open keeps its old permissions, so the replug is what
actually grants access.

`install.sh` does two independent things, either skippable with `--no-udev` or
`--no-bin`: it installs the udev rule to `/etc/udev/rules.d` (this is the only
step needing sudo), and copies the binary plus a desktop entry under `PREFIX`,
which defaults to `~/.local`. You do not have to install anything to use the
project — `./target/release/g502` runs from the build directory.

## Command line

```
g502 info                   capabilities and current state
g502 dpi 1600               set DPI; snapped to the nearest step and reported
g502 rate 1000              125/250/500/1000 Hz, written to the active profile
g502 rate 1000 --live       set it live instead (host mode, not persistent)
g502 mode onboard           hand the mouse back after live lighting
g502 button                 list bindings
g502 button 10 dpi-cycle    rebind a button (writes flash)
g502 watch                  per-application profile switching
g502 profile 2              switch the active onboard profile
g502 profile show 0         decode a stored profile, buttons and all
g502 led cyan               solid colour, both zones
g502 led breathing ff0066   breathing
g502 led off
g502 effect rainbow         stream an animation until Ctrl-C
g502 backup mouse.json      dump all 16 sectors
g502 restore mouse.json
```

## What each module does

| File | Role |
|---|---|
| `src/hidpp.rs` | HID++ 2.0 transport: short/long reports, software-id matching, typed errors |
| `src/discover.rs` | Finds the HID++ endpoint by probing — a G502 has two, only one answers |
| `src/features.rs` | Feature-id table and type flags |
| `src/dpi.rs` | 0x2201, including the range-vs-list descriptor decode |
| `src/rate.rs` | 0x8060 |
| `src/led.rs` | 0x8070, both zones, per-mode parameter layouts |
| `src/profiles.rs` | 0x8100: mode, profile switching, sector reads |
| `src/profile_blob.rs` | The 256-byte profile sector codec and its CRC |
| `src/profile_io.rs` | Flash writes with readback verification, backup and restore |
| `src/effects.rs` | Host-side animation |
| `src/appwatch.rs` | Per-application profile rules, debounced, /proc based |
| `src/device.rs` | Worker thread owning the device, so the UI never blocks on I/O |
| `src/gui/` | egui front end |

`PROTOCOL.md` documents the protocol itself, marking every fact as either
probed from the device or taken from libratbag. Start there rather than reading
the code if you want to port this elsewhere.

Everything above the protocol layer has been exercised against a real G502
HERO: reads, DPI and rate changes, profile switching, live lighting, host-side
effects, button rebinding and profile flash writes, each reverted afterwards
and confirmed byte-identical to a backup taken first.

`python-reference/` holds the original Python implementation. It is the
validated reference the Rust was ported from, not something you need to run.

### Per-application rules

`g502 watch` reads `~/.config/g502/rules.conf` and writes an annotated example
there on first run. One `pattern = profile[,dpi]` per line; the first match
wins, so put specific patterns first.

```
csgo     = 1, 800
blender  = 2
fallback = 0
poll_ms  = 1000
```

## Limitations

These are properties of the hardware, not things left to do.

* **The firmware implements four lighting modes**: off, fixed, breathing and
  cycle. `WAVE`, `STARLIGHT`, `RIPPLE` and `CUSTOM` are defined by HID++ but
  absent on this device. Since `CUSTOM` is the frame-streaming mode, there is
  no way to push an animation into the mouse and let it run.
* **Animated effects therefore stop when the app stops.** They are streamed as
  solid-colour writes at 30 fps. Lighting stored into an onboard profile does
  persist without any software, and into Windows.
* **Live lighting needs host mode.** In onboard mode the LEDs follow the stored
  profile and 0x8070 writes are ignored. The app switches modes for you and
  always switches back, including on Ctrl-C and on error — otherwise the
  mouse's lighting would depend on a process that is gone.
* **`getZoneEffect` is not implemented** on this device (`ext_caps` 0x0000). It
  answers with zeros rather than an error, so a zero readback says nothing
  about whether a write landed. Confirm lighting by eye.
* **Only fixed and breathing can be set live.** `setZoneEffect` addresses
  effects by the zone's *effect index*, not the mode id, and this firmware
  rejects index 0 (off) and index 3 (cycle) under every parameter layout
  tested. Live "off" is therefore a fixed black, and live cycling has to be
  streamed from the host. Both modes are still storable in a profile sector,
  which uses mode ids rather than indices — two number spaces that are easy to
  conflate.
* **The report rate cannot be set live in onboard mode** — the device answers
  `invalid argument`. `g502 rate` therefore writes it into the active profile,
  which is also the only way to make it persist. `--live` takes host mode
  instead.
* **Per-application switching matches running processes, not the focused
  window.** Wayland does not let an ordinary process see the active window, so
  a rule means "while this program is open".
* **Surface tuning, lift-off distance and angle snapping are out of scope.**
  They are not standalone HID++ features here — G HUB drives them through the
  `engineering`-flagged `TDE_ACCESS` (0x1eb0) and `SPI_DIRECT_ACCESS` (0x1e22),
  writing sensor registers directly. A wrong write there is not recoverable
  from userspace.
* **Firmware update (DFU) is out of scope**, for the same reason.
* **Button rebinding is CLI and flash only**; the GUI's Buttons tab decodes
  bindings but does not yet edit them.
* Onboard profile writes are flash writes with finite endurance. Take a backup
  first, and do not script them in a loop.

## Status

| Feature | State |
|---|---|
| DPI, report rate, profile switching | working |
| Lighting: off, fixed, breathing, cycle, both zones | working |
| Host-streamed effects | working |
| Profile decode, backup, restore | working |
| Profile flash writes | working, readback-verified on hardware |
| GUI: sensor, lighting, profiles, buttons | working |
| Button rebinding, CLI and GUI | working |
| Per-application profile switching | working |
| Surface tuning / lift-off / DFU | out of scope, see above |
