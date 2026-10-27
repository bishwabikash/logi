//! Driver and management app for the Logitech G502 HERO, over HID++ 2.0.
//!
//! Run with no arguments to open the GUI; subcommands give the same control
//! from a shell.

mod actions;
mod appwatch;
mod buttonspy;
mod capabilities;
mod deviceinfo;
mod device;
mod discover;
mod dpi;
mod effects;
mod features;
mod gui;
mod hidpp;
mod led;
mod profile_blob;
mod profile_io;
mod profiles;
mod rate;
mod service;
mod tray;
mod uinput;

use hidpp::Error;

const USAGE: &str = "\
logi — control a Logitech mouse over HID++ 2.0

  logi                        open the graphical app
  logi devices                every Logitech HID++ device, and what it can do
  logi info                   capabilities and current state
  logi dpi [<n>]              show or set DPI (continuous, snapped to the step)
  logi rate [<hz>] [--live]   show or set the report rate; persists to the
                              active profile unless --live is given
  logi button [<n> <action>]   show or rebind buttons (writes flash)
  logi mode [onboard|host]    show or set who drives the mouse. Live lighting
                              needs host; onboard hands it back so its stored
                              profile applies with nothing running.
  logi profile [<n>]          show or switch the active onboard profile
  logi profile show <n>       decode one stored profile
  logi led <spec>             lighting, live (needs host mode)
                                off | <colour> | breathing <colour> | cycle
  logi effect [list|<name>]   stream a host-side animation until Ctrl-C
  logi watch [--dry-run]      switch profiles per running application, from
                              ~/.config/logi/rules.conf
  logi backup <file>          dump every sector to JSON
  logi restore <file>         write a backup back to flash
  logi --help

Colours take hex (ff0066, #ff0066) or a name (red, cyan, purple...).
With more than one Logitech device attached, pick one with G502_DEVICE,
e.g. G502_DEVICE=046d:c08b logi info
";

fn main() -> std::process::ExitCode {
    // Rust masks SIGPIPE, so `logi info | head` makes println! panic on EPIPE
    // instead of exiting quietly. Restore the default so piping behaves.
    // SAFETY: restoring a signal's default disposition before any threads or
    // output exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return match gui::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("logi: could not start the GUI: {e}");
                eprintln!("      run `logi info` for the command line instead.");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(CliError::Usage(m)) => {
            eprintln!("logi: {m}\n\n{USAGE}");
            std::process::ExitCode::from(2)
        }
        Err(CliError::Failed(m)) => {
            eprintln!("logi: {m}");
            std::process::ExitCode::FAILURE
        }
    }
}

enum CliError {
    Usage(String),
    Failed(String),
}

impl From<Error> for CliError {
    fn from(e: Error) -> Self {
        match e {
            Error::Permission(m) => CliError::Failed(format!(
                "{m}\n\
                 \n\
                 Install the udev rule, then unplug and replug the mouse:\n\
                 \x20   packaging/install.sh\n\
                 \n\
                 The rule tags the node with uaccess, so your logged-in session gets\n\
                 access and sudo is no longer needed. As a one-off: sudo logi ..."
            )),
            other => CliError::Failed(other.to_string()),
        }
    }
}

/// The device id in $G502_DEVICE (e.g. "046d:c08b"), if one is set.
fn wanted_device() -> Option<String> {
    std::env::var("G502_DEVICE").ok().filter(|s| !s.trim().is_empty())
}

/// Open the device named by $G502_DEVICE, or the only one.
fn open() -> Result<discover::Found, CliError> {
    Ok(discover::open_device_matching(wanted_device().as_deref())?)
}

fn run(args: &[String]) -> Result<(), CliError> {
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "devices" | "list" => cmd_devices(),
        "info" => cmd_info(),
        "dpi" => cmd_dpi(args.get(1)),
        "rate" => cmd_rate(&args[1..]),
        "profile" => cmd_profile(&args[1..]),
        "button" => cmd_button(&args[1..]),
        "mode" => cmd_mode(args.get(1)),
        "watch" => cmd_watch(&args[1..]),
        "serve" => cmd_serve(),
        "bind" => cmd_bind(&args[1..]),
        "led" => cmd_led(&args[1..]),
        "effect" => cmd_effect(&args[1..]),
        "backup" => {
            let p = args.get(1).ok_or_else(|| CliError::Usage("backup needs a file path".into()))?;
            cmd_backup(p)
        }
        "restore" => {
            let p = args.get(1).ok_or_else(|| CliError::Usage("restore needs a file path".into()))?;
            cmd_restore(p)
        }
        other => Err(CliError::Usage(format!("unknown command {other:?}"))),
    }
}

fn cmd_devices() -> Result<(), CliError> {
    let found = discover::list_devices();
    if found.is_empty() {
        println!("No Logitech HID++ device found.");
        println!();
        println!("A plain mouse with no configurable features does not speak HID++ and");
        println!("will not appear here. If you expected one, check permissions:");
        println!("  ls -l /dev/hidraw*    (the device's nodes should be group plugdev)");
        return Ok(());
    }
    for f in &found {
        println!("{}  {}", f.id(), f.name);
        println!("    type      : {}", f.kind);
        println!("    endpoint  : {}", f.device.path);
        println!("    HID++     : {}.{}", f.protocol.0, f.protocol.1);
        println!("    features  : {} ({} total)", f.caps.summary(), f.caps.features.len());
        if !f.caps.restricted.is_empty() {
            let ids: Vec<String> =
                f.caps.restricted.iter().map(|i| format!("0x{i:04x}")).collect();
            println!("    restricted: {} (not touched)", ids.join(" "));
        }
        println!();
    }
    if found.len() > 1 {
        println!("Pick one with G502_DEVICE=<id>, e.g. G502_DEVICE={}", found[0].id());
    }
    Ok(())
}

