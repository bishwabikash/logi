//! What a bound button does, and how bindings are written down.
//!
//! Three things live here: the action model, the config file's syntax, and
//! the detached spawn behind a `command` action. Deliberately no device and no
//! uinput access, so all of it is unit-testable and so each dangerous piece
//! stays with the module that owns it: synthesising input belongs to uinput,
//! and the 0x8110 remapping table — which can leave every button on the mouse
//! unmapped, see PROTOCOL.md — belongs to the module that talks to the device.
//! This module only says which buttons need silencing, via `to_silence`.
//!
//! Key names are not resolved here either. A `Keymap` is passed in, so parsing
//! a config file does not drag in the uinput keycode tables and the tests can
//! use four made-up keys.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// The spy event bitmap is a u16, so there cannot be a seventeenth button.
pub const MAX_BUTTON: u8 = 16;

/// Key-name lookup, implemented by uinput. Split out as a trait so this
/// module neither depends on that one nor hardcodes a second copy of the
/// kernel's key table.
pub trait Keymap {
    fn code(&self, name: &str) -> Option<u16>;
    /// Some only for codes the host side can actually emit. Parsing leans on
    /// that: a raw numeric code the keymap cannot name is refused, because the
    /// virtual device never registers it and the binding would do nothing.
    fn name(&self, code: u16) -> Option<String>;
}

/// Adapter for the plain functions uinput exposes:
/// `FnKeymap::new(uinput::key_code, |c| uinput::key_name(c).map(Into::into))`.
pub struct FnKeymap<C, N> {
    code: C,
    name: N,
}

impl<C, N> FnKeymap<C, N>
where
    C: Fn(&str) -> Option<u16>,
    N: Fn(u16) -> Option<String>,
{
    pub fn new(code: C, name: N) -> Self {
        FnKeymap { code, name }
    }
}

impl<C, N> Keymap for FnKeymap<C, N>
where
    C: Fn(&str) -> Option<u16>,
    N: Fn(u16) -> Option<String>,
{
    fn code(&self, name: &str) -> Option<u16> {
        (self.code)(name)
    }

    fn name(&self, code: u16) -> Option<String> {
        (self.name)(code)
    }
}

/// Short modifier names mapped to the kernel's own. Accepted on input and
/// preferred on output, because `ctrl+c` is what people write.
const MOD_ALIASES: &[(&str, &str)] = &[
    ("ctrl", "leftctrl"),
    ("control", "leftctrl"),
    ("shift", "leftshift"),
    ("alt", "leftalt"),
    ("super", "leftmeta"),
    ("meta", "leftmeta"),
    ("win", "leftmeta"),
];

/// Linux BTN_* codes, which are not keys and so are not the keymap's business.
pub const MOUSE_BUTTONS: &[(&str, u16)] = &[
    ("left", 0x110),
    ("right", 0x111),
    ("middle", 0x112),
    ("side", 0x113),
    ("extra", 0x114),
    ("forward", 0x115),
    ("back", 0x116),
    ("task", 0x117),
];

fn canonical(name: &str) -> &str {
    MOD_ALIASES
        .iter()
        .find(|(short, _)| short.eq_ignore_ascii_case(name))
        .map(|(_, long)| *long)
        .unwrap_or(name)
}

/// A name, or a raw decimal code.
///
/// The numeric form is only accepted for a code the keymap can name back, so
/// that every code this returns is one the host can emit. Taking any number
/// would turn `key 999` into a binding that parses, loads, and then silently
/// does nothing on every press.
fn key_code(keys: &dyn Keymap, name: &str) -> Option<u16> {
    if let Some(c) = keys.code(canonical(name)) {
        return Some(c);
    }
    let c = name.parse::<u16>().ok()?;
    keys.name(c).map(|_| c)
}

fn key_label(keys: &dyn Keymap, code: u16, short: bool) -> String {
    if short {
        if let Some((s, _)) = MOD_ALIASES.iter().find(|(_, long)| keys.code(long) == Some(code)) {
            return (*s).to_string();
        }
    }
    keys.name(code).unwrap_or_else(|| code.to_string())
}

fn mouse_label(code: u16) -> String {
    MOUSE_BUTTONS
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(n, _)| (*n).to_string())
        .unwrap_or_else(|| code.to_string())
}

