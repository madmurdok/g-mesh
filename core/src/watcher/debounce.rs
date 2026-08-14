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
        self.last_seen.insert(path, Instant::now());
    }

    /// Returns every path whose debounce window has elapsed since its last
    /// recorded event, removing them so each ready path fires exactly once.
    pub fn drain_ready(&mut self) -> Vec<PathBuf> {
        let now = Instant::now();
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
    use std::thread;

    #[test]
    fn coalesces_rapid_repeated_events_for_the_same_file_into_one_trigger() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let path = PathBuf::from("a.rs");

        debouncer.record(path.clone());
        thread::sleep(Duration::from_millis(10));
        debouncer.record(path.clone());
        thread::sleep(Duration::from_millis(10));
        debouncer.record(path.clone());

        assert!(debouncer.drain_ready().is_empty(), "must not fire while events keep resetting the timer");

        thread::sleep(Duration::from_millis(60));
        assert_eq!(debouncer.drain_ready(), vec![path], "exactly one trigger for the whole burst");

        assert!(debouncer.drain_ready().is_empty(), "an already-drained path must not fire again on its own");
    }

    #[test]
    fn different_files_debounce_independently() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let a = PathBuf::from("a.rs");
        let b = PathBuf::from("b.rs");

        debouncer.record(a.clone());
        thread::sleep(Duration::from_millis(60));
        // a's window has elapsed; b's is only just starting - they must not
        // be coalesced together into a single combined trigger.
        debouncer.record(b.clone());

        assert_eq!(debouncer.drain_ready(), vec![a], "only a is ready; b's own window hasn't elapsed yet");

        thread::sleep(Duration::from_millis(60));
        assert_eq!(debouncer.drain_ready(), vec![b]);
    }

    #[test]
    fn a_fresh_event_during_the_window_extends_it() {
        let mut debouncer = Debouncer::new(Duration::from_millis(50));
        let path = PathBuf::from("a.rs");

        debouncer.record(path.clone());
        thread::sleep(Duration::from_millis(30));
        debouncer.record(path.clone()); // resets the timer before it would have fired
        thread::sleep(Duration::from_millis(30));

        assert!(debouncer.drain_ready().is_empty(), "30ms since the second record is still under the 50ms window");

        thread::sleep(Duration::from_millis(30));
        assert_eq!(debouncer.drain_ready(), vec![path]);
    }
}
