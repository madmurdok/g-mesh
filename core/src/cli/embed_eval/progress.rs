//! Progress lines for the long phases of `debug-embed-eval run` (node
//! embedding, query embedding, scoring). A line goes to the sink every tenth
//! of the phase, or when `MAX_SILENCE` has passed since the last one,
//! whichever comes first. The clock and the sink are injected so tests need
//! no real time; `run` passes a monotonic clock and stderr. Lines never reach
//! stdout or any result file.

use std::io::Write;
use std::time::{Duration, Instant};

/// The longest a phase stays silent between lines.
pub const MAX_SILENCE: Duration = Duration::from_secs(5 * 60);

pub struct Progress<W: Write, C: FnMut() -> Duration> {
    label: String,
    total: usize,
    clock: C,
    started: Duration,
    last_line: Duration,
    last_decile: usize,
    pub sink: W,
}

/// A reporter for one phase on stderr, timed by a monotonic clock.
pub fn stderr(
    variant: &str,
    corpus: &str,
    phase: &str,
    total: usize,
) -> Progress<std::io::Stderr, impl FnMut() -> Duration> {
    let origin = Instant::now();
    Progress::new(variant, corpus, phase, total, move || origin.elapsed(), std::io::stderr())
}

impl<W: Write, C: FnMut() -> Duration> Progress<W, C> {
    pub fn new(variant: &str, corpus: &str, phase: &str, total: usize, mut clock: C, sink: W) -> Self {
        let started = clock();
        Self {
            label: format!("[embed-eval {variant} {corpus} {phase}]"),
            total,
            clock,
            started,
            last_line: started,
            last_decile: 0,
            sink,
        }
    }

    /// Record that `done` of `total` items are finished, writing a line if a
    /// new tenth was crossed or `MAX_SILENCE` passed since the last line.
    /// Write errors are ignored: progress must never fail the run.
    pub fn tick(&mut self, done: usize) {
        if self.total == 0 || done == 0 {
            return;
        }
        let now = (self.clock)();
        let decile = done.min(self.total) * 10 / self.total;
        if decile <= self.last_decile && now.saturating_sub(self.last_line) < MAX_SILENCE {
            return;
        }
        self.last_decile = self.last_decile.max(decile);
        self.last_line = now;
        let elapsed = now.saturating_sub(self.started);
        let remaining = self.total.saturating_sub(done);
        let eta = elapsed.mul_f64(remaining as f64 / done as f64);
        let _ = writeln!(
            self.sink,
            "{} {done}/{} ({}%) elapsed {} eta {}",
            self.label,
            self.total,
            done.min(self.total) * 100 / self.total,
            format_duration(elapsed),
            format_duration(eta)
        );
        let _ = self.sink.flush();
    }
}

/// `41s`, `2m41s`, `1h03m07s`.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn reporter(total: usize, now: &Rc<Cell<Duration>>) -> Progress<Vec<u8>, impl FnMut() -> Duration> {
        let now = Rc::clone(now);
        Progress::new("jina-v2-base-code-int8", "g-mesh", "embed", total, move || now.get(), Vec::new())
    }

    fn lines(p: &Progress<Vec<u8>, impl FnMut() -> Duration>) -> Vec<String> {
        String::from_utf8(p.sink.clone()).unwrap().lines().map(str::to_string).collect()
    }

    /// Control: make `tick` return before writing (a reporter that never
    /// emits) and this fails on the count; drop the decile condition and it
    /// fails the same way.
    #[test]
    fn one_line_per_tenth_with_elapsed_and_eta() {
        let now = Rc::new(Cell::new(Duration::ZERO));
        let mut p = reporter(100, &now);
        for done in 1..=100 {
            now.set(Duration::from_secs(done as u64));
            p.tick(done);
        }
        let out = lines(&p);
        assert_eq!(out.len(), 10, "{out:?}");
        assert_eq!(
            out[0],
            "[embed-eval jina-v2-base-code-int8 g-mesh embed] 10/100 (10%) elapsed 10s eta 1m30s"
        );
        assert_eq!(
            out[9],
            "[embed-eval jina-v2-base-code-int8 g-mesh embed] 100/100 (100%) elapsed 1m40s eta 0s"
        );
    }

    /// Control: remove the `MAX_SILENCE` clause from `tick` and no line is
    /// written before the first tenth, so this fails on the count.
    #[test]
    fn a_slow_phase_still_speaks_every_five_minutes() {
        let now = Rc::new(Cell::new(Duration::ZERO));
        let mut p = reporter(1000, &now);
        // 99 items (under a tenth), each taking a minute.
        for done in 1..=99 {
            now.set(Duration::from_secs(60 * done as u64));
            p.tick(done);
        }
        let out = lines(&p);
        assert_eq!(out.len(), 19, "{out:?}");
        assert_eq!(
            out[0],
            "[embed-eval jina-v2-base-code-int8 g-mesh embed] 5/1000 (0%) elapsed 5m00s eta 16h35m00s"
        );
    }

    /// Control: drop the `total == 0` guard and the percent division panics.
    #[test]
    fn an_empty_phase_writes_nothing() {
        let now = Rc::new(Cell::new(Duration::ZERO));
        let mut p = reporter(0, &now);
        p.tick(0);
        p.tick(1);
        assert!(lines(&p).is_empty());
    }

    /// Control: make `format_duration` print raw seconds (`{}s` of
    /// `as_secs`) and the minute and hour cases fail.
    #[test]
    fn durations_read_as_hours_minutes_seconds() {
        assert_eq!(format_duration(Duration::from_millis(41_900)), "41s");
        assert_eq!(format_duration(Duration::from_secs(161)), "2m41s");
        assert_eq!(format_duration(Duration::from_secs(3787)), "1h03m07s");
    }
}
