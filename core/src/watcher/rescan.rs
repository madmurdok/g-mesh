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