fn cmd_info() -> Result<(), CliError> {
    let mut f = open()?;
    let model = f.name.clone();
    let kind = f.kind.clone();
    let id = f.id();
    let path = f.device.path.clone();
    let caps = f.caps.clone();
    let dev = &mut f.device;
    println!("device   : {model}");
    println!("type     : {kind}  ({id})");
    println!("endpoint : {path}");
    if let Ok((a, b)) = dev.protocol_version() {
        println!("HID++    : {a}.{b}");
    }

    if let Ok(fs) = dev.feature_index(features::FEATURE_SET) {
        if let Ok(p) = dev.call(fs, 0x00, &[]) {
            let count = p.first().copied().unwrap_or(0);
            println!("\nfeatures ({count} + root):");
            for i in 0..=count {
                if let Ok(q) = dev.call(fs, 0x01, &[i]) {
                    if q.len() >= 3 {
                        let id = u16::from_be_bytes([q[0], q[1]]);
                        println!(
                            "  {i:3}  0x{id:04x}  {:<22} {}",
                            features::type_flags(q[2]),
                            features::name(id)
                        );
                    }
                }
            }
        }
    }

    println!();
    println!("supported: {}", caps.summary());
    if !caps.restricted.is_empty() {
        let ids: Vec<String> = caps.restricted.iter().map(|i| format!("0x{i:04x}")).collect();
        println!("restricted (not touched): {}", ids.join(" "));
    }

    if let Ok(d) = dpi::Dpi::new(dev) {
        println!("\nsensor:");
        match d.capability(dev, 0) {
            Ok(dpi::Capability::Range { min, max, step }) => {
                println!("  range    : {min}-{max} dpi, step {step}")
            }
            Ok(dpi::Capability::List(v)) => println!("  stages   : {v:?}"),
            Err(e) => println!("  range    : unavailable ({e})"),
        }
        if let Ok(v) = d.get(dev, 0) {
            println!("  current  : {v} dpi");
        }
    }

    if let Ok(r) = rate::ReportRate::new(dev) {
        if let (Ok(cur), Ok(all)) = (r.get(dev), r.supported(dev)) {
            println!("\nreport rate: {cur} Hz (supported: {all:?})");
        }
    }

    if let Ok(l) = led::Leds::new(dev) {
        println!("\nlighting: {} zones, ext_caps 0x{:04x}", l.zone_count, l.ext_caps);
        if !l.can_read_back() {
            println!("  (getZoneEffect unimplemented: a zero readback means nothing)");
        }
        for z in &l.zones {
            let modes: Vec<&str> = z.modes.iter().map(|m| led::mode_name(*m)).collect();
            println!("  zone {} {:<14} modes: {}", z.index, z.location_name(), modes.join(", "));
        }
    }

    if let Ok(p) = profiles::OnboardProfiles::new(dev) {
        let i = p.info;
        println!("\nonboard profiles:");
        println!("  {} profiles (+{} rom), {} buttons", i.profile_count, i.profile_count_oob, i.button_count);
        println!("  {} sectors x {} bytes, profile format {}", i.sector_count, i.sector_size, i.profile_format_id);
        if let Ok(m) = p.mode(dev) {
            println!("  mode     : {m} ({})", profiles::mode_name(m));
        }
        if let Ok(a) = p.current(dev) {
            println!("  active   : profile {a}");
        }
    }
    Ok(())
}

fn cmd_dpi(arg: Option<&String>) -> Result<(), CliError> {
    let mut f = open()?;
    let dev = &mut f.device;
    let d = dpi::Dpi::new(dev)?;
    match arg {
        None => {
            println!("{}", d.get(dev, 0)?);
            Ok(())
        }
        Some(v) => {
            let want: u16 = v.parse().map_err(|_| CliError::Usage(format!("bad dpi {v:?}")))?;
            let got = d.set(dev, want, 0)?;
            if got != want {
                println!("{got} (snapped from {want})");
            } else {
                println!("{got}");
            }
            Ok(())
        }
    }
}