fn unknown_key(name: &str) -> String {
    format!("unknown key name {name:?}; want something like ctrl+c, f5 or volumeup")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A key, optionally with modifiers: "ctrl+c", "f5", "volumeup".
    Key { mods: Vec<u16>, code: u16 },
    /// Type a literal string. One line: the config file is line-based and has
    /// no escape syntax, so a newline here cannot be written back out.
    Text(String),
    /// Another mouse button, by Linux BTN_* code.
    MouseButton(u16),
    /// Run a command, detached, without blocking the caller. One line, for the
    /// reason given on `Text`.
    Command(String),
    /// Explicitly do nothing. Useful with Replace to disable a button.
    Nothing,
}

impl Action {
    /// Parse one action: `key ctrl+c`, `text Hello`, `mouse middle`,
    /// `command notify-send hi`, or `nothing`.
    pub fn parse(s: &str, keys: &dyn Keymap) -> std::result::Result<Action, String> {
        let t = s.trim();
        let (word, rest) = match t.split_once(char::is_whitespace) {
            Some((w, r)) => (w, r.trim()),
            None => (t, ""),
        };
        // `text` and `command` take the rest of the line verbatim; the actions
        // with a fixed grammar get a trailing comment cut off them instead.
        // `strip_comment` says why the line is drawn here.
        let bare = strip_comment(rest).trim();
        match word.to_ascii_lowercase().as_str() {
            "key" => {
                if bare.is_empty() {
                    return Err("`key` needs a key name, like ctrl+c".into());
                }
                let parts: Vec<&str> = bare.split('+').map(str::trim).collect();
                if parts.iter().any(|p| p.is_empty()) {
                    return Err(format!("bad key {bare:?}: empty name beside a +"));
                }
                let (last, mods) = parts.split_last().unwrap();
                let mut codes = Vec::with_capacity(mods.len());
                for m in mods {
                    codes.push(key_code(keys, m).ok_or_else(|| unknown_key(m))?);
                }
                let code = key_code(keys, last).ok_or_else(|| unknown_key(last))?;
                Ok(Action::Key { mods: codes, code })
            }
            "text" => {
                if rest.is_empty() {
                    return Err("`text` needs something to type".into());
                }
                Ok(Action::Text(rest.to_string()))
            }
            "mouse" => {
                let code = MOUSE_BUTTONS
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(bare))
                    .map(|(_, c)| *c)
                    .or_else(|| bare.parse::<u16>().ok())
                    .ok_or_else(|| {
                        let names: Vec<&str> = MOUSE_BUTTONS.iter().map(|(n, _)| *n).collect();
                        format!("unknown mouse button {bare:?}; want one of {}", names.join(" "))
                    })?;
                // The names above are resolved without the keymap, so the code
                // is checked against it: a binding the host cannot press would
                // leave a Replace-bound button silenced and doing nothing,
                // which is the one failure worth being strict about.
                if keys.name(code).is_none() {
                    return Err(format!(
                        "mouse button {bare:?} (code {code}) is not one the host can press"
                    ));
                }
                Ok(Action::MouseButton(code))
            }
            "command" => {
                if rest.is_empty() {
                    return Err("`command` needs something to run".into());
                }
                Ok(Action::Command(rest.to_string()))
            }
            "nothing" => {
                if bare.is_empty() {
                    Ok(Action::Nothing)
                } else {
                    Err(format!("`nothing` takes no argument, got {bare:?}"))
                }
            }
            "" => Err("expected an action: key, text, mouse, command or nothing".into()),
            other => Err(format!(
                "unknown action {other:?}; want key, text, mouse, command or nothing"
            )),
        }
    }

    /// The config syntax for this action; `parse` reads back what this writes.
    pub fn describe(&self, keys: &dyn Keymap) -> String {
        match self {
            Action::Key { mods, code } => {
                let mut s = String::from("key ");
                for m in mods {
                    s.push_str(&key_label(keys, *m, true));
                    s.push('+');
                }
                s.push_str(&key_label(keys, *code, false));
                s
            }
            Action::Text(t) => format!("text {t}"),
            Action::MouseButton(c) => format!("mouse {}", mouse_label(*c)),
            Action::Command(c) => format!("command {c}"),
            Action::Nothing => "nothing".to_string(),
        }
    }
}

/// Whether the physical button keeps working alongside the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindMode {
    Augment,
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub button: u8,
    pub action: Action,
    pub mode: BindMode,
}

#[derive(Debug, Clone, Default)]
pub struct Bindings {
    pub list: Vec<Binding>,
}

