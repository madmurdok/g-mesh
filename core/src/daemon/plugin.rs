//! Spawns a language plugin as a child process from its manifest, performs
//! its handshake, and gives the rest of the daemon a way to route
//! `FileChanged` requests to it and apply the diff it answers with - the
//! missing link between the daemon (Rust) and a plugin process (Node.js for
//! the bundled one; whatever a discovered manifest's `command`/`args` name,
//! for any other).
//!
//! [`PluginProcess`]/[`PluginState`] here are generic over any one plugin;
//! discovery and per-language routing live in `daemon::manifest`
//! (`~/.g-mesh/plugins/<language>/plugin.toml` + the bundled root) and
//! `daemon::registry::PluginRegistry`, which spawns one `PluginProcess` per
//! discovered language, lazily, the first time anything needs it. This
//! module keeps a narrower "the bundled JS/TS plugin specifically" surface
//! too - [`bundled_manifest`], [`plugin_entry_path`], [`PLUGIN_PATH_ENV`],
//! [`bundled_fingerprint`] - for the handful of callers that genuinely mean
//! that one plugin rather than whatever the registry has discovered:
//! `daemon::build_stamp` compares one *running daemon's* JS/TS build against
//! another's (a question about this install, not about what filled any one
//! project's index), and a couple of tests want a bare manifest without a
//! `plugin.toml` fixture. See each item's own doc comment for why it is not
//! dead code.
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

use crate::daemon::manifest::PluginManifest;
use crate::embedding::EmbeddingPipeline;
use crate::protocol::handshake;
use crate::protocol::types::{RequestId, CURRENT_PROTOCOL_VERSION};
use crate::watcher::apply::{apply_file_change as apply_file_change_diff, apply_semantic_pass};
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

/// Directory names skipped while fingerprinting *every* plugin, regardless of
/// what its own manifest says - see `docs/architecture/plugin-modularity.md`'s
/// Data Model section ("Built-in baseline ignore"). A manifest's own
/// `fingerprint_ignore` extends this list; it never replaces it.
const BASELINE_FINGERPRINT_IGNORE: &[&str] =
    &[".git", "node_modules", "__pycache__", ".venv", "venv", ".pytest_cache"];

/// Shared with `daemon::bulk_index`, which spawns the same entry point in
/// its one-shot mode - both must honor the same override.
pub(crate) fn plugin_entry_path() -> PathBuf {
    if let Ok(over) = std::env::var(PLUGIN_PATH_ENV) {
        return PathBuf::from(over);
    }
    // `core/` and `plugins/typescript/` are sibling directories in this repo, and
    // there is no distribution pipeline yet (see release notes' backlog) -
    // resolving relative to this crate's own source tree, baked in at
    // compile time, is the pragmatic MVP answer.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript/dist/src/index.js")
}

/// The bundled plugin's wire identifier - `Handshake.language`
/// (`plugins/typescript/src/index.ts`), and since task 155 renamed
/// `plugins/js-ts/` to `plugins/typescript/`, its manifest's `language` and
/// its directory name too: the bundled plugin is discovered through the
/// exact same "directory name is the manifest's language" rule as any other
/// plugin, no special case retained (see
/// `docs/architecture/plugin-modularity.md`'s Options Considered #1).
///
/// A constant rather than a literal at each use site because it names two
/// genuinely different things that must never drift apart: the language
/// [`bundled_manifest`] hands out below, and the pid-file name
/// `daemon::plugin_pid_path` still resolves for the test suite and the few
/// other callers that predate per-language pid files and only ever meant
/// "the bundled JS/TS plugin's pid" (see that function's doc comment).
/// Production code that has to be genuinely multi-language-aware
/// (`cli::status`, `cli::stop`, `cli::clean`) never references this constant
/// - it lists every `plugin-<language>.pid` file `PluginRegistry` writes
/// instead of assuming this one.
pub const BUNDLED_LANGUAGE: &str = "typescript";

