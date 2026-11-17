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
