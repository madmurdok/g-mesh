pub mod build_stamp;
pub mod bulk_index;
pub mod identity;
pub mod indexing_status;
pub mod lifecycle;
pub mod manifest;
pub mod plugin;
pub mod registry;
pub mod semantic;
/// A fake, protocol-speaking plugin for the unit tests in this module tree -
/// the only way to exercise *two* languages without a second real plugin
/// existing. Compiled only under `cfg(test)`, and never referenced by
/// anything that ships.
#[cfg(test)]
pub(crate) mod test_plugin;

use std::fs::{self, File, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::lifecycle::{CoreActivity, IdleTimeouts};
use crate::daemon::registry::PluginRegistry;
use crate::gc::last_used;
use crate::ipc;
use crate::mcp;
use crate::storage::connection::{self, project_dir};
use crate::storage::schema;
use crate::watcher::debounce::Debouncer;
use crate::watcher::ProjectWatcher;

/// Unix only: on Windows the endpoint is a pipe name, not a file in this
/// directory (see [`socket_path`] and `ipc::windows`).
#[cfg(unix)]
const SOCKET_FILE: &str = "daemon.sock";
const PID_FILE: &str = "daemon.pid";
/// Which build the live daemon started from, so a shim (or `cli::status`) can
/// tell an incumbent that is still this build from one that has been left
/// behind by an upgrade - see `daemon::build_stamp`.
const BUILD_STAMP_FILE: &str = "daemon.build";
/// Held by a shim while it decides whether to bootstrap a daemon and while
/// it waits for the one it spawned to come up (see `shim::connect_or_bootstrap`).
const BOOTSTRAP_LOCK_FILE: &str = "bootstrap.lock";
/// Held by a running daemon for its whole lifetime: whoever owns it owns the
/// project's socket. Deliberately a different file from the bootstrap lock -
/// the shim holds that one *while* spawning the daemon, so a daemon waiting
/// on it would deadlock against the shim waiting for the daemon.
const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// How long the watcher thread waits, after the most recent raw filesystem
/// event for a given path, before treating that path's burst as settled and
/// asking the plugin to reparse it - the window
/// [`watcher::debounce::Debouncer`](crate::watcher::debounce::Debouncer)
/// coalesces around. Task 129 wired that debouncer in; before it, this
/// constant (then named `WATCH_POLL_INTERVAL`, three orders of magnitude
/// larger) only bounded how long the loop blocked between iterations so it
/// never parked forever on a channel whose sender had gone away - nothing
/// depended on its actual value. That is no longer true: this loop now also
/// depends on this being short enough to notice a settled burst promptly (a
/// path recorded just before the channel goes quiet is only drained the next
/// time the loop wakes up on its own), so it doubles as both the debounce
/// window and the idle poll bound. Deliberately not related to either idle
/// timeout: the plugin's sleep is decided by `daemon::lifecycle`, never by how
/// long this happened to wait.
///
/// 300ms: long enough to coalesce the rapid-fire re-saves a real burst
/// produces (editor autosave, a formatter re-writing the file it just saved,
/// `git checkout`/`git pull` touching many files at once) into a single
/// plugin round trip per file, short enough that an isolated, ordinary edit
/// still reaches the plugin promptly - in the same range most editors and
/// file-watch tooling already use for the same trade-off.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// Where a project's daemon listens. The shim derives the same endpoint from
/// its own cwd, which is how the two find each other without any configured
/// port or discovery step.
///
/// One identity, spelled twice: on Unix it is the AF_UNIX socket file inside
/// the project's state directory, and on Windows it is a name in the
/// machine-wide pipe namespace carrying the same project hash the state
/// directory is named after. `ipc::windows`'s header has the full argument
/// for why the second cannot be a file.
pub fn endpoint(root: &Path) -> Result<ipc::Endpoint> {
    let dir = project_dir(root)?;
    endpoint_in(&dir).with_context(|| format!("failed to derive the daemon endpoint from {}", dir.display()))
}

/// [`endpoint`] resolved from an already-known state directory rather than
/// from a project root - the form `daemon::lifecycle` has on its way out of
/// being a daemon, and the same shape as [`pid_path_in`] and
/// [`build_stamp_path_in`].
///
/// It works on both platforms for one reason, and it is the reason the
/// Windows naming scheme is what it is: the state directory is *named after*
/// the project hash (`storage::connection::project_dir`), so the directory
/// carries everything the pipe name needs. `None` only for a path with no
/// final component, which no state directory has.
pub fn endpoint_in(state_dir: &Path) -> Option<ipc::Endpoint> {
    #[cfg(unix)]
    {
        Some(ipc::Endpoint::at_path(state_dir.join(SOCKET_FILE)))
    }
    #[cfg(windows)]
    {
        Some(ipc::Endpoint::named(state_dir.file_name()?.to_str()?))
    }
}

/// The AF_UNIX socket file a project's daemon listens on.
///
/// Unix-only, and deliberately so: it is the one part of the endpoint that is
/// a path, and a Windows build that could ask for it would be asking for a
/// file that never exists. Everything that only needs to *reach* the daemon
/// goes through [`endpoint`] instead; this is for the callers that genuinely
/// handle the file (`cli::stop` clearing one, the test suite waiting for one
/// to appear).
#[cfg(unix)]
pub fn socket_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(SOCKET_FILE))
}

/// Records the live daemon's pid next to its socket, so tooling can tell a
/// stale socket file from a running daemon.
pub fn pid_path(root: &Path) -> Result<PathBuf> {
    Ok(pid_path_in(&project_dir(root)?))
}

/// The bundled JS/TS (`plugin::BUNDLED_LANGUAGE`) plugin's own pid file -
/// what this function named before task 155 replaced the daemon's single
/// `Arc<PluginSupervisor>` with a `PluginRegistry` giving each language a pid
/// file of its own (`plugin-<language>.pid`, see
/// `daemon::registry::PluginRegistry::pid_file_for`). Kept, rather than
/// removed, purely as a convenience alias for the test suite and the couple
/// of other callers that predate per-language pid files and only ever meant
/// "the bundled plugin's pid" - real multi-language-aware tooling
/// (`cli::status`, `cli::stop`, `cli::clean`) does not call this; it lists
/// every `plugin-*.pid` file instead (`daemon::registry::discovered_pid_files`).
pub fn plugin_pid_path(root: &Path) -> Result<PathBuf> {
    Ok(plugin_pid_path_in(&project_dir(root)?))
}

/// The same two paths resolved from an already-known state directory rather
/// than from a project root - the form a scan over `~/.g-mesh/projects/*`
/// has, where the root a directory was named after may not even exist any
/// more (`cli::clean`).
pub fn pid_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(PID_FILE)
}

pub fn plugin_pid_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(registry::plugin_pid_file_name(plugin::BUNDLED_LANGUAGE))
}

/// Where the live daemon records the build it started from, resolved from a
/// project root and - like the pid files - from an already-known state
/// directory too, for callers that have one but no root.
pub fn build_stamp_path(root: &Path) -> Result<PathBuf> {
    Ok(build_stamp_path_in(&project_dir(root)?))
}

pub fn build_stamp_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(BUILD_STAMP_FILE)
}

/// The file shims serialize their bootstrap on, derived exactly like the
/// socket and pid paths so every process agrees on it without configuration.
pub fn lock_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(BOOTSTRAP_LOCK_FILE))
}

