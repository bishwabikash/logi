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
  logi effect [list|<name> [<colour>]]
                              stream a host-side animation until Ctrl-C
  logi watch [--dry-run]      switch profiles per running application, from
                              ~/.config/logi/rules.conf
  logi bind [list]            host actions bound to buttons, from
                              ~/.config/logi/bindings.conf
  logi bind run               act on those bindings until Ctrl-C
  logi bind reset             put the factory button mapping back. The escape
                              hatch if a button has stopped responding.
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

/// Refuse any `--flag` a command does not know.
///
/// Silently skipping one is not harmless here: for both commands that take a
/// flag, the flag is what holds back the consequential behaviour. A mistyped
/// `--live` writes a flash sector instead of setting the rate in RAM, and a
/// mistyped `--dry-run` switches the user's profiles for real.
fn reject_unknown_flags(args: &[String], known: &[&str]) -> Result<(), CliError> {
    match args.iter().find(|a| a.starts_with("--") && !known.contains(&a.as_str())) {
        Some(bad) => Err(CliError::Usage(format!(
            "unknown option {bad:?}; this command takes {}",
            known.join(" ")
        ))),
        None => Ok(()),
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
    // Checked before the device is opened, and before anything is written.
    // Every unrecognised flag used to be skipped silently, and the flag that
    // got skipped decided whether the new rate went to RAM or to flash: `logi
    // rate 500 --liv` quietly wrote the profile sector instead of setting the
    // rate live.
    reject_unknown_flags(args, &["--live"])?;
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
                enter_host_mode(&p, dev, previous)?;
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
                // "stored in", not "written to": set_report_rate skips the
                // flash write when the sector already holds this rate, and it
                // does not say which of the two happened. Claiming a write
                // that did not occur is the wrong way round to be wrong about
                // flash.
                profile_io::set_report_rate(dev, &p, idx, hz)?;
                println!("{hz} (stored in onboard profile {idx})");
            }
            Ok(())
        }
    }
}

