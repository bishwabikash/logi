//! Per-application profile switching.
//!
//! Foreground-window detection is not available to an ordinary process on
//! Wayland: there is no equivalent of `_NET_ACTIVE_WINDOW`, and GNOME's shell
//! Eval interface is locked down outside developer mode. So this matches on
//! whether a process is *running*, read from /proc, which works on every
//! session type including a bare TTY.
//!
//! The practical consequence is that rules are "while this program is open",
//! not "while this program is focused". For games, which is what this is for,
//! that is usually the same thing.

use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

/// One rule: if a process matches, select this profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Matched case-insensitively as a substring of the executable name and
    /// of the full command line.
    pub pattern: String,
    pub profile: u8,
    pub dpi: Option<u16>,
}

impl Rule {
    pub fn new(pattern: impl Into<String>, profile: u8) -> Self {
        Rule { pattern: pattern.into(), profile, dpi: None }
    }

    fn matches(&self, comm: &str, cmdline: &str) -> bool {
        let p = self.pattern.to_ascii_lowercase();
        if p.is_empty() {
            return false;
        }
        comm.to_ascii_lowercase().contains(&p) || cmdline.to_ascii_lowercase().contains(&p)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub rules: Vec<Rule>,
    /// Applied when nothing matches. None leaves the profile alone.
    pub fallback: Option<u8>,
    pub poll: Duration,
    /// A rule must hold for this long before it is applied, so a short-lived
    /// process does not thrash the mouse's profile.
    pub settle: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rules: Vec::new(),
            fallback: None,
            poll: Duration::from_millis(1000),
            settle: Duration::from_millis(1500),
        }
    }
}

/// A running process, as much as /proc will tell us cheaply.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub comm: String,
    pub cmdline: String,
}

/// Every process this user can see. Unreadable entries are skipped: /proc is
/// racy by nature and a process exiting mid-scan is normal, not an error.
pub fn scan_processes() -> Vec<Proc> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // Bytes, not `read_to_string`: a process is free to put anything in
        // its name or arguments, and a single byte that is not UTF-8 made the
        // strict read fail and the process vanish from the scan, or lose its
        // command line, so a rule naming it never matched.
        let comm = fs::read(format!("/proc/{n}/comm"))
            .map(|b| proc_text(&b))
            .unwrap_or_default();
        if comm.is_empty() {
            continue;
        }
        let cmdline = fs::read(format!("/proc/{n}/cmdline"))
            .map(|b| proc_text(&b))
            .unwrap_or_default();
        out.push(Proc { comm, cmdline });
    }
    out
}

/// A /proc text file as one trimmed string: arguments are NUL-separated, and
/// bytes that are not UTF-8 are replaced rather than failing the whole read.
fn proc_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', " ").trim().to_string()
}

/// First matching rule wins, so order is precedence.
pub fn select<'a>(cfg: &'a Config, procs: &[Proc]) -> Option<&'a Rule> {
    cfg.rules
        .iter()
        .find(|r| procs.iter().any(|p| r.matches(&p.comm, &p.cmdline)))
}

/// What the watcher decided to do on one tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Switch to this profile, because of this rule pattern (or the fallback).
    Switch { profile: u8, dpi: Option<u16>, reason: String },
    /// A candidate is waiting out the settle window.
    Pending { profile: u8, reason: String },
    /// Nothing to do.
    Hold,
}

/// Debounced rule evaluation, with no device access and no clock of its own
/// so it can be driven deterministically in tests.
pub struct Watcher {
    cfg: Config,
    /// The profile the device was last told to select, and the dpi sent with
    /// it. The dpi is part of it because two rules can name the same profile
    /// and differ only in the dpi they carry; tracking the profile alone would
    /// leave the second rule's dpi unapplied.
    applied: Option<(u8, Option<u16>)>,
    /// The profile waiting out the settle window, and when it first appeared.
    /// Only the profile is held: what else the winning rule carries is read
    /// when the window closes, not when it opens.
    candidate: Option<(u8, Instant)>,
}

