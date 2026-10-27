//! The systemd user unit, driven from inside the app.
//!
//! The service is what makes per-application profiles and button bindings work
//! with the window closed, and it is the one piece of this program a user would
//! otherwise have to reach a terminal for. So everything `systemctl --user`
//! does for `logi.service` is reachable from here.
//!
//! Three things shaped this module:
//!
//!   * `systemctl` and `journalctl` are shelled out to rather than talked to
//!     over D-Bus. They are the stable interface, they are what the user would
//!     type by hand, and a D-Bus client for this would be a dependency and a
//!     reimplementation of unit-file state resolution for no gain.
//!   * A call takes tens of milliseconds and can take seconds when the unit is
//!     slow to stop, so nothing here may be called from a paint pass. The GUI
//!     and the tray use [`Watcher`], which keeps the last known answer and does
//!     the work on its own thread.
//!   * "no systemd user session" and "the unit is not installed" are different
//!     problems with different fixes. Conflating them produces an error that
//!     tells the user nothing, so they are separate states resolved from the
//!     exit code and stderr that systemd actually produces.
//!
//! Nothing here panics: a missing `systemctl` is an ordinary condition on a
//! machine that does not run systemd, not a bug.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The unit `packaging/install.sh` installs, as systemd names it.
pub const UNIT: &str = "logi.service";

/// The unit file text, kept in one place: this is the same file the installer
/// writes, embedded at build time so the app can install it without needing
/// the source tree beside it.
const UNIT_TEMPLATE: &str = include_str!("../packaging/logi.service");

/// How long the background thread waits between unprompted state polls.
const POLL: Duration = Duration::from_secs(2);

/// The journal is read less often than the state: it costs several times as
/// much and scrolls far more slowly than a unit starts.
const LOG_EVERY: u32 = 3;

/// What the unit is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    Active,
    Inactive,
    Failed,
    /// The unit file does not exist, or systemd cannot load it.
    NotInstalled,
    /// No systemd user session to talk to: an SSH login, a bare TTY, or a
    /// machine that does not run systemd at all.
    ///
    /// The default, because it is what we know before asking.
    #[default]
    NoSystemd,
}

/// One plain sentence about a state, for the panel.
pub fn describe(s: State) -> &'static str {
    match s {
        State::Active => {
            "The service is running, so profile rules and button bindings work with this \
             window closed."
        }
        State::Inactive => {
            "The service is installed but stopped, so profile rules and button bindings do \
             nothing right now."
        }
        State::Failed => {
            "The service started and then failed. The log below has the reason, usually a \
             line in the rules or bindings file."
        }
        State::NotInstalled => {
            "The service is not installed, so nothing runs once this window is closed. \
             Installing it writes one file into your own home directory."
        }
        State::NoSystemd => {
            "There is no systemd user session here, so the service cannot be started from \
             this app. Open logi from your desktop session instead."
        }
    }
}

/// Two or three words for a pill or a tray menu line.
pub fn label(s: State) -> &'static str {
    match s {
        State::Active => "Running",
        State::Inactive => "Stopped",
        State::Failed => "Failed",
        State::NotInstalled => "Not installed",
        State::NoSystemd => "No systemd",
    }
}

/// Whether an action could plausibly succeed, so the UI can disable controls
/// rather than offer buttons that only produce an error.
pub fn can_control(s: State) -> bool {
    matches!(s, State::Active | State::Inactive | State::Failed)
}

// ----------------------------------------------------------- one-shot API ---

/// Current unit state. Blocks for tens of milliseconds; never call this from a
/// paint pass, use [`Watcher::state`].
pub fn state() -> State {
    probe().state
}

/// Whether the unit is enabled, i.e. whether it starts at login.
pub fn is_enabled() -> bool {
    probe().enabled
}

pub fn start() -> Result<(), String> {
    verb("start")
}

pub fn stop() -> Result<(), String> {
    verb("stop")
}

pub fn restart() -> Result<(), String> {
    verb("restart")
}

/// Enable or disable start-at-login. This does not start or stop the unit now.
pub fn set_enabled(on: bool) -> Result<(), String> {
    verb(if on { "enable" } else { "disable" })
}

/// Most recent journal lines for the unit, newest last.
///
/// Timestamps are reduced to a clock time and the hostname and pid are dropped:
/// the panel is narrow and every line is from the same unit on the same
/// machine. Lines that do not match that shape are passed through untouched.
pub fn recent_log(lines: usize) -> Vec<String> {
    let n = lines.clamp(1, 500);
    let count = n.to_string();
    let out = run(
        "journalctl",
        &["--user", "-u", UNIT, "-n", &count, "--no-pager", "-o", "short-iso"],
    );
    match out {
        Ok(o) => parse_journal(&o.stdout, n),
        // No journalctl, or no journal for this user: an empty log, not an
        // error. The state already says whether anything is wrong.
        Err(_) => Vec::new(),
    }
}

/// Ask systemd to re-read unit files. Needed after the unit is written.
pub fn daemon_reload() -> Result<(), String> {
    verb("daemon-reload")
}

// ------------------------------------------------------------- installing ---

/// Where the user unit lives. `~/.config/systemd/user/logi.service`, honouring
/// `XDG_CONFIG_HOME` the same way [`crate::appwatch::config_path`] does.
pub fn unit_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(base).join("systemd/user").join(UNIT)
}

/// Whether the unit file exists on disk. Cheaper than asking systemd, and the
/// answer the install button needs.
pub fn unit_file_exists() -> bool {
    unit_path().is_file()
}

