//! Detects an event stream that has gone silent while the project still
//! changes (FSEvents can starve one stream under heavy churn elsewhere) and
//! says when the consumer should read the disk instead: see
//! [`StarvationWatchdog`]. Design: `docs/design/GM-727-starvation-watchdog.md`.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::rescan::RESCAN_MIN_GAP_FACTOR;

/// How long the stream must have delivered no signal before a canary probe
/// is written. A healthy stream delivers an edit in about a second, so while
/// events flow nothing is written.
pub const QUIET_BEFORE_PROBE: Duration = Duration::from_secs(5);

/// The most often a probe is written while the project is quiet.
pub const CANARY_PERIOD: Duration = Duration::from_secs(10);

/// How long a written probe may stay unheard before the stream counts as
/// starved.
pub const CANARY_DEADLINE: Duration = Duration::from_secs(5);

/// While starved, sweeps start at least this far apart (and never sooner than
/// [`RESCAN_MIN_GAP_FACTOR`] × the previous sweep's duration after it ended).
/// Without a canary, it is also the first backoff interval.
pub const STARVED_SWEEP_EVERY: Duration = Duration::from_secs(15);

/// Without a canary, the longest the backoff between empty sweeps grows to.
pub const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// A probe the watchdog writes into a directory watched by the project's own
/// event stream, and whether the stream has delivered it.
pub trait Canary {
    /// Writes a probe.
    fn probe(&self) -> io::Result<()>;
    /// Whether the stream delivered a probe since the last call; clears it.
    fn take_heard(&self) -> bool;
}

/// The production [`Canary`]: writes `<dir>/probe`; the watcher's callback
/// sets `heard` when an event under `dir` arrives
/// ([`ProjectWatcher::canary`](super::ProjectWatcher::canary)).
pub struct FileCanary {
    probe_path: PathBuf,
    heard: Arc<AtomicBool>,
    written: AtomicU64,
}

impl FileCanary {
    pub(crate) fn new(dir: PathBuf, heard: Arc<AtomicBool>) -> Self {
        Self { probe_path: dir.join("probe"), heard, written: AtomicU64::new(0) }
    }
}

impl Canary for FileCanary {
    fn probe(&self) -> io::Result<()> {
        let n = self.written.fetch_add(1, Ordering::Relaxed);
        std::fs::write(&self.probe_path, n.to_string())
    }

    fn take_heard(&self) -> bool {
        self.heard.swap(false, Ordering::Relaxed)
    }
}

/// The timings a [`StarvationWatchdog`] runs with; [`Default`] is the
/// daemon's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarvationTimings {
    pub quiet_before_probe: Duration,
    pub canary_period: Duration,
    pub canary_deadline: Duration,
    pub sweep_every: Duration,
    pub gap_factor: u32,
    pub backoff_cap: Duration,
}

impl Default for StarvationTimings {
    fn default() -> Self {
        Self {
            quiet_before_probe: QUIET_BEFORE_PROBE,
            canary_period: CANARY_PERIOD,
            canary_deadline: CANARY_DEADLINE,
            sweep_every: STARVED_SWEEP_EVERY,
            gap_factor: RESCAN_MIN_GAP_FACTOR,
            backoff_cap: BACKOFF_CAP,
        }
    }
}

/// What the consumer does after [`StarvationWatchdog::decide_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Idle,
    /// Write a canary probe; on failure, call
    /// [`probe_failed`](StarvationWatchdog::probe_failed).
    Probe,
    /// Read the disk, then call [`swept_at`](StarvationWatchdog::swept_at).
    Sweep,
}

/// A starvation episode that has just ended, for its log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeEnd {
    /// From the last signal before the episode to the end.
    pub silent_for: Duration,
    pub sweeps: usize,
    /// What the sweeps changed, summed ([`StarvationWatchdog::swept_at`]).
    pub changed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Starved = quiet and a probe overdue.
    Canary,
    /// No canary: sweep after `interval` of quiet, doubling after each sweep
    /// that changed nothing.
    Backoff { interval: Duration },
}

