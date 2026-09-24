//! A test-only knob that parks the plugin at a named point, neither reading
//! stdin nor writing stdout, for as long as a test wants it parked (GM-397).
//!
//! # Why a plugin needs its own hold
//!
//! Core's `HOLD_*` knobs (`daemon::bulk_index`) hold the *daemon*. What
//! GM-397 has to test is a *plugin* outliving a daemon that was killed while
//! the plugin was busy - and "busy" is exactly the state in which today's
//! plugins notice nothing: a walk that has not written yet, or a
//! `semanticPass` still computing. A real fixture large enough to be busy for
//! seconds is slow and machine-dependent; this knob makes the busy state
//! deterministic instead.
//!
//! # The contract, identical in all four bundled plugins
//!
//! With [`HOLD_DIR_ENV`] set to a directory, a plugin reaching hold point `P`
//! for language `L` looks for `<dir>/P-L.hold`. If it exists, the plugin
//! writes its own pid to `<dir>/P-L.pid` (via a temporary file and a rename,
//! so a reader never sees a half-written pid) and then waits while the hold
//! file exists, checking every 10 ms, for at most 60 s. The wait blocks the
//! calling thread, as a real computation would. With the variable unset, or
//! the hold file absent, this is one `stat` at most.
//!
//! The pid file is also the only way a test can learn a bulk child's pid:
//! bulk children have no pid file of their own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The directory the hold and pid files live in. Test-only: no real install
/// sets it.
pub(crate) const HOLD_DIR_ENV: &str = "G_MESH_PLUGIN_HOLD_DIR";

/// How often the hold file is re-checked.
const POLL: Duration = Duration::from_millis(10);

/// The hold's upper bound, so a test that forgets to release it cannot park a
/// plugin forever.
const MAX_HOLD: Duration = Duration::from_secs(60);

/// Parks the calling thread at `point` (`bulk` or `semantic`) if the knob asks
/// for it. See the module doc for the exact contract.
pub(crate) fn hold_point(point: &str, language: &str) {
    let Some(dir) = std::env::var_os(HOLD_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let hold = dir.join(format!("{point}-{language}.hold"));
    if !hold.exists() {
        return;
    }
    if let Err(err) = write_pid(&dir, &format!("{point}-{language}")) {
        eprintln!("[{language}] {HOLD_DIR_ENV}: failed to record the pid at hold point {point}: {err}");
    }
    eprintln!("[{language}] {HOLD_DIR_ENV}: holding at {point} while {} exists", hold.display());
    let started = Instant::now();
    while hold.exists() && started.elapsed() < MAX_HOLD {
        std::thread::sleep(POLL);
    }
}

fn write_pid(dir: &Path, stem: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!("{stem}.pid.tmp"));
    std::fs::write(&tmp, std::process::id().to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{stem}.pid")))
}
