//! Spawns the bundled JS/TS language plugin as a child process, performs its
//! handshake, and gives the rest of the daemon a way to route `FileChanged`
//! requests to it and apply the diff it answers with - the missing link
//! between the daemon (Rust) and the plugin (Node.js) process.
//!
//! Only the one bundled JS/TS plugin is spawned here, unconditionally. The
//! general `~/.g-mesh/plugins/<language>/` discovery/manifest scheme
//! documented in the v1 architecture doc is deliberately not built: this MVP
//! release bundles exactly one plugin, so there is nothing to discover.
//!
//! # Crash detection and lazy relaunch
//!
//! An *unexpected* plugin exit - a panic, an OOM kill, anything that is not
//! the plugin choosing to stop - is a different problem from the deliberate
//! idle-sleep this daemon may grow later (see task 38): there, the daemon
//! decides to let the plugin go and knows exactly when it will need it back;
//! here, the plugin is just gone and the daemon finds out the hard way, mid
//! request. Leaving that silently broken until someone notices indexing has
//! stopped and restarts the daemon by hand is the failure mode this module
//! avoids: [`PluginProcess::apply_file_change`] treats a dead process as
//! recoverable, not fatal - it spawns a fresh one and replays whatever file
//! paths were still pending against it, transparently to the caller besides
//! the added latency of doing so.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::protocol::handshake;
use crate::protocol::types::RequestId;
use crate::watcher::apply::apply_file_change as apply_file_change_diff;

/// Overrides where the plugin's compiled entry point lives. Real installs
/// never need this - the default already resolves to the bundled plugin -
/// but it lets the integration test suite point at a build without
/// depending on the daemon binary's own install location.
pub const PLUGIN_PATH_ENV: &str = "G_MESH_JS_TS_PLUGIN_PATH";

/// Shared with `daemon::bulk_index`, which spawns the same entry point in
/// its one-shot mode - both must honor the same override.
pub(crate) fn plugin_entry_path() -> PathBuf {
    if let Ok(over) = std::env::var(PLUGIN_PATH_ENV) {
        return PathBuf::from(over);
    }
    // `core/` and `plugins/js-ts/` are sibling directories in this repo, and
    // there is no distribution pipeline yet (see release notes' backlog) -
    // resolving relative to this crate's own source tree, baked in at
    // compile time, is the pragmatic MVP answer.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/js-ts/dist/src/index.js")
}

struct PluginIo {
    reader: BufReader<ChildStdout>,
    writer: ChildStdin,
}

/// The live child process plus its handshake-verified pipes, as one unit -
/// everything a relaunch has to replace together so a caller never observes
/// a `child` and an `io` that belong to two different processes.
struct PluginState {
    // Kept alive so the child is not dropped (and its pipes closed) while
    // still in use; only its pid is ever read, never its exit status, except
    // by `PluginProcess::process_has_exited`'s non-blocking crash check.
    // Killing it explicitly on daemon shutdown is unnecessary: the OS closes
    // the daemon's end of the child's stdin when the daemon process exits,
    // which the plugin already treats as its cue to exit (see index.ts's
    // stdin "end" handler).
    child: Child,
    io: PluginIo,
}