fn cmd_rate(args: &[String]) -> Result<(), CliError> {
    let mut f = open()?;
    let dev = &mut f.device;
    let r = rate::ReportRate::new(dev)?;
    let live = args.iter().any(|a| a == "--live");
    let value = args.iter().find(|a| !a.starts_with("--"));
    match value {
        None => {
            println!("{}", r.get(dev)?);
            Ok(())
        }
        Some(v) => {
            let hz: u16 = v.parse().map_err(|_| CliError::Usage(format!("bad rate {v:?}")))?;
            let ok = r.supported(dev)?;
            if !ok.contains(&hz) {
                return Err(CliError::Usage(format!("{hz} Hz unsupported; have {ok:?}")));
            }
            if live {
                // 0x8060 is rejected in onboard mode, so take host mode first.
                let p = profiles::OnboardProfiles::new(dev)?;
                let previous = p.mode(dev).unwrap_or(profiles::MODE_ONBOARD);
                p.set_mode(dev, profiles::MODE_HOST)?;
                if let Err(e) = r.set(dev, hz) {
                    // The rate did not change, so host mode bought nothing;
                    // leaving the mouse in it would stop its stored profile
                    // applying for no reason the user could see.
                    if previous != profiles::MODE_HOST {
                        if let Err(e2) = p.set_mode(dev, previous) {
                            eprintln!("logi: the mouse is still in host mode: {e2}");
                            eprintln!("      run `logi mode onboard` to hand it back.");
                        }
                    }
                    return Err(e.into());
                }
                println!("{hz} (live; host mode, lost when the mouse returns to onboard)");
            } else {
                let p = profiles::OnboardProfiles::new(dev)?;
                let idx = p.current(dev)?;
                profile_io::set_report_rate(dev, &p, idx, hz)?;
                println!("{hz} (written to onboard profile {idx})");
            }
            Ok(())
        }
    }
}

fn cmd_mode(arg: Option<&String>) -> Result<(), CliError> {
    let mut f = open()?;
    let dev = &mut f.device;
    let p = profiles::OnboardProfiles::new(dev)?;
    match arg.map(|s| s.as_str()) {
        None => {
            let m = p.mode(dev)?;
            println!("{m} ({})", profiles::mode_name(m));
            Ok(())
        }
        Some("onboard") => {
            p.set_mode(dev, profiles::MODE_ONBOARD)?;
            println!("onboard: the mouse drives itself from its stored profile");
            Ok(())
        }
        Some("host") => {
            p.set_mode(dev, profiles::MODE_HOST)?;
            println!("host: live lighting works; the stored profile is not applied");
            Ok(())
        }
        Some(o) => Err(CliError::Usage(format!("mode must be onboard or host, got {o:?}"))),
    }
}

fn cmd_button(args: &[String]) -> Result<(), CliError> {
    let mut f = open()?;
    let dev = &mut f.device;
    let p = profiles::OnboardProfiles::new(dev)?;
    let idx = p.current(dev)?;
    if args.is_empty() {
        let blob = p.read_sector(dev, idx as u16 + 1, None)?;
        let prof = profile_blob::Profile::parse(&blob).map_err(CliError::Failed)?;
        println!("profile {idx}:");
        for (i, b) in prof.buttons.iter().take(p.info.button_count as usize).enumerate() {
            println!("  {i:2}  {}", b.describe());
        }
        println!("\nbind with: logi button <n> <action>");
        println!("actions: button <1-16> | {} | disabled",
                 profile_blob::SPECIALS.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(" | "));
        return Ok(());
    }
    let n: u8 = args[0].parse().map_err(|_| CliError::Usage("button needs an index".into()))?;
    let action = args.get(1).ok_or_else(|| CliError::Usage("button needs an action".into()))?;
    let binding = match action.as_str() {
        "disabled" | "none" => profile_blob::Button::disabled(),
        "button" => {
            let m: u8 = args
                .get(2)
                .ok_or_else(|| CliError::Usage("button <n> button <1-16>".into()))?
                .parse()
                .map_err(|_| CliError::Usage("bad mouse button number".into()))?;
            if !(1..=16).contains(&m) {
                return Err(CliError::Usage("mouse button must be 1-16".into()));
            }
            profile_blob::Button::mouse(m)
        }
        special => profile_blob::Button::special(special).ok_or_else(|| {
            CliError::Usage(format!(
                "unknown action {special:?}; have: {} | button <n> | disabled",
                profile_blob::SPECIALS.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(" | ")
            ))
        })?,
    };
    profile_io::set_button(dev, &p, idx, n, binding)?;
    println!("button {n} -> {} (profile {idx}, written to flash)", binding.describe());
    Ok(())
}

fn cmd_profile(args: &[String]) -> Result<(), CliError> {
    let mut f = open()?;
    let dev = &mut f.device;
    let p = profiles::OnboardProfiles::new(dev)?;
    match args.first().map(|s| s.as_str()) {
        None => {
            println!("{}", p.current(dev)?);
            Ok(())
        }
        Some("show") => {
            let n: u8 = args
                .get(1)
                .ok_or_else(|| CliError::Usage("profile show needs an index".into()))?
                .parse()
                .map_err(|_| CliError::Usage("bad profile index".into()))?;
            if n >= p.info.profile_count {
                // saturating: profile_count comes straight off the device, and
                // a reply of 0 would underflow the subtraction.
                return Err(CliError::Usage(format!(
                    "profile {n} out of range 0..{}",
                    p.info.profile_count.saturating_sub(1)
                )));
            }
            let blob = p.read_sector(dev, n as u16 + 1, None)?;
            let prof = profile_blob::Profile::parse(&blob)
                .map_err(CliError::Failed)?;
            println!("{}", prof.describe(p.info.button_count as usize));
            if !prof.crc_ok() {
                println!("\nwarning: stored CRC does not match the sector contents");
            }
            Ok(())
        }
        Some(v) => {
            let n: u8 = v.parse().map_err(|_| CliError::Usage(format!("bad profile {v:?}")))?;
            p.set_current(dev, n)?;
            println!("{n}");
            Ok(())
        }
    }
}