/// The singleton lock a running daemon holds for its whole lifetime, resolved
/// from a project root and - like the pid files - from an already-known state
/// directory.
pub fn daemon_lock_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(DAEMON_LOCK_FILE))
}

pub fn daemon_lock_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(DAEMON_LOCK_FILE)
}

/// Reads a pid out of one of the files above. `None` for a file that isn't
/// there or doesn't hold a pid - both mean "nothing recorded", which is what
/// every caller does with them anyway.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process with this pid currently exists - [`crate::process::is_alive`]
/// under the name the rest of this module tree has always called it by.
///
/// Inherently a snapshot, and pids are reused, so a caller that cares
/// (`cli::status`) corroborates it with the daemon's endpoint rather than
/// trusting a recorded pid on its own.
pub fn is_process_alive(pid: u32) -> bool {
    crate::process::is_alive(pid)
}

/// Whether something is accepting connections on this project's socket right
/// now - the liveness check that cannot be fooled by a recycled pid, since it
/// is answered by the daemon itself.
///
/// The connection is opened and immediately dropped; the daemon treats that
/// as a client that hung up before saying anything.
pub fn is_listening(root: &Path) -> Result<bool> {
    Ok(ipc::Stream::connect(&endpoint(root)?).is_ok())
}

/// Per-project daemon core: opens the SQLite index (checking schema
/// version), binds the project's daemon endpoint, builds the initial index if
/// the project has never been walked (`bulk_index`), registers the file
/// watcher, and serves an MCP session per connection until it is stopped or
/// its own long idle timeout expires (`daemon::lifecycle`).
///
/// # Why the socket is bound before the index exists
///
/// It used to be bound after the cold-start walk, so that a client could not
/// reach a daemon whose graph was half built. Task 105 moved it ahead of the
/// walk and put the same guarantee in the response layer instead: an accepted
/// connection is answered with an explicit "still indexing" tool error until
/// the walk commits (see `daemon::indexing_status`, which carries the full
/// argument). Nobody is served off a partial graph either way; the difference
/// is that a walk longer than `shim::BOOTSTRAP_TIMEOUT` now costs the caller a
/// retry instead of costing it its whole tool surface.
///
/// The bind is deliberately the *first* slow-ish thing that happens, ahead of
/// even the plugin spawn: the shim's bootstrap budget is a race against the
/// socket appearing, and nothing the daemon does at startup should be allowed
/// to grow into that budget again.
///
/// The endpoint itself is `crate::ipc`'s business: an AF_UNIX socket file on
/// Unix, a named pipe on Windows. Everything this function does around the
/// bind - the ordering, the pid file, the stale-endpoint clearing - is the
/// same on both.
pub fn run(root: &Path) -> Result<()> {
    let dir = project_dir(root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;

    // Singleton guard, taken before anything else touches the project's
    // files: whoever holds it owns the socket. A second daemon for the same
    // project - spawned by a shim that raced ahead of this one's bind, or by
    // hand - exits here instead of going on to clear and rebind a socket its
    // predecessor is already serving. Losing this race is the expected,
    // healthy outcome, not an error: the caller connects to the incumbent.
    let singleton = match acquire_singleton_lock(&dir)? {
        Some(lock) => lock,
        None => return stand_down(root),
    };

    // Published here - after the singleton lock, so it always describes the
    // process that actually owns this project, and long before the socket is
    // bound, so it is never *transiently* missing. That ordering is what
    // lets a shim read "listening, but no stamp" as "a daemon from before
    // this check existed" rather than "a daemon that has not got round to
    // publishing yet"; see `daemon::build_stamp` for what is compared and why.
    // Failing to publish is not fatal: the worst it costs is a daemon that
    // reads as outdated and gets replaced, which is the safe direction.
    match build_stamp::of_running_process() {
        Ok(stamp) => {
            if let Err(err) = build_stamp::write(&build_stamp_path_in(&dir), &stamp) {
                eprintln!("g-mesh daemon: could not publish its build stamp: {err:#}");
            }
        }
        Err(err) => eprintln!("g-mesh daemon: could not describe its own build: {err:#}"),
    }

    // Discovery runs here - ahead of the index, the socket and everything
    // else this daemon owns - because the generation check below cannot be
    // made without it (see `registry::indexer_version`), and that check has to
    // happen before anything trusts what is in the index. Nothing it needs
    // exists yet or has to: it reads `plugin.toml` files off two fixed roots
    // and touches no per-project state, so the only ordering constraint it
    // carries is the singleton lock above, already taken. Moving it in front
    // of the bind costs the shim's bootstrap budget a couple of small file
    // reads, and buys a hard startup failure (a malformed manifest, two
    // plugins claiming one extension) that happens before a socket and pid
    // file are published for a daemon that is about to give up on them.
    //
    // Malformed manifest, or two plugins claiming the same extension, is a
    // hard daemon-startup failure naming the manifest path and the specific
    // problem (`daemon::manifest::discover`'s own contract) - the same
    // "protocol is code, a mismatch is a hard load failure" philosophy
    // `handshake::verify` already applies, just at discovery time instead of
    // at a live handshake. `default_roots()` is what makes this test-safe:
    // `G_MESH_PLUGIN_ROOTS_OVERRIDE` replaces both standard roots wholesale,
    // so a test can hand this a fixture directory instead of touching the
    // real `~/.g-mesh/plugins/`.
    let discovered =
        manifest::discover(&manifest::default_roots()).context("failed to discover language plugins")?;

    let conn = connection::open(root).context("failed to open the project's SQLite index")?;
    // The generation names every discovered plugin's build as well as core's
    // own pipeline (`registry::indexer_version`), so an index filled by a
    // plugin that has since been rebuilt is thrown away here - which is what
    // makes the walk below happen at all. Before task 116 only core's half was
    // compared, and a plugin-only change left every existing index intact and
    // wrong; before task 163 only the *bundled* plugin's half was, which said
    // the same thing about every other language.
    if schema::ensure_current(&conn, &registry::indexer_version(&discovered))
        .context("failed to check the index's schema and indexer versions")?
    {
        eprintln!("g-mesh daemon: index (re)initialized - a full reindex is needed");
    }
    // Recorded here rather than once the daemon is serving: a start that gets
    // as far as opening the index is already this project being used, and a
    // cold start that then spends minutes in its bulk walk must not read as
    // minutes of idleness to a GC scan running alongside it.
    last_used::touch(&conn).context("failed to record that the project was used")?;
    // Asked before anything can answer it, and answered by a recorded fact
    // rather than by the schema being fresh: a walk killed half way through
    // leaves a current schema behind a partial graph, and that project is
    // still owed its index.
    let needs_bulk_index = !schema::bulk_index_completed(&conn)
        .context("failed to check whether the project has been indexed")?;
    // Independent of the walk: a project that has been walked can still owe
    // its whole-project semantic pass, if a previous attempt at it (this
    // cold-start branch, `cli::init`, or `cli::reindex`) was interrupted
    // after `bulkIndexedAt` was already recorded but before the pass itself
    // finished - see `daemon::semantic`'s module doc. `needs_bulk_index`
    // above already covers a project with no walk at all (its own cold-start
    // branch below asks for the pass as part of that walk); this is what
    // notices the case that branch will never take again, because the walk
    // it would have run is not owed.
    let needs_semantic_pass_retry = !needs_bulk_index
        && !schema::semantic_pass_completed(&conn)
            .context("failed to check whether the project's semantic pass has completed")?;
    let conn = Arc::new(Mutex::new(conn));

    // ProjectWatcher reports canonicalized absolute paths (see its own doc
    // comment on FSEvents' /var -> /private/var behavior); canonicalizing
    // here too is what lets `relative_wire_path` turn those back into the
    // project-relative paths the wire protocol and storage layer use.
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;

    let endpoint = endpoint_in(&dir)
        .with_context(|| format!("failed to derive the daemon endpoint from {}", dir.display()))?;
    // On Unix a socket file left behind by a crashed daemon makes bind() fail
    // with AddrInUse forever, so it is cleared first. That is only safe
    // because the singleton lock above guarantees no other daemon is serving
    // this project: any socket file still here belongs to a dead one. On
    // Windows this is a no-op, because a pipe name cannot outlive the process
    // that held it - see `ipc::windows`'s header.
    endpoint.clear_stale();
    // Bound here, before the plugin and long before the bulk walk: from this
    // point a shim's `connect()` succeeds (the kernel queues it on the
    // listener's backlog until the accept loop below is up), which is what
    // its bootstrap timeout is actually waiting for. See this function's doc
    // comment for what replaced the old "bind last" guarantee.
    let listener = ipc::Listener::bind(&endpoint)
        .with_context(|| format!("failed to bind the daemon endpoint at {endpoint}"))?;

    // Still written immediately after the bind, so "the pid file exists"
    // continues to mean "something is listening" for `cli::status` and for
    // the tests that wait on it - it just no longer also means "and the index
    // is complete", which `meta.bulkIndexedAt` is the record of.
    let pid_file = dir.join(PID_FILE);
    fs::write(&pid_file, std::process::id().to_string())
        .with_context(|| format!("failed to write pid file {}", pid_file.display()))?;

    // And recorded in the lock file too, now that this process is genuinely
    // serving. The pid file above answers "which process is the daemon"; this
    // answers "is the lock's holder still doing the job the lock entitles it
    // to", which is the only question a process that cannot take the lock can
    // usefully ask - and the one nothing could answer before task 184. See
    // `record_serving_owner`.
    record_serving_owner(&singleton);

    // Both idle timers are resolved once, here, from the project's
    // config.toml (or its documented defaults, for a project with none), and
    // handed to everything that has to honor them - see `daemon::lifecycle`
    // for what each one governs.
    let project_config =
        crate::config::read_project_config(root).context("failed to read the project's config.toml")?;
    let timeouts = IdleTimeouts::from_config(&project_config);

    // Not loaded here, not even in the background: `EmbeddingPipeline::load`
    // does no I/O and spawns no thread, it only stores the config behind an
    // `OnceLock` that resolves on the first real `apply` call - see
    // `embedding::pipeline`'s module doc. A background `thread::spawn` here
    // was tried and measured to still cost daemon startup enough to fail
    // `serving_while_indexing`'s 1s "already-walked restart" budget and
    // `cli::clean`'s 10s "daemon is listening" wait under load - a bare
    // thread spawn competing with the plugin spawn and the accept loop for
    // scheduling, on top of the ~600MiB ONNX load itself once it actually
    // runs. Whichever of the bulk walk below or the plugin supervisor's first
    // incremental write asks first pays the real load cost, synchronously, on
    // its own thread - never this one, and never before something has
    // actually asked to embed. A model that is not available on this machine
    // does not stop the daemon - see `EmbeddingPipeline::load`'s doc comment -
    // it just means nothing gets embedded until the user has run
    // `g-mesh model fetch` themselves. Never this process: the daemon has no
    // way to reach the fetcher (see `cli::model`), on purpose.
    let embedding = Arc::new(crate::embedding::EmbeddingPipeline::load(&project_config.embedding));

    // The cold-start bulk walk below (`daemon::bulk_index::run`) needs its
    // own view of what was discovered: it spawns one one-shot `--bulk-index`
    // process per language directly from each manifest's `command`/`args`,
    // which is a different (and simpler) shape than `PluginRegistry`'s lazy,
    // long-lived per-language supervisors below - so it takes a plain copy
    // rather than reaching into the registry for one. Cloning here, before
    // `discovered` is moved into the registry, is cheap (discovery runs once,
    // at startup, over a small number of plugins) and leaves
    // `PluginRegistry`'s own public API untouched.
    let discovered_for_bulk_index = discovered.clone();

    // Nothing is spawned by this call - see `daemon::registry`'s module doc
    // for why lazy, per-language spawning is the whole point of this type.
    // The registry, not a bare supervisor, is what the rest of the daemon
    // gets: from here on *any* language's plugin is allowed to be absent
    // (never yet needed, or asleep on its idle timeout) with file changes
    // queueing up behind whichever supervisor eventually claims them, which
    // nothing holding a single hardcoded supervisor could express. The
    // bundled JS/TS plugin goes through this exact same discovered-manifest
    // path now too - no permanent hardcoded fallback survives it (see
    // `docs/architecture/plugin-modularity.md`'s Options Considered #1).
    let registry = Arc::new(PluginRegistry::new(
        &canonical_root,
        dir.clone(),
        discovered,
        timeouts.plugin,
        Arc::clone(&embedding),
    ));

    // Starts ticking at startup, so a daemon nobody ever connects to still
    // goes away on its own eventually rather than living until the machine
    // reboots.
    let core_activity = CoreActivity::new();

    // Decided from a fact recorded on disk, not from how this start went, so
    // a restart against an already-walked project (the common case) is `ready`
    // from its first instant and no caller ever sees "still indexing" for it.
    let indexing = if needs_bulk_index { IndexingStatus::indexing() } else { IndexingStatus::ready() };

    // The accept loop moves to a thread of its own so the walk below can run
    // alongside it. It, not this function, is the daemon's real main loop;
    // what is left here is finite startup work, and this thread spends the
    // rest of its life supervising (`lifecycle::supervise`).
    //
    // Its outcome comes back over a channel rather than through `join`,
    // because the supervising thread has to wake on a schedule of its own -
    // and a `join` it could not interrupt is exactly what stopped the old MVP
    // daemon from ever ending by itself.
    let (accept_result, accept_loop) = mpsc::channel();
    {
        let conn = Arc::clone(&conn);
        let registry = Arc::clone(&registry);
        let core_activity = Arc::clone(&core_activity);
        let indexing = indexing.clone();
        let embedding = Arc::clone(&embedding);
        thread::spawn(move || {
            let _ = accept_result.send(serve_forever(
                listener,
                conn,
                registry,
                core_activity,
                indexing,
                embedding,
            ));
        });
    }

    // Cold start only, and before the watcher: a bulk walk racing incremental
    // updates could commit its own (older) parse of a file over one the
    // watcher had just refreshed. Connections accepted while this runs are
    // answered with `mcp`'s "still indexing" error rather than off the batches
    // committed so far, so a client's query is never answered off a half-built
    // graph - the same promise the old "bind only once this returns" ordering
    // made, kept without making the client unreachable to make it. A failure
    // here is fatal for the same reason a failed plugin handshake is - an
    // empty index that looks like a working one is worse than a daemon that
    // says why it didn't start - and is recoverable: the completion marker
    // stays unset, so the next start walks again. Any client connected at that
    // moment loses its session when this process exits, which is the honest
    // outcome: a daemon that stayed up would hold the singleton lock while
    // serving nothing, and every later shim would find it listening, current,
    // and reuse it forever.
    if needs_bulk_index {
        let summary = bulk_index::run(&canonical_root, &conn, &embedding, &discovered_for_bulk_index)
            .context("failed to build the project's initial index")?;
        // Flipped *before* the completion marker is written, and the order
        // matters. The two facts become true at the same moment - the walk is
        // over - but they are read by different parties: the flag governs
        // what this process answers, `bulkIndexedAt` governs whether the
        // *next* process walks again. Writing the marker second means any
        // outside observer of it (`cli::status`, the integration tests) can
        // only ever see it once real answers are already being given; the
        // other order would let someone read "indexed" off the database and
        // still be told "still indexing" by the daemon that wrote it.
        //
        // Ahead of the watcher for the same reason a project that owed no
        // walk starts out `ready`: the watcher only ever matters for edits
        // made after it is registered, so gating answers on it would buy
        // nothing the fast path does not already do without.
        indexing.mark_ready();
        schema::record_bulk_index(&conn.lock().unwrap())
            .context("failed to record that the project was indexed")?;
        // A walk that took minutes is minutes the core spent working, not
        // minutes of silence - the same reasoning `last_used::touch` above
        // applies to a GC scan, applied to the core's own idle timer.
        core_activity.request();
        eprintln!(
            "g-mesh daemon: initial index built - {} nodes, {} edges ({} imports linked to their target file)",
            summary.nodes, summary.edges, summary.linked_imports
        );
        if summary.skipped_lines > 0 {
            eprintln!(
                "g-mesh daemon: {} unreadable lines were skipped - the index may be incomplete",
                summary.skipped_lines
            );
        }

        // The walk's edges are in and linked, so the graph is complete enough
        // to be worth asking the type checker about - and the daemon is
        // already answering off it (`mark_ready` above), which is why this
        // sits *after* that call rather than in front of it: the semantic
        // layer's job is to make existing answers better, never to delay the
        // first one. An empty file list is what "the whole project" looks
        // like on the wire (see `ControlMessage::SemanticPass`); no single
        // file changed here, the project simply became resolvable at once.
        //
        // Best-effort, like every other semantic pass: a project whose
        // checker cannot start is a project served by its structural graph,
        // which is the state it was in a moment ago anyway. Failing daemon
        // startup over it would throw away a perfectly good index.
        //
        // Run inline, not backgrounded, deliberately - a background thread
        // was tried here and reverted. `get_or_spawn` has to spawn the plugin
        // process itself the first time anything needs it (this call used to
        // be the one exception, running against a supervisor the daemon had
        // already spawned unconditionally before this point), and a query
        // arriving alongside it used to wait out that whole spawn - the
        // registry's supervisors lock was held for all of it. That was task
        // 164, and it is fixed where it belonged: the lock is now held for a
        // map lookup only, so a concurrent query waits on nothing (see
        // `daemon::registry`'s doc comment). Backgrounding this call instead
        // was the wrong fix for it, and still is: this pass
        // and the watcher's own incremental per-file semantic passes both
        // ultimately serialize on the same plugin process, but nothing
        // orders *which* of two independently-triggered passes commits last,
        // and a whole-project pass that lands after a newer incremental one
        // can overwrite its correct, fresher edges with stale ones - exactly
        // the failure `core/tests/overload_call_binding.rs` caught when this
        // was tried backgrounded (a collapsed edge the incremental pass had
        // already cleaned up came back). Inline keeps this pass strictly
        // before the watcher (registered further down) can trigger any of
        // its own, which is what baseline's eager, pre-registry spawn timing
        // gave for free and this daemon still needs.
        //
        // Which plugin this asks, and why only one of them, is
        // `daemon::semantic`'s to say - as is the reason `cli::init` and
        // `cli::reindex` now run the very same pass off their own walks
        // rather than leaving it to a daemon start that will never come.
        // Spawns the plugin if nothing has needed it yet, same as any other
        // first touch.
        let semantic_outcome = semantic::run_with_registry(&registry, &conn);
        match &semantic_outcome {
            Ok(true) => eprintln!("g-mesh daemon: semantic pass over the freshly built index complete"),
            Ok(false) => {}
            Err(err) => eprintln!(
                "g-mesh daemon: the semantic pass over the freshly built index failed ({err:#}) - \
                 its edges keep whatever the structural pass resolved"
            ),
        }
        // `Ok(_)` either way - the pass ran, or there was nothing for it to
        // do (no bundled plugin) - is what `record_semantic_pass`'s own doc
        // says counts as complete. Only `Err` (the pass was asked for and did
        // not finish) must leave this unset, so a later start still finds it
        // owed.
        if semantic_outcome.is_ok() {
            if let Err(err) = schema::record_semantic_pass(&conn.lock().unwrap()) {
                eprintln!("g-mesh daemon: failed to record that the semantic pass completed ({err:#})");
            }
        }
    } else if needs_semantic_pass_retry {
        // The walk this project was owed already happened, in some earlier
        // start or in `cli::init` / `cli::reindex` - `needs_bulk_index` above
        // is false, so the branch that would normally ask for this pass will
        // never run. Asked for here instead, at the same point in startup
        // (before the watcher, for the same race the comment above this
        // block explains) and against the same registry, so a project whose
        // pass was interrupted gets exactly one more chance at it per daemon
        // start rather than none.
        eprintln!(
            "g-mesh daemon: the project was walked but its semantic pass never completed - retrying it"
        );
        let semantic_outcome = semantic::run_with_registry(&registry, &conn);
        match &semantic_outcome {
            Ok(true) => {
                eprintln!("g-mesh daemon: semantic pass over the previously-interrupted index complete")
            }
            Ok(false) => {}
            Err(err) => eprintln!(
                "g-mesh daemon: retrying the semantic pass failed ({err:#}) - \
                 it will be retried again on the next start"
            ),
        }
        if semantic_outcome.is_ok() {
            if let Err(err) = schema::record_semantic_pass(&conn.lock().unwrap()) {
                eprintln!("g-mesh daemon: failed to record that the semantic pass completed ({err:#})");
            }
        }
    }

    let watcher = ProjectWatcher::new(root).context("failed to start the file watcher")?;
    {
        let conn = Arc::clone(&conn);
        let registry = Arc::clone(&registry);
        let root = canonical_root.clone();
        // `watcher::debounce::Debouncer` is wired in here (task 129): raw
        // events are recorded into it instead of routed straight to the
        // registry, and only a path whose debounce window has gone quiet is
        // actually sent to the plugin - see `watch_and_route_once` below for
        // the per-iteration shape and `DEBOUNCE_WINDOW` for the window and
        // why its value is no longer a "nothing depends on this" constant.
        //
        // `watcher::burst::BurstBatcher` is a different type for a different
        // problem and is deliberately *not* wired in here. Its job is
        // coalescing already-produced diffs from *several different files*
        // into one SQLite transaction - but the plugin's own wire protocol
        // (`protocol::types`, `plugins/typescript/src/protocol.ts`) has no
        // batch request: every file still needs its own `FileChanged`
        // round trip to the plugin no matter how those round trips' diffs
        // get committed afterward, so `BurstBatcher` would not reduce the
        // thing this ticket is about - plugin round trips - for a burst
        // touching many distinct files (a `git checkout`, say). What it
        // would reduce is SQLite commit count, and reaching that would mean
        // splitting `daemon::plugin::PluginProcess::apply_file_change`'s
        // single locked round trip into "get the diff from the plugin" and
        // "commit it", so several files' round trips could share one
        // `BurstBatcher::flush_if_ready` commit - a real change to that
        // module's crash-recovery/pending-queue contract and to
        // `ensure_fresh`'s synchronous per-file check (both hold the same
        // connection this would need to leave uncommitted for longer), not
        // "construct an existing type here". `Debouncer` alone already
        // clears this ticket's acceptance bar - fewer plugin round trips for
        // a burst of rapid saves - for the shape its own problem statement
        // leads with (editor autosave rewriting the same file repeatedly);
        // batching *different* files' commits is real, separate work left
        // for its own ticket rather than folded in here as scope creep.
        thread::spawn(move || {
            let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
            loop {
                watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
            }
        });
    }

    // Startup is over; what is left is the two idle timers and the accept
    // loop's outcome, whichever arrives first. `supervise` returning `Ok` is
    // this daemon deciding it has been unused long enough to go - `main`
    // returns and the OS reclaims the socket, the watchers and the SQLite
    // handle.
    let outcome = lifecycle::supervise(&dir, &registry, &core_activity, timeouts, accept_loop);

    // Released here, explicitly, rather than whenever this frame happens to
    // unwind. `supervise` has already removed the socket and the pid file on
    // its way out, so from this instant the lock is the *only* thing still
    // claiming this project - and every other process reads a held lock as a
    // daemon that serves it. Anything slow between here and the process
    // actually going away (a teardown, a static destructor of a statically
    // linked dependency, a thread that will not join) would therefore leave a
    // live process holding a project nobody can reach, which is task 184's
    // wedge. Dropping the file closes the fd, which is what releases the
    // advisory lock; nothing after this point can put it back.
    drop(singleton);
    outcome
}

/// What a daemon that lost the singleton race does about it.
///
/// Losing to a healthy incumbent is the expected outcome and not an error: the
/// caller connects to the incumbent, and the exit is quiet. Losing to a holder
/// that is *not* serving is a different thing entirely (the caller is about to
/// wait out its whole bootstrap timeout on a socket that will never appear), so
/// that one exits non-zero with a diagnostic naming the offending pid.
/// Nobody reads a detached daemon's stderr, which is why the shim recovers
/// from this state on its own (`shim::connect_or_bootstrap`); this message is
/// for the person who ran `g-mesh daemon` by hand, and for a daemon log if one
/// is ever collected.
fn stand_down(root: &Path) -> Result<()> {
    match inspect_daemon_lock(root)? {
        DaemonLock::Wedged { pid } => anyhow::bail!(
            "pid {pid} holds the daemon lock for {} but is not serving it - nothing can connect \
             to it and no other daemon can take over while it lives; run `g-mesh stop` in that \
             project to clear it",
            root.display()
        ),
        // `Free` is reachable only if the incumbent released the lock between
        // the failed acquisition and this check, which is a bootstrap that
        // arrived a moment too early rather than a fault: the next one wins.
        DaemonLock::Free | DaemonLock::Serving | DaemonLock::Starting => {
            eprintln!("g-mesh daemon: another daemon already serves {} - exiting", root.display());
            Ok(())
        }
    }
}

/// One iteration of the watcher thread's loop: waits up to [`DEBOUNCE_WINDOW`]
/// for the next raw change, records it into `debouncer`, then routes every
/// path whose debounce window has gone quiet since - possibly none, possibly
/// more than one - to `registry`.
///
/// Pulled out of the `thread::spawn` closure in [`run`] as its own function
/// so a test can drive it directly, one call per iteration, without spawning
/// a real background thread: [`run`]'s own loop is just this called
/// unconditionally forever.
fn watch_and_route_once(
    watcher: &ProjectWatcher,
    debouncer: &mut Debouncer,
    root: &Path,
    conn: &Mutex<Connection>,
    registry: &PluginRegistry,
) {
    if let Some(path) = watcher.next_change(DEBOUNCE_WINDOW) {
        debouncer.record(path);
    }
    for settled in debouncer.drain_ready() {
        let Some(file_path) = relative_wire_path(root, &settled) else {
            // Outside the project root - shouldn't happen given how
            // ProjectWatcher is scoped, but there is nothing to route a
            // plugin request for if it does.
            continue;
        };
        if file_path.is_empty() {
            // The project root itself, which macOS reports as a change to
            // the directory a file was written in. There is no file to
            // reparse, and queueing it would put a path the plugin cannot
            // answer for into the replay list a sleeping core builds.
            continue;
        }
        // Routed to whichever language claims this file's extension,
        // spawning that plugin if this is the first file of its kind
        // (`daemon::registry::PluginRegistry::file_changed`); applied now if
        // that plugin is awake, queued for its next wake if it is asleep -
        // the supervisor it resolves to owns that decision because only it
        // can read both facts at once.
        registry.file_changed(conn, file_path);
    }
}

/// Runs the MCP accept loop until the process is killed.
///
/// This is the only async part of the daemon, and it is confined to a thread
/// of its own: `rmcp` requires tokio, but SQLite, the plugin bridge, the
/// watcher and the bulk walk have no use for it, so the runtime is entered
/// here rather than wrapped around a daemon that would otherwise gain nothing
/// from it. The listener is bound synchronously by [`run`] and only then
/// handed to tokio (`ipc::Listener::into_async`), which keeps the
/// bind/pid-file ordering the bootstrap race depends on exactly where it was
/// - and is the requirement that decided how `crate::ipc` had to be shaped.
///
/// `indexing` is cloned into every accepted session, which is what lets a
/// connection made during the cold-start walk be answered honestly rather
/// than refused - see `daemon::indexing_status`.
///
/// `core_activity` is what stops the core's own idle timeout from firing under
/// a client that is merely quiet: every accepted connection holds a guard for
/// as long as it lives, so only a project with nobody attached can ever be
/// found idle.
fn serve_forever(
    listener: ipc::Listener,
    conn: Arc<Mutex<Connection>>,
    registry: Arc<PluginRegistry>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
    embedding: Arc<crate::embedding::EmbeddingPipeline>,
) -> Result<()> {
    // Two workers: connections are few (one per MCP client) and their work is
    // dominated by a mutex-guarded SQLite handle, so more threads would only
    // queue on the same lock.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to build the daemon's async runtime")?;

    runtime.block_on(async move {
        let mut listener =
            listener.into_async().context("failed to register the daemon endpoint with the async runtime")?;

        loop {
            let stream = listener.accept().await.context("failed to accept a daemon connection")?;
            let conn = Arc::clone(&conn);
            let registry = Arc::clone(&registry);
            let indexing = indexing.clone();
            let embedding = Arc::clone(&embedding);
            // Taken here rather than inside the task, so the count rises
            // before this loop can come back round and consider the core
            // unattended.
            let attached = core_activity.connection_opened();
            let core_activity = Arc::clone(&core_activity);
            tokio::spawn(async move {
                // Dropped when this session ends, whichever way it ends, which
                // is also what restarts the core's idle clock.
                let _attached = attached;
                if let Err(err) =
                    mcp::serve_connection(stream, conn, registry, core_activity, indexing, embedding).await
                {
                    eprintln!("g-mesh daemon: connection ended: {err:#}");
                }
            });
        }
    })
}

/// Converts an absolute, canonicalized path (as `ProjectWatcher` reports
/// them) into the project-relative, forward-slash path string the wire
/// protocol and storage layer use - the same convention the plugin's own
/// `toPosixPath` follows in bulkIndex.ts. `None` for a path outside `root`,
/// which is not this function's job to treat as an error.
fn relative_wire_path(root: &Path, absolute: &Path) -> Option<String> {
    let rel = absolute.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        parts.push(component.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// How long [`acquire_singleton_lock`] keeps retrying a contended lock before
/// concluding a live incumbent holds it.
///
/// Bounded well below the shim's shortest observed bootstrap budget (1s in
/// `serving_while_indexing.rs`'s tests, 10s by default) so a genuinely
/// running second daemon still gives up promptly - this only papers over a
/// release that is already in flight, not a real incumbent.
const SINGLETON_LOCK_RETRY_BUDGET: Duration = Duration::from_millis(300);

/// How long each retry waits before trying the lock again. Short next to
/// [`SINGLETON_LOCK_RETRY_BUDGET`] so the kernel's async release of a just-
/// killed predecessor's `flock` (see below) is caught within a handful of
/// attempts rather than costing most of the budget on one long sleep.
const SINGLETON_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Takes the project's daemon lock, or reports that someone else holds it.
///
/// The returned `File` must stay alive for as long as the daemon runs: the
/// lock is advisory and tied to the open file, so dropping it (or exiting,
/// or being killed) releases it - which is exactly what lets the next daemon
/// take over from a crashed one without any stale-lock cleanup.
///
/// # Why a contended lock is retried instead of failing immediately
///
/// A `kill -9`'d predecessor's `flock` is released by the kernel as part of
/// its teardown, not the instant the signal lands or even the instant the
/// process stops being visible to `kill(pid, 0)` (`is_process_alive`) - the
/// fd table is torn down slightly later. A replacement daemon bootstrapped
/// immediately after such a kill (exactly what a test harness restarting a
/// daemon does, and what a crash-and-respawn does in the wild) can therefore
/// find the lock still held by a process that is, for every purpose other
/// than this exact race, already gone. Retrying briefly tells that case apart
/// from an actual incumbent without changing the outcome for one: a real
/// second daemon still holds the lock past `SINGLETON_LOCK_RETRY_BUDGET` and
/// still loses. A clean shutdown (`g-mesh stop`, SIGTERM + wait for exit)
/// never hits this at all - it does not leave the lock held after the process
/// it belonged to is confirmed gone.
fn acquire_singleton_lock(dir: &Path) -> Result<Option<File>> {
    let path = dir.join(DAEMON_LOCK_FILE);
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open daemon lock file {}", path.display()))?;

    let deadline = std::time::Instant::now() + SINGLETON_LOCK_RETRY_BUDGET;
    loop {
        match file.try_lock() {
            Ok(()) => {
                // Emptied the instant it is taken, so the file says "held by a
                // daemon that is not serving yet" for exactly as long as that
                // is true - see `record_serving_owner` for the other half.
                clear_serving_owner(&file, &path);
                return Ok(Some(file));
            }
            Err(TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(SINGLETON_LOCK_RETRY_INTERVAL);
            }
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

/// What holds a project's daemon lock right now, judged from outside the
/// process that holds it.
///
/// The lock is the only thing that decides who may serve a project
/// ([`acquire_singleton_lock`]), so every other process's picture of "is this
/// project served" has to be answerable from it - which, before task 184, it
/// was not: a held lock read as "a daemon serves this project" and nothing
/// else, so a holder that had stopped serving wedged the project for as long
/// as it stayed alive. These four states are what "held" is now allowed to
/// mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonLock {
    /// Nobody holds it: the project has no daemon and a bootstrap may go ahead.
    Free,
    /// Held by a daemon that is answering on the project's socket - the
    /// healthy incumbent, and the only state in which another daemon must
    /// stand down.
    Serving,
    /// Held, but the holder has not published itself as serving yet: a daemon
    /// between taking the lock and binding its socket (or one from a build
    /// before that was recorded). Not a wedge - it is entitled to a moment -
    /// so nothing may evict it on the strength of this alone.
    Starting,
    /// Held by a live process that *did* publish itself as serving and is not
    /// answering any more: the state task 184 is about. The socket it bound is
    /// gone (or it stopped accepting on it), so no client can ever reach it
    /// again, and it will keep the project to itself until it exits.
    Wedged { pid: u32 },
}

/// Diagnoses [`DaemonLock`] for `root`.
///
/// Deliberately asks the socket first: an incumbent that answers is healthy
/// whatever the lock file says, and that is also the cheapest of the three
/// checks. Only once nothing answers is the lock itself probed - by taking it
/// and immediately dropping it, which is the one way to tell "held" from
/// "free" without the holder's cooperation. That momentary acquisition is
/// invisible to a daemon racing for the same lock: it retries for
/// [`SINGLETON_LOCK_RETRY_BUDGET`], three orders of magnitude longer than this
/// holds anything.
pub fn inspect_daemon_lock(root: &Path) -> Result<DaemonLock> {
    let listening = is_listening(root)?;
    inspect_daemon_lock_in(&project_dir(root)?, listening)
}

/// [`inspect_daemon_lock`] against an already-resolved state directory and an
/// already-answered "is anything listening", so the judgement can be tested
/// over its whole table without a real socket or a real `~/.g-mesh`.
fn inspect_daemon_lock_in(state_dir: &Path, listening: bool) -> Result<DaemonLock> {
    if listening {
        return Ok(DaemonLock::Serving);
    }
    if !daemon_lock_is_held(state_dir)? {
        return Ok(DaemonLock::Free);
    }
    match serving_owner_in(state_dir).filter(|&pid| is_process_alive(pid)) {
        Some(pid) => Ok(DaemonLock::Wedged { pid }),
        None => Ok(DaemonLock::Starting),
    }
}

/// Whether anything holds the project's daemon lock.
///
/// A lock file that does not exist has never been taken; failing to *open* one
/// that does is reported as "not held" for the same reason every other
/// external observation here degrades that way - the answer this feeds
/// (evicting a wedged daemon) must fail towards leaving processes alone.
fn daemon_lock_is_held(state_dir: &Path) -> Result<bool> {
    let path = daemon_lock_path_in(state_dir);
    let Ok(file) = File::options().write(true).truncate(false).open(&path) else {
        return Ok(false);
    };
    match file.try_lock() {
        // Held by nobody: taken here for an instant and released by this drop.
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("failed to probe {}", path.display()))
        }
    }
}

/// Records that the lock's holder is now serving, by writing its pid into the
/// lock file.
///
/// Called after the socket is bound, never before, and that ordering is the
/// whole point: the pid in this file does not mean "a daemon exists" (the lock
/// itself already means that) but "a daemon got as far as serving". A holder
/// with no pid recorded is starting up and must be left alone; a holder whose
/// recorded pid is alive while nothing answers has stopped serving and can be
/// evicted. Two different questions, and a pid file that could only answer the
/// first is why `cli::stop` could not see the wedged daemon at all.
///
/// Written into the lock file rather than alongside it so the two can never
/// disagree: whoever reads it is reading the very file whose lock they just
/// found held, and only the holder can have written it.
///
/// Terminated by a newline, and [`serving_owner_in`] refuses a record without
/// one. Nothing else in the daemon's pid files bothers, and this one has to:
/// its reader may go on to *signal* the pid it reads, and a reader that caught
/// the file mid-rewrite and parsed half a pid would signal an unrelated
/// process. The newline makes any state other than "fully written" - empty,
/// truncated, half-written - read as nothing recorded, which is the state that
/// gets left alone.
///
/// Best-effort, like every other pid-file write in the daemon: the cost of
/// failing is a wedge that reads as `Starting` and is left alone, which is the
/// behaviour this project had before the record existed.
fn record_serving_owner(lock: &File) {
    if let Err(err) = write_lock_body(lock, format!("{}\n", std::process::id()).as_bytes()) {
        eprintln!("g-mesh daemon: failed to record itself as the lock's owner: {err}");
    }
}

/// Empties the lock file, so a fresh holder does not inherit its predecessor's
/// claim to be serving.
fn clear_serving_owner(lock: &File, path: &Path) {
    if let Err(err) = write_lock_body(lock, b"") {
        eprintln!("g-mesh daemon: failed to clear {}: {err}", path.display());
    }
}

fn write_lock_body(lock: &File, body: &[u8]) -> std::io::Result<()> {
    let mut handle = lock;
    handle.set_len(0)?;
    handle.seek(SeekFrom::Start(0))?;
    handle.write_all(body)?;
    handle.flush()
}

/// The pid the lock's holder recorded once it began serving, if any.
///
/// `None` for an empty file (a holder that is still starting up), a missing
/// one, an unparseable one, or one with no terminating newline - all of which
/// mean "nothing recorded", the same reading [`read_pid_file`] gives the
/// daemon's other pid files, with the newline requirement
/// [`record_serving_owner`] explains added on top.
pub fn serving_owner_in(state_dir: &Path) -> Option<u32> {
    let recorded = fs::read_to_string(daemon_lock_path_in(state_dir)).ok()?;
    recorded.strip_suffix('\n')?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forces the exact shape of task 165's race deterministically, instead
    /// of relying on a real `kill -9`'s unpredictable teardown delay the way
    /// the integration tests that first surfaced it incidentally do.
    ///
    /// `flock` is scoped to the *open file description*, not the process
    /// that holds it, so two independent `open()` calls from the very same
    /// process contend exactly as two different processes would - which is
    /// what lets a second thread stand in for a just-killed predecessor: it
    /// takes the lock, holds it for a short, fixed window, then releases it,
    /// the same shape as the kernel finishing a killed daemon's fd teardown
    /// slightly after the replacement's first attempt. Before the retry loop
    /// this exercises, that first attempt alone would see `WouldBlock` and
    /// `acquire_singleton_lock` would give up immediately - exactly bug 165.
    #[test]
    fn a_lock_released_shortly_after_the_first_attempt_is_still_acquired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_LOCK_FILE);

        let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
        holder.lock().expect("failed to take the stand-in lock");

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let hold_for = SINGLETON_LOCK_RETRY_INTERVAL * 2;
        let releaser = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            thread::sleep(hold_for);
            drop(holder);
        });

        // Waits for the holder thread to have the lock before racing it, so
        // this cannot flake the other way - `acquire_singleton_lock` winning
        // a lock nobody was contending yet.
        ready_rx.recv().unwrap();
        let acquired =
            acquire_singleton_lock(dir.path()).expect("acquire_singleton_lock must not error on contention");
        releaser.join().unwrap();

        assert!(
            acquired.is_some(),
            "a lock released well within the retry budget must still be acquired, not treated as a live incumbent"
        );
    }

    /// The other half of the acceptance bar, without which the fix above
    /// could regress into "the singleton lock no longer excludes anyone":
    /// a lock held for the whole retry budget - a genuinely running
    /// incumbent, not a predecessor mid-teardown - must still lose.
    #[test]
    fn a_lock_held_past_the_retry_budget_is_not_acquired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_LOCK_FILE);

        let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
        holder.lock().expect("failed to take the stand-in lock");

        let acquired =
            acquire_singleton_lock(dir.path()).expect("acquire_singleton_lock must not error on contention");

        assert!(
            acquired.is_none(),
            "a lock held by a live incumbent for the whole retry budget must not be acquired"
        );
        drop(holder);
    }

    /// Takes the daemon lock the way a real daemon does, and hands back both
    /// the file (dropping it releases the lock) and the directory it lives in.
    /// `flock` is scoped to the open file description rather than to the
    /// process, so a lock taken here contends with `acquire_singleton_lock`
    /// exactly as another process's would - the same property task 165's
    /// tests above already rely on.
    fn held_lock(dir: &Path) -> File {
        acquire_singleton_lock(dir)
            .expect("taking the lock must not error")
            .expect("nothing else holds this fixture's lock")
    }

    /// A project nobody is serving and nobody has locked: a bootstrap may go
    /// ahead, and nothing is there to evict.
    #[test]
    fn an_unheld_lock_reads_as_free() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Free);

        // Including once a daemon has held it and let it go - the pid it
        // recorded while serving must not outlive the lock itself, or a
        // recycled pid would read as a wedged incumbent.
        let lock = held_lock(dir.path());
        record_serving_owner(&lock);
        drop(lock);
        assert_eq!(
            inspect_daemon_lock_in(dir.path(), false).unwrap(),
            DaemonLock::Free,
            "a released lock is free however recently its holder was serving"
        );
    }

    /// A daemon between taking the lock and binding its socket. Indistinguishable
    /// from a wedged one by the lock alone, which is exactly why the daemon
    /// records itself as serving separately - and why this must not be
    /// evictable.
    #[test]
    fn a_lock_held_by_a_daemon_that_has_not_begun_serving_reads_as_starting() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = held_lock(dir.path());

        assert_eq!(
            inspect_daemon_lock_in(dir.path(), false).unwrap(),
            DaemonLock::Starting,
            "a holder that never published itself as serving is starting up, not wedged"
        );
    }

    /// Task 184's state, forced deterministically: the lock is held, its
    /// holder published itself as serving and is alive, and nothing is
    /// listening. That combination - and only that one - is a wedge.
    #[test]
    fn a_live_holder_that_served_and_stopped_listening_reads_as_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let lock = held_lock(dir.path());
        record_serving_owner(&lock);

        assert_eq!(
            inspect_daemon_lock_in(dir.path(), false).unwrap(),
            DaemonLock::Wedged { pid: std::process::id() },
            "a live holder that was serving and answers nothing is the wedge, and it must be \
             named by pid so `stop` and the shim can act on it"
        );
    }

    /// The guard that keeps the eviction path from ever taking a session away
    /// from someone: a holder that answers is healthy no matter what else is
    /// true of it.
    #[test]
    fn a_holder_that_is_still_answering_is_never_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let lock = held_lock(dir.path());
        record_serving_owner(&lock);

        assert_eq!(
            inspect_daemon_lock_in(dir.path(), true).unwrap(),
            DaemonLock::Serving,
            "a daemon answering on its socket must never be a candidate for eviction"
        );
    }

    /// The record has to survive one daemon replacing another: a fresh holder
    /// starts out as `Starting`, not as the wedged incumbent it took over
    /// from.
    #[test]
    fn a_new_holder_does_not_inherit_its_predecessors_claim_to_be_serving() {
        let dir = tempfile::tempdir().unwrap();
        let first = held_lock(dir.path());
        record_serving_owner(&first);
        assert_eq!(serving_owner_in(dir.path()), Some(std::process::id()));
        drop(first);

        let _second = held_lock(dir.path());
        assert_eq!(serving_owner_in(dir.path()), None, "taking the lock clears the record");
        assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Starting);
    }

    /// A lock file from a build that predates the record reads as `Starting` -
    /// nothing is claimed about it, so nothing may be done to it. The safe
    /// direction: an old daemon is left alone rather than signalled on a guess.
    #[test]
    fn a_lock_file_with_no_record_in_it_is_never_treated_as_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_LOCK_FILE);
        let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
        holder.lock().unwrap();

        assert_eq!(serving_owner_in(dir.path()), None);
        assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Starting);
    }

    /// The record is what decides whether a live process gets signalled, so a
    /// half-written one must never parse. Asserted over the shapes a reader
    /// could catch mid-rewrite - and over a stray one, since a lock file is
    /// world-readable and this is the one pid the daemon acts on rather than
    /// merely reports.
    #[test]
    fn a_record_without_its_terminating_newline_is_not_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DAEMON_LOCK_FILE);

        for partial in ["", "1", "12", "1234"] {
            fs::write(&path, partial).unwrap();
            assert_eq!(
                serving_owner_in(dir.path()),
                None,
                "{partial:?} is not a finished record and must not name anything to signal"
            );
        }

        fs::write(&path, "1234\n").unwrap();
        assert_eq!(serving_owner_in(dir.path()), Some(1234));
    }

    /// A registry over `languages`, each installed as a fake plugin
    /// (`test_plugin`) claiming one extension named after it (`python` ->
    /// `.python-src`), plus the fixture directories backing it - dropping
    /// them would delete the plugin/project directories before a test using
    /// the registry is done with them. Mirrors `daemon::registry`'s own
    /// `registry_over` test fixture, which is private to that module and so
    /// not reusable from here directly.
    fn registry_over(
        languages: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");

        let dirs = languages
            .iter()
            .map(|language| {
                let extension = format!(".{language}-src");
                test_plugin::install(plugins.path(), language, &[extension.as_str()])
            })
            .collect();

        let discovered =
            manifest::discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
        let root = project.path().canonicalize().expect("failed to canonicalize the fixture project root");
        let state_dir =
            project_dir(project.path()).expect("failed to resolve the fixture project's state directory");
        fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            &root,
            state_dir,
            discovered,
            None,
            Arc::new(crate::embedding::EmbeddingPipeline::disabled()),
        );
        (project, plugins, dirs, registry)
    }

    /// Runs `watch_and_route_once` a handful of times up front, the same way
    /// `watcher::mod`'s own tests drain a fresh watcher before asserting on
    /// it - macOS FSEvents can replay startup noise (a creation event for
    /// the watched root itself) shortly after the watch begins, and letting
    /// that settle first keeps it from being mistaken for the burst under
    /// test. `relative_wire_path` already filters root-directory events out
    /// before they would ever reach `registry`, so this exists for timing
    /// hygiene rather than because noise could itself register as a round
    /// trip.
    fn drain_startup_noise(
        watcher: &ProjectWatcher,
        debouncer: &mut Debouncer,
        root: &Path,
        conn: &Mutex<Connection>,
        registry: &PluginRegistry,
    ) {
        for _ in 0..2 {
            watch_and_route_once(watcher, debouncer, root, conn, registry);
        }
    }

    /// Pumps `watch_and_route_once` for a bounded window comfortably longer
    /// than [`DEBOUNCE_WINDOW`] - long enough to both drain whatever raw
    /// events a preceding burst queued and let their debounce window elapse
    /// and fire, without looping forever the way `run`'s real watcher thread
    /// does.
    fn pump_until_settled(
        watcher: &ProjectWatcher,
        debouncer: &mut Debouncer,
        root: &Path,
        conn: &Mutex<Connection>,
        registry: &PluginRegistry,
    ) {
        let deadline = std::time::Instant::now() + DEBOUNCE_WINDOW * 3;
        while std::time::Instant::now() < deadline {
            watch_and_route_once(watcher, debouncer, root, conn, registry);
        }
    }

    /// The concrete acceptance bar for task 129: a burst of rapid saves to
    /// the *same* file must cost the plugin one round trip, not one per
    /// save. Drives `watch_and_route_once` - `run`'s watcher thread's own
    /// per-iteration logic - directly, against a real `ProjectWatcher` over
    /// a real tempdir and a real `PluginRegistry` over a fake plugin that
    /// logs every `fileChanged` round trip it actually answers
    /// (`test_plugin::file_changed_requests`), rather than spawning a
    /// background thread: that lets the burst and the assertion be
    /// sequenced deterministically instead of racing a loop nothing here
    /// could join.
    #[test]
    fn a_burst_of_rapid_saves_to_the_same_file_costs_one_plugin_round_trip_not_one_per_save() {
        let (project, _plugins, dirs, registry) = registry_over(&["python"]);
        let plugin_dir = &dirs[0];
        let conn = test_plugin::empty_index();
        let root = project.path().canonicalize().unwrap();

        let watcher = ProjectWatcher::new(&root).unwrap();
        let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
        drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

        let file = root.join("a.python-src");
        for i in 0..5 {
            fs::write(&file, format!("v{i}")).unwrap();
            thread::sleep(Duration::from_millis(20));
            watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
        }

        // Every write so far was well under DEBOUNCE_WINDOW apart, so nothing
        // must have settled and fired yet - proof this is actually debounced
        // rather than merely fast.
        assert!(
            test_plugin::file_changed_requests(plugin_dir).is_empty(),
            "a burst still in progress must not have reached the plugin yet"
        );

        pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

        assert_eq!(
            test_plugin::file_changed_requests(plugin_dir).len(),
            1,
            "a burst of 5 rapid saves to the same file must cost exactly one plugin round trip, not one per save"
        );
    }

    /// The other half of the same claim: debouncing must only coalesce a
    /// genuine *burst*, never delay or drop an isolated, non-bursty edit.
    #[test]
    fn an_isolated_save_still_reaches_the_plugin_exactly_once() {
        let (project, _plugins, dirs, registry) = registry_over(&["python"]);
        let plugin_dir = &dirs[0];
        let conn = test_plugin::empty_index();
        let root = project.path().canonicalize().unwrap();

        let watcher = ProjectWatcher::new(&root).unwrap();
        let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
        drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

        fs::write(root.join("a.python-src"), "v0").unwrap();
        pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

        assert_eq!(
            test_plugin::file_changed_requests(plugin_dir).len(),
            1,
            "a single isolated save must still reach the plugin, exactly once"
        );
    }

    /// Debouncing coalesces repeats of the *same* path; it must not
    /// accidentally coalesce two genuinely different files landing in the
    /// same burst into one round trip - each still needs its own reparse.
    #[test]
    fn two_different_files_in_the_same_burst_each_get_their_own_round_trip() {
        let (project, _plugins, dirs, registry) = registry_over(&["python"]);
        let plugin_dir = &dirs[0];
        let conn = test_plugin::empty_index();
        let root = project.path().canonicalize().unwrap();

        let watcher = ProjectWatcher::new(&root).unwrap();
        let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
        drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

        fs::write(root.join("a.python-src"), "a").unwrap();
        thread::sleep(Duration::from_millis(20));
        watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
        fs::write(root.join("b.python-src"), "b").unwrap();
        pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

        let mut requested = test_plugin::file_changed_requests(plugin_dir);
        requested.sort();
        assert_eq!(
            requested,
            vec!["a.python-src".to_string(), "b.python-src".to_string()],
            "two distinct files in the same burst must each still get their own round trip"
        );
    }
}
