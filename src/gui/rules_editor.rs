//! Editing the per-application profile rules from inside the app.
//!
//! The rules themselves live in `appwatch`; this is only an editor for the
//! file at `appwatch::config_path()`, which until now had to be written by
//! hand. Two things shape it:
//!
//!   * The file stays hand-editable, so saving renders the same
//!     `pattern = profile[,dpi]` format, then parses back what it wrote and
//!     compares. Refusing to save is better than silently mangling a config
//!     the user may have written themselves.
//!   * A line the parser cannot read is kept verbatim, not dropped. One typo
//!     must not cost the user the line.
//!
//! Nothing here touches the device or writes flash: a rule is a host-side
//! decision about which onboard profile to select, so this panel works with
//! no mouse attached.

use crate::appwatch::{self, Config, Proc};
use crate::device::Snapshot;
use crate::dpi::Capability;
use crate::gui::theme;
use eframe::egui;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `appwatch::parse_rules` clamps `poll_ms` to this, so the editor has to as
/// well or a save would not round-trip.
pub const POLL_FLOOR_MS: u64 = 100;
/// As far as the slider goes. A file may hold more; see `check`.
pub const POLL_CEILING_MS: u64 = 10_000;
pub const DEFAULT_POLL_MS: u64 = 1000;

/// DPI used when a rule's optional DPI is first switched on and the device
/// has not told us its own.
const FALLBACK_DPI: u16 = 800;

/// One editable rule. The on-disk equivalent is `appwatch::Rule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub pattern: String,
    pub profile: u8,
    /// None leaves DPI alone when the rule fires.
    pub dpi: Option<u16>,
}

/// A line `parse_rules` rejected, carried through a save untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub raw: String,
    pub problem: String,
}

/// The whole file in editable form. Rows are in precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub rows: Vec<Row>,
    pub fallback: Option<u8>,
    pub poll_ms: u64,
    pub kept: Vec<Kept>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            rows: Vec::new(),
            fallback: None,
            poll_ms: DEFAULT_POLL_MS,
            kept: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The rule probably will not do what the user wants.
    Warning,
    /// The file could not be written correctly like this, so saving is blocked.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Which rule it belongs to; None for a file-wide setting.
    pub row: Option<usize>,
    pub severity: Severity,
    pub text: String,
}

/// Did this line contribute nothing the model already holds?
///
/// Decided by re-parsing the line rather than by reading the problem text: a
/// bad `dpi` still yields a rule (with no DPI), so keeping that line verbatim
/// as well would duplicate the rule on the next save.
fn line_is_lost(raw: &str) -> bool {
    let (c, _) = appwatch::parse_rules(raw);
    c.rules.is_empty() && c.fallback.is_none() && c.poll == Config::default().poll
}

/// Pull the 1-based line number out of a `parse_rules` problem.
fn problem_line(p: &str) -> Option<usize> {
    p.strip_prefix("line ")?
        .split(':')
        .next()?
        .trim()
        .parse::<usize>()
        .ok()
}

impl Model {
    /// Parse a config file into the edit model, keeping the lines
    /// `parse_rules` had to discard.
    pub fn from_text(text: &str) -> (Model, Vec<String>) {
        let (cfg, problems) = appwatch::parse_rules(text);
        let lines: Vec<&str> = text.lines().collect();
        let mut kept = Vec::new();
        let mut notes = Vec::new();
        for p in &problems {
            match problem_line(p).and_then(|n| n.checked_sub(1)).and_then(|i| lines.get(i)) {
                Some(raw) if line_is_lost(raw) => {
                    kept.push(Kept { raw: (*raw).to_string(), problem: p.clone() });
                }
                // The line still produced a rule, so it is already in `rows`;
                // the user only needs telling what was ignored in it.
                Some(raw) => notes.push(format!("{p} (the line reads: {})", raw.trim())),
                None => notes.push(p.clone()),
            }
        }
        let model = Model {
            rows: cfg
                .rules
                .iter()
                .map(|r| Row { pattern: r.pattern.clone(), profile: r.profile, dpi: r.dpi })
                .collect(),
            fallback: cfg.fallback,
            poll_ms: cfg.poll.as_millis() as u64,
            kept,
        };
        (model, notes)
    }

    /// What `parse_rules` will make of this model once it is written.
    pub fn to_config(&self) -> Config {
        let n = self.normalized();
        Config {
            rules: n
                .rows
                .iter()
                .map(|r| appwatch::Rule {
                    pattern: r.pattern.clone(),
                    profile: r.profile,
                    dpi: r.dpi,
                })
                .collect(),
            fallback: n.fallback,
            poll: Duration::from_millis(n.poll_ms),
            ..Config::default()
        }
    }

    /// Trim patterns and clamp the poll interval, matching what the parser
    /// does on the way back in. Comparisons and rendering both use this, so a
    /// trailing space is not reported as an unsaved change.
    pub fn normalized(&self) -> Model {
        Model {
            rows: self
                .rows
                .iter()
                .map(|r| Row { pattern: r.pattern.trim().to_string(), ..r.clone() })
                .collect(),
            fallback: self.fallback,
            poll_ms: self.poll_ms.max(POLL_FLOOR_MS),
            kept: self.kept.clone(),
        }
    }

    /// Render the file. `appwatch::parse_rules` reads back what this writes.
    pub fn render(&self) -> String {
        let n = self.normalized();
        let mut s = String::from(HEADER);
        for r in &n.rows {
            match r.dpi {
                Some(d) => s.push_str(&format!("{} = {}, {}\n", r.pattern, r.profile, d)),
                None => s.push_str(&format!("{} = {}\n", r.pattern, r.profile)),
            }
        }
        s.push('\n');
        if let Some(f) = n.fallback {
            s.push_str(&format!("fallback = {f}\n"));
        }
        s.push_str(&format!("poll_ms = {}\n", n.poll_ms));
        if !n.kept.is_empty() {
            s.push_str("\n# logi could not read the lines below and has left them alone.\n");
            for k in &n.kept {
                s.push_str(k.raw.trim_end());
                s.push('\n');
            }
        }
        s
    }

    /// Raise the rule's precedence by one: rows are in precedence order, so
    /// this moves it one place earlier. False if it is already first.
    pub fn move_up(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.rows.len() {
            return false;
        }
        self.rows.swap(i - 1, i);
        true
    }

    /// Lower it by one. False if it is already last.
    pub fn move_down(&mut self, i: usize) -> bool {
        if i + 1 >= self.rows.len() {
            return false;
        }
        self.rows.swap(i, i + 1);
        true
    }

    pub fn remove(&mut self, i: usize) -> Option<Row> {
        (i < self.rows.len()).then(|| self.rows.remove(i))
    }

    /// Append, so a new rule has the lowest precedence and cannot silently
    /// take over from one the user already tuned.
    pub fn add(&mut self, pattern: impl Into<String>, profile: u8) {
        self.rows.push(Row { pattern: pattern.into(), profile, dpi: None });
    }

