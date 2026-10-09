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
//! `supervisors` is the outermost lock in the daemon (the whole order is in
//! `storage::index_store`'s module doc). It is released before any caller
//! touches the supervisor it got back, and it is only ever held for a map
//! lookup or a single insert/remove - never across a spawn, which would
//! stall every other caller of the map, including the `has_pending` check
//! every MCP tool call makes.
//!
//! [`SupervisorSlot`] makes that possible: a language being spawned right now
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::manifest::{self, extension_of, under_excluded_dir, DiscoveredPlugins};
use crate::daemon::plugin;
use crate::embedding::EmbeddingPipeline;
use crate::languages::LanguageOutcome;
use crate::storage::index_store::{self, IndexStore};
use crate::storage::schema::{self, CURRENT_INDEXER_VERSION};
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

/// Where a language's memory-limit suspension marker is recorded, relative to
/// the project's state directory (task GM-274, decision 6) - the same
/// per-language naming convention as [`plugin_pid_file_name`], so
/// `daemon::lifecycle::PluginSupervisor::suspended_marker_path` can build it
/// next to that language's own pid file without a second lookup.
pub(crate) fn plugin_suspended_marker_file_name(language: &str) -> String {
    format!("plugin-{language}.suspended")
}

/// Every `plugin-<language>.suspended` marker currently present in
/// `state_dir`, paired with the language its name encodes and the reason
/// text `daemon::lifecycle::PluginSupervisor::write_suspended_marker` wrote
/// into it - [`discovered_pid_files`]'s counterpart for suspension rather
/// than liveness.
///
/// Deliberately independent of [`discovered_pid_files`]: a language
/// suspended by `[plugin] memoryLimitMb` has *no* pid file by the time
/// anyone reads this (`PluginSupervisor::check_memory_limit` puts the plugin
/// to sleep through the same path idle-sleep uses, which removes it - see
/// `PluginSupervisor::put_to_sleep`), so `cli::status` needs its own listing
/// to find a suspended language at all, not a field bolted onto a pid-file
/// row that will already be gone.
///
/// Empty - not an error - for a state directory that does not exist or
/// cannot be listed, matching [`discovered_pid_files`]'s own convention. A
/// marker that exists but cannot be read is skipped, its reason reported as
/// `"(unreadable)"` rather than dropping the whole row - unlike a dead pid
/// (which really does mean "nothing to report"), a marker on disk with no
/// readable reason is still evidence *something* suspended this language,
/// and `g-mesh status` should say so rather than silently agreeing with a
/// stat/read failure.
pub fn discovered_suspended_markers(state_dir: &Path) -> Vec<(String, String)> {
    let mut markers = Vec::new();
    let Ok(entries) = fs::read_dir(state_dir) else { return markers };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(language) = name.strip_prefix("plugin-").and_then(|n| n.strip_suffix(".suspended")) {
            let reason = fs::read_to_string(entry.path())
                .map(|contents| contents.trim().to_string())
                .unwrap_or_else(|_| "(unreadable)".to_string());
            markers.push((language.to_string(), reason));
        }
    }
    markers.sort_by(|a, b| a.0.cmp(&b.0));
    markers
}

/// Removes every `plugin-<language>.suspended` marker in `state_dir` -
/// decision 4/5's honest consequence: since config is read once at daemon
/// startup and never hot-reloaded, "suspended until the daemon restarts or
/// the config changes" only ever actually clears on a restart in practice
/// (see `daemon::lifecycle::PluginSupervisor`'s own doc comment on
/// `semantic_suspended`). This is where that restart takes effect on disk -
/// called once, early in `daemon::run`, while the project's singleton lock
/// guarantees no other daemon is serving it (the same guarantee
/// `endpoint.clear_stale()` right beside it already relies on) - and again by
/// `cli::stop`'s own state-file cleanup, so a project nothing is serving
/// never keeps reporting a suspension no daemon remembers deciding.
///
/// Best-effort, like every other state-file removal in this daemon: a marker
/// this fails to remove is stale, not incorrect - the next successful call
/// (the next start, or the next `stop`) clears it.
pub fn clear_stale_suspension_markers(state_dir: &Path) {
    for (_, path) in discovered_suspended_marker_paths(state_dir) {
        let _ = fs::remove_file(path);
    }
}

