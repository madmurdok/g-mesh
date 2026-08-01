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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::protocol::handshake;
use crate::protocol::types::RequestId;
use crate::storage::schema::CURRENT_INDEXER_VERSION;
use crate::watcher::apply::apply_file_change as apply_file_change_diff;
use crate::watcher::staleness::{self, StalenessOutcome};

/// Overrides where the plugin's compiled entry point lives. Real installs
/// never need this - the default already resolves to the bundled plugin -
/// but it lets the integration test suite point at a build without
/// depending on the daemon binary's own install location.
pub const PLUGIN_PATH_ENV: &str = "G_MESH_JS_TS_PLUGIN_PATH";

/// How often [`PluginProcess::shutdown`] checks whether the plugin has taken
/// the hint and exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How much of the digest [`fingerprint`] keeps. 64 bits is far more than
/// enough to tell two builds of one plugin apart, and short enough that a
/// human reading a build stamp file can compare two of them at a glance.
const FINGERPRINT_HEX_CHARS: usize = 16;

/// What [`fingerprint`] answers when it cannot read the plugin's build at
/// all. Deliberately a fixed string rather than a random or timestamped one:
/// two processes that both fail to look compare *equal*, which degrades to
/// the behavior there was before this existed instead of making every start
/// look like a change. It cannot collide with a real answer, which is hex.
pub const FINGERPRINT_UNAVAILABLE: &str = "unavailable";

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

/// Identifies the plugin *logic* this process would run, as a short hex
/// digest of its compiled output.
///
/// # Why this exists
///
/// The whole graph is computed by the plugin, but until task 116 nothing in
/// g-mesh could tell that the plugin had changed. The two staleness checks
/// that existed both looked somewhere else: `daemon::build_stamp` at the core
/// executable, and `storage::schema::CURRENT_INDEXER_VERSION` at a constant
/// somebody has to remember to bump. Task 115 rewrote how the extractor
/// resolves same-file edges and - correctly following every rule that was
/// written down, none of which is enforced by anything - did not bump that
/// constant. Every existing index went on serving the previous extractor's
/// output, with a current schema, a current core binary, and no symptom other
/// than wrong answers.
///
/// So this is the plugin's half of "which pipeline produced what is in the
/// index", derived the way `build_stamp`'s docs argue the core's half should
/// be: from the artifact itself, so it needs no discipline to maintain and
/// cannot silently agree when it should not.
///
/// # Content, not mtime
///
/// `build_stamp` compares the core executable's mtime because it only needs an
/// *ordering* ("is that daemon behind me?"). This one has to answer a
/// different question - "would that build produce a different graph?" - where
/// mtime is both too eager and unordered: `npm run build` rewrites every file
/// in `dist/` on every invocation, and a re-emitted but byte-identical bundle
/// must not cost a project a full re-walk. A digest over the bytes changes
/// exactly when the logic does.
///
/// # What it does not cover
///
/// The plugin's *dependencies* - the tree-sitter grammars in `node_modules` -
/// are not hashed: they are large, they are not part of what `npm run build`
/// emits, and walking them on every shim start would turn a sub-millisecond
/// check into a directory crawl. A grammar upgrade that changes extraction is
/// therefore still a manual [`CURRENT_INDEXER_VERSION`] bump, which is exactly
/// what that constant remains for - the two halves are complementary, not
/// redundant.
///
/// Computed once per process: the shim asks for it on every call it makes, and
/// the answer cannot change under a running process in any way that would
/// matter (the plugin a daemon already spawned is the one it keeps).
pub fn fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| {
        let entry = plugin_entry_path();
        digest_of_plugin_build(&entry).unwrap_or_else(|err| {
            eprintln!(
                "g-mesh: could not fingerprint the JS/TS plugin at {}: {err:#} - \
                 a change to its extraction logic will not be noticed",
                entry.display()
            );
            FINGERPRINT_UNAVAILABLE.to_string()
        })
    })
}

