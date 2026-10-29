//! A virtual keyboard/mouse over /dev/uinput, so a spied mouse button can be
//! bound to any host action.
//!
//! The device registers EV_KEY plus every code in `KEYS`, which includes the
//! BTN_* range: that is what lets one physical button be rebound to another
//! button without touching the mouse's own remapping table.
//!
//! Two things are worth knowing before reading the code:
//!
//!   * Key codes are *positional*, not characters. KEY_A is the key where A
//!     sits on a US QWERTY board; the compositor's keymap decides what it
//!     produces. `type_text` therefore types the right letters only under a
//!     US-ish layout. There is no way around this from uinput — the kernel
//!     layer has no notion of characters.
//!   * EV_REL with REL_X/REL_Y is enabled although no motion is ever emitted.
//!     libinput classifies a device by its capabilities, and a device carrying
//!     BTN_LEFT but no relative axes is tagged as a keyboard, whose button
//!     events it then drops. The two axis bits are what make it a pointer.
//!
//! /dev/uinput is root-only on a stock system; see `VirtualDevice::new` for
//! the udev rule that fixes that.

use std::io;
use std::thread;
use std::time::Duration;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0x00;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;

/// _IOW('U', nr, T) in the asm-generic encoding: direction 1 at bit 30, the
/// argument size at bit 16. x86, arm, arm64 and riscv all use this; the few
/// arches that do not (powerpc, mips, sparc, alpha) are not targets here.
const fn iow(nr: u32, size: u32) -> libc::c_ulong {
    ((1 << 30) | (size << 16) | (b'U' as u32) << 8 | nr) as libc::c_ulong
}

/// _IO('U', nr) — no argument, so no size and no direction bits.
const fn io_(nr: u32) -> libc::c_ulong {
    (((b'U' as u32) << 8) | nr) as libc::c_ulong
}

const UI_DEV_CREATE: libc::c_ulong = io_(1);
const UI_DEV_DESTROY: libc::c_ulong = io_(2);
const UI_DEV_SETUP: libc::c_ulong = iow(3, std::mem::size_of::<UinputSetup>() as u32);
const UI_SET_EVBIT: libc::c_ulong = iow(100, 4);
const UI_SET_KEYBIT: libc::c_ulong = iow(101, 4);
const UI_SET_RELBIT: libc::c_ulong = iow(102, 4);

/// Gap between typed characters. Short enough that a hundred characters cost a
/// fifth of a second, long enough for the reader to drain between them.
const TYPING_GAP: Duration = Duration::from_millis(2);

const UINPUT_MAX_NAME_SIZE: usize = 80;
const BUS_VIRTUAL: u16 = 0x06;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

/// A registered virtual input device. Dropping it unregisters the device.
pub struct VirtualDevice {
    fd: libc::c_int,
}

impl VirtualDevice {
    /// Create and register the device.
    ///
    /// Fails cleanly when /dev/uinput is missing (the `uinput` module is not
    /// loaded) or not writable (it is root-only unless a udev rule says
    /// otherwise). The error text names the fix, because this is the one
    /// failure every user of this feature will hit first.
    pub fn new(name: &str) -> io::Result<Self> {
        let path = c"/dev/uinput";
        // O_NONBLOCK so a write can never wedge the caller; uinput writes do
        // not block in practice, but a bound button lives on the event loop.
        //
        // O_CLOEXEC because a `command` binding spawns arbitrary programs and
        // `std::process::Command` only replaces stdio: without it every child
        // inherits this descriptor. The virtual device stays registered for as
        // long as any descriptor on it is open, so a child that outlives us —
        // which is the whole point of a detached command — would keep a ghost
        // "logi virtual input" device in the compositor after logi is gone,
        // and a fresh run would add another beside it.
        let fd = unsafe {
            libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return Err(open_error(io::Error::last_os_error()));
        }
        let dev = VirtualDevice { fd };

        dev.ioctl_arg(UI_SET_EVBIT, EV_KEY)?;
        dev.ioctl_arg(UI_SET_EVBIT, EV_SYN)?;
        dev.ioctl_arg(UI_SET_EVBIT, EV_REL)?;
        // See the module docs: axes are declared, never emitted.
        dev.ioctl_arg(UI_SET_RELBIT, REL_X)?;
        dev.ioctl_arg(UI_SET_RELBIT, REL_Y)?;
        for (_, code) in KEYS {
            dev.ioctl_arg(UI_SET_KEYBIT, *code)?;
        }

        let mut setup = UinputSetup {
            // Not the real Logitech ids: discover.rs probes by vendor/product,
            // and this device must never look like a mouse it can drive.
            id: InputId { bustype: BUS_VIRTUAL, vendor: 0x0001, product: 0x0001, version: 1 },
            name: [0; UINPUT_MAX_NAME_SIZE],
            ff_effects_max: 0,
        };
        // Truncated on a character boundary: the kernel treats the name as
        // bytes, but half a UTF-8 sequence would show up mangled in sysfs.
        let mut n = name.len().min(UINPUT_MAX_NAME_SIZE - 1);
        while n > 0 && !name.is_char_boundary(n) {
            n -= 1;
        }
        setup.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        if unsafe { libc::ioctl(dev.fd, UI_DEV_SETUP, &setup) } < 0 {
            return Err(ioctl_error("UI_DEV_SETUP"));
        }
        if unsafe { libc::ioctl(dev.fd, UI_DEV_CREATE) } < 0 {
            return Err(ioctl_error("UI_DEV_CREATE"));
        }

        // The kernel creates the event node asynchronously and udev, libinput
        // and the compositor each have to notice it. Events written before
        // that chain completes are accepted by uinput and then dropped on the
        // floor, so the first bound press would silently do nothing.
        thread::sleep(Duration::from_millis(200));
        Ok(dev)
    }

