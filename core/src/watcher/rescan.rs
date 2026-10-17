//! Coalesces the watcher's dropped-event signals (GM-616) into few
//! reconciles: see [`RescanScheduler`].

use std::time::{Duration, Instant};

use crate::project_walk::under_any;

/// Quiet time after the last Rescan before the pending subtrees are
/// reconciled: a storm of drops is reconciled once, after it ends.
pub const RESCAN_SETTLE: Duration = Duration::from_secs(1);

/// The longest a pending Rescan waits, quiet or not, so a storm that never
/// stops is still reconciled.
pub const RESCAN_MAX_DELAY: Duration = Duration::from_secs(30);

/// A run never starts sooner than this many times the previous run's
/// duration after that run ended, so a slow reconcile of a huge root takes at
/// most about a quarter of the watcher thread during a storm.
pub const RESCAN_MIN_GAP_FACTOR: u32 = 3;

/// Holds the subtrees whose events the OS dropped until a reconcile of them
/// is due. Subtrees are project-relative, forward-slash separated; `""` is
/// the root.
///
/// - A subtree inside a pending one folds into it; a pending one inside a new
///   one is replaced by it; a root Rescan absorbs all others.
/// - [`due_at`](Self::due_at) fires once no Rescan has arrived for `settle`,
///   or once the oldest pending one is `max_delay` old - but never sooner
///   than `gap_factor` × the previous run's duration after that run ended
///   ([`ran_at`](Self::ran_at)).
/// - Rescans recorded while a run is in progress stay pending for the next
///   one, which therefore catches edits made during the previous run.
///
/// Every method has an `_at` twin taking the clock as an argument, like
/// [`Debouncer`](super::debounce::Debouncer), so the tests decide what time
/// it is instead of sleeping.
#[derive(Debug)]
pub struct RescanScheduler {
    settle: Duration,
    max_delay: Duration,
    gap_factor: u32,
    pending: Vec<String>,
    // When the oldest pending Rescan arrived, and the newest.
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    // The earliest the next run may start, set by `ran_at`.
    not_before: Option<Instant>,
}

impl Default for RescanScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl RescanScheduler {
    /// A scheduler with the daemon's timings: [`RESCAN_SETTLE`],
    /// [`RESCAN_MAX_DELAY`], [`RESCAN_MIN_GAP_FACTOR`].
    pub fn new() -> Self {
        Self::with_timings(RESCAN_SETTLE, RESCAN_MAX_DELAY, RESCAN_MIN_GAP_FACTOR)
    }

    /// A scheduler with the given timings; tests that drive the daemon's
    /// loop use a zero `settle` so a Rescan is due on the next iteration.
    pub fn with_timings(settle: Duration, max_delay: Duration, gap_factor: u32) -> Self {
        Self {
            settle,
            max_delay,
            gap_factor,
            pending: Vec::new(),
            first_at: None,
            last_at: None,
            not_before: None,
        }
    }

    /// Records a Rescan of `subtree`, now.
    pub fn record(&mut self, subtree: String) {
        self.record_at(subtree, Instant::now());
    }

    /// [`record`](Self::record) against a supplied clock.
    pub fn record_at(&mut self, subtree: String, now: Instant) {
        if self.pending.is_empty() {
            self.first_at = Some(now);
        }
        self.last_at = Some(now);
        if under_any(&subtree, &self.pending) {
            return;
        }
        self.pending.retain(|pending| !under_any(pending, std::slice::from_ref(&subtree)));
        self.pending.push(subtree);
    }

    /// The subtrees pending, sorted, without taking them.
    pub fn pending(&self) -> Vec<String> {
        let mut pending = self.pending.clone();
        pending.sort();
        pending
    }

    /// The pending subtrees, taken, when a run is due now; else `None`.
    pub fn due(&mut self) -> Option<Vec<String>> {
        self.due_at(Instant::now())
    }