/// The unit file text for a given `logi` binary path.
///
/// The path is quoted when it contains whitespace: systemd splits `ExecStart`
/// on spaces, so an unquoted path under a directory like
/// `~/Desktop/My Project/` would be read as a command plus two arguments and
/// fail with "No such file or directory".
///
/// A backslash forces the quoted form too, and is doubled inside it. systemd
/// runs C-style escapes over `ExecStart` whether the word is quoted or not, so
/// a path through a directory with a backslash in its name is otherwise either
/// mangled (`\b` becomes a backspace) or rejected outright as an unknown
/// escape, which leaves the unit with no ExecStart at all.
pub fn unit_text(exe: &str) -> String {
    let quoted = if exe.contains(char::is_whitespace) || exe.contains(['"', '\\']) {
        // Backslashes first: doing it the other way round would then escape
        // the backslashes this step just added.
        format!("\"{}\"", exe.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        exe.to_string()
    };
    UNIT_TEMPLATE.replace("@BIN@", &quoted)
}

/// Write the unit file for the running binary and reload systemd.
///
/// Returns a sentence for the status bar. Refuses to overwrite a unit whose
/// text differs unless `force`, because a user who has edited their unit should
/// not lose it to a button labelled "install".
pub fn install_unit(force: bool) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find this program's own path: {e}"))?;
    let exe = exe.to_string_lossy().to_string();
    let want = unit_text(&exe);
    let path = unit_path();

    let have = std::fs::read_to_string(&path);
    let why = match &have {
        Err(e) => format!(" ({e})"),
        Ok(_) => String::new(),
    };
    let present = have.is_ok() || path.exists();
    let v = verdict(have.as_deref().ok(), present, &want, force);
    match v {
        Verdict::UpToDate => {
            // Still reload: the file may have been written while systemd was
            // not looking, and reloading twice costs nothing.
            return Ok(format!(
                "{UNIT} is already installed and up to date.{}",
                reload_suffix()
            ));
        }
        Verdict::Differs => {
            return Err(format!(
                "{} already exists and differs from what this app would write. Use \
                 Reinstall to replace it.",
                path.display()
            ));
        }
        Verdict::Unreadable => {
            return Err(format!(
                "{} already exists but cannot be read back{}, so there is no telling \
                 what replacing it would lose. Use Reinstall to replace it anyway.",
                path.display(),
                why
            ));
        }
        Verdict::Write => {}
    }

    let dir = path.parent().ok_or_else(|| "bad unit path".to_string())?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // Write beside the target and rename, so an interrupted write cannot leave
    // systemd with half a unit file.
    let tmp = path.with_extension("service.new");
    std::fs::write(&tmp, &want).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        // Otherwise a failed install leaves logi.service.new sitting in the
        // user's systemd directory for good. systemd ignores it, but it is
        // still our litter in a directory that is theirs.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot install {}: {e}", path.display()));
    }
    Ok(format!("Installed {}. It runs {exe} serve.{}", path.display(), reload_suffix()))
}

/// What to do about whatever is already at the unit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Write,
    UpToDate,
    Differs,
    Unreadable,
}

/// Decide whether an install may write, given what was read back.
///
/// `have` is the file's text, or `None` when there is nothing there *or* when
/// something is there that could not be read: unreadable, or not UTF-8.
/// `present` separates those two. "Differs from what we would write" has to
/// include "we cannot tell", or a unit file the user chmod'd or wrote in some
/// other encoding is the one thing a button labelled Install silently destroys.
fn verdict(have: Option<&str>, present: bool, want: &str, force: bool) -> Verdict {
    match have {
        Some(h) if h == want => Verdict::UpToDate,
        Some(_) if force => Verdict::Write,
        Some(_) => Verdict::Differs,
        None if !present => Verdict::Write,
        None if force => Verdict::Write,
        None => Verdict::Unreadable,
    }
}

/// Reload systemd, and say so only when it did not work.
///
/// A failed reload is not a failed install: the unit file is on disk and the
/// next login picks it up. Reporting the write as an error would be wrong and
/// would invite the user to click Install again.
fn reload_suffix() -> String {
    match daemon_reload() {
        Ok(()) => String::new(),
        Err(_) => " systemd could not be reloaded here, so it takes effect at your next \
                    desktop login."
            .to_string(),
    }
}

// ----------------------------------------------------------------- probing ---

/// Everything one `systemctl show` tells us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    state: State,
    enabled: bool,
}

fn probe() -> Probe {
    let out = run(
        "systemctl",
        &["--user", "show", "-p", "LoadState", "-p", "ActiveState", "-p", "UnitFileState", UNIT],
    );
    let o = match out {
        Ok(o) => o,
        // ENOENT on the binary itself: this machine has no systemd.
        Err(_) => return Probe { state: State::NoSystemd, enabled: false },
    };
    if let Some(s) = classify(&o.stderr, o.code) {
        return Probe { state: s, enabled: false };
    }
    match parse_show(&o.stdout) {
        Some(p) => p,
        // `show` answered with something unexpected. Fall back to the simpler
        // command rather than reporting a state we did not establish.
        None => {
            // Its stderr goes through `classify` first for the same reason
            // `show`'s does: a failed `is-active` leaves stdout empty, and an
            // empty stdout reads as "unknown unit", so without this a command
            // that never reached systemd at all would be reported as a unit
            // that is not installed.
            let state = match run("systemctl", &["--user", "is-active", UNIT]) {
                Ok(o) => classify(&o.stderr, o.code).unwrap_or_else(|| parse_is_active(&o.stdout)),
                Err(_) => State::NoSystemd,
            };
            let enabled = run("systemctl", &["--user", "is-enabled", UNIT])
                .map(|o| parse_enabled(&o.stdout))
                .unwrap_or(false);
            Probe { state, enabled }
        }
    }
}