    /// The bit-setting ioctls take their argument as an `unsigned long`. It has
    /// to be widened here: a narrower variadic argument leaves the top half of
    /// the register unspecified, and the kernel range-checks the whole word.
    fn ioctl_arg(&self, req: libc::c_ulong, bit: u16) -> io::Result<()> {
        let rc = unsafe { libc::ioctl(self.fd, req, bit as libc::c_ulong) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Press or release one key or button.
    ///
    /// A code the device never registered is refused rather than written:
    /// uinput accepts the write and the input core then filters the event
    /// against the device's capability bits, so it would be a silent no-op.
    pub fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        if key_name(code).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("key code {code} is not one this device registered"),
            ));
        }
        self.emit(EV_KEY, code, if down { 1 } else { 0 })
    }

    /// Press and release, with a sync between.
    ///
    /// The release runs even when the press failed. Bailing out early on a
    /// write error would leave the key held down for as long as the virtual
    /// device lives, which to the desktop is indistinguishable from a wedged
    /// keyboard, so the first error is reported but every step still runs.
    pub fn tap(&mut self, code: u16) -> io::Result<()> {
        let mut err = None;
        keep(&mut err, self.key(code, true));
        keep(&mut err, self.sync());
        keep(&mut err, self.key(code, false));
        // The trailing sync is not optional: without it the release sits in
        // the kernel's buffer and the key reads as held down.
        keep(&mut err, self.sync());
        err.map_or(Ok(()), Err)
    }

    /// Hold the modifiers, tap the key, release the modifiers.
    ///
    /// The modifiers are released whatever the tap did, for the reason given
    /// on `tap`: a stuck Ctrl is worse than a lost keystroke.
    pub fn chord(&mut self, mods: &[u16], code: u16) -> io::Result<()> {
        let mut err = None;
        for m in mods {
            keep(&mut err, self.key(*m, true));
        }
        keep(&mut err, self.sync());
        keep(&mut err, self.tap(code));
        // Released in reverse order, which is what a human hand does and what
        // applications watching modifier state expect.
        for m in mods.iter().rev() {
            keep(&mut err, self.key(*m, false));
        }
        keep(&mut err, self.sync());
        err.map_or(Ok(()), Err)
    }

    /// Type a string as individual key events, shifting where needed.
    ///
    /// Characters with no key on a US layout are skipped rather than failing
    /// the whole string: a macro that is mostly typeable should still type.
    ///
    /// A pause follows every character. The input core hands each event to the
    /// compositor's evdev client through a small ring buffer, and a string
    /// written in one tight burst can outrun the reader: the buffer overflows,
    /// the client resyncs, and characters go missing from the middle of the
    /// text.
    pub fn type_text(&mut self, text: &str) -> io::Result<()> {
        for (code, shift) in text_keys(text) {
            if shift {
                self.chord(&[KEY_LEFTSHIFT], code)?;
            } else {
                self.tap(code)?;
            }
            thread::sleep(TYPING_GAP);
        }
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        // A zeroed timeval is correct: the kernel timestamps the event itself.
        let ev = InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            kind,
            code,
            value,
        };
        let size = std::mem::size_of::<InputEvent>();
        let n = unsafe {
            libc::write(self.fd, &ev as *const InputEvent as *const libc::c_void, size)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write to /dev/uinput: {n} of {size} bytes"),
            ));
        }
        Ok(())
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // Errors here are unactionable; the close releases the device anyway.
        unsafe {
            libc::ioctl(self.fd, UI_DEV_DESTROY);
            libc::close(self.fd);
        }
    }
}