    /// [`due`](Self::due) against a supplied clock.
    pub fn due_at(&mut self, now: Instant) -> Option<Vec<String>> {
        let (Some(first_at), Some(last_at)) = (self.first_at, self.last_at) else {
            return None;
        };
        if self.pending.is_empty() {
            return None;
        }
        if self.not_before.is_some_and(|not_before| now < not_before) {
            return None;
        }
        let quiet = now.saturating_duration_since(last_at) >= self.settle;
        let overdue = now.saturating_duration_since(first_at) >= self.max_delay;
        if !quiet && !overdue {
            return None;
        }
        self.first_at = None;
        self.last_at = None;
        let mut subtrees = std::mem::take(&mut self.pending);
        subtrees.sort();
        Some(subtrees)
    }

    /// Records that a run taking `took` has just ended.
    pub fn ran(&mut self, took: Duration) {
        self.ran_at(Instant::now(), took);
    }

    /// [`ran`](Self::ran) against a supplied clock: the run ended at `ended`.
    pub fn ran_at(&mut self, ended: Instant, took: Duration) {
        self.not_before = Some(ended + took.saturating_mul(self.gap_factor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// The daemon's own timings, so every timing test below pins the
    /// owner-approved numbers rather than ones of its own.
    fn daemon_scheduler() -> RescanScheduler {
        RescanScheduler::new()
    }

    #[test]
    fn the_daemon_timings_are_the_approved_ones() {
        assert_eq!(RESCAN_SETTLE, Duration::from_secs(1));
        assert_eq!(RESCAN_MAX_DELAY, Duration::from_secs(30));
        assert_eq!(RESCAN_MIN_GAP_FACTOR, 3);
    }

    #[test]
    fn repeated_rescans_of_one_subtree_are_pending_once() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        for i in 0..5 {
            scheduler.record_at("src".to_string(), t0 + ms(10 * i));
        }
        assert_eq!(scheduler.pending(), vec!["src".to_string()]);
    }

    #[test]
    fn a_nested_subtree_folds_into_its_pending_ancestor_whichever_comes_first() {
        let t0 = Instant::now();

        let mut ancestor_first = daemon_scheduler();
        ancestor_first.record_at("src".to_string(), t0);
        ancestor_first.record_at("src/deep/er".to_string(), t0);
        assert_eq!(ancestor_first.pending(), vec!["src".to_string()]);

        let mut nested_first = daemon_scheduler();
        nested_first.record_at("src/deep".to_string(), t0);
        nested_first.record_at("src/other".to_string(), t0);
        nested_first.record_at("src".to_string(), t0);
        assert_eq!(nested_first.pending(), vec!["src".to_string()]);
    }

    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_folded() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        scheduler.record_at("src".to_string(), t0);
        scheduler.record_at("src2".to_string(), t0);
        scheduler.record_at("lib/a".to_string(), t0);
        assert_eq!(scheduler.pending(), vec!["lib/a".to_string(), "src".to_string(), "src2".to_string()]);
    }

    #[test]
    fn a_root_rescan_absorbs_every_other_subtree() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        scheduler.record_at("src".to_string(), t0);
        scheduler.record_at("lib/a".to_string(), t0);
        scheduler.record_at(String::new(), t0);
        assert_eq!(scheduler.pending(), vec![String::new()]);

        scheduler.record_at("docs".to_string(), t0 + ms(1));
        assert_eq!(scheduler.pending(), vec![String::new()], "a subtree after the root folds into it");
    }