/// A [`PluginManifest`] describing the bundled JS/TS plugin exactly as it was
/// spawned before this module took a manifest as an argument: `node` plus
/// [`plugin_entry_path`]'s resolved entry point, honoring the same
/// [`PLUGIN_PATH_ENV`] override - nothing read from an actual `plugin.toml`.
///
/// Still used after `daemon::run` moved to `daemon::registry::PluginRegistry`
/// (task 155) and after the index's generation string stopped being keyed off
/// this one bundled-plugin view (task 163 - see
/// `daemon::registry::indexer_version`): [`bundled_fingerprint`] below is what
/// `daemon::build_stamp` compares one *running daemon's* JS/TS plugin against
/// another's, which is a different question from "what filled this index" and
/// is deliberately still asked of the bundled plugin alone; and a few tests
/// (`plugin_crash_recovery.rs`) still want a bare [`PluginManifest`] for the
/// bundled plugin without going through a `plugin.toml` fixture.
pub fn bundled_manifest() -> PluginManifest {
    let entry = plugin_entry_path();
    let manifest_dir = entry.parent().map(Path::to_path_buf).unwrap_or_default();
    PluginManifest {
        language: BUNDLED_LANGUAGE.to_string(),
        protocol_version: CURRENT_PROTOCOL_VERSION,
        plugin_version: String::new(),
        command: PathBuf::from("node"),
        args: vec![entry.to_string_lossy().into_owned()],
        extensions: Vec::new(),
        fingerprint_ignore: Vec::new(),
        manifest_dir,
    }
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
/// A plugin's dependency/VCS junk - `node_modules`, `.git`, and the rest of
/// [`BASELINE_FINGERPRINT_IGNORE`] (plus anything a manifest's own
/// `fingerprint_ignore` adds) - is walked but skipped by name, not hashed:
/// those directories are large, are not what a plugin's own build emits, and
/// walking them on every shim start would turn a sub-millisecond check into a
/// directory crawl. A dependency upgrade that changes extraction is therefore
/// still a manual [`CURRENT_INDEXER_VERSION`](crate::storage::schema::CURRENT_INDEXER_VERSION)
/// bump, which is exactly what that constant remains for - the two halves are
/// complementary, not redundant.
pub fn fingerprint(manifest: &PluginManifest) -> String {
    digest_of_plugin_build(&manifest.manifest_dir, &manifest.fingerprint_ignore).unwrap_or_else(
        |err| {
            eprintln!(
                "g-mesh: could not fingerprint the {} plugin at {}: {err:#} - \
                 a change to its extraction logic will not be noticed",
                manifest.language,
                manifest.manifest_dir.display()
            );
            FINGERPRINT_UNAVAILABLE.to_string()
        },
    )
}

/// [`fingerprint`] for [`bundled_manifest`], memoized for the process's
/// lifetime - what `build_stamp::of_running_process` needs, and since task 163
/// its only caller.
///
/// It is deliberately *not* what stamps the index any more: that is
/// `daemon::registry::indexer_version`, a digest over every *discovered*
/// plugin's fingerprint, because with N plugins an index is only as current as
/// the least current of the builds that filled it. The two questions differ in
/// what they are for. A build stamp compares one running daemon against
/// another so a shim can decide whether to retire the incumbent, and the
/// bundled JS/TS plugin is the part of a daemon's build that
/// [`PLUGIN_PATH_ENV`] can redirect out from under it; the index's generation
/// is about content that is already stored.
///
/// [`fingerprint`] itself does not cache - `daemon::registry` computes one
/// fingerprint per discovered plugin, and owns that concern for its own set of
/// manifests rather than leaving a process-wide `OnceLock` to answer for all
/// of them.
///
/// Computed once per process: the shim asks for it on every call it makes,
/// and the answer cannot change under a running process in any way that
/// would matter (the plugin a daemon already spawned is the one it keeps).
pub fn bundled_fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| fingerprint(&bundled_manifest()))
}

