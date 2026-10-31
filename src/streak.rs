//! Counts presses toward a streak and forgets stale counts; the clock is passed in so tests do not wait.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::actions::{Binding, Combo};

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

/// Matches button combos against the recent presses of every button.
#[derive(Default)]
pub struct Sequences {
    history: Vec<(u8, Instant)>,
}

/// Presses older than this can belong to no combo (the longest allowed gap).
const KEEP: Duration = Duration::from_millis(3000);

impl Sequences {
    pub fn new() -> Self {
        Self::default()
    }

    /// A press of `button`. Returns the indices of the combos it completes.
    ///
    /// A combo matches when the runs of presses ending now are exactly its steps
    /// (a run is one button pressed repeatedly), with no pause longer than its
    /// limit. It fires on the press that completes the last step.
    pub fn press(&mut self, combos: &[Combo], button: u8, now: Instant) -> Vec<usize> {
        self.history.retain(|(_, t)| now.saturating_duration_since(*t) <= KEEP);
        self.history.push((button, now));
        if self.history.len() > 64 {
            self.history.remove(0);
        }
        (0..combos.len()).filter(|i| matches(&self.history, &combos[*i])).collect()
    }
}

fn matches(history: &[(u8, Instant)], combo: &Combo) -> bool {
    let within = Duration::from_millis(combo.within_ms as u64);
    // Runs of the same button, newest first, stopping at the first long pause.
    let mut runs: Vec<(u8, u8)> = Vec::new();
    let mut later: Option<Instant> = None;
    for (b, t) in history.iter().rev() {
        if later.is_some_and(|l| l.saturating_duration_since(*t) > within) {
            break;
        }
        later = Some(*t);
        match runs.last_mut() {
            Some((rb, n)) if rb == b => *n = n.saturating_add(1),
            _ => runs.push((*b, 1)),
        }
    }
    runs.len() >= combo.steps.len()
        && combo.steps.iter().rev().zip(&runs).all(|(want, got)| want == got)
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

    fn combo(steps: &[(u8, u8)], within_ms: u16) -> Combo {
        Combo { steps: steps.to_vec(), within_ms, action: Action::Nothing }
    }

    fn feed(seq: &mut Sequences, c: &[Combo], presses: &[(u8, u64)], t: Instant) -> Vec<(usize, Vec<usize>)> {
        presses
            .iter()
            .enumerate()
            .map(|(k, (b, at))| (k, seq.press(c, *b, ms(t, *at))))
            .filter(|(_, f)| !f.is_empty())
            .collect()
    }

    #[test]
    fn sniper_twice_then_front_once_fires_on_the_last_press() {
        let c = [combo(&[(6, 2), (4, 1)], 500)];
        let mut s = Sequences::new();
        let fired = feed(&mut s, &c, &[(6, 0), (6, 150), (4, 300)], Instant::now());
        assert_eq!(fired, vec![(2, vec![0])]);
    }

    #[test]
    fn the_counts_must_be_exact() {
        let c = [combo(&[(6, 2), (4, 1)], 500)];
        let t = Instant::now();
        // Three sniper presses, not two.
        assert!(feed(&mut Sequences::new(), &c, &[(6, 0), (6, 100), (6, 200), (4, 300)], t).is_empty());
        // Only one.
        assert!(feed(&mut Sequences::new(), &c, &[(6, 0), (4, 100)], t).is_empty());
        // Wrong order.
        assert!(feed(&mut Sequences::new(), &c, &[(4, 0), (6, 100), (6, 200)], t).is_empty());
    }

    #[test]
    fn a_long_pause_breaks_the_combo() {
        let c = [combo(&[(6, 2), (4, 1)], 500)];
        let t = Instant::now();
        assert!(feed(&mut Sequences::new(), &c, &[(6, 0), (6, 100), (4, 800)], t).is_empty());
        // The pause before the combo starts does not matter.
        let fired = feed(&mut Sequences::new(), &c, &[(1, 0), (6, 5000), (6, 5100), (4, 5200)], t);
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn sniper_three_times_then_right_click_twice() {
        let c = [combo(&[(6, 3), (2, 2)], 500)];
        let mut s = Sequences::new();
        let fired = feed(&mut s, &c, &[(6, 0), (6, 100), (6, 200), (2, 300), (2, 400)], Instant::now());
        assert_eq!(fired, vec![(4, vec![0])]);
    }

    #[test]
    fn the_same_combo_can_run_again_and_other_presses_before_it_are_ignored() {
        let c = [combo(&[(6, 2), (4, 1)], 500)];
        let mut s = Sequences::new();
        let fired = feed(
            &mut s,
            &c,
            &[(1, 0), (6, 100), (6, 200), (4, 300), (6, 400), (6, 500), (4, 600)],
            Instant::now(),
        );
        assert_eq!(fired.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![3, 6]);
    }

    #[test]
    fn two_combos_can_share_a_start() {
        let c = [combo(&[(6, 2), (4, 1)], 500), combo(&[(6, 2), (5, 1)], 500)];
        let t = Instant::now();
        assert_eq!(feed(&mut Sequences::new(), &c, &[(6, 0), (6, 50), (4, 100)], t), vec![(2, vec![0])]);
        assert_eq!(feed(&mut Sequences::new(), &c, &[(6, 0), (6, 50), (5, 100)], t), vec![(2, vec![1])]);
    }
}