/// Run one `systemctl --user <verb> [UNIT]` and turn a failure into advice.
fn verb(v: &str) -> Result<(), String> {
    // daemon-reload takes no unit; everything else does.
    let args: Vec<&str> = if v == "daemon-reload" {
        vec!["--user", v]
    } else {
        vec!["--user", v, UNIT]
    };
    let o = run("systemctl", &args).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "systemctl is not installed on this machine, so there is no service to manage. \
             Run `logi serve` yourself if you want the background features."
                .to_string()
        } else {
            format!("could not run systemctl: {e}")
        }
    })?;
    if o.ok {
        return Ok(());
    }
    Err(match classify(&o.stderr, o.code) {
        Some(State::NoSystemd) => {
            "There is no systemd user session here, so the service cannot be started. Open \
             logi from your desktop session."
                .to_string()
        }
        Some(State::NotInstalled) => format!(
            "{UNIT} is not installed. Install it from this panel; it writes one file to {}.",
            unit_path().display()
        ),
        _ => {
            let detail = first_line(&o.stderr);
            if detail.is_empty() {
                format!("systemctl {v} failed{}.", exit_suffix(o.code))
            } else {
                format!("systemctl {v} failed: {detail}")
            }
        }
    })
}

fn exit_suffix(code: Option<i32>) -> String {
    match code {
        Some(c) => format!(" with exit status {c}"),
        None => " (killed by a signal)".to_string(),
    }
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// A finished child process.
struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Spawn a command with no stdin and capture its output.
///
/// There is no timeout: `Command` offers none without a watchdog thread per
/// call, and every caller is already off the paint thread. A wedged systemctl
/// stalls the watcher thread and leaves the UI showing the last known state,
/// which is the right failure mode.
fn run(bin: &str, args: &[&str]) -> std::io::Result<Out> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    Ok(Out {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

// ----------------------------------------------------------------- parsing ---

/// Decide what a failed invocation means, from exactly what systemd writes.
///
/// Returns None when the failure is a real unit-level error (a bad unit file, a
/// masked unit, a service that would not start) and the caller should show the
/// message instead of a state.
fn classify(stderr: &str, code: Option<i32>) -> Option<State> {
    let e = stderr.to_ascii_lowercase();
    // No bus to talk to. The wording differs by version and by why it failed,
    // so this matches several: "Failed to connect to bus: No such file or
    // directory" on a TTY with no XDG_RUNTIME_DIR, "...: No medium found" under
    // sudo, "Failed to connect to user scope bus via local transport" on newer
    // systemd.
    //
    // "System has not been booted with systemd as init system (PID 1). Can't
    // operate." is the other one, and it is the only one that can arrive on
    // its own: a container or a non-systemd distro that still ships the
    // systemctl binary prints it with no bus line after it. Without this the
    // message falls through to the unit checks, `show` prints nothing, and
    // the panel ends up claiming the unit is not installed on a machine that
    // could not have told it either way.
    if e.contains("failed to connect to bus")
        || e.contains("failed to connect to user scope bus")
        || e.contains("failed to get d-bus connection")
        || e.contains("has not been booted with systemd")
        || e.contains("no medium found")
        || e.contains("xdg_runtime_dir")
        || e.contains("dbus_session_bus_address")
    {
        return Some(State::NoSystemd);
    }
    // No unit. The wording again differs: `is-enabled` says "Failed to get
    // unit file state for logi.service: No such file or directory" and exits 1,
    // `status` says "Unit logi.service could not be found" and exits 4, and
    // `start` says "Unit logi.service not found." and exits 5.
    if e.contains("could not be found")
        || e.contains("not-found")
        || e.contains("no such unit")
        || (e.contains(UNIT) && e.contains("not found"))
        || (e.contains("unit file state") && e.contains("no such file"))
        || (code == Some(4) && e.contains(UNIT))
    {
        return Some(State::NotInstalled);
    }
    None
}

/// Parse `systemctl show -p LoadState -p ActiveState -p UnitFileState`.
///
/// Key=Value lines are matched by name rather than position: `--value` would be
/// shorter but then the meaning of each line depends on argument order.
fn parse_show(stdout: &str) -> Option<Probe> {
    let mut load = None;
    let mut active = None;
    let mut file = None;
    for line in stdout.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "LoadState" => load = Some(v.to_ascii_lowercase()),
            "ActiveState" => active = Some(v.to_ascii_lowercase()),
            "UnitFileState" => file = Some(v.to_ascii_lowercase()),
            _ => {}
        }
    }
    let load = load?;
    let enabled = file.as_deref().map(parse_enabled).unwrap_or(false);
    // "not-found" is systemd's answer for a unit that does not exist; "error"
    // and "bad-setting" mean the file is there but unusable, which from the
    // user's point of view is also "not installed properly".
    if load == "not-found" || load == "error" || load == "bad-setting" || load.is_empty() {
        return Some(Probe { state: State::NotInstalled, enabled });
    }
    // A masked unit is loaded and inactive, and start will fail with a message
    // saying so. There is no Masked state to report, and claiming it is not
    // installed would be wrong: the file is right there.
    let state = active.as_deref().map(parse_active_word).unwrap_or(State::Inactive);
    Some(Probe { state, enabled })
}

/// `systemctl is-active` output, which is one word.
///
/// A unit that does not exist answers "inactive" on some versions and "unknown"
/// on others, so "unknown" is read as not installed.
fn parse_is_active(stdout: &str) -> State {
    let w = stdout.trim().lines().next().unwrap_or("").trim().to_ascii_lowercase();
    if w == "unknown" || w.is_empty() {
        return State::NotInstalled;
    }
    parse_active_word(&w)
}

/// One ActiveState word. Transitional states are reported as the state they are
/// heading towards, because a button that flickers to "stopped" while the unit
/// comes up reads as a failure.
fn parse_active_word(w: &str) -> State {
    match w.trim() {
        "active" | "activating" | "reloading" | "refreshing" => State::Active,
        "failed" => State::Failed,
        "inactive" | "deactivating" | "maintenance" => State::Inactive,
        // Including "unknown": a word we do not know is not a reason to claim
        // the service is running.
        _ => State::Inactive,
    }
}

/// `systemctl is-enabled`, or `UnitFileState`. Only a unit that actually starts
/// at login counts as enabled: "static", "linked" and "generated" units have no
/// enablement to toggle.
fn parse_enabled(word: &str) -> bool {
    let w = word.trim().lines().next().unwrap_or("").trim().to_ascii_lowercase();
    w == "enabled" || w == "enabled-runtime"
}

/// Tidy journal output into display lines, newest last.
fn parse_journal(stdout: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in stdout.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        // journalctl's own metadata: "-- Logs begin at ... --",
        // "-- No entries --", "-- Boot ... --".
        if line.trim_start().starts_with("--") {
            continue;
        }
        out.push(tidy_journal_line(line));
    }
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

/// `2026-10-26T20:19:01+0530 host logi[123]: message` -> `20:19:01  message`.
fn tidy_journal_line(line: &str) -> String {
    let Some((ts, rest)) = line.split_once(' ') else {
        return line.to_string();
    };
    let Some(cut) = rest.find("]: ") else {
        return line.to_string();
    };
    let Some(time) = ts.split_once('T').map(|(_, t)| t) else {
        return line.to_string();
    };
    let clock = time.get(..8).unwrap_or(time);
    format!("{clock}  {}", rest[cut + 3..].trim())
}

// ----------------------------------------------------------------- watcher ---

/// Something to ask the service to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
    /// Write the unit file and reload systemd. Not a systemctl verb, but the
    /// step before any of them on a machine where install.sh has not been run,
    /// and the whole point is that the user never opens a terminal.
    Install,
    /// Install over a unit file that differs from ours.
    Reinstall,
}