/// Digests every regular file under `dir`, skipping any subdirectory whose
/// name is in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`, in a stable order.
///
/// The whole directory rather than just an entry point's siblings: a plugin's
/// own build output can be laid out however it likes, and the file most
/// likely to change what the graph looks like is not necessarily the entry
/// file. Each file's path and length go into the digest alongside its bytes,
/// so moving code between two files cannot leave the concatenation unchanged.
fn digest_of_plugin_build(dir: &Path, ignore: &[String]) -> Result<String> {
    let mut files = Vec::new();
    collect_fingerprinted_files(dir, dir, ignore, &mut files)?;
    if files.is_empty() {
        bail!("no plugin files found under {}", dir.display());
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

    Ok(truncated_hex(hasher))
}

/// Finishes `hasher` the one way this daemon renders a digest: lowercase hex,
/// cut to [`FINGERPRINT_HEX_CHARS`].
///
/// Shared with `daemon::registry::indexer_version`, which hashes every
/// discovered plugin's fingerprint into a single digest of its own. Two
/// digests that mean different things may as well look alike - both are read
/// by humans comparing a build stamp or a `meta.indexer_version` at a glance -
/// and a second, subtly different truncation convention invented next door is
/// exactly the kind of drift a shared helper costs nothing to rule out.
pub(crate) fn truncated_hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(FINGERPRINT_HEX_CHARS)
        .collect()
}

