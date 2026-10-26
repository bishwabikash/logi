# G502 HERO — HID++ 2.0 protocol reference

Everything here is either read directly off the device (046d:c08b, firmware
`U1 v27.03`, HID++ 4.2) or taken from libratbag's implementation of the same
device. Observations from hardware are marked **[probed]**; protocol layouts
taken from libratbag are marked **[libratbag]**.

## Transport

Two HID interfaces. Only `.0005` carries the vendor collections
`ff00.0001` (short, report `0x10`, 7 bytes) and `ff00.0002` (long, report
`0x11`, 20 bytes). On this host that is `/dev/hidraw2`, but the node number
is not stable — probe for the one that answers the root feature. **[probed]**

Request: `[report_id, device_index, feature_index, (function<<4)|sw_id, params…]`
`device_index` is `0xFF` for wired. `sw_id` is any value 1–15; echo it back to
tell your replies apart from unsolicited event notifications. Errors return
with `feature_index == 0xFF`.

## Feature table **[probed]**

19 features. The ones that matter:

| idx | id | name | use |
|----|--------|------|-----|
| 2  | 0x8070 | COLOR_LED_EFFECTS | both RGB zones |
| 5  | 0x1300 | LED_CONTROL | SW/HW lighting arbitration |
| 10 | 0x2201 | ADJUSTABLE_DPI | sensor DPI |
| 11 | 0x8060 | REPORT_RATE | polling rate |
| 12 | 0x8100 | ONBOARD_PROFILES | profiles, buttons, macros |
| 13 | 0x8110 | MOUSE_BUTTON_SPY | button event capture |

Marked `hidden,engineering`: `0x18a1` LED_TEST, `0x1e00`
ENABLE_HIDDEN_FEATURES, `0x1e22` SPI_DIRECT_ACCESS, `0x1eb0` TDE_ACCESS,
`0x1801` MANUFACTURING_MODE, `0x1802` DEVICE_RESET, `0x1806`
CONFIG_DEVICE_PROPS, `0x18b1` STATIC_MONITOR_MODE. Writing these can brick the
device. Surface tuning / lift-off distance live behind them; see "Not covered".

## 0x2201 ADJUSTABLE_DPI **[probed]**

1 sensor. `getSensorDpiList` returns `0064 e032 6400`. A word with `0xE000`
set is a *step*, not a DPI — so this is a **range**, not a list:

    min 100, max 25600, step 50

`getSensorDpi` → current at bytes 1-2, default at bytes 3-4 (1000).
Note libratbag only exposes 5 discrete stages; the hardware accepts any
50-step value in range.

## 0x8060 REPORT_RATE **[probed]**

`getReportRateList` → bitmask, bit *N* means interval *N+1* ms is supported.
This device: **125 / 250 / 500 / 1000 Hz**. `getReportRate`/`setReportRate`
take the interval in ms, not Hz.

**`setReportRate` only works in host mode.** In onboard mode the rate comes
from the active profile and the device answers `invalid argument`. A rate set
live is also lost when the device returns to onboard mode, so to make one
stick you write byte 0x00 of the profile sector instead. Verified both ways on
the hardware. **[probed]**

## 0x8070 COLOR_LED_EFFECTS

Functions: 0 getInfo, 1 getZoneInfo, 2 getZoneEffectInfo,
3 getZoneEffect, 4 setZoneEffect. **[libratbag]**

`getInfo` → `02 00 03 00 00`. The reply is `{u8 zone_count, u16 nv_caps,
u16 ext_caps}`, so this is **2 zones, nv_caps 0x0003, ext_caps 0x0000**. **[probed]**

ext_caps bit 0 is `HAS_ZONE_EFFECT`. It is **clear**, so this device does not
implement `getZoneEffect` at all — libratbag guards the call on exactly that
bit. The device answers the request with all zeros instead of an error, so a
zero readback says nothing about whether your write landed. Verify lighting
changes by eye, not by readback. **[probed + libratbag]**

Zones **[probed]**:

| zone | location | effects |
|---|---|---|
| 0 | 0x0001 primary / logo | 4 |
| 1 | 0x0002 logo (DPI indicator) | 4 |

Only four effects are implemented in firmware — `off`, `fixed`, `breathing`,
`cycling`. `WAVE`, `STARLIGHT`, `RIPPLE` and **`CUSTOM` (direct frame
streaming)** are defined in the protocol but **absent on this device**. **[probed]**

### setZoneEffect payload