impl Action {
    /// Present tense, for a busy indicator.
    pub fn describe(self) -> &'static str {
        match self {
            Action::Start => "starting the service",
            Action::Stop => "stopping the service",
            Action::Restart => "restarting the service",
            Action::Enable => "enabling start at login",
            Action::Disable => "disabling start at login",
            Action::Install | Action::Reinstall => "installing the service",
        }
    }
}

#[derive(Debug, Clone)]
struct Shared {
    state: State,
    enabled: bool,
    unit_file: bool,
    log: Vec<String>,
    error: Option<String>,
    notice: Option<String>,
    busy: Option<Action>,
    /// Bumped on every change, so a tray can rebuild its menu only when
    /// something has actually moved.
    rev: u64,
    /// False until the first poll has landed, so the UI can say "checking"
    /// instead of flashing "not installed".
    known: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            state: State::Inactive,
            enabled: false,
            unit_file: false,
            log: Vec::new(),
            error: None,
            notice: None,
            busy: None,
            rev: 0,
            known: false,
        }
    }
}

enum Msg {
    Refresh,
    Do(Action),
}

/// Service state for the UI: a background thread does the blocking work and
/// every accessor reads a cached value.
///
/// The sender is behind a mutex so the whole thing is `Sync`, which lets the
/// window and the tray share one `Arc<Watcher>` instead of each polling
/// systemctl on its own schedule.
pub struct Watcher {
    tx: Mutex<Sender<Msg>>,
    shared: Arc<Mutex<Shared>>,
}