    /// Everything wrong with the model, in the order the panel shows it.
    ///
    /// `profile_count` is None when no mouse is attached, in which case
    /// profile numbers are not checked rather than being guessed at.
    pub fn check(&self, profile_count: Option<u8>, dpi: Option<&Capability>) -> Vec<Problem> {
        let mut out = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            let pat = r.pattern.trim();
            let mut push = |severity, text: String| {
                out.push(Problem { row: Some(i), severity, text })
            };

            if pat.is_empty() {
                push(
                    Severity::Warning,
                    "An empty pattern never matches. Type a program name, or remove the rule."
                        .to_string(),
                );
            }
            if pat.contains('=') {
                push(
                    Severity::Error,
                    "A pattern cannot contain '='. Each line is split on the first one, so \
                     this rule would be read as something else. Remove the '='."
                        .to_string(),
                );
            }
            if pat.contains('\n') || pat.contains('\r') {
                push(
                    Severity::Error,
                    "A pattern cannot contain a line break. The file is one rule per line, \
                     so this would be written out as two lines, neither of them the rule \
                     you meant."
                        .to_string(),
                );
            }
            if pat.contains('#') {
                push(
                    Severity::Error,
                    "A pattern cannot contain '#'. Everything after it is a comment, so part \
                     of this rule would be thrown away. Remove the '#'."
                        .to_string(),
                );
            }
            if pat.eq_ignore_ascii_case("fallback") || pat.eq_ignore_ascii_case("poll_ms") {
                push(
                    Severity::Error,
                    format!(
                        "'{pat}' is a setting name in this file, so it cannot also be a \
                         pattern. Match on part of the program's name instead."
                    ),
                );
            }
            if let Some(n) = profile_count {
                if r.profile >= n {
                    push(
                        Severity::Warning,
                        format!(
                            "Profile {} does not exist on the attached mouse, which has {} \
                             (0 to {}). This rule will fail when it fires.",
                            r.profile,
                            n,
                            n.saturating_sub(1)
                        ),
                    );
                }
            }
            if !pat.is_empty() {
                let low = pat.to_ascii_lowercase();
                if let Some(earlier) = self.rows[..i]
                    .iter()
                    .map(|e| e.pattern.trim())
                    .filter(|e| !e.is_empty())
                    .find(|e| low.contains(&e.to_ascii_lowercase()))
                {
                    let text = if earlier.eq_ignore_ascii_case(pat) {
                        "The same pattern appears above. First match wins, so this rule never \
                         applies. Remove it, or move it above the other one."
                            .to_string()
                    } else {
                        format!(
                            "'{earlier}' above matches every process this one does. First \
                             match wins, so this rule never applies. Move it above '{earlier}'."
                        )
                    };
                    push(Severity::Warning, text);
                }
            }
            if let (Some(d), Some(cap)) = (r.dpi, dpi) {
                // `snap` rejects what is out of range and rounds what is merely
                // between two steps, so both get said, and differently: the
                // second one still works, it just does not do exactly this.
                match cap.snap(d) {
                    Err(_) => {
                        let detail = match cap {
                            Capability::Range { min, max, step } => {
                                format!("{min} to {max} in steps of {step}")
                            }
                            Capability::List(v) => {
                                v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
                            }
                        };
                        push(
                            Severity::Warning,
                            format!("{d} DPI is not a value this mouse accepts ({detail})."),
                        );
                    }
                    Ok(snapped) if snapped != d => push(
                        Severity::Warning,
                        format!(
                            "{d} DPI falls between two steps of this sensor, so the rule \
                             will set {snapped} instead."
                        ),
                    ),
                    Ok(_) => {}
                }
            }
        }

        if let (Some(f), Some(n)) = (self.fallback, profile_count) {
            if f >= n {
                out.push(Problem {
                    row: None,
                    severity: Severity::Warning,
                    text: format!(
                        "The fallback names profile {f}, but the attached mouse has {n} \
                         (0 to {}).",
                        n.saturating_sub(1)
                    ),
                });
            }
        }
        if self.poll_ms > POLL_CEILING_MS {
            out.push(Problem {
                row: None,
                severity: Severity::Warning,
                text: format!(
                    "The poll interval is {} ms, further than the slider reaches \
                     ({POLL_CEILING_MS} ms). It is kept as it is until you move the slider.",
                    self.poll_ms
                ),
            });
        }
        if self.poll_ms < POLL_FLOOR_MS {
            out.push(Problem {
                row: None,
                severity: Severity::Warning,
                text: format!(
                    "A poll interval under {POLL_FLOOR_MS} ms is raised to {POLL_FLOOR_MS} ms \
                     when the file is read."
                ),
            });
        }
        out
    }
}

/// Render the model, parse the result back, and confirm it survived.
///
/// The one check that matters before overwriting someone's config: if what
/// comes back out is not what went in, the file is not written at all.
pub fn round_trip(model: &Model) -> Result<String, String> {
    let want = model.normalized();
    let text = want.render();
    let (got, problems) = appwatch::parse_rules(&text);

    if got.rules.len() != want.rows.len() {
        return Err(format!(
            "Not saved: writing these rules produced {} back instead of {}. One of the \
             patterns is not writable in this format; check the flagged rules.",
            got.rules.len(),
            want.rows.len()
        ));
    }
    for (r, w) in got.rules.iter().zip(&want.rows) {
        if r.pattern != w.pattern || r.profile != w.profile || r.dpi != w.dpi {
            return Err(format!(
                "Not saved: the rule '{}' came back as '{}' = {}. Remove any '=' or '#' \
                 from the pattern.",
                w.pattern, r.pattern, r.profile
            ));
        }
    }
    if got.fallback != want.fallback {
        return Err("Not saved: the fallback profile did not survive being written.".to_string());
    }
    if got.poll.as_millis() as u64 != want.poll_ms {
        return Err("Not saved: the poll interval did not survive being written.".to_string());
    }
    // The only problems allowed are the ones from the lines kept verbatim,
    // which were already unreadable when they arrived.
    if problems.len() != want.kept.len() {
        return Err(format!(
            "Not saved: the file this would write has {} unreadable line(s), and only {} \
             were there before. Check the flagged rules.",
            problems.len(),
            want.kept.len()
        ));
    }
    Ok(text)
}

/// One program the user could make a rule for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub comm: String,
    /// How many processes share this name, which is how a browser or a game
    /// launcher shows up.
    pub count: usize,
    pub cmdline: String,
}

/// Collapse a /proc scan into pickable programs.
///
/// Kernel threads have an empty cmdline and are not something a rule can
/// usefully match, and logi's own processes would only ever match itself, so
/// both are dropped. Names are deduplicated because a rule matches on a
/// substring anyway, and twenty `chrome` rows would bury everything else.
pub fn pick_list(procs: &[Proc], filter: &str, exclude_comm: &str) -> Vec<ProcEntry> {
    let f = filter.trim().to_ascii_lowercase();
    let mut out: Vec<ProcEntry> = Vec::new();
    for p in procs {
        if p.cmdline.is_empty() || p.comm.is_empty() {
            continue;
        }
        if !exclude_comm.is_empty() && p.comm == exclude_comm {
            continue;
        }
        if !f.is_empty()
            && !p.comm.to_ascii_lowercase().contains(&f)
            && !p.cmdline.to_ascii_lowercase().contains(&f)
        {
            continue;
        }
        match out.iter_mut().find(|e| e.comm == p.comm) {
            Some(e) => e.count += 1,
            None => out.push(ProcEntry {
                comm: p.comm.clone(),
                count: 1,
                cmdline: p.cmdline.clone(),
            }),
        }
    }
    out.sort_by(|a, b| a.comm.to_ascii_lowercase().cmp(&b.comm.to_ascii_lowercase()));
    out
}

