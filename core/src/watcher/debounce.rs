use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Coalesces rapid repeated events for the same path (editor autosave,
/// formatter-on-save) into a single downstream trigger: each `record` resets
/// that path's timer, and `drain_ready` only returns paths whose timer has
/// gone quiet for `window` - i.e. it fires on the trailing edge, once, per
/// burst. Different paths track independent timers, so unrelated files
/// never coalesce with each other.
///
/// Constructed and driven by `daemon::run`'s watcher thread since task 129
/// (`daemon::watch_and_route_once`, windowed by `daemon::DEBOUNCE_WINDOW`) -
/// see that thread's own comment for why this type, specifically, is what
/// closes the "one plugin round trip per raw event" gap, and why
/// `watcher::burst::BurstBatcher` is not wired in alongside it.
pub struct Debouncer {
    window: Duration,
    last_seen: HashMap<PathBuf, Instant>,
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self { window, last_seen: HashMap::new() }
    }

    /// Records a raw watcher event for `path`, resetting its debounce timer.
    pub fn record(&mut self, path: PathBuf) {
        self.record_at(path, Instant::now());
    }

    /// [`record`](Self::record) with the clock supplied rather than read - see
    /// [`drain_ready_at`](Self::drain_ready_at).
    fn record_at(&mut self, path: PathBuf, now: Instant) {
        self.last_seen.insert(path, now);
    }

    /// Returns every path whose debounce window has elapsed since its last
    /// recorded event, removing them so each ready path fires exactly once.
    pub fn drain_ready(&mut self) -> Vec<PathBuf> {
        self.drain_ready_at(Instant::now())
    }

    /// [`drain_ready`](Self::drain_ready) against a supplied clock.
    ///
    /// The `_at` pair exists so the tests can decide what time it is instead
    /// of sleeping: the window is a function of timestamps, and sleeping made
    /// it a function of the scheduler as well. Both macOS runners failed
    /// `a_fresh_event_during_the_window_extends_it` this way, on code nobody
    /// had touched (GM-245) - and it was not one runner's quirk, since the
    /// same shape failed on x86_64 and aarch64 in different runs.
    fn drain_ready_at(&mut self, now: Instant) -> Vec<PathBuf> {
        let ready: Vec<PathBuf> = self
            .last_seen
            .iter()
            .filter(|(_, &seen)| now.duration_since(seen) >= self.window)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &ready {
            self.last_seen.remove(path);
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed starting point, so each test states its timeline in
    /// milliseconds from it rather than sleeping.
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn coalesces_rapid_repeated_events_for_the_same_file_into_one_trigger() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let path = PathBuf::from("a.rs");

        let t0 = Instant::now();
        debouncer.record_at(path.clone(), t0);
        debouncer.record_at(path.clone(), at(t0, 10));
        debouncer.record_at(path.clone(), at(t0, 20));

        assert!(
            debouncer.drain_ready_at(at(t0, 60)).is_empty(),
            "40ms since the last event - still resetting, must not fire"
        );

        assert_eq!(
            debouncer.drain_ready_at(at(t0, 70)),
            vec![path],
            "exactly one trigger for the whole burst"
        );

        assert!(
            debouncer.drain_ready_at(at(t0, 200)).is_empty(),
            "an already-drained path must not fire again on its own"
        );
    }

    #[test]
    fn different_files_debounce_independently() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let a = PathBuf::from("a.rs");
        let b = PathBuf::from("b.rs");

        let t0 = Instant::now();
        debouncer.record_at(a.clone(), t0);
        // a's window has elapsed; b's is only just starting - they must not
        // be coalesced together into a single combined trigger.
        debouncer.record_at(b.clone(), at(t0, 60));

        assert_eq!(
            debouncer.drain_ready_at(at(t0, 60)),
            vec![a],
            "only a is ready; b's own window hasn't elapsed yet"
        );

        assert_eq!(debouncer.drain_ready_at(at(t0, 120)), vec![b]);
    }

    #[test]
    fn a_fresh_event_during_the_window_extends_it() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let path = PathBuf::from("a.rs");

        let t0 = Instant::now();
        debouncer.record_at(path.clone(), t0);
        // Resets the timer before it would have fired.
        debouncer.record_at(path.clone(), at(t0, 30));

        assert!(
            debouncer.drain_ready_at(at(t0, 60)).is_empty(),
            "30ms since the second record is still under the 50ms window - and 60ms since the \
             first, which is what would have fired had the second not reset it"
        );

        assert_eq!(debouncer.drain_ready_at(at(t0, 80)), vec![path]);
    }
}