/// Take host mode for a live write, handing the mouse back if the request
/// itself fails.
///
/// A setter that timed out may still have been applied, and then the mouse is
/// in host mode with nothing about the failure to say so: its stored profile
/// stays unapplied until the user works out `logi mode onboard`. An endpoint
/// that is gone needs no put-back, there being nothing to put it back on.
fn enter_host_mode(
    p: &profiles::OnboardProfiles,
    dev: &mut hidpp::Device,
    previous: u8,
) -> Result<(), CliError> {
    let Err(e) = p.set_mode(dev, profiles::MODE_HOST) else {
        return Ok(());
    };
    if previous != profiles::MODE_HOST && !endpoint_gone(&e) {
        if let Err(e2) = p.set_mode(dev, previous) {
            eprintln!("logi: the mouse may be in host mode: {e2}");
            eprintln!("      run `logi mode onboard` to hand it back.");
        }
    }
    Err(e.into())
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
    // Not "written to flash": set_button leaves the sector alone when it
    // already holds this binding, and it does not report which way it went.
    // The command does write flash, which is what USAGE says; this line is
    // about one invocation, and it must not claim a write that did not happen.
    profile_io::set_button(dev, &p, idx, n, binding)?;
    println!("button {n} -> {} (onboard profile {idx})", binding.describe());
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
    // `cycle` is refused by this firmware whatever is sent, so it is answered
    // before the mode is touched: taking host mode for it and handing it back
    // is two setters, either of which can time out and land anyway, for a
    // request that was never going to apply.
    if let Spec::Cycle = spec {
        l.cycle(dev, 0, 8000, 100)?;
        println!("applied (live).");
        return Ok(());
    }
    let p = profiles::OnboardProfiles::new(dev)?;
    // Live lighting is inert in onboard mode, so take host mode first.
    let previous = p.mode(dev).unwrap_or(profiles::MODE_ONBOARD);
    enter_host_mode(&p, dev, previous)?;

    let r = (|| -> hidpp::Result<()> {
        match &spec {
            Spec::Off => l.all_off(dev),
            // Answered above, before host mode was taken.
            Spec::Cycle => Ok(()),
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
        // user works out that `logi mode onboard` is the way back.
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
            //
            // `is_finished()` too: the effect thread ends on its own when its
            // writes keep failing, which is what an unplug looks like. A loop
            // that waited only for Ctrl-C went on printing "running" over a
            // mouse that was no longer being driven, then reported the
            // hand-back as done and exited 0.
            while !leaving() && !running.is_finished() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let ended_alone = running.is_finished() && !leaving();
            running.stop();
            if ended_alone {
                return Err(CliError::Failed(
                    "the effect stopped by itself (the message above says why); the mouse \
                     was handed back if it could be.\n\
                     `logi mode` says which mode it is in."
                        .into(),
                ));
            }
            // Not "returned to onboard mode". effects restores whichever mode
            // it found, so after `logi led <colour>` — which leaves the mouse
            // in host mode on purpose — this hands it back to host, and saying
            // onboard would tell the user their stored profile is applied when
            // it is not.
            println!("\nstopped; the mouse is back in the mode it was in before");
            println!("`logi mode` says which, and `logi mode onboard` applies its stored profile");
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

/// The hidraw node whose button table this process has silenced and not yet
/// restored.
static OWED_RESTORE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Record that the table at `path` is about to be written with buttons
/// silenced. Called before the write, not after: a reply lost to the timeout
/// can hide a write that landed.
fn owe_restore(path: &str) {
    *OWED_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_string());
}

fn restore_done() {
    *OWED_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Put the table back from a panic hook.
///
/// Release builds abort on panic, so none of the teardown that follows a loop
/// runs, and a silenced button outlives the process in non-volatile memory:
/// the mouse that moves but cannot click. This opens the node afresh. The
/// snapshot it takes reads the silenced table, but heals every zeroed slot to
/// its own button, which is exactly the repair needed; and if even that read
/// fails the factory identity is the fallback.
fn emergency_restore(path: &str) {
    // Not eprintln!: that panics when stderr is gone (a closed pipe, a
    // terminal that hung up), and a panic inside a panic hook aborts at once,
    // before the restore below has run. The reason for the first panic is very
    // often exactly that failed print.
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "logi: crashing with buttons silenced; putting the button mapping back");
    let r = hidpp::Device::open(path).and_then(|mut d| {
        let spy = buttonspy::ButtonSpy::new(&mut d)?;
        spy.restore(&mut d).or_else(|_| spy.restore_identity(&mut d))
    });
    match r {
        Ok(()) => {
            let _ = writeln!(err, "logi: button mapping restored");
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "logi: COULD NOT RESTORE the button mapping: {e}. Run `logi bind reset`."
            );
        }
    }
}

/// Make a closed pipe an error rather than a death.
///
/// `main` restores SIGPIPE's default so `logi info | head` exits quietly. The
/// commands that silence buttons cannot take that: a default SIGPIPE kills the
/// process on the spot, and neither the teardown nor the panic hook runs for a
/// signal death, so `logi bind run 2>&1 | head -1` would leave every
/// replace-bound button dead in the non-volatile table. Ignored, the failed
/// print becomes a panic instead, and the hook puts the table back first.
fn survive_closed_output() {
    // SAFETY: setting a signal's disposition to ignore; no handler code runs.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Whether a table this process silenced is still not known to be put back.
fn restore_owed() -> bool {
    OWED_RESTORE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Settle a restore that an earlier run could not complete.
///
/// A teardown that fails because the mouse was unplugged leaves the silenced
/// table in the device, and the table is non-volatile. Reconnecting is the first
/// chance to fix it, and it must not depend on this connection having bindings
/// to silence: with the file emptied in the meantime nothing else would ever
/// touch the table again. The live table is compared first, so a silence that
/// never landed costs no flash write.
fn settle_owed_restore(dev: &mut hidpp::Device) -> hidpp::Result<()> {
    let spy = buttonspy::ButtonSpy::new(dev)?;
    let Some(saved) = spy.saved_remapping().map(|t| t.to_vec()) else {
        return Err(hidpp::Error::Protocol(
            "could not read the button remapping table to check it".into(),
        ));
    };
    if spy.get_remapping(dev)? != saved {
        spy.restore(dev)?;
        eprintln!("logi: button mapping restored on the reconnected mouse");
    }
    Ok(())
}

/// Make a panic undo any silencing before the process dies.
fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let owed = OWED_RESTORE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(path) = owed {
            emergency_restore(&path);
        }
    }));
}

