//! Driver and management app for the Logitech G502 HERO, over HID++ 2.0.
//!
//! Run with no arguments to open the GUI; subcommands give the same control
//! from a shell.

mod appwatch;
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

/// Open the device named by $G502_DEVICE (e.g. "046d:c08b"), or the only one.
fn open() -> Result<discover::Found, CliError> {
    let want = std::env::var("G502_DEVICE").ok();
    Ok(discover::open_device_matching(want.as_deref())?)
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
                p.set_mode(dev, profiles::MODE_HOST)?;
                r.set(dev, hz)?;
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
                return Err(CliError::Usage(format!(
                    "profile {n} out of range 0..{}",
                    p.info.profile_count - 1
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
    let mut f = open()?;
    let dev = &mut f.device;
    let l = led::Leds::new(dev)?;
    let p = profiles::OnboardProfiles::new(dev)?;
    // Live lighting is inert in onboard mode, so take host mode first.
    p.set_mode(dev, profiles::MODE_HOST)?;

    let r = (|| -> hidpp::Result<()> {
        match args[0].as_str() {
            "off" => l.all_off(dev),
            "cycle" => {
                // Returns a message explaining the two working routes.
                l.cycle(dev, 0, 8000, 100)
            }
            "breathing" => {
                let c = led::parse_color(args.get(1).map(|s| s.as_str()).unwrap_or("cyan"))?;
                for z in 0..l.zone_count {
                    l.breathing(dev, z, c, 4000, 0, 100)?;
                }
                Ok(())
            }
            spec => {
                let c = led::parse_color(spec)?;
                l.all_fixed(dev, c)
            }
        }
    })();
    r?;
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
            let running = effects::start(name.to_string(), color, 4000)
                .map_err(CliError::Failed)?;
            println!("running {name}; press Ctrl-C to stop and hand the mouse back");
            // Ctrl-C must unwind rather than kill the process: dropping
            // `running` is what puts the device back into onboard mode, and a
            // bare SIGINT would leave its lighting owned by a dead process.
            install_interrupt_handler();
            while !INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            running.stop();
            println!("\nstopped; mouse returned to onboard mode");
            Ok(())
        }
    }
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_interrupt(_sig: libc::c_int) {
    // Only an atomic store, which is async-signal-safe.
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn install_interrupt_handler() {
    // SAFETY: installing a handler that does nothing but one relaxed atomic
    // store, for two signals whose default action we are deliberately
    // replacing.
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_interrupt as *const () as libc::sighandler_t);
    }
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
    loop {
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let rules = std::fs::read_to_string(&path).ok();
        match rules {
            None => {
                eprintln!(
                    "logi: no rules at {}; idling. Run `logi watch` once to write an example.",
                    path.display()
                );
                // Nothing to do, so wait rather than spin or exit.
                for _ in 0..60 {
                    if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            Some(text) => {
                let (cfg, problems) = appwatch::parse_rules(&text);
                for p in &problems {
                    eprintln!("logi: {}: {p}", path.display());
                }
                if cfg.rules.is_empty() && cfg.fallback.is_none() {
                    eprintln!("logi: {} has no rules yet; idling", path.display());
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    continue;
                }
                if let Err(e) = serve_once(cfg) {
                    // A missing or unplugged device is normal, not fatal.
                    eprintln!("logi: {e}; retrying in 10s");
                    for _ in 0..10 {
                        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(());
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        }
    }
    eprintln!("logi: service stopping");
    Ok(())
}

/// One connected run of the watcher. Returns when the device goes away or the
/// service is asked to stop, so the caller can retry.
fn serve_once(cfg: appwatch::Config) -> Result<(), hidpp::Error> {
    let mut f = discover::open_device()?;
    let dev = &mut f.device;
    let profiles = profiles::OnboardProfiles::new(dev)?;
    let dpi_feat = dpi::Dpi::new(dev).ok();
    eprintln!("logi: watching {} with {} rule(s)", f.name, cfg.rules.len());

    let have = profiles.info.profile_count;
    for p in appwatch::referenced_profiles(&cfg) {
        if p >= have {
            eprintln!("logi: rules name profile {p}, device has 0..{}", have - 1);
        }
    }

    let poll = cfg.poll;
    let mut watcher = appwatch::Watcher::new(cfg);
    while !INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
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
        std::thread::sleep(poll);
    }
    Ok(())
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
            eprintln!("logi: warning: rules reference profile {p}, device has 0..{}", have - 1);
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
    while !INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
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
        std::thread::sleep(poll);
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
    std::fs::write(path, profile_io::dump_to_json(&model, &dump))
        .map_err(|e| CliError::Failed(format!("writing {path}: {e}")))?;
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