impl Watcher {
    pub fn new(cfg: Config) -> Self {
        Watcher { cfg, applied: None, candidate: None }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Feed a process list and the current time; get back what to do.
    pub fn tick(&mut self, procs: &[Proc], now: Instant) -> Decision {
        let (want, dpi, reason) = match select(&self.cfg, procs) {
            Some(r) => (r.profile, r.dpi, format!("{} is running", r.pattern)),
            None => match self.cfg.fallback {
                Some(p) => (p, None, "nothing matched, using the fallback".to_string()),
                None => {
                    self.candidate = None;
                    return Decision::Hold;
                }
            },
        };

        if self.applied == Some((want, dpi)) {
            self.candidate = None;
            return Decision::Hold;
        }

        // The settle window is about the profile, so a rule that keeps the
        // profile and changes only its dpi does not restart it. What gets
        // applied, and the reason logged for it, are whatever is in force when
        // the window closes: the rule that opened it may have stopped matching
        // in the meantime, and applying its dpi would be applying a rule that
        // is no longer true.
        match self.candidate {
            Some((p, since)) if p == want => {
                if now.duration_since(since) >= self.cfg.settle {
                    self.applied = Some((want, dpi));
                    self.candidate = None;
                    Decision::Switch { profile: want, dpi, reason }
                } else {
                    Decision::Pending { profile: want, reason }
                }
            }
            // No candidate, or a different one: start the window here.
            _ => {
                self.candidate = Some((want, now));
                Decision::Pending { profile: want, reason }
            }
        }
    }

    /// Note that something else changed the profile, so the next match
    /// re-applies rather than being suppressed as already-applied.
    pub fn invalidate(&mut self) {
        self.applied = None;
    }
}

/// Parse rules from the simple config format: one `pattern = profile[,dpi]`
/// per line, `#` comments, and `fallback = N`.
pub fn parse_rules(text: &str) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut problems = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            problems.push(format!("line {}: expected `pattern = profile`", n + 1));
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        if key.eq_ignore_ascii_case("fallback") {
            match val.parse::<u8>() {
                Ok(p) => cfg.fallback = Some(p),
                Err(_) => problems.push(format!("line {}: bad fallback {val:?}", n + 1)),
            }
            continue;
        }
        if key.eq_ignore_ascii_case("poll_ms") {
            match val.parse::<u64>() {
                Ok(ms) => cfg.poll = Duration::from_millis(ms.max(100)),
                Err(_) => problems.push(format!("line {}: bad poll_ms {val:?}", n + 1)),
            }
            continue;
        }
        let mut parts = val.split(',');
        let prof = parts.next().unwrap_or("").trim();
        let Ok(profile) = prof.parse::<u8>() else {
            problems.push(format!("line {}: bad profile {prof:?}", n + 1));
            continue;
        };
        let dpi = match parts.next().map(|d| d.trim().parse::<u16>()) {
            Some(Ok(d)) => Some(d),
            Some(Err(_)) => {
                problems.push(format!("line {}: bad dpi", n + 1));
                None
            }
            None => None,
        };
        cfg.rules.push(Rule { pattern: key.to_string(), profile, dpi });
    }
    (cfg, problems)
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base).join("logi/rules.conf")
}

pub const EXAMPLE_RULES: &str = "\
# logi per-application profile rules.
#
# One `pattern = profile[,dpi]` per line. The pattern is matched
# case-insensitively against each running process's name and command line, and
# the FIRST matching rule wins, so put the specific ones first.
#
# Matching is on whether a program is RUNNING, not whether it is focused:
# Wayland does not let an ordinary process see the active window.
#
# fallback = N   applies profile N when nothing matches (optional)
# poll_ms = N    how often to scan /proc (default 1000, minimum 100)

# csgo       = 1, 800
# blender    = 2
# fallback   = 0
";