impl Watcher {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mine = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("logi-service".into())
            .spawn(move || pump(rx, mine));
        // A thread that never started would leave the panel waiting for a first
        // poll that cannot arrive, so say so once instead.
        if spawned.is_err() {
            if let Ok(mut s) = shared.lock() {
                s.known = true;
                s.state = State::NoSystemd;
                s.error = Some("could not start a thread to talk to systemd".into());
            }
        }
        Watcher { tx: Mutex::new(tx), shared }
    }

    fn get<T>(&self, f: impl FnOnce(&Shared) -> T, default: T) -> T {
        self.shared.lock().map(|s| f(&s)).unwrap_or(default)
    }

    /// Last known state. Cached, cheap, never blocks on systemctl.
    pub fn state(&self) -> State {
        self.get(|s| s.state, State::Inactive)
    }

    pub fn is_enabled(&self) -> bool {
        self.get(|s| s.enabled, false)
    }

    /// Whether the unit file exists, which is what the install button needs:
    /// it can be true while the state is still `NoSystemd`.
    pub fn unit_file_exists(&self) -> bool {
        self.get(|s| s.unit_file, false)
    }

    /// True once the first poll has completed.
    pub fn known(&self) -> bool {
        self.get(|s| s.known, false)
    }

    pub fn log(&self) -> Vec<String> {
        self.get(|s| s.log.clone(), Vec::new())
    }

    /// The last action that failed, with systemd's reason.
    pub fn last_error(&self) -> Option<String> {
        self.get(|s| s.error.clone(), None)
    }

    /// What the last successful action did, for the status bar.
    pub fn last_notice(&self) -> Option<String> {
        self.get(|s| s.notice.clone(), None)
    }

    /// The action in flight, if any. Controls should be disabled while this is
    /// `Some`: systemctl serialises these anyway and a double click on Restart
    /// just queues a second restart.
    pub fn busy(&self) -> Option<Action> {
        self.get(|s| s.busy, None)
    }

    /// Changes whenever anything above changes.
    pub fn revision(&self) -> u64 {
        self.get(|s| s.rev, 0)
    }

    pub fn clear_messages(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.error = None;
            s.notice = None;
            s.rev = s.rev.wrapping_add(1);
        }
    }

    /// Ask for an update. Returns immediately.
    pub fn refresh(&self) {
        self.send(Msg::Refresh);
    }

    /// Ask for an action. Returns immediately; watch [`Watcher::busy`] and
    /// [`Watcher::last_error`] for the outcome.
    pub fn request(&self, action: Action) {
        if let Ok(mut s) = self.shared.lock() {
            s.busy = Some(action);
            s.error = None;
            s.rev = s.rev.wrapping_add(1);
        }
        if self.send(Msg::Do(action)) {
            return;
        }
        // Nothing is going to run this, so nothing is going to clear `busy`,
        // and every control is greyed while it is set. Without this the panel
        // sits on "starting the service" for the rest of the session with no
        // way back, and the message `new` left about the missing thread has
        // just been wiped by the `error = None` above.
        if let Ok(mut s) = self.shared.lock() {
            s.busy = None;
            s.error = Some(
                "the thread that talks to systemd is not running, so the service cannot \
                 be controlled from here. Use systemctl --user in a terminal."
                    .into(),
            );
            s.rev = s.rev.wrapping_add(1);
        }
    }

    /// False when the message could not be queued, i.e. the thread is gone.
    ///
    /// A dropped refresh is not worth reporting: the cached state simply stops
    /// moving and every control still renders. A dropped action is, because
    /// the caller has already set `busy` on the strength of it.
    fn send(&self, m: Msg) -> bool {
        match self.tx.lock() {
            Ok(tx) => tx.send(m).is_ok(),
            Err(_) => false,
        }
    }

    /// A watcher with no thread behind it, for the test that the UI does not
    /// get stuck when there is none. Built here rather than in the test module
    /// because `Msg` and the fields are private to this one.
    #[cfg(test)]
    fn detached() -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        drop(rx);
        Watcher { tx: Mutex::new(tx), shared: Arc::new(Mutex::new(Shared::default())) }
    }
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

// Dropping the Watcher drops the Sender, the thread's recv_timeout returns
// Disconnected and it exits. Deliberately not joined: a systemctl call in
// flight would hold up window close for as long as it takes.

/// The background thread: poll on a timer, act when asked.
fn pump(rx: mpsc::Receiver<Msg>, shared: Arc<Mutex<Shared>>) {
    let mut ticks: u32 = 0;
    // One poll before the first wait, so the panel is populated by the time
    // anyone looks at it.
    poll_into(&shared, true);
    loop {
        let msg = match rx.recv_timeout(POLL) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match msg {
            Some(Msg::Do(a)) => {
                let before = Instant::now();
                let result = apply(a);
                // A stop or start takes a moment to settle; polling the instant
                // systemctl returns can still read the old state.
                if before.elapsed() < Duration::from_millis(250) {
                    std::thread::sleep(Duration::from_millis(250));
                }
                if let Ok(mut s) = shared.lock() {
                    s.busy = None;
                    match result {
                        Ok(note) => {
                            s.notice = note;
                            s.error = None;
                        }
                        Err(e) => {
                            s.error = Some(e);
                            s.notice = None;
                        }
                    }
                    s.rev = s.rev.wrapping_add(1);
                }
                poll_into(&shared, true);
                ticks = 0;
            }
            Some(Msg::Refresh) => {
                poll_into(&shared, true);
                ticks = 0;
            }
            None => {
                ticks = ticks.wrapping_add(1);
                poll_into(&shared, ticks % LOG_EVERY == 0);
            }
        }
    }
}

fn apply(a: Action) -> Result<Option<String>, String> {
    match a {
        Action::Start => start().map(|_| Some("Service started.".into())),
        Action::Stop => stop().map(|_| Some("Service stopped.".into())),
        Action::Restart => restart().map(|_| Some("Service restarted.".into())),
        Action::Enable => set_enabled(true).map(|_| Some("The service will start at login.".into())),
        Action::Disable => {
            set_enabled(false).map(|_| Some("The service will no longer start at login.".into()))
        }
        Action::Install => install_unit(false).map(Some),
        Action::Reinstall => install_unit(true).map(Some),
    }
}

