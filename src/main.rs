//! Driver and management app for the Logitech G502 HERO, over HID++ 2.0.
//!
//! Run with no arguments to open the GUI; subcommands give the same control
//! from a shell.

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

use hidpp::{Device, Error};

const USAGE: &str = "\
g502 — control a Logitech G502 HERO over HID++ 2.0

  g502                        open the graphical app
  g502 info                   capabilities and current state
  g502 dpi [<n>]              show or set DPI (continuous, snapped to the step)
  g502 rate [<hz>]            show or set the report rate
  g502 profile [<n>]          show or switch the active onboard profile
  g502 profile show <n>       decode one stored profile
  g502 led <spec>             lighting, live (needs host mode)
                                off | <colour> | breathing <colour> | cycle
  g502 effect [list|<name>]   stream a host-side animation until Ctrl-C
  g502 backup <file>          dump every sector to JSON
  g502 restore <file>         write a backup back to flash
  g502 --help

Colours take hex (ff0066, #ff0066) or a name (red, cyan, purple...).
";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return match gui::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("g502: could not start the GUI: {e}");
                eprintln!("      run `g502 info` for the command line instead.");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(CliError::Usage(m)) => {
            eprintln!("g502: {m}\n\n{USAGE}");
            std::process::ExitCode::from(2)
        }
        Err(CliError::Failed(m)) => {
            eprintln!("g502: {m}");
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
                 access and sudo is no longer needed. As a one-off: sudo g502 ..."
            )),
            other => CliError::Failed(other.to_string()),
        }
    }
}

fn open() -> Result<(Device, &'static str), CliError> {
    let f = discover::open_device()?;
    Ok((f.device, f.model))
}