/// The endpoint itself is broken, as opposed to the device answering with an
/// error or not answering in time.
///
/// `hidpp` only raises `Io` from the node's own read, write or poll, so it
/// means this file descriptor will never work again. That matters for the
/// foreground commands, which hold one open handle and never reopen it: a loop
/// that treats an unplug like a transient failure keeps polling /proc and
/// re-reporting the same error for as long as the user leaves it running,
/// looking for all the world like it is still watching. `logi serve` is the
/// one that reconnects, and it does so by returning from serve_once.
fn endpoint_gone(e: &hidpp::Error) -> bool {
    matches!(e, hidpp::Error::Io(_))
}

/// Whether a failed profile switch could succeed if tried again.
///
/// A lost reply or a busy device can; `Protocol` is `set_current`'s own range
/// check, which will give the same answer every time. Retrying that one is not
/// harmless in the service: the failure ends `serve_once`, whose teardown and
/// bring-up each write the button remapping table, and that table is
/// non-volatile.
fn switch_retryable(e: &hidpp::Error) -> bool {
    matches!(e, hidpp::Error::Timeout { .. } | hidpp::Error::Device { .. })
}

/// Ask the device something harmless, so an unplug is noticed while nothing
/// else is using the endpoint.
///
/// Without this a loop that only talks to the device when a rule fires holds a
/// dead descriptor indefinitely after an unplug: the rules are never
/// re-applied to the re-plugged mouse until some application happens to start
/// or stop. Only the endpoint failing counts; a slow answer proves nothing.
/// `current` is a getter.
fn check_endpoint(
    p: &profiles::OnboardProfiles,
    dev: &mut hidpp::Device,
) -> hidpp::Result<()> {
    match p.current(dev) {
        Err(e) if endpoint_gone(&e) => Err(e),
        _ => Ok(()),
    }
}

/// How often an otherwise idle loop checks that its device is still there.
const ENDPOINT_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

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
            // Only a file that is genuinely absent or genuinely blank gets the
            // example written over it. `unwrap_or_default()` here conflated
            // every other read failure with "empty": a bindings file with one
            // stray non-UTF-8 byte in a `text` action, or one whose
            // permissions had gone wrong, came back as "" and was then
            // replaced by the example, and the user's bindings were gone.
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(CliError::Failed(format!(
                        "reading {}: {e}\n\
                         Left alone rather than overwritten with the example.",
                        path.display()
                    )))
                }
            };
            if text.trim().is_empty() {
                if let Some(d) = path.parent() {
                    std::fs::create_dir_all(d).map_err(|e| {
                        CliError::Failed(format!("creating {}: {e}", d.display()))
                    })?;
                }
                // Reported, not ignored: claiming the example was written when
                // the write failed sends the user to edit a file that is not
                // there.
                std::fs::write(&path, actions::EXAMPLE).map_err(|e| {
                    CliError::Failed(format!("writing {}: {e}", path.display()))
                })?;
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
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CliError::Failed(format!(
                "no bindings file at {}; `logi bind` writes an example to start from",
                path.display()
            ))
        } else {
            CliError::Failed(format!("reading {}: {e}", path.display()))
        }
    })?;
    let km = keymap();
    let (bindings, problems) = actions::Bindings::parse(&text, &km);
    for p in &problems {
        eprintln!("logi: {}: {p}", path.display());
    }
    if bindings.list.is_empty() {
        return Err(CliError::Failed(format!("{} has no bindings", path.display())));
    }

    // The service acts on this same file. Two listeners means every press is
    // handled twice, and when this command exits its restore writes the whole
    // table, which un-silences the buttons the service had silenced.
    if service::state() == service::State::Active {
        eprintln!(
            "logi: warning: the background service is running and acts on these bindings too.\n\
             \x20     Stop it first, or each press is handled twice:\n\
             \x20         systemctl --user stop logi"
        );
    }

    let mut f = open()?;
    if !f.caps.button_spy {
        return Err(CliError::Failed(format!(
            "{} does not report the button-spy feature (0x8110)",
            f.name
        )));
    }
    let spy = buttonspy::ButtonSpy::new(&mut f.device)?;
    let silence = bindings.to_silence();
    // Before the spy starts, so a binding for a button this mouse lacks costs
    // nothing instead of a table write on the way out.
    silence_fits(&silence, spy.button_count()).map_err(CliError::Failed)?;
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
    install_panic_restore();
    survive_closed_output();
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
            owe_restore(&f.device.path);
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
            Ok(()) => {
                restore_done();
                println!("logi: button mapping restored")
            }
            Err(e) => eprintln!(
                "logi: COULD NOT RESTORE the button mapping: {e}\n\
                 Run `logi bind reset` to put it back."
            ),
        }
    }
    outcome
}