`[zone_index, effect_index, …10 bytes]` — **twelve bytes total**. **[probed]**

Byte 1 is the zone's **effect index**, not the mode id. This is easy to miss
because they coincide for `FIXED` (index 1, id 1) and diverge for everything
else: `BREATHING` is id `0x0a` at **index 2**. Sending the id returns
`invalid argument`. Resolve the index from the zone's effect list
(`getZoneEffectInfo`), which on this device is:

| index | mode id | mode |
|---|---|---|
| 0 | 0x00 | off |
| 1 | 0x01 | fixed |
| 2 | 0x0a | breathing |
| 3 | 0x03 | cycle |

**Only indices 1 and 2 are settable live.** Index 0 (off) and index 3 (cycle)
are rejected with `invalid argument` under every parameter layout tried —
period big- and little-endian, 992/2000/4000/8000 ms, intensity 0/50/100, and
the period at four different offsets. So live "off" has to be a fixed black,
and live cycling has to be streamed from the host. **[probed]**

Both modes are nonetheless storable in a profile sector, which uses **mode
ids**, not indices: writing `0x03` into an LED record and reading it back
works and the device honours it. The two number spaces are easy to conflate —
the live path takes indices, the flash path takes ids. **[probed]**

The per-mode parameter layout below is the `hidpp20_internal_led` union, and
applies to the flash records; the live path uses the same parameter bytes
after its index byte. **[libratbag]**

Modes and their ids:

    OFF 0x00   FIXED 0x01   CYCLE 0x03   WAVE 0x04
    STARLIGHT 0x05   BREATHING 0x0A   RIPPLE 0x0B   CUSTOM 0x0C

Per-mode parameter layout (the 10 bytes after `mode`):

| mode | layout |
|---|---|
| FIXED 0x01 | `R G B effect` |
| CYCLE 0x03 | `· · · · ·` (5 unused) `period:u16_be` `intensity` |
| BREATHING 0x0A | `R G B period:u16_be waveform intensity` |

`intensity` is 1–100 percent; 0 means 100. `period` is milliseconds.
Breathing `waveform`: 0 default, 1 sine, 2 square, 3 triangle, 4 sawtooth,
5 sharkfin, 6 exponential.

The 5 unused bytes before CYCLE's period are the usual reason a hand-rolled
cycling write is rejected with `invalid argument`.

## 0x8100 ONBOARD_PROFILES

Functions: 0 getInfo, 1 **setOnboardMode**, 2 getOnboardMode,
3 setCurrentProfile, 4 getCurrentProfile, 5 memoryRead, 6 memoryAddrWrite,
7 memoryWrite, 8 memoryWriteEnd, 11 getCurrentDpiIndex,
12 setCurrentDpiIndex. **[libratbag]**

Careful: function 1 is a *setter*. Calling it to "read" the mode writes it.

`getInfo` → `01 02 01 05 05 0b 10 0100 0a` **[probed]**

    memory_model_id   1
    profile_format_id 2
    macro_format_id   1
    profile_count     5      (+5 ROM)
    button_count      11
    sector_count      16
    sector_size       256 bytes
    mechanical_layout 0x0a

Modes: 1 = onboard (device drives itself), 2 = host/software (the host drives
it). **Live lighting via 0x8070 only takes effect in mode 2** — in onboard
mode the LEDs follow the stored profile and 0x8070 writes are inert. This is
the switch G HUB flips for its software effects. Restore mode 1 when done or
the mouse stays dependent on the host. **[probed]**

Mode 0 is invalid; the firmware rejects it harmlessly. **[probed]**

Switching modes needs a settle delay before the device honours writes in the
new mode. 50 ms was marginal — a lighting write issued immediately after the
switch intermittently returned `invalid argument`. 150 ms was reliable across
repeated runs. **[probed]**

## Not covered

**Surface tuning, lift-off distance, angle snapping.** These are not
standalone HID++ features on this device — the feature table has no entry for
them. G HUB implements them through the `engineering`-flagged features above
(`TDE_ACCESS` 0x1eb0 / `SPI_DIRECT_ACCESS` 0x1e22), writing sensor
registers directly. No open-source implementation exists, and a wrong write
is not recoverable from userspace.

