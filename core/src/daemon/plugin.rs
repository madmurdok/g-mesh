//! Spawns the bundled JS/TS language plugin as a child process, performs its
//! handshake, and gives the rest of the daemon a way to route `FileChanged`
//! requests to it and apply the diff it answers with - the missing link
//! between the daemon (Rust) and the plugin (Node.js) process.
//!
//! Only the one bundled JS/TS plugin is spawned here, unconditionally. The
//! general `~/.g-mesh/plugins/<language>/` discovery/manifest scheme
//! documented in the v1 architecture doc is deliberately not built: this MVP
//! release bundles exactly one plugin, so there is nothing to discover.

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

/// How often [`PluginProcess::shutdown`] checks whether the plugin has taken
/// the hint and exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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

/// A live handle on the spawned JS/TS plugin process. `Mutex`-wrapped so it
/// can be shared across the connection-serving threads and the watcher
/// thread the same way `daemon::run` already shares its `Connection` - a
/// full actor/async rewrite is more than this ticket needs.
pub struct PluginProcess {
    // Kept alive so the child is not dropped (and its pipes closed) while
    // still in use. A daemon that is killed outright still needs nothing from
    // it: the OS closes the daemon's end of the child's stdin, which the
    // plugin already treats as its cue to exit (see index.ts's stdin "end"
    // handler). What [`shutdown`](Self::shutdown) adds is the *deliberate*
    // ending of a plugin whose core carries on - a sleep on the idle timeout -
    // where nothing closes those pipes unless this process says so.
    child: Child,
    io: Mutex<PluginIo>,
    next_id: AtomicI64,
}

impl PluginProcess {
    /// Spawns the plugin for `project_root`, reads its handshake off stdout,
    /// and hard-fails - matching `handshake::verify`'s "a protocol mismatch
    /// is a hard load failure" philosophy - if it doesn't check out.
    pub fn spawn(project_root: &Path) -> Result<Self> {
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

        Ok(Self {
            child,
            io: Mutex::new(PluginIo { reader, writer: stdin }),
            next_id: AtomicI64::new(1),
        })
    }

    /// The plugin process's pid, so the daemon can record it for tooling that
    /// has to reason about the plugin from outside this process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Ends this plugin process and waits for it to be gone, consuming the
    /// handle - what `daemon::lifecycle` calls when the plugin has been idle
    /// long enough to sleep, and again on the core's own way out.
    ///
    /// Closing the pipes is the whole shutdown on the ordinary path: the
    /// plugin exits on its stdin's `end` event (index.ts), which is the same
    /// mechanism that makes it die with a core that was killed. The signal is
    /// only insurance against a plugin that does not notice - a hung reparse,
    /// a future handler that swallows the event - because a "sleeping" plugin
    /// still holding its half gigabyte of tsserver would be the exact cost
    /// this timeout exists to avoid.
    ///
    /// Reaping matters as much as ending: the daemon is this process's parent
    /// for its whole run, so an unwaited child would sit as a zombie until the
    /// core exits, and `is_process_alive` (which `cli::status` and the tests
    /// ask) cannot tell a zombie from a running process.
    pub fn shutdown(self, grace: Duration) -> Result<()> {
        // Destructured rather than dropped field by field: dropping `io`
        // closes both pipes, and closing the write half of the plugin's stdin
        // is precisely the "please exit" signal.
        let Self { mut child, io, .. } = self;
        drop(io);

        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait().context("failed to check whether the plugin had exited")? {
                Some(_) => return Ok(()),
                None if Instant::now() >= deadline => break,
                None => std::thread::sleep(EXIT_POLL_INTERVAL),
            }
        }

        child.kill().context("failed to signal a plugin that ignored its closed stdin")?;
        child.wait().context("failed to reap the plugin process")?;
        Ok(())
    }

    /// Sends a `FileChanged` request for `file_path` to the plugin and
    /// applies its diff response to `conn`. The plugin's stdin/stdout pair
    /// is locked for the round trip's duration, so concurrent callers (e.g.
    /// a future reindex path alongside the watcher thread) queue rather than
    /// interleave their requests on the wire.
    pub fn apply_file_change(&self, conn: &Mutex<Connection>, file_path: impl Into<String>) -> Result<()> {
        // A per-process atomic counter is all `apply_file_change`'s doc
        // comment asks for - it only needs an id unique enough to catch a
        // response answering the wrong request, not a globally unique one.
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut io = self.io.lock().unwrap();
        // Split into disjoint field borrows up front - borrowing `io.reader`
        // and `io.writer` directly as two separate `&mut` arguments doesn't
        // typecheck through the `MutexGuard`'s `DerefMut`.
        let PluginIo { reader, writer } = &mut *io;
        let mut conn = conn.lock().unwrap();
        apply_file_change_diff(reader, writer, &mut conn, file_path, id)
    }
}
