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