/// [`discovered_suspended_markers`]'s sibling for callers that need the path
/// to remove rather than the reason to display - kept as its own small walk
/// (rather than reusing `discovered_suspended_markers` and discarding the
/// reason) so [`clear_stale_suspension_markers`] does not pay for reading
/// every marker's contents just to delete the file.
fn discovered_suspended_marker_paths(state_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(state_dir) else { return files };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(language) = name.strip_prefix("plugin-").and_then(|n| n.strip_suffix(".suspended")) {
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
pub(crate) fn plugins_digest(discovered: &DiscoveredPlugins) -> String {
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

/// Above this many files one language would gain from a `.gitignore` change,
/// [`PluginRegistry::gitignore_changed`] does not reindex it (GM-508, Q2).
pub(crate) const GITIGNORE_REINDEX_GUARD: usize = 10_000;

/// The directories holding `gitignores` (project-relative, `""` for the
/// root), deduplicated; just the root when the root's own file is among them.
fn gitignore_subtrees(gitignores: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = gitignores
        .iter()
        .map(|path| path.rsplit_once('/').map_or(String::new(), |(dir, _)| dir.to_string()))
        .collect();
    if dirs.iter().any(String::is_empty) {
        return vec![String::new()];
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Each language's `File` nodes under `subtrees` (as [`gitignore_subtrees`]
/// spells them): the live side of [`PluginRegistry::gitignore_changed`].
fn indexed_files_under(
    conn: &rusqlite::Connection,
    subtrees: &[String],
) -> Result<BTreeMap<String, HashSet<String>>> {
    let mut statement = conn
        .prepare("SELECT language, filePath FROM nodes WHERE kind = 'File'")
        .context("failed to query indexed files")?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .context("failed to read indexed files")?;
    let mut files: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for row in rows {
        let (language, file_path) = row.context("failed to read indexed files")?;
        let under = subtrees.iter().any(|dir| {
            dir.is_empty() || file_path.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/'))
        });
        if under {
            files.entry(language).or_default().insert(file_path);
        }
    }
    Ok(files)
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
                    supervisors
                        .insert(self.language.clone(), SupervisorSlot::Running(Arc::clone(supervisor)));
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

/// Why a path-anchored answer about one file can be empty while the file
/// exists: [`PluginRegistry::path_coverage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathCoverage {
    /// No discovered plugin claims the file; the catalogue entry of the
    /// plugin that would index it.
    Absent(&'static crate::languages::CatalogueEntry),
    /// The discovered plugin for this language failed its bulk walk, so
    /// nothing of the language is in the index (ADR 0021, section 2).
    Failed(String),
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
    /// Handed to every supervisor as it is created, exactly like
    /// `idle_timeout` beside it - `[plugin] memoryLimitMb` (task GM-274) is
    /// one number for every language, not a per-language override (see the
    /// architecture doc's "Plugin memory limit" section for why).
    memory_limit_mb: Option<u64>,
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
    /// Languages whose bulk walk failed, set once by the walk for the rest of
    /// this daemon's life. Their files are not routed: a single-file update
    /// would put part of a language into an index that holds it wholly or not
    /// at all (ADR 0021).
    failed_languages: Mutex<HashSet<String>>,
}

impl PluginRegistry {
    /// The canonicalized project root, for the one caller that needs to read
    /// the project's files rather than route a request about them: the MCP
    /// server, whose handlers answer out of the index and so have no root of
    /// their own. `find_definition` uses it to return the source its
    /// coordinates point at.
    ///
    /// Read-only by design. Nothing outside this module may substitute a root -
    /// every supervisor a registry spawns is bound to this one, and two answers
    /// about "the project" disagreeing on which project would be a bug with no
    /// visible symptom.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// The embedding pipeline every supervisor this registry spawns already
    /// shares - GM-272's `daemon::workspace_reindex` needs the same `&Arc`
    /// `daemon::bulk_index::walk_one_language` takes, for the same reason
    /// every other bulk-index caller does (`EmbeddingPipeline::apply` on each
    /// committed batch). `pub(crate)`, not `pub`: unlike `project_root`, this
    /// has no caller outside this daemon module tree.
    pub(crate) fn embedding(&self) -> &Arc<EmbeddingPipeline> {
        &self.embedding
    }

    /// The project's state directory, where `daemon::workspace_reindex` puts
    /// its staging index.
    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Builds a registry over `discovered`. Spawns nothing - see this
    /// module's doc comment.
    pub fn new(
        project_root: &Path,
        state_dir: PathBuf,
        discovered: DiscoveredPlugins,
        idle_timeout: Option<Duration>,
        memory_limit_mb: Option<u64>,
        embedding: Arc<EmbeddingPipeline>,
    ) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            state_dir,
            discovered,
            idle_timeout,
            memory_limit_mb,
            embedding,
            supervisors: Mutex::new(HashMap::new()),
            unroutable: Mutex::new(HashSet::new()),
            failed_languages: Mutex::new(HashSet::new()),
        }
    }

    /// Records the languages the bulk walk failed, replacing any earlier set.
    pub(crate) fn set_failed_languages(&self, languages: impl IntoIterator<Item = String>) {
        let mut failed = self.failed_languages.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *failed = languages.into_iter().collect();
    }

    /// Loads the languages the last walk failed (`language_outcome` rows
    /// recorded as `Failed`), so a failed language stays out of single-file
    /// updates across daemon restarts, not only in the process that walked:
    /// see `docs/adr/0021-per-language-bulk-outcome.md`, section 2. The daemon
    /// calls it once at startup, before anything that routes a file (the tool
    /// listener, the watcher's consumer) exists; a later walk replaces the set
    /// with its own result.
    ///
    /// A table that cannot be read leaves the set empty rather than failing
    /// startup: the structural index is otherwise usable, and refusing to
    /// serve it over this table would turn a degraded guarantee into an outage.
    pub(crate) fn seed_failed_languages(&self, conn: &IndexStore) {
        match conn.with(schema::language_outcomes) {
            Ok(outcomes) => {
                self.set_failed_languages(outcomes.into_iter().filter_map(|(language, outcome)| {
                    matches!(outcome, LanguageOutcome::Failed { .. }).then_some(language)
                }))
            }
            Err(err) => crate::log_line!(
                "g-mesh daemon: could not read the recorded language outcomes - failed languages are not \
                 excluded from incremental updates until the next walk: {err:#}"
            ),
        }
    }

    /// Whether the bulk walk failed `language`, so its files are not routed.
    pub(crate) fn is_failed_language(&self, language: &str) -> bool {
        self.failed_languages.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(language)
    }

    /// Why the index can hold nothing of `file_path`, read from what this
    /// daemon already has in memory (no rediscovery, no I/O): see
    /// `docs/architecture/gm-503-absent-language-field.md`, sections 3 and 5.
    /// `None` when the file's language is indexed, or when nothing explains
    /// an empty answer (unsupported or extensionless path, or a path under
    /// the language's own `exclude_dirs`, which no plugin would index).
    ///
    /// Precedence: a discovered manifest that claims the extension makes the
    /// path failed or covered, never absent; only with no claiming manifest
    /// is the catalogue asked, so a language recorded `Failed` whose plugin
    /// has since gone reads as absent - installing is the fix.
    pub(crate) fn path_coverage(&self, file_path: &str) -> Option<PathCoverage> {
        if self.discovered.language_for(file_path).is_some() {
            let language = self.discovered.indexing_language(file_path)?;
            return self.is_failed_language(language).then(|| PathCoverage::Failed(language.to_string()));
        }
        crate::languages::absent_for_path(&self.discovered, file_path).map(PathCoverage::Absent)
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
        self.discovered.language_for(file_path)
    }

    /// Every language whose manifest routes `file_path` as a **workspace**
    /// file - GM-272's addition alongside extension routing above, and the
    /// mechanism `docs/architecture/multi-language-plugins.md`'s "Editing a
    /// Go file" data-flow paragraph describes for `go.mod`: "`watch_files` ->
    /// `workspaceChanged` -> per-language reindex".
    ///
    /// A language is in the result when **both** hold:
    ///  - its `[plugin.workspace] watch_files` (`daemon::manifest::
    ///    WorkspaceConfig::watch_files`, already-compiled globs) matches
    ///    `file_path`'s **file name alone** - "exact file names (any
    ///    directory)" per the architecture doc's `plugin.toml additions`
    ///    section, which is exactly what an exact name is as a `Glob`
    ///    (`daemon::manifest`'s own doc comment: an exact name needs no
    ///    separate code path from a genuine glob like `*.csproj`, since both
    ///    compile through the same `Glob::compile_matcher`);
    ///  - `file_path` is not [`under_excluded_dir`] of *that same manifest's*
    ///    `exclude_dirs` - each language's exclusions are its own, matching
    ///    `[plugin.workspace] exclude_dirs`'s own doc comment ("mirroring
    ///    the plugin's own walk exclusions").
    ///
    /// Sorted, and a `Vec` rather than the first match: nothing in the
    /// manifest schema forbids two languages from declaring the same
    /// `watch_files` pattern (a plugin bug, or two plugins that both watch
    /// `Makefile` for entirely different reasons), and silently routing to
    /// only one of them would drop the other's reindex with no diagnostic at
    /// all. Empty is the overwhelmingly common answer - every language whose
    /// `watch_files` is empty can never
    /// appear here, by construction, which is also GM-272's answer to "must
    /// not break a plugin that does not know `workspaceChanged`": a plugin
    /// with nothing in `watch_files` is simply never a candidate for this
    /// routing path, so it is never sent the notification, wakened for the
    /// reindex, or spawned by it - not a special case, a direct consequence
    /// of the same emptiness check this docstring already needs.
    pub fn workspace_language_matches(&self, file_path: &str) -> Vec<String> {
        let name = file_name_of(file_path);
        let mut languages: Vec<String> = self
            .discovered
            .manifests
            .values()
            .filter(|manifest| !manifest.workspace.watch_files.is_empty())
            .filter(|manifest| {
                manifest.workspace.watch_files.iter().any(|glob| glob.compile_matcher().is_match(name))
            })
            .filter(|manifest| !under_excluded_dir(file_path, &manifest.workspace.exclude_dirs))
            .map(|manifest| manifest.language.clone())
            .collect();
        languages.sort();
        languages
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

    /// The union of every discovered plugin's own `[plugin.workspace]
    /// entry_points` (`daemon::manifest::WorkspaceConfig::entry_points`),
    /// deduplicated - what `mcp::get_dependencies` feeds
    /// `graph::queries::find_files_under`/`find_files_ending_in_dir` (via
    /// `entry_point_rank_expr`) so a miss-path directory lookup ranks each
    /// language's own convention first, instead of the single hardcoded
    /// `index.*` check GM-273 replaced.
    ///
    /// "Every discovered manifest", not "only languages this project's index
    /// actually has files for" - `discovered` (see this struct's own doc
    /// comment) is read once at startup, before a single file has been
    /// indexed, and the distinction would need its own query against
    /// `nodes.language` on the hot path of every `get_dependencies` miss,
    /// for an outcome that cannot change which candidate wins: a Rust-only
    /// repo simply has no file named `index.*` for an unused `entry_points`
    /// convention to falsely match, so an extra candidate a manifest declares
    /// costs one more no-op `LIKE` test per scanned row (see
    /// `entry_point_rank_expr`'s cost section), not a wrong answer.
    ///
    /// Deduplicated (not just concatenated) because two plugins are free to
    /// declare the same literal entry point without that meaning anything -
    /// `entry_point_rank_expr` only cares about the *set* of strings, and a
    /// duplicate would otherwise double one candidate's `LIKE` cost in the
    /// generated SQL for no behavioral difference. Sorted first so that
    /// dedup, and this method's own output, do not depend on `HashMap`
    /// iteration order - the same "stable regardless of scan order" property
    /// [`indexer_version`] already goes out of its way to guarantee for
    /// `DiscoveredPlugins` as a whole.
    pub fn entry_points(&self) -> Vec<String> {
        let mut points: Vec<String> = self
            .discovered
            .manifests
            .values()
            .flat_map(|m| m.workspace.entry_points.iter().cloned())
            .collect();
        points.sort();
        points.dedup();
        points
    }

    /// Every discovered manifest's `[plugin.capabilities]`
    /// (`daemon::manifest::Capabilities`), keyed by language - what
    /// `mcp::instructions` (GM-262) reads to decide, per language present in
    /// the index, whether its receiver-call gap
    /// (`Capabilities::receiver_calls`/`receiver_calls_structural`) is still
    /// open. `Capabilities` is `Copy` (see its own derive), so this clones
    /// nothing heavier than the language-id keys - the same cheap shape as
    /// [`entry_points`](Self::entry_points) above, read fresh per call rather
    /// than cached for the same reason `mcp::get_dependencies` reads
    /// `entry_points()` fresh: `discovered` never changes while this daemon
    /// runs, so there is nothing a cache would save.
    ///
    /// "Every discovered manifest", not "only languages this project's index
    /// actually has files for" - the same distinction
    /// [`entry_points`](Self::entry_points)'s own doc comment draws, for the
    /// same reason: this registry has no per-project presence to consult, and
    /// the caller (`mcp::instructions::build`) already filters by what the
    /// index reports present before this map is ever indexed into.
    pub fn receiver_call_capabilities(&self) -> HashMap<String, manifest::Capabilities> {
        self.discovered.manifests.iter().map(|(language, m)| (language.clone(), m.capabilities)).collect()
    }

    /// Every catalogued language with no discovered plugin
    /// (`languages::missing`), in catalogue order. Needs no I/O, so the cold
    /// start's MCP instructions can name them.
    pub fn missing_languages(&self) -> Vec<&'static str> {
        crate::languages::missing(&self.discovered).into_iter().map(|entry| entry.language).collect()
    }

    /// Every discovered language's `[plugin.non_symbol_queries]`, for the
    /// semantic rung. Fixed for the daemon's lifetime, as discovery is.
    pub(crate) fn query_shapes(&self) -> crate::mcp::query_shapes::QueryShapes {
        crate::mcp::query_shapes::QueryShapes::from_manifests(self.discovered.manifests.values())
    }

    /// Whether `language`'s manifest declares both `semantic_pass` and
    /// `semantic_prepare` - the plugins `daemon::semantic` tells about an
    /// owed pass before asking for it. Answered from the manifest, so it
    /// spawns nothing.
    pub fn wants_semantic_prepare(&self, language: &str) -> bool {
        self.discovered
            .manifests
            .get(language)
            .is_some_and(|m| m.capabilities.semantic_pass && m.capabilities.semantic_prepare)
    }

    /// Every discovered language whose manifest declares
    /// `capabilities.semantic_pass = true`, sorted - what
    /// `daemon::semantic::run_with_registry` iterates over to ask each
    /// language's whole-project semantic pass, generalized (GM-270) from the
    /// single hardcoded `plugin::BUNDLED_LANGUAGE` question it replaces.
    ///
    /// A thin wrapper over `daemon::manifest::semantic_pass_capable_languages`,
    /// which owns the filter and the sort order (see that function's own doc
    /// comment) - both shared with `daemon::semantic::run_once`, which has a
    /// bare `&DiscoveredPlugins` and no registry to ask this of.
    pub fn semantic_pass_languages(&self) -> Vec<String> {
        manifest::semantic_pass_capable_languages(&self.discovered.manifests)
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
        index_store::assert_not_held();
        let mut supervisors = self.supervisors.lock().unwrap();
        // Cloned out of the map so the decision below can act with the guard
        // dropped - both arms leave the map lock before doing anything that
        // takes longer than a hash lookup.
        match supervisors.get(language).cloned() {
            Some(SupervisorSlot::Running(running)) => return Ok(running),
            Some(SupervisorSlot::Spawning(marker)) => {
                drop(supervisors);
                return marker.wait().with_context(|| {
                    format!("the in-progress spawn of the {language} plugin this call waited on failed")
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

        let mut reservation =
            SpawnReservation { registry: self, language: language.to_string(), marker, settled: false };
        let spawned = PluginSupervisor::start(
            &self.project_root,
            manifest.clone(),
            self.pid_file_for(language),
            self.idle_timeout,
            self.memory_limit_mb,
            Arc::clone(&self.embedding),
        );
        reservation.settle(spawned)
    }

    /// The watcher thread's actual entry point (GM-272): decides, for one
    /// settled path, whether it is a **workspace** file for some language
    /// ([`workspace_language_matches`](Self::workspace_language_matches)) or
    /// an ordinary source file routed by extension
    /// ([`file_changed`](Self::file_changed)) - never both, and in that
    /// order, matching the architecture doc's "routes a settled path whose
    /// file name matches a manifest's watch_files... to that language"
    /// wording: a workspace match is a *different kind* of event for that
    /// path (a per-language reindex, not a reparse of the path itself - a
    /// `go.mod` is never itself an indexable source file), so it supersedes
    /// extension routing for the same settled path rather than running
    /// alongside it.
    ///
    /// Every matching language is reindexed (see [`workspace_language_matches`](Self::workspace_language_matches)
    /// on why that can be more than one), each independently - one
    /// language's reindex failing must not skip another's, the same
    /// "failures are reported and dropped, never propagated" contract every
    /// other watcher-thread entry point in this module already has.
    pub fn route_settled_path(&self, conn: &IndexStore, file_path: String) {
        let workspace_languages = self.workspace_language_matches(&file_path);
        if workspace_languages.is_empty() {
            self.file_changed(conn, file_path);
            return;
        }
        for language in workspace_languages {
            if self.is_failed_language(&language) {
                continue;
            }
            self.workspace_file_changed(conn, &language, &file_path);
        }
    }

    /// Announces one batch's created paths, per language, before any of them
    /// is routed ([`route_settled_path`](Self::route_settled_path) still routes
    /// every one of them afterwards). A path counts for the language that would
    /// receive its `fileChanged`, under the same filters
    /// [`file_changed`](Self::file_changed) applies: not a workspace file, claimed,
    /// not under that language's `exclude_dirs`, not a failed language.
    ///
    /// Only a language with at least two such paths is told: one created file
    /// cannot be both the importer and the target the notification exists for.
    /// Only a running supervisor is asked, so nothing is spawned or woken; a
    /// plugin that starts later reads the disk after the batch. Whether the
    /// plugin understands the message is its manifest's `files_created`,
    /// checked where the message is sent. A failed send is reported and
    /// dropped: the per-file routing that follows meets the same process.
    pub fn announce_created(&self, created: &[String]) {
        let mut by_language: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for file_path in created {
            if !self.workspace_language_matches(file_path).is_empty() {
                continue;
            }
            let Some(language) = self.discovered.indexing_language(file_path) else { continue };
            if self.is_failed_language(language) {
                continue;
            }
            by_language.entry(language.to_string()).or_default().push(file_path.clone());
        }
        for (language, file_paths) in by_language {
            if file_paths.len() < 2 {
                continue;
            }
            let running = self.supervisors.lock().unwrap().get(&language).and_then(SupervisorSlot::running);
            let Some(supervisor) = running else { continue };
            if let Err(err) = supervisor.files_created(&file_paths) {
                crate::log_line!(
                    "g-mesh daemon: could not tell the {language} plugin about {} created files: {err:#}",
                    file_paths.len()
                );
            }
        }
    }

    /// The watcher thread's ordinary (extension-routed) entry point: hands
    /// `file_path` to the supervisor for the language that claims it,
    /// spawning that plugin if this is the first file of its kind.
    ///
    /// Nothing here propagates. A file no plugin claims is skipped (see
    /// [`unroutable_notice`](Self::unroutable_notice)), and a plugin that
    /// cannot be started is reported - the same "failures are reported and
    /// dropped" contract `PluginSupervisor::file_changed` already has, for
    /// the same reason: one file the daemon cannot index must not take the
    /// watcher thread, or the other languages, down with it.
    ///
    /// GM-272 adds one more silent skip, alongside the existing unclaimed-
    /// extension one: a file [`under_excluded_dir`] of the claiming
    /// language's own `[plugin.workspace] exclude_dirs` is not routed at
    /// all, matching the architecture doc's "watcher should never route to
    /// it either" for that field, and mirroring
    /// [`workspace_language_matches`](Self::workspace_language_matches)'s
    /// identical check on the workspace-routing side - so `dist/bundle.js`
    /// under the bundled TS plugin's own `exclude_dirs = ["node_modules",
    /// "dist"]` stops reaching a live plugin process here exactly as it
    /// already stops being walked by that plugin's own bulk index. Silent,
    /// not logged via [`unroutable_notice`](Self::unroutable_notice): that
    /// helper's whole point is naming an extension *nothing* claims, and an
    /// excluded file's extension is claimed just fine - it is the directory
    /// that says not to route this one instance of it, which is exactly as
    /// ordinary and expected as `.gitignore` already is at the filesystem-
    /// watch layer.
    pub fn file_changed(&self, conn: &IndexStore, file_path: String) {
        if self.language_for(&file_path).is_none() {
            if let Some(notice) = self.unroutable_notice(&file_path) {
                crate::log_line!("{notice}");
            }
            return;
        }
        // Claimed, but under that language's own `exclude_dirs` - the same
        // `DiscoveredPlugins::indexing_language` filter `g-mesh status`'s
        // coverage walk applies, so the two agree on which files exist.
        let Some(language) = self.discovered.indexing_language(&file_path).map(str::to_string) else {
            return;
        };
        if self.is_failed_language(&language) {
            return;
        }

        match self.get_or_spawn(&language) {
            Ok(supervisor) => supervisor.file_changed(conn, file_path),
            Err(err) => crate::log_line!(
                "g-mesh daemon: could not start the {language} plugin for {file_path}: {err:#} - \
                 the change was not indexed"
            ),
        }
    }

    /// Routes one workspace-file match ([`workspace_language_matches`](Self::workspace_language_matches))
    /// to `language`'s per-language reindex (`daemon::workspace_reindex`),
    /// spawning that language's supervisor if this is the first file of its
    /// kind - the workspace-routing counterpart to
    /// [`file_changed`](Self::file_changed)'s ordinary `get_or_spawn` call,
    /// with the same "failures are reported and dropped" contract.
    pub(crate) fn workspace_file_changed(&self, conn: &IndexStore, language: &str, changed_file: &str) {
        match self.get_or_spawn(language) {
            Ok(supervisor) => {
                if let Err(err) = crate::daemon::workspace_reindex::run(self, &supervisor, conn, changed_file)
                {
                    crate::log_line!(
                        "g-mesh daemon: failed to reindex the {language} workspace after \
                         {changed_file} changed: {err:#} - {language}'s previous graph keeps \
                         serving, and the reindex runs again on the next daemon start"
                    );
                }
            }
            Err(err) => crate::log_line!(
                "g-mesh daemon: could not start the {language} plugin to reindex its workspace \
                 after {changed_file} changed: {err:#}"
            ),
        }
    }

    /// GM-508's gate: `gitignores` (project-relative paths of settled
    /// `.gitignore` files, created, edited or deleted) may have changed which
    /// files are indexed. Walks the directories holding them (with their
    /// ancestors' rules, `project_walk::project_files_under`), splits the
    /// files by [`DiscoveredPlugins::indexing_language`] and compares each
    /// language with its `File` nodes under the same directories. Every
    /// language with a difference is reindexed as after a workspace-file edit
    /// ([`workspace_file_changed`](Self::workspace_file_changed), the first of
    /// `gitignores` as the trigger), unless it would gain more than
    /// [`GITIGNORE_REINDEX_GUARD`] files: then one log line asks for
    /// `g-mesh reindex`. No difference, no reindex.
    pub(crate) fn gitignore_changed(&self, conn: &IndexStore, gitignores: &[String]) {
        let Some(trigger) = gitignores.first() else { return };
        let subtrees = gitignore_subtrees(gitignores);
        let live = match conn.with(|conn| indexed_files_under(conn, &subtrees)) {
            Ok(live) => live,
            Err(err) => {
                crate::log_line!(
                    "g-mesh daemon: could not read the indexed files to compare after {trigger} changed: {err:#}"
                );
                return;
            }
        };
        let mut walked: BTreeMap<String, HashSet<String>> = BTreeMap::new();
        let pruned = self.discovered.excluded_by_every_language();
        for file in crate::project_walk::project_files_under(&self.project_root, &pruned, &subtrees) {
            if let Some(language) = self.discovered.indexing_language(&file.relative) {
                walked.entry(language.to_string()).or_default().insert(file.relative);
            }
        }
        let languages: std::collections::BTreeSet<String> =
            walked.keys().chain(live.keys()).cloned().collect();
        let none = HashSet::new();
        for language in languages {
            if !self.has_manifest(&language) || self.is_failed_language(&language) {
                continue;
            }
            let now = walked.get(&language).unwrap_or(&none);
            let indexed = live.get(&language).unwrap_or(&none);
            let added = now.difference(indexed).count();
            let removed = indexed.difference(now).count();
            if added == 0 && removed == 0 {
                continue;
            }
            if added > GITIGNORE_REINDEX_GUARD {
                crate::log_line!(
                    "g-mesh daemon: {trigger} changed would add {added} {language} files to the index - \
                     more than {GITIGNORE_REINDEX_GUARD}, so nothing was reindexed; run `g-mesh reindex` \
                     to index them"
                );
                continue;
            }
            crate::log_line!(
                "g-mesh daemon: {trigger} changed which {language} files are indexed (+{added}, \
                 -{removed}) - reindexing {language}"
            );
            self.workspace_file_changed(conn, &language, trigger);
        }
    }

    /// GM-508, owner's Q3: at daemon start on an indexed project, the
    /// `.gitignore` files edited since the last walk recorded `bulkIndexedAt`
    /// go through [`gitignore_changed`](Self::gitignore_changed). A
    /// `.gitignore` deleted while the daemon was down leaves no mtime and is
    /// not seen here.
    pub(crate) fn recheck_gitignores_since_index(&self, conn: &IndexStore) {
        let indexed_at = match conn.with(schema::bulk_indexed_at_unix) {
            Ok(Some(indexed_at)) => indexed_at,
            Ok(None) => return,
            Err(err) => {
                crate::log_line!("g-mesh daemon: could not read when the project was last walked: {err:#}");
                return;
            }
        };
        let pruned = self.discovered.excluded_by_every_language();
        let newer: Vec<String> = crate::project_walk::gitignore_files(&self.project_root, &pruned)
            .filter(|file| {
                fs::metadata(&file.path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    // Whole seconds, like `bulkIndexedAt`. Strictly newer:
                    // a file written in the walk's own last second is taken
                    // as walked (an edit landing there is missed until the
                    // next `.gitignore` event).
                    .is_some_and(|since_epoch| since_epoch.as_secs() as i64 > indexed_at)
            })
            .map(|file| file.relative)
            .collect();
        if !newer.is_empty() {
            self.gitignore_changed(conn, &newer);
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
        let mut languages: Vec<&str> = self.discovered.manifests.keys().map(String::as_str).collect();
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

    /// Samples every active supervisor's plugin process tree against
    /// `[plugin] memoryLimitMb` - one call to
    /// [`PluginSupervisor::check_memory_limit`] per spawned language, on the
    /// same tick [`sleep_if_idle_all`](Self::sleep_if_idle_all) already runs
    /// on (`daemon::lifecycle::supervise` calls both, back to back). Run
    /// independently, exactly like `sleep_if_idle_all`: one language's
    /// process tree being over the limit (or not) never affects another's own
    /// check, and a language that was never spawned is not in
    /// [`active_supervisors`](Self::active_supervisors) at all, so this never
    /// spawns anything new either.
    ///
    /// With `memoryLimitMb` unset for the project, every supervisor's own
    /// `check_memory_limit` returns before sampling anything (see that
    /// method's own doc comment) - so this call costs one `Vec` walk over
    /// already-spawned supervisors and nothing more, which is what keeps "no
    /// key set means no sampling side effects" true even once this is wired
    /// into the daemon's real tick.
    pub fn check_memory_limits_all(&self) {
        for supervisor in self.active_supervisors() {
            supervisor.check_memory_limit();
        }
    }

    /// Whether `language`'s semantic passes are suspended right now - `false`
    /// for a language that was never spawned (nothing has ever sampled its
    /// memory, so it cannot have been suspended) as much as for one that was
    /// spawned and never went over the limit; both read the same way to this
    /// call's callers (`daemon::semantic`, `daemon::workspace_reindex`
    /// indirectly through `PluginSupervisor::semantic_pass`'s own gate - this
    /// method exists for callers that need the answer *without* going through
    /// that method, none of which this task adds, but kept `pub` alongside it
    /// for symmetry with `has_pending`/`active_supervisors` above).
    pub fn is_semantic_suspended(&self, language: &str) -> bool {
        self.supervisors
            .lock()
            .unwrap()
            .get(language)
            .and_then(SupervisorSlot::running)
            .is_some_and(|supervisor| supervisor.is_semantic_suspended())
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

    /// "typescript (2 files), rust (1 file)" - one entry per active
    /// supervisor that actually has something queued, in
    /// [`active_supervisors`](Self::active_supervisors)' order. GM-403's
    /// replay progress ticker names this so a caller staring at a silent
    /// call for tens of seconds sees which language woke up and how much it
    /// owes, not just that something is happening.
    ///
    /// Read once, before [`replay_pending`](Self::replay_pending) starts
    /// draining the queue it describes - a ticker that re-asked this on
    /// every tick would watch the count fall to zero mid-replay and call
    /// that news, when it is only the replay's own progress.
    pub fn pending_summary(&self) -> String {
        let entries: Vec<String> = self
            .active_supervisors()
            .iter()
            .filter(|supervisor| supervisor.has_pending())
            .map(|supervisor| {
                let count = supervisor.pending_len();
                let noun = if count == 1 { "file" } else { "files" };
                format!("{} ({count} {noun})", supervisor.language())
            })
            .collect();
        if entries.is_empty() {
            return "none".to_string();
        }
        entries.join(", ")
    }

    /// Replays every active supervisor's queued changes, one language at a
    /// time. Best-effort per language, matching [`file_changed`](Self::file_changed)'s
    /// contract: one language's replay failing (or waking a plugin that
    /// fails to start) is reported and must not stop another language's
    /// replay from running. Returns how many files were replayed in total,
    /// across every language, for a caller that only cares whether anything
    /// happened.
    pub fn replay_pending(&self, conn: &IndexStore) -> usize {
        let mut replayed = 0;
        for supervisor in self.active_supervisors() {
            match supervisor.replay_pending(conn) {
                Ok(count) => replayed += count,
                Err(err) => crate::log_line!(
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
    pub fn ensure_fresh(&self, conn: &IndexStore, file_path: &str) -> Result<Option<StalenessOutcome>> {
        let Some(language) = self.language_for(file_path).map(str::to_string) else {
            return Ok(None);
        };
        // Not in the index at all, so there is nothing to keep fresh.
        if self.is_failed_language(&language) {
            return Ok(None);
        }

        if !conn.with(|conn| staleness::is_stale(conn, &self.project_root, file_path))? {
            return Ok(Some(StalenessOutcome::AlreadyFresh));
        }

        let supervisor = self.get_or_spawn(&language)?;
        supervisor.ensure_fresh(conn, file_path).map(Some)
    }
}

/// `some/dir/go.mod` -> `"go.mod"` - the final path segment, matching the
/// project-relative, forward-slash-joined convention `relative_wire_path`
/// (`daemon::mod`) already produces for every path this module ever sees.
/// A path with no `/` at all (a root-level file) returns itself unchanged.
fn file_name_of(file_path: &str) -> &str {
    file_path.rsplit('/').next().unwrap_or(file_path)
}

#[cfg(test)]
mod tests;
