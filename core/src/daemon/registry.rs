//! One [`PluginSupervisor`] per language, spawned the first time a file of
//! that language is actually touched.
//!
//! This is the piece that turns "the daemon has a plugin" into "the daemon
//! has plugins": it owns discovery's results (see `daemon::manifest`) and
//! answers the only two questions the rest of the daemon has about them -
//! *which* plugin claims this file, and *is it running yet* - while
//! `PluginSupervisor` and `PluginProcess` keep doing exactly what they
//! already did, once per language instead of once per daemon. See
//! `docs/architecture/plugin-modularity.md`'s Components and Data Flow
//! sections; this module implements the `PluginRegistry` described there.
//!
//! # Lazy, not eager
//!
//! Nothing is spawned by [`PluginRegistry::new`]. A plugin costs a real
//! process (for the bundled JS/TS one, a Node runtime with the TypeScript
//! compiler in it), and a project with no `.py` file in it must not pay for a
//! Python plugin that would have nothing to say - the resource-footprint
//! constraint the architecture doc rejects eager spawning on. The first file
//! of a language is what brings its plugin up, and from that moment the
//! supervisor's existing sleep/wake and crash-recovery machinery takes over
//! unchanged.
//!
//! # Independence between languages
//!
//! Each language's supervisor owns its own process, its own dirty-file queue
//! and its own idle timer, so a plugin that crashes, hangs or sleeps affects
//! exactly the files its own manifest claims. That is not a feature this
//! module adds - it falls out of there being N supervisors instead of one -
//! but it is a guarantee worth stating, because the single-plugin daemon this
//! replaces could not offer it: there, a broken plugin was a broken daemon.
//!
//! # Lock order, and why nothing waits behind a spawn
//!
//! `supervisors` is the *outermost* lock in the daemon. It is released before
//! any caller touches the supervisor it got back, so the existing order
//! `daemon::lifecycle` documents (supervisor lock first, SQLite connection
//! inside it) continues below this one and nothing ever reaches back up:
//! a supervisor knows nothing about the registry that owns it.
//!
//! Since task 164 it is also only ever held for a map lookup or a single
//! insert/remove - never across a spawn.
//! [`get_or_spawn`](PluginRegistry::get_or_spawn) used to hold it for the
//! whole process launch plus handshake, deliberately (task 154), because a
//! get-or-insert under one lock is the simplest thing that cannot
//! double-spawn a language. What that
//! overlooked is that `std::sync::Mutex` has no reader/writer distinction, so
//! *every* other caller of this map paid for it - including
//! [`active_supervisors`](PluginRegistry::active_supervisors), which only
//! wants to clone it, and through it [`has_pending`](PluginRegistry::has_pending),
//! which every MCP tool call asks before it answers. One language's spawn
//! stalled queries about every other language, and queries that needed no
//! plugin at all (measured: 270-494ms, see `core/tests/cold_start_grace_wait.rs`).
//!
//! [`SupervisorSlot`] is what replaces it: a language being spawned right now
//! has a `Spawning` marker in the map from before the lock is released until
//! after the spawn ends, so the get-or-insert that rules out a double spawn is
//! still one short critical section, and the spawn itself happens with no lock
//! held at all. Readers skip a `Spawning` slot (there is no process to sleep,
//! no queue to replay and no idle timer to check behind it yet); a second
//! caller wanting the *same* language waits on the marker itself rather than
//! on the map. See [`SpawnInProgress`] and [`SpawnReservation`].
//!
//! An `RwLock` alone would not have fixed this and is not what this uses: the
//! problem was never that readers exclude each other, it was that one writer
//! held the map for hundreds of milliseconds. A write guard taken across the
//! same spawn would block readers for exactly as long. Once the spawn happens
//! outside the lock there is nothing left for a read/write split to buy - every
//! remaining critical section is a hash lookup.
//!
//! # Wired into `daemon::run` (task 155)
//!
//! `daemon::run` holds one `Arc<PluginRegistry>` where it used to hold one
//! `Arc<PluginSupervisor>`: `discover(default_roots())`'s result becomes this
//! registry at startup (a `discover()` failure is a hard daemon-startup
//! failure, same as a bad manifest always was), the watcher loop calls
//! [`file_changed`](PluginRegistry::file_changed) instead of reaching a
//! single supervisor directly, `daemon::lifecycle::supervise` drives every
//! active supervisor's idle timer through
//! [`sleep_if_idle_all`](PluginRegistry::sleep_if_idle_all) /
//! [`sleep_all_now`](PluginRegistry::sleep_all_now), and the MCP layer wakes
//! and queries plugins through
//! [`replay_pending`](PluginRegistry::replay_pending) /
//! [`ensure_fresh`](PluginRegistry::ensure_fresh) instead of a bare
//! `Arc<PluginSupervisor>` field. `PluginProcess::relaunch` was the one
//! remaining spot that wrote the legacy single `plugin.pid` regardless of
//! which language actually crashed - it now rewrites its own supervisor's
//! pid file (see [`pid_file_for`](PluginRegistry::pid_file_for)), so two
//! languages can never step on each other's record, on a crash relaunch any
//! more than on an ordinary sleep. `cli::status`/`cli::stop`/`cli::clean`
//! read every `plugin-<language>.pid` file present
//! ([`discovered_pid_files`]) rather than assuming exactly one.
//!
//! # The index's generation string (task 163)
//!
//! [`indexer_version`] lives here too, beside the type that owns discovery's
//! results, though it is a free function over [`DiscoveredPlugins`] rather
//! than a method - see its own doc comment for both halves of that (what it
//! hashes, and why no caller has a registry to ask it). It is why `daemon::run`
//! now discovers plugins *before* it opens and validates the index: with N
//! plugins, "is what is in this index still what today's pipeline would
//! produce?" cannot be answered by looking at one of them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::plugin;
use crate::embedding::EmbeddingPipeline;
use crate::storage::schema::CURRENT_INDEXER_VERSION;
use crate::watcher::staleness::{self, StalenessOutcome};

/// Where a language's plugin pid is recorded, relative to the project's state
/// directory: one file per language, unlike the single legacy `plugin.pid`
/// (see `daemon::plugin_pid_path_in`'s doc comment for what still uses that
/// name). `pub(crate)` rather than private so `daemon::mod`'s own
/// bundled-plugin pid-path alias can build the same filename instead of
/// duplicating the `"plugin-<language>.pid"` convention as a second literal.
pub(crate) fn plugin_pid_file_name(language: &str) -> String {
    format!("plugin-{language}.pid")
}