    /// Behaviour 7: a burst of Rescans 100 ms apart fires once, only after
    /// `RESCAN_SETTLE` of quiet following the last one, with every subtree.
    #[test]
    fn a_burst_fires_once_after_the_settle_time_of_quiet() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        let mut last = t0;
        for i in 0..10u64 {
            last = t0 + ms(100 * i);
            scheduler.record_at(format!("dir{}", i % 3), last);
            assert_eq!(scheduler.due_at(last), None, "nothing fires mid-burst (rescan {i})");
            assert_eq!(scheduler.due_at(last + ms(99)), None, "nor before the next rescan arrives");
        }
        assert_eq!(scheduler.due_at(last + RESCAN_SETTLE - ms(1)), None, "not before the settle time");
        assert_eq!(
            scheduler.due_at(last + RESCAN_SETTLE),
            Some(vec!["dir0".to_string(), "dir1".to_string(), "dir2".to_string()]),
            "one run for the whole burst, once it has been quiet for the settle time"
        );
        assert_eq!(scheduler.due_at(last + RESCAN_SETTLE * 5), None, "the burst fires once");
        assert!(scheduler.pending().is_empty());
    }

    /// Behaviour 8 (cap): Rescans 100 ms apart that never stop still fire,
    /// first at `RESCAN_MAX_DELAY` after the oldest pending one, not before.
    #[test]
    fn a_storm_that_never_stops_fires_at_the_max_delay() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        let mut fired_at = None;
        for i in 0..400u64 {
            let now = t0 + ms(100 * i);
            scheduler.record_at(String::new(), now);
            if scheduler.due_at(now).is_some() {
                fired_at = Some(now);
                break;
            }
        }
        let fired_at = fired_at.expect("a continuous storm must still fire");
        assert_eq!(fired_at - t0, RESCAN_MAX_DELAY, "it fires at the max delay, not sooner and not later");
    }

    /// Behaviour 8 (gap): after a run that took `took`, nothing fires until
    /// `RESCAN_MIN_GAP_FACTOR × took` after it ended, quiet or overdue; at
    /// that point the pending subtrees fire.
    #[test]
    fn no_run_starts_sooner_than_the_gap_factor_after_the_previous_one() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        let took = Duration::from_secs(20);
        scheduler.ran_at(t0, took);
        let not_before = t0 + took * RESCAN_MIN_GAP_FACTOR;

        scheduler.record_at("src".to_string(), t0 + ms(1));
        // Quiet for far longer than the settle time, and overdue past the
        // max delay, yet still inside the gap.
        assert!(t0 + ms(1) + RESCAN_MAX_DELAY < not_before);
        assert_eq!(scheduler.due_at(t0 + ms(1) + RESCAN_SETTLE), None, "quiet, but inside the gap");
        assert_eq!(scheduler.due_at(t0 + ms(1) + RESCAN_MAX_DELAY), None, "overdue, but inside the gap");
        assert_eq!(scheduler.due_at(not_before - ms(1)), None, "one moment before the gap ends");
        assert_eq!(scheduler.due_at(not_before), Some(vec!["src".to_string()]), "fires as the gap ends");
    }

    /// Rescans that arrive while a run is in progress (after `due_at` took
    /// the pending set, before `ran_at`) stay pending for the next run.
    #[test]
    fn rescans_recorded_during_a_run_stay_pending_for_the_next_one() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        scheduler.record_at("src".to_string(), t0);
        let started = t0 + RESCAN_SETTLE;
        assert_eq!(scheduler.due_at(started), Some(vec!["src".to_string()]));

        scheduler.record_at("lib".to_string(), started + ms(200));
        let ended = started + ms(500);
        scheduler.ran_at(ended, ended - started);
        assert_eq!(scheduler.pending(), vec!["lib".to_string()], "the run did not take the later rescan");

        // Past both the settle time and the gap after the first run.
        let next = ended + (ended - started) * RESCAN_MIN_GAP_FACTOR;
        assert!(next >= started + ms(200) + RESCAN_SETTLE);
        assert_eq!(scheduler.due_at(next), Some(vec!["lib".to_string()]), "the next run reconciles it");
    }

    #[test]
    fn nothing_pending_is_never_due() {
        let mut scheduler = daemon_scheduler();
        let t0 = Instant::now();
        assert_eq!(scheduler.due_at(t0 + RESCAN_MAX_DELAY * 2), None);
    }
}