fn cmd_led(args: &[String]) -> Result<(), CliError> {
    if args.is_empty() {
        return Err(CliError::Usage("led needs a spec: off | <colour> | breathing <colour> | cycle".into()));
    }
    // Decided before the device is touched. Parsing inside the apply step put
    // the mouse into host mode first, so `logi led notacolour` left its stored
    // profile unapplied and said nothing about it.
    enum Spec {
        Off,
        Cycle,
        Breathing([u8; 3]),
        Fixed([u8; 3]),
    }
    let spec = match args[0].as_str() {
        "off" => Spec::Off,
        "cycle" => Spec::Cycle,
        "breathing" => {
            Spec::Breathing(led::parse_color(args.get(1).map(|s| s.as_str()).unwrap_or("cyan"))?)
        }
        other => Spec::Fixed(led::parse_color(other)?),
    };

    let mut f = open()?;
    let dev = &mut f.device;
    let l = led::Leds::new(dev)?;
    let p = profiles::OnboardProfiles::new(dev)?;
    // Live lighting is inert in onboard mode, so take host mode first.
    let previous = p.mode(dev).unwrap_or(profiles::MODE_ONBOARD);
    p.set_mode(dev, profiles::MODE_HOST)?;

    let r = (|| -> hidpp::Result<()> {
        match &spec {
            Spec::Off => l.all_off(dev),
            // Returns a message explaining the two working routes.
            Spec::Cycle => l.cycle(dev, 0, 8000, 100),
            Spec::Breathing(c) => {
                for z in 0..l.zone_count {
                    l.breathing(dev, z, *c, 4000, 0, 100)?;
                }
                Ok(())
            }
            Spec::Fixed(c) => l.all_fixed(dev, *c),
        }
    })();
    if let Err(e) = r {
        // Nothing was applied, so host mode bought nothing, and leaving the
        // mouse in it would mean its stored profile stays unapplied until the
        // user works out that `logi mode onboard` is the way back. `cycle`
        // always lands here: this firmware refuses to set it live.
        if previous != profiles::MODE_HOST {
            if let Err(e2) = p.set_mode(dev, previous) {
                eprintln!("logi: the mouse is still in host mode: {e2}");
                eprintln!("      run `logi mode onboard` to hand it back.");
            }
        }
        return Err(e.into());
    }
    println!("applied (live, host mode).");
    println!("This is lost on unplug, and the mouse stays in host mode until you run");
    println!("`logi mode onboard`. To make lighting persist instead, store it into the");
    println!("onboard profile from the GUI's Lighting tab.");
    Ok(())
}

fn cmd_effect(args: &[String]) -> Result<(), CliError> {
    match args.first().map(|s| s.as_str()) {
        None | Some("list") => {
            println!("host-driven effects (streamed while the command runs):");
            for (n, d) in effects::EFFECTS {
                println!("  {n:<10} {d}");
            }
            Ok(())
        }
        Some(name) => {
            if !effects::is_effect(name) {
                return Err(CliError::Usage(format!("unknown effect {name:?}")));
            }
            let color = args
                .get(1)
                .map(|s| led::parse_color(s))
                .transpose()?
                .unwrap_or([0, 255, 255]);
            // Installed before the effect starts, not after: a signal in the
            // gap would kill the process with the device already in host mode,
            // and dropping `running` is the only thing that puts it back.
            //
            // Ctrl-C must unwind rather than kill the process: dropping
            // `running` is what puts the device back into onboard mode, and a
            // bare SIGINT would leave its lighting owned by a dead process.
            install_interrupt_handler();
            let running = effects::start(name.to_string(), color, 4000)
                .map_err(CliError::Failed)?;
            println!("running {name}; press Ctrl-C to stop and hand the mouse back");
            // `leaving()`, not just INTERRUPTED: the handler above replaces
            // SIGHUP's default of killing us, so a loop that ignored a hangup
            // would survive its terminal closing and hold the mouse in host
            // mode for as long as the session lasted.
            while !leaving() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            running.stop();
            println!("\nstopped; mouse returned to onboard mode");
            Ok(())
        }
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_sig: libc::c_int) {
    // Only an atomic store, which is async-signal-safe.
    INTERRUPTED.store(true, Ordering::Relaxed);
}

extern "C" fn on_hangup(_sig: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

fn install_interrupt_handler() {
    // SAFETY: installing handlers that do nothing but one relaxed atomic
    // store, for three signals whose default action we are deliberately
    // replacing.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_interrupt as *const () as libc::sighandler_t);
        // SIGHUP's default action is to kill us outright, and dying here skips
        // the teardown that un-silences buttons. Two ordinary things send it:
        // the installed unit's own `ExecReload=/bin/kill -HUP $MAINPID`, so
        // `systemctl --user reload logi` would otherwise leave every
        // replace-bound button dead in the device's remapping table, and a
        // terminal closing on a foreground `logi bind run`.
        libc::signal(libc::SIGHUP, on_hangup as *const () as libc::sighandler_t);
    }
}

/// Asked to stop: Ctrl-C or SIGTERM.
fn stopping() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Asked to re-read configuration: SIGHUP.
fn reloading() -> bool {
    RELOAD.load(Ordering::Relaxed)
}

/// Any reason to leave a run loop. A hangup ends a foreground command and
/// restarts the service's inner loop; either way the loop has to exit first so
/// its teardown runs.
fn leaving() -> bool {
    stopping() || reloading()
}

/// Sleep in short steps so a stop or a reload is acted on promptly rather than
/// a whole interval later. The step is well under a second because `poll_ms`
/// has no upper bound: a rule file asking for a minute between scans would
/// otherwise make Ctrl-C look like a hang.
fn nap_for(total: std::time::Duration) {
    const STEP: std::time::Duration = std::time::Duration::from_millis(250);
    let deadline = std::time::Instant::now() + total;
    while !leaving() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(STEP));
    }
}

