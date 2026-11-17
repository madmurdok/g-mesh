//! Whether anything serves a project: the daemon lock and the endpoint, judged
//! from outside the lock's holder. Used by `stop`, the shim's eviction and
//! `g-mesh status`.

use std::path::Path;

use anyhow::{Context, Result};

use crate::ipc;
use crate::paths::state_dir::{OnUnreadable, StateDir};

/// What holds a project's daemon lock right now, judged from outside the
/// holder. The lock alone decides who may serve a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonLock {
    /// Nobody holds it: the project has no daemon and a bootstrap may go ahead.
    Free,
    /// Held by a daemon answering on the project's socket: the only state in
    /// which another daemon must stand down.
    Serving,
    /// Held, but the holder has not published itself as serving yet. Entitled to
    /// a moment: nothing may evict it on this alone.
    Starting,
    /// Held by a live process that published itself as serving and no longer
    /// answers: no client can reach it, and it keeps the project until it exits.
    Wedged { pid: u32 },
}

/// Diagnoses [`DaemonLock`] for `root`: the socket first (an answer means
/// healthy), then the lock, probed by taking and dropping it. A daemon racing
/// for the lock does not notice the probe only because it retries for
/// `daemon::SINGLETON_LOCK_RETRY_BUDGET`, orders of magnitude longer than the
/// probe holds it.
pub fn inspect_daemon_lock(root: &Path) -> Result<DaemonLock> {
    let state_dir = StateDir::of(root)?;
    let listening = is_listening(&state_dir)?;
    inspect_daemon_lock_in(&state_dir, listening)
}

/// [`inspect_daemon_lock`] with the state directory and "is anything
/// listening" given, so the judgement is testable without a real socket.
/// A lock file that cannot be opened reads as "not held": eviction must fail
/// towards leaving processes alone.
pub(crate) fn inspect_daemon_lock_in(state_dir: &StateDir, listening: bool) -> Result<DaemonLock> {
    if listening {
        return Ok(DaemonLock::Serving);
    }
    if !state_dir.daemon_lock_held(OnUnreadable::Free)? {
        return Ok(DaemonLock::Free);
    }
    match state_dir.serving_owner().filter(|&pid| crate::process::is_alive(pid)) {
        Some(pid) => Ok(DaemonLock::Wedged { pid }),
        None => Ok(DaemonLock::Starting),
    }
}

/// Whether something accepts connections on this project's endpoint now; not
/// fooled by a recycled pid. The probe connection is dropped at once.
pub fn is_listening(state_dir: &StateDir) -> Result<bool> {
    Ok(ipc::Stream::connect(&endpoint(state_dir)?).is_ok())
}

/// [`ipc::endpoint_for`], an error for a state directory with no final component.
pub fn endpoint(state_dir: &StateDir) -> Result<ipc::Endpoint> {
    ipc::endpoint_for(state_dir)
        .with_context(|| format!("failed to derive the daemon endpoint from {}", state_dir.path().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock file that cannot be opened reads as free: eviction fails
    /// towards leaving processes alone.
    ///
    /// Control: probe with `OnUnreadable::Held` here: the lock reads as
    /// `Starting`.
    #[cfg(unix)]
    #[test]
    fn an_unopenable_daemon_lock_reads_as_free() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = StateDir::at(dir.path());
        if !crate::paths::state_dir::make_daemon_lock_unopenable(&state_dir) {
            return;
        }
        assert_eq!(inspect_daemon_lock_in(&state_dir, false).unwrap(), DaemonLock::Free);
    }
}