/// Describe what a tick decided, for logging.
pub fn describe(d: &Decision) -> Option<String> {
    match d {
        Decision::Switch { profile, dpi, reason } => Some(match dpi {
            Some(v) => format!("profile {profile} + {v} dpi ({reason})"),
            None => format!("profile {profile} ({reason})"),
        }),
        Decision::Pending { .. } | Decision::Hold => None,
    }
}

/// Count how many distinct profiles the rules reference, for validation
/// against the device's profile count.
pub fn referenced_profiles(cfg: &Config) -> Vec<u8> {
    let mut seen: HashMap<u8, ()> = HashMap::new();
    for r in &cfg.rules {
        seen.insert(r.profile, ());
    }
    if let Some(f) = cfg.fallback {
        seen.insert(f, ());
    }
    let mut v: Vec<u8> = seen.into_keys().collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs(names: &[&str]) -> Vec<Proc> {
        names
            .iter()
            .map(|n| Proc { comm: n.to_string(), cmdline: format!("/usr/bin/{n}") })
            .collect()
    }

    fn cfg(rules: Vec<Rule>, fallback: Option<u8>) -> Config {
        Config { rules, fallback, settle: Duration::from_millis(100), ..Default::default() }
    }

    #[test]
    fn first_matching_rule_wins() {
        let c = cfg(vec![Rule::new("steam", 1), Rule::new("steamwebhelper", 2)], None);
        let r = select(&c, &procs(&["steamwebhelper"])).unwrap();
        assert_eq!(r.profile, 1, "order is precedence");
    }

    #[test]
    fn matching_is_case_insensitive_and_substring() {
        let c = cfg(vec![Rule::new("BLENDER", 3)], None);
        assert!(select(&c, &procs(&["blender-4.2"])).is_some());
    }

    #[test]
    fn empty_pattern_never_matches() {
        let c = cfg(vec![Rule::new("", 1)], None);
        assert!(select(&c, &procs(&["anything"])).is_none());
    }

    #[test]
    fn no_match_and_no_fallback_holds() {
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], None));
        assert_eq!(w.tick(&procs(&["bash"]), Instant::now()), Decision::Hold);
    }

    #[test]
    fn switch_waits_for_the_settle_window() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], None));
        let p = procs(&["csgo"]);
        assert!(matches!(w.tick(&p, t), Decision::Pending { profile: 1, .. }));
        // too soon
        assert!(matches!(w.tick(&p, t + Duration::from_millis(50)), Decision::Pending { .. }));
        // settled
        assert!(matches!(
            w.tick(&p, t + Duration::from_millis(150)),
            Decision::Switch { profile: 1, .. }
        ));
    }

    #[test]
    fn an_applied_profile_is_not_reapplied() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], None));
        let p = procs(&["csgo"]);
        w.tick(&p, t);
        w.tick(&p, t + Duration::from_millis(150));
        assert_eq!(w.tick(&p, t + Duration::from_millis(300)), Decision::Hold);
    }

    #[test]
    fn a_brief_blip_never_switches() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], None));
        // appears, then is gone before the settle window elapses
        assert!(matches!(w.tick(&procs(&["csgo"]), t), Decision::Pending { .. }));
        assert_eq!(w.tick(&procs(&["bash"]), t + Duration::from_millis(50)), Decision::Hold);
        // and the pending candidate is forgotten
        assert_eq!(w.tick(&procs(&["bash"]), t + Duration::from_millis(500)), Decision::Hold);
    }

    #[test]
    fn a_new_candidate_restarts_the_window() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("a", 1), Rule::new("b", 2)], None));
        assert!(matches!(w.tick(&procs(&["a"]), t), Decision::Pending { profile: 1, .. }));
        // b replaces the pending a, so the window restarts rather than the
        // old elapsed time carrying over
        assert!(matches!(
            w.tick(&procs(&["b"]), t + Duration::from_millis(80)),
            Decision::Pending { profile: 2, .. }
        ));
        assert!(matches!(
            w.tick(&procs(&["b"]), t + Duration::from_millis(120)),
            Decision::Pending { .. }
        ));
        assert!(matches!(
            w.tick(&procs(&["b"]), t + Duration::from_millis(200)),
            Decision::Switch { profile: 2, .. }
        ));
    }

    #[test]
    fn fallback_applies_when_nothing_matches() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], Some(0)));
        assert!(matches!(w.tick(&procs(&["bash"]), t), Decision::Pending { profile: 0, .. }));
        assert!(matches!(
            w.tick(&procs(&["bash"]), t + Duration::from_millis(150)),
            Decision::Switch { profile: 0, .. }
        ));
    }

    #[test]
    fn rule_carries_a_dpi() {
        let t = Instant::now();
        let mut r = Rule::new("csgo", 1);
        r.dpi = Some(800);
        let mut w = Watcher::new(cfg(vec![r], None));
        let p = procs(&["csgo"]);
        w.tick(&p, t);
        assert_eq!(
            w.tick(&p, t + Duration::from_millis(150)),
            Decision::Switch { profile: 1, dpi: Some(800), reason: "csgo is running".into() }
        );
    }

    #[test]
    fn the_rule_in_force_when_the_window_closes_is_the_one_applied() {
        // Two rules naming the same profile: the first opens the settle
        // window, the second is what is true when it closes. Applying the
        // first rule's dpi would be applying a rule that stopped matching.
        let t = Instant::now();
        let mut a = Rule::new("csgo", 1);
        a.dpi = Some(800);
        let mut b = Rule::new("blender", 1);
        b.dpi = Some(1600);
        let mut w = Watcher::new(cfg(vec![a, b], None));
        assert!(matches!(w.tick(&procs(&["csgo"]), t), Decision::Pending { profile: 1, .. }));
        assert_eq!(
            w.tick(&procs(&["blender"]), t + Duration::from_millis(150)),
            Decision::Switch {
                profile: 1,
                dpi: Some(1600),
                reason: "blender is running".into()
            },
            "the settled switch carried the rule that had already stopped matching"
        );
    }

    #[test]
    fn a_same_profile_rule_with_a_different_dpi_still_applies() {
        // The profile does not change, so nothing would be re-selected if the
        // watcher tracked the profile alone, and the dpi would never be sent.
        let t = Instant::now();
        let mut a = Rule::new("csgo", 1);
        a.dpi = Some(800);
        let mut b = Rule::new("blender", 1);
        b.dpi = Some(1600);
        let mut w = Watcher::new(cfg(vec![a, b], None));
        let csgo = procs(&["csgo"]);
        w.tick(&csgo, t);
        assert!(matches!(
            w.tick(&csgo, t + Duration::from_millis(150)),
            Decision::Switch { dpi: Some(800), .. }
        ));
        let blender = procs(&["blender"]);
        assert!(matches!(
            w.tick(&blender, t + Duration::from_millis(200)),
            Decision::Pending { profile: 1, .. }
        ));
        assert!(matches!(
            w.tick(&blender, t + Duration::from_millis(400)),
            Decision::Switch { profile: 1, dpi: Some(1600), .. }
        ));
    }

    #[test]
    fn an_unchanged_rule_is_not_reapplied_for_its_dpi() {
        let t = Instant::now();
        let mut r = Rule::new("csgo", 1);
        r.dpi = Some(800);
        let mut w = Watcher::new(cfg(vec![r], None));
        let p = procs(&["csgo"]);
        w.tick(&p, t);
        w.tick(&p, t + Duration::from_millis(150));
        assert_eq!(w.tick(&p, t + Duration::from_millis(300)), Decision::Hold);
        assert_eq!(w.tick(&p, t + Duration::from_millis(900)), Decision::Hold);
    }

    #[test]
    fn invalidate_lets_the_same_rule_apply_again() {
        let t = Instant::now();
        let mut w = Watcher::new(cfg(vec![Rule::new("csgo", 1)], None));
        let p = procs(&["csgo"]);
        w.tick(&p, t);
        assert!(matches!(
            w.tick(&p, t + Duration::from_millis(150)),
            Decision::Switch { profile: 1, .. }
        ));
        w.invalidate();
        assert!(matches!(
            w.tick(&p, t + Duration::from_millis(200)),
            Decision::Pending { profile: 1, .. }
        ));
        assert!(matches!(
            w.tick(&p, t + Duration::from_millis(400)),
            Decision::Switch { profile: 1, .. }
        ));
    }

    #[test]
    fn parse_rules_reads_profiles_dpi_and_fallback() {
        let (c, probs) = parse_rules("csgo = 1, 800\nblender=2\nfallback = 0\npoll_ms = 500\n");
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].pattern, "csgo");
        assert_eq!(c.rules[0].dpi, Some(800));
        assert_eq!(c.rules[1].profile, 2);
        assert_eq!(c.fallback, Some(0));
        assert_eq!(c.poll, Duration::from_millis(500));
    }

    #[test]
    fn parse_rules_skips_comments_and_blanks() {
        let (c, probs) = parse_rules("# a comment\n\n  \ncsgo = 1  # trailing\n");
        assert!(probs.is_empty());
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.rules[0].profile, 1);
    }

    #[test]
    fn parse_rules_reports_bad_lines_without_dropping_good_ones() {
        let (c, probs) = parse_rules("good = 1\nnonsense\nalso_bad = xyz\nfine = 2\n");
        assert_eq!(c.rules.len(), 2);
        assert_eq!(probs.len(), 2);
    }

    #[test]
    fn poll_interval_has_a_floor() {
        let (c, _) = parse_rules("poll_ms = 1\n");
        assert_eq!(c.poll, Duration::from_millis(100));
    }

    #[test]
    fn referenced_profiles_is_sorted_and_deduped() {
        let (c, _) = parse_rules("a = 2\nb = 1\nc = 2\nfallback = 0\n");
        assert_eq!(referenced_profiles(&c), vec![0, 1, 2]);
    }

    #[test]
    fn scanning_proc_finds_this_process() {
        // `/proc/self/comm` is the process's name, not the calling thread's,
        // so this is the same string the scan reads back for our own pid.
        let me = fs::read_to_string("/proc/self/comm")
            .expect("/proc/self/comm")
            .trim()
            .to_string();
        let ps = scan_processes();
        assert!(!ps.is_empty(), "/proc scan returned nothing");
        // Asserting only that some entry had a non-empty `comm` proved
        // nothing: `scan_processes` skips the ones that do not, so that held
        // however much of /proc the scan had missed.
        assert!(
            ps.iter().any(|p| p.comm == me),
            "the scan did not find this process ({me:?})"
        );
    }

    #[test]
    fn proc_text_survives_bytes_that_are_not_utf8() {
        assert_eq!(proc_text(b"game\0--map\0one\0"), "game --map one");
        assert_eq!(proc_text(b"game\n"), "game");
        let t = proc_text(b"wine\0C:\\caf\xe9\\game.exe\0");
        assert!(t.starts_with("wine C:\\caf"), "{t:?}");
        assert!(t.ends_with("\\game.exe"), "{t:?}");
    }

    #[test]
    fn a_scanned_process_is_matchable() {
        // The two halves have to fit: whatever `scan_processes` puts in `comm`
        // is what a rule's pattern is tested against.
        let ps = scan_processes();
        let me = fs::read_to_string("/proc/self/comm")
            .expect("/proc/self/comm")
            .trim()
            .to_string();
        let c = cfg(vec![Rule::new(me.to_ascii_uppercase(), 1)], None);
        assert!(
            select(&c, &ps).is_some(),
            "a rule naming a running process did not match it"
        );
    }
}