fn nap(secs: u64) {
    nap_for(std::time::Duration::from_secs(secs));
}

/// A Keymap over uinput's tables, which is what actions.rs expects.
fn keymap() -> actions::FnKeymap<fn(&str) -> Option<u16>, fn(u16) -> Option<String>> {
    actions::FnKeymap::new(
        uinput::key_code as fn(&str) -> Option<u16>,
        (|c| uinput::key_name(c).map(|s| s.to_string())) as fn(u16) -> Option<String>,
    )
}

/// Carry out one bound action on the virtual input device.
fn perform(action: &actions::Action, vd: &mut uinput::VirtualDevice) -> std::io::Result<()> {
    match action {
        actions::Action::Key { mods, code } => vd.chord(mods, *code),
        actions::Action::Text(t) => vd.type_text(t),
        actions::Action::MouseButton(c) => vd.tap(*c),
        // actions owns how a `command` binding runs. Going through it rather
        // than splitting the string here is what gives the quoting, pipes and
        // $VAR the bindings file documents, puts the child in its own process
        // group, and reaps it on a thread so fifty presses do not leave fifty
        // zombies behind this process.
        actions::Action::Command(cmd) => actions::spawn_detached(cmd),
        actions::Action::Nothing => Ok(()),
    }
}

fn cmd_bind(args: &[String]) -> Result<(), CliError> {
    let path = actions::config_path();
    match args.first().map(|s| s.as_str()) {
        Some("reset") => {
            // The escape hatch. Deliberately does not depend on a saved
            // snapshot: a device whose table is already zeroed has nothing
            // useful to restore, only the factory identity.
            let mut f = open()?;
            let spy = buttonspy::ButtonSpy::new(&mut f.device)?;
            spy.restore_identity(&mut f.device)?;
            println!("Factory button mapping restored on {}.", f.name);
            println!("Every physical button does its normal job again.");
            Ok(())
        }
        Some("run") => cmd_bind_run(),
        None | Some("list") => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.trim().is_empty() {
                if let Some(d) = path.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(&path, actions::EXAMPLE);
                println!("Wrote an example binding file to {}.", path.display());
                println!("Edit it, or use the Buttons tab in the app, then `logi bind run`.");
                return Ok(());
            }
            let km = keymap();
            let (b, problems) = actions::Bindings::parse(&text, &km);
            for p in &problems {
                eprintln!("logi: {}: {p}", path.display());
            }
            println!("{}", path.display());
            for bind in &b.list {
                let mode = match bind.mode {
                    actions::BindMode::Augment => "augment",
                    actions::BindMode::Replace => "replace",
                };
                println!("  button {:<3} {:<9} {}", bind.button, mode, bind.action.describe(&km));
            }
            if b.list.is_empty() {
                println!("  (no bindings)");
            }
            Ok(())
        }
        Some(o) => Err(CliError::Usage(format!("unknown bind command {o:?}"))),
    }
}

/// Run the spy: silence what Replace asks for, listen, act, and always put the
/// device back.
fn cmd_bind_run() -> Result<(), CliError> {
    let path = actions::config_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CliError::Failed(format!("reading {}: {e}", path.display())))?;
    let km = keymap();
    let (bindings, problems) = actions::Bindings::parse(&text, &km);
    for p in &problems {
        eprintln!("logi: {}: {p}", path.display());
    }
    if bindings.list.is_empty() {
        return Err(CliError::Failed(format!("{} has no bindings", path.display())));
    }

    let mut f = open()?;
    if !f.caps.button_spy {
        return Err(CliError::Failed(format!(
            "{} does not report the button-spy feature (0x8110)",
            f.name
        )));
    }
    let spy = buttonspy::ButtonSpy::new(&mut f.device)?;
    let mut vd = uinput::VirtualDevice::new("logi virtual input").map_err(|e| {
        CliError::Failed(format!(
            "{e}\n\
             \n\
             Binding buttons needs /dev/uinput. Install the rules with:\n\
             \x20   packaging/install.sh\n\
             then re-plug the mouse."
        ))
    })?;

    // Anything that silences a button must be undone even on an abnormal exit,
    // because a silenced button that outlives this process is a mouse the user
    // cannot fix by clicking.
    install_interrupt_handler();
    let silence = bindings.to_silence();
    let mut silenced = false;

    let outcome = (|| -> Result<(), CliError> {
        spy.start(&mut f.device)?;
        let mut listener = f.device.open_listener()?;
        // Silencing comes last, once the host side is known to work: a
        // silenced button with nothing listening is a dead button, and it
        // stays dead until something puts the table back. `silenced` is set
        // before the write, not after, because a reply lost to the timeout can
        // hide a write that landed — the teardown below has to run either way.
        if !silence.is_empty() {
            silenced = true;
            spy.silence(&mut f.device, &silence)?;
        }
        let idx = spy.feature_index();
        println!("logi: acting on {} binding(s); Ctrl-C to stop", bindings.list.len());
        let mut prev: u16 = 0;
        while !leaving() {
            let Some(report) =
                listener.read_event(std::time::Duration::from_millis(250))?
            else {
                continue;
            };
            let Some(now) = buttonspy::decode_event(&report, idx) else { continue };
            let (pressed, _released) = buttonspy::transitions(prev, now);
            prev = now;
            for b in pressed {
                if let Some(bind) = bindings.get(b) {
                    if let Err(e) = perform(&bind.action, &mut vd) {
                        eprintln!("logi: button {b}: {e}");
                    }
                }
            }
        }
        Ok(())
    })();

    // Unconditional teardown. Report failures rather than swallowing them:
    // a restore that did not happen is exactly what the user needs to know.
    if let Err(e) = spy.stop(&mut f.device) {
        eprintln!("logi: could not stop the spy: {e}");
    }
    if silenced {
        match spy.restore(&mut f.device) {
            Ok(()) => println!("logi: button mapping restored"),
            Err(e) => eprintln!(
                "logi: COULD NOT RESTORE the button mapping: {e}\n\
                 Run `logi bind reset` to put it back."
            ),
        }
    }
    outcome
}

