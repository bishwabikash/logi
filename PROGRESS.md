# Status

What works, what is being built, and what is deliberately out of scope. Kept
current so nothing has to be rediscovered.

## Works now, verified on hardware

Verified against a real G502 HERO (046d:c08b, firmware U1 v27.03, HID++ 4.2).
Every write below was exercised and then reverted, with the device confirmed
byte-identical to a backup taken first.

| Area | State |
|---|---|
| Device discovery | Any Logitech device answering HID++; no model whitelist |
| Capability probing | From the device's own feature table |
| Device naming | From feature 0x0005, not a table in this repo |
| DPI | Continuous range, snapping, presets |
| Report rate | Live and persisted-to-profile |
| Lighting | off / fixed / breathing, both zones, live and stored |
| Host-side effects | static, breathe, rainbow, pulse, wave |
| Onboard profiles | Switching, decoding, backup, restore |
| Profile flash writes | Readback-verified on every write |
| Button rebinding | CLI and GUI, written to flash |
| Per-app switching | `logi watch` and the background service |
| GUI | Sensor, Lighting, Profiles, Buttons, About |
| Packaging | udev rules, desktop entry, systemd user unit, installer |
| Service | Installed, enabled, running as `logi serve` |

## Editable from the app

Seven tabs: Sensor, Lighting, Profiles, Buttons, Apps, Service, About.
209 tests pass.

| Panel | Edits |
|---|---|
| Profiles | Name, DPI stages, default and sniper index, rate, per-zone lighting, all 11 buttons. Staged in memory, committed by one explicit save, because this is flash |
| Buttons | Host bindings: a key or chord, typed text, another button, a command, or nothing. Augment or Replace |
| Apps | Per-application rules, with a picker listing running programs so no process name has to be typed |
| Service | Start, stop, restart, start-at-login, and the journal tail |

Button bindings also work from the CLI: `logi bind`, `logi bind run`, and
`logi bind reset`, which restores the factory button mapping and is the escape
hatch if a button stops responding.

## Tray icon

Spawned from the GUI and verified registered with GNOME's watcher as
org.kde.StatusNotifierItem, exposing /StatusNotifierItem and /MenuBar. The
menu carries the device, service state with start/stop/restart, a profile
submenu, a DPI submenu, open and quit. Absent on a desktop with no tray host,
which the app treats as normal rather than an error.

## The terminal is no longer needed

The background service runs the per-application rules and the button bindings
together, so bindings edited in the Buttons tab take effect without anything
being typed. They share the device: the watcher issues commands on the control
handle while the spy reads events on its own, which works because hidraw
queues input reports per open file description.

One command is still needed once, to install the udev rules, because an
unprivileged GUI cannot grant itself device access.

## Deliberately out of scope

**Surface tuning, lift-off distance, angle snapping.** Not standalone HID++
features on this device. G HUB drives them through the engineering-flagged
TDE_ACCESS (0x1eb0) and SPI_DIRECT_ACCESS (0x1e22), writing sensor registers
directly. A wrong write there is not recoverable from userspace.

**Firmware update (DFU).** Same reasoning.

**Obtaining lghub_agent.exe.** The G HUB installer is a WiX Burn downloader
whose embedded bundles are Visual C++ redistributables. The agent binary comes
from Logitech's pipeline depot behind SigV4 signing, RSA depot verification and
encryption at rest, over an S3 bucket that masks NoSuchKey as AccessDenied.
Documented in PROTOCOL.md.

## Hard-won facts

Each of these cost real debugging; they are written up in PROTOCOL.md.

* `setZoneEffect` addresses effects by the zone's **effect index**, not the
  mode id. They coincide only for fixed. Only indices 1 and 2 are settable
  live; off and cycle are rejected.
* `setReportRate` is rejected in onboard mode, where the rate comes from the
  profile. Persisting a rate means writing the sector.
* Memory mode does **not** affect buttons or movement. Measured by alternating
  modes while using the mouse.
* `0x8110` function 4 is `setRemapping`. Writing zeros to it unmaps every
  button, survives unplugging, and looks exactly like broken hardware. This
  happened during development. Recovery is restoring
  `01 02 03 04 05 06 07 08 09 0a 0b`.
* Probing a feature by calling functions `0..n` with empty parameters hits
  setters. It has caused two incidents here. Resolve each function first.
* Profile sector DPI stages are little-endian; the sector CRC is big-endian
  CRC-16/CCITT-FALSE over bytes 0..253.

## Next: the app should follow the device live

Today nothing the mouse does on its own reaches the app. The worker waits for
commands and only re-reads state after running one, so pressing the DPI button
on the mouse, or the background service switching profile for a rule, leaves
the GUI and the tray showing the old value until something is clicked.

**What the hardware offers.** Per Solaar's notification handler, 0x8100
announces two things unprompted: the current profile changed (byte 3 = 0x00,
payload a big-endian u16 profile sector) and the current DPI index changed
(byte 3 = 0x10, payload the new stage index). The event carries the index, not
the DPI, so the value still has to be read afterwards. Nothing else in the
references announces a DPI change. **Not yet confirmed on a G502 HERO.**

**Design.**
* A listener thread on `Device::open_listener()`, because `call` drains the
  queue before every request and would eat events on the control handle. It
  decodes 0x8100 events and nudges the worker.
* The worker stays the only owner of the control handle: on a nudge it re-reads
  the cheap state (about five round trips) and publishes. No outcome is
  recorded, since nobody asked.
* A slow reconcile poll while idle, as the safety net: events might not fire in
  every mode, and other software (or the service in another process) can change
  state too. Publish only when something actually differs.
* Skip both while an effect owns the device.
* Show which DPI stage is active, not just the number.
* The GUI already repaints every 500 ms and reads the snapshot each frame, and
  the tray is fed from the same snapshot, so neither needs to change.

**Waiting on:** the review workflow, which is editing device.rs, hidpp.rs and
gui/mod.rs. Building on files other agents are mid-edit in is how two earlier
rounds ended with a tree that would not compile.

