//! Hooks run when core's control stream closes while the plugin is still
//! running: the reader thread in `run` calls [`run_lifeline_exit_hooks`] just
//! before `process::exit`, which skips every destructor. A semantic engine
//! that owns child processes (a language server) registers a hook that kills
//! them, or they outlive the plugin.

use std::sync::Mutex;

static HOOKS: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Registers `hook` to run when the plugin exits because core's control
/// stream closed. A hook registered twice runs twice; a caller that may
/// register more than once guards it with a `Once`.
pub fn on_lifeline_exit(hook: fn()) {
    HOOKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(hook);
}

/// Runs every registered hook, in registration order. The list is copied
/// out under the lock and the lock released before any hook runs, so a hook
/// that registers another cannot deadlock.
#[doc(hidden)]
pub fn run_lifeline_exit_hooks() {
    let hooks = HOOKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    for hook in hooks {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static FIRST: AtomicUsize = AtomicUsize::new(0);
    static SECOND: AtomicUsize = AtomicUsize::new(0);
    static REGISTERING: AtomicUsize = AtomicUsize::new(0);
    static REGISTERED_LATE: AtomicUsize = AtomicUsize::new(0);
    static LATE_HOOK_ADDED: AtomicBool = AtomicBool::new(false);

    fn first() {
        FIRST.fetch_add(1, Ordering::SeqCst);
    }

    fn second() {
        SECOND.fetch_add(1, Ordering::SeqCst);
    }

    fn registered_late() {
        REGISTERED_LATE.fetch_add(1, Ordering::SeqCst);
    }

    /// Registers [`registered_late`] from inside a hook, once.
    fn registering() {
        REGISTERING.fetch_add(1, Ordering::SeqCst);
        if !LATE_HOOK_ADDED.swap(true, Ordering::SeqCst) {
            on_lifeline_exit(registered_late);
        }
    }

    /// One test, because the registry is process-global: a second test
    /// registering hooks would change these counts when lib tests share a
    /// process. Every run happens on a helper thread so a deadlock is a
    /// failed `recv_timeout`, not a hung test.
    #[test]
    fn every_hook_runs_once_per_run_and_a_hook_may_register_another() {
        on_lifeline_exit(first);
        on_lifeline_exit(second);
        on_lifeline_exit(registering);

        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            run_lifeline_exit_hooks();
            let after_one = (
                FIRST.load(Ordering::SeqCst),
                SECOND.load(Ordering::SeqCst),
                REGISTERING.load(Ordering::SeqCst),
            );
            run_lifeline_exit_hooks();
            let _ = done.send(after_one);
        });
        let after_one = finished
            .recv_timeout(Duration::from_secs(30))
            .expect("run_lifeline_exit_hooks must return when a hook registers another hook");

        assert_eq!(after_one, (1, 1, 1), "one run calls every registered hook exactly once");
        assert_eq!(FIRST.load(Ordering::SeqCst), 2, "a second run calls each hook again");
        assert_eq!(SECOND.load(Ordering::SeqCst), 2);
        assert_eq!(REGISTERING.load(Ordering::SeqCst), 2);
        assert!(
            REGISTERED_LATE.load(Ordering::SeqCst) >= 1,
            "a hook registered from inside a hook is in the registry for the next run"
        );
    }
}