/// The background service.
///
/// Deliberately tolerant: with no rule file, or no device attached, it idles
/// rather than exiting. A service that quits because its config is missing
/// just flaps under systemd's restart policy and buries the real reason.
fn cmd_serve() -> Result<(), CliError> {
    install_interrupt_handler();
    eprintln!("logi: service starting");

    let path = appwatch::config_path();
    // Idling is the steady state with no rules, and repeating the same
    // sentence into the journal every minute forever buries everything else.
    let mut announced_idle = false;
    // Same reason: with rules in place and no mouse attached, the retry loop
    // would otherwise put the identical line in the journal every ten seconds
    // for as long as the machine stayed up.
    let mut announced_problem: Option<String> = None;
    while !stopping() {
        // A hangup is the unit's ExecReload. serve_once has already torn down
        // by the time we get here, so re-reading the file is all it takes.
        if RELOAD.swap(false, Ordering::Relaxed) {
            eprintln!("logi: reloading {}", path.display());
            announced_idle = false;
            announced_problem = None;
        }
        let rules = std::fs::read_to_string(&path).ok();
        match rules {
            None => {
                if !announced_idle {
                    eprintln!(
                        "logi: no rules at {}; idling. Run `logi watch` once to write an example.",
                        path.display()
                    );
                    announced_idle = true;
                }
                // Nothing to do, so wait rather than spin or exit.
                nap(60);
            }
            Some(text) => {
                let (cfg, problems) = appwatch::parse_rules(&text);
                for p in &problems {
                    eprintln!("logi: {}: {p}", path.display());
                }
                if cfg.rules.is_empty() && cfg.fallback.is_none() {
                    if !announced_idle {
                        eprintln!("logi: {} has no rules yet; idling", path.display());
                        announced_idle = true;
                    }
                    nap(30);
                    continue;
                }
                announced_idle = false;
                match serve_once(cfg) {
                    // A missing or unplugged device is normal, not fatal.
                    Err(e) => {
                        let msg = e.to_string();
                        if announced_problem.as_deref() != Some(msg.as_str()) {
                            eprintln!("logi: {msg}; retrying every 10s until it clears");
                            announced_problem = Some(msg);
                        }
                        nap(10);
                    }
                    Ok(()) => announced_problem = None,
                }
            }
        }
    }
    eprintln!("logi: service stopping");
    Ok(())
}

