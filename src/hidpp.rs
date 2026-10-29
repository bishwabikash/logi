//! HID++ 2.0 transport for wired Logitech devices.
//!
//! Speaks the vendor-defined `ff00.0001` (short, 7 byte) and `ff00.0002`
//! (long, 20 byte) reports that Logitech's own software uses. See PROTOCOL.md.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SHORT_REPORT: u8 = 0x10;
pub const LONG_REPORT: u8 = 0x11;
pub const VERY_LONG_REPORT: u8 = 0x12;

pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;

/// Wired / direct attach. A receiver would use 1..6 for its paired devices.
pub const DIRECT_INDEX: u8 = 0xFF;
/// Our tag in the low nibble of byte 3, so replies are distinguishable from
/// unsolicited event notifications — and from other clients' replies.
///
/// hidraw hands every input report to every open file description, and this
/// project runs exactly two clients on one node at once: the GUI's worker and
/// the systemd service. With one fixed id, the GUI would accept the service's
/// answer to a request for the same feature and function as its own. Both ask
/// for things like `memoryRead`, whose reply for one sector is the wrong data
/// for another. So the id is chosen once per process, from the pid and the
/// clock, which makes a collision between two live processes one in fifteen
/// instead of certain. Ids run 1..=15; zero is what events carry.
pub fn software_id() -> u8 {
    static ID: OnceLock<u8> = OnceLock::new();
    *ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        pick_software_id(std::process::id(), nanos)
    })
}

fn pick_software_id(pid: u32, nanos: u32) -> u8 {
    let mixed = pid.wrapping_mul(2_654_435_761).wrapping_add(nanos);
    1 + ((mixed ^ (mixed >> 16)) % 15) as u8
}

/// What one input report is, from the point of view of one outstanding request.
#[derive(Debug, PartialEq, Eq)]
enum Reply<'a> {
    /// The answer to our request: the payload, bytes 4...
    Answer(&'a [u8]),
    /// The device refused our request, with this error code.
    Fault(u8),
    /// Anything else: an event, or another client's reply.
    Other,
}

/// Decide whether `r` answers the request for `feature`/`function` sent with
/// software id `sw`.
///
/// An error report is `report, index, 0xFF, feature, function<<4|sw, code`. The
/// echoed feature, function and software id are all checked: accepting any
/// 0xFF report as the answer meant another client's refusal (or a stale one)
/// was read as ours, and the caller was told its own request failed.
fn classify<'a>(r: &'a [u8], feature: u8, function: u8, sw: u8) -> Reply<'a> {
    if r.len() < 4 || !matches!(r[0], SHORT_REPORT | LONG_REPORT | VERY_LONG_REPORT) {
        return Reply::Other;
    }
    let tag = (function << 4) | sw;
    if r[2] == 0xFF {
        return if r.len() >= 6 && r[3] == feature && r[4] == tag {
            Reply::Fault(r[5])
        } else {
            Reply::Other
        };
    }
    if r[2] == feature && r[3] == tag {
        Reply::Answer(&r[4..])
    } else {
        Reply::Other
    }
}