/// Remember the first error while letting the remaining steps run, so a
/// half-finished keystroke still gets its releases emitted.
fn keep(slot: &mut Option<io::Error>, r: io::Result<()>) {
    if let Err(e) = r {
        slot.get_or_insert(e);
    }
}

/// Name the ioctl that failed: a bare "Invalid argument" from the setup path
/// is impossible to act on.
fn ioctl_error(which: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{which} on /dev/uinput failed: {e}"))
}

fn open_error(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::NotFound => io::Error::new(
            io::ErrorKind::NotFound,
            "/dev/uinput does not exist: the uinput kernel module is not loaded. \
             Load it with `sudo modprobe uinput`, and make it permanent with \
             `echo uinput | sudo tee /etc/modules-load.d/uinput.conf`.",
        ),
        io::ErrorKind::PermissionDenied => io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no permission to open /dev/uinput, which is root-only by default. \
             Install packaging/99-uinput.rules (KERNEL==\"uinput\", \
             SUBSYSTEM==\"misc\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"), \
             reload udev, add yourself to the input group and re-login — or run \
             as root.",
        ),
        _ => e,
    }
}

// Named because chord building presses shift itself.
pub const KEY_LEFTSHIFT: u16 = 42;

/// Canonical name to code. One entry per code: `key_name` reverses this, so a
/// second spelling of a code belongs in `ALIASES`, not here.
const KEYS: &[(&str, u16)] = &[
    ("esc", 1),
    ("1", 2), ("2", 3), ("3", 4), ("4", 5), ("5", 6),
    ("6", 7), ("7", 8), ("8", 9), ("9", 10), ("0", 11),
    ("minus", 12), ("equal", 13), ("backspace", 14), ("tab", 15),
    ("q", 16), ("w", 17), ("e", 18), ("r", 19), ("t", 20),
    ("y", 21), ("u", 22), ("i", 23), ("o", 24), ("p", 25),
    ("leftbrace", 26), ("rightbrace", 27), ("enter", 28), ("leftctrl", 29),
    ("a", 30), ("s", 31), ("d", 32), ("f", 33), ("g", 34),
    ("h", 35), ("j", 36), ("k", 37), ("l", 38),
    ("semicolon", 39), ("apostrophe", 40), ("grave", 41),
    ("leftshift", 42), ("backslash", 43),
    ("z", 44), ("x", 45), ("c", 46), ("v", 47), ("b", 48), ("n", 49), ("m", 50),
    ("comma", 51), ("dot", 52), ("slash", 53), ("rightshift", 54),
    ("kpasterisk", 55), ("leftalt", 56), ("space", 57), ("capslock", 58),
    ("f1", 59), ("f2", 60), ("f3", 61), ("f4", 62), ("f5", 63),
    ("f6", 64), ("f7", 65), ("f8", 66), ("f9", 67), ("f10", 68),
    ("numlock", 69), ("scrolllock", 70),
    ("kp7", 71), ("kp8", 72), ("kp9", 73), ("kpminus", 74),
    ("kp4", 75), ("kp5", 76), ("kp6", 77), ("kpplus", 78),
    ("kp1", 79), ("kp2", 80), ("kp3", 81), ("kp0", 82), ("kpdot", 83),
    ("f11", 87), ("f12", 88),
    ("kpenter", 96), ("rightctrl", 97), ("kpslash", 98), ("sysrq", 99),
    ("rightalt", 100),
    ("home", 102), ("arrowup", 103), ("pageup", 104), ("arrowleft", 105),
    ("arrowright", 106), ("end", 107), ("arrowdown", 108), ("pagedown", 109),
    ("insert", 110), ("delete", 111),
    ("mute", 113), ("volumedown", 114), ("volumeup", 115), ("power", 116),
    ("kpequal", 117), ("pause", 119),
    ("leftmeta", 125), ("rightmeta", 126), ("compose", 127),
    ("undo", 131), ("copy", 133), ("paste", 135), ("cut", 137),
    ("help", 138), ("menu", 139), ("calc", 140),
    ("sleep", 142), ("wakeup", 143),
    ("mail", 155), ("bookmarks", 156), ("computer", 157),
    ("browserback", 158), ("browserforward", 159),
    ("nextsong", 163), ("playpause", 164), ("previoussong", 165),
    ("stopcd", 166), ("record", 167), ("rewind", 168),
    ("homepage", 172), ("refresh", 173),
    ("f13", 183), ("f14", 184), ("f15", 185), ("f16", 186), ("f17", 187),
    ("f18", 188), ("f19", 189), ("f20", 190), ("f21", 191), ("f22", 192),
    ("f23", 193), ("f24", 194),
    ("print", 210), ("search", 217),
    ("brightnessdown", 224), ("brightnessup", 225),
    // The BTN_* range, so a button can be bound to another button.
    ("btn_left", 0x110), ("btn_right", 0x111), ("btn_middle", 0x112),
    ("btn_side", 0x113), ("btn_extra", 0x114), ("btn_forward", 0x115),
    ("btn_back", 0x116), ("btn_task", 0x117),
];