/// One connected run of the service: per-application profile switching and
/// button bindings together.
///
/// Both at once, because they are what the service is for and neither is much
/// use alone. They share the device: the watcher issues commands on the
/// control handle while the spy reads events on its own, which works because
/// hidraw queues input reports per open file description.
///
/// Returns when the device goes away or the service is asked to stop, so the
/// caller can retry.
fn serve_once(cfg: appwatch::Config) -> Result<(), hidpp::Error> {
    // The same $G502_DEVICE every other command honours: with two Logitech
    // devices attached, the service must drive the one the user named.
    let want = wanted_device();
    let mut f = discover::open_device_matching(want.as_deref())?;
    let name = f.name.clone();
    let has_spy = f.caps.button_spy;
    let dev = &mut f.device;
    let profiles = profiles::OnboardProfiles::new(dev)?;
    let dpi_feat = dpi::Dpi::new(dev).ok();

    let have = profiles.info.profile_count;
    for p in appwatch::referenced_profiles(&cfg) {
        if p >= have {
            eprintln!(
                "logi: rules name profile {p}, device has 0..{}",
                have.saturating_sub(1)
            );
        }
    }

    // Button bindings are optional: no file, no bindings, or a device without
    // the feature all just mean the watcher runs alone.
    let km = keymap();
    let bindings = std::fs::read_to_string(actions::config_path())
        .ok()
        .map(|t| {
            let (b, problems) = actions::Bindings::parse(&t, &km);
            for p in &problems {
                eprintln!("logi: bindings: {p}");
            }
            b
        })
        .filter(|b| !b.list.is_empty());

    let mut spy_state = None;
    if let Some(b) = &bindings {
        if !has_spy {
            eprintln!("logi: {name} has no button-spy feature; bindings ignored");
        } else {
            match start_spy(dev, b) {
                Ok(s) => spy_state = Some(s),
                Err(e) => eprintln!("logi: button bindings unavailable: {e}"),
            }
        }
    }

    eprintln!(
        "logi: watching {name}: {} rule(s), {} binding(s)",
        cfg.rules.len(),
        bindings.as_ref().map(|b| b.list.len()).unwrap_or(0)
    );

    let poll = cfg.poll;
    let mut watcher = appwatch::Watcher::new(cfg);
    let mut last_scan = std::time::Instant::now() - poll;
    let mut prev_mask: u16 = 0;

    let outcome = (|| -> Result<(), hidpp::Error> {
        // `leaving()`, not just INTERRUPTED: the unit's ExecReload is a SIGHUP,
        // and cmd_serve re-reads its configuration only once this returns. A
        // loop that watched INTERRUPTED alone made `systemctl --user reload
        // logi` a silent no-op for as long as the device stayed attached.
        while !leaving() {
            // The spy read doubles as the loop's tick, so with bindings active
            // we block on events rather than sleeping, and button presses are
            // acted on immediately instead of at the next poll.
            if let Some(s) = &mut spy_state {
                if let Some(report) =
                    s.listener.read_event(std::time::Duration::from_millis(200))?
                {
                    if let Some(now) = buttonspy::decode_event(&report, s.spy.feature_index()) {
                        let (pressed, _) = buttonspy::transitions(prev_mask, now);
                        prev_mask = now;
                        for b in pressed {
                            if let Some(bind) = s.bindings.get(b) {
                                if let Err(e) = perform(&bind.action, &mut s.vd) {
                                    eprintln!("logi: button {b}: {e}");
                                }
                            }
                        }
                    }
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }

            if last_scan.elapsed() >= poll {
                last_scan = std::time::Instant::now();
                let procs = appwatch::scan_processes();
                let decision = watcher.tick(&procs, std::time::Instant::now());
                if let appwatch::Decision::Switch { profile, dpi: want, .. } = &decision {
                    if let Some(msg) = appwatch::describe(&decision) {
                        eprintln!("logi: applying {msg}");
                    }
                    profiles.set_current(dev, *profile)?;
                    if let (Some(d), Some(v)) = (dpi_feat.as_ref(), want) {
                        if let Err(e) = d.set(dev, *v, 0) {
                            eprintln!("logi: setting {v} dpi: {e}");
                        }
                    }
                }
            }
        }
        Ok(())
    })();

    // Unconditional teardown. A silenced button outliving this process is the
    // one way the feature can leave someone unable to click, so a failure here
    // is reported loudly rather than swallowed.
    if let Some(s) = spy_state {
        if let Err(e) = s.spy.stop(dev) {
            eprintln!("logi: could not stop the spy: {e}");
        }
        if s.silenced {
            match s.spy.restore(dev) {
                Ok(()) => eprintln!("logi: button mapping restored"),
                Err(e) => eprintln!(
                    "logi: COULD NOT RESTORE the button mapping: {e}. \
                     Run `logi bind reset`."
                ),
            }
        }
    }
    outcome
}

/// Everything a running spy owns, so teardown can be one place.
struct SpyState {
    spy: buttonspy::ButtonSpy,
    listener: hidpp::Device,
    vd: uinput::VirtualDevice,
    bindings: actions::Bindings,
    silenced: bool,
}

/// Bring the spy up, or leave the device exactly as it was found.
///
/// Order matters and is the whole point of this function. Silencing is done
/// last, after the spy is running and the listener is open, because a button
/// silenced while nothing is listening is a button that does nothing at all,
/// in both memory modes, until something writes the table back. And anything
/// that fails after the first silencing write unwinds it here: the caller
/// treats a failure as "no bindings today" and carries on watching, so a
/// silenced table escaping this function would leave the service running for
/// hours against a mouse that cannot click.
fn start_spy(
    dev: &mut hidpp::Device,
    bindings: &actions::Bindings,
) -> Result<SpyState, hidpp::Error> {
    let spy = buttonspy::ButtonSpy::new(dev)?;
    let vd = uinput::VirtualDevice::new("logi virtual input").map_err(|e| {
        hidpp::Error::Protocol(format!(
            "{e} (install the udev rules with packaging/install.sh)"
        ))
    })?;
    spy.start(dev)?;

    // Set before the write, not after: a reply lost to the timeout can hide a
    // write that landed, so an attempt is enough to owe a restore.
    let mut silenced = false;
    let brought_up = (|| -> Result<hidpp::Device, hidpp::Error> {
        let listener = dev.open_listener()?;
        let silence = bindings.to_silence();
        if !silence.is_empty() {
            silenced = true;
            spy.silence(dev, &silence)?;
        }
        Ok(listener)
    })();

    match brought_up {
        Ok(listener) => Ok(SpyState { spy, listener, vd, bindings: bindings.clone(), silenced }),
        Err(e) => {
            if silenced {
                if let Err(e2) = spy.restore(dev) {
                    eprintln!(
                        "logi: COULD NOT RESTORE the button mapping: {e2}. \
                         Run `logi bind reset`."
                    );
                }
            }
            if let Err(e2) = spy.stop(dev) {
                eprintln!("logi: could not stop the spy: {e2}");
            }
            Err(e)
        }
    }
}

fn cmd_watch(args: &[String]) -> Result<(), CliError> {
    let dry = args.iter().any(|a| a == "--dry-run");
    let path = appwatch::config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| CliError::Failed(format!("creating {}: {e}", dir.display())))?;
            }
            std::fs::write(&path, appwatch::EXAMPLE_RULES)
                .map_err(|e| CliError::Failed(format!("writing {}: {e}", path.display())))?;
            println!("Wrote an example rule file to {}.", path.display());
            println!("Edit it, then run `logi watch` again. Nothing to do until it has rules.");
            return Ok(());
        }
        Err(e) => return Err(CliError::Failed(format!("reading {}: {e}", path.display()))),
    };
    let (cfg, problems) = appwatch::parse_rules(&text);
    for p in &problems {
        eprintln!("logi: {}: {p}", path.display());
    }
    if cfg.rules.is_empty() && cfg.fallback.is_none() {
        return Err(CliError::Failed(format!(
            "{} has no rules yet; edit it and try again",
            path.display()
        )));
    }

    let mut f = open()?;
    let model = f.name.clone();
    let dev = &mut f.device;
    let profiles = profiles::OnboardProfiles::new(dev)?;
    let dpi = dpi::Dpi::new(dev).ok();

    // A rule naming a profile the device does not have would fail silently
    // every time it matched, so say so once at startup instead.
    let have = profiles.info.profile_count;
    for p in appwatch::referenced_profiles(&cfg) {
        if p >= have {
            eprintln!(
                "logi: warning: rules reference profile {p}, device has 0..{}",
                have.saturating_sub(1)
            );
        }
    }

    println!("watching {model}: {} rule(s), poll {:?}{}",
             cfg.rules.len(), cfg.poll, if dry { ", dry run" } else { "" });
    for r in &cfg.rules {
        println!("  {:<24} -> profile {}{}", r.pattern, r.profile,
                 r.dpi.map(|d| format!(" + {d} dpi")).unwrap_or_default());
    }
    println!("Ctrl-C to stop.");

    install_interrupt_handler();
    let poll = cfg.poll;
    let mut watcher = appwatch::Watcher::new(cfg);
    // `leaving()`, not just INTERRUPTED: the handler above replaces SIGHUP's
    // default of killing us, so ignoring a hangup here would leave this
    // command running after the terminal that started it had gone.
    while !leaving() {
        let procs = appwatch::scan_processes();
        let decision = watcher.tick(&procs, std::time::Instant::now());
        if let appwatch::Decision::Switch { profile, dpi: want, .. } = &decision {
            if let Some(msg) = appwatch::describe(&decision) {
                println!("{}{}", if dry { "would apply " } else { "applying " }, msg);
            }
            if !dry {
                if let Err(e) = profiles.set_current(dev, *profile) {
                    eprintln!("logi: switching to profile {profile}: {e}");
                    // Let the next match try again rather than believing it stuck.
                    watcher.invalidate();
                }
                if let (Some(d), Some(v)) = (dpi.as_ref(), want) {
                    if let Err(e) = d.set(dev, *v, 0) {
                        eprintln!("logi: setting {v} dpi: {e}");
                    }
                }
            }
        }
        nap_for(poll);
    }
    println!("\nstopped");
    Ok(())
}