fn run(args: &[String]) -> Result<(), CliError> {
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "info" => cmd_info(),
        "dpi" => cmd_dpi(args.get(1)),
        "rate" => cmd_rate(args.get(1)),
        "profile" => cmd_profile(&args[1..]),
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

fn cmd_info() -> Result<(), CliError> {
    let (mut dev, model) = open()?;
    println!("device   : {model}");
    println!("endpoint : {}", dev.path);
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

    if let Ok(d) = dpi::Dpi::new(&mut dev) {
        println!("\nsensor:");
        match d.capability(&mut dev, 0) {
            Ok(dpi::Capability::Range { min, max, step }) => {
                println!("  range    : {min}-{max} dpi, step {step}")
            }
            Ok(dpi::Capability::List(v)) => println!("  stages   : {v:?}"),
            Err(e) => println!("  range    : unavailable ({e})"),
        }
        if let Ok(v) = d.get(&mut dev, 0) {
            println!("  current  : {v} dpi");
        }
    }

    if let Ok(r) = rate::ReportRate::new(&mut dev) {
        if let (Ok(cur), Ok(all)) = (r.get(&mut dev), r.supported(&mut dev)) {
            println!("\nreport rate: {cur} Hz (supported: {all:?})");
        }
    }

    if let Ok(l) = led::Leds::new(&mut dev) {
        println!("\nlighting: {} zones, ext_caps 0x{:04x}", l.zone_count, l.ext_caps);
        if !l.can_read_back() {
            println!("  (getZoneEffect unimplemented: a zero readback means nothing)");
        }
        for z in &l.zones {
            let modes: Vec<&str> = z.modes.iter().map(|m| led::mode_name(*m)).collect();
            println!("  zone {} {:<14} modes: {}", z.index, z.location_name(), modes.join(", "));
        }
    }

    if let Ok(p) = profiles::OnboardProfiles::new(&mut dev) {
        let i = p.info;
        println!("\nonboard profiles:");
        println!("  {} profiles (+{} rom), {} buttons", i.profile_count, i.profile_count_oob, i.button_count);
        println!("  {} sectors x {} bytes, profile format {}", i.sector_count, i.sector_size, i.profile_format_id);
        if let Ok(m) = p.mode(&mut dev) {
            println!("  mode     : {m} ({})", profiles::mode_name(m));
        }
        if let Ok(a) = p.current(&mut dev) {
            println!("  active   : profile {a}");
        }
    }
    Ok(())
}

fn cmd_dpi(arg: Option<&String>) -> Result<(), CliError> {
    let (mut dev, _) = open()?;
    let d = dpi::Dpi::new(&mut dev)?;
    match arg {
        None => {
            println!("{}", d.get(&mut dev, 0)?);
            Ok(())
        }
        Some(v) => {
            let want: u16 = v.parse().map_err(|_| CliError::Usage(format!("bad dpi {v:?}")))?;
            let got = d.set(&mut dev, want, 0)?;
            if got != want {
                println!("{got} (snapped from {want})");
            } else {
                println!("{got}");
            }
            Ok(())
        }
    }
}

fn cmd_rate(arg: Option<&String>) -> Result<(), CliError> {
    let (mut dev, _) = open()?;
    let r = rate::ReportRate::new(&mut dev)?;
    match arg {
        None => {
            println!("{}", r.get(&mut dev)?);
            Ok(())
        }
        Some(v) => {
            let hz: u16 = v.parse().map_err(|_| CliError::Usage(format!("bad rate {v:?}")))?;
            r.set(&mut dev, hz)?;
            println!("{hz}");
            Ok(())
        }
    }
}

fn cmd_profile(args: &[String]) -> Result<(), CliError> {
    let (mut dev, _) = open()?;
    let p = profiles::OnboardProfiles::new(&mut dev)?;
    match args.first().map(|s| s.as_str()) {
        None => {
            println!("{}", p.current(&mut dev)?);
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
            let blob = p.read_sector(&mut dev, n as u16 + 1, None)?;
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
            p.set_current(&mut dev, n)?;
            println!("{n}");
            Ok(())
        }
    }
}

fn cmd_led(args: &[String]) -> Result<(), CliError> {
    if args.is_empty() {
        return Err(CliError::Usage("led needs a spec: off | <colour> | breathing <colour> | cycle".into()));
    }
    let (mut dev, _) = open()?;
    let l = led::Leds::new(&mut dev)?;
    let p = profiles::OnboardProfiles::new(&mut dev)?;
    // Live lighting is inert in onboard mode, so take host mode first.
    p.set_mode(&mut dev, profiles::MODE_HOST)?;

    let r = (|| -> hidpp::Result<()> {
        match args[0].as_str() {
            "off" => l.all_off(&mut dev),
            "cycle" => {
                for z in 0..l.zone_count {
                    l.cycle(&mut dev, z, 8000, 100)?;
                }
                Ok(())
            }
            "breathing" => {
                let c = led::parse_color(args.get(1).map(|s| s.as_str()).unwrap_or("cyan"))?;
                for z in 0..l.zone_count {
                    l.breathing(&mut dev, z, c, 4000, 0, 100)?;
                }
                Ok(())
            }
            spec => {
                let c = led::parse_color(spec)?;
                l.all_fixed(&mut dev, c)
            }
        }
    })();
    r?;
    println!("applied (live; host mode). It is lost on unplug — use the GUI's");
    println!("\"Store to onboard profile\" to make it persist.");
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

fn cmd_backup(path: &str) -> Result<(), CliError> {
    let (mut dev, model) = open()?;
    let p = profiles::OnboardProfiles::new(&mut dev)?;
    let dump = profile_io::dump_all(&mut dev, &p);
    let readable = dump.iter().filter(|(_, d)| d.is_some()).count();
    std::fs::write(path, profile_io::dump_to_json(model, &dump))
        .map_err(|e| CliError::Failed(format!("writing {path}: {e}")))?;
    println!("{readable}/{} sectors written to {path}", dump.len());
    Ok(())
}

fn cmd_restore(path: &str) -> Result<(), CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::Failed(format!("reading {path}: {e}")))?;
    let backup = profile_io::dump_from_json(&text)?;
    let (mut dev, _) = open()?;
    let p = profiles::OnboardProfiles::new(&mut dev)?;
    let n = profile_io::restore_all(&mut dev, &p, &backup)?;
    println!("{n} sector(s) written; the rest already matched");
    Ok(())
}