impl Bindings {
    pub fn get(&self, button: u8) -> Option<&Binding> {
        self.list.iter().find(|b| b.button == button)
    }

    /// Buttons that must be silenced in the device's remapping table.
    ///
    /// Only Replace bindings: augmenting a button must never take it away.
    ///
    /// The dangerous half belongs to the caller. Silence a button only once
    /// the host side is known to work — a silenced button with nothing
    /// listening is a dead button, and it stays dead across unplugging — and
    /// check these numbers against the device's own button count, not
    /// `MAX_BUTTON`, which is only the event bitmap's ceiling.
    pub fn to_silence(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self
            .list
            .iter()
            .filter(|b| b.mode == BindMode::Replace)
            .map(|b| b.button)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Parse the config file. Bad lines become problems and are skipped; the
    /// good ones still load, because a typo in one binding is no reason to
    /// leave the rest of someone's mouse unconfigured.
    pub fn parse(text: &str, keys: &dyn Keymap) -> (Bindings, Vec<String>) {
        let mut out = Bindings::default();
        let mut problems = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            // Only a whole-line comment is dropped here. A `#` further along
            // is the action's business, because a `text` or `command` value
            // owns the rest of the line.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((lhs, rhs)) = line.split_once('=') else {
                problems.push(format!("line {}: expected `button N = action`", n + 1));
                continue;
            };
            let (button, mode) = match parse_target(lhs) {
                Ok(v) => v,
                Err(e) => {
                    problems.push(format!("line {}: {e}", n + 1));
                    continue;
                }
            };
            let action = match Action::parse(rhs, keys) {
                Ok(a) => a,
                Err(e) => {
                    problems.push(format!("line {}: {e}", n + 1));
                    continue;
                }
            };
            if let Some(pos) = out.list.iter().position(|b| b.button == button) {
                problems.push(format!(
                    "line {}: button {button} was already bound; this line wins",
                    n + 1
                ));
                out.list[pos] = Binding { button, action, mode };
            } else {
                out.list.push(Binding { button, action, mode });
            }
        }
        (out, problems)
    }

    /// Write the config file back out; `parse` reads back what this writes.
    pub fn render(&self, keys: &dyn Keymap) -> String {
        let mut s = String::new();
        for b in &self.list {
            let mode = match b.mode {
                BindMode::Replace => " replace",
                BindMode::Augment => "",
            };
            s.push_str(&format!(
                "button {}{} = {}\n",
                b.button,
                mode,
                b.action.describe(keys)
            ));
        }
        s
    }
}

/// Cut a trailing `#` comment off an action that has a fixed grammar.
///
/// Deliberately not applied to a `text` or `command` value. There is no escape
/// syntax, so either `#` is a comment marker everywhere — and `text see #5`
/// quietly loses half its payload, while `text #5` loses the whole binding —
/// or a free-form value runs to the end of the line and a comment needs a line
/// of its own. The second loses nothing and keeps `render` an exact inverse of
/// `parse`, which matters once anything but a human writes the file.
///
/// The marker still has to follow whitespace, so `mouse 4#2` stays a bad
/// number rather than becoming a valid 4.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// The left-hand side: `button 10`, `button 10 replace`, or just `10`.
fn parse_target(lhs: &str) -> std::result::Result<(u8, BindMode), String> {
    let mut words = lhs.split_whitespace().peekable();
    if words.peek().map(|w| w.eq_ignore_ascii_case("button")) == Some(true) {
        words.next();
    }
    let Some(num) = words.next() else {
        return Err("expected a button number".into());
    };
    let button = num
        .parse::<u8>()
        .map_err(|_| format!("bad button number {num:?}"))?;
    if button == 0 || button > MAX_BUTTON {
        return Err(format!("button {button} is out of range (1..={MAX_BUTTON})"));
    }
    let mode = match words.next() {
        None => BindMode::Augment,
        Some(w) if w.eq_ignore_ascii_case("replace") => BindMode::Replace,
        Some(w) if w.eq_ignore_ascii_case("augment") => BindMode::Augment,
        Some(w) => return Err(format!("expected `replace` or `augment`, got {w:?}")),
    };
    if let Some(extra) = words.next() {
        return Err(format!("unexpected {extra:?} after the button"));
    }
    Ok((button, mode))
}

/// Metacharacters that mean the string wants a shell rather than a bare exec.
const SHELL_CHARS: &[char] = &[
    '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', '{', '}', '~',
    '\n',
];

