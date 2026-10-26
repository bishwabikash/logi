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

## Built, not yet reachable from the UI

`src/buttonspy.rs`, `src/uinput.rs`, `src/actions.rs` compile with 101 tests
passing between them and the rest of the suite. They cover the 0x8110 button
spy, a uinput virtual device, and the action model with both bind modes. The
CLI and GUI entry points are still to be wired.

## In progress

In-app editing of profiles, per-application rules and button bindings; a tray
icon carrying service status and a context menu; and service control from
inside the app. The aim is that the terminal is never required.

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