impl PluginState {
    /// Spawns the plugin for `project_root` and reads its handshake off
    /// stdout, hard-failing - matching `handshake::verify`'s "a protocol
    /// mismatch is a hard load failure" philosophy - if it doesn't check
    /// out. Shared by the first spawn (`PluginProcess::spawn`) and every
    /// crash relaunch (`PluginProcess::relaunch`): both need exactly the
    /// same startup sequence.
    fn spawn(project_root: &Path) -> Result<Self> {
        let entry = plugin_entry_path();
        let mut child = Command::new("node")
            .arg(&entry)
            .arg(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Plugin logs are diagnostic-only today - nothing consumes them
            // programmatically - so forwarding to the daemon's own stderr
            // is simplest; it still shows up wherever the daemon's stderr
            // goes (or /dev/null in tests that don't care).
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn JS/TS plugin at {}", entry.display()))?;

        let stdout = child
            .stdout
            .take()
            .context("plugin child process has no stdout")?;
        let stdin = child
            .stdin
            .take()
            .context("plugin child process has no stdin")?;

        let mut reader = BufReader::new(stdout);
        handshake::perform(&mut reader).context("JS/TS plugin handshake failed")?;

        Ok(Self { child, io: PluginIo { reader, writer: stdin } })
    }
}

/// A live handle on the spawned JS/TS plugin process. `Mutex`-wrapped so it
/// can be shared across the connection-serving threads and the watcher
/// thread the same way `daemon::run` already shares its `Connection` - a
/// full actor/async rewrite is more than this ticket needs.
pub struct PluginProcess {
    /// Kept so a crash relaunch can spawn a replacement for exactly the same
    /// project without any caller having to remember and pass it back in.
    project_root: PathBuf,
    state: Mutex<PluginState>,
    next_id: AtomicI64,
    /// File paths handed to [`Self::apply_file_change`] whose diff has not
    /// yet been confirmed committed. Ordinarily holds at most the one file
    /// currently in flight, dropped the instant its diff commits; a plugin
    /// that dies mid round-trip leaves it here instead, which is exactly the
    /// "pending dirty-file queue" a crash relaunch replays before returning.
    pending: Mutex<Vec<String>>,
}

impl PluginProcess {
    /// Spawns the plugin for `project_root` - see [`PluginState::spawn`].
    pub fn spawn(project_root: &Path) -> Result<Self> {
        let state = PluginState::spawn(project_root)?;
        Ok(Self {
            project_root: project_root.to_path_buf(),
            state: Mutex::new(state),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(Vec::new()),
        })
    }

    /// The plugin process's pid, so the daemon can record it for tooling that
    /// has to reason about the plugin from outside this process. Reflects
    /// whichever process is current, so it changes across a crash relaunch -
    /// `relaunch` keeps the on-disk pid file in step with it for the same
    /// reason.
    pub fn pid(&self) -> u32 {
        self.state.lock().unwrap().child.id()
    }

    /// Sends a `FileChanged` request for `file_path` to the plugin and
    /// applies its diff response to `conn`. The plugin's stdin/stdout pair
    /// is locked for each round trip's duration, so concurrent callers (e.g.
    /// a future reindex path alongside the watcher thread) queue rather than
    /// interleave their requests on the wire.
    ///
    /// If the plugin process has exited unexpectedly, this transparently
    /// spawns a fresh one and replays every file path still pending -
    /// including `file_path` itself - against it before returning, rather
    /// than surfacing the crash to the caller. See this module's doc comment
    /// for why that distinction (crash vs. a deliberate stop) matters.
    pub fn apply_file_change(&self, conn: &Mutex<Connection>, file_path: impl Into<String>) -> Result<()> {
        let file_path = file_path.into();
        self.enqueue_pending(&file_path);

        if let Err(first_err) = self.send_one(conn, &file_path) {
            // The plugin's pipes only fail like this when the process behind
            // them is gone. Confirm that before replacing a merely-slow
            // process's live handle out from under it - `process_has_exited`
            // is a non-blocking (if briefly polled) check for exactly that.
            // Another thread may already have won the relaunch race by the
            // time we check, which is fine: `replay_pending` always sends
            // against whatever is current, so it recovers either way.
            if self.process_has_exited() {
                self.relaunch(&first_err)
                    .context("failed to relaunch the JS/TS plugin after it exited unexpectedly")?;
            }
            return self.replay_pending(conn).with_context(|| {
                format!(
                    "JS/TS plugin process exited unexpectedly and could not be recovered while applying a change to {file_path}"
                )
            });
        }

        // The ordinary, no-crash case: `send_one` above already delivered
        // this file, so it is done, not still pending. `replay_pending`
        // never runs this call, so nothing else would otherwise drop it -
        // leaving it here would grow the queue forever and make every
        // future crash replay the project's entire change history.
        self.remove_pending(&file_path);
        Ok(())
    }

