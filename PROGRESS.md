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

**Logitech's own Windows software.** Its installer downloads the component
that drives the sensor tuning features at install time, and fetching it was not
pursued. See PROTOCOL.md, "Not covered".

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

## Code review: ended at round seven, by decision

Ten units were reviewed in rounds by independent agents, each fixing in place.
Non-minor findings per round: 25, 17, 16, 9, 13, 4, 2. Criticals: 4, 2, 2, then
none from round four on. 408 tests.

Every unit's last completed review found nothing non-minor except `cli`, whose
two round-seven fixes (retrying the button-table restore at teardown, and
settling a restore still owed from an earlier unplug) were made and then not
re-reviewed, because round eight was stopped as it began.

**This is static review, and it cannot confirm what the hardware does.** Every
protocol fact that broke something in this project was found on the mouse, not
in review. Not yet exercised against the device: the service acting on button
bindings, silencing a button (which rests on how the remapping table's values
are read), the profile editor saving to flash, and the Rust virtual input
device. Silencing matters most, since the table is non-volatile and a wrong
assumption leaves a mouse that cannot click. `logi bind reset` restores it.

## Following the device

The window and the tray follow changes made on the mouse itself or by other
software. A listener thread on its own handle decodes 0x8100's two
announcements (current profile changed, current DPI stage changed) and nudges
the worker; the worker also re-reads the cheap state every 1.5 s regardless,
and publishes only when something a panel shows has moved. The same re-read
notices an unplugged mouse.

Checked on a G502 HERO two ways. With `logi state --follow` running while a
second process changed DPI and profile, every change appeared within about a
second and a half. And with the DPI buttons pressed by hand, the mouse
announced each press (75 announcements, 74 state changes tracked in 50 s, about
0.35 s apart while pressing, final state consistent). The stage index and the
sensor DPI occasionally disagreed for one read, 2 of 74, and the next read
agreed. **Not yet watched:** the profile button.

A window opened before the build that added this keeps running the old code, so
it will not follow anything until it is closed and reopened.

## Changes sent by themselves

The window has no Save or Apply buttons for the mouse. A change goes out once
it has stopped changing: 0.3 s for a live colour, 2 s for anything that writes
flash (`src/gui/autosend.rs`). Gates, all of which must hold for a flash write:
no other flash write in flight; the once-per-launch safety backup has been
taken (`src/backups.rs`, into `~/.local/share/logi/backups`, nothing written if
the newest backup already matches); the profile still has a left click, a valid
checksum and at least one DPI stage. A write that fails is shown with Try again
and is never repeated automatically. Closing the window sends what is still
waiting.

The Sensor tab's DPI stages are the active profile's stored stages, edited
through the same staged draft as the Profiles tab: chips to choose, a slider
over the range the mouse reported, double-click to type, add and remove.

Checked on the G502 HERO: the backup is written, is not duplicated on a second
launch, and `logi restore` of it changes nothing; opening the window changes no
byte of the mouse. **Not run on hardware:** an edit made in the window being
sent. The write it uses is the one verified from the CLI.

## Report rate from the Sensor tab

The live rate setter is refused by the mouse outside host mode, and the Sensor
tab used to send it without going there first, so in onboard mode a click did
nothing. It now switches to host mode first (as the lighting commands do) and
also stages the rate into the active profile, which is then sent like any other
edit. Checked on the G502 HERO with the CLI: a rate stored in a profile applies
once the mouse returns to onboard mode (500 Hz read back after host then
onboard), and the original state was restored byte for byte.

## Closing the window

The window closes for real and the process stays as the panel icon
(`src/gui/host.rs`, `run` in `src/gui/mod.rs`). A background loop with a worker
of its own serves the icon's menu until it asks for a window; starting `logi`
again signals the running one (`src/gui/instance.rs`, a lock file and SIGUSR1)
instead of making a second icon. Checked on GNOME (the session is Wayland; the
window was driven through XWayland so it could be closed and found from a
script): close leaves the process and icon, the icon's click and its "Open
Logi" item open a new window, a second launch does the same, and "Quit" ends
the process and removes the icon with the mouse left in onboard mode. **Not
checked:** the look of the icon in the dock, which needs the desktop entry and
icon installed by `packaging/install.sh`.

## The icon follows the service

`logi serve` starts `logi tray` through `systemd-run --user` as a transient unit
of its own (`logi-tray`), so the icon is up whenever the service is, and is not
taken down with it: stopping or restarting the service leaves the one icon in
place. Checked on GNOME: restarting the service brought the icon up, a second
restart did not duplicate it, stopping the service kept it, and the icon's
click started a window from that unit. The installer also adds the bin
directory to PATH (tested in a scratch home with bash and zsh, twice, for one
block per file).

Found after a reboot: the service started the icon before GNOME had brought up
its tray support (the AppIndicator extension), the single registration attempt
found no host and the process exited, so there was no icon. Registration is now
retried every 3 s (up to 100 times; a tray-only start waits up to five
minutes). Reproduced with a private session bus whose watcher appears 8 s late:
the tray stayed up, logged once, and registered when the watcher came.

## Warnings

The tree builds with no warnings and CI denies them. Dead code was deleted or
made `#[cfg(test)]`; the firmware version is now read at connect and shown on
the About tab, and the Service tab can install and reinstall the user unit
itself.