/// Every `plugin-<language>.pid` file currently present in `state_dir`,
/// paired with the language its name encodes - [`PluginRegistry::pid_file_for`]'s
/// naming convention, reversed.
///
/// For tooling that has no running daemon (or `PluginRegistry`) to ask:
/// `cli::status`, `cli::stop` and `cli::clean` all run *after* the fact,
/// against a project whose daemon may or may not still be alive, so "list
/// what is on disk" is the only source of truth available to them - unlike
/// `daemon::lifecycle::supervise`, which asks a live registry directly for
/// exactly this reason wherever one exists (see
/// [`PluginRegistry::active_supervisors`]).
///
/// Empty - not an error - for a state directory that does not exist or
/// cannot be listed, matching every other pid-file helper in this daemon's
/// "unreadable means nothing recorded" convention.
pub fn discovered_pid_files(state_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(state_dir) else { return files };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(language) = name.strip_prefix("plugin-").and_then(|n| n.strip_suffix(".pid")) {
            files.push((language.to_string(), entry.path()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// The generation string an index is stamped with, and the thing
/// `storage::schema::ensure_current` compares: core's hand-maintained pipeline
/// generation ([`CURRENT_INDEXER_VERSION`]) and the plugin *builds* that
/// filled the index, joined.
///
/// Both halves have to be in it. The constant alone misses every plugin-side
/// change (the failure task 116 fixes); the plugin half alone would miss every
/// change in `graph::imports` / `graph::symbol_links`, which run in core and
/// leave every plugin's bytes untouched.
///
/// # Why every discovered plugin, and not just the one that was spawned
///
/// An index is a single artifact filled by whichever plugins had something to
/// say about the project, so it is only as current as the *least* current of
/// the builds behind it: a Python plugin rebuilt with a different extractor
/// invalidates the same `index.db` a JS/TS rebuild does. Keying this off one
/// bundled plugin (which is what `daemon::plugin::indexer_version` did until
/// task 163) would reproduce task 116's exact failure for every language that
/// is not the bundled one - a current schema, a current constant, and a graph
/// nothing will ever refresh.
///
/// Discovered rather than *active*, for the same reason: which supervisors
/// happen to have been spawned is a property of what the daemon has been asked
/// so far, not of what filled the index, and reading it would make the answer
/// change under a running daemon.
///
/// # Why this spawns nothing
///
/// [`plugin::fingerprint`] only reads files under a manifest's
/// `manifest_dir`, so the whole answer comes off the filesystem. That is what
/// lets this run at daemon startup - before the index is trusted for anything,
/// and long before the first file of any language brings its plugin up - and
/// keeps `PluginRegistry`'s lazy-spawn promise intact (see this module's doc
/// comment).
///
/// # A free function, not a `PluginRegistry` method
///
/// The architecture doc sketches this as `PluginRegistry::indexer_version()`,
/// but no caller has a registry when it needs the answer: `daemon::run` has to
/// stamp/validate the index *before* it builds one (the registry needs the
/// canonicalized root, the project's config and the embedding pipeline; the
/// version check needs only discovery's output and has to run before anything
/// trusts the index at all), and `cli::init`/`cli::reindex` never build a
/// registry in the first place. Taking `&DiscoveredPlugins` directly removes
/// that ordering problem rather than working around it - the same shape, and
/// for the same reason, as [`discovered_pid_files`] above: a fact about
/// plugins that callers with no live registry still have to be able to ask
/// for.
pub fn indexer_version(discovered: &DiscoveredPlugins) -> String {
    format!("{CURRENT_INDEXER_VERSION}+{}", plugins_digest(discovered))
}

/// One digest over every discovered plugin's `(language, fingerprint)` pair.
///
/// Re-hashed rather than concatenated so the result stays one fixed-width
/// value however many plugins are installed - `meta.indexer_version` is
/// compared, printed and eyeballed, and a string that grows with the plugin
/// count would make all three worse for no gain.
///
/// Sorted by language first. `DiscoveredPlugins::manifests` is a `HashMap`, so
/// its iteration order varies between processes for reasons that have nothing
/// to do with what any plugin contains; hashing in that order would make two
/// daemons of the same install disagree about the index they share and wipe
/// each other's work. Same hazard [`plugin::fingerprint`]'s own file sort
/// exists for, one level up. The language is hashed alongside its fingerprint
/// (and both are length-delimited by a NUL) so that renaming a plugin, or two
/// languages swapping builds, cannot leave the concatenation unchanged.
fn plugins_digest(discovered: &DiscoveredPlugins) -> String {
    let mut fingerprinted: Vec<(&str, String)> = discovered
        .manifests
        .iter()
        .map(|(language, manifest)| (language.as_str(), plugin::fingerprint(manifest)))
        .collect();
    fingerprinted.sort_unstable_by_key(|(one, _)| *one);

    let mut hasher = Sha256::new();
    for (language, fingerprint) in &fingerprinted {
        hasher.update(language.as_bytes());
        hasher.update([0]);
        hasher.update(fingerprint.as_bytes());
        hasher.update([0]);
    }
    plugin::truncated_hex(hasher)
}

/// [`indexer_version`] for a discovery that found nothing - the generation
/// string a *test* stamps a fixture index with when it needs `meta` to exist
/// and nothing will ever compare that stamp against a live daemon's.
///
/// Shared (`gc::last_used`, `gc::warning`, `cli::clean`, `cli::status`) rather
/// than repeated as a literal per test module, so those fixtures keep tracking
/// the real shape of what `schema::ensure_current` stores instead of drifting
/// into four hand-written strings. Deliberately over an empty
/// [`DiscoveredPlugins`]: a real discovery would make every one of those
/// fixtures walk and hash the installed plugins' whole build for a value none
/// of them reads back.
#[cfg(test)]
pub(crate) fn fixture_indexer_version() -> String {
    indexer_version(&DiscoveredPlugins::default())
}

/// What the `supervisors` map holds for one language: a plugin that is up, or
/// the reservation left behind by whoever is bringing it up right now.
///
/// The second arm is the whole of task 164's fix (see this module's doc
/// comment). A spawn is hundreds of milliseconds of process launch and
/// handshake; a map entry saying "someone is already doing that" costs one
/// pointer and lets the map lock be released for every bit of it.
#[derive(Clone)]
enum SupervisorSlot {
    /// A spawn is in flight. Cloned out from under the map lock by whoever
    /// needs to wait for it - never waited on while that lock is held.
    Spawning(Arc<SpawnInProgress>),
    /// The plugin is up, and this is the supervisor every caller for this
    /// language gets from now on.
    Running(Arc<PluginSupervisor>),
}

impl SupervisorSlot {
    /// The supervisor behind this slot, or `None` while its spawn is still in
    /// flight.
    ///
    /// `None` rather than a wait, because every caller of
    /// [`PluginRegistry::active_supervisors`] is asking about a *live* plugin:
    /// a language still spawning has no process to put to sleep, no queue to
    /// replay and no idle timer that could have expired, so there is nothing
    /// for those callers to do about it even once it arrives. Waiting would
    /// buy them exactly the stall this design exists to remove.
    fn running(&self) -> Option<Arc<PluginSupervisor>> {
        match self {
            Self::Running(supervisor) => Some(Arc::clone(supervisor)),
            Self::Spawning(_) => None,
        }
    }
}

/// The rendezvous behind a [`SupervisorSlot::Spawning`] entry: how a second
/// caller for the same language finds out how the first one's spawn ended.
///
/// The outcome is carried here rather than left for waiters to re-read out of
/// the map because a *failed* spawn leaves no map entry at all (a failure
/// memoizes nothing - see [`PluginRegistry::get_or_spawn`]), so "look again"
/// would say "absent" and send every waiter off to repeat a spawn that has
/// just been shown not to work. Answering them with the failure the spawn
/// actually hit is both cheaper and more honest.
struct SpawnInProgress {
    /// `None` until the spawning thread settles it.
    ///
    /// The failure side is a rendered string, not an `anyhow::Error`: one
    /// failure has to be handed to arbitrarily many waiters and
    /// `anyhow::Error` is not `Clone`. `{err:#}` keeps the whole context
    /// chain, which is all any caller here does with it anyway (every one of
    /// them logs it).
    outcome: Mutex<Option<Result<Arc<PluginSupervisor>, String>>>,
    settled: Condvar,
}

impl SpawnInProgress {
    fn new() -> Arc<Self> {
        Arc::new(Self { outcome: Mutex::new(None), settled: Condvar::new() })
    }

    /// Blocks until the spawn this marker stands for has ended, and answers
    /// with whatever it produced.
    ///
    /// Called with no other lock held - in particular not the map's, which is
    /// the entire point - so a language spawning slowly holds up only the
    /// callers that asked for that same language.
    fn wait(&self) -> Result<Arc<PluginSupervisor>> {
        let mut outcome = self.outcome.lock().unwrap();
        while outcome.is_none() {
            outcome = self.settled.wait(outcome).unwrap();
        }
        match outcome.as_ref().expect("the loop above only exits once it is settled") {
            Ok(supervisor) => Ok(Arc::clone(supervisor)),
            Err(message) => Err(anyhow!("{message}")),
        }
    }

    /// Publishes how the spawn ended and releases everyone waiting on it.
    fn settle(&self, outcome: Result<Arc<PluginSupervisor>, String>) {
        *self.outcome.lock().unwrap() = Some(outcome);
        self.settled.notify_all();
    }
}

/// The [`SupervisorSlot::Spawning`] entry one caller put in the map, and the
/// promise that it is replaced (on success) or removed (on failure) however
/// the spawn ends.
///
/// A guard rather than a plain pair of statements because "however it ends"
/// includes a panic. A reservation left behind by a thread that unwound would
/// be a language nothing can ever spawn again and a marker every later caller
/// waits on forever - a hang, where the old code's equivalent (a panic while
/// holding the map lock) was at least a loud, poisoned-mutex failure. [`Drop`]
/// below turns it back into a loud one.
struct SpawnReservation<'registry> {
    registry: &'registry PluginRegistry,
    language: String,
    marker: Arc<SpawnInProgress>,
    settled: bool,
}

impl SpawnReservation<'_> {
    /// Hands `spawned` back to the caller after making it the map's answer for
    /// this language and releasing anyone who waited on it.
    ///
    /// The map is updated *before* the marker is settled, so a waiter released
    /// by it can never observe a supervisor that
    /// [`PluginRegistry::active_supervisors`] would still be blind to.
    fn settle(&mut self, spawned: Result<Arc<PluginSupervisor>>) -> Result<Arc<PluginSupervisor>> {
        let published = {
            let mut supervisors = self.registry.supervisors.lock().unwrap();
            match &spawned {
                Ok(supervisor) => {
                    supervisors.insert(
                        self.language.clone(),
                        SupervisorSlot::Running(Arc::clone(supervisor)),
                    );
                    Ok(Arc::clone(supervisor))
                }
                // A spawn that failed memoizes nothing, so the reservation
                // goes too: the next file of this language tries again, which
                // is the right behaviour for a plugin whose runtime is missing
                // or briefly unavailable.
                Err(err) => {
                    supervisors.remove(&self.language);
                    Err(format!("{err:#}"))
                }
            }
        };
        self.marker.settle(published);
        self.settled = true;
        spawned
    }
}

impl Drop for SpawnReservation<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Only reachable while unwinding out of the spawn, so this deviates
        // from the `.lock().unwrap()` this module uses everywhere else: a
        // second panic here would abort the process outright, and the point of
        // this guard is to leave a *reportable* failure behind rather than a
        // permanently wedged language.
        if let Ok(mut supervisors) = self.registry.supervisors.lock() {
            supervisors.remove(&self.language);
        }
        self.marker.settle(Err(format!(
            "the thread spawning the {} plugin panicked before it finished",
            self.language
        )));
    }
}