    /// Adds `file_path` to the pending queue unless it is already there -
    /// repeated crashes must not grow the queue without bound, and there is
    /// nothing to gain from sending the same path to the plugin twice.
    fn enqueue_pending(&self, file_path: &str) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.iter().any(|f| f == file_path) {
            pending.push(file_path.to_string());
        }
    }

    /// Drops `file_path` from the pending queue - its diff has committed, so
    /// there is nothing left to replay it for.
    fn remove_pending(&self, file_path: &str) {
        self.pending.lock().unwrap().retain(|f| f != file_path);
    }

    /// Sends every file still queued to the plugin, in the order they were
    /// queued, dropping each once its diff commits. Only reached from the
    /// crash-recovery path in [`Self::apply_file_change`]: it starts from
    /// whatever the dead process left pending (which always includes the
    /// file that triggered this replay, still queued behind whatever an
    /// earlier crash may have left too) and picks up exactly where it left
    /// off.
    fn replay_pending(&self, conn: &Mutex<Connection>) -> Result<()> {
        loop {
            let next = { self.pending.lock().unwrap().first().cloned() };
            let Some(file_path) = next else { return Ok(()) };
            self.send_one(conn, &file_path)?;
            self.remove_pending(&file_path);
        }
    }

    fn send_one(&self, conn: &Mutex<Connection>, file_path: &str) -> Result<()> {
        // A per-process atomic counter is all `apply_file_change_diff`'s doc
        // comment asks for - it only needs an id unique enough to catch a
        // response answering the wrong request, not a globally unique one.
        // Left untouched across a relaunch: the fresh process has never seen
        // any of these ids either, so there is nothing to collide with.
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.lock().unwrap();
        // Split into disjoint field borrows up front - borrowing the
        // reader/writer directly as two separate `&mut` arguments doesn't
        // typecheck through the `MutexGuard`'s `DerefMut`.
        let PluginState { io: PluginIo { reader, writer }, .. } = &mut *state;
        let mut conn = conn.lock().unwrap();
        apply_file_change_diff(reader, writer, &mut conn, file_path, id)
    }

    /// Whether the process backing the *current* state has exited.
    ///
    /// Polls `try_wait` for up to half a second rather than checking exactly
    /// once: a killed process's pipes close - which is what makes the write
    /// in [`Self::send_one`] fail in the first place - a moment *before* the
    /// kernel finishes tearing it down far enough for `try_wait` to see it
    /// as exited, so a single check made immediately after that failed write
    /// can race a gap that is real, if usually tiny. A plugin that is merely
    /// slow to answer (not dead) still costs nothing extra: `try_wait`
    /// itself never blocks, and the very first call already covers the
    /// overwhelmingly common case where the exit is already visible.
    fn process_has_exited(&self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if matches!(self.state.lock().unwrap().child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Replaces the dead process with a freshly spawned one, handshake and
    /// all. `cause` is logged, not propagated - a relaunch that itself fails
    /// to spawn is the caller's problem (via the `Result` this returns), but
    /// one that succeeds should read as "recovered from X", not silently
    /// swallow what X was.
    ///
    /// The plugin pid file the daemon writes at startup (`cli::stop`,
    /// `cli::status` read it) is rewritten too - left alone, it would keep
    /// naming a process that no longer exists, or worse, one a recycled pid
    /// now belongs to.
    fn relaunch(&self, cause: &anyhow::Error) -> Result<()> {
        eprintln!(
            "g-mesh daemon: JS/TS plugin process exited unexpectedly ({cause:#}) - \
             relaunching and replaying pending file changes"
        );
        let fresh = PluginState::spawn(&self.project_root)?;
        let pid = fresh.child.id();
        *self.state.lock().unwrap() = fresh;
        match crate::daemon::plugin_pid_path(&self.project_root) {
            Ok(pid_file) => {
                if let Err(err) = fs::write(&pid_file, pid.to_string()) {
                    eprintln!(
                        "g-mesh daemon: could not update the plugin pid file after relaunch: {err:#}"
                    );
                }
            }
            Err(err) => {
                eprintln!("g-mesh daemon: could not resolve the plugin pid file after relaunch: {err:#}")
            }
        }
        Ok(())
    }
}
