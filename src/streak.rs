//! Counts presses toward a streak and forgets stale counts; the clock is passed in so tests do not wait.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::actions::Binding;

#[derive(Default)]
pub struct Streaks {
    /// Per button: presses so far in the current streak, and when the last was.
    state: HashMap<u8, (u8, Instant)>,
}

impl Streaks {
    pub fn new() -> Self {
        Self::default()
    }

    /// A press of `bind.button` at `now`; true when the action should run.
    /// A count of one runs every press with no state; longer runs on the press completing the streak, then restarts.
    pub fn press(&mut self, bind: &Binding, now: Instant) -> bool {
        if bind.presses <= 1 {
            return true;
        }
        let within = Duration::from_millis(bind.within_ms as u64);
        let entry = self.state.entry(bind.button).or_insert((0, now));
        // `saturating`: a clock that stepped back counts as no time passing.
        if entry.0 > 0 && now.saturating_duration_since(entry.1) > within {
            entry.0 = 0;
        }
        entry.0 += 1;
        entry.1 = now;
        if entry.0 >= bind.presses {
            self.state.remove(&bind.button);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Action, BindMode};

    fn bind(button: u8, presses: u8, within_ms: u16) -> Binding {
        let mut b = Binding::new(button, Action::Nothing, BindMode::Augment);
        b.presses = presses;
        b.within_ms = within_ms;
        b
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn a_single_press_binding_fires_every_time() {
        let mut s = Streaks::new();
        let b = bind(6, 1, 500);
        let t = Instant::now();
        assert!(s.press(&b, t));
        assert!(s.press(&b, t));
        assert!(s.press(&b, ms(t, 10_000)));
    }

    #[test]
    fn nothing_happens_until_the_last_press_of_the_streak() {
        let mut s = Streaks::new();
        let b = bind(6, 3, 500);
        let t = Instant::now();
        assert!(!s.press(&b, t));
        assert!(!s.press(&b, ms(t, 200)));
        assert!(s.press(&b, ms(t, 400)));
    }

    #[test]
    fn a_pause_that_is_too_long_starts_the_count_again() {
        let mut s = Streaks::new();
        let b = bind(6, 3, 500);
        let t = Instant::now();
        assert!(!s.press(&b, t));
        assert!(!s.press(&b, ms(t, 200)));
        // 700 ms after the last: the first two are forgotten, this is press one.
        assert!(!s.press(&b, ms(t, 900)));
        assert!(!s.press(&b, ms(t, 1000)));
        assert!(s.press(&b, ms(t, 1100)));
    }

    #[test]
    fn the_limit_is_the_gap_between_presses_not_the_whole_streak() {
        // A slow, steady streak counts: 400 ms apart, a full second end to end.
        let mut s = Streaks::new();
        let b = bind(6, 3, 500);
        let t = Instant::now();
        assert!(!s.press(&b, t));
        assert!(!s.press(&b, ms(t, 400)));
        assert!(s.press(&b, ms(t, 800)));
        // And a gap exactly at the limit still counts.
        assert!(!s.press(&b, ms(t, 2000)));
        assert!(!s.press(&b, ms(t, 2500)));
        assert!(s.press(&b, ms(t, 3000)));
    }

    #[test]
    fn it_starts_over_after_firing() {
        let mut s = Streaks::new();
        let b = bind(6, 2, 500);
        let t = Instant::now();
        let fired: Vec<bool> = (0..6).map(|i| s.press(&b, ms(t, i * 100))).collect();
        assert_eq!(fired, [false, true, false, true, false, true]);
    }

    #[test]
    fn buttons_are_counted_separately() {
        let mut s = Streaks::new();
        let a = bind(6, 2, 500);
        let c = bind(7, 2, 500);
        let t = Instant::now();
        assert!(!s.press(&a, t));
        assert!(!s.press(&c, ms(t, 50)));
        assert!(s.press(&a, ms(t, 100)));
        assert!(s.press(&c, ms(t, 150)));
    }

    #[test]
    fn a_clock_that_steps_back_does_not_panic() {
        let mut s = Streaks::new();
        let b = bind(6, 2, 500);
        let t = Instant::now() + Duration::from_secs(10);
        assert!(!s.press(&b, t));
        assert!(s.press(&b, t - Duration::from_secs(5)));
    }
}