/// The daemon's plugins: what was discovered, and which of them are running.
pub struct PluginRegistry {
    /// Canonicalized, exactly as [`PluginSupervisor`] wants it - every
    /// supervisor this registry creates is spawned against the same root.
    project_root: PathBuf,
    /// The project's state directory - `~/.g-mesh/projects/<hash>/` - taken
    /// as an explicit argument rather than recomputed from `project_root`
    /// via `storage::connection::project_dir`. Those two must not be
    /// conflated: `daemon::run` hashes its state directory from the *raw*
    /// root it was given (`dir`), matching the socket, the main pid file,
    /// and the index it already opened, but passes this registry the
    /// *canonicalized* root (`canonical_root`) for spawning plugins against,
    /// because that is what lets `relative_wire_path` turn a
    /// `ProjectWatcher`-reported path back into a project-relative one (see
    /// `daemon::run`'s own comment on `canonical_root`). On a filesystem
    /// where the two differ as strings - `/var` vs. `/private/var` on macOS,
    /// which is exactly what `tempfile::tempdir()` returns there - hashing
    /// the canonicalized root instead of reusing `dir` would put every
    /// `plugin-<language>.pid` file in a *different* directory than every
    /// other piece of this project's state, and `cli::status`/`cli::stop`
    /// (which read `dir`, not this type) would never find them.
    state_dir: PathBuf,
    /// Discovery's output, taken as a finished value rather than produced
    /// here: scanning roots and validating manifests is `daemon::manifest`'s
    /// job and happens once at startup, before this registry (or the schema
    /// staleness check that also consumes it) exists. Never re-read while the
    /// daemon runs - a plugin install takes effect on the next start, per the
    /// architecture doc.
    discovered: DiscoveredPlugins,
    /// Handed to every supervisor as it is created, so all languages honor
    /// the one `plugin.idleTimeoutMinutes` the project configured.
    idle_timeout: Option<Duration>,
    /// Shared with every supervisor and with the cold-start bulk walk - one
    /// pipeline, one lazily-loaded model, however many plugins ask it to
    /// embed something.
    embedding: Arc<EmbeddingPipeline>,
    /// language -> its supervisor, or the reservation standing in for one
    /// while it is being spawned ([`SupervisorSlot`]), filled in lazily by
    /// [`get_or_spawn`](Self::get_or_spawn). A language absent from this map
    /// is one whose files this daemon has not seen yet, not one that failed.
    ///
    /// Held for a lookup or a single insert/remove and nothing else - see this
    /// module's doc comment for why that is a hard rule and not a preference.
    supervisors: Mutex<HashMap<String, SupervisorSlot>>,
    /// Extensions already reported as unclaimed. A mixed-language repo where
    /// only some languages have plugins is an expected steady state, not an
    /// error, so it gets one line per extension for the whole daemon run
    /// rather than one per file - see [`unroutable_notice`](Self::unroutable_notice).
    unroutable: Mutex<HashSet<String>>,
}

