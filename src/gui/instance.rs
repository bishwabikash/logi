//! One logi window-and-icon per user.
//!
//! The process stays up as the panel icon after its window closes, so starting
//! `logi` again (the launcher, a second terminal) must not make a second icon
//! and a second device worker. It asks the first one to show its window instead,
//! with SIGUSR1, and exits.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Holds the lock for as long as it lives. Dropping it, or the process dying,
/// releases the lock: nothing is left behind to go stale.
pub struct Instance {
    _file: std::fs::File,
}

pub enum Acquired {
    /// This process is the one.
    First(Instance),
    /// Another is running, and this was its pid (0 if it had not written it yet).
    Running(i32),
}

pub fn lock_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| d.starts_with('/'))
        .map(PathBuf::from)
        // SAFETY: getuid cannot fail.
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/logi-{}", unsafe { libc::getuid() })));
    dir.join("logi-gui.lock")
}

pub fn acquire() -> std::io::Result<Acquired> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        // The /tmp fallback is a directory of our own; the runtime dir exists.
        let _ = std::fs::create_dir_all(parent);
    }
    acquire_at(&path)
}

pub fn acquire_at(path: &Path) -> std::io::Result<Acquired> {
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    // SAFETY: a valid fd we own, and a flag combination flock defines.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        file.flush()?;
        return Ok(Acquired::First(Instance { _file: file }));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(err);
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    Ok(Acquired::Running(text.trim().parse().unwrap_or(0)))
}

/// Set by SIGUSR1: someone started logi again and wants to see the window.
static RAISE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    // Async-signal-safe: one atomic store.
    RAISE.store(true, Ordering::SeqCst);
}

/// Start listening for "show your window".
pub fn listen_for_raise() {
    // SAFETY: installs a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGUSR1, on_usr1 as *const () as libc::sighandler_t);
    }
}

/// True once per request.
pub fn take_raise() -> bool {
    RAISE.swap(false, Ordering::SeqCst)
}

/// Ask the running instance to show its window. False if there is no one to ask.
pub fn ask_to_show(pid: i32) -> bool {
    // A pid of 0 or below would signal a whole process group.
    pid > 1
        // SAFETY: kill with a positive pid and a plain signal.
        && unsafe { libc::kill(pid, libc::SIGUSR1) } == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("logi-instance-{tag}-{}", std::process::id()))
    }

    #[test]
    fn the_second_start_finds_the_first() {
        let p = temp("two");
        let _ = std::fs::remove_file(&p);
        let first = acquire_at(&p).unwrap();
        assert!(matches!(first, Acquired::First(_)));
        match acquire_at(&p).unwrap() {
            Acquired::Running(pid) => assert_eq!(pid, std::process::id() as i32),
            Acquired::First(_) => panic!("two instances"),
        }
        drop(first);
        // Released with the first, so a later start is the first again.
        assert!(matches!(acquire_at(&p).unwrap(), Acquired::First(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_raise_request_is_seen_once() {
        RAISE.store(false, Ordering::SeqCst);
        assert!(!take_raise());
        on_usr1(0);
        assert!(take_raise());
        assert!(!take_raise());
    }

    #[test]
    fn nobody_is_signalled_without_a_real_pid() {
        assert!(!ask_to_show(0));
        assert!(!ask_to_show(-1));
        assert!(!ask_to_show(1));
    }
}
