# Contributing

Thanks for looking. This is a small project that writes to real hardware, so
most of what follows is about doing that without hurting a mouse.

## What is most useful

* **A report from a mouse other than the G502 HERO.** Nothing else has been
  tried, so a note of what worked and what did not is worth more than code.
  See [Reporting a device](#reporting-a-device).
* **A fix for something that is wrong.** Bugs, and especially places where the
  app says something untrue about the device.
* **Protocol facts you have checked on a real mouse**, written up in
  [PROTOCOL.md](PROTOCOL.md).
* **Running the parts that have only unit tests** against hardware. The README
  lists them under "What has and has not been tested".

For anything bigger than a fix, please open an issue first. It saves you
writing something that does not fit.

## Building and testing

You need a Rust toolchain, 1.89 or newer. No `-dev` packages are needed.

```
cargo build --release
cargo test
```

`cargo test` needs no mouse and no special permissions, and CI runs the same
thing on every push and pull request, plus a build on the minimum Rust version.
Four tests are marked `#[ignore]` because they need a `/dev/uinput` you can
write to, a session bus with a tray host, or a systemd user session. Run them
with `cargo test -- --ignored` if you want to.

Tests must not open `/dev/hidraw*` or `/dev/uinput`, and must not touch your
real `~/.config`. Use a temporary directory. A test that needs the device is an
ignored test, and CI never runs it.

If you add a dependency, check that the minimum Rust version in `Cargo.toml` and
the README still holds. CI will tell you.

## Writing to a device

Read this before running anything that writes. Each rule here comes from
something that went wrong on the development mouse.

* **Onboard profiles are flash with finite endurance.** Never write on a
  keystroke, a slider drag or a loop. The window sends a change only after it
  has stopped changing (`src/gui/autosend.rs`), one write at a time, never
  retries a failure on its own, and waits for the once-per-launch backup.
  Anything new that writes flash goes through the same gates. Always verify by
  reading back.
* **Take a backup first:** `logi backup before.json`, and `logi restore` puts it
  back.
* **The button remapping table survives unplugging.** Writing zeros to it leaves
  a mouse that moves but cannot click, and it looks exactly like broken
  hardware. The code refuses that write. `logi bind reset` is the way back, so
  know it before you test anything that touches buttons.
* **Do not probe unknown HID++ functions by calling `0..n` with empty
  parameters.** Some functions are setters. That is how both of the problems
  above were first found. Work out what a function is before calling it.
* **A write that times out may still have been applied.** Treat an error from a
  setter as "state unknown", not "nothing happened".
* **Anything touching the sensor-tuning registers is out of scope** (surface
  tuning, lift-off distance, firmware update). A wrong write there is not
  recoverable from the host. See "Not covered" in PROTOCOL.md.

## Changing the protocol layer

[PROTOCOL.md](PROTOCOL.md) marks every fact by where it came from: **[probed]**
from a real device, or taken from libratbag or Solaar. Keep doing that. When you
add or change a fact, say which mouse and firmware you saw it on. A guess that
is written down as a fact costs the next person a debugging session.

Two things about where information may come from:

* **libratbag is MIT, Solaar is GPL-2.0, and this project is MIT.** Read either
  to learn how the protocol works, then write your own code. Do not copy code
  from Solaar.
* **Do not add anything taken from Logitech's software**: no binaries, no
  installers, no decompiled code, no captured traffic from it. The `artifacts/`
  and `reference/` directories are git-ignored for this reason; keep it that
  way.

## Reporting a device

Open an issue with:

* the output of `logi devices` and `logi info`
* your distribution, and whether the session is Wayland or X11
* what you tried and what happened

Please do not run anything that writes until you have a backup, and say so in
the report if you did. If a feature your mouse reports does not work, that is
the interesting part.

## Reporting a bug

Include `logi --version`, your distribution and session type, and the steps. For
anything involving the service, `journalctl --user -u logi` is usually the
answer to "what did it do". If a button stopped working, run `logi bind reset`
first, then describe what led up to it.

## Code

* **Match the surrounding code.** The code is not rustfmt-formatted, so do not
  run `cargo fmt` over a file: it would bury your change in unrelated
  reformatting.
* **Comments say why**, not what. The reasoning behind an odd choice (a delay, a
  refusal, an ordering) belongs next to it, because the odd choices are mostly
  scars.
* **No compiler warnings.** The tree has none and CI builds with
  `-D warnings`. Code only a test uses is `#[cfg(test)]`; unused code is
  deleted, not silenced.
* **No model whitelist.** Behaviour comes from the device's own feature table. A
  control the device does not report is hidden or disabled with a reason, never
  shown as a button that errors.
* **Anything that can fail on a missing device, missing file, absent systemd or
  absent tray host must degrade with a message**, not panic.

For the window, use the helpers in `src/gui/theme.rs` instead of styling
widgets by hand. The accent colour means "selected" or "live value" and nothing
else; state uses the separate good, warning and bad colours. egui widgets can
write back through their `&mut` argument during painting (`DragValue::range` and
`Slider` both clamp an existing value by default), so give them a local copy.

## Commits and pull requests

* One logical change per commit. Explain **why** in the body; the diff already
  shows what.
* Keep the history tidy: fix up a mistake instead of stacking a "fix" commit on
  it.
* In the pull request, say what you ran and on which mouse. "Unit tests only"
  is a fine answer, and an honest one.
* Do not move something to "verified" in the README unless it ran on the
  hardware. Say which mouse and firmware.

By contributing you agree your work is released under the [MIT license](LICENSE).