/// This process's own name, so the picker does not offer logi itself.
fn self_comm() -> String {
    std::fs::read_to_string("/proc/self/comm")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[derive(Default)]
struct ScanShared {
    procs: Vec<Proc>,
    me: String,
    /// Bumped on every completed scan, so `ui` can tell a new result from the
    /// one it already copied without comparing a few hundred strings.
    rev: u64,
}

/// The /proc scan, moved off the paint pass.
///
/// `scan_processes` reads two files for every process on the machine, several
/// hundred of them. Doing that inside `ui()` stalled the frame it happened on,
/// which is a visible hitch every couple of seconds while this tab is open. So
/// a thread does it on request and `ui()` only ever reads the last result.
///
/// The thread parks in `recv` between requests and ends when the editor is
/// dropped and the sender goes with it, so nothing keeps scanning for a tab
/// the user is not looking at.
struct Scanner {
    shared: Arc<Mutex<ScanShared>>,
    req: Sender<()>,
}

impl Scanner {
    /// `Builder`, not `thread::spawn`: a process that cannot make another
    /// thread must lose the picker, not the window.
    fn start() -> Option<Scanner> {
        let (req, rx) = mpsc::channel::<()>();
        let shared: Arc<Mutex<ScanShared>> = Arc::new(Mutex::new(ScanShared::default()));
        let mine = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("logi-procscan".into())
            .spawn(move || {
                let me = self_comm();
                while rx.recv().is_ok() {
                    // Coalesce anything that queued up while we scanned.
                    while rx.try_recv().is_ok() {}
                    let procs = appwatch::scan_processes();
                    let Ok(mut s) = mine.lock() else { return };
                    s.procs = procs;
                    s.me = me.clone();
                    s.rev = s.rev.wrapping_add(1);
                }
            })
            .ok()?;
        Some(Scanner { shared, req })
    }

    fn request(&self) {
        // A dead thread means a stale list, not a broken panel.
        let _ = self.req.send(());
    }

    /// The newest scan, if it is newer than `seen`.
    fn newer_than(&self, seen: u64) -> Option<(Vec<Proc>, String, u64)> {
        let s = self.shared.lock().ok()?;
        (s.rev != seen && s.rev != 0).then(|| (s.procs.clone(), s.me.clone(), s.rev))
    }
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "rules.conf".to_string());
    // Write beside the target and rename, so a crash mid-write cannot leave a
    // truncated config behind.
    let tmp = path.with_file_name(format!(".{name}.new"));
    match write_then_rename(&tmp, path, text) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leaving a half-written dotfile next to the config would be read
            // as part of it by nothing, but it would still be litter the user
            // has to find and delete.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn write_then_rename(tmp: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp)?;
        // Keep whatever mode the config already had: the rename replaces the
        // inode, so without this a file the user had locked down to 0600 comes
        // back at the umask default.
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = f.set_permissions(meta.permissions());
        }
        f.write_all(text.as_bytes())?;
        // The rename is atomic against another reader, but only a flushed
        // temp file is atomic against the power going out.
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Quiet,
    Note(String),
    Saved(String),
    Failed(String),
}

/// The rules panel.
pub struct RulesEditor {
    path: PathBuf,
    model: Model,
    /// The model as last read from or written to disk, for `dirty`.
    disk: Model,
    /// The file text as last read, to notice an edit made outside the app.
    disk_text: Option<String>,
    /// Problems from the last load that did not cost a whole line.
    notes: Vec<String>,
    status: Status,
    loaded_once: bool,
    /// logi's own process name, from the scanning thread.
    me: String,

    filter: String,
    /// The last scan the background thread handed over.
    procs: Vec<Proc>,
    procs_rev: u64,
    scanned: bool,
    scanner: Option<Scanner>,
    /// Set once if the thread could not be started; the picker then says so
    /// rather than scanning /proc from inside the paint pass instead.
    no_scanner: bool,
    /// Why the file could not be read, when it is there but unreadable.
    /// Saving is blocked while this is set: an empty editor over a config we
    /// never managed to read would write the user's rules away.
    unreadable: Option<String>,
    /// Set when a Save found the file different from the copy in
    /// `disk_text`. The next Save goes through; this is the one chance to
    /// press Reload instead.
    confirm_overwrite: bool,
    requested: Option<Instant>,
}

impl Default for RulesEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl RulesEditor {
    /// Does not touch the disk; the first `ui` call loads.
    pub fn new() -> Self {
        Self::with_path(appwatch::config_path())
    }

