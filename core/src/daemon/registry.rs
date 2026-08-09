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
//! # Lock order
//!
//! `supervisors` is the *outermost* lock in the daemon. It is taken only by
//! [`get_or_spawn`](PluginRegistry::get_or_spawn), which releases it before
//! its caller touches the supervisor it returns, so the existing order
//! `daemon::lifecycle` documents (supervisor lock first, SQLite connection
//! inside it) continues below this one and nothing ever reaches back up:
//! a supervisor knows nothing about the registry that owns it.
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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::embedding::EmbeddingPipeline;
use crate::watcher::staleness::StalenessOutcome;

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

/// The daemon's plugins: what was discovered, and which of them are running.
pub struct PluginRegistry {
    /// Canonicalized, exactly as [`PluginSupervisor`] wants it - every
    /// supervisor this registry creates is spawned against the same root.
    project_root: PathBuf,
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
    /// language -> its supervisor, filled in lazily by
    /// [`get_or_spawn`](Self::get_or_spawn). A language absent from this map
    /// is one whose files this daemon has not seen yet, not one that failed.
    supervisors: Mutex<HashMap<String, Arc<PluginSupervisor>>>,
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
        discovered: DiscoveredPlugins,
        idle_timeout: Option<Duration>,
        embedding: Arc<EmbeddingPipeline>,
    ) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
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

    /// The supervisor for `language`, spawning its plugin if this is the
    /// first time anything has needed it.
    ///
    /// # Why the map stays locked across the spawn
    ///
    /// Spawning is not free - it is a process launch plus a handshake round
    /// trip, and for a plugin that starts a type checker it is the most
    /// expensive thing the daemon ever does - so holding a daemon-wide lock
    /// across it deserves an argument rather than an assumption.
    ///
    /// The alternative (look, unlock, spawn, relock, insert) makes two
    /// callers racing on the *same* new language spawn two processes, one of
    /// which is then immediately thrown away: either killed - paying its
    /// whole startup cost for nothing, and doing so precisely when the daemon
    /// is busiest - or, if the loser is simply dropped without a
    /// `shutdown`, left running with nothing reading its pipes. A get-or-
    /// insert under one lock cannot produce that state at all.
    ///
    /// What it costs instead is that a caller wanting language B waits behind
    /// a caller starting language A. That is bounded and small: it happens at
    /// most once per language per daemon run (afterwards this is a hash
    /// lookup), the number of languages is the number of installed plugins,
    /// and the wait is the same handshake the waiting caller would have had
    /// to pay for its own plugin anyway. Nothing can deadlock behind it
    /// either - `PluginSupervisor::start` takes no lock this daemon shares
    /// (the supervisor it builds is not reachable by anyone else until this
    /// function returns it), and no supervisor ever calls back into the
    /// registry. A finer-grained scheme - a per-language spawn slot, so two
    /// languages can start concurrently - would buy back only that one
    /// serialized handshake, and is not worth a second lock to reason about
    /// until a real install has enough plugins for it to matter.
    ///
    /// A spawn that *fails* memoizes nothing: the error goes to the caller
    /// and the next file of that language tries again, which is the right
    /// behaviour for a plugin whose runtime is missing or briefly
    /// unavailable.
    pub fn get_or_spawn(&self, language: &str) -> Result<Arc<PluginSupervisor>> {
        let mut supervisors = self.supervisors.lock().unwrap();
        if let Some(running) = supervisors.get(language) {
            return Ok(Arc::clone(running));
        }

        let manifest = self.discovered.manifests.get(language).with_context(|| {
            format!(
                "no plugin was discovered for language \"{language}\" (discovered: {})",
                self.discovered_languages()
            )
        })?;

        let supervisor = PluginSupervisor::start(
            &self.project_root,
            manifest.clone(),
            self.pid_file_for(language)?,
            self.idle_timeout,
            Arc::clone(&self.embedding),
        )?;
        supervisors.insert(language.to_string(), Arc::clone(&supervisor));
        Ok(supervisor)
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
    /// other's record. Resolved at spawn time rather than in
    /// [`new`](Self::new) because that is where its failure has somewhere to
    /// go, and where the state directory is guaranteed to already exist.
    fn pid_file_for(&self, language: &str) -> Result<PathBuf> {
        let state_dir = crate::storage::connection::project_dir(&self.project_root)
            .context("failed to resolve the project's state directory")?;
        Ok(state_dir.join(plugin_pid_file_name(language)))
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
    pub fn active_supervisors(&self) -> Vec<Arc<PluginSupervisor>> {
        self.supervisors.lock().unwrap().values().cloned().collect()
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
    pub fn sleep_all_now(&self, reason: &str) {
        for supervisor in self.active_supervisors() {
            supervisor.sleep_now(reason);
        }
    }

    /// Whether any active supervisor has something queued for its next wake -
    /// the registry's analog of [`PluginSupervisor::has_pending`], cheap
    /// enough to ask on every MCP tool call the same way that one already is.
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
    /// extension - spawning that plugin if this is the first time anything
    /// has needed it, exactly like [`file_changed`](Self::file_changed).
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
        let project = tempfile::tempdir().expect("failed to create a project root");
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
        let registry = PluginRegistry::new(
            project.path(),
            discovered,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        (project, plugins, dirs, registry)
    }

    fn extension_for(language: &str) -> String {
        format!(".{language}-src")
    }

    /// Kills `pid` the way an OOM-killer would - the same out-of-band crash
    /// `core/tests/plugin_crash_recovery.rs` stages, and deliberately not a
    /// cooperative shutdown.
    fn kill_out_of_band(pid: u32) {
        // SAFETY: `kill` only signals a pid this test read off a child of its
        // own process; no pointers are involved.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
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

        let python = registry.pid_file_for("python").unwrap();
        let go = registry.pid_file_for("go").unwrap();
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