/// Alternative spellings. Bare "left", "right", "middle", "back" and "forward"
/// resolve to the *mouse* buttons: this is a mouse driver, every binding is
/// written for a mouse button, and that is the reading a user expects. The
/// arrow keys and the browser navigation keys keep their explicit names.
const ALIASES: &[(&str, &str)] = &[
    ("ctrl", "leftctrl"), ("control", "leftctrl"), ("lctrl", "leftctrl"),
    ("rctrl", "rightctrl"),
    ("shift", "leftshift"), ("lshift", "leftshift"), ("rshift", "rightshift"),
    ("alt", "leftalt"), ("lalt", "leftalt"),
    ("altgr", "rightalt"), ("ralt", "rightalt"),
    ("meta", "leftmeta"), ("super", "leftmeta"), ("win", "leftmeta"),
    ("cmd", "leftmeta"), ("lmeta", "leftmeta"), ("rmeta", "rightmeta"),
    ("escape", "esc"),
    ("return", "enter"), ("cr", "enter"),
    ("bs", "backspace"),
    ("del", "delete"), ("ins", "insert"),
    ("pgup", "pageup"), ("pgdn", "pagedown"), ("pagedn", "pagedown"),
    ("up", "arrowup"), ("down", "arrowdown"),
    ("leftarrow", "arrowleft"), ("rightarrow", "arrowright"),
    ("uparrow", "arrowup"), ("downarrow", "arrowdown"),
    ("spacebar", "space"), ("period", "dot"), ("dash", "minus"),
    ("hyphen", "minus"), ("equals", "equal"), ("plus", "kpplus"),
    ("lbracket", "leftbrace"), ("rbracket", "rightbrace"),
    ("bracketleft", "leftbrace"), ("bracketright", "rightbrace"),
    ("quote", "apostrophe"), ("backtick", "grave"), ("tilde", "grave"),
    ("printscreen", "sysrq"), ("prtsc", "sysrq"),
    ("contextmenu", "compose"),
    ("play", "playpause"), ("pauseplay", "playpause"),
    ("next", "nextsong"), ("prev", "previoussong"), ("previous", "previoussong"),
    ("volup", "volumeup"), ("voldown", "volumedown"),
    ("left", "btn_left"), ("right", "btn_right"), ("middle", "btn_middle"),
    ("mouseleft", "btn_left"), ("mouseright", "btn_right"),
    ("mousemiddle", "btn_middle"),
    ("side", "btn_side"), ("extra", "btn_extra"),
    ("forward", "btn_forward"), ("back", "btn_back"), ("task", "btn_task"),
    ("click", "btn_left"), ("rclick", "btn_right"), ("mclick", "btn_middle"),
];

/// Codes from here up are BTN_*, not keys.
const BTN_FIRST: u16 = 0x100;

/// Kernel spellings whose bare form is taken by a mouse-button alias. Reached
/// only through an explicit `KEY_` prefix, which is what keeps `KEY_LEFT` the
/// arrow key while a bare "left" stays `BTN_LEFT`.
const KERNEL_ONLY: &[(&str, &str)] = &[
    ("left", "arrowleft"),
    ("right", "arrowright"),
    ("up", "arrowup"),
    ("down", "arrowdown"),
    ("back", "browserback"),
    ("forward", "browserforward"),
];