    /// Same, against a given path. Used by the tests so they never go near the
    /// real config.
    pub fn with_path(path: PathBuf) -> Self {
        RulesEditor {
            path,
            model: Model::default(),
            disk: Model::default(),
            disk_text: None,
            notes: Vec::new(),
            status: Status::Quiet,
            loaded_once: false,
            me: String::new(),
            filter: String::new(),
            procs: Vec::new(),
            procs_rev: 0,
            scanned: false,
            scanner: None,
            no_scanner: false,
            unreadable: None,
            confirm_overwrite: false,
            requested: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Read the file, separating "there is no file yet" from "there is one
    /// and it would not come back".
    ///
    /// `read_to_string(..).ok()` collapsed both into `None`, and `adopt(None)`
    /// reads that as a fresh start: a config the user had chmodded, or one
    /// holding a byte sequence that is not UTF-8, came up as an empty editor
    /// saying "No rules file yet", and the next Save overwrote every rule in
    /// it. Anything that is not `NotFound` has to stop the load instead.
    fn read_file(&self) -> Result<Option<String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!(
                "Could not read {}: {e}. The file is still there, so saving is blocked \
                 rather than writing an empty one over it. Fix its permissions or its \
                 contents, or move it aside, then press Reload from file.",
                self.path.display()
            )),
        }
    }

    /// `read_file`, recording the failure in the panel. `None` means the
    /// caller must stop: the model is left exactly as it was.
    fn read_or_record(&mut self) -> Option<Option<String>> {
        match self.read_file() {
            Ok(t) => {
                self.unreadable = None;
                Some(t)
            }
            Err(msg) => {
                self.unreadable = Some(msg.clone());
                self.status = Status::Failed(msg);
                None
            }
        }
    }

    /// Load from disk. Safe to call repeatedly: unsaved edits are kept, and
    /// the user is told the file moved under them instead of losing work.
    pub fn load(&mut self) {
        self.loaded_once = true;
        let Some(text) = self.read_or_record() else { return };
        if self.dirty() && self.disk_text.is_some() && text != self.disk_text {
            self.status = Status::Note(
                "This file changed on disk while you were editing. Saving replaces it; \
                 Reload from file throws your edits away."
                    .to_string(),
            );
            return;
        }
        if self.dirty() {
            return;
        }
        self.adopt(text);
    }

    /// Load from disk and throw away any edits.
    pub fn reload(&mut self) {
        self.loaded_once = true;
        let Some(text) = self.read_or_record() else { return };
        self.adopt(text);
    }

    fn adopt(&mut self, text: Option<String>) {
        match &text {
            Some(t) => {
                let (m, notes) = Model::from_text(t);
                self.notes = notes;
                self.model = m.clone();
                self.disk = m;
                self.status = Status::Quiet;
            }
            None => {
                self.notes.clear();
                self.model = Model::default();
                self.disk = Model::default();
                self.status = Status::Note(format!(
                    "No rules file yet. Add a rule and press Save to create {}.",
                    self.path.display()
                ));
            }
        }
        self.disk_text = text;
        self.confirm_overwrite = false;
    }

    pub fn dirty(&self) -> bool {
        self.model.normalized() != self.disk.normalized()
    }

    /// Render, verify, then replace the file. Nothing is written if the
    /// round trip does not match.
    ///
    /// The copy in `disk_text` can be hours old: `load` runs once, when the
    /// window opens, and this file is meant to be hand-edited. So the file is
    /// re-read here and a Save that would replace something logi never showed
    /// costs a second press. Writing over an editor window the user still has
    /// open is the one way this panel can destroy rules it never displayed.
    pub fn save(&mut self) {
        if let Some(why) = self.unreadable.clone() {
            self.status = Status::Failed(why);
            return;
        }
        if !self.confirm_overwrite {
            match self.read_file() {
                Ok(text) if text != self.disk_text => {
                    self.confirm_overwrite = true;
                    self.status = Status::Note(format!(
                        "{} changed on disk since logi read it. Press Save again to \
                         replace it with what is on screen, or Reload from file to take \
                         the version on disk and lose the edits here.",
                        self.path.display()
                    ));
                    return;
                }
                Ok(_) => {}
                Err(msg) => {
                    // It was readable at load and is not now. Same rule as
                    // there: do not write over a file we cannot read.
                    self.unreadable = Some(msg.clone());
                    self.status = Status::Failed(msg);
                    return;
                }
            }
        }
        let model = self.model.normalized();
        match round_trip(&model) {
            Err(e) => self.status = Status::Failed(e),
            Ok(text) => match write_atomically(&self.path, &text) {
                Ok(()) => {
                    self.model = model.clone();
                    self.disk = model;
                    self.disk_text = Some(text);
                    self.confirm_overwrite = false;
                    // `round_trip` has already confirmed that the only
                    // problems the written file still has are the kept lines,
                    // so every note from the load is about a line that has
                    // just been rewritten. Leaving them up would keep
                    // "Lines logi could not read" complaining about a line
                    // that is no longer in the file.
                    self.notes.clear();
                    self.status = Status::Saved(format!("Saved to {}.", self.path.display()));
                }
                Err(e) => {
                    self.status = Status::Failed(format!(
                        "Could not write {}: {e}. Check that the directory exists and is \
                         yours to write to.",
                        self.path.display()
                    ))
                }
            },
        }
    }

    /// Collect the last scan and ask for another if it is getting old.
    ///
    /// Nothing here touches the disk: the asking is a channel send and the
    /// collecting is a mutex that the scanning thread only holds while it
    /// swaps its result in.
    fn maybe_scan(&mut self) {
        if self.scanner.is_none() {
            if self.no_scanner {
                return;
            }
            match Scanner::start() {
                Some(s) => self.scanner = Some(s),
                None => {
                    self.no_scanner = true;
                    return;
                }
            }
        }
        let fresh = self.scanner.as_ref().and_then(|s| s.newer_than(self.procs_rev));
        if let Some((procs, me, rev)) = fresh {
            self.procs = procs;
            self.me = me;
            self.procs_rev = rev;
            self.scanned = true;
        }
        let due = self
            .requested
            .map(|t| t.elapsed() > Duration::from_secs(2))
            .unwrap_or(true);
        if due {
            if let Some(s) = &self.scanner {
                s.request();
            }
            self.requested = Some(Instant::now());
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if !self.loaded_once {
            self.load();
        }
        let profile_count = (snap.profile_count > 0).then_some(snap.profile_count);
        let problems = self.model.check(profile_count, snap.dpi_cap.as_ref());
        let errors = problems.iter().filter(|p| p.severity == Severity::Error).count();
        let warnings = problems.len() - errors;

        let n = self.model.rows.len();
        theme::readout(ui, &n.to_string(), if n == 1 { "rule" } else { "rules" });
        ui.horizontal_wrapped(|ui| {
            if self.unreadable.is_some() {
                theme::pill(ui, "file unreadable", theme::BAD);
            } else if self.dirty() {
                theme::pill(ui, "unsaved changes", theme::WARN);
            } else {
                theme::pill(ui, "matches the file", theme::GOOD);
            }
            if errors > 0 {
                theme::pill(ui, &format!("{errors} blocking saving"), theme::BAD);
            }
            if warnings > 0 {
                theme::pill(ui, &format!("{warnings} to look at"), theme::WARN);
            }
        });
        ui.add_space(8.0);

        theme::section(ui, "How a rule fires", |ui| {
            theme::hint(
                ui,
                "A rule matches on whether a program is RUNNING, not on which window has \
                 focus: Wayland does not let an ordinary process see the active window. For \
                 a game, which is what this is for, those are usually the same thing.",
            );
            theme::hint(
                ui,
                "The pattern is matched, case-insensitively, against each running process's \
                 name and its full command line. The first rule that matches wins, so the \
                 order below is the precedence: put the specific patterns above the general \
                 ones.",
            );
            theme::hint(
                ui,
                "Saving writes a text file. It does not write the mouse's flash, and it \
                 cannot change a button binding.",
            );
        });

        self.rules_section(ui, &problems, profile_count, snap);
        self.settings_section(ui, &problems, profile_count);
        self.picker_section(ui, snap);
        self.kept_section(ui);
        self.file_section(ui, errors);
    }

    fn rules_section(
        &mut self,
        ui: &mut egui::Ui,
        problems: &[Problem],
        profile_count: Option<u8>,
        snap: &Snapshot,
    ) {
        let limit = profile_count.map(|n| n.saturating_sub(1)).unwrap_or(15);
        let default_dpi = if snap.dpi >= 100 { snap.dpi } else { FALLBACK_DPI };
        let mut act: Option<(usize, i8)> = None;
        let mut remove: Option<usize> = None;

        theme::section(ui, "Rules, highest precedence first", |ui| {
            if profile_count.is_none() {
                theme::hint(
                    ui,
                    "No mouse attached, so profile numbers are not being checked against one.",
                );
                ui.add_space(6.0);
            }
            if self.model.rows.is_empty() {
                theme::hint(
                    ui,
                    "No rules yet. Add one below, or pick a program from the list of what is \
                     running.",
                );
            } else {
                let pw = (ui.available_width() - 322.0).clamp(100.0, 290.0);
                ui.horizontal(|ui| {
                    for (w, label) in [
                        (pw, "WHEN THIS IS RUNNING"),
                        (58.0, "PROFILE"),
                        (96.0, "DPI"),
                    ] {
                        ui.add_sized(
                            egui::vec2(w, 13.0),
                            egui::Label::new(
                                egui::RichText::new(label).color(theme::FAINT).size(10.0).strong(),
                            ),
                        );
                    }
                });
                let last = self.model.rows.len() - 1;
                for i in 0..self.model.rows.len() {
                    ui.horizontal(|ui| {
                        let row = &mut self.model.rows[i];
                        ui.add_sized(
                            egui::vec2(pw, 26.0),
                            egui::TextEdit::singleline(&mut row.pattern)
                                .hint_text("program name"),
                        );
                        // `clamp_existing_to_range(false)` matters: egui
                        // otherwise writes the clamped value straight back
                        // through the borrow, so merely painting this panel
                        // would rewrite a hand-written `csgo = 7` to the
                        // attached mouse's last profile. The device's count is
                        // advisory here; `check` says so in words instead.
                        ui.add_sized(
                            egui::vec2(54.0, 26.0),
                            egui::DragValue::new(&mut row.profile)
                                .range(0..=limit)
                                .clamp_existing_to_range(false),
                        );
                        let mut on = row.dpi.is_some();
                        if ui.checkbox(&mut on, "").changed() {
                            row.dpi = on.then_some(default_dpi);
                        }
                        match row.dpi.as_mut() {
                            Some(d) => {
                                ui.add_sized(
                                    egui::vec2(66.0, 26.0),
                                    egui::DragValue::new(d)
                                        .range(100..=25600)
                                        .speed(25.0)
                                        .clamp_existing_to_range(false),
                                );
                            }
                            None => {
                                ui.add_sized(
                                    egui::vec2(66.0, 26.0),
                                    egui::Label::new(
                                        egui::RichText::new("unchanged")
                                            .color(theme::FAINT)
                                            .size(11.0),
                                    ),
                                );
                            }
                        }
                        if ui
                            .add_enabled(i > 0, egui::Button::new("Up").small())
                            .on_hover_text("Give this rule precedence over the one above")
                            .clicked()
                        {
                            act = Some((i, -1));
                        }
                        if ui
                            .add_enabled(i < last, egui::Button::new("Down").small())
                            .clicked()
                        {
                            act = Some((i, 1));
                        }
                        if ui.add(egui::Button::new("Remove").small()).clicked() {
                            remove = Some(i);
                        }
                    });
                    for p in problems.iter().filter(|p| p.row == Some(i)) {
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            let c = match p.severity {
                                Severity::Error => theme::BAD,
                                Severity::Warning => theme::WARN,
                            };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&p.text).color(c).size(11.5),
                                )
                                .wrap(),
                            );
                        });
                    }
                    ui.add_space(3.0);
                }
            }

            ui.add_space(8.0);
            if ui.button("Add a rule").clicked() {
                let p = profile_count.map(|n| n.saturating_sub(1)).unwrap_or(0);
                self.model.add(String::new(), snap.active_profile.min(p));
            }

            if let Some((i, dir)) = act {
                if dir < 0 {
                    self.model.move_up(i);
                } else {
                    self.model.move_down(i);
                }
            }
            if let Some(i) = remove {
                self.model.remove(i);
            }
        });
    }

    fn settings_section(
        &mut self,
        ui: &mut egui::Ui,
        problems: &[Problem],
        profile_count: Option<u8>,
    ) {
        let limit = profile_count.map(|n| n.saturating_sub(1)).unwrap_or(15);
        theme::section(ui, "When nothing matches", |ui| {
            ui.horizontal(|ui| {
                let mut on = self.model.fallback.is_some();
                if ui.checkbox(&mut on, "Select a fallback profile").changed() {
                    self.model.fallback = on.then_some(0);
                }
                if let Some(f) = self.model.fallback.as_mut() {
                    ui.add(
                        egui::DragValue::new(f)
                            .range(0..=limit)
                            .clamp_existing_to_range(false),
                    );
                }
            });
            theme::hint(
                ui,
                "With no fallback, the profile is left exactly as it is when no rule matches.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                // Same reason as the profile DragValue: the default
                // `SliderClamping::Always` would quietly pull a `poll_ms =
                // 30000` from the file down to the end of the slider on the
                // first frame the panel is painted.
                ui.add(
                    egui::Slider::new(&mut self.model.poll_ms, POLL_FLOOR_MS..=POLL_CEILING_MS)
                        .clamping(egui::SliderClamping::Edits)
                        .suffix(" ms")
                        .text("Check for programs every"),
                );
            });
            theme::hint(
                ui,
                "How often the service scans the process list. 1000 ms is the default; \
                 below 100 ms is ignored.",
            );
            for p in problems.iter().filter(|p| p.row.is_none()) {
                ui.add_space(4.0);
                let c = match p.severity {
                    Severity::Error => theme::BAD,
                    Severity::Warning => theme::WARN,
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(&p.text).color(c).size(11.5)).wrap(),
                );
            }
        });
    }

    fn picker_section(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        self.maybe_scan();
        let list = pick_list(&self.procs, &self.filter, &self.me);
        let shown = list.len().min(60);
        let mut add: Option<String> = None;

        theme::section(ui, "Pick a running program", |ui| {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 70.0).max(120.0);
                ui.add_sized(
                    egui::vec2(w, 26.0),
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("filter by name or command line"),
                );
                if ui.button("Clear").clicked() {
                    self.filter.clear();
                }
            });
            ui.add_space(6.0);
            if list.is_empty() {
                theme::hint(
                    ui,
                    if self.no_scanner {
                        "The process list is unavailable: logi could not start the thread \
                         that reads it. Type a program name into a rule above instead."
                    } else if !self.scanned {
                        "Reading the process list..."
                    } else if self.filter.trim().is_empty() {
                        "Nothing to show. The list excludes kernel threads and logi itself."
                    } else {
                        "No running program matches that filter."
                    },
                );
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("rules_proc_list")
                    .max_height(200.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for e in list.iter().take(shown) {
                            ui.horizontal(|ui| {
                                if ui
                                    .add(egui::Button::new(&e.comm).small())
                                    .on_hover_text(&e.cmdline)
                                    .clicked()
                                {
                                    add = Some(e.comm.clone());
                                }
                                if e.count > 1 {
                                    ui.label(
                                        egui::RichText::new(format!("x{}", e.count))
                                            .color(theme::DIM)
                                            .size(11.0),
                                    );
                                }
                                let mut cmd = e.cmdline.clone();
                                if cmd.chars().count() > 56 {
                                    cmd = cmd.chars().take(56).collect::<String>() + "...";
                                }
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(cmd)
                                            .color(theme::FAINT)
                                            .size(11.0),
                                    )
                                    .truncate(),
                                );
                            });
                        }
                    });
            }
            ui.add_space(8.0);
            theme::hint(
                ui,
                &format!(
                    "{} program name(s) to choose from{}, {} shown. Clicking one adds a rule \
                     for that name at the bottom of the list, matching profile {}. Repeats of \
                     one name are collapsed, and kernel threads and logi itself are left \
                     out. {}",
                    list.len(),
                    if self.filter.trim().is_empty() { "" } else { " after the filter" },
                    shown,
                    snap.active_profile,
                    if self.no_scanner {
                        "The list is not being refreshed."
                    } else {
                        "Refreshed every couple of seconds, in the background."
                    }
                ),
            );

            if let Some(name) = add {
                let p = snap.profile_count.saturating_sub(1);
                self.model.add(name, if p > 0 { snap.active_profile.min(p) } else { 0 });
            }
        });
    }

    fn kept_section(&mut self, ui: &mut egui::Ui) {
        if self.model.kept.is_empty() && self.notes.is_empty() {
            return;
        }
        let mut drop: Option<usize> = None;
        theme::section(ui, "Lines logi could not read", |ui| {
            for n in &self.notes {
                ui.add(
                    egui::Label::new(egui::RichText::new(n).color(theme::WARN).size(11.5))
                        .wrap(),
                );
            }
            if !self.model.kept.is_empty() {
                if !self.notes.is_empty() {
                    ui.add_space(6.0);
                }
                theme::hint(
                    ui,
                    "These are written back to the file exactly as they are, so nothing you \
                     typed is lost. Nothing acts on them until they parse.",
                );
                ui.add_space(6.0);
                for (i, k) in self.model.kept.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new("Discard").small()).clicked() {
                            drop = Some(i);
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(k.raw.trim()).monospace().color(theme::TEXT),
                            )
                            .truncate(),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&k.problem).color(theme::WARN).size(11.5),
                            )
                            .wrap(),
                        );
                    });
                }
            }
            if let Some(i) = drop {
                if i < self.model.kept.len() {
                    self.model.kept.remove(i);
                }
            }
        });
    }

    fn file_section(&mut self, ui: &mut egui::Ui, errors: usize) {
        theme::section(ui, "File", |ui| {
            theme::row(ui, "path", &self.path.display().to_string());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_save = errors == 0 && self.dirty() && self.unreadable.is_none();
                if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                    self.save();
                }
                if ui.button("Reload from file").clicked() {
                    self.reload();
                }
            });
            ui.add_space(8.0);
            if errors > 0 {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "Saving is blocked by {errors} problem(s) marked in red above."
                        ))
                        .color(theme::BAD)
                        .size(12.0),
                    )
                    .wrap(),
                );
            }
            match &self.status {
                Status::Quiet => {}
                Status::Note(m) => theme::hint(ui, m),
                Status::Saved(m) => {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(m).color(theme::GOOD).size(12.0),
                        )
                        .wrap(),
                    );
                }
                Status::Failed(m) => {
                    ui.add(
                        egui::Label::new(egui::RichText::new(m).color(theme::BAD).size(12.0))
                            .wrap(),
                    );
                }
            }
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Saving writes the whole file, so comments added to it by hand are not \
                 kept. Unreadable lines are.",
            );
            theme::hint(
                ui,
                "The background service reads this file when it starts a watch, so restart \
                 the service for a change to take effect.",
            );
        });
    }
}