/// One poll, optionally including the journal.
fn poll_into(shared: &Arc<Mutex<Shared>>, with_log: bool) {
    let p = probe();
    let exists = unit_file_exists();
    // Showing the log for a unit that was never installed is just an empty box.
    let log = if with_log && p.state != State::NoSystemd && exists {
        Some(recent_log(60))
    } else {
        None
    };
    if let Ok(mut s) = shared.lock() {
        let changed = s.state != p.state
            || s.enabled != p.enabled
            || s.unit_file != exists
            || !s.known
            || log.as_ref().is_some_and(|l| *l != s.log);
        s.state = p.state;
        s.enabled = p.enabled;
        s.unit_file = exists;
        s.known = true;
        if let Some(l) = log {
            s.log = l;
        }
        if changed {
            s.rev = s.rev.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is-active / ActiveState words -----------------------------------

    #[test]
    fn the_four_words_systemctl_actually_prints() {
        assert_eq!(parse_is_active("active\n"), State::Active);
        assert_eq!(parse_is_active("inactive\n"), State::Inactive);
        assert_eq!(parse_is_active("failed\n"), State::Failed);
        // A unit that does not exist, on versions that say so this way.
        assert_eq!(parse_is_active("unknown\n"), State::NotInstalled);
    }

    #[test]
    fn transitional_states_read_as_where_they_are_heading() {
        assert_eq!(parse_is_active("activating"), State::Active);
        assert_eq!(parse_is_active("reloading"), State::Active);
        assert_eq!(parse_is_active("deactivating"), State::Inactive);
    }

    #[test]
    fn empty_or_surprising_output_never_claims_running() {
        assert_eq!(parse_is_active(""), State::NotInstalled);
        assert_eq!(parse_is_active("   \n"), State::NotInstalled);
        assert_eq!(parse_active_word("something-new"), State::Inactive);
    }

    #[test]
    fn is_active_output_is_case_insensitive_and_single_line() {
        assert_eq!(parse_is_active("ACTIVE\n"), State::Active);
        assert_eq!(parse_is_active("failed\nextra junk\n"), State::Failed);
    }

    // ---- show output ------------------------------------------------------

    #[test]
    fn show_reports_a_running_enabled_unit() {
        let p = parse_show(
            "LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n",
        )
        .unwrap();
        assert_eq!(p.state, State::Active);
        assert!(p.enabled);
    }

    #[test]
    fn show_reports_a_stopped_disabled_unit() {
        let p = parse_show(
            "LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n",
        )
        .unwrap();
        assert_eq!(p.state, State::Inactive);
        assert!(!p.enabled);
    }

    #[test]
    fn show_reports_a_failed_unit() {
        let p =
            parse_show("LoadState=loaded\nActiveState=failed\nUnitFileState=enabled\n").unwrap();
        assert_eq!(p.state, State::Failed);
        assert!(p.enabled, "a failed unit is still enabled");
    }

    #[test]
    fn show_reports_a_missing_unit_as_not_installed() {
        // What systemd prints for a unit with no file: show succeeds, exit 0.
        let p =
            parse_show("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n").unwrap();
        assert_eq!(p.state, State::NotInstalled);
        assert!(!p.enabled);
    }

    #[test]
    fn a_broken_unit_file_is_not_installed_properly() {
        for load in ["error", "bad-setting"] {
            let p =
                parse_show(&format!("LoadState={load}\nActiveState=inactive\n")).unwrap();
            assert_eq!(p.state, State::NotInstalled, "LoadState={load}");
        }
    }

    #[test]
    fn a_masked_unit_is_loaded_and_stopped_not_missing() {
        let p = parse_show("LoadState=masked\nActiveState=inactive\nUnitFileState=masked\n")
            .unwrap();
        assert_eq!(p.state, State::Inactive);
        assert!(!p.enabled);
    }

    #[test]
    fn show_properties_are_matched_by_name_not_position() {
        let p = parse_show(
            "UnitFileState=enabled\nActiveState=active\nSomethingElse=1\nLoadState=loaded\n",
        )
        .unwrap();
        assert_eq!(p.state, State::Active);
        assert!(p.enabled);
    }

    #[test]
    fn show_output_without_loadstate_is_unusable() {
        assert!(parse_show("ActiveState=active\n").is_none());
        assert!(parse_show("").is_none());
        assert!(parse_show("not key value output").is_none());
    }

    #[test]
    fn enabled_runtime_counts_but_static_does_not() {
        assert!(parse_enabled("enabled\n"));
        assert!(parse_enabled("enabled-runtime\n"));
        assert!(!parse_enabled("disabled\n"));
        assert!(!parse_enabled("static\n"));
        assert!(!parse_enabled("linked\n"));
        assert!(!parse_enabled("generated\n"));
        assert!(!parse_enabled(""));
    }

    // ---- NotInstalled vs NoSystemd ---------------------------------------

    #[test]
    fn no_bus_on_a_tty_is_no_systemd() {
        // systemd 25x, no XDG_RUNTIME_DIR.
        let e = "Failed to connect to bus: $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR \
                 not defined (consider using --machine=<user>@.host --user to connect to \
                 bus of other user)";
        assert_eq!(classify(e, Some(1)), Some(State::NoSystemd));
    }

    #[test]
    fn the_other_no_bus_wordings_are_also_no_systemd() {
        for e in [
            "Failed to connect to bus: No medium found",
            "Failed to connect to bus: No such file or directory",
            "Failed to connect to user scope bus via local transport: Permission denied",
            "Failed to get D-Bus connection: Operation not permitted",
        ] {
            assert_eq!(classify(e, Some(1)), Some(State::NoSystemd), "{e}");
        }
    }

    /// A container or a distro that ships systemctl without running it as PID
    /// 1 prints this and nothing else: no bus line to match on. It used to
    /// fall through to the unit checks and come back as NotInstalled, which
    /// offers the user an install that cannot help.
    #[test]
    fn a_machine_not_booted_with_systemd_is_no_systemd_not_a_missing_unit() {
        let alone = "System has not been booted with systemd as init system (PID 1). \
                     Can't operate.";
        assert_eq!(classify(alone, Some(1)), Some(State::NoSystemd));
        // And with the bus line some versions add after it.
        let both = format!("{alone}\nFailed to connect to bus: Host is down");
        assert_eq!(classify(&both, Some(1)), Some(State::NoSystemd));
    }

    #[test]
    fn a_missing_unit_is_not_installed() {
        // `systemctl --user start logi.service`, exit 5 in newer systemd.
        assert_eq!(
            classify("Failed to start logi.service: Unit logi.service not found.", Some(5)),
            Some(State::NotInstalled)
        );
        // `systemctl --user status logi.service`, exit 4.
        assert_eq!(
            classify("Unit logi.service could not be found.", Some(4)),
            Some(State::NotInstalled)
        );
        // `systemctl --user is-enabled logi.service`, exit 1.
        assert_eq!(
            classify(
                "Failed to get unit file state for logi.service: No such file or directory",
                Some(1)
            ),
            Some(State::NotInstalled)
        );
    }

    #[test]
    fn no_bus_wins_over_a_missing_unit_in_the_same_message() {
        // Without a bus, systemd cannot know whether the unit exists, so the
        // advice must be about the session and not about installing.
        let e = "Failed to connect to bus: No medium found\nUnit logi.service not found.";
        assert_eq!(classify(e, Some(1)), Some(State::NoSystemd));
    }

    #[test]
    fn a_real_unit_error_is_left_to_the_caller_to_report() {
        assert_eq!(classify("Unit logi.service is masked.", Some(1)), None);
        assert_eq!(
            classify("Job for logi.service failed because the control process exited", Some(1)),
            None
        );
        assert_eq!(classify("", Some(1)), None);
        assert_eq!(classify("", None), None);
    }

    #[test]
    fn exit_four_naming_the_unit_is_not_installed() {
        assert_eq!(classify("something about logi.service", Some(4)), Some(State::NotInstalled));
        // The same code without our unit named is not enough to conclude that.
        assert_eq!(classify("unrelated failure", Some(4)), None);
    }

    // ---- journal ----------------------------------------------------------

    #[test]
    fn journal_lines_lose_the_host_and_pid_and_keep_the_clock() {
        let out = "2026-10-26T20:19:01+0530 kali logi[4242]: profile 1 (csgo is running)\n";
        assert_eq!(parse_journal(out, 10), vec!["20:19:01  profile 1 (csgo is running)"]);
    }

    #[test]
    fn journal_metadata_lines_are_dropped() {
        let out = "-- Logs begin at Sun 2026-10-26 20:00:00 IST. --\n\
                   2026-10-26T20:19:01+0530 kali logi[1]: up\n\
                   -- Reboot --\n";
        assert_eq!(parse_journal(out, 10), vec!["20:19:01  up"]);
    }

    #[test]
    fn an_empty_journal_is_an_empty_list() {
        assert!(parse_journal("-- No entries --\n", 10).is_empty());
        assert!(parse_journal("", 10).is_empty());
        assert!(parse_journal("\n  \n", 10).is_empty());
    }

    #[test]
    fn the_journal_is_capped_keeping_the_newest_last() {
        let out: String = (0..10)
            .map(|i| format!("2026-10-26T20:19:0{i}+0530 kali logi[1]: line {i}\n"))
            .collect();
        let got = parse_journal(&out, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], "20:19:07  line 7");
        assert_eq!(got[2], "20:19:09  line 9", "newest last");
    }

    #[test]
    fn journal_lines_of_another_shape_are_kept_as_they_are() {
        // No "]: " — e.g. a kernel line, or an operator's own echo.
        let out = "2026-10-26T20:19:01+0530 kali kernel: usb 1-2: new device\n";
        assert_eq!(parse_journal(out, 10).len(), 1);
        assert!(parse_journal(out, 10)[0].contains("usb 1-2"));
        // A line with no timestamp at all survives too.
        assert_eq!(parse_journal("bare text\n", 10), vec!["bare text"]);
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        let out = "2026-10-26T20:19:01+0530 kali logi[1]: up\r\n";
        assert_eq!(parse_journal(out, 10), vec!["20:19:01  up"]);
    }

    #[test]
    fn systemd_own_lines_come_through() {
        let out = "2026-10-26T20:19:01+0530 kali systemd[1234]: Started Logitech mouse \
                   service.\n";
        assert_eq!(parse_journal(out, 5), vec!["20:19:01  Started Logitech mouse service."]);
    }

    // ---- unit file --------------------------------------------------------

    #[test]
    fn the_unit_text_has_no_placeholder_left() {
        let t = unit_text("/home/u/.local/bin/logi");
        assert!(!t.contains("@BIN@"));
        assert!(t.contains("ExecStart=/home/u/.local/bin/logi serve"));
    }

    #[test]
    fn a_path_with_spaces_is_quoted_for_systemd() {
        // This project's own build tree has spaces in it; unquoted, systemd
        // would read the path as a command plus two arguments.
        let t = unit_text("/home/u/My Project/target/release/logi");
        assert!(
            t.contains("ExecStart=\"/home/u/My Project/target/release/logi\" serve"),
            "{t}"
        );
    }

    #[test]
    fn a_backslash_in_the_path_is_quoted_and_doubled() {
        // systemd runs C escapes over ExecStart quoted or not, so an unescaped
        // `\b` would become a backspace and `\D` would void the whole line.
        let t = unit_text("/home/u/a\\b/logi");
        assert!(t.contains("ExecStart=\"/home/u/a\\\\b/logi\" serve"), "{t}");
        // And a quote is still escaped, with the backslash it needs intact.
        let t = unit_text("/home/u/say \"hi\"/logi");
        assert!(t.contains("ExecStart=\"/home/u/say \\\"hi\\\"/logi\" serve"), "{t}");
        // An ordinary path is still left alone.
        assert!(unit_text("/usr/bin/logi").contains("ExecStart=/usr/bin/logi serve"));
    }

    // ---- install, deciding what to do with what is already there ----------

    #[test]
    fn nothing_there_is_simply_written() {
        assert_eq!(verdict(None, false, "want", false), Verdict::Write);
        assert_eq!(verdict(None, false, "want", true), Verdict::Write);
    }

    #[test]
    fn the_same_text_is_up_to_date_whether_or_not_it_was_forced() {
        assert_eq!(verdict(Some("want"), true, "want", false), Verdict::UpToDate);
        assert_eq!(verdict(Some("want"), true, "want", true), Verdict::UpToDate);
    }

    #[test]
    fn a_hand_edited_unit_is_kept_until_reinstall() {
        assert_eq!(verdict(Some("theirs"), true, "ours", false), Verdict::Differs);
        assert_eq!(verdict(Some("theirs"), true, "ours", true), Verdict::Write);
    }

    /// The case the plain `if let Ok(have)` missed: a unit file that is there
    /// but will not read back, because it is chmod 000 or not UTF-8. Falling
    /// through to the write destroys exactly the file the force flag exists to
    /// protect.
    #[test]
    fn a_unit_file_that_cannot_be_read_is_not_silently_overwritten() {
        assert_eq!(verdict(None, true, "ours", false), Verdict::Unreadable);
        assert_eq!(verdict(None, true, "ours", true), Verdict::Write);
    }

    #[test]
    fn the_embedded_unit_is_the_one_the_installer_uses() {
        assert!(UNIT_TEMPLATE.contains("ExecStart=@BIN@ serve"));
        assert!(UNIT_TEMPLATE.contains("WantedBy=default.target"));
    }

    #[test]
    fn the_unit_path_is_a_user_unit_under_the_config_dir() {
        let p = unit_path();
        assert!(p.ends_with("systemd/user/logi.service"), "{}", p.display());
    }

    // ---- copy -------------------------------------------------------------

    #[test]
    fn every_state_has_advice_and_a_label() {
        for s in [
            State::Active,
            State::Inactive,
            State::Failed,
            State::NotInstalled,
            State::NoSystemd,
        ] {
            assert!(describe(s).len() > 20, "{s:?} needs a real sentence");
            assert!(describe(s).ends_with('.'));
            assert!(!label(s).is_empty());
        }
        assert!(can_control(State::Inactive));
        assert!(!can_control(State::NotInstalled));
        assert!(!can_control(State::NoSystemd));
    }

    #[test]
    fn every_action_describes_itself() {
        for a in [
            Action::Start,
            Action::Stop,
            Action::Restart,
            Action::Enable,
            Action::Disable,
            Action::Install,
            Action::Reinstall,
        ] {
            assert!(!a.describe().is_empty());
        }
    }

    #[test]
    fn exit_status_wording() {
        assert_eq!(exit_suffix(Some(3)), " with exit status 3");
        assert_eq!(exit_suffix(None), " (killed by a signal)");
        assert_eq!(first_line("\n  real problem\nmore\n"), "real problem");
        assert_eq!(first_line(""), "");
    }

    // ---- the watcher with nothing behind it ------------------------------

    /// `request` sets `busy`, and the panel greys every control while it is
    /// set. If the message cannot be queued there is no thread to ever clear
    /// it, so the request has to clear it itself: otherwise the Service tab
    /// spends the rest of the session saying "starting the service" with no
    /// button left to press. `new` already handles a thread that would not
    /// start; this is the other half of it.
    #[test]
    fn an_action_nothing_will_run_does_not_grey_the_panel_for_good() {
        let w = Watcher::detached();
        w.request(Action::Start);
        assert_eq!(w.busy(), None, "busy outlived a request nothing can run");
        let e = w.last_error().expect("the failure should be said out loud");
        assert!(e.contains("systemd"), "{e}");
        // And the revision moved, so a tray polling it redraws.
        assert!(w.revision() > 0);
    }

    /// A refresh is the other way round: a stale state is not worth an error
    /// message, so a dropped one stays quiet.
    #[test]
    fn a_refresh_that_goes_nowhere_says_nothing() {
        let w = Watcher::detached();
        w.refresh();
        assert_eq!(w.last_error(), None);
        assert_eq!(w.busy(), None);
    }

    // ---- live, so not run by default -------------------------------------

    #[test]
    #[ignore = "runs systemctl; read-only but needs a systemd user session"]
    fn probing_the_real_unit_answers_something_sane() {
        let p = probe();
        println!("state={:?} enabled={}", p.state, p.enabled);
        if p.state == State::Active {
            assert!(!recent_log(5).is_empty(), "an active unit should have logged something");
        }
    }

    #[test]
    #[ignore = "runs systemctl in a background thread"]
    fn the_watcher_reaches_a_known_state() {
        let w = Watcher::new();
        for _ in 0..50 {
            if w.known() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(w.known(), "no poll landed within five seconds");
        println!("{:?} rev={}", w.state(), w.revision());
    }
}