/// The bindings the service should act on: none when there is no file, or it
/// has nothing in it, or it cannot be read.
///
/// An unreadable file is said so rather than treated as absent: the service
/// would otherwise run for hours without the bindings the user can see in the
/// file, with nothing in the journal to explain why.
fn service_bindings() -> Option<actions::Bindings> {
    let path = actions::config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("logi: reading {}: {e}; running without button bindings", path.display());
            return None;
        }
    };
    let km = keymap();
    let (b, problems) = actions::Bindings::parse(&text, &km);
    for p in &problems {
        eprintln!("logi: bindings: {p}");
    }
    Some(b).filter(|b| !b.list.is_empty())
}

/// Whether the file at `path` holds bindings for the service to run, without
/// reporting parse problems a second time.
///
/// Absent is `Ok(false)`; unreadable is an error the caller has to say out
/// loud. Folding the two together made an idle service claim "no bindings" about
/// a file the user could see, which is the defect `service_bindings` was fixed
/// for, reached by the other door.
fn bindings_in(path: &std::path::Path) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let km = keymap();
    Ok(!actions::Bindings::parse(&text, &km).0.list.is_empty())
}

/// True when `now` differs from what was last announced, and remembers it.
///
/// The service re-reads its files on every pass of its retry loop, every ten
/// seconds while no mouse is attached; a bad line in the rules file would put
/// the identical complaint in the journal on each of them.
fn changed_since(announced: &mut Vec<String>, now: &[String]) -> bool {
    if announced.as_slice() == now {
        return false;
    }
    *announced = now.to_vec();
    true
}

/// A silenced button must be one the mouse has. Checked before anything is
/// sent: the table write that would otherwise reject it comes after the spy is
/// started, and the teardown that follows its failure writes the table again,
/// to flash, for a mistake in a text file.
fn silence_fits(silence: &[u8], count: u8) -> Result<(), String> {
    match silence.iter().find(|b| **b == 0 || **b > count) {
        Some(b) => Err(format!(
            "button {b} is bound with `replace`, but this mouse has buttons 1..{count}"
        )),
        None => Ok(()),
    }
}