fn cmd_backup(path: &str) -> Result<(), CliError> {
    let mut f = open()?;
    let model = f.name.clone();
    let dev = &mut f.device;
    let p = profiles::OnboardProfiles::new(dev)?;
    let dump = profile_io::dump_all(dev, &p);
    let readable = dump.iter().filter(|(_, d)| d.is_some()).count();
    // A dump where every sector is absent is not a backup. `restore` rejects
    // one ("backup contains no sectors"), so writing it can only destroy the
    // good backup that was already at this path.
    if readable == 0 {
        return Err(CliError::Failed(format!(
            "no sector could be read from {model}, so {path} was left alone"
        )));
    }
    // Written beside the target and renamed into place. fs::write truncates
    // first, so a write that fails or is interrupted halfway would leave the
    // user with neither the old backup nor a usable new one.
    let tmp = format!("{path}.part");
    let write = std::fs::write(&tmp, profile_io::dump_to_json(&model, &dump))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::Failed(format!("writing {path}: {e}")));
    }
    if readable < dump.len() {
        eprintln!(
            "logi: {} sector(s) would not read and are recorded as absent; restoring \
             this file will leave them as they are",
            dump.len() - readable
        );
    }
    println!("{readable}/{} sectors written to {path}", dump.len());
    Ok(())
}

fn cmd_restore(path: &str) -> Result<(), CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::Failed(format!("reading {path}: {e}")))?;
    let backup = profile_io::dump_from_json(&text)?;
    let mut f = open()?;
    let dev = &mut f.device;
    let p = profiles::OnboardProfiles::new(dev)?;
    let n = profile_io::restore_all(dev, &p, &backup)?;
    println!("{n} sector(s) written; the rest already matched");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn nap_for_honours_sub_second_durations() {
        // The reason nap_for exists alongside nap: `logi watch` sleeps a poll
        // interval whose floor is 100ms, and a whole-second step would both
        // overshoot the interval and make Ctrl-C look like a hang.
        let t = Instant::now();
        super::nap_for(Duration::from_millis(120));
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(100), "returned early, after {took:?}");
        assert!(took < Duration::from_millis(900), "overslept {took:?}");
    }

    #[test]
    fn nap_for_zero_returns_without_sleeping() {
        let t = Instant::now();
        super::nap_for(Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