#[derive(Debug)]
pub enum Error {
    /// The device answered with an error report.
    Device { code: u8, feature: u8, function: u8 },
    /// No reply arrived within the timeout.
    Timeout { feature: u8, function: u8 },
    /// The device does not implement a feature we asked for.
    NoSuchFeature(u16),
    Io(std::io::Error),
    Permission(String),
    NotFound(String),
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Device { code, feature, function } => write!(
                f,
                "HID++ error 0x{code:02x} ({}) on feature index 0x{feature:02x} function 0x{function:x}",
                error_name(*code)
            ),
            Error::Timeout { feature, function } => write!(
                f,
                "no reply for feature index 0x{feature:02x} function 0x{function:x}"
            ),
            Error::NoSuchFeature(id) => {
                write!(f, "device does not support feature 0x{id:04x}")
            }
            Error::Io(e) => write!(f, "{e}"),
            Error::Permission(s) => write!(f, "{s}"),
            Error::NotFound(s) => write!(f, "{s}"),
            Error::Protocol(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn error_name(code: u8) -> &'static str {
    match code {
        0x00 => "no error",
        0x01 => "unknown",
        0x02 => "invalid argument",
        0x03 => "out of range",
        0x04 => "hardware error",
        0x05 => "logitech internal",
        0x06 => "invalid feature index",
        0x07 => "invalid function id",
        0x08 => "busy",
        0x09 => "unsupported",
        _ => "?",
    }
}

/// True for a report that answers some client's request rather than announcing
/// an event: an error report, or one tagged with a nonzero software id.
fn is_reply(r: &[u8]) -> bool {
    r.len() >= 4 && (r[2] == 0xFF || (r[3] & 0x0F) != 0)
}

/// A read that failed for a reason that says nothing about the device.
fn is_retryable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// What a poll on the endpoint found.
///
/// Readable is kept distinct from Hangup because an unplug can set POLLHUP
/// while reports are still queued, and those are worth reading; and from Idle
/// because a timeout means "nothing yet", whereas a hangup means the device is
/// gone and the caller should stop waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ready {
    Readable,
    Idle,
    Hangup,
}

/// One open HID++ control endpoint.
pub struct Device {
    file: File,
    pub path: String,
    pub device_index: u8,
    feature_cache: HashMap<u16, u8>,
}