/// The background service.
///
/// Deliberately tolerant: with no rule file, or no device attached, it idles
/// rather than exiting. A service that quits because its config is missing
/// just flaps under systemd's restart policy and buries the real reason.
fn cmd_serve() -> Result<(), CliError> {
    install_interrupt_handler();
    install_panic_restore();
    survive_closed_output();
    eprintln!("logi: service starting");

    let path = appwatch::config_path();
    let bindings_path = actions::config_path();
    // Idling is the steady state with no rules, and repeating the same
    // sentence into the journal every minute forever buries everything else.
    let mut announced_idle = false;
    // Same reason: with rules in place and no mouse attached, the retry loop
    // would otherwise put the identical line in the journal every ten seconds
    // for as long as the machine stayed up.
    let mut announced_problem: Option<String> = None;
    let mut announced_bindings_problem: Option<String> = None;
    let mut announced_parse: Vec<String> = Vec::new();
    while !stopping() {
        // A hangup is the unit's ExecReload. serve_once has already torn down
        // by the time we get here, so re-reading the file is all it takes.
        if RELOAD.swap(false, Ordering::Relaxed) {
            eprintln!("logi: reloading {}", path.display());
            announced_idle = false;
            announced_problem = None;
            announced_bindings_problem = None;
            announced_parse.clear();
        }
        // A file that is there but unreadable is not the same as no file, and
        // saying "no rules" about it sends the user to write an example over
        // rules they already have. `.ok()` reported a wrong-permissions or
        // non-UTF-8 rule file as absent and then idled on it forever.
        let rules = match std::fs::read_to_string(&path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                let msg = format!("reading {}: {e}", path.display());
                if announced_problem.as_deref() != Some(msg.as_str()) {
                    eprintln!("logi: {msg}; retrying every 60s until it clears");
                    announced_problem = Some(msg);
                }
                nap(60);
                continue;
            }
        };
        // Rules are not the only thing the service is for: button bindings
        // run in it too, and the panel tells the user that with the service
        // up they work with the window closed. Idling on "no rules" alone made
        // that untrue for anyone who bound buttons and never wrote a rule.
        //
        // Asked only when there is nothing else to run: with rules present,
        // serve_once reads the bindings itself and says if it cannot.
        let mut have_bindings = || match bindings_in(&bindings_path) {
            Ok(b) => b,
            Err(msg) => {
                if announced_bindings_problem.as_deref() != Some(msg.as_str()) {
                    eprintln!("logi: {msg}; treating it as no bindings until it can be read");
                    announced_bindings_problem = Some(msg);
                }
                false
            }
        };
        let cfg = match rules {
            None => {
                if have_bindings() {
                    Some(appwatch::Config::default())
                } else {
                    if !announced_idle {
                        eprintln!(
                            "logi: no rules at {} and no bindings; idling. Run `logi watch` \
                             once to write an example.",
                            path.display()
                        );
                        announced_idle = true;
                    }
                    // Nothing to do, so wait rather than spin or exit.
                    nap(60);
                    None
                }
            }
            Some(text) => {
                let (cfg, problems) = appwatch::parse_rules(&text);
                if changed_since(&mut announced_parse, &problems) {
                    for p in &problems {
                        eprintln!("logi: {}: {p}", path.display());
                    }
                }
                if cfg.rules.is_empty() && cfg.fallback.is_none() && !have_bindings() {
                    if !announced_idle {
                        eprintln!("logi: {} has no rules yet; idling", path.display());
                        announced_idle = true;
                    }
                    nap(30);
                    None
                } else {
                    Some(cfg)
                }
            }
        };
        let Some(cfg) = cfg else { continue };
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

    // A restore an earlier connection could not finish, because the mouse went
    // away under it. Before the stop check: this is a repair, not bring-up.
    if has_spy && restore_owed() {
        match settle_owed_restore(dev) {
            Ok(()) => restore_done(),
            Err(e) => eprintln!(
                "logi: the button mapping may still be silenced from the last run: {e}. \
                 `logi bind reset` puts it back."
            ),
        }
    }

    // Asked to stop or reload while connecting: bringing the spy up only to
    // tear it straight down again is two writes to a non-volatile table for
    // nothing.
    if leaving() {
        return Ok(());
    }

    // Button bindings are optional: no file, no bindings, or a device without
    // the feature all just mean the watcher runs alone.
    let bindings = service_bindings();

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
    let mut last_switch_err: Option<String> = None;
    let mut last_check = std::time::Instant::now();

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
                    match profiles.set_current(dev, *profile) {
                        Ok(()) => {
                            last_switch_err = None;
                            if let (Some(d), Some(v)) = (dpi_feat.as_ref(), want) {
                                if let Err(e) = d.set(dev, *v, 0) {
                                    eprintln!("logi: setting {v} dpi: {e}");
                                }
                            }
                        }
                        // Only the endpoint failing ends the run. Any other
                        // failure used to return from here too, and the
                        // teardown and reconnect that follow rewrite the
                        // non-volatile button table twice; a rule naming a
                        // profile the device lacks would have done that every
                        // few seconds for as long as the service ran.
                        Err(e) if endpoint_gone(&e) => return Err(e),
                        Err(e) => {
                            let msg = format!("switching to profile {profile}: {e}");
                            if last_switch_err.as_deref() != Some(msg.as_str()) {
                                eprintln!("logi: {msg}");
                                last_switch_err = Some(msg);
                            }
                            if switch_retryable(&e) {
                                watcher.invalidate();
                            }
                        }
                    }
                }
            }

            if spy_state.is_none() && last_check.elapsed() >= ENDPOINT_CHECK_EVERY {
                last_check = std::time::Instant::now();
                check_endpoint(&profiles, dev)?;
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
                Ok(()) => {
                    restore_done();
                    eprintln!("logi: button mapping restored")
                }
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
    let silence = bindings.to_silence();
    silence_fits(&silence, spy.button_count()).map_err(hidpp::Error::Protocol)?;
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
        if !silence.is_empty() {
            silenced = true;
            owe_restore(&dev.path);
            spy.silence(dev, &silence)?;
        }
        Ok(listener)
    })();

    match brought_up {
        Ok(listener) => Ok(SpyState { spy, listener, vd, bindings: bindings.clone(), silenced }),
        Err(e) => {
            if silenced {
                match spy.restore(dev) {
                    Ok(()) => restore_done(),
                    Err(e2) => eprintln!(
                        "logi: COULD NOT RESTORE the button mapping: {e2}. \
                         Run `logi bind reset`."
                    ),
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
    // A mistyped --dry-run used to be skipped in silence, and the run it was
    // asked to rehearse went ahead and switched profiles for real.
    reject_unknown_flags(args, &["--dry-run"])?;
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
    let mut last_check = std::time::Instant::now();
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
                    if endpoint_gone(&e) {
                        return Err(CliError::Failed(
                            "the endpoint is gone, and this command holds one open\n\
                             handle, so nothing it does from here can work. Re-plug the\n\
                             mouse and run `logi watch` again, or use `logi serve`, which\n\
                             reconnects on its own."
                                .into(),
                        ));
                    }
                    // Let the next match try again rather than believing it
                    // stuck, unless it cannot work: a profile the device does
                    // not have is refused the same way every time.
                    if switch_retryable(&e) {
                        watcher.invalidate();
                    }
                } else if let (Some(d), Some(v)) = (dpi.as_ref(), want) {
                    if let Err(e) = d.set(dev, *v, 0) {
                        eprintln!("logi: setting {v} dpi: {e}");
                    }
                }
            }
        }
        if !dry && last_check.elapsed() >= ENDPOINT_CHECK_EVERY {
            last_check = std::time::Instant::now();
            if let Err(e) = check_endpoint(&profiles, dev) {
                eprintln!("logi: {e}");
                return Err(CliError::Failed(
                    "the endpoint is gone, and this command holds one open\n\
                     handle, so nothing it does from here can work. Re-plug the\n\
                     mouse and run `logi watch` again, or use `logi serve`, which\n\
                     reconnects on its own."
                        .into(),
                ));
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
    // restore_all validates the whole file before it writes anything, but once
    // writing has started it stops at the first sector that fails and the
    // count is lost with the error. Saying only what went wrong leaves the
    // user unable to tell "nothing happened" from "half the flash is the
    // backup and half is what was there before" — and that second state is one
    // sector away from a profile whose buttons do not match its CRC. Re-running
    // is the way out: it skips every sector that already matches.
    let n = profile_io::restore_all(dev, &p, &backup).map_err(|e| {
        CliError::Failed(format!(
            "{e}\n\
             \n\
             Some sectors may already have been written, so the mouse can be\n\
             part way between this backup and what it held before. Run\n\
             \x20   logi restore {path}\n\
             again once the device answers: it rewrites only the sectors that\n\
             still differ."
        ))
    })?;
    println!("{n} sector(s) written; the rest already matched");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    fn flags(args: &[&str], known: &[&str]) -> Result<(), super::CliError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        super::reject_unknown_flags(&owned, known)
    }

    #[test]
    fn a_mistyped_flag_is_refused_rather_than_skipped() {
        // Both flags in this CLI hold back the consequential behaviour, so a
        // flag that is quietly dropped is a flash write the user asked not to
        // make, or a dry run that switched their profiles for real.
        assert!(matches!(
            flags(&["500", "--liv"], &["--live"]),
            Err(super::CliError::Usage(_))
        ));
        assert!(matches!(
            flags(&["--dry-rune"], &["--dry-run"]),
            Err(super::CliError::Usage(_))
        ));
    }

    #[test]
    fn known_flags_and_bare_values_pass() {
        assert!(flags(&["500", "--live"], &["--live"]).is_ok());
        assert!(flags(&["--live"], &["--live"]).is_ok());
        assert!(flags(&[], &["--live"]).is_ok());
        // A value is not a flag, however odd it looks. `-1` reaching the
        // parser is what produces "bad rate"; refusing it here would report it
        // as an unknown option instead.
        assert!(flags(&["-1"], &["--live"]).is_ok());
    }

    #[test]
    fn only_an_endpoint_failure_counts_as_gone() {
        use crate::hidpp::Error;
        // The distinction cmd_watch turns on. A device that answers with an
        // error, or does not answer in time, is still there and the next poll
        // can try again; an Io failure is the node itself, and nothing in a
        // foreground command reopens it.
        assert!(super::endpoint_gone(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        ))));
        assert!(super::endpoint_gone(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::NotConnected
        ))));
        assert!(!super::endpoint_gone(&Error::Timeout { feature: 1, function: 2 }));
        assert!(!super::endpoint_gone(&Error::Device { code: 8, feature: 1, function: 2 }));
        assert!(!super::endpoint_gone(&Error::Protocol("short reply".into())));
        assert!(!super::endpoint_gone(&Error::NoSuchFeature(0x8110)));
    }

    #[test]
    fn a_refused_switch_is_not_retried_but_a_lost_one_is() {
        use crate::hidpp::Error;
        // Protocol is set_current's own range check: it answers the same way
        // every time, and retrying it in the service means tearing down and
        // rewriting the non-volatile button table on every attempt.
        assert!(!super::switch_retryable(&Error::Protocol("profile 9 out of range".into())));
        assert!(!super::switch_retryable(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        ))));
        assert!(super::switch_retryable(&Error::Timeout { feature: 1, function: 2 }));
        assert!(super::switch_retryable(&Error::Device { code: 8, feature: 1, function: 2 }));
    }

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
    fn a_silenced_button_must_exist_on_the_mouse() {
        assert!(super::silence_fits(&[], 11).is_ok());
        assert!(super::silence_fits(&[1, 11], 11).is_ok());
        assert!(super::silence_fits(&[12], 11).is_err());
        assert!(super::silence_fits(&[3, 0], 11).is_err());
        let msg = super::silence_fits(&[12], 11).unwrap_err();
        assert!(msg.contains("12") && msg.contains("1..11"), "{msg}");
    }

    #[test]
    fn a_complaint_is_repeated_only_when_it_changes() {
        let mut seen = Vec::new();
        let a = vec!["line 3: bad".to_string()];
        assert!(super::changed_since(&mut seen, &a));
        assert!(!super::changed_since(&mut seen, &a));
        let b = vec!["line 3: bad".to_string(), "line 5: worse".to_string()];
        assert!(super::changed_since(&mut seen, &b));
        // Fixed, then broken the same way again: said again.
        assert!(super::changed_since(&mut seen, &[]));
        assert!(super::changed_since(&mut seen, &a));
    }

    #[test]
    fn bindings_file_states_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("logi-main-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Absent is not an error: the service just has no bindings.
        assert_eq!(super::bindings_in(&dir.join("absent.conf")), Ok(false));

        let empty = dir.join("empty.conf");
        std::fs::write(&empty, "# nothing here\n").unwrap();
        assert_eq!(super::bindings_in(&empty), Ok(false));

        let some = dir.join("some.conf");
        std::fs::write(&some, "button 4 = key ctrl+c\n").unwrap();
        assert_eq!(super::bindings_in(&some), Ok(true));

        // Present but not readable as text is an error, not "no bindings".
        let bad = dir.join("bad.conf");
        std::fs::write(&bad, [0x62u8, 0x75, 0xff, 0xfe]).unwrap();
        let err = super::bindings_in(&bad).unwrap_err();
        assert!(err.contains("bad.conf"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unfinished_restore_is_remembered_until_it_is_done() {
        // The only test that touches the process-wide record, so it cannot
        // race another.
        assert!(!super::restore_owed());
        super::owe_restore("/dev/hidraw-test");
        assert!(super::restore_owed());
        super::restore_done();
        assert!(!super::restore_owed());
    }

    #[test]
    fn nap_for_zero_returns_without_sleeping() {
        let t = Instant::now();
        super::nap_for(Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