/// Whether this command string has to go through `/bin/sh`.
pub fn needs_shell(command: &str) -> bool {
    command.contains(SHELL_CHARS)
}

/// How a `command` action is executed.
///
/// A plain `notify-send hi` is exec'd directly, with no shell in the way. A
/// string using quoting, a pipe, a redirect, a glob or `$VAR` genuinely needs
/// one, so it goes to `/bin/sh -c` verbatim. That makes the bindings file as
/// trusted as the user's own shell profile: anything that can write it can
/// already run commands as them.
pub fn argv(command: &str) -> Vec<String> {
    if needs_shell(command) {
        vec!["/bin/sh".into(), "-c".into(), command.to_string()]
    } else {
        command.split_whitespace().map(String::from).collect()
    }
}

/// Run a `command` action, detached.
///
/// Two things to avoid: stalling the event loop on a button press, and piling
/// up defunct children when a button is pressed fifty times. So the child gets
/// its own process group (a Ctrl+C aimed at us does not reach it) and /dev/null
/// for stdio, and a throwaway thread takes ownership of the `Child` purely to
/// `wait` on it. The thread blocks, never the caller, and the reap happens
/// whenever the command finishes, so nothing becomes a zombie. A double fork
/// would also work, but this needs no unsafe code.
///
/// `Builder`, not `thread::spawn`, which panics when the process cannot make
/// another thread: a button press must not be able to kill the driver. If the
/// reaper will not start, the error is returned and that one child is left for
/// process exit to collect.
pub fn spawn_detached(command: &str) -> std::io::Result<()> {
    let args = argv(command);
    let Some((exe, rest)) = args.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty command",
        ));
    };
    let child = Command::new(exe)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    // The reaper only ever sits in wait(), so it has no use for the default
    // 8 MiB of stack apiece.
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        })?;
    Ok(())
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base).join("logi/bindings.conf")
}

pub const EXAMPLE: &str = "\
# logi button bindings.
#
# One `button N [replace] = ACTION` per line. A line starting with `#` is a
# comment, and so is a `#` after the action on a `key`, `mouse` or `nothing`
# line; a `text` or `command` value instead runs to the end of the line, so
# give those a comment line of their own rather than a trailing one.
#
# Button numbers are the device's own, counting from 1. Press each button
# while watching button events to see which is which.
#
# ACTION is one of:
#   key ctrl+c              a key, with modifiers (ctrl shift alt super).
#                           Any name the driver knows; a raw code is taken
#                           only if it is one it can actually press.
#   text Hello there        type a literal string
#   mouse middle            another mouse button: left right middle side
#                           extra forward back task, or a raw BTN_ code
#   command notify-send hi  run a program, detached from this one
#   nothing                 do nothing
#
# A binding AUGMENTS the button by default: the mouse's own click still gets
# through and the action happens as well. Writing `replace` instead silences
# the physical button, which needs the device's remapping table; that is
# opt-in because a silenced button stays silenced in the firmware, across
# unplugging, until something puts the table back. `replace = nothing` is how
# you disable a button outright.
#
# A `command` runs through /bin/sh only when it uses quoting, a pipe, a
# redirect, a glob or $VAR. Either way this file is as trusted as ~/.profile.