/// Linux input-event-codes name to code, e.g. "ctrl", "a", "f5", "volumeup",
/// "left" (the mouse button — see `ALIASES`). Case-insensitive, and the
/// kernel's own `KEY_`/`BTN_` spellings work: the prefix pins the namespace, so
/// `KEY_LEFT` is the arrow key and never a click.
pub fn key_code(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(bare) = n.strip_prefix("key_") {
        if let Some((_, canon)) = KERNEL_ONLY.iter().find(|(k, _)| *k == bare) {
            return lookup(canon);
        }
        // A KEY_ name must not reach a button through an alias.
        return lookup(bare).filter(|c| *c < BTN_FIRST);
    }
    lookup(&n)
}

fn lookup(n: &str) -> Option<u16> {
    if let Some((_, c)) = KEYS.iter().find(|(k, _)| *k == n) {
        return Some(*c);
    }
    let (_, canon) = ALIASES.iter().find(|(a, _)| *a == n)?;
    KEYS.iter().find(|(k, _)| k == canon).map(|(_, c)| *c)
}

pub fn key_name(code: u16) -> Option<&'static str> {
    KEYS.iter().find(|(_, c)| *c == code).map(|(k, _)| *k)
}

/// The key, and whether shift is held, for one character on a US layout.
fn char_key(c: char) -> Option<(u16, bool)> {
    if c.is_ascii_lowercase() {
        return key_code(&c.to_string()).map(|k| (k, false));
    }
    if c.is_ascii_uppercase() {
        return key_code(&c.to_ascii_lowercase().to_string()).map(|k| (k, true));
    }
    if c.is_ascii_digit() {
        return key_code(&c.to_string()).map(|k| (k, false));
    }
    let (name, shift) = match c {
        ' ' => ("space", false),
        '\n' => ("enter", false),
        '\t' => ("tab", false),
        '-' => ("minus", false),
        '_' => ("minus", true),
        '=' => ("equal", false),
        '+' => ("equal", true),
        '[' => ("leftbrace", false),
        '{' => ("leftbrace", true),
        ']' => ("rightbrace", false),
        '}' => ("rightbrace", true),
        ';' => ("semicolon", false),
        ':' => ("semicolon", true),
        '\'' => ("apostrophe", false),
        '"' => ("apostrophe", true),
        '`' => ("grave", false),
        '~' => ("grave", true),
        '\\' => ("backslash", false),
        '|' => ("backslash", true),
        ',' => ("comma", false),
        '<' => ("comma", true),
        '.' => ("dot", false),
        '>' => ("dot", true),
        '/' => ("slash", false),
        '?' => ("slash", true),
        '!' => ("1", true),
        '@' => ("2", true),
        '#' => ("3", true),
        '$' => ("4", true),
        '%' => ("5", true),
        '^' => ("6", true),
        '&' => ("7", true),
        '*' => ("8", true),
        '(' => ("9", true),
        ')' => ("0", true),
        _ => return None,
    };
    key_code(name).map(|k| (k, shift))
}