#[derive(Debug, Clone, Copy)]
struct Episode {
    silent_since: Instant,
    sweeps: usize,
    changed: usize,
}

/// A state machine on the watcher's consumer thread that decides when the
/// stream is starved and a sweep of the disk is due.
///
/// - With a canary: starved means no signal for `quiet_before_probe` **and**
///   a probe written at least `canary_deadline` ago still unheard. Probes are
///   written only while quiet, at most every `canary_period`; each sweep
///   counts as a new probe written when it ends, so the end of starvation is
///   seen even when an earlier probe was lost.
/// - Without one: a sweep after `sweep_every` with no signal, then after
///   twice the interval for every sweep that changed nothing (capped at
///   `backoff_cap`); a signal or a sweep that changed something resets it.
///
/// Either way, sweeps start at least `sweep_every` apart and never sooner
/// than `gap_factor` × the previous sweep's duration after it ended. Any
/// signal, or a heard probe, ends an episode: no sweep happens while the
/// stream delivers.
///
/// Every input carries the clock (`_at`), like
/// [`RescanScheduler`](super::rescan::RescanScheduler), so tests decide the
/// time.
#[derive(Debug)]
pub struct StarvationWatchdog {
    timings: StarvationTimings,
    mode: Mode,
    // The last signal observed (or the first observation).
    last_signal: Option<Instant>,
    // The outstanding, not yet heard probe's write time.
    probe_at: Option<Instant>,
    // The latest probe write, outstanding or not, for `canary_period`.
    last_probe: Option<Instant>,
    // The latest sweep's end, for the backoff interval.
    last_sweep: Option<Instant>,
    not_before: Option<Instant>,
    episode: Option<Episode>,
    ended: Option<EpisodeEnd>,
}

impl StarvationWatchdog {
    /// The canary-gated watchdog with the daemon's timings.
    pub fn new() -> Self {
        Self::with_timings(StarvationTimings::default())
    }

    /// The canary-gated watchdog with `timings`.
    pub fn with_timings(timings: StarvationTimings) -> Self {
        Self::build(timings, Mode::Canary)
    }

    /// The backoff watchdog for a watcher without a canary, with the daemon's
    /// timings.
    pub fn without_canary() -> Self {
        Self::without_canary_with_timings(StarvationTimings::default())
    }

    /// The backoff watchdog with `timings`.
    pub fn without_canary_with_timings(timings: StarvationTimings) -> Self {
        Self::build(timings, Mode::Backoff { interval: timings.sweep_every })
    }

    fn build(timings: StarvationTimings, mode: Mode) -> Self {
        Self {
            timings,
            mode,
            last_signal: None,
            probe_at: None,
            last_probe: None,
            last_sweep: None,
            not_before: None,
            episode: None,
            ended: None,
        }
    }

    /// Whether this watchdog is gated by a canary.
    pub fn uses_canary(&self) -> bool {
        self.mode == Mode::Canary
    }

    /// Whether a starvation episode is in progress (a sweep has been asked
    /// for since the last signal or heard probe).
    pub fn starved(&self) -> bool {
        self.episode.is_some()
    }

    /// Whether the episode in progress has not swept yet: the sweep
    /// [`decide_at`](Self::decide_at) just asked for is its first.
    pub fn first_sweep_of_episode(&self) -> bool {
        self.episode.is_some_and(|episode| episode.sweeps == 0)
    }

    /// The last signal before the episode in progress.
    pub fn silent_since(&self) -> Option<Instant> {
        self.episode.map(|episode| episode.silent_since)
    }