impl PluginRegistry {
    /// Builds a registry over `discovered`. Spawns nothing - see this
    /// module's doc comment.
    pub fn new(
        project_root: &Path,
        state_dir: PathBuf,
        discovered: DiscoveredPlugins,
        idle_timeout: Option<Duration>,
        embedding: Arc<EmbeddingPipeline>,
    ) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            state_dir,
            discovered,
            idle_timeout,
            embedding,
            supervisors: Mutex::new(HashMap::new()),
            unroutable: Mutex::new(HashSet::new()),
        }
    }

    /// Which language claims `file_path`, by its extension; `None` if no
    /// discovered plugin does.
    ///
    /// The extension is lowercased before the lookup (`App.TSX` is a
    /// TypeScript file on a case-insensitive filesystem, and nothing upstream
    /// normalizes what the watcher reports), while the routing table's keys
    /// are already lowercase-with-leading-dot by the manifest convention
    /// `daemon::manifest` documents.
    pub fn language_for(&self, file_path: &str) -> Option<&str> {
        let extension = extension_of(file_path)?;
        self.discovered.routing.get(&extension).map(String::as_str)
    }

    /// Whether a plugin was discovered for `language` at all - the same
    /// question [`get_or_spawn`](Self::get_or_spawn) answers with an `Err`
    /// when it fails, exposed here so a caller that wants "nothing to do" to
    /// be a distinct outcome from "the plugin was there and the request to
    /// it failed" can tell the two apart before asking. `daemon::semantic`
    /// is the motivating caller: an install with no bundled JS/TS plugin
    /// owes the whole-project semantic pass nothing, on every daemon start,
    /// not a stderr line every time.
    pub fn has_manifest(&self, language: &str) -> bool {
        self.discovered.manifests.contains_key(language)
    }

    /// The supervisor for `language`, spawning its plugin if this is the
    /// first time anything has needed it.
    ///
    /// # Why the map is not locked across the spawn
    ///
    /// Spawning is not free - it is a process launch plus a handshake round
    /// trip, and for a plugin that starts a type checker it is the most
    /// expensive thing the daemon ever does. Task 154 held the map across all
    /// of it, on the argument that a get-or-insert under one lock is the only
    /// shape that cannot double-spawn a language: the naive alternative (look,
    /// unlock, spawn, relock, insert) has two callers racing on the same new
    /// language start two processes, one of which is then either killed -
    /// paying its whole startup cost for nothing, precisely when the daemon is
    /// busiest - or dropped without a `shutdown` and left running with nothing
    /// reading its pipes.
    ///
    /// That argument is still right about the alternative it considered, and
    /// wrong about the cost, which turned out not to be "a caller wanting
    /// language B waits behind a caller starting language A" at all: with one
    /// plain `Mutex` over the map, *every* reader of it waited too - including
    /// [`has_pending`](Self::has_pending), which every MCP tool call asks
    /// before it answers, about languages it has nothing to do with. See this
    /// module's doc comment (and task 164) for the measured effect.
    ///
    /// So the reservation, not the spawn, is what this holds the lock for:
    /// insert a [`SupervisorSlot::Spawning`] marker, release the map, spawn
    /// with nothing locked, then swap the marker for the finished supervisor
    /// under a second, equally short critical section
    /// ([`SpawnReservation::settle`]). A concurrent caller for the *same*
    /// language finds the marker and waits on that instead of on the map, so
    /// it still cannot start a second process - the double-spawn guarantee is
    /// unchanged - while a caller for any other language, and every reader,
    /// is held up for a hash lookup rather than a handshake.
    ///
    /// Nothing can deadlock behind it: `PluginSupervisor::start` takes no lock
    /// this daemon shares (the supervisor it builds is not reachable by anyone
    /// else until it is published), no supervisor ever calls back into the
    /// registry, and the wait above is entered only after the map lock has
    /// been dropped.
    ///
    /// A spawn that *fails* memoizes nothing: the error goes to the caller
    /// and the next file of that language tries again, which is the right
    /// behaviour for a plugin whose runtime is missing or briefly
    /// unavailable. Callers that were already waiting on that same spawn are
    /// given its failure rather than each repeating it, which is the one
    /// behavioural difference from the serialized version - and the honest
    /// one: they asked while it was in flight, so it is their answer too.
    pub fn get_or_spawn(&self, language: &str) -> Result<Arc<PluginSupervisor>> {
        let mut supervisors = self.supervisors.lock().unwrap();
        // Cloned out of the map so the decision below can act with the guard
        // dropped - both arms leave the map lock before doing anything that
        // takes longer than a hash lookup.
        match supervisors.get(language).cloned() {
            Some(SupervisorSlot::Running(running)) => return Ok(running),
            Some(SupervisorSlot::Spawning(marker)) => {
                drop(supervisors);
                return marker.wait().with_context(|| {
                    format!(
                        "the in-progress spawn of the {language} plugin this call waited on failed"
                    )
                });
            }
            None => {}
        }

        // Ahead of the reservation, so a language nothing was discovered for
        // never leaves a marker behind for a spawn that is not going to happen.
        let manifest = self.discovered.manifests.get(language).with_context(|| {
            format!(
                "no plugin was discovered for language \"{language}\" (discovered: {})",
                self.discovered_languages()
            )
        })?;

        let marker = SpawnInProgress::new();
        supervisors.insert(language.to_string(), SupervisorSlot::Spawning(Arc::clone(&marker)));
        drop(supervisors);

        let mut reservation = SpawnReservation {
            registry: self,
            language: language.to_string(),
            marker,
            settled: false,
        };
        let spawned = PluginSupervisor::start(
            &self.project_root,
            manifest.clone(),
            self.pid_file_for(language),
            self.idle_timeout,
            Arc::clone(&self.embedding),
        );
        reservation.settle(spawned)
    }

    /// The watcher thread's entry point, with routing in front of it: hands
    /// `file_path` to the supervisor for the language that claims it,
    /// spawning that plugin if this is the first file of its kind.
    ///
    /// Nothing here propagates. A file no plugin claims is skipped (see
    /// [`unroutable_notice`](Self::unroutable_notice)), and a plugin that
    /// cannot be started is reported - the same "failures are reported and
    /// dropped" contract `PluginSupervisor::file_changed` already has, for
    /// the same reason: one file the daemon cannot index must not take the
    /// watcher thread, or the other languages, down with it.
    pub fn file_changed(&self, conn: &Mutex<Connection>, file_path: String) {
        let Some(language) = self.language_for(&file_path).map(str::to_string) else {
            if let Some(notice) = self.unroutable_notice(&file_path) {
                eprintln!("{notice}");
            }
            return;
        };

        match self.get_or_spawn(&language) {
            Ok(supervisor) => supervisor.file_changed(conn, file_path),
            Err(err) => eprintln!(
                "g-mesh daemon: could not start the {language} plugin for {file_path}: {err:#} - \
                 the change was not indexed"
            ),
        }
    }

    /// The message to log for a file no plugin claims, or `None` if this
    /// extension has already been reported.
    ///
    /// Returning the line instead of printing it is what makes "logged once,
    /// not once per file" testable: the `Option` *is* the decision to print,
    /// so a test can assert the decision directly rather than trying to count
    /// lines on a process-wide stderr several tests share.
    ///
    /// Keyed by extension rather than by file, because that is the grain the
    /// message is about - a repo with three hundred `.md` files has one thing
    /// to say about them, and the next unclaimed extension is genuinely new
    /// information. Per registry rather than per process for the same reason
    /// the map above is: one registry is one daemon run, and a `static` would
    /// leak one test's state into the next.
    fn unroutable_notice(&self, file_path: &str) -> Option<String> {
        let extension = extension_of(file_path);
        if !self.unroutable.lock().unwrap().insert(extension.clone().unwrap_or_default()) {
            return None;
        }
        Some(match extension {
            Some(extension) => format!(
                "g-mesh daemon: no installed plugin claims \"{extension}\" files (first seen: \
                 {file_path}) - changes to them are not indexed; further \"{extension}\" files \
                 will not be reported again"
            ),
            None => format!(
                "g-mesh daemon: no installed plugin claims files without an extension (first \
                 seen: {file_path}) - changes to them are not indexed; further extensionless \
                 files will not be reported again"
            ),
        })
    }

    /// Where `language`'s plugin pid is recorded - one file per language, so
    /// two supervisors can never overwrite (or, on sleep, delete) each
    /// other's record. Joined against `self.state_dir` - the same directory
    /// `daemon::run` already resolved everything else in - not recomputed
    /// from `self.project_root`; see this type's doc comment on `state_dir`
    /// for why those must not be conflated.
    fn pid_file_for(&self, language: &str) -> PathBuf {
        self.state_dir.join(plugin_pid_file_name(language))
    }

    /// Every discovered language, sorted - for error messages only, where a
    /// stable order is worth the sort.
    fn discovered_languages(&self) -> String {
        let mut languages: Vec<&str> =
            self.discovered.manifests.keys().map(String::as_str).collect();
        languages.sort_unstable();
        if languages.is_empty() {
            return "none".to_string();
        }
        languages.join(", ")
    }

    /// A snapshot of every supervisor spawned so far - the languages this
    /// daemon has actually needed, not every language discovery found. A
    /// language nothing has touched yet has no process to sleep and no queue
    /// to replay, so [`daemon::lifecycle::supervise`](crate::daemon::lifecycle::supervise)
    /// and the MCP layer's wake/replay path both act on this set rather than
    /// on every discovered manifest.
    ///
    /// Never waits on anything: a language whose spawn is still in flight is
    /// skipped (see [`SupervisorSlot::running`]), and the map lock is held for
    /// the walk over what is already there and nothing else. That is what
    /// makes this - and therefore [`has_pending`](Self::has_pending), which
    /// every MCP tool call asks - safe to call while some other language, or
    /// this same one, is in the middle of a spawn.
    pub fn active_supervisors(&self) -> Vec<Arc<PluginSupervisor>> {
        self.supervisors.lock().unwrap().values().filter_map(SupervisorSlot::running).collect()
    }

    /// Puts every active, idle-enough supervisor to sleep - one call to
    /// [`PluginSupervisor::sleep_if_idle`] per spawned language, run
    /// independently: one language falling asleep (or not) never affects
    /// another's own timer. A language that was never spawned is not in
    /// [`active_supervisors`](Self::active_supervisors) at all, so this never
    /// spawns anything new.
    pub fn sleep_if_idle_all(&self) {
        for supervisor in self.active_supervisors() {
            supervisor.sleep_if_idle();
        }
    }

    /// Stops every active supervisor's plugin regardless of idleness - what
    /// the daemon does on its own way out, applied to every language that
    /// has ever been spawned rather than to one hardcoded supervisor.
    ///
    /// The one caller that waits for a spawn in flight rather than skipping
    /// it, and the reason [`active_supervisors`](Self::active_supervisors) is
    /// not enough here: this runs as the core exits, so a plugin that finishes
    /// spawning a moment after being skipped would be a process nothing ever
    /// deliberately ended, and a `plugin-<language>.pid` file written just
    /// after `daemon::lifecycle::release_state_files` cleared it. Waiting
    /// costs the shutdown path one handshake it was going to have to reap
    /// anyway, and costs no other caller anything - nobody is waiting on a
    /// daemon that is already on its way out.
    ///
    /// One pass, not a loop until the map is quiet: this waits for the spawns
    /// that were in flight when it was called, which is the guarantee it
    /// needs, rather than promising to outlast a thread that keeps starting
    /// new ones.
    pub fn sleep_all_now(&self, reason: &str) {
        let in_flight: Vec<Arc<SpawnInProgress>> = {
            let supervisors = self.supervisors.lock().unwrap();
            supervisors
                .values()
                .filter_map(|slot| match slot {
                    SupervisorSlot::Spawning(marker) => Some(Arc::clone(marker)),
                    SupervisorSlot::Running(_) => None,
                })
                .collect()
        };
        for marker in in_flight {
            // Its failure, if it failed, is already the spawning caller's to
            // report; all this needs is for it to be over.
            let _ = marker.wait();
        }

        for supervisor in self.active_supervisors() {
            supervisor.sleep_now(reason);
        }
    }

    /// Whether any active supervisor has something queued for its next wake -
    /// the registry's analog of [`PluginSupervisor::has_pending`], cheap
    /// enough to ask on every MCP tool call the same way that one already is.
    ///
    /// "Cheap" is a property of the whole path, not just of the atomic each
    /// supervisor answers from: this is the call every MCP handler makes
    /// before it answers anything (`mcp::GMeshMcpServer::replay_queued_changes`),
    /// so the map lookup underneath it must never be able to queue behind a
    /// spawn - see [`active_supervisors`](Self::active_supervisors).
    pub fn has_pending(&self) -> bool {
        self.active_supervisors().iter().any(|supervisor| supervisor.has_pending())
    }

    /// Replays every active supervisor's queued changes, one language at a
    /// time. Best-effort per language, matching [`file_changed`](Self::file_changed)'s
    /// contract: one language's replay failing (or waking a plugin that
    /// fails to start) is reported and must not stop another language's
    /// replay from running. Returns how many files were replayed in total,
    /// across every language, for a caller that only cares whether anything
    /// happened.
    pub fn replay_pending(&self, conn: &Mutex<Connection>) -> usize {
        let mut replayed = 0;
        for supervisor in self.active_supervisors() {
            match supervisor.replay_pending(conn) {
                Ok(count) => replayed += count,
                Err(err) => eprintln!(
                    "g-mesh daemon: could not replay the changes queued while the {} plugin \
                     slept: {err:#}",
                    supervisor.language()
                ),
            }
        }
        replayed
    }

    /// Query-time staleness check (see `PluginSupervisor::ensure_fresh`) for
    /// `file_path`, routed to whichever language's plugin claims its
    /// extension.
    ///
    /// The cheap mtime/hash comparison runs first, here, against
    /// `self.project_root` directly - before any supervisor is involved, not
    /// after - so the overwhelmingly common case (nothing changed) never
    /// spawns a plugin it does not need: a language nobody has touched yet
    /// stays unspawned for every fresh file anchoring a query, exactly the
    /// property [`PluginSupervisor::ensure_fresh`] documents for its own
    /// already-live process and that this method would otherwise silently
    /// give up the moment nothing has spawned that language yet - which,
    /// under lazy per-language spawn, is the common startup state, not an
    /// edge case. `get_or_spawn` - and therefore an actual plugin process -
    /// is only reached once the file is confirmed genuinely stale.
    ///
    /// `Ok(None)` - not an error - for a file no discovered plugin claims:
    /// there is no plugin to ask, and therefore nothing this check could ever
    /// have caught for it, the same "skip, do not fail" contract
    /// [`file_changed`](Self::file_changed) already has for an unroutable
    /// file.
    pub fn ensure_fresh(
        &self,
        conn: &Mutex<Connection>,
        file_path: &str,
    ) -> Result<Option<StalenessOutcome>> {
        let Some(language) = self.language_for(file_path).map(str::to_string) else {
            return Ok(None);
        };

        {
            let guard = conn.lock().unwrap();
            if !staleness::is_stale(&guard, &self.project_root, file_path)? {
                return Ok(Some(StalenessOutcome::AlreadyFresh));
            }
        }

        let supervisor = self.get_or_spawn(&language)?;
        supervisor.ensure_fresh(conn, file_path).map(Some)
    }
}