# button 10 = key ctrl+c
# button 9  = text Hello
# button 8  replace = command notify-send \"button 8\"
# button 7  = mouse middle
# button 6  replace = nothing
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Four keys and two modifiers is enough to test the syntax without
    /// pulling in uinput's tables.
    struct FakeKeys;

    const FAKE: &[(&str, u16)] = &[
        ("a", 30),
        ("c", 46),
        ("f5", 63),
        ("volumeup", 115),
        ("leftctrl", 29),
        ("leftshift", 42),
        // uinput's table carries the BTN_ range too, so `mouse 274` has to
        // have somewhere to resolve.
        ("btn_middle", 0x112),
    ];

    impl Keymap for FakeKeys {
        fn code(&self, name: &str) -> Option<u16> {
            FAKE.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
        }

        fn name(&self, code: u16) -> Option<String> {
            FAKE.iter().find(|(_, c)| *c == code).map(|(n, _)| (*n).to_string())
        }
    }

    fn round_trip(src: &str) -> Action {
        let a = Action::parse(src, &FakeKeys).expect(src);
        let back = a.describe(&FakeKeys);
        let b = Action::parse(&back, &FakeKeys).expect(&back);
        assert_eq!(a, b, "{src:?} described as {back:?}");
        a
    }

    #[test]
    fn every_action_round_trips() {
        assert_eq!(round_trip("key ctrl+c"), Action::Key { mods: vec![29], code: 46 });
        assert_eq!(round_trip("key f5"), Action::Key { mods: vec![], code: 63 });
        assert_eq!(
            round_trip("key ctrl+shift+a"),
            Action::Key { mods: vec![29, 42], code: 30 }
        );
        assert_eq!(round_trip("text Hello there"), Action::Text("Hello there".into()));
        assert_eq!(round_trip("mouse middle"), Action::MouseButton(0x112));
        assert_eq!(
            round_trip("command notify-send hi"),
            Action::Command("notify-send hi".into())
        );
        assert_eq!(round_trip("nothing"), Action::Nothing);
    }

    #[test]
    fn describe_prefers_short_modifier_names() {
        let a = Action::parse("key control+c", &FakeKeys).unwrap();
        assert_eq!(a.describe(&FakeKeys), "key ctrl+c");
    }

    #[test]
    fn a_raw_code_is_taken_only_when_the_keymap_knows_it() {
        // A code the keymap names is usable, and describe prints the name.
        let a = Action::parse("key 63", &FakeKeys).unwrap();
        assert_eq!(a, Action::Key { mods: vec![], code: 63 });
        assert_eq!(a.describe(&FakeKeys), "key f5");
        assert_eq!(round_trip("mouse 274"), Action::MouseButton(0x112));

        // A code nothing can press has to fail here, where the error is
        // printed, rather than silently on every press for ever after.
        // `mouse side` is a known name, but the fake keymap cannot press it,
        // so it is refused for the same reason.
        for bad in ["key 999", "key ctrl+999", "mouse 300", "mouse side"] {
            assert!(Action::parse(bad, &FakeKeys).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn a_free_form_value_keeps_everything_after_a_hash() {
        let src = "button 9 = text see # 5\nbutton 8 = command sed s/#// a # b\n";
        let (b, probs) = Bindings::parse(src, &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(9).unwrap().action, Action::Text("see # 5".into()));
        assert_eq!(
            b.get(8).unwrap().action,
            Action::Command("sed s/#// a # b".into())
        );
        // The whole point: what render writes, parse reads back unchanged.
        assert_eq!(b.render(&FakeKeys), src);
    }

    #[test]
    fn a_text_value_that_is_only_a_hash_is_not_a_dropped_binding() {
        let (b, probs) = Bindings::parse("button 9 = text #5\n", &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(9).unwrap().action, Action::Text("#5".into()));
    }

    #[test]
    fn a_fixed_action_still_loses_a_trailing_comment() {
        let (b, probs) = Bindings::parse(
            "button 10 = key ctrl+c   # copy\n\
             button 7 = mouse middle  # wheel click\n\
             button 6 replace = nothing # dead\n",
            &FakeKeys,
        );
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(10).unwrap().action, Action::Key { mods: vec![29], code: 46 });
        assert_eq!(b.get(7).unwrap().action, Action::MouseButton(0x112));
        assert_eq!(b.get(6).unwrap().action, Action::Nothing);
    }

    #[test]
    fn a_command_spawns_without_waiting_for_it() {
        // The child runs for a second; the call has to come straight back or a
        // bound button would freeze the event loop for as long as it runs.
        let t = std::time::Instant::now();
        spawn_detached("/bin/sleep 1").expect("spawn /bin/sleep");
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        assert!(spawn_detached("/nonexistent/no-such-program").is_err());
    }

    #[test]
    fn an_unknown_key_name_is_an_error_not_silence() {
        let e = Action::parse("key ctrl+nosuchkey", &FakeKeys).unwrap_err();
        assert!(e.contains("nosuchkey"), "{e}");
        assert!(e.contains("unknown key name"), "{e}");
    }

    #[test]
    fn empty_and_malformed_actions_are_rejected() {
        for bad in ["key", "key ctrl+", "text", "command", "nothing please", "wiggle it"] {
            assert!(Action::parse(bad, &FakeKeys).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn augment_is_the_default() {
        let (b, probs) = Bindings::parse("button 10 = key ctrl+c\n", &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(10).unwrap().mode, BindMode::Augment);
    }

    #[test]
    fn replace_is_parsed() {
        let (b, probs) = Bindings::parse("button 8 replace = nothing\n", &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        let got = b.get(8).unwrap();
        assert_eq!(got.mode, BindMode::Replace);
        assert_eq!(got.action, Action::Nothing);
    }

    #[test]
    fn only_replace_bindings_are_silenced() {
        let (b, probs) = Bindings::parse(
            "button 10 = key ctrl+c\n\
             button 8 replace = command true\n\
             button 6 replace = nothing\n\
             button 7 augment = mouse middle\n",
            &FakeKeys,
        );
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.to_silence(), vec![6, 8]);
    }

    #[test]
    fn bindings_round_trip_through_render() {
        let src = "button 10 = key ctrl+c\n\
                   button 9 = text Hello\n\
                   button 8 replace = command notify-send hi\n\
                   button 7 = mouse middle\n\
                   button 6 replace = nothing\n";
        let (a, probs) = Bindings::parse(src, &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        let text = a.render(&FakeKeys);
        assert_eq!(text, src);
        let (b, probs) = Bindings::parse(&text, &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(a.list, b.list);
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let (b, probs) = Bindings::parse(
            "# a comment\n\n   \nbutton 10 = key ctrl+c   # trailing\n",
            &FakeKeys,
        );
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.list.len(), 1);
        assert_eq!(b.get(10).unwrap().action, Action::Key { mods: vec![29], code: 46 });
    }

    #[test]
    fn a_hash_inside_a_word_is_not_a_comment() {
        let (b, probs) = Bindings::parse("button 9 = text issue#5\n", &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(9).unwrap().action, Action::Text("issue#5".into()));
    }

    #[test]
    fn bad_lines_are_reported_without_dropping_good_ones() {
        let (b, probs) = Bindings::parse(
            "button 10 = key ctrl+c\n\
             nonsense\n\
             button 9 = key nosuchkey\n\
             button 99 = nothing\n\
             button 0 = nothing\n\
             button 8 sideways = nothing\n\
             button 7 = mouse middle\n",
            &FakeKeys,
        );
        assert_eq!(b.list.len(), 2, "{:?}", b.list);
        assert!(b.get(10).is_some() && b.get(7).is_some());
        assert_eq!(probs.len(), 5, "{probs:?}");
        assert!(probs.iter().all(|p| p.starts_with("line ")), "{probs:?}");
    }

    #[test]
    fn a_second_binding_for_one_button_wins_and_is_reported() {
        let (b, probs) = Bindings::parse(
            "button 10 = key ctrl+c\nbutton 10 replace = nothing\n",
            &FakeKeys,
        );
        assert_eq!(b.list.len(), 1);
        assert_eq!(b.get(10).unwrap().action, Action::Nothing);
        assert_eq!(b.get(10).unwrap().mode, BindMode::Replace);
        assert_eq!(probs.len(), 1, "{probs:?}");
    }

    #[test]
    fn the_button_keyword_is_optional() {
        let (b, probs) = Bindings::parse("10 replace = key f5\n", &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.get(10).unwrap().mode, BindMode::Replace);
    }

    #[test]
    fn a_plain_command_avoids_the_shell() {
        assert!(!needs_shell("notify-send hi"));
        assert_eq!(argv("notify-send hi"), vec!["notify-send", "hi"]);
    }

    #[test]
    fn a_command_that_needs_a_shell_gets_one_verbatim() {
        for c in ["echo $HOME", "a | b", "x > /tmp/y", "ls *.txt", "say \"hi there\""] {
            assert!(needs_shell(c), "{c:?}");
            assert_eq!(argv(c), vec!["/bin/sh", "-c", c]);
        }
    }

    #[test]
    fn the_example_file_is_valid_once_uncommented() {
        let (empty, probs) = Bindings::parse(EXAMPLE, &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert!(empty.list.is_empty(), "the shipped example binds nothing");

        // Every commented binding in it must parse, modifiers aside: the fake
        // keymap knows ctrl and c, which is all the examples use.
        let live: String = EXAMPLE
            .lines()
            .filter_map(|l| l.strip_prefix("# button"))
            .map(|l| format!("button{l}\n"))
            .collect();
        let (b, probs) = Bindings::parse(&live, &FakeKeys);
        assert!(probs.is_empty(), "{probs:?}");
        assert_eq!(b.list.len(), 5);
        assert_eq!(b.to_silence(), vec![6, 8]);
    }

    #[test]
    fn config_path_is_under_logi() {
        let p = config_path();
        assert!(p.ends_with("logi/bindings.conf"), "{p:?}");
    }
}