/// The generation string an index is stamped with, and the thing
/// `storage::schema::ensure_current` compares: core's hand-maintained
/// pipeline generation and the plugin build that filled the index, joined.
///
/// Both halves have to be in it. The constant alone misses every plugin-side
/// change (the failure task 116 fixes); the fingerprint alone would miss every
/// change in `graph::imports` / `graph::symbol_links`, which run in core and
/// leave the plugin's bytes untouched.
pub fn indexer_version() -> String {
    format!("{CURRENT_INDEXER_VERSION}+{}", fingerprint())
}

/// Digests every compiled file the plugin ships, in a stable order.
///
/// The whole emitted tree rather than just the entry point: `dist/src` is one
/// `tsc` output split across modules, and the extractor - the part most likely
/// to change what the graph looks like - is not the entry file. Each file's
/// path and length go into the digest alongside its bytes, so moving code
/// between two files cannot leave the concatenation unchanged.
fn digest_of_plugin_build(entry: &Path) -> Result<String> {
    let dir = entry
        .parent()
        .with_context(|| format!("{} has no parent directory", entry.display()))?;

    let mut files = Vec::new();
    collect_emitted_files(dir, dir, &mut files)?;
    if files.is_empty() {
        bail!("no compiled plugin files found under {}", dir.display());
    }
    // Directory iteration order is whatever the filesystem feels like, and a
    // fingerprint that depends on it would differ between two identical
    // checkouts.
    files.sort();

    let mut hasher = Sha256::new();
    for (relative, path) in &files {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }

    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(FINGERPRINT_HEX_CHARS)
        .collect())
}