/// Separated from `type_text` so the mapping is testable without /dev/uinput.
fn text_keys(text: &str) -> Vec<(u16, bool)> {
    text.chars().filter_map(char_key).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_DEV_SETUP, 0x405C_5503);
    }

    #[test]
    fn structs_have_the_layout_the_kernel_expects() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputId>(), 8);
        // timeval + two u16 + i32; 24 on a 64-bit kernel, 16 on 32-bit.
        assert_eq!(
            std::mem::size_of::<InputEvent>(),
            std::mem::size_of::<libc::timeval>() + 8
        );
    }

    #[test]
    fn every_code_is_listed_once() {
        for (i, (_, c)) in KEYS.iter().enumerate() {
            assert!(
                KEYS[..i].iter().all(|(_, o)| o != c),
                "code {c} appears twice; a second spelling belongs in ALIASES"
            );
        }
    }

    #[test]
    fn names_round_trip() {
        for (name, code) in KEYS {
            assert_eq!(key_code(name), Some(*code), "{name}");
            assert_eq!(key_name(*code), Some(*name), "{name}");
        }
    }

    #[test]
    fn every_alias_resolves() {
        for (alias, canon) in ALIASES {
            assert_eq!(key_code(alias), key_code(canon), "{alias} -> {canon}");
            assert!(key_code(alias).is_some(), "{alias}");
        }
    }

    #[test]
    fn kernel_spellings_keep_their_namespace() {
        // The clash that would otherwise bind a left arrow to a left click.
        assert_eq!(key_code("KEY_LEFT"), Some(105));
        assert_eq!(key_code("left"), Some(0x110));
        assert_eq!(key_code("KEY_RIGHT"), Some(106));
        assert_eq!(key_code("KEY_UP"), Some(103));
        assert_eq!(key_code("KEY_DOWN"), Some(108));
        assert_eq!(key_code("KEY_BACK"), Some(158));
        assert_eq!(key_code("back"), Some(0x116));
        assert_eq!(key_code("KEY_FORWARD"), Some(159));
        // No KEY_MIDDLE exists, so the prefixed form must not find the button.
        assert_eq!(key_code("KEY_MIDDLE"), None);
        assert_eq!(key_code("middle"), Some(0x112));
        for (bare, canon) in KERNEL_ONLY {
            assert_eq!(key_code(&format!("key_{bare}")), key_code(canon), "{bare}");
        }
    }

    #[test]
    fn no_alias_is_shadowed_or_repeated() {
        for (i, (alias, _)) in ALIASES.iter().enumerate() {
            assert!(
                !KEYS.iter().any(|(k, _)| k == alias),
                "alias {alias} is also a canonical name, so it is dead"
            );
            assert!(
                ALIASES[..i].iter().all(|(a, _)| a != alias),
                "alias {alias} is listed twice"
            );
        }
    }

    #[test]
    fn keep_retains_the_first_error_and_ignores_success() {
        let mut err = None;
        keep(&mut err, Ok(()));
        assert!(err.is_none());
        keep(&mut err, Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        keep(&mut err, Err(io::Error::from(io::ErrorKind::NotFound)));
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn lookup_is_case_insensitive_and_takes_kernel_spellings() {
        assert_eq!(key_code("CTRL"), Some(29));
        assert_eq!(key_code(" F5 "), key_code("f5"));
        assert_eq!(key_code("KEY_A"), key_code("a"));
        assert_eq!(key_code("BTN_LEFT"), Some(0x110));
        assert_eq!(key_code("nonsense"), None);
    }

    #[test]
    fn bare_directions_are_mouse_buttons_and_arrows_are_explicit() {
        assert_eq!(key_code("left"), Some(0x110));
        assert_eq!(key_code("right"), Some(0x111));
        assert_eq!(key_code("arrowleft"), Some(105));
        assert_eq!(key_code("leftarrow"), Some(105));
        // No BTN_UP exists, so bare up/down are unambiguous.
        assert_eq!(key_code("up"), Some(103));
        assert_eq!(key_code("back"), Some(0x116));
        assert_eq!(key_code("browserback"), Some(158));
    }

    #[test]
    fn modifier_names_map_to_their_kernel_codes() {
        for (n, code) in [
            ("ctrl", 29), ("rctrl", 97), ("shift", 42), ("rshift", 54),
            ("alt", 56), ("altgr", 100), ("super", 125), ("rmeta", 126),
        ] {
            assert_eq!(key_code(n), Some(code), "{n}");
        }
    }

    #[test]
    fn media_keys_are_present() {
        for n in ["playpause", "nextsong", "previoussong", "volumeup", "volumedown", "mute"] {
            assert!(key_code(n).is_some(), "{n}");
        }
    }

    #[test]
    fn text_maps_to_codes_and_shift_where_needed() {
        let a = key_code("a").unwrap();
        let b = key_code("b").unwrap();
        let one = key_code("1").unwrap();
        let space = key_code("space").unwrap();
        assert_eq!(
            text_keys("aB 1!"),
            vec![(a, false), (b, true), (space, false), (one, false), (one, true)]
        );
    }

    #[test]
    fn text_maps_punctuation_and_control_characters() {
        let slash = key_code("slash").unwrap();
        assert_eq!(
            text_keys("/?\n\t"),
            vec![
                (slash, false),
                (slash, true),
                (key_code("enter").unwrap(), false),
                (key_code("tab").unwrap(), false),
            ]
        );
    }

    #[test]
    fn untypeable_characters_are_skipped_not_fatal() {
        let a = key_code("a").unwrap();
        assert_eq!(text_keys("a\u{e9}\u{1f600}a"), vec![(a, false), (a, false)]);
    }

    #[test]
    #[ignore = "needs write access to /dev/uinput"]
    fn opens_a_real_device() {
        let mut d = VirtualDevice::new("logi test").expect("open /dev/uinput");
        d.tap(key_code("f24").unwrap()).unwrap();
    }
}