/// Gathers every regular file under `dir` as a pair of its path relative to
/// `root` and its full path, recursing into subdirectories except those named
/// in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`. Recursive rather than one
/// flat `read_dir` so a plugin laid out in subdirectories does not silently
/// fall outside the fingerprint - a blind spot in this function is a wrong
/// answer served later, which is the exact failure it exists to prevent.
fn collect_fingerprinted_files(
    root: &Path,
    dir: &Path,
    ignore: &[String],
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if BASELINE_FINGERPRINT_IGNORE.contains(&name.as_ref())
                || ignore.iter().any(|ignored| ignored == name.as_ref())
            {
                continue;
            }
            collect_fingerprinted_files(root, &path, ignore, out)?;
            continue;
        }
        if !file_type.is_file() {
            // Symlinks, sockets, etc. - not a regular file this plugin's
            // build emitted.
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
    /// Spawns `manifest`'s plugin for `project_root` and reads its handshake
    /// off stdout, hard-failing - matching `handshake::verify`'s "a protocol
    /// mismatch is a hard load failure" philosophy - if it doesn't check
    /// out, or if the live handshake's language disagrees with what the
    /// manifest declared. Shared by the first spawn (`PluginProcess::spawn`)
    /// and every crash relaunch (`PluginProcess::relaunch`): both need
    /// exactly the same startup sequence.
    fn spawn(project_root: &Path, manifest: &PluginManifest) -> Result<Self> {
        let mut child = Command::new(&manifest.command)
            .args(&manifest.args)
            .arg(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Plugin logs are diagnostic-only today - nothing consumes them
            // programmatically - so forwarding to the daemon's own stderr
            // is simplest; it still shows up wherever the daemon's stderr
            // goes (or /dev/null in tests that don't care).
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn {} plugin ({})",
                    manifest.language,
                    manifest.command.display()
                )
            })?;

        let stdout = child
            .stdout
            .take()
            .context("plugin child process has no stdout")?;
        let stdin = child
            .stdin
            .take()
            .context("plugin child process has no stdin")?;

        let mut reader = BufReader::new(stdout);
        let handshake =
            handshake::perform(&mut reader).context("plugin handshake failed")?;

        // No precedent before N plugins existed to compare against - see
        // `docs/architecture/plugin-modularity.md`'s Interfaces section.
        // Same "protocol is code, a mismatch is a hard load failure"
        // philosophy `handshake::verify` already applies to protocol version
        // mismatches, just for the manifest's declared language instead.
        if handshake.language != manifest.language {
            bail!(
                "plugin at {} declares language \"{}\" in its manifest but its \
                 handshake reports \"{}\" - refusing to load",
                manifest.manifest_dir.display(),
                manifest.language,
                handshake.language,
            );
        }

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
    /// Kept for the same reason as `project_root`: a crash relaunch
    /// (`Self::relaunch`) needs the exact command/language this process was
    /// spawned with, and no caller re-supplies it on that path.
    manifest: PluginManifest,
    /// Where this plugin's pid is recorded - `daemon::registry
    /// ::PluginRegistry::pid_file_for`'s per-language path for a registry-
    /// owned supervisor, or whatever legacy/test path a caller that predates
    /// the registry still passes. [`Self::relaunch`] rewrites *this* file,
    /// not a hardcoded one: before task 155 it always wrote the single
    /// legacy `plugin.pid` regardless of which language actually crashed,
    /// which two supervisors sharing that one file would have stepped on
    /// each other's records over - the exact hazard `PluginRegistry`'s own
    /// module doc calls out as "not deferred" until this was fixed.
    pid_file: PathBuf,
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
    /// Spawns `manifest`'s plugin for `project_root` - see
    /// [`PluginState::spawn`]. `pid_file` is where [`Self::relaunch`] records
    /// a crash-recovery respawn's fresh pid; the *first* pid (this call's own)
    /// is the caller's job to write, matching every existing caller
    /// (`PluginSupervisor::start`/`replay_pending`/`ensure_fresh`), which
    /// already write it themselves right after a successful spawn.
    pub fn spawn(project_root: &Path, manifest: &PluginManifest, pid_file: PathBuf) -> Result<Self> {
        let state = PluginState::spawn(project_root, manifest)?;
        Ok(Self {
            project_root: project_root.to_path_buf(),
            manifest: manifest.clone(),
            pid_file,
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
    pub fn apply_file_change(
        &self,
        conn: &Mutex<Connection>,
        file_path: impl Into<String>,
        embedding: &EmbeddingPipeline,
    ) -> Result<()> {
        let file_path = file_path.into();
        self.enqueue_pending(&file_path);

        if let Err(first_err) = self.send_one(conn, &file_path, embedding) {
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
            return self.replay_pending(conn, embedding).with_context(|| {
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
    fn replay_pending(&self, conn: &Mutex<Connection>, embedding: &EmbeddingPipeline) -> Result<()> {
        loop {
            let next = { self.pending.lock().unwrap().first().cloned() };
            let Some(file_path) = next else { return Ok(()) };
            self.send_one(conn, &file_path, embedding)?;
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
    pub fn ensure_fresh(
        &self,
        conn: &Mutex<Connection>,
        file_path: &str,
        embedding: &EmbeddingPipeline,
    ) -> Result<StalenessOutcome> {
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
        staleness::ensure_fresh(reader, writer, &mut conn, &self.project_root, file_path, id, embedding)
    }

    /// Asks the plugin's semantic layer to upgrade what the structural pass
    /// could only guess at, and commits its answer - see
    /// `watcher::apply::apply_semantic_pass`.
    ///
    /// This is the *whole-project* entry point, used once the cold-start
    /// bulk walk is done (`daemon::run`); the per-file pass that follows an
    /// incremental reparse needs no call of its own, because
    /// `apply_file_change` already sends it on the same locked round trip.
    ///
    /// Unlike [`Self::apply_file_change`], a dead plugin surfaces here as an
    /// ordinary `Err` rather than a relaunch: there is nothing pending to
    /// replay (a semantic pass owns no file the index is missing), and the
    /// caller treats a missing upgrade as best-effort.
    pub fn semantic_pass(
        &self,
        conn: &Mutex<Connection>,
        file_paths: Vec<String>,
        embedding: &EmbeddingPipeline,
    ) -> Result<()> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.lock().unwrap();
        let PluginState { io: PluginIo { reader, writer }, .. } = &mut *state;
        let mut conn = conn.lock().unwrap();
        apply_semantic_pass(reader, writer, &mut conn, file_paths, id, embedding)
    }

    fn send_one(&self, conn: &Mutex<Connection>, file_path: &str, embedding: &EmbeddingPipeline) -> Result<()> {
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
        apply_file_change_diff(reader, writer, &mut conn, file_path, id, embedding)
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
    /// `self.pid_file` - this process's own, not a hardcoded shared path - is
    /// rewritten too - left alone, it would keep naming a process that no
    /// longer exists, or worse, one a recycled pid now belongs to. Before
    /// task 155 this always wrote the single legacy `plugin.pid`, which was
    /// harmless while there was only ever one plugin but would have let two
    /// languages' relaunches overwrite each other's pid file once there was
    /// more than one.
    fn relaunch(&self, cause: &anyhow::Error) -> Result<()> {
        eprintln!(
            "g-mesh daemon: {} plugin process exited unexpectedly ({cause:#}) - \
             relaunching and replaying pending file changes",
            self.manifest.language
        );
        let fresh = PluginState::spawn(&self.project_root, &self.manifest)?;
        let pid = fresh.child.id();
        *self.state.lock().unwrap() = fresh;
        if let Err(err) = fs::write(&self.pid_file, pid.to_string()) {
            eprintln!(
                "g-mesh daemon: could not update the plugin pid file after relaunch: {err:#}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `files` (relative-path, contents pairs) under `dir`, creating
    /// whatever subdirectories they need - the fixture every digest test in
    /// this module builds on, whether or not it goes through a
    /// [`PluginManifest`].
    fn emit(dir: &Path, files: &[(&str, &str)]) {
        for (relative, contents) in files {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
    }

    /// A minimal fixture [`PluginManifest`] pointing at `dir`, with `ignore`
    /// as its `fingerprint_ignore` - everything else is irrelevant to
    /// [`fingerprint`], which only reads `manifest_dir` and
    /// `fingerprint_ignore`.
    fn manifest_for(dir: &Path, ignore: &[&str]) -> PluginManifest {
        PluginManifest {
            language: "fixture".to_string(),
            protocol_version: CURRENT_PROTOCOL_VERSION,
            plugin_version: String::new(),
            command: PathBuf::from("true"),
            args: Vec::new(),
            extensions: Vec::new(),
            fingerprint_ignore: ignore.iter().map(|s| s.to_string()).collect(),
            manifest_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn the_same_build_fingerprints_the_same_way_twice() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);

        let first = digest_of_plugin_build(dir.path(), &[]).unwrap();
        assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), first);
        assert_eq!(first.len(), FINGERPRINT_HEX_CHARS);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first} must be hex");
    }

    /// The case task 116 is about: nothing but the extractor's own compiled
    /// logic changed, and that has to be visible.
    #[test]
    fn changing_one_emitted_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

        fs::write(dir.path().join("extract.js"), "parse(); resolveLexically();").unwrap();

        assert_ne!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
    }

    /// `npm run build` rewrites every file on every invocation. A rebuild
    /// that changed nothing must not cost a project a full re-walk, which is
    /// why the digest is over content and not over mtimes.
    #[test]
    fn re_emitting_identical_bytes_leaves_the_fingerprint_alone() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(dir.path().join("extract.js"), "parse();").unwrap();

        assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
    }

    /// Moving a line from one module to another leaves the concatenated
    /// bytes identical - the path and length mixed in are what keep the two
    /// builds apart.
    #[test]
    fn moving_code_between_two_files_still_changes_the_fingerprint() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        emit(one.path(), &[("index.js", "ab"), ("extract.js", "c")]);
        emit(other.path(), &[("index.js", "a"), ("extract.js", "bc")]);

        let before = digest_of_plugin_build(one.path(), &[]).unwrap();
        let after = digest_of_plugin_build(other.path(), &[]).unwrap();

        assert_ne!(after, before);
    }

    #[test]
    fn a_file_in_a_subdirectory_is_part_of_the_build_too() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();"), ("lang/ts.js", "grammar();")]);
        let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

        fs::write(dir.path().join("lang/ts.js"), "grammar(2);").unwrap();

        assert_ne!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
    }

    /// An absent or unbuilt plugin is not a fingerprint of zero files - that
    /// would compare equal to every other unbuilt tree and read as "nothing
    /// changed".
    #[test]
    fn an_unbuilt_plugin_has_no_fingerprint_at_all() {
        let dir = tempfile::tempdir().unwrap();
        assert!(digest_of_plugin_build(dir.path(), &[]).is_err());
    }

    /// A change inside a directory on the built-in baseline ignore list (see
    /// `docs/architecture/plugin-modularity.md`'s "Built-in baseline ignore")
    /// must not move the fingerprint - the whole point of walking every file
    /// by default is that known dependency/VCS junk is the one thing that
    /// still has to be carved out by name.
    #[test]
    fn a_change_inside_a_baseline_ignored_directory_does_not_change_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();")]);
        let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

        emit(dir.path(), &[("node_modules/pkg/index.js", "module.exports = {};")]);
        assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);

        fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "module.exports = { changed: true };",
        )
        .unwrap();
        assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
    }

    /// The same, but for a directory named in a manifest's own
    /// `fingerprint_ignore` rather than the built-in baseline list -
    /// `[plugin.fingerprint].ignore` extends the baseline, it does not
    /// replace it.
    #[test]
    fn a_change_inside_a_manifest_declared_ignore_directory_does_not_change_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();")]);
        let manifest = manifest_for(dir.path(), &["vendor"]);
        let before = fingerprint(&manifest);
        assert_ne!(before, FINGERPRINT_UNAVAILABLE);

        emit(dir.path(), &[("vendor/lib.js", "var x = 1;")]);
        assert_eq!(fingerprint(&manifest), before);

        fs::write(dir.path().join("vendor/lib.js"), "var x = 2;").unwrap();
        assert_eq!(fingerprint(&manifest), before);
    }

    /// The critical correctness property behind both ignore-list tests above:
    /// ignoring specific directories must not accidentally ignore everything
    /// else too.
    #[test]
    fn a_change_to_a_non_ignored_file_still_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        emit(dir.path(), &[("index.js", "run();"), ("vendor/lib.js", "var x = 1;")]);
        let manifest = manifest_for(dir.path(), &["vendor"]);
        let before = fingerprint(&manifest);

        fs::write(dir.path().join("index.js"), "run(2);").unwrap();

        assert_ne!(fingerprint(&manifest), before);
    }

    /// The bundled plugin's own build is readable from the running test
    /// binary - `core/build.rs` has just built the plugin it points at - so
    /// every build stamp this process publishes names a real fingerprint
    /// rather than degrading to [`FINGERPRINT_UNAVAILABLE`].
    ///
    /// (What the *index* is stamped with is no longer this value - see
    /// `daemon::registry::indexer_version` and its own tests.)
    #[test]
    fn the_bundled_plugins_build_is_fingerprintable_from_the_test_binary() {
        let bundled = bundled_fingerprint();

        assert_ne!(
            bundled, FINGERPRINT_UNAVAILABLE,
            "the test binary's own plugin build must be readable - `cargo test` builds it"
        );
        assert_eq!(bundled.len(), FINGERPRINT_HEX_CHARS);
        assert!(bundled.chars().all(|c| c.is_ascii_hexdigit()), "{bundled} must be hex");
    }

    /// The check `docs/architecture/plugin-modularity.md`'s Interfaces
    /// section adds right after `handshake::perform` succeeds: a manifest
    /// whose declared `language` disagrees with what the live plugin's
    /// handshake actually reports is a hard-fail, naming both values - the
    /// bundled JS/TS plugin's handshake reports `"typescript"` (see
    /// `protocol::types`'s handshake test), so declaring anything else in
    /// the manifest must be refused.
    #[test]
    fn spawning_a_manifest_whose_language_disagrees_with_the_live_handshake_hard_fails_naming_both() {
        let manifest = PluginManifest { language: "python".to_string(), ..bundled_manifest() };
        let project = tempfile::tempdir().unwrap();

        let err = match PluginProcess::spawn(project.path(), &manifest, project.path().join("plugin.pid")) {
            Ok(_) => panic!("spawning against a manifest declaring the wrong language must fail"),
            Err(err) => err,
        };

        let message = format!("{err:#}");
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("typescript"), "{message}");
    }
}