const HEADER: &str = "\
# logi per-application profile rules.
#
# Written by logi's rules editor. The format is unchanged and the file is
# still meant to be hand-editable: one `pattern = profile[,dpi]` per line,
# `#` starts a comment, and the FIRST matching rule wins, so order is
# precedence.
#
# The pattern is matched case-insensitively against each running process's
# name and its full command line. Matching is on whether a program is
# RUNNING, not whether it is focused: Wayland does not let an ordinary
# process see the active window.
#
# fallback = N   applies profile N when nothing matches (optional)
# poll_ms = N    how often to scan /proc (default 1000, minimum 100)
#
# Saving from the editor rewrites this file, so comments added here by hand
# are not preserved.

";

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pattern: &str, profile: u8, dpi: Option<u16>) -> Row {
        Row { pattern: pattern.to_string(), profile, dpi }
    }

    fn model(rows: Vec<Row>) -> Model {
        Model { rows, ..Model::default() }
    }

    fn errors(p: &[Problem]) -> Vec<&str> {
        p.iter()
            .filter(|x| x.severity == Severity::Error)
            .map(|x| x.text.as_str())
            .collect()
    }

    fn warnings(p: &[Problem]) -> Vec<&str> {
        p.iter()
            .filter(|x| x.severity == Severity::Warning)
            .map(|x| x.text.as_str())
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("logi-rules-editor-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ---- edit model -----------------------------------------------------

    #[test]
    fn add_appends_so_a_new_rule_cannot_pre_empt_an_existing_one() {
        let mut m = model(vec![row("csgo", 1, None)]);
        m.add("blender", 2);
        assert_eq!(m.rows[1].pattern, "blender");
    }

    #[test]
    fn moving_up_raises_precedence_and_stops_at_the_top() {
        let mut m = model(vec![row("a", 0, None), row("b", 1, None), row("c", 2, None)]);
        assert!(m.move_up(2));
        assert_eq!(
            m.rows.iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert!(!m.move_up(0), "the first rule has nowhere to go");
        assert!(!m.move_up(9), "out of range is a no-op, not a panic");
    }

    #[test]
    fn moving_down_lowers_precedence_and_stops_at_the_bottom() {
        let mut m = model(vec![row("a", 0, None), row("b", 1, None)]);
        assert!(m.move_down(0));
        assert_eq!(m.rows[0].pattern, "b");
        assert!(!m.move_down(1), "the last rule has nowhere to go");
        assert!(!m.move_down(9));
    }

    #[test]
    fn removing_shifts_the_rules_below_up() {
        let mut m = model(vec![row("a", 0, None), row("b", 1, None), row("c", 2, None)]);
        assert_eq!(m.remove(1).unwrap().pattern, "b");
        assert_eq!(
            m.rows.iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert!(m.remove(7).is_none());
    }

    #[test]
    fn normalizing_trims_patterns_and_lifts_the_poll_floor() {
        let m = Model { rows: vec![row("  csgo  ", 1, None)], poll_ms: 5, ..Model::default() };
        let n = m.normalized();
        assert_eq!(n.rows[0].pattern, "csgo");
        assert_eq!(n.poll_ms, POLL_FLOOR_MS);
    }

    // ---- validation -----------------------------------------------------

    #[test]
    fn a_profile_the_device_does_not_have_is_flagged() {
        let m = model(vec![row("csgo", 7, None)]);
        assert_eq!(warnings(&m.check(Some(5), None)).len(), 1);
        assert!(m.check(Some(8), None).is_empty(), "profile 7 of 8 is fine");
    }

    #[test]
    fn profiles_are_not_checked_with_no_device_attached() {
        let m = model(vec![row("csgo", 200, None)]);
        assert!(m.check(None, None).is_empty());
    }

    #[test]
    fn a_fallback_outside_the_profile_count_is_flagged() {
        let m = Model { fallback: Some(9), ..Model::default() };
        assert_eq!(m.check(Some(5), None).len(), 1);
    }

    #[test]
    fn patterns_that_would_break_the_file_are_errors() {
        let m = model(vec![
            row("a=b", 0, None),
            row("steam # beta", 0, None),
            row("fallback", 0, None),
            row("POLL_MS", 0, None),
        ]);
        assert_eq!(errors(&m.check(None, None)).len(), 4);
    }

    #[test]
    fn an_empty_pattern_is_a_warning_not_an_error() {
        let m = model(vec![row("   ", 0, None)]);
        let p = m.check(None, None);
        assert!(errors(&p).is_empty());
        assert_eq!(warnings(&p).len(), 1);
    }

    #[test]
    fn a_rule_shadowed_by_a_broader_one_above_it_is_flagged() {
        // the appwatch case: `steam` above `steamwebhelper` means the second
        // can never win.
        let m = model(vec![row("steam", 1, None), row("steamwebhelper", 2, None)]);
        let checked = m.check(None, None);
        let w = warnings(&checked);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("steam"), "{w:?}");
        // the other way round is fine
        let ok = model(vec![row("steamwebhelper", 2, None), row("steam", 1, None)]);
        assert!(ok.check(None, None).is_empty());
    }

    #[test]
    fn a_duplicated_pattern_is_flagged() {
        let m = model(vec![row("csgo", 1, None), row("CSGO", 2, None)]);
        assert_eq!(warnings(&m.check(None, None)).len(), 1);
    }

    #[test]
    fn a_dpi_the_sensor_cannot_reach_is_flagged() {
        let cap = Capability::Range { min: 100, max: 25600, step: 50 };
        let bad = model(vec![row("csgo", 0, Some(30_000))]);
        assert_eq!(warnings(&bad.check(None, Some(&cap))).len(), 1);
        let good = model(vec![row("csgo", 0, Some(800))]);
        assert!(good.check(None, Some(&cap)).is_empty());
    }

    // ---- round trip -----------------------------------------------------

    #[test]
    fn every_feature_of_the_format_survives_a_round_trip() {
        let src = "\
# a hand-written comment
csgo = 1, 800
blender=2
Factorio = 3
nonsense without an equals
fallback = 0
poll_ms = 500
";
        let (m, notes) = Model::from_text(src);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[0], row("csgo", 1, Some(800)));
        assert_eq!(m.rows[2], row("Factorio", 3, None));
        assert_eq!(m.fallback, Some(0));
        assert_eq!(m.poll_ms, 500);
        assert_eq!(m.kept.len(), 1, "the unreadable line is held, not dropped");
        assert_eq!(m.kept[0].raw, "nonsense without an equals");

        let text = round_trip(&m).expect("round trip");
        let (again, notes2) = Model::from_text(&text);
        assert!(notes2.is_empty(), "{notes2:?}");

        // Everything the user typed must survive. The diagnostic attached to a
        // kept line must NOT be compared: it names a line number, and
        // re-rendering moves the line, so the old number is stale by
        // definition. Both numbers are right for their own file.
        let norm = m.normalized();
        assert_eq!(again.rows, norm.rows, "rules changed across a round trip");
        assert_eq!(again.fallback, norm.fallback);
        assert_eq!(again.poll_ms, norm.poll_ms);
        let kept_raw: Vec<&str> = again.kept.iter().map(|k| k.raw.as_str()).collect();
        let norm_raw: Vec<&str> = norm.kept.iter().map(|k| k.raw.as_str()).collect();
        assert_eq!(kept_raw, norm_raw, "an unreadable line was lost or altered");

        // and it is still stable on a second pass
        assert_eq!(round_trip(&again).unwrap(), text);
    }

    #[test]
    fn a_line_with_only_a_bad_dpi_is_not_kept_twice() {
        // parse_rules still produces the rule, with no dpi, so holding the raw
        // line as well would duplicate the rule on the next save.
        let (m, notes) = Model::from_text("csgo = 1, abc\n");
        assert_eq!(m.rows.len(), 1);
        assert_eq!(m.rows[0].dpi, None);
        assert!(m.kept.is_empty());
        assert_eq!(notes.len(), 1, "but the user is told");
        let text = round_trip(&m).unwrap();
        assert_eq!(Model::from_text(&text).0.rows.len(), 1);
    }

    #[test]
    fn a_pattern_that_collides_with_a_setting_name_refuses_to_save() {
        let m = model(vec![row("fallback", 3, None)]);
        let e = round_trip(&m).expect_err("must not be written");
        assert!(e.starts_with("Not saved"), "{e}");
    }

    #[test]
    fn rendering_produces_a_file_the_parser_reads_cleanly() {
        let m = Model {
            rows: vec![row("csgo", 1, Some(800))],
            fallback: Some(2),
            poll_ms: 750,
            kept: Vec::new(),
        };
        let (_, problems) = appwatch::parse_rules(&m.render());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(m.render().starts_with('#'), "the header explains the format");
    }

    #[test]
    fn an_empty_model_renders_to_a_file_with_no_rules() {
        let (cfg, problems) = appwatch::parse_rules(&Model::default().render());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.fallback, None);
    }

    // ---- the process picker --------------------------------------------

    fn proc(comm: &str, cmdline: &str) -> Proc {
        Proc { comm: comm.to_string(), cmdline: cmdline.to_string() }
    }

    #[test]
    fn the_picker_drops_kernel_threads_and_logi_itself() {
        let ps = vec![
            proc("kworker/0:1", ""),
            proc("logi", "/usr/bin/logi"),
            proc("csgo_linux64", "/home/x/csgo_linux64 -novid"),
        ];
        let got = pick_list(&ps, "", "logi");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].comm, "csgo_linux64");
    }

    #[test]
    fn the_picker_collapses_repeats_and_counts_them() {
        let ps = vec![
            proc("chrome", "/opt/chrome --type=renderer"),
            proc("chrome", "/opt/chrome --type=gpu"),
            proc("blender", "/usr/bin/blender"),
        ];
        let got = pick_list(&ps, "", "");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].comm, "blender", "sorted by name");
        assert_eq!(got[1].count, 2);
    }

    #[test]
    fn the_picker_filters_on_the_name_and_the_command_line() {
        let ps = vec![
            proc("blender", "/usr/bin/blender"),
            proc("steam", "/home/x/.steam/steam.sh"),
        ];
        assert_eq!(pick_list(&ps, "BLEND", "").len(), 1);
        assert_eq!(pick_list(&ps, ".steam", "").len(), 1, "matches the command line");
        assert!(pick_list(&ps, "nothing here", "").is_empty());
    }

    // ---- load and save --------------------------------------------------

    #[test]
    fn save_then_load_gives_back_the_same_model() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("rules.conf");

        let mut ed = RulesEditor::with_path(path.clone());
        assert!(!ed.dirty(), "a fresh editor has nothing to save");
        ed.load();
        ed.model.add("csgo", 1);
        ed.model.rows[0].dpi = Some(800);
        ed.model.add("blender", 2);
        ed.model.fallback = Some(0);
        ed.model.poll_ms = 750;
        assert!(ed.dirty());
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        assert!(!ed.dirty(), "saving clears the dirty flag");

        let mut other = RulesEditor::with_path(path.clone());
        other.load();
        assert_eq!(other.model, ed.model);
        assert!(!other.dirty());

        // the file is text a human can still read and edit
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("csgo = 1, 800"), "{text}");
        assert!(text.contains("poll_ms = 750"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_creates_the_directory_and_leaves_no_temp_file() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested/rules.conf");
        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("csgo", 1);
        ed.save();
        assert!(path.exists(), "{:?}", ed.status);
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["rules.conf".to_string()], "temp file not cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_does_not_throw_away_unsaved_edits() {
        let dir = temp_dir("keepedits");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);
        ed.load();
        assert_eq!(ed.model.rows.len(), 2, "a second load must not revert the edit");

        ed.reload();
        assert_eq!(ed.model.rows.len(), 1, "reload is the explicit discard");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_loads_as_an_empty_editor_and_says_so() {
        let dir = temp_dir("missing");
        let mut ed = RulesEditor::with_path(dir.join("rules.conf"));
        ed.load();
        assert!(ed.model.rows.is_empty());
        assert!(matches!(ed.status, Status::Note(_)));
        assert!(!ed.dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_line_survives_a_save() {
        let dir = temp_dir("kept");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\nthis line is nonsense\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        assert_eq!(ed.model.kept.len(), 1);
        ed.model.add("blender", 2);
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("this line is nonsense"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_break_in_a_pattern_is_an_error() {
        // The TextEdit is single-line, but a paste can still carry one, and a
        // pattern written over two lines would be saved as two broken rules.
        let m = model(vec![row("csgo\nblender", 0, None)]);
        assert_eq!(errors(&m.check(None, None)).len(), 1);
        round_trip(&m).expect_err("and it must not be written either");
    }

    #[test]
    fn a_dpi_between_two_steps_is_flagged_as_the_value_it_will_become() {
        let cap = Capability::Range { min: 100, max: 25600, step: 50 };
        let m = model(vec![row("csgo", 0, Some(825))]);
        let checked = m.check(None, Some(&cap));
        let w = warnings(&checked);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("850"), "{w:?}");
    }

    #[test]
    fn a_poll_interval_past_the_slider_is_flagged_but_kept() {
        let m = Model { poll_ms: 30_000, ..Model::default() };
        assert_eq!(warnings(&m.check(None, None)).len(), 1);
        assert_eq!(m.normalized().poll_ms, 30_000, "advisory, not a clamp");
        assert!(m.render().contains("poll_ms = 30000"));
    }

    // ---- the paint pass -------------------------------------------------

    /// The one that actually bit: egui's `DragValue::range` and `Slider`
    /// write a clamped value straight back through the borrow they are given,
    /// so a panel whose ranges came from the attached device rewrote the
    /// user's file-loaded values on the first frame it was painted, with no
    /// click involved. Validation against the device has to be advisory.
    #[test]
    fn painting_the_panel_never_rewrites_a_value_from_the_file() {
        use std::cell::RefCell;

        let ed = RefCell::new(RulesEditor::with_path(PathBuf::from(
            "/nonexistent/logi-must-not-be-touched/rules.conf",
        )));
        {
            let mut e = ed.borrow_mut();
            e.model = Model {
                rows: vec![row("csgo", 7, Some(30_000))],
                fallback: Some(9),
                poll_ms: 30_000,
                kept: Vec::new(),
            };
            e.disk = e.model.clone();
            e.loaded_once = true;
        }

        // A mouse with three profiles: every value above is out of its range.
        let snap = Snapshot { profile_count: 3, active_profile: 1, ..Snapshot::default() };
        egui::__run_test_ui(|ui| {
            let mut e = ed.borrow_mut();
            let problems = e.model.check(Some(3), None);
            e.rules_section(ui, &problems, Some(3), &snap);
            e.settings_section(ui, &problems, Some(3));
        });

        let e = ed.borrow();
        assert_eq!(e.model.rows[0].profile, 7, "the rule's profile was rewritten");
        assert_eq!(e.model.rows[0].dpi, Some(30_000), "the rule's dpi was rewritten");
        assert_eq!(e.model.fallback, Some(9), "the fallback was rewritten");
        assert_eq!(e.model.poll_ms, 30_000, "the poll interval was rewritten");
        assert!(!e.dirty(), "painting must not make the editor think it has edits");
    }

    // ---- writing --------------------------------------------------------

    #[test]
    fn a_save_that_cannot_round_trip_leaves_the_file_exactly_as_it_was() {
        let dir = temp_dir("refuse");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.rows[0].pattern = "fallback".to_string();
        ed.save();
        assert!(matches!(ed.status, Status::Failed(_)), "{:?}", ed.status);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "csgo = 1\n",
            "a refused save must not have touched the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_keeps_the_mode_the_config_already_had() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the rename must not widen the file's mode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_into_somewhere_unwritable_is_reported_not_a_panic() {
        let dir = temp_dir("unwritable");
        // A file where the config's directory would have to be.
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "not a directory").unwrap();
        let mut ed = RulesEditor::with_path(blocker.join("rules.conf"));
        ed.load();
        ed.model.add("csgo", 1);
        ed.save();
        assert!(matches!(ed.status, Status::Failed(_)), "{:?}", ed.status);
        assert!(ed.dirty(), "the edit is still unsaved, so it is still dirty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_exists_but_will_not_read_is_never_treated_as_no_file() {
        // The one that would hurt: `read_to_string(..).ok()` turned a config
        // the process cannot read into `None`, which `adopt` reads as a fresh
        // start. The user saw "No rules file yet", added a rule, saved, and
        // the rules already in the file were gone.
        let dir = temp_dir("unreadable-file");
        let path = dir.join("rules.conf");
        // Not UTF-8, so the read fails with the file very much present.
        std::fs::write(&path, b"csgo = 1\n\xff\xfe not utf 8 \xff\n").unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        assert!(matches!(ed.status, Status::Failed(_)), "{:?}", ed.status);
        assert!(ed.unreadable.is_some(), "the failure has to block saving");

        // An edit on top of it still must not be written over the file.
        ed.model.add("blender", 2);
        assert!(ed.dirty());
        ed.save();
        assert!(matches!(ed.status, Status::Failed(_)), "{:?}", ed.status);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a save over a file we could not read must not have touched it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_becomes_readable_again_recovers_on_reload() {
        let dir = temp_dir("unreadable-recover");
        let path = dir.join("rules.conf");
        std::fs::write(&path, b"\xff\xfe not utf 8\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        assert!(ed.unreadable.is_some());

        std::fs::write(&path, "csgo = 1\n").unwrap();
        ed.reload();
        assert!(ed.unreadable.is_none(), "{:?}", ed.status);
        assert_eq!(ed.model.rows.len(), 1);

        ed.model.add("blender", 2);
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_over_a_file_changed_since_it_was_read_costs_a_second_press() {
        // `load` runs once, when the window opens. A file this panel calls
        // hand-editable can therefore have been rewritten in a text editor
        // hours ago, and the first Save must not throw those rules away
        // without saying so.
        let dir = temp_dir("stale");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);

        std::fs::write(&path, "factorio = 3\n").unwrap();
        ed.save();
        assert!(matches!(ed.status, Status::Note(_)), "{:?}", ed.status);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "factorio = 3\n",
            "the first Save must not have written anything"
        );
        assert!(ed.dirty(), "and the edit is still unsaved");

        // Pressing it again is the user saying to replace it anyway.
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("csgo = 1"), "{text}");
        assert!(text.contains("blender = 2"), "{text}");
        assert!(!text.contains("factorio"), "{text}");

        // And the next save is unremarkable again: the file is ours now.
        ed.model.add("doom", 0);
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reloading_instead_of_confirming_takes_the_version_on_disk() {
        let dir = temp_dir("stale-reload");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);
        std::fs::write(&path, "factorio = 3\n").unwrap();
        ed.save();
        assert!(matches!(ed.status, Status::Note(_)), "{:?}", ed.status);

        ed.reload();
        assert_eq!(ed.model.rows.len(), 1);
        assert_eq!(ed.model.rows[0].pattern, "factorio");
        // Reload took the file, so the armed confirmation has to be gone: the
        // next edit's Save must check the file again, not sail past it.
        ed.model.add("doom", 0);
        std::fs::write(&path, "quake = 2\n").unwrap();
        ed.save();
        assert!(matches!(ed.status, Status::Note(_)), "{:?}", ed.status);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "quake = 2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_is_refused_if_the_file_stopped_being_readable_after_the_load() {
        let dir = temp_dir("went-bad");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);

        std::fs::write(&path, b"\xff\xfe not utf 8\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        ed.save();
        assert!(matches!(ed.status, Status::Failed(_)), "{:?}", ed.status);
        assert!(ed.unreadable.is_some(), "and saving stays blocked");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a file we can no longer read must not be written over"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_stops_complaining_about_a_line_it_has_just_rewritten() {
        // `csgo = 1, abc` parses to a rule with no dpi and a note about the
        // dpi. Saving rewrites the line as `csgo = 1`, so the note is about
        // something no longer in the file and must not stay on screen.
        let dir = temp_dir("stale-note");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1, abc\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        assert_eq!(ed.notes.len(), 1, "{:?}", ed.notes);
        ed.model.add("blender", 2);
        ed.save();
        assert!(matches!(ed.status, Status::Saved(_)), "{:?}", ed.status);
        assert!(ed.notes.is_empty(), "{:?}", ed.notes);

        // and the file really is clean now
        let (_, problems) = appwatch::parse_rules(&std::fs::read_to_string(&path).unwrap());
        assert!(problems.is_empty(), "{problems:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_changed_underneath_an_edit_is_reported_rather_than_merged() {
        let dir = temp_dir("conflict");
        let path = dir.join("rules.conf");
        std::fs::write(&path, "csgo = 1\n").unwrap();

        let mut ed = RulesEditor::with_path(path.clone());
        ed.load();
        ed.model.add("blender", 2);
        std::fs::write(&path, "csgo = 1\nfactorio = 3\n").unwrap();
        ed.load();
        assert!(matches!(ed.status, Status::Note(_)), "{:?}", ed.status);
        assert_eq!(ed.model.rows.len(), 2, "the user's edit is still there");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
