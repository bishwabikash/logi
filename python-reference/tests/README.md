# tests

Every test here is **offline**. Nothing in this directory may open a device,
read or write `/dev/hidraw*`, require root, or care whether a G502 is plugged
in. The hardware side is verified by hand against a real mouse; these tests
cover the parts that are pure logic — byte layouts, CRCs, descriptor decoding,
rule matching, frame generation — which is where the bugs actually are.

Writing to profile flash with a wrong CRC can leave a profile unusable, so the
codec that produces those bytes is exactly the thing that needs test coverage
that does not involve a device.

## Running them

From the repository root:

```
python3 -m unittest discover tests
```

Run from the root so `import g502` resolves to the checkout rather than an
installed copy. A single module or case:

```
python3 -m unittest tests.test_profile_blob -v
python3 -m unittest tests.test_profile_blob.TestCrc.test_known_vector
```

No arguments, no device, no `sudo`. If a test prompts for a password or hangs
for ~0.8 s per call, it is talking to hardware and is broken.

## Rules for new tests

* **Never construct `g502.hidpp.Device`.** Its `__init__` opens the path it is
  given. Use a fake that implements the same surface instead:
  `call(feature_index, function, params=b"", long=True, timeout=...)`,
  `send_nowait(...)`, `flush()`, `feature_index(feature_id)`, `has_feature()`,
  `protocol_version()`.
* **Never call `discover.open_device()`.** Importing `g502.discover` is safe —
  it only touches sysfs and `/dev` from inside `open_device()` and
  `candidates()` — but neither of those belongs in a test.
* Feed the fake the real replies recorded from the device (they are in
  `PROTOCOL.md`), so a decoder test fails when the decoder drifts rather than
  when the fixture does.
* Keep the filesystem out of `$HOME`. `g502.config` defaults to
  `~/.config/g502/config.json`; pass an explicit path under `tempfile` and
  assert on that.
* Effect generators in `g502.effects` are infinite and read the monotonic
  clock. Pull a bounded number of frames (`itertools.islice`) and assert on
  shape and range, not on exact values at a given time.
* `effects.audio` shells out to `parec`. Do not exercise it; assert only that
  it refuses cleanly when `parec` is absent.

## What is worth covering

| Target | Why |
|---|---|
| `profile_blob.crc_ccitt` | A wrong CRC is rejected by the firmware and can brick a profile. |
| `Profile.parse` → `.serialize()` | Must be byte-identical for all five stored sectors, including bytes the codec does not model. |
| `Button.*` / `Led.*` constructors and `.describe()` | These are the bytes that get written to flash. |
| `led.parse_color` | User input, including the named colours. |
| `led.Leds` parameter packing | Per-mode layouts differ; `CYCLE` has five unused bytes before its period. |
| `dpi.Dpi.capability` / `.set` | Range-vs-list decoding hinges on the `0xE000` step marker; `set` snaps to the step. |
| `rate.ReportRate.supported` | Bitmask-to-Hz conversion. |
| `profiles.OnboardProfiles.write_sector` | Must refuse a bad CRC and a wrong length *before* sending anything. |
| `appwatch.match_rule` | First-match-wins, substring and case handling. |
| `config.load` / `.save` | Defaults merge, atomic replace, bad JSON. |
| `effects` generators | Output shape, channel range, zone count. |
