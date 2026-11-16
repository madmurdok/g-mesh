//! The one heartbeat behind every long MCP wait (GM-709 R-11).

use std::future::Future;
use std::time::{Duration, Instant};

use rmcp::model::ProgressNotificationParam;
use rmcp::service::RequestContext;
use rmcp::RoleServer;

use super::{env_millis, DEFAULT_PROGRESS_INTERVAL, PROGRESS_INTERVAL_ENV};

/// Whether a heartbeated wait also ends on the request's cancellation.
pub(super) enum Cancel {
    Honour,
    Ignore,
}

pub(super) enum Beat<T> {
    Done(T),
    /// Only with `Cancel::Honour`.
    Cancelled,
}

/// Awaits `task`, sending a progress notification every
/// `PROGRESS_INTERVAL_ENV` while it runs (only with a `progressToken`; `0`
/// sends none). The first tick is one interval after `started`, so a task
/// that finishes within one interval sends nothing; `Delay` so a stalled send
/// does not come back to a burst of catch-up ticks. `progress` is
/// `progress_origin.elapsed()`: one origin per tool call keeps it strictly
/// increasing across that call's heartbeats. Biased in this order: the task,
/// then cancellation, then the tick - a ready outcome wins over a tick or a
/// cancellation ready at the same moment. A failed send is logged and ignored.
/// Returns the outcome and how many notifications were sent.
pub(super) async fn with_heartbeat<F: Future>(
    ctx: &RequestContext<RoleServer>,
    started: Instant,
    progress_origin: Instant,
    cancel: Cancel,
    task: F,
    message: impl Fn() -> String,
) -> (Beat<F::Output>, u32) {
    let interval = env_millis(PROGRESS_INTERVAL_ENV, DEFAULT_PROGRESS_INTERVAL);
    let token = ctx.meta.get_progress_token().filter(|_| !interval.is_zero());

    // The ticker's first tick is one interval in, not immediately: a
    // call that resolves on the fast path, or within one interval, sends
    // nothing. `Delay` so a notification send that stalled does not come
    // back to a burst of catch-up ticks.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::from_std(started) + interval.max(Duration::from_millis(1)),
        interval.max(Duration::from_millis(1)),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sent = 0u32;

    tokio::pin!(task);
    let beat = loop {
        tokio::select! {
            // Biased, task first: an outcome that is ready wins over a tick
            // or a cancellation that happens to be ready as well.
            biased;
            out = &mut task => break Beat::Done(out),
            () = ctx.ct.cancelled(), if matches!(cancel, Cancel::Honour) => break Beat::Cancelled,
            _ = ticker.tick(), if token.is_some() => {
                let token = token.clone().expect("the branch is only enabled with a token");
                let param = ProgressNotificationParam::new(token, progress_origin.elapsed().as_secs_f64())
                    .with_message(message());
                match ctx.peer.notify_progress(param).await {
                    Ok(()) => sent += 1,
                    Err(err) => crate::log_line!(
                        "g-mesh daemon: could not send a progress notification for request {}: {err}",
                        ctx.id
                    ),
                }
            }
        }
    };
    (beat, sent)
}