    /// Records one consumer iteration at `now`: `signal_seen` is whether the
    /// watcher received any signal (ignored paths and Rescans included). The
    /// first call starts the quiet clock either way. A signal ends an episode
    /// and drops the outstanding probe.
    pub fn observe_at(&mut self, now: Instant, signal_seen: bool) {
        if !signal_seen && self.last_signal.is_some() {
            return;
        }
        self.last_signal = Some(now);
        if signal_seen {
            self.probe_at = None;
            if let Mode::Backoff { interval } = &mut self.mode {
                *interval = self.timings.sweep_every;
            }
            self.end_episode(now);
        }
    }

    /// What to do at `now`; `canary_heard` is the canary's
    /// [`take_heard`](Canary::take_heard). A heard probe ends an episode.
    /// Returning [`Action::Probe`] records the probe as written at `now`.
    pub fn decide_at(&mut self, now: Instant, canary_heard: bool) -> Action {
        if canary_heard {
            self.probe_at = None;
            self.end_episode(now);
        }
        let Some(last_signal) = self.last_signal else {
            self.last_signal = Some(now);
            return Action::Idle;
        };
        let quiet = now.saturating_duration_since(last_signal);
        if quiet < self.timings.quiet_before_probe {
            return Action::Idle;
        }
        let sweep_allowed = self.not_before.is_none_or(|not_before| now >= not_before);
        match self.mode {
            Mode::Canary => match self.probe_at {
                Some(probed) => {
                    let overdue = now.saturating_duration_since(probed) >= self.timings.canary_deadline;
                    if overdue && sweep_allowed {
                        self.start_episode(last_signal);
                        Action::Sweep
                    } else {
                        Action::Idle
                    }
                }
                None => {
                    let due = self.last_probe.is_none_or(|written| {
                        now.saturating_duration_since(written) >= self.timings.canary_period
                    });
                    if due {
                        self.probe_at = Some(now);
                        self.last_probe = Some(now);
                        Action::Probe
                    } else {
                        Action::Idle
                    }
                }
            },
            Mode::Backoff { interval } => {
                let since = self.last_sweep.map_or(last_signal, |swept| swept.max(last_signal));
                if now.saturating_duration_since(since) >= interval && sweep_allowed {
                    self.start_episode(last_signal);
                    Action::Sweep
                } else {
                    Action::Idle
                }
            }
        }
    }

    /// The probe [`decide_at`](Self::decide_at) asked for could not be
    /// written: nothing is outstanding, and the next probe waits a full
    /// `canary_period`.
    pub fn probe_failed(&mut self) {
        self.probe_at = None;
    }

    /// Records a sweep that ended at `ended` after `took` and changed
    /// `changed` things. With a canary, the consumer writes a new probe right
    /// after it, recorded here as written at `ended`.
    pub fn swept_at(&mut self, ended: Instant, took: Duration, changed: usize) {
        let started = ended.checked_sub(took).unwrap_or(ended);
        let by_period = started + self.timings.sweep_every;
        let by_gap = ended + took.saturating_mul(self.timings.gap_factor);
        self.not_before = Some(by_period.max(by_gap));
        self.last_sweep = Some(ended);
        if let Some(episode) = &mut self.episode {
            episode.sweeps += 1;
            episode.changed += changed;
        }
        match &mut self.mode {
            Mode::Canary => {
                self.probe_at = Some(ended);
                self.last_probe = Some(ended);
            }
            Mode::Backoff { interval } => {
                *interval = if changed > 0 {
                    self.timings.sweep_every
                } else {
                    interval.saturating_mul(2).min(self.timings.backoff_cap)
                };
            }
        }
    }

    /// The episode that the latest signal or heard probe ended, taken.
    pub fn take_episode_end(&mut self) -> Option<EpisodeEnd> {
        self.ended.take()
    }

    fn start_episode(&mut self, silent_since: Instant) {
        if self.episode.is_none() {
            self.episode = Some(Episode { silent_since, sweeps: 0, changed: 0 });
        }
    }

    fn end_episode(&mut self, now: Instant) {
        if let Some(episode) = self.episode.take() {
            self.ended = Some(EpisodeEnd {
                silent_for: now.saturating_duration_since(episode.silent_since),
                sweeps: episode.sweeps,
                changed: episode.changed,
            });
        }
    }
}