/// Gathers every `.js` file under `dir` as a pair of its path relative to
/// `root` and its full path. Recursive rather than one flat `read_dir` so a
/// future plugin laid out in subdirectories does not silently fall outside
/// the fingerprint - a blind spot in this function is a wrong answer served
/// later, which is the exact failure it exists to prevent.
fn collect_emitted_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_dir() {
            collect_emitted_files(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        out.push((relative, path));
    }
    Ok(())
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
    // by `PluginProcess::process_has_exited`'s non-blocking crash check. A
    // daemon that is killed outright still needs nothing from it: the OS
    // closes the daemon's end of the child's stdin, which the plugin already
    // treats as its cue to exit (see index.ts's stdin "end" handler). What
    // [`PluginProcess::shutdown`] adds is the *deliberate* ending of a plugin
    // whose core carries on - a sleep on the idle timeout - where nothing
    // closes those pipes unless this process says so.
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
        // is precisely the "please exit" signal. `state`'s Mutex is unwrapped
        // via `into_inner` - `self` is owned here, so there is no contention
        // left to guard against, only the poisoning case `.unwrap()` already
        // treats as fatal everywhere else in this module.
        let Self { state, .. } = self;
        let PluginState { mut child, io } = state.into_inner().unwrap();
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

    /// Synchronous per-file staleness check plus reindex-if-needed, per
    /// `watcher::staleness::ensure_fresh` - see
    /// `daemon::lifecycle::PluginSupervisor::ensure_fresh`'s doc for why this
    /// exists and what gap it closes.
    ///
    /// The mtime/hash comparison (`watcher::staleness::is_stale`) runs
    /// without this process's `state` lock at all - the overwhelmingly common
    /// case (nothing changed) must not queue behind a live reparse it has
    /// nothing to do with, matching the whole point of `watcher::staleness`'s
    /// two-tier design. Only a real mismatch takes the lock, for exactly the
    /// one round trip a live watcher event would also pay for.
    ///
    /// Unlike [`Self::apply_file_change`], this does not go through the
    /// pending-queue crash-recovery path: a plugin that has crashed since the
    /// last round trip surfaces as an ordinary `Err` here, which the MCP
    /// layer logs and treats as best-effort (see `mcp::GMeshMcpServer::
    /// ensure_file_fresh`) rather than something worth relaunching a process
    /// over on a mere freshness check.
    pub fn ensure_fresh(&self, conn: &Mutex<Connection>, file_path: &str) -> Result<StalenessOutcome> {
        {
            let guard = conn.lock().unwrap();
            if !staleness::is_stale(&guard, &self.project_root, file_path)? {
                return Ok(StalenessOutcome::AlreadyFresh);
            }
        }

        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.lock().unwrap();
        let PluginState { io: PluginIo { reader, writer }, .. } = &mut *state;
        let mut conn = conn.lock().unwrap();
        staleness::ensure_fresh(reader, writer, &mut conn, &self.project_root, file_path, id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway `dist/`-shaped directory and returns its entry
    /// point, so the digest can be exercised without a real plugin build.
    fn emitted(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        for (relative, contents) in files {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        dir.join("index.js")
    }

    #[test]
    fn the_same_build_fingerprints_the_same_way_twice() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);

        let first = digest_of_plugin_build(&entry).unwrap();
        assert_eq!(digest_of_plugin_build(&entry).unwrap(), first);
        assert_eq!(first.len(), FINGERPRINT_HEX_CHARS);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first} must be hex");
    }

    /// The case task 116 is about: nothing but the extractor's own compiled
    /// logic changed, and that has to be visible.
    #[test]
    fn changing_one_emitted_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        fs::write(dir.path().join("extract.js"), "parse(); resolveLexically();").unwrap();

        assert_ne!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// `npm run build` rewrites every file on every invocation. A rebuild
    /// that changed nothing must not cost a project a full re-walk, which is
    /// why the digest is over content and not over mtimes.
    #[test]
    fn re_emitting_identical_bytes_leaves_the_fingerprint_alone() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(dir.path().join("extract.js"), "parse();").unwrap();

        assert_eq!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// Moving a line from one module to another leaves the concatenated
    /// bytes identical - the path and length mixed in are what keep the two
    /// builds apart.
    #[test]
    fn moving_code_between_two_files_still_changes_the_fingerprint() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let before = digest_of_plugin_build(&emitted(
            one.path(),
            &[("index.js", "ab"), ("extract.js", "c")],
        ))
        .unwrap();
        let after = digest_of_plugin_build(&emitted(
            other.path(),
            &[("index.js", "a"), ("extract.js", "bc")],
        ))
        .unwrap();

        assert_ne!(after, before);
    }

    #[test]
    fn a_file_in_a_subdirectory_is_part_of_the_build_too() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("lang/ts.js", "grammar();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        fs::write(dir.path().join("lang/ts.js"), "grammar(2);").unwrap();

        assert_ne!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// An absent or unbuilt plugin is not a fingerprint of zero files - that
    /// would compare equal to every other unbuilt tree and read as "nothing
    /// changed".
    #[test]
    fn an_unbuilt_plugin_has_no_fingerprint_at_all() {
        let dir = tempfile::tempdir().unwrap();
        assert!(digest_of_plugin_build(&dir.path().join("index.js")).is_err());

        fs::write(dir.path().join("README.md"), "not a build").unwrap();
        assert!(digest_of_plugin_build(&dir.path().join("index.js")).is_err());
    }

    /// The two halves of the generation string are both there, and the one
    /// the running test binary computes is a real one - `core/build.rs` has
    /// just built the plugin it points at.
    #[test]
    fn the_recorded_generation_names_the_core_pipeline_and_the_plugin_build() {
        let version = indexer_version();
        let (core, plugin) = version.split_once('+').expect("both halves must be present");

        assert_eq!(core, CURRENT_INDEXER_VERSION);
        assert_eq!(plugin, fingerprint());
        assert_ne!(
            plugin, FINGERPRINT_UNAVAILABLE,
            "the test binary's own plugin build must be readable - `cargo test` builds it"
        );
    }
}
