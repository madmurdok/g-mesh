//! [`with_heartbeat`]'s order and timing, under tokio's paused
//! clock: the heartbeat runs against a real rmcp peer whose transport records
//! every progress notification with the paused clock's time, and can hold a
//! send (a stalled client) or fail it. No real sleeps: every wait below is a
//! paused-clock timer the runtime jumps to once it is idle.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{NumberOrString, ProgressToken};
use rmcp::service::{serve_directly, RequestContext, RunningService, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::{RoleServer, ServerHandler};
use tokio::sync::watch;
use tokio::time::Instant;

use super::progress::{with_heartbeat, Beat, Cancel};
use super::{env_millis, DEFAULT_PROGRESS_INTERVAL, PROGRESS_INTERVAL_ENV};

/// One progress notification the server tried to send.
#[derive(Debug, Clone)]
struct Attempt {
    /// Paused-clock time since the test's `t0`.
    at: Duration,
    progress: f64,
}

/// The server's side of the wire: records each progress notification when
/// it is handed over, then holds it until the gate is open; the first
/// `failures` sends fail.
struct Wire {
    t0: Instant,
    attempts: Arc<Mutex<Vec<Attempt>>>,
    gate: watch::Receiver<bool>,
    failures: Arc<AtomicUsize>,
}

impl Transport<RoleServer> for Wire {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let message = serde_json::to_value(&item).expect("a server message serializes");
        if message["method"] == "notifications/progress" {
            let progress = message["params"]["progress"].as_f64().expect("a progress value");
            self.attempts.lock().unwrap().push(Attempt { at: self.t0.elapsed(), progress });
        }
        let mut gate = self.gate.clone();
        let fail = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok();
        async move {
            let _ = gate.wait_for(|open| *open).await;
            if fail {
                Err(std::io::Error::other("the test wire refused this send"))
            } else {
                Ok(())
            }
        }
    }

    fn receive(&mut self) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        std::future::pending()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A server that handles nothing: only its peer is used.
struct Idle;
impl ServerHandler for Idle {}

/// A served peer over a [`Wire`], and the handles a test drives it with.
struct Harness {
    t0: Instant,
    every: Duration,
    attempts: Arc<Mutex<Vec<Attempt>>>,
    gate: watch::Sender<bool>,
    service: RunningService<RoleServer, Idle>,
}

impl Harness {
    /// `open`: whether sends complete at once; `failures`: how many of the
    /// first sends fail.
    fn new(open: bool, failures: usize) -> Self {
        let every = env_millis(PROGRESS_INTERVAL_ENV, DEFAULT_PROGRESS_INTERVAL);
        assert!(!every.is_zero(), "{PROGRESS_INTERVAL_ENV} must not be 0 for these tests");
        let t0 = Instant::now();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let (gate, gate_rx) = watch::channel(open);
        let wire = Wire {
            t0,
            attempts: Arc::clone(&attempts),
            gate: gate_rx,
            failures: Arc::new(AtomicUsize::new(failures)),
        };
        let service = serve_directly(Idle, wire, None);
        Self { t0, every, attempts, gate, service }
    }

    /// A request context on this peer, with a progress token or without.
    fn ctx(&self, token: bool) -> RequestContext<RoleServer> {
        let mut ctx = RequestContext::new(NumberOrString::Number(1), self.service.peer().clone());
        if token {
            ctx.meta.set_progress_token(ProgressToken(NumberOrString::Number(7)));
        }
        ctx
    }

    /// `n` intervals after `t0`.
    fn at(&self, n: f64) -> Instant {
        self.t0 + self.every.mul_f64(n)
    }

    fn attempts(&self) -> Vec<Attempt> {
        self.attempts.lock().unwrap().clone()
    }

    /// The paused-clock time of each attempt, in whole milliseconds.
    fn attempt_millis(&self) -> Vec<u128> {
        self.attempts().iter().map(|a| a.at.as_millis()).collect()
    }

    /// `n` intervals, in whole milliseconds.
    fn millis(&self, n: f64) -> u128 {
        self.every.mul_f64(n).as_millis()
    }
}

/// The heartbeat with `started` at the harness's `t0` and the progress
/// origin at `origin`.
async fn beat<F: std::future::Future>(
    h: &Harness,
    ctx: &RequestContext<RoleServer>,
    origin: std::time::Instant,
    cancel: Cancel,
    task: F,
) -> (Beat<F::Output>, u32) {
    with_heartbeat(ctx, h.t0.into_std(), origin, cancel, task, || "waiting".to_string()).await
}

fn is_done<T>(beat: &Beat<T>) -> bool {
    matches!(beat, Beat::Done(_))
}

/// A task that ends within one interval sends nothing; a longer one sends
/// its first beat one interval after `started`, then one per interval.
#[tokio::test(start_paused = true)]
async fn the_first_beat_is_one_interval_after_started_then_one_per_interval() {
    let h = Harness::new(true, 0);
    let ctx = h.ctx(true);
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(0.9))).await;
    assert!(is_done(&outcome));
    assert_eq!((sent, h.attempts().len()), (0, 0), "nothing within the first interval");

    let h = Harness::new(true, 0);
    let ctx = h.ctx(true);
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(2.5))).await;
    assert!(is_done(&outcome));
    assert_eq!(sent, 2);
    assert_eq!(h.attempt_millis(), vec![h.millis(1.0), h.millis(2.0)], "{:?}", h.attempts());
}

