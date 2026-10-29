//! Sending changes to the mouse without a button.
//!
//! The rule it implements: a change is sent once it has stopped changing. Not
//! on every keystroke or step of a drag, because a profile write is flash with
//! finite endurance and a name typed a letter at a time would be a write per
//! letter. After a short quiet, then.
//!
//! The shorter wait is for things that cost the mouse nothing, like a live
//! colour; the longer one for flash.

use std::time::{Duration, Instant};

/// Quiet needed before a live, non-flash change is sent.
pub const LIVE_QUIET: Duration = Duration::from_millis(300);

/// Quiet needed before a change that writes flash is sent.
pub const FLASH_QUIET: Duration = Duration::from_millis(2000);

/// Whether it is safe to send a change that writes flash on its own.
///
/// The app takes a backup once per launch before the first such write. Until
/// that has happened, or if it could not be taken, nothing is sent
/// automatically: sending flash writes with no way back, unasked, is the thing
/// the backup exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Safety {
    /// The backup is still being taken. A moment, not a problem.
    Pending,
    Ready,
    /// It could not be taken, and why. Nothing is sent unless the user says to.
    Failed(String),
}

impl Safety {
    pub fn allows_flash(&self) -> bool {
        matches!(self, Safety::Ready)
    }
}

/// Tracks one edit that is waiting for its quiet period.
///
/// Holds no clock: callers pass the time, so the timing is testable without
/// waiting for it.
#[derive(Debug, Default, Clone)]
pub struct Settle {
    changed: Option<Instant>,
}

impl Settle {
    pub const fn new() -> Self {
        Settle { changed: None }
    }

    /// The value just changed. Restarts the wait, which is the point: every
    /// further change pushes the send back.
    pub fn touch(&mut self, now: Instant) {
        self.changed = Some(now);
    }

    pub fn pending(&self) -> bool {
        self.changed.is_some()
    }

    /// True once it is pending and has been left alone for `quiet`.
    pub fn due(&self, now: Instant, quiet: Duration) -> bool {
        self.changed
            .is_some_and(|t| now.saturating_duration_since(t) >= quiet)
    }

    /// How long until it is due, for scheduling the repaint that will send it.
    ///
    /// Needed because nothing else wakes an idle window: without this an edit
    /// would sit until the user moved the mouse.
    pub fn wait_left(&self, now: Instant, quiet: Duration) -> Option<Duration> {
        self.changed
            .map(|t| quiet.saturating_sub(now.saturating_duration_since(t)))
    }

    /// It was sent, or is no longer wanted.
    pub fn clear(&mut self) {
        self.changed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_pending_until_something_changes() {
        let s = Settle::new();
        let t = Instant::now();
        assert!(!s.pending());
        assert!(!s.due(t + Duration::from_secs(60), FLASH_QUIET));
        assert_eq!(s.wait_left(t, FLASH_QUIET), None);
    }

    #[test]
    fn a_change_is_due_only_after_the_quiet_period() {
        let mut s = Settle::new();
        let t = Instant::now();
        s.touch(t);
        assert!(s.pending());
        assert!(!s.due(t, FLASH_QUIET));
        assert!(!s.due(t + Duration::from_millis(1999), FLASH_QUIET));
        assert!(s.due(t + Duration::from_millis(2000), FLASH_QUIET));
        assert!(s.due(t + Duration::from_secs(30), FLASH_QUIET));
    }

    #[test]
    fn every_further_change_pushes_the_send_back() {
        // A name typed a letter at a time: each keystroke restarts the wait, so
        // the write happens once, after the last one, and not once per letter.
        let mut s = Settle::new();
        let t = Instant::now();
        let mut sends = 0;
        for i in 0..10u64 {
            let now = t + Duration::from_millis(i * 300);
            s.touch(now);
            if s.due(now, FLASH_QUIET) {
                sends += 1;
                s.clear();
            }
        }
        assert_eq!(sends, 0, "typing at this pace never settles");
        assert!(s.due(t + Duration::from_millis(9 * 300 + 2000), FLASH_QUIET));
    }

    #[test]
    fn clearing_forgets_the_change() {
        let mut s = Settle::new();
        let t = Instant::now();
        s.touch(t);
        s.clear();
        assert!(!s.pending());
        assert!(!s.due(t + Duration::from_secs(60), FLASH_QUIET));
    }

    #[test]
    fn the_wait_left_counts_down_and_stops_at_zero() {
        let mut s = Settle::new();
        let t = Instant::now();
        s.touch(t);
        assert_eq!(s.wait_left(t, FLASH_QUIET), Some(FLASH_QUIET));
        assert_eq!(
            s.wait_left(t + Duration::from_millis(500), FLASH_QUIET),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(s.wait_left(t + Duration::from_secs(9), FLASH_QUIET), Some(Duration::ZERO));
    }

    #[test]
    fn a_clock_that_steps_back_does_not_panic_or_fire_early() {
        let mut s = Settle::new();
        let t = Instant::now() + Duration::from_secs(10);
        s.touch(t);
        let earlier = t - Duration::from_secs(5);
        assert!(!s.due(earlier, LIVE_QUIET));
        assert_eq!(s.wait_left(earlier, LIVE_QUIET), Some(LIVE_QUIET));
    }

    #[test]
    fn live_changes_settle_faster_than_flash_ones() {
        assert!(LIVE_QUIET < FLASH_QUIET);
    }

    #[test]
    fn only_a_ready_backup_allows_flash() {
        assert!(Safety::Ready.allows_flash());
        assert!(!Safety::Pending.allows_flash());
        assert!(!Safety::Failed("disk full".into()).allows_flash());
    }
}