/// `some/dir/App.TSX` -> `Some(".tsx")`; `None` for a path with no extension
/// at all (`Makefile`, `.gitignore`, a bare directory name).
fn extension_of(file_path: &str) -> Option<String> {
    let extension = Path::new(file_path).extension()?.to_str()?;
    Some(format!(".{}", extension.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::manifest::discover;
    use crate::daemon::test_plugin;
    use crate::storage::schema;

    /// A registry over `languages`, each installed as a fake plugin claiming
    /// one extension named after it (`python` -> `.python-src`), deliberately
    /// unlike any extension a real plugin would claim.
    ///
    /// Returns the tempdirs it built (the project root and the discovery
    /// root) alongside the registry: dropping them would delete the plugin
    /// directories the registry is about to spawn from.
    fn registry_over(
        languages: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
        registry_over_inner(languages, false)
    }

    /// [`registry_over`], over plugins whose spawn does not finish until the
    /// test opens their handshake gate - what every test about *while a spawn
    /// is in flight* is built on, since that window is otherwise a few
    /// unobservable milliseconds wide. See `test_plugin::install_gated`, and
    /// note its warning: every one of those tests has to open the gate on
    /// every path, or the thread it left spawning never joins.
    fn registry_over_gated(
        languages: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
        registry_over_inner(languages, true)
    }

    fn registry_over_inner(
        languages: &[&str],
        gated: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");

        let dirs = languages
            .iter()
            .map(|language| {
                let extension = extension_for(language);
                let install =
                    if gated { test_plugin::install_gated } else { test_plugin::install };
                install(plugins.path(), language, &[extension.as_str()])
            })
            .collect();

        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        (project, plugins, dirs, registry)
    }

    fn extension_for(language: &str) -> String {
        format!(".{language}-src")
    }

    /// Discovery over `languages`, each installed as the same stub plugin
    /// [`registry_over`] uses - what [`indexer_version`] takes, without the
    /// registry it deliberately does not need. Returns the plugin root's
    /// tempdir (dropping it would delete the very files being fingerprinted)
    /// and each installed plugin's directory, so a test can edit one.
    fn discovery_over(languages: &[&str]) -> (tempfile::TempDir, Vec<PathBuf>, DiscoveredPlugins) {
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let dirs = languages
            .iter()
            .map(|language| {
                let extension = extension_for(language);
                test_plugin::install(plugins.path(), language, &[extension.as_str()])
            })
            .collect();
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
        (plugins, dirs, discovered)
    }

    /// Rewrites a plugin's entry point with different bytes - a rebuild that
    /// changed its extraction logic, which is the whole event
    /// [`indexer_version`] exists to make visible.
    fn rebuild_with_a_change(plugin_dir: &Path) {
        let path = plugin_dir.join("plugin.js");
        let mut source = fs::read_to_string(&path).expect("failed to read the stub plugin");
        source.push_str("\n// rebuilt with different extraction logic\n");
        fs::write(&path, source).expect("failed to rewrite the stub plugin");
    }

    #[test]
    fn the_generation_names_the_core_pipeline_and_a_digest_of_every_plugin() {
        let (_plugins, _dirs, discovered) = discovery_over(&["python", "go"]);

        let version = indexer_version(&discovered);
        let (core, plugins) = version.split_once('+').expect("both halves must be present");

        assert_eq!(core, CURRENT_INDEXER_VERSION);
        assert!(plugins.chars().all(|c| c.is_ascii_hexdigit()), "{plugins} must be hex");
        assert!(!plugins.is_empty(), "the plugin half must be a real digest");
        // Re-asked, it answers the same: nothing about this is derived from
        // the process asking.
        assert_eq!(indexer_version(&discovered), version);
    }

    /// The property the sort exists for, at the level a real install produces
    /// it: the same two plugins, found by scanning their roots in either
    /// order, are the same index generation. A daemon that hashed them in scan
    /// order would wipe the index of a machine whose roots happened to be
    /// listed the other way round.
    #[test]
    fn scanning_the_same_plugins_roots_in_either_order_is_the_same_generation() {
        let one_root = tempfile::tempdir().expect("failed to create a plugin root");
        let other_root = tempfile::tempdir().expect("failed to create a plugin root");
        test_plugin::install(one_root.path(), "python", &[".python-src"]);
        test_plugin::install(other_root.path(), "go", &[".go-src"]);
        let (one, other) = (one_root.path().to_path_buf(), other_root.path().to_path_buf());

        let forwards = discover(&[one.clone(), other.clone()]).unwrap();
        let backwards = discover(&[other, one]).unwrap();

        assert_eq!(forwards.manifests.len(), 2, "both roots must have contributed");
        assert_eq!(indexer_version(&backwards), indexer_version(&forwards));
    }

    /// The same property against the other source of order this could have
    /// picked up: `HashMap`'s own iteration, which is seeded per map and so
    /// genuinely differs between two maps holding the same entries. Enough
    /// languages that two maps iterating in the same order by chance is not
    /// what makes this pass.
    #[test]
    fn the_generation_does_not_depend_on_hash_map_iteration_order() {
        let languages = ["python", "go", "rust", "ruby", "elixir", "zig", "nim", "ocaml"];
        let (_plugins, _dirs, discovered) = discovery_over(&languages);

        let mut reversed = DiscoveredPlugins::default();
        for language in languages.iter().rev() {
            let manifest = discovered.manifests[*language].clone();
            for extension in &manifest.extensions {
                reversed.routing.insert(extension.clone(), language.to_string());
            }
            reversed.manifests.insert(language.to_string(), manifest);
        }

        assert_eq!(reversed.manifests, discovered.manifests, "the same set, differently built");
        assert_eq!(indexer_version(&reversed), indexer_version(&discovered));
    }

    /// Task 116's failure, once per language: whichever plugin was rebuilt,
    /// the index it filled is no longer what today's pipeline would produce.
    #[test]
    fn rebuilding_either_languages_plugin_changes_the_generation() {
        let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
        let (python, go) = (&dirs[0], &dirs[1]);

        let before = indexer_version(&discovered);

        rebuild_with_a_change(python);
        let after_python = indexer_version(&discovered);
        assert_ne!(after_python, before, "a rebuilt python plugin must move the generation");

        rebuild_with_a_change(go);
        let after_go = indexer_version(&discovered);
        assert_ne!(after_go, after_python, "and so must a rebuilt go plugin");
        assert_ne!(after_go, before);
    }

    /// The control that keeps the test above about content rather than about
    /// mtime - a plugin's build system re-emitting identical bytes must not
    /// cost every project on the machine a full re-walk.
    #[test]
    fn re_emitting_a_plugin_unchanged_leaves_the_generation_alone() {
        let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
        let before = indexer_version(&discovered);

        let path = dirs[0].join("plugin.js");
        let source = fs::read(&path).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&path, source).unwrap();

        assert_eq!(indexer_version(&discovered), before);
    }

    /// Installing (or removing) a plugin changes the generation, because it
    /// changes what the index is a graph *of*: the languages an existing
    /// index covers are exactly the ones that were installed when it was
    /// filled. The languages that stayed put contribute the same thing
    /// either way, which is what makes this a fact about the set and not
    /// about one plugin having moved.
    #[test]
    fn adding_a_second_language_changes_the_generation_without_disturbing_the_first() {
        let (_one_root, _one_dirs, only_python) = discovery_over(&["python"]);
        let (_other_root, _other_dirs, both) = discovery_over(&["python", "go"]);
        let (_third_root, _third_dirs, python_again) = discovery_over(&["python"]);

        assert_ne!(indexer_version(&both), indexer_version(&only_python));
        // Two separate installs of the same one plugin agree - the digest is
        // over what the plugins *contain*, not over where they were found.
        assert_eq!(indexer_version(&python_again), indexer_version(&only_python));
    }

    /// A machine with no plugins at all still gets a well-formed generation
    /// rather than an empty half or a panic - and it is not the generation of
    /// a machine that has one.
    #[test]
    fn a_discovery_that_found_nothing_still_produces_a_well_formed_generation() {
        let empty = indexer_version(&DiscoveredPlugins::default());
        let (_plugins, _dirs, discovered) = discovery_over(&["python"]);

        let (core, plugins) = empty.split_once('+').expect("both halves must be present");
        assert_eq!(core, CURRENT_INDEXER_VERSION);
        assert!(plugins.chars().all(|c| c.is_ascii_hexdigit()), "{plugins} must be hex");
        assert_ne!(empty, indexer_version(&discovered));
    }

    /// The acceptance criterion this whole value exists to serve, driven
    /// through the check that really consumes it: `ensure_current` keeps an
    /// index whose generation still matches and throws one away whose
    /// plugin-derived half has moved - now for *any* discovered plugin, not
    /// just the bundled one.
    #[test]
    fn ensure_current_reindexes_when_any_one_plugins_build_changes() {
        let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
        let conn = Connection::open_in_memory().unwrap();

        assert!(
            schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
            "a fresh index always owes a walk"
        );
        assert!(
            !schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
            "nothing has changed, so the index it just stamped must satisfy its own check"
        );

        // Only the *second* language's plugin is rebuilt - the one a
        // single-bundled-plugin generation string would have said nothing
        // about.
        rebuild_with_a_change(&dirs[1]);

        assert!(
            schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
            "a rebuilt plugin must cost the index it filled a full re-walk"
        );
        assert!(
            !schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
            "and the generation it re-stamped must be the rebuilt one"
        );
    }

    /// Against the real bundled plugin rather than a stub: the generation a
    /// daemon on this machine would actually compute names a readable build,
    /// not [`plugin::FINGERPRINT_UNAVAILABLE`] - `core/build.rs` has just
    /// built the plugin discovery finds.
    #[test]
    fn the_real_bundled_plugin_root_produces_a_readable_generation() {
        let bundled_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
        let discovered = discover(&[bundled_root]).expect("the bundled root must discover cleanly");

        assert!(
            discovered.manifests.contains_key(crate::daemon::plugin::BUNDLED_LANGUAGE),
            "the bundled plugin must be among what was discovered"
        );
        for manifest in discovered.manifests.values() {
            assert_ne!(
                plugin::fingerprint(manifest),
                plugin::FINGERPRINT_UNAVAILABLE,
                "{} must be fingerprintable - `cargo test` builds it",
                manifest.language
            );
        }
        assert_eq!(
            indexer_version(&discovered),
            format!("{CURRENT_INDEXER_VERSION}+{}", plugins_digest(&discovered))
        );
    }

    /// Kills `pid` the way an OOM-killer would - the same out-of-band crash
    /// `core/tests/plugin_crash_recovery.rs` stages, and deliberately not a
    /// cooperative shutdown. `force_stop` is `SIGKILL` on Unix and
    /// `TerminateProcess` on Windows, which is the same "no chance to clean
    /// up" this asks for on both.
    fn kill_out_of_band(pid: u32) {
        let _ = crate::process::force_stop(pid);
    }

    #[test]
    fn routing_resolves_an_extension_to_its_language_and_ignores_case() {
        let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

        assert_eq!(registry.language_for("src/app.python-src"), Some("python"));
        assert_eq!(registry.language_for("src/App.PYTHON-SRC"), Some("python"));
        assert_eq!(registry.language_for("README.md"), None);
        assert_eq!(registry.language_for("Makefile"), None);
    }

    /// The acceptance criterion for an unclaimed extension: skipped, not an
    /// error, and nothing spawned for it.
    #[test]
    fn a_file_no_plugin_claims_is_skipped_without_spawning_anything() {
        let (_project, _plugins, dirs, registry) = registry_over(&["python"]);
        let conn = test_plugin::empty_index();

        registry.file_changed(&conn, "README.md".to_string());
        registry.file_changed(&conn, "docs/design.md".to_string());
        registry.file_changed(&conn, "Makefile".to_string());

        assert!(
            registry.supervisors.lock().unwrap().is_empty(),
            "a file nothing claims must not bring a plugin up"
        );
        assert!(test_plugin::spawns(&dirs[0]).is_empty(), "no plugin process may have been spawned");
    }

    /// ...and it is reported once, however many such files arrive. The
    /// `Option` returned here is exactly what `file_changed` prints, so
    /// counting `Some`s counts logged lines.
    #[test]
    fn an_unclaimed_extension_is_reported_once_however_many_files_share_it() {
        let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

        let first = registry.unroutable_notice("README.md");
        assert!(first.is_some(), "the first unclaimed file of its kind must be reported");
        assert!(first.unwrap().contains(".md"), "the message must name the extension");

        for i in 0..50 {
            assert_eq!(
                registry.unroutable_notice(&format!("docs/page-{i}.md")),
                None,
                "every later .md file must be silent"
            );
        }

        // A genuinely new extension is genuinely new information, and gets
        // its own single line.
        assert!(registry.unroutable_notice("notes.txt").is_some());
        assert_eq!(registry.unroutable_notice("other.txt"), None);

        // Extensionless files are one more kind, reported once as a group.
        assert!(registry.unroutable_notice("Makefile").is_some());
        assert_eq!(registry.unroutable_notice("Dockerfile"), None);
    }

    /// The lazy-spawn acceptance criterion, asserted on processes rather than
    /// on return values: two files of one language produce exactly one plugin
    /// process, and the same `Arc` both times.
    #[test]
    fn the_second_file_of_a_language_reuses_the_first_files_plugin() {
        let (_project, _plugins, dirs, registry) = registry_over(&["python"]);
        let conn = test_plugin::empty_index();
        let python = &dirs[0];

        registry.file_changed(&conn, "src/one.python-src".to_string());
        assert_eq!(test_plugin::spawns(python).len(), 1, "the first file must spawn the plugin");
        let pid = registry.get_or_spawn("python").unwrap().pid();

        registry.file_changed(&conn, "src/two.python-src".to_string());

        assert_eq!(
            test_plugin::spawns(python).len(),
            1,
            "a second file of the same language must not spawn a second process"
        );
        assert_eq!(registry.supervisors.lock().unwrap().len(), 1);
        assert_eq!(registry.get_or_spawn("python").unwrap().pid(), pid, "still the same process");

        // And the memoized supervisor is literally the same object, not an
        // equal-looking one.
        assert!(Arc::ptr_eq(
            &registry.get_or_spawn("python").unwrap(),
            &registry.get_or_spawn("python").unwrap()
        ));
    }

    /// Two languages get two independent supervisors, each running its own
    /// manifest's plugin - the routing table decides which, not spawn order.
    #[test]
    fn each_language_gets_its_own_supervisor_and_its_own_process() {
        let (_project, _plugins, dirs, registry) = registry_over(&["python", "go"]);
        let conn = test_plugin::empty_index();
        let (python, go) = (&dirs[0], &dirs[1]);

        registry.file_changed(&conn, "app.python-src".to_string());
        registry.file_changed(&conn, "main.go-src".to_string());

        assert_eq!(test_plugin::spawns(python).len(), 1);
        assert_eq!(test_plugin::spawns(go).len(), 1);
        assert_eq!(registry.get_or_spawn("python").unwrap().language(), "python");
        assert_eq!(registry.get_or_spawn("go").unwrap().language(), "go");
        assert!(!Arc::ptr_eq(
            &registry.get_or_spawn("python").unwrap(),
            &registry.get_or_spawn("go").unwrap()
        ));
    }

    /// The isolation guarantee this whole shape exists for: one language's
    /// plugin dying (and being recovered) leaves every other language's
    /// exactly where it was.
    #[test]
    fn a_crash_in_one_languages_plugin_leaves_another_languages_untouched() {
        let (_project, _plugins, dirs, registry) = registry_over(&["python", "go"]);
        let conn = test_plugin::empty_index();
        let (python_dir, go_dir) = (&dirs[0], &dirs[1]);

        registry.file_changed(&conn, "app.python-src".to_string());
        registry.file_changed(&conn, "main.go-src".to_string());

        let python = registry.get_or_spawn("python").unwrap();
        let go = registry.get_or_spawn("go").unwrap();
        let crashed_pid = python.pid().expect("the python plugin must be awake");
        let go_pid = go.pid().expect("the go plugin must be awake");

        kill_out_of_band(crashed_pid);
        // The next change to a python file is what finds the plugin gone:
        // `PluginProcess` relaunches and replays it, transparently.
        registry.file_changed(&conn, "app.python-src".to_string());

        let recovered_pid = python.pid().expect("the python plugin must have been relaunched");
        assert_ne!(recovered_pid, crashed_pid, "a fresh python process must have been spawned");
        assert_eq!(
            test_plugin::spawns(python_dir).len(),
            2,
            "exactly one relaunch: the original process and its replacement"
        );

        // The whole point: nothing about `go` moved. Same process, never
        // re-spawned, still its own supervisor, still serving.
        assert_eq!(go.pid(), Some(go_pid), "the go plugin must be the very same process");
        assert!(crate::daemon::is_process_alive(go_pid), "the go plugin must still be running");
        assert_eq!(
            test_plugin::spawns(go_dir).len(),
            1,
            "the go plugin must not have been re-spawned by another language's crash"
        );
        registry.file_changed(&conn, "other.go-src".to_string());
        assert_eq!(go.pid(), Some(go_pid), "and it must still be serving off that same process");
        assert_eq!(test_plugin::spawns(go_dir).len(), 1);
    }

    /// How long the task-164 tests below give a plugin *process* to appear
    /// once something has asked for it. Only ever hit by a genuine failure, so
    /// it is generous: a `node` start under a fully parallel `cargo test` has
    /// been measured taking over a second on this machine, and none of these
    /// tests is about how long that takes.
    const SPAWN_DEADLINE: Duration = Duration::from_secs(20);

    /// How long a test waits for a reader thread it expects straight back.
    /// Reaching this means the reader is blocked on the in-progress spawn -
    /// which nothing releases until the test itself does, further down - so it
    /// is a failure signal, not a timing measurement.
    const READER_DEADLINE: Duration = Duration::from_secs(10);

    /// What the reader call itself is allowed to cost once it has been shown
    /// to come back at all. Deliberately loose next to the map lookup it
    /// really measures: [`READER_DEADLINE`] is what catches a reader that
    /// waited on the spawn, and this only has to rule out a reader that
    /// somehow waited on *most* of one without a loaded machine's scheduling
    /// noise failing it by accident.
    const READER_BUDGET: Duration = Duration::from_millis(500);

    /// Whether `plugin_dir`'s plugin has been started `count` times, waiting
    /// up to [`SPAWN_DEADLINE`] for it.
    ///
    /// A `bool` rather than an assertion because the caller usually has a gate
    /// to open before it is allowed to fail (see `test_plugin::install_gated`):
    /// panicking here would leave a thread spawning forever and hang the test
    /// instead of failing it.
    ///
    /// The fixture plugin records its pid before it does anything else, so
    /// against a gated plugin this returns *during* the spawn - the process
    /// exists, `PluginProcess::spawn` is still blocked reading its handshake -
    /// which is exactly the state the tests below need, entered by observation
    /// rather than by sleeping a guessed amount.
    fn spawned_within(plugin_dir: &Path, count: usize) -> bool {
        let deadline = std::time::Instant::now() + SPAWN_DEADLINE;
        while test_plugin::spawns(plugin_dir).len() < count {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Task 164's headline acceptance criterion: a reader of the registry is
    /// not blocked behind an in-progress spawn - not even the spawn of the
    /// very language it is asking about.
    ///
    /// `has_pending` is the call that made this matter (every MCP tool call
    /// asks it, via `mcp::GMeshMcpServer::replay_queued_changes`), and
    /// `active_supervisors` is what it and every other reader goes through.
    /// Both are called from a thread of their own while the spawn is held
    /// open, so "was it blocked" is answered by whether that thread comes back
    /// at all rather than by how long anything took: before this fix they took
    /// the same `Mutex` `get_or_spawn` held for the whole spawn, so the reader
    /// could not return until the gate below was opened - which happens after
    /// it is waited for.
    #[test]
    fn a_reader_is_not_held_up_by_an_in_progress_spawn() {
        let (_project, _plugins, dirs, registry) = registry_over_gated(&["python"]);
        let python = &dirs[0];

        let (in_flight, measured): (bool, Option<(usize, bool, Duration)>) =
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    registry.get_or_spawn("python").expect("the fixture plugin must start");
                });
                // The process is up and waiting on its gate: the spawn is in
                // flight, and stays that way until this test says otherwise.
                let in_flight = spawned_within(python, 1);

                let measured = in_flight.then(|| {
                    let (reported, reports) = std::sync::mpsc::channel();
                    let reader = &registry;
                    scope.spawn(move || {
                        let started = std::time::Instant::now();
                        let active = reader.active_supervisors().len();
                        let pending = reader.has_pending();
                        let _ = reported.send((active, pending, started.elapsed()));
                    });
                    reports.recv_timeout(READER_DEADLINE).ok()
                });

                // Before anything is asserted, whatever happened above: a
                // spawning thread that is never let go never joins, and this
                // scope would hang instead of failing.
                test_plugin::open_handshake_gate(python);
                (in_flight, measured.flatten())
            });

        assert!(in_flight, "the plugin process never started, so no spawn was ever in flight");
        let (active, pending, elapsed) = measured.expect(
            "a reader never came back while a spawn was in flight - it is blocked behind it",
        );
        // Nothing is reported yet, which is what proves the reader really did
        // run inside the spawn rather than after it.
        assert_eq!(
            active, 0,
            "a language whose spawn has not finished is not an active supervisor yet"
        );
        assert!(!pending, "a supervisor that does not exist yet has nothing queued");
        assert!(elapsed < READER_BUDGET, "a reader spent most of a spawn waiting: {elapsed:?}");

        // ...and once the spawn lands, the same readers see it, so skipping
        // the slot was a deferral and not a drop.
        assert_eq!(registry.active_supervisors().len(), 1);
        assert_eq!(test_plugin::spawns(python).len(), 1, "exactly one process, once");
    }

    /// Task 154's guarantee, kept: callers racing on the same unspawned
    /// language produce exactly one process, and the same supervisor for
    /// everyone. Asserted against a plugin whose spawn is held open until
    /// every racer has had its chance at it, rather than one that comes up so
    /// fast the race is over before it starts.
    #[test]
    fn racing_callers_for_one_language_still_spawn_exactly_one_process() {
        let (_project, _plugins, dirs, registry) = registry_over_gated(&["python"]);
        let python = &dirs[0];

        let supervisors: Vec<Arc<PluginSupervisor>> = std::thread::scope(|scope| {
            let racers: Vec<_> =
                (0..4).map(|_| scope.spawn(|| registry.get_or_spawn("python"))).collect();

            // One racer has reserved the slot and its process is up; the
            // others get a moment to reach the same call before that spawn is
            // allowed to finish. Any of them that got past the reservation
            // would start a second process, which the spawn log below counts -
            // the grace period makes the race real, it is not what makes the
            // assertion true.
            let in_flight = spawned_within(python, 1);
            std::thread::sleep(Duration::from_millis(20));
            test_plugin::open_handshake_gate(python);
            assert!(in_flight, "the plugin process never started");

            racers
                .into_iter()
                .map(|racer| {
                    racer.join().expect("no racer may panic").expect("the fixture plugin must start")
                })
                .collect()
        });

        assert_eq!(
            test_plugin::spawns(python).len(),
            1,
            "four callers racing on one language must not start four plugin processes"
        );
        assert_eq!(registry.supervisors.lock().unwrap().len(), 1);
        for supervisor in &supervisors {
            assert!(
                Arc::ptr_eq(supervisor, &supervisors[0]),
                "every racer must be handed the one supervisor that was actually spawned"
            );
        }
    }

    /// The other half of what the old whole-spawn lock cost: two *different*
    /// languages could not start at the same time either.
    ///
    /// Asserted on processes rather than on elapsed time, and so without a
    /// timing assumption of any kind: both plugins have to be *running* while
    /// neither handshake has been let through, which under a lock held across
    /// the whole spawn is impossible by construction - whichever language got
    /// there first would hold the map until its own gate opened, and the
    /// second language's process could not even be launched.
    #[test]
    fn two_languages_spawn_at_the_same_time_rather_than_one_after_the_other() {
        let (_project, _plugins, dirs, registry) = registry_over_gated(&["python", "go"]);
        let (python, go) = (&dirs[0], &dirs[1]);

        let both_up = std::thread::scope(|scope| {
            scope.spawn(|| {
                registry.get_or_spawn("python").expect("the python fixture must start");
            });
            scope.spawn(|| {
                registry.get_or_spawn("go").expect("the go fixture must start");
            });

            let both_up = spawned_within(python, 1) && spawned_within(go, 1);
            test_plugin::open_handshake_gate(python);
            test_plugin::open_handshake_gate(go);
            both_up
        });

        assert!(
            both_up,
            "one language's process never started while the other's spawn was in flight - \
             the two spawns were serialized"
        );
        assert_eq!(registry.active_supervisors().len(), 2);
    }

    /// A spawn that fails still memoizes nothing - the criterion the old
    /// implementation met by never inserting anything, and this one has to
    /// meet by removing the reservation it did insert. The callers that were
    /// waiting on it are answered with its failure rather than each repeating
    /// it, and the language is spawnable again the moment its plugin works.
    #[test]
    fn a_failed_spawn_memoizes_nothing_and_answers_everyone_waiting_on_it() {
        let (_project, plugins, dirs, registry) = registry_over(&["python"]);
        // A plugin whose runtime is broken: it stays up long enough for the
        // other callers to pile in behind it, then exits without ever sending
        // a handshake, which is what `PluginProcess::spawn` fails on.
        fs::write(
            dirs[0].join("plugin.js"),
            "// Generated by daemon::registry's tests - a plugin that never handshakes.\n\
             setTimeout(() => process.exit(1), 200);\n",
        )
        .expect("failed to break the fixture plugin");

        let failures: Vec<String> = std::thread::scope(|scope| {
            let racers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| match registry.get_or_spawn("python") {
                        Ok(_) => panic!("a plugin that never handshakes must not start"),
                        Err(err) => format!("{err:#}"),
                    })
                })
                .collect();
            racers.into_iter().map(|racer| racer.join().expect("no racer may panic")).collect()
        });

        assert_eq!(failures.len(), 3);
        for failure in &failures {
            assert!(failure.contains("python"), "the failure must name the language: {failure}");
        }
        assert!(
            registry.supervisors.lock().unwrap().is_empty(),
            "a failed spawn - reservation and all - must leave the map exactly as it found it"
        );

        // Nothing is wedged: the next caller after the plugin is fixed gets a
        // real supervisor, which a leftover reservation would have made
        // impossible (it would wait forever on a spawn that already ended).
        test_plugin::install(plugins.path(), "python", &[extension_for("python").as_str()]);
        let supervisor = registry.get_or_spawn("python").expect("the repaired plugin must start");
        assert_eq!(supervisor.language(), "python");
        assert_eq!(registry.active_supervisors().len(), 1);
    }

    #[test]
    fn a_language_nothing_was_discovered_for_is_an_error_naming_what_was() {
        let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

        let message = match registry.get_or_spawn("rust") {
            Ok(_) => panic!("a language nothing was discovered for must not spawn anything"),
            Err(err) => format!("{err:#}"),
        };
        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("python"), "{message}");
        assert!(registry.supervisors.lock().unwrap().is_empty(), "nothing may be memoized");
    }

    #[test]
    fn each_language_records_its_pid_in_a_file_of_its_own() {
        let (_project, _plugins, _dirs, registry) = registry_over(&["python", "go"]);

        let python = registry.pid_file_for("python");
        let go = registry.pid_file_for("go");
        assert_ne!(python, go);
        assert_eq!(python.file_name().unwrap().to_str().unwrap(), "plugin-python.pid");
        assert_eq!(python.parent(), go.parent(), "both live in the project's state directory");
    }

    #[test]
    fn an_empty_discovery_result_routes_nothing_and_starts_nothing() {
        let (_project, _plugins, _dirs, registry) = registry_over(&[]);
        let conn = test_plugin::empty_index();

        assert_eq!(registry.language_for("app.py"), None);
        registry.file_changed(&conn, "app.py".to_string());
        assert!(registry.supervisors.lock().unwrap().is_empty());
    }
}