The G HUB installer (`lghub_installer.exe`, 2026.4.9028.0,
sha256 `4b2f9903…e811e1`) does **not** contain this code. It is a WiX Burn
downloader chain: six embedded PE binaries, of which three are Burn
bootstrappers (`burn.pdb`), one is `logi_installer_shared.dll`, and one is
`lghub_sso_handler.exe`. Zero HID/HID++ strings in any of them. The agent that
speaks to the device (`lghub_agent.exe`) is fetched at install time from
Logitech's build pipeline and is not in the installer. Reverse engineering it
requires obtaining that binary separately.

### Why the agent binary is hard to obtain

The three `.wixburn` bundles inside the installer are **Microsoft Visual C++
2022 Runtime 14.42.34438** redistributables (x86/x64/arm64) plus the KB2999226
Universal CRT CABs — bundled dependencies, not G HUB. Their Burn manifests
carry **zero `DownloadUrl` attributes**, so the vcredists are fully embedded
and nothing is fetched from them. The G HUB installer is itself not a Burn
bundle, so WiX's `/layout` download-without-install flag does not apply; its
only flags are `/install /repair /uninstall /quiet /passive /norestart /?`.

`lghub_agent.exe` is pulled at install time from Logitech's "pipeline" depot:

    pipeline://core/LGHUB/lghub_software_manager.exe

Hosts referenced by `logi_installer_shared.dll`: `pipeline.logitech.io`
(resolves to RFC1918 addresses — not publicly routable),
`updates.ghub.logitechg.com` (CloudFront → S3), `2pipeline.s3.amazonaws.com`.
Endpoints `/pipeline/v2/access/`, `/details.json`, `/content.json`,
`/update.json`, `/updates/channel`.

The S3 bucket denies `ListBucket` and returns a blanket `AccessDenied` for
every key including bogus ones, so `NoSuchKey` is masked and response codes
cannot be used to confirm a path. Blind key guessing is not viable. Even with
a correct key, fetching is not a plain download: the DLL implements AWS
SigV4-signed requests (`Authorization: …4-HMAC-SHA256 Credential=…`), RSA
depot verification (`depot_verifier_rsa`), key rotation (`keymaster`,
`keys_refresh_download_group`), encryption at rest (`DecryptedStream`,
`CapsuleResourceProtectedStream`) and protobuf schemas
`logi.protocol.updates.{Depot,Signature,KeyStatus,ContentRequest}`.

The remaining practical route is to let the installer do the download itself
inside a throwaway Wine prefix. On this host that is blocked on i386 multiarch
(`wine32` missing), which is a system-level change.

## 0x8110 MOUSE_BUTTON_SPY

Present on this device, reporting **11 buttons**. Functions:

| func | name | notes |
|---|---|---|
| 0 | getNbOfButtons | returns 0x0b here |
| 1 | startSpy | begins unsolicited button notifications |
| 2 | stopSpy | |
| 3 | getRemapping | 11 bytes, one per physical button |
| 4 | **setRemapping** | **a setter — see the warning below** |

**[probed]**

### The remapping table, and a way to brick your buttons

`getRemapping` returns eleven bytes whose default is the identity table
`01 02 03 04 05 06 07 08 09 0a 0b`. Writing **zeros** to it with
`setRemapping` unmaps every button: the mouse keeps moving, but no button does
anything, in either memory mode. It survives a mode change, so it looks exactly
like the device being broken.

This is easy to do by accident. Probing a feature by calling its functions
`0..n` with empty parameters sends `setRemapping` a table of zeros. The same
trap exists at `0x8100` function 1, which is `setOnboardMode`, not a getter.
**Do not sweep unknown HID++ functions with empty parameters.** Resolve what
each function is first.

To recover:

    setRemapping(01 02 03 04 05 06 07 08 09 0a 0b)

### Event format

After `startSpy`, the device sends unsolicited long reports:

    11 ff 0d 00 | 00 01 00 00     button 1 down
    11 ff 0d 00 | 00 00 00 00     all released

Byte 2 is the feature index, byte 3 is zero (event, no software id). The
payload's first two bytes are a **big-endian u16 bitmap of pressed buttons**,
bit 0 being button 1. **[probed]**

## Memory mode does not affect input

Worth stating because it is easy to assume otherwise: a G502 in host mode
reports movement and button presses exactly as it does in onboard mode.
Measured by alternating modes while using the mouse and counting kernel input
events: onboard 1261, host 3273, onboard 4960, with buttons registering in all
three. **[probed]**

What host mode changes is that the stored profile is not applied, so lighting
is whatever the host last pushed, and the live `0x8070` and `0x8060` setters
start working. Nothing else. **[probed]**