/// A task that is ready at the very moment a tick is due wins: the tick is
/// not sent.
#[tokio::test(start_paused = true)]
async fn a_ready_result_beats_a_tick_due_at_the_same_moment() {
    let h = Harness::new(true, 0);
    let ctx = h.ctx(true);
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Honour, tokio::time::sleep_until(h.at(2.0))).await;
    assert!(is_done(&outcome));
    assert_eq!(sent, 1, "the tick due with the result is not sent: {:?}", h.attempts());
    assert_eq!(h.attempt_millis(), vec![h.millis(1.0)]);
}

/// The first send stalls; while it does, the task finishes, the request is
/// cancelled and the next tick falls due. Once the send returns, all three
/// are ready at once: the result wins.
#[tokio::test(start_paused = true)]
async fn a_ready_result_beats_a_cancellation_and_a_tick() {
    let h = Harness::new(false, 0);
    let ctx = h.ctx(true);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<u8>();
    let ct = ctx.ct.clone();
    let gate = h.gate.clone();
    let release = h.at(2.5);
    tokio::spawn(async move {
        tokio::time::sleep_until(release).await;
        done_tx.send(42).unwrap();
        ct.cancel();
        gate.send(true).unwrap();
    });
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Honour, async { done_rx.await.unwrap() }).await;
    assert!(matches!(outcome, Beat::Done(42)), "the ready result wins over the cancellation");
    assert_eq!(sent, 1, "the due tick is not sent: {:?}", h.attempts());
}

/// As above with a task that never finishes: the cancellation wins over the
/// due tick, which is not sent.
#[tokio::test(start_paused = true)]
async fn a_cancellation_beats_a_tick_due_with_it() {
    let h = Harness::new(false, 0);
    let ctx = h.ctx(true);
    let ct = ctx.ct.clone();
    let gate = h.gate.clone();
    let release = h.at(2.5);
    tokio::spawn(async move {
        tokio::time::sleep_until(release).await;
        ct.cancel();
        gate.send(true).unwrap();
    });
    let (outcome, sent) = beat(&h, &ctx, h.t0.into_std(), Cancel::Honour, std::future::pending::<()>()).await;
    assert!(matches!(outcome, Beat::Cancelled));
    assert_eq!(sent, 1, "the due tick is not sent: {:?}", h.attempts());
    assert_eq!(h.attempt_millis(), vec![h.millis(1.0)]);
}

/// `Cancel::Ignore` runs the task to its end through a cancellation.
#[tokio::test(start_paused = true)]
async fn an_ignored_cancellation_never_ends_the_wait() {
    let h = Harness::new(true, 0);
    let ctx = h.ctx(true);
    let ct = ctx.ct.clone();
    let cancel_at = h.at(0.5);
    tokio::spawn(async move {
        tokio::time::sleep_until(cancel_at).await;
        ct.cancel();
    });
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(1.5))).await;
    assert!(ctx.ct.is_cancelled());
    assert!(is_done(&outcome), "Cancel::Ignore never yields Cancelled");
    assert_eq!(sent, 1);
}

/// A send that stalls for several intervals is followed by one beat as soon
/// as it returns, then the next one a full interval later: no burst of the
/// missed ticks (`Delay`), and no realignment to the old grid (`Skip`).
#[tokio::test(start_paused = true)]
async fn a_stalled_send_is_followed_by_one_beat_not_a_burst() {
    let h = Harness::new(false, 0);
    let ctx = h.ctx(true);
    let gate = h.gate.clone();
    let release = h.at(4.6);
    tokio::spawn(async move {
        tokio::time::sleep_until(release).await;
        gate.send(true).unwrap();
    });
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(5.5))).await;
    assert!(is_done(&outcome));
    assert_eq!(h.attempt_millis(), vec![h.millis(1.0), h.millis(4.6)], "{:?}", h.attempts());
    assert_eq!(sent, 2);
}

/// `progress` is the time since the call's origin, not since `started`, and
/// rises from one beat to the next.
#[tokio::test(start_paused = true)]
async fn progress_counts_from_the_origin_and_rises() {
    let h = Harness::new(true, 0);
    let ctx = h.ctx(true);
    let origin = std::time::Instant::now()
        .checked_sub(Duration::from_secs(100))
        .expect("the monotonic clock is past 100 s");
    let (outcome, sent) = beat(&h, &ctx, origin, Cancel::Ignore, tokio::time::sleep_until(h.at(2.5))).await;
    assert!(is_done(&outcome));
    assert_eq!(sent, 2);
    let progress: Vec<f64> = h.attempts().iter().map(|a| a.progress).collect();
    assert_eq!(progress.len(), 2);
    assert!(progress.iter().all(|p| (100.0..400.0).contains(p)), "counted from the origin: {progress:?}");
    assert!(progress[1] > progress[0], "rising: {progress:?}");
}

/// Without a progress token nothing is sent, however long the task runs.
#[tokio::test(start_paused = true)]
async fn no_progress_token_sends_nothing() {
    let h = Harness::new(true, 0);
    let ctx = h.ctx(false);
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(3.5))).await;
    assert!(is_done(&outcome));
    assert_eq!((sent, h.attempts().len()), (0, 0));
}

/// A failed send is not counted and does not end the heartbeat.
#[tokio::test(start_paused = true)]
async fn a_failed_send_is_not_counted() {
    let h = Harness::new(true, 1);
    let ctx = h.ctx(true);
    let (outcome, sent) =
        beat(&h, &ctx, h.t0.into_std(), Cancel::Ignore, tokio::time::sleep_until(h.at(2.5))).await;
    assert!(is_done(&outcome));
    assert_eq!(h.attempts().len(), 2, "both ticks were attempted");
    assert_eq!(sent, 1, "only the send that went through counts");
}