impl Default for StarvationWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `t0 + secs`, to the millisecond.
    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_millis((secs * 1000.0).round() as u64)
    }

    /// A canary watchdog that observed its first (signal-less) iteration at
    /// `t0`, then was asked at `t0 + 5` (a probe) and at `t0 + 10` (the probe
    /// overdue: the episode's first sweep).
    fn starved_at_ten(t0: Instant) -> StarvationWatchdog {
        let mut watchdog = StarvationWatchdog::new();
        watchdog.observe_at(t0, false);
        assert_eq!(watchdog.decide_at(at(t0, 5.0), false), Action::Probe);
        assert_eq!(watchdog.decide_at(at(t0, 10.0), false), Action::Sweep);
        watchdog
    }

    /// The daemon runs with the owner's numbers: a probe after 5 s of quiet,
    /// at most every 10 s, starved 5 s after an unheard probe, sweeps at
    /// least 15 s apart and 3x the last sweep's duration after it.
    #[test]
    fn the_daemon_timings_are_the_decided_ones() {
        let timings = StarvationTimings::default();
        assert_eq!(timings.quiet_before_probe, Duration::from_secs(5));
        assert_eq!(timings.canary_period, Duration::from_secs(10));
        assert_eq!(timings.canary_deadline, Duration::from_secs(5));
        assert_eq!(timings.sweep_every, Duration::from_secs(15));
        assert_eq!(timings.gap_factor, 3);
        assert!(StarvationWatchdog::new().uses_canary());
        assert!(!StarvationWatchdog::without_canary().uses_canary());
    }

    /// A probe is written only after the quiet, and the stream is starved
    /// only once that probe stays unheard past its deadline.
    ///
    /// Control: drop the `quiet < quiet_before_probe` return in `decide_at` -
    /// a probe is asked for at 4.999 s.
    #[test]
    fn a_probe_waits_for_the_quiet_and_a_sweep_for_the_probes_deadline() {
        let t0 = Instant::now();
        let mut watchdog = StarvationWatchdog::new();
        watchdog.observe_at(t0, false);
        assert_eq!(watchdog.decide_at(at(t0, 4.999), false), Action::Idle, "not quiet long enough");
        assert_eq!(watchdog.decide_at(at(t0, 5.0), false), Action::Probe);
        assert_eq!(watchdog.decide_at(at(t0, 9.999), false), Action::Idle, "the probe is not overdue");
        assert!(!watchdog.starved());
        assert_eq!(watchdog.decide_at(at(t0, 10.0), false), Action::Sweep);
        assert!(watchdog.starved());
        assert!(watchdog.first_sweep_of_episode());
        assert_eq!(watchdog.silent_since(), Some(t0));
    }

    /// The guarantee: while signals arrive more often than the quiet, the
    /// watchdog neither probes nor sweeps, with or without a canary, however
    /// long it goes on and however the canary answers.
    ///
    /// Control: in `observe_at`, return early when a signal is seen (the
    /// last-signal stamp is never refreshed) - the canary watchdog probes
    /// and sweeps.
    #[test]
    fn no_probe_and_no_sweep_while_signals_are_delivered() {
        for mut watchdog in [StarvationWatchdog::new(), StarvationWatchdog::without_canary()] {
            let t0 = Instant::now();
            watchdog.observe_at(t0, false);
            // A signal every 4 s for 20 minutes, the watchdog asked every 0.5 s.
            for step in 0..2400u32 {
                let now = at(t0, f64::from(step) * 0.5);
                watchdog.observe_at(now, step % 8 == 0);
                let action = watchdog.decide_at(now, false);
                assert_eq!(action, Action::Idle, "at {} s ({:?})", f64::from(step) * 0.5, watchdog.mode);
            }
            assert!(!watchdog.starved());
        }
    }

    /// B4: while starved, a sweep starts at least `sweep_every` after the
    /// previous one started and at least 3x its duration after it ended,
    /// even though the probe is long overdue.
    ///
    /// Control: drop `sweep_allowed` from the overdue check in `decide_at` -
    /// a sweep is asked for at 24.999 s.
    #[test]
    fn sweeps_are_at_least_the_interval_apart_and_three_times_the_last_duration_after_it() {
        let t0 = Instant::now();
        let mut watchdog = starved_at_ten(t0);
        // Took 1 s: the interval (10 + 15) binds, not the gap (11 + 3).
        watchdog.swept_at(at(t0, 11.0), Duration::from_secs(1), 0);
        assert_eq!(
            watchdog.decide_at(at(t0, 16.0), false),
            Action::Idle,
            "the probe is overdue, the interval is not"
        );
        assert_eq!(watchdog.decide_at(at(t0, 24.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 25.0), false), Action::Sweep);
        assert!(!watchdog.first_sweep_of_episode(), "the same episode");

        // Took 10 s: the gap (35 + 30) binds, not the interval (25 + 15).
        watchdog.swept_at(at(t0, 35.0), Duration::from_secs(10), 0);
        assert_eq!(watchdog.decide_at(at(t0, 40.0), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 64.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 65.0), false), Action::Sweep);
    }

    /// B8: a signal ends the episode (reported once, with its sweeps and
    /// changes), and no sweep comes before a new quiet and a new unheard
    /// probe: 10 s of quiet after the signal, which starts a new episode.
    ///
    /// Control: drop `end_episode` in `observe_at` - the episode is still in
    /// progress after the signal and no end is reported.
    #[test]
    fn a_signal_ends_the_episode_and_no_sweep_comes_before_a_new_quiet() {
        let t0 = Instant::now();
        let mut watchdog = starved_at_ten(t0);
        watchdog.swept_at(at(t0, 10.5), Duration::from_millis(500), 2);
        assert_eq!(watchdog.decide_at(at(t0, 25.0), false), Action::Sweep);
        watchdog.swept_at(at(t0, 25.5), Duration::from_millis(500), 1);

        let signal = at(t0, 36.0);
        watchdog.observe_at(signal, true);
        assert!(!watchdog.starved(), "a signal ends the episode");
        assert_eq!(
            watchdog.take_episode_end(),
            Some(EpisodeEnd { silent_for: Duration::from_secs(36), sweeps: 2, changed: 3 })
        );
        assert_eq!(watchdog.take_episode_end(), None, "reported once");

        for tenth in 0..50u32 {
            let now = signal + Duration::from_millis(u64::from(tenth) * 100);
            assert_eq!(watchdog.decide_at(now, false), Action::Idle, "{tenth} tenths after the signal");
        }
        assert_eq!(watchdog.decide_at(at(t0, 41.0), false), Action::Probe, "a new probe after the quiet");
        assert_eq!(watchdog.decide_at(at(t0, 45.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 46.0), false), Action::Sweep);
        assert!(watchdog.first_sweep_of_episode(), "a new episode");
        assert_eq!(watchdog.silent_since(), Some(signal));
    }

    /// B8: a heard canary ends the episode too, and the next sweep again
    /// needs a probe that goes unheard past its deadline.
    ///
    /// Control: ignore `canary_heard` in `decide_at` - the episode is still
    /// in progress after the heard probe.
    #[test]
    fn a_heard_canary_ends_the_episode_and_the_next_sweep_needs_a_new_unheard_probe() {
        let t0 = Instant::now();
        let mut watchdog = starved_at_ten(t0);
        // The sweep's own probe, written at its end.
        watchdog.swept_at(at(t0, 10.0), Duration::ZERO, 0);

        assert_eq!(watchdog.decide_at(at(t0, 12.0), true), Action::Idle);
        assert!(!watchdog.starved(), "a heard probe ends the episode");
        assert_eq!(
            watchdog.take_episode_end(),
            Some(EpisodeEnd { silent_for: Duration::from_secs(12), sweeps: 1, changed: 0 })
        );
        assert_eq!(
            watchdog.decide_at(at(t0, 19.999), false),
            Action::Idle,
            "the probe period has not passed"
        );
        assert_eq!(watchdog.decide_at(at(t0, 25.0), false), Action::Probe);
        assert_eq!(watchdog.decide_at(at(t0, 29.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 30.0), false), Action::Sweep);
    }

    /// A probe that could not be written leaves nothing outstanding: no
    /// sweep from it, and the next probe a full period after it.
    #[test]
    fn a_failed_probe_starts_no_sweep_and_the_next_waits_a_period() {
        let t0 = Instant::now();
        let mut watchdog = StarvationWatchdog::new();
        watchdog.observe_at(t0, false);
        assert_eq!(watchdog.decide_at(at(t0, 5.0), false), Action::Probe);
        watchdog.probe_failed();
        assert_eq!(watchdog.decide_at(at(t0, 10.0), false), Action::Idle, "no probe is outstanding");
        assert_eq!(watchdog.decide_at(at(t0, 14.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 15.0), false), Action::Probe);
        assert_eq!(watchdog.decide_at(at(t0, 20.0), false), Action::Sweep);
    }

    /// Without a canary: a sweep after 15 s of quiet, then twice the
    /// interval after each sweep that changed nothing, capped at 5 minutes;
    /// a sweep that changed something, or a signal, resets it.
    ///
    /// Control: keep the interval in `swept_at` when nothing changed - the
    /// second sweep comes at 30 s.
    #[test]
    fn without_a_canary_empty_sweeps_back_off_to_the_cap_and_a_change_or_signal_resets_it() {
        let t0 = Instant::now();
        let mut watchdog = StarvationWatchdog::without_canary();
        watchdog.observe_at(t0, false);
        assert_eq!(watchdog.decide_at(at(t0, 14.999), false), Action::Idle);
        assert_eq!(watchdog.decide_at(at(t0, 15.0), false), Action::Sweep);
        watchdog.swept_at(at(t0, 15.0), Duration::ZERO, 0);
        assert_eq!(watchdog.decide_at(at(t0, 44.999), false), Action::Idle, "doubled to 30 s");
        assert_eq!(watchdog.decide_at(at(t0, 45.0), false), Action::Sweep);
        watchdog.swept_at(at(t0, 45.0), Duration::ZERO, 0);
        assert_eq!(watchdog.decide_at(at(t0, 104.999), false), Action::Idle, "doubled to 60 s");
        assert_eq!(watchdog.decide_at(at(t0, 105.0), false), Action::Sweep);

        // Empty sweeps until the interval reaches the cap and stays there.
        let mut last = at(t0, 105.0);
        watchdog.swept_at(last, Duration::ZERO, 0);
        let mut gaps = Vec::new();
        for _ in 0..6 {
            let mut now = last;
            while watchdog.decide_at(now, false) != Action::Sweep {
                now += Duration::from_secs(1);
            }
            gaps.push(now.duration_since(last).as_secs());
            watchdog.swept_at(now, Duration::ZERO, 0);
            last = now;
        }
        assert_eq!(gaps, vec![120, 240, 300, 300, 300, 300]);

        // A sweep that changed something resets the interval.
        watchdog.swept_at(last, Duration::ZERO, 1);
        assert_eq!(watchdog.decide_at(last + Duration::from_secs(15), false), Action::Sweep);
        watchdog.swept_at(last + Duration::from_secs(15), Duration::ZERO, 0);
        let last = last + Duration::from_secs(15);
        assert_eq!(watchdog.decide_at(last + Duration::from_secs(29), false), Action::Idle);

        // So does a signal.
        watchdog.observe_at(last + Duration::from_secs(1), true);
        assert_eq!(watchdog.decide_at(last + Duration::from_secs(16), false), Action::Sweep);
    }
}