impl Device {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(p).map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::PermissionDenied => Error::Permission(format!(
                    "no permission to open {}",
                    p.display()
                )),
                std::io::ErrorKind::NotFound => {
                    Error::NotFound(format!("{} does not exist", p.display()))
                }
                _ => Error::Io(e),
            }
        })?;
        Ok(Device {
            file,
            path: p.display().to_string(),
            device_index: DIRECT_INDEX,
            feature_cache: HashMap::new(),
        })
    }

    /// Wait up to `timeout` for the fd to become readable.
    fn wait_readable(&self, timeout: Duration) -> Ready {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: a single valid pollfd for a file we own, with a bounded timeout.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            // A signal landed mid-wait: nothing is known yet, so let the
            // caller come back round. Any other error is the fd itself.
            return match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted => Ready::Idle,
                _ => Ready::Hangup,
            };
        }
        // Data first: an unplug can set POLLHUP alongside reports that are
        // still queued, and those are worth reading.
        if (pfd.revents & libc::POLLIN) != 0 {
            Ready::Readable
        } else if (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 {
            Ready::Hangup
        } else {
            Ready::Idle
        }
    }

    /// Discard anything already queued, so a stale event is not mistaken for
    /// our reply.
    pub fn drain(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.wait_readable(Duration::from_millis(1)), Ready::Readable) {
            if self.file.read(&mut buf).is_err() {
                break;
            }
        }
    }

    fn frame(&self, feature: u8, function: u8, params: &[u8], long: bool) -> Result<Vec<u8>> {
        let size = if long { LONG_LEN } else { SHORT_LEN };
        let report = if long { LONG_REPORT } else { SHORT_REPORT };
        if 4 + params.len() > size {
            return Err(Error::Protocol(format!(
                "{} params do not fit a {} report",
                params.len(),
                if long { "long" } else { "short" }
            )));
        }
        let mut pkt = vec![0u8; size];
        pkt[0] = report;
        pkt[1] = self.device_index;
        pkt[2] = feature;
        pkt[3] = (function << 4) | software_id();
        pkt[4..4 + params.len()].copy_from_slice(params);
        Ok(pkt)
    }

    /// Issue one request and return its payload (bytes 4..).
    pub fn call(&mut self, feature: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        self.call_opts(feature, function, params, true, Duration::from_millis(800))
    }

    pub fn call_short(&mut self, feature: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        self.call_opts(feature, function, params, false, Duration::from_millis(800))
    }

    pub fn call_opts(
        &mut self,
        feature: u8,
        function: u8,
        params: &[u8],
        long: bool,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let pkt = self.frame(feature, function, params, long)?;
        let sw = software_id();
        self.drain();
        self.file.write_all(&pkt)?;

        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.wait_readable(left.min(Duration::from_millis(50))) {
                Ready::Readable => {}
                Ready::Idle => continue,
                // The endpoint is gone. Spinning here until the timeout only
                // delays the error and hides the reason for it.
                Ready::Hangup => {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "device disconnected",
                    )))
                }
            }
            let n = match self.file.read(&mut buf) {
                Ok(n) => n,
                Err(ref e) if is_retryable(e) => continue,
                Err(e) => return Err(Error::Io(e)),
            };
            match classify(&buf[..n], feature, function, sw) {
                Reply::Answer(payload) => return Ok(payload.to_vec()),
                Reply::Fault(code) => return Err(Error::Device { code, feature, function }),
                Reply::Other => {}
            }
        }
        Err(Error::Timeout { feature, function })
    }

    /// Fire a request without waiting for its reply.
    ///
    /// Animation pushes a frame every few milliseconds; waiting for each round
    /// trip caps throughput near 19 fps. The device still queues a response, so
    /// callers must `drain()` periodically. Only for idempotent writes where a
    /// dropped frame does not matter.
    pub fn send_nowait(&mut self, feature: u8, function: u8, params: &[u8]) -> Result<()> {
        let pkt = self.frame(feature, function, params, true)?;
        match self.file.write_all(&pkt) {
            Ok(()) => Ok(()),
            // Kernel queue full: drop this frame rather than stall.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Open a second handle on the same node, for receiving events.
    ///
    /// hidraw queues input reports per open file description, so a listener on
    /// its own handle gets its own copy and the two do not steal from each
    /// other. That matters because `call` drains the queue before every
    /// request: sharing one handle between command traffic and an event
    /// listener would throw away button presses as a side effect of, say,
    /// reading the DPI.
    pub fn open_listener(&self) -> Result<Device> {
        Device::open(&self.path)
    }

    /// Wait for one unsolicited event report.
    ///
    /// Returns Ok(None) on timeout, which is the normal idle case rather than
    /// an error. Replies are skipped: byte 3's low nibble carries a software
    /// id, and events carry zero there. Matching only *our* id would let the
    /// other client's replies through as events, and every report this handle
    /// sees from them is one, since hidraw copies each to every open handle.
    pub fn read_event(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.wait_readable(left.min(Duration::from_millis(100))) {
                Ready::Readable => {}
                Ready::Idle => continue,
                // Distinct from the timeout above: Ok(None) means "nothing yet,
                // ask again", and a listener that got that on an unplug would
                // poll a dead endpoint forever.
                Ready::Hangup => {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "device disconnected",
                    )))
                }
            }
            let n = match self.file.read(&mut buf) {
                Ok(n) => n,
                Err(ref e) if is_retryable(e) => continue,
                Err(e) => return Err(Error::Io(e)),
            };
            if n < 4 {
                continue;
            }
            let r = &buf[..n];
            if !matches!(r[0], SHORT_REPORT | LONG_REPORT | VERY_LONG_REPORT) {
                continue;
            }
            if is_reply(r) {
                continue;
            }
            return Ok(Some(r.to_vec()));
        }
    }

    /// Resolve a feature id (e.g. 0x8070) to its index on this device.
    pub fn feature_index(&mut self, id: u16) -> Result<u8> {
        if let Some(&i) = self.feature_cache.get(&id) {
            return Ok(i);
        }
        let p = self.call(0x00, 0x00, &[(id >> 8) as u8, (id & 0xFF) as u8])?;
        let idx = *p.first().unwrap_or(&0);
        if idx == 0 {
            return Err(Error::NoSuchFeature(id));
        }
        self.feature_cache.insert(id, idx);
        Ok(idx)
    }

    pub fn has_feature(&mut self, id: u16) -> bool {
        self.feature_index(id).is_ok()
    }

    pub fn protocol_version(&mut self) -> Result<(u8, u8)> {
        let p = self.call(0x00, 0x01, &[])?;
        if p.len() < 2 {
            return Err(Error::Protocol("short protocol version reply".into()));
        }
        Ok((p[0], p[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(feature: u8, tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; LONG_LEN];
        r[0] = LONG_REPORT;
        r[1] = DIRECT_INDEX;
        r[2] = feature;
        r[3] = tag;
        r[4..4 + payload.len()].copy_from_slice(payload);
        r
    }

    fn fault(feature: u8, tag: u8, code: u8) -> Vec<u8> {
        let mut r = vec![0u8; SHORT_LEN];
        r[0] = SHORT_REPORT;
        r[1] = DIRECT_INDEX;
        r[2] = 0xFF;
        r[3] = feature;
        r[4] = tag;
        r[5] = code;
        r
    }

    #[test]
    fn a_reply_must_carry_our_software_id() {
        let ours = long(0x05, 0x30 | 4, &[9, 9]);
        assert!(matches!(classify(&ours, 0x05, 3, 4), Reply::Answer(p) if p[..2] == [9, 9]));
        // The same feature and function from a client using another id is
        // that client's answer, not ours.
        let theirs = long(0x05, 0x30 | 5, &[1, 1]);
        assert_eq!(classify(&theirs, 0x05, 3, 4), Reply::Other);
    }

    #[test]
    fn an_error_report_must_echo_our_request() {
        assert_eq!(classify(&fault(0x05, 0x30 | 4, 0x08), 0x05, 3, 4), Reply::Fault(0x08));
        // Another client's refusal, another function's, another feature's:
        // none of them is the answer to this request.
        assert_eq!(classify(&fault(0x05, 0x30 | 5, 0x08), 0x05, 3, 4), Reply::Other);
        assert_eq!(classify(&fault(0x05, 0x40 | 4, 0x08), 0x05, 3, 4), Reply::Other);
        assert_eq!(classify(&fault(0x06, 0x30 | 4, 0x08), 0x05, 3, 4), Reply::Other);
    }

    #[test]
    fn a_truncated_or_foreign_report_is_ignored() {
        assert_eq!(classify(&[0x10, 0xFF, 0xFF], 0x05, 3, 4), Reply::Other);
        // Too short to hold an error code.
        assert_eq!(classify(&[0x10, 0xFF, 0xFF, 0x05, 0x34], 0x05, 3, 4), Reply::Other);
        // Not a HID++ report id at all.
        let mut not_hidpp = long(0x05, 0x34, &[]);
        not_hidpp[0] = 0x01;
        assert_eq!(classify(&not_hidpp, 0x05, 3, 4), Reply::Other);
    }

    #[test]
    fn events_are_not_replies_and_every_clients_replies_are() {
        // An event carries zero in the software id nibble.
        assert!(!is_reply(&long(0x0d, 0x00, &[0, 1])));
        // Our reply and another client's, whatever their ids.
        assert!(is_reply(&long(0x0d, 0x38, &[])));
        assert!(is_reply(&long(0x0d, 0x35, &[])));
        // And an error report, whose byte 3 is a feature index that could
        // have a zero low nibble.
        assert!(is_reply(&fault(0x10, 0x34, 0x08)));
    }

    #[test]
    fn the_chosen_software_id_is_never_the_event_id() {
        let mut seen = [false; 16];
        for pid in (1..400u32).step_by(7) {
            for nanos in (0..1_000_000_000u32).step_by(7_654_321) {
                let id = pick_software_id(pid, nanos);
                assert!((1..=15).contains(&id), "{id}");
                seen[id as usize] = true;
            }
        }
        assert!(!seen[0]);
        assert!(seen[1..].iter().all(|s| *s), "some ids are unreachable: {seen:?}");
        assert!((1..=15).contains(&software_id()));
    }
}
