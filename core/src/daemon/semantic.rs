//! The whole-project semantic pass, and the one rule about when it runs: a
//! freshly built index is not finished until every semantic-pass-capable
//! language in it has had one.
//!
//! # Why this module exists
//!
//! The pass itself lives in each language's own plugin (for JS/TS,
//! `plugins/typescript/src/semanticPass.ts`) and reaches core through
//! `watcher::apply::apply_semantic_pass`. What lives here is only *when* it
//! is asked for over a whole project - which used to be one call site inside
//! `daemon::run`'s cold-start branch, and that turned out to be a bug rather
//! than a location.
//!
//! Three commands build a complete index from nothing:
//!
//!   - `daemon::run`'s own cold start, for a project nobody pre-indexed;
//!   - `cli::init`, whose entire promise is that the index is ready when it
//!     returns;
//!   - `cli::reindex`, which rebuilds from scratch on demand.
//!
//! All three finish by recording `meta.bulkIndexedAt`, and the *next* daemon
//! start reads that record to decide it owes no walk - and, when the pass hung
//! off the walk, no semantic pass either. So a project prepared with `init` or
//! repaired with `reindex` kept a permanently structural-only graph: no
//! namespace-import call edges at all (nothing else produces them - see
//! `NamespaceMemberUse` in the TS plugin), re-export chains left unresolved
//! where core's own walk could not finish them, and overloaded calls left
//! collapsed. `find_callers` on a function every caller reaches through
//! `import * as ns from "./mod"` answered nothing, on an index that reported
//! itself complete.
//!
//! Hanging the pass off *whoever built the index* rather than off the daemon
//! alone is what closes that: the three paths above now produce the same graph,
//! which is the only property any of them was ever meant to have.
//!
//! # Per-language, not one hardcoded plugin (GM-270)
//!
//! Before this task, [`run_with_registry`]/[`run_once`] asked exactly one
//! plugin - `plugin::BUNDLED_LANGUAGE` - because that was the only plugin
//! there ever was. GM-269 gave every manifest its own
//! `capabilities.semantic_pass` bit (`daemon::manifest::Capabilities`); this
//! task is what makes the scheduler actually branch on it: both functions
//! below now iterate every language whose manifest declares the capability
//! (`daemon::manifest::semantic_pass_capable_languages`), ask each one for
//! its own pass, and record `language_state.semanticPassAt`
//! (`storage::schema::record_language_semantic_pass`) per language rather
//! than one project-wide flag written off a single plugin's answer. A
//! language whose manifest never declares the capability (the conservative
//! default - see `Capabilities::default`) is never asked, exactly as it was
//! never asked for a per-file pass either (`watcher::apply::apply_file_change`,
//! gated the same way by `daemon::plugin::PluginProcess`, which owns the
//! manifest that decision comes from).
//!
//! **"Owed" is per language.** A language is owed a pass when it is present
//! in the index (at least one `File` node - the same definition GM-264's
//! roll-up uses), its manifest declares `capabilities.semantic_pass = true`,
//! and `language_state.semanticPassAt` is still unset -
//! `storage::schema::owed_semantic_pass_languages` is the one place that
//! definition lives, and both functions below schedule against it rather than
//! against "every capable language" - re-asking a language whose pass already
//! landed would repeat real, possibly expensive work (a cold `rust-analyzer`
//! or `tsserver` load) for no new information. This is also what makes an
//! *interrupted* pass retryable without repeating a language that already
//! succeeded: the next call only ever asks what is still owed, per language,
//! never the whole set again.
//!
//! **Sequential, not concurrent.** Several owed languages are asked one after
//! another, in a fixed (sorted) order, never in parallel. This is a
//! deliberate choice, not a missing optimization: a semantic engine can hold
//! several gigabytes (a cold `rust-analyzer` load over a large workspace,
//! `go/packages` type-checking a big module graph - see the architecture
//! doc's "Memory on large repos" failure mode), and running two of them at
//! once on a developer's machine during a cold start is exactly the kind of
//! sustained overage `[plugin] memoryLimitMb` exists to catch, not something
//! this scheduler should manufacture by racing them. ("Catch", not "prevent":
//! GM-304 settled that the limit is a circuit breaker rather than a ceiling,
//! so it would stop two such engines only *after* both had loaded - see
//! `PluginSupervisor::check_memory_limit`. Which is the stronger reason not
//! to race them here, not a weaker one.) Nothing here would need to
//! change structurally to go concurrent later (each language's request is
//! already independent - a different plugin process, a different
//! `language_state` row), but that is a decision for whenever the memory
//! cost is actually measured against a benefit, not assumed away today.
//!
//! # What `meta.bulkIndexedAt` still does not record
//!
//! That the pass ran. All three callers record the marker *before* asking for
//! the pass, because in the daemon that marker is what gates serving - it means
//! "the structural graph is committed and answers are real", and delaying it
//! behind a checker would put the whole point of the fast layer back to sleep.
//! init and reindex keep the same order so one marker keeps one meaning.
//!
//! The cost is a window: a build interrupted *during* the pass leaves an index
//! that calls itself complete and has no semantic layer, and nothing later goes
//! looking - unless something reads a second, independent fact. That is
//! `meta.semanticPassAt` (`storage::schema::semantic_pass_completed`), a
//! project-wide roll-up over every present, capable language's own
//! `language_state.semanticPassAt` (`storage::schema::
//! reconcile_semantic_pass_rollup`) - set only once every such language has
//! recorded its own completion, so a slow language does not hold a fast one's
//! completion hostage, and a language with no semantic-pass capability at all
//! never blocks the roll-up (see that function's own doc comment).
//!
//! `daemon::run`'s cold start is the one caller that can ask again on its own:
//! a restart against a project whose walk is done (`bulkIndexedAt` set) but
//! whose pass is not (`semanticPassAt` unset) retries the pass right where the
//! cold-start branch would have run it, without repeating the walk - and,
//! per-language, without repeating a language that already finished (see
//! "Owed", above). `cli::init` does the same when a second run finds
//! `already_indexed` true but the pass still unrecorded. `cli::reindex` never
//! needed this: it always ends with the pass regardless of what came before,
//! which is why it stayed the escape hatch this doc used to point to - it
//! still works, it is just no longer the *only* thing that notices.
//!
//! `semanticPassAt` (both the per-language row and the project-wide roll-up)
//! is a one-shot completion flag, not a staleness digest: it answers "has
//! this language's whole-project pass ever finished", once, and is not reset
//! by an ordinary source edit after that. Keeping the semantic layer current
//! as files change is the incremental watcher's job already
//! (`watcher::apply::apply_semantic_pass`, exercised end-to-end by
//! `core/tests/semantic_pass_trigger.rs`'s
//! `core_asks_for_a_semantic_pass_after_each_incremental_reparse`) - every
//! settled reparse gets its own per-file pass over exactly the file that
//! changed, which is strictly finer-grained than re-running the whole-project
//! pass could be. Wiring `semanticPassAt` into that machinery would duplicate
//! a freshness guarantee that already exists; it only has to cover the one
//! thing nothing else does, a completion that was interrupted before it ever
//! happened once. It does reset to `NULL` alongside `bulkIndexedAt` whenever
//! `storage::schema::reset` wipes the project (a schema or indexer-version
//! mismatch, or `cli::reindex`'s unconditional wipe) - that is the same
//! "throw the whole graph away and start over" event `bulkIndexedAt` already
//! resets for, not a second staleness mechanism of its own.
//!
//! # Best-effort, everywhere
//!
//! A pass that cannot run leaves an index that is already committed and
//! serviceable - the state the project was in a moment ago. That is worth
//! reporting and never worth failing over, so both entry points here never
//! fail the caller: one language's failure is captured in the returned
//! [`SemanticPassRun`] and every other language is still asked, and neither
//! entry point is on any command's success path.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;

use crate::daemon::manifest::{self, DiscoveredPlugins};
use crate::daemon::plugin::PluginProcess;
use crate::daemon::registry::{plugin_pid_file_name, PluginRegistry};
use crate::embedding::EmbeddingPipeline;
use crate::storage::schema;

/// How long a one-shot plugin gets to exit on its own before it is killed -
/// the same budget `daemon::lifecycle` gives a supervised one, for the same
/// reason: a plugin that ignores its closed stdin must not hold a command open.
const PLUGIN_EXIT_GRACE: Duration = Duration::from_millis(500);

/// How many `File` nodes `language` has in the index right now - what
/// `plugin::RoundTripTimeouts::semantic_pass_project_timeout` scales that
/// language's whole-project pass timeout against, so a real pass on a real-
/// sized project gets a real budget instead of the flat one this task's
/// review found too short. See that method and `plugin::RoundTripTimeouts`'s
/// own doc comment for the measurement behind the scaling.
///
/// Counted **per language** (`WHERE kind = 'File' AND language = ?`), not
/// over the whole index: GM-271's review left this scaled off every
/// discovered language's files combined, because no per-language `File`-node
/// accounting existed yet, and named GM-270 (this task) as the place to
/// narrow it once that distinction exists - `nodes.language` (GM-264) is that
/// distinction. Narrowing it here means a language's own timeout tracks its
/// own size, never inflated by an unrelated language's files sitting in the
/// same project, and never starved by them either.
///
/// Returns `0` (which `semantic_pass_project_timeout` reads as "use the
/// floor") rather than propagating a query failure - a timeout budget that
/// falls back to its pre-review flat value is a strictly better failure mode
/// here than this whole best-effort pass failing outright over a `COUNT(*)`
/// that could not run.
///
/// `pub(crate)` since GM-272: `daemon::workspace_reindex` needs the exact
/// same per-language file count to scale its own single-language semantic
/// pass's timeout, for the exact same reason - there is no second
/// implementation to keep in sync with this one, just a second caller.
pub(crate) fn indexed_file_count(conn: &Mutex<Connection>, language: &str) -> usize {
    let guard = conn.lock().unwrap();
    let result: rusqlite::Result<i64> = guard.query_row(
        "SELECT COUNT(*) FROM nodes WHERE kind = 'File' AND language = ?1",
        [language],
        |row| row.get(0),
    );
    match result {
        Ok(count) => count.max(0) as usize,
        Err(err) => {
            eprintln!(
                "g-mesh daemon: failed to count {language}'s indexed files for the semantic-pass \
                 timeout ({err:#}) - falling back to the flat floor"
            );
            0
        }
    }
}

/// What asking every owed language for its whole-project semantic pass
/// produced - the per-language generalization of the single `Ok(bool)`
/// [`run_with_registry`]/[`run_once`] returned before GM-270, when there was
/// only ever one language to ask.
///
/// Neither field is populated for a language this run did not ask at all -
/// one that was capable but not owed (see `storage::schema::
/// owed_semantic_pass_languages`, and this module's own doc comment on
/// "Owed"). That is not a third outcome worth recording: a language with
/// nothing owed is exactly as complete, from this run's point of view, as one
/// that completed in an earlier run.
#[derive(Debug, Default)]
pub struct SemanticPassRun {
    /// Languages whose pass actually ran to completion and had their
    /// `language_state.semanticPassAt` recorded by this call.
    pub completed: Vec<String>,
    /// Languages that were asked and did not finish - `(language, error)`,
    /// kept for the caller's own log line. `language_state.semanticPassAt`
    /// stays unset, so the next call (a later daemon start, a later
    /// `init`/`reindex` retry) tries again - see this module's doc comment on
    /// `Err` vs "left unset" - and the error is recorded as that language's
    /// `semanticPassError` for `g-mesh status`.
    pub failed: Vec<(String, anyhow::Error)>,
}

impl SemanticPassRun {
    /// Whether at least one language's pass actually completed this call -
    /// what `cli::init`/`cli::reindex` report as `semantic_pass_ran` in their
    /// own `Outcome`, generalized from the single-language `bool` those types
    /// used to carry directly.
    pub fn any_ran(&self) -> bool {
        !self.completed.is_empty()
    }

    /// Records `language`'s completed pass, which also clears any failure
    /// recorded for it earlier.
    fn record_success(&mut self, conn: &Mutex<Connection>, language: String) {
        let recorded = schema::record_language_semantic_pass(&conn.lock().unwrap(), &language);
        match recorded {
            Ok(()) => self.completed.push(language),
            // The pass ran, but the index does not say so: the language stays
            // owed, and this error is the reason status shows for it.
            Err(err) => self.record_failure(conn, language, err),
        }
    }

    /// Records `language`'s failed pass and its reason in `language_state`.
    fn record_failure(&mut self, conn: &Mutex<Connection>, language: String, err: anyhow::Error) {
        record_failure(conn, &language, &err);
        self.failed.push((language, err));
    }

    /// Logs one line per language this run touched - a completion for every
    /// success, a failure (with its error) for every language still owed one
    /// afterwards. `context` is a short phrase naming which index this pass
    /// followed (e.g. `"the freshly built index"`,
    /// `"the previously-interrupted index"`), reused verbatim by every caller
    /// so the wording stays one sentence per language instead of N
    /// near-identical ones hand-written at each call site.
    pub fn log(&self, context: &str) {
        for language in &self.completed {
            eprintln!("g-mesh: {language} semantic pass over {context} complete");
        }
        for (language, err) in &self.failed {
            eprintln!(
                "g-mesh: the {language} semantic pass over {context} failed ({err:#}) - \
                 its edges keep whatever the structural pass resolved"
            );
        }
    }
}

/// Runs the whole-project pass for every currently-owed language against a
/// registry that already exists, spawning each language's plugin if nothing
/// has needed it yet.
///
/// This is `daemon::run`'s entry point: the daemon owns its registry for the
/// rest of its life, so nothing is torn down here.
///
/// Languages are asked sequentially, in the sorted order
/// `PluginRegistry::semantic_pass_languages`/`storage::schema::
/// owed_semantic_pass_languages` already return - see this module's doc
/// comment ("Sequential, not concurrent") for why.
///
/// A language's own `get_or_spawn`/`PluginSupervisor::semantic_pass` failure
/// is captured into the returned [`SemanticPassRun::failed`] rather than
/// stopping the loop - the whole point of scheduling per language is that one
/// language's trouble (a crash, a timeout, a missing toolchain) must not cost
/// another language its own pass. A supervisor left asleep
/// (`PluginSupervisor::semantic_pass` returning `Ok(false)`) is deliberately
/// neither completed nor in [`SemanticPassRun::failed`]: nothing was actually
/// run, and the language stays owed for whoever asks next - see that method's
/// own doc comment for why this is expected to be unreachable at this call
/// site in practice, not a case worth surfacing as an error. It is still
/// recorded as the language's reason ([`NOT_RUN_REASON`]), so status says why
/// the language is owed rather than "never completed".
pub fn run_with_registry(registry: &PluginRegistry, conn: &Mutex<Connection>) -> SemanticPassRun {
    let capable: HashSet<String> = registry.semantic_pass_languages().into_iter().collect();
    let owed = {
        let guard = conn.lock().unwrap();
        match schema::owed_semantic_pass_languages(&guard, &capable) {
            Ok(owed) => owed,
            Err(err) => {
                eprintln!("g-mesh daemon: failed to determine which languages owe a semantic pass ({err:#})");
                Vec::new()
            }
        }
    };

    let mut run = SemanticPassRun::default();
    for language in owed {
        let file_count = indexed_file_count(conn, &language);
        let outcome = registry
            .get_or_spawn(&language)
            .and_then(|supervisor| supervisor.semantic_pass(conn, Vec::new(), file_count));
        match outcome {
            Ok(true) => run.record_success(conn, language),
            // The supervisor was asleep or suspended and deliberately left
            // that way - see this function's own doc comment. Not a failure:
            // the language stays owed for the next caller, and status says
            // why rather than "never completed".
            Ok(false) => record_not_run(conn, &language),
            Err(err) => run.record_failure(conn, language, err),
        }
    }

    reconcile_rollup(conn, &capable);
    run
}

/// Runs the whole-project pass for every currently-owed language for a
/// command that has no registry and wants none afterwards: one plugin
/// process per language, one pass each, one shutdown each.
///
/// `cli::init` and `cli::reindex` are the callers. Both have already stopped
/// whatever daemon was serving the project, so this is the only thing touching
/// its index and its `plugin-<language>.pid` files for as long as the passes
/// take.
///
/// A `PluginProcess` rather than a [`PluginRegistry`] deliberately, per
/// language: the registry's supervisors exist to keep a plugin *alive*
/// between requests and to log about idling it out, neither of which a
/// one-shot command wants - exactly the same reasoning that has
/// `daemon::bulk_index` spawn its own one-shot process per language rather
/// than borrowing the daemon's.
///
/// Languages are asked sequentially - see this module's doc comment
/// ("Sequential, not concurrent") - and one language's spawn/pass/shutdown
/// failure is captured into [`SemanticPassRun::failed`] rather than aborting
/// the remaining languages, the same as [`run_with_registry`].
pub fn run_once(
    canonical_root: &Path,
    state_dir: &Path,
    conn: &Mutex<Connection>,
    discovered: &DiscoveredPlugins,
    embedding: &EmbeddingPipeline,
) -> SemanticPassRun {
    let capable: HashSet<String> =
        manifest::semantic_pass_capable_languages(&discovered.manifests).into_iter().collect();
    let owed = {
        let guard = conn.lock().unwrap();
        match schema::owed_semantic_pass_languages(&guard, &capable) {
            Ok(owed) => owed,
            Err(err) => {
                eprintln!("g-mesh: failed to determine which languages owe a semantic pass ({err:#})");
                Vec::new()
            }
        }
    };

    let mut run = SemanticPassRun::default();
    for language in owed {
        // Guaranteed present: `owed` only ever names languages `capable`
        // (built from `discovered.manifests` above) already contains.
        let manifest = &discovered.manifests[&language];
        let pid_file = state_dir.join(plugin_pid_file_name(&language));
        let process = match PluginProcess::spawn(canonical_root, manifest, pid_file.clone()) {
            Ok(process) => process,
            Err(err) => {
                let context = format!("failed to start the {language} plugin");
                run.record_failure(conn, language, err.context(context));
                continue;
            }
        };
        // The first pid is the spawner's to record (`PluginProcess::spawn`),
        // and this one is worth recording for exactly as long as the pass
        // runs: on a large project that is minutes during which the only way
        // anything outside this process - `cli::status`, `cli::stop`, a
        // test's teardown - can name the checker holding a project open is
        // this file.
        super::write_pid_file(&pid_file, process.pid());

        let file_count = indexed_file_count(conn, &language);
        let outcome = process.semantic_pass(conn, Vec::new(), file_count, embedding);

        // Shut down whatever the pass did: a plugin left running behind a
        // command that has returned is a checker holding real memory with
        // nothing reading its pipes, and a pid file naming it is the exact
        // shape `cli::stop` and `cli::status` read as a crashed daemon.
        let shutdown = process.shutdown(PLUGIN_EXIT_GRACE);
        let _ = std::fs::remove_file(&pid_file);

        match outcome {
            Ok(()) => run.record_success(conn, language.clone()),
            Err(err) => run.record_failure(conn, language.clone(), err),
        }
        if let Err(err) = shutdown {
            eprintln!("g-mesh: the {language} plugin did not shut down cleanly ({err:#})");
        }
    }

    reconcile_rollup(conn, &capable);
    run
}

/// Persists `err` as `language`'s `semanticPassError`. A failure to write it
/// is logged, never propagated: the pass has already failed, and the index
/// stays serviceable.
pub(crate) fn record_failure(conn: &Mutex<Connection>, language: &str, err: &anyhow::Error) {
    record_reason(conn, language, &format!("{err:#}"));
}

/// The reason recorded for a language whose pass was not run because its
/// plugin was asleep or memory-suspended.
pub(crate) const NOT_RUN_REASON: &str = "not run - its plugin was asleep or memory-suspended; \
     the next daemon start or `g-mesh reindex` asks again";

/// Logs and records that `language`'s pass was not run because its plugin was
/// asleep or memory-suspended. The language stays owed.
pub(crate) fn record_not_run(conn: &Mutex<Connection>, language: &str) {
    eprintln!("g-mesh daemon: the {language} semantic pass was not run - its plugin is asleep or suspended");
    record_reason(conn, language, NOT_RUN_REASON);
}

fn record_reason(conn: &Mutex<Connection>, language: &str, reason: &str) {
    if let Err(write_err) =
        schema::record_language_semantic_pass_failure(&conn.lock().unwrap(), language, reason)
    {
        eprintln!("g-mesh: failed to record why the {language} semantic pass failed ({write_err:#})");
    }
}

/// Shared tail of [`run_with_registry`]/[`run_once`]: re-checks the project-
/// wide `meta.semanticPassAt` roll-up once, after every owed language in this
/// run has been asked.
///
/// Unconditional, even when `run` above completed nothing: a run over an
/// empty `owed` set (every capable language already passed in an earlier
/// run, or no discovered language is capable at all) still has to reconcile,
/// or a project with no semantic-capable plugin would be reported as "pass
/// still owed" by `daemon::mod`'s cold-start retry on every single daemon
/// start, forever - see `storage::schema::reconcile_semantic_pass_rollup`'s
/// own doc comment.
fn reconcile_rollup(conn: &Mutex<Connection>, capable: &HashSet<String>) {
    if let Err(err) = schema::reconcile_semantic_pass_rollup(&conn.lock().unwrap(), capable) {
        eprintln!("g-mesh: failed to update the project-wide semantic-pass roll-up ({err:#})");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};

    use rusqlite::{params, OptionalExtension};

    use super::*;
    use crate::daemon::manifest::discover;
    use crate::daemon::test_plugin;

    /// Guards `SEMANTIC_PASS_PROJECT_TIMEOUT_ENV` the same way
    /// `daemon::plugin`'s own tests guard their env-var overrides: it is
    /// process-wide state, and `cargo test` runs this module on multiple
    /// threads by default.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    /// A `File` node for `language` at `file_path`, so `storage::schema::
    /// present_languages` (and therefore "owed") considers it present - the
    /// same fixture shape `storage::schema`'s own roll-up tests use,
    /// reimplemented here rather than shared across a `pub(crate)` boundary
    /// neither module otherwise needs.
    fn seed_file(conn: &Connection, id: &str, language: &str, file_path: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'File', ?1, ?2, ?2, 0, 0, 0, 0, ?3)",
            params![id, file_path, language],
        )
        .unwrap();
    }

    /// One language's installation shape for the fixtures below: capable
    /// (`[plugin.capabilities] semantic_pass = true`) or not, and stalling
    /// (never answers its first-ever framed request - `test_plugin::
    /// install_stalling`) or not. `install_stalling` predates GM-270's
    /// capability gating and has no capability knob of its own, so the
    /// capable-and-stalling combination writes the fixture via
    /// `install_stalling` and then appends the capability table to its
    /// manifest by hand - exactly the shape a real hand-edited `plugin.toml`
    /// would take.
    fn install_language(
        plugins_root: &std::path::Path,
        language: &str,
        capable: bool,
        stalling: bool,
    ) -> PathBuf {
        let extension = format!(".{language}-src");
        match (capable, stalling) {
            (true, true) => {
                let dir = test_plugin::install_stalling(plugins_root, language, &[extension.as_str()]);
                let manifest_path = dir.join("plugin.toml");
                let mut body = std::fs::read_to_string(&manifest_path).unwrap();
                body.push_str("\n[plugin.capabilities]\nsemantic_pass = true\n");
                std::fs::write(&manifest_path, body).unwrap();
                dir
            }
            (true, false) => {
                test_plugin::install_semantic_pass_capable(plugins_root, language, &[extension.as_str()])
            }
            (false, true) => test_plugin::install_stalling(plugins_root, language, &[extension.as_str()]),
            (false, false) => test_plugin::install(plugins_root, language, &[extension.as_str()]),
        }
    }

    /// A registry over two fake languages, each installed per
    /// [`install_language`], with a `File` node seeded for both so both are
    /// "present" - the fixture every test below builds on. Returns the
    /// tempdirs (dropping either would delete the plugin directories the
    /// registry spawns from, or the project the index is scoped to), each
    /// language's plugin directory, the shared in-memory index, and the
    /// registry itself.
    #[allow(clippy::too_many_arguments)]
    fn two_language_registry(
        first: &str,
        first_capable: bool,
        first_stalling: bool,
        second: &str,
        second_capable: bool,
        second_stalling: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf, Mutex<Connection>, PluginRegistry) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");

        let first_dir = install_language(plugins.path(), first, first_capable, first_stalling);
        let second_dir = install_language(plugins.path(), second, second_capable, second_stalling);

        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifests must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");

        let conn = test_plugin::empty_index();
        {
            let guard = conn.lock().unwrap();
            // `test_plugin::empty_index` only applies the DDL
            // (`schema::apply`) - it never inserts the `meta` row itself, so
            // `reconcile_semantic_pass_rollup`'s `UPDATE meta ... WHERE id =
            // 1` would silently affect no row and `semantic_pass_completed`
            // would read that as "not complete" forever, regardless of what
            // `language_state` says. `ensure_current` is what every real
            // caller runs first (`daemon::run`, `cli::init`, `cli::reindex`)
            // and is what actually creates that row.
            schema::ensure_current(&guard, "test-generation").unwrap();
            seed_file(&guard, "f1", first, &format!("src/a.{first}-src"));
            seed_file(&guard, "f2", second, &format!("src/b.{second}-src"));
        }

        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );

        (project, plugins, first_dir, second_dir, conn, registry)
    }

    /// What `g-mesh status` prints about the semantic pass for this index,
    /// through the same two functions its report is built with.
    fn status_lines(conn: &Mutex<Connection>, registry: &PluginRegistry) -> Vec<String> {
        let guard = conn.lock().unwrap();
        let capable: HashSet<String> = registry.semantic_pass_languages().into_iter().collect();
        let (owed, failures) = crate::cli::status::semantic_pass_state(&guard, &capable).unwrap();
        let completed = schema::semantic_pass_completed(&guard).unwrap();
        crate::cli::status::semantic_pass_lines(completed, &owed, &failures)
    }

    fn semantic_pass_at(conn: &Connection, language: &str) -> Option<String> {
        conn.query_row(
            "SELECT semanticPassAt FROM language_state WHERE language = ?1",
            params![language],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
        .flatten()
    }

    /// The core acceptance criterion: with two languages present, only the
    /// one whose manifest declares `capabilities.semantic_pass = true`
    /// receives a `semanticPass` request at all.
    ///
    /// Discriminates: replace `registry.semantic_pass_languages()` inside
    /// `run_with_registry` with "every discovered language" (e.g. the keys of
    /// `registry`'s own discovered manifests, unfiltered) and the
    /// `assert!(incapable_requests.is_empty())` below fails, because `beta`
    /// would be spawned and asked too.
    #[test]
    fn only_the_semantic_pass_capable_language_receives_the_request() {
        let (_project, _plugins, capable_dir, incapable_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", false, false);

        let run = run_with_registry(&registry, &conn);

        assert_eq!(run.completed, vec!["alpha".to_string()]);
        assert!(run.failed.is_empty(), "{:?}", run.failed);

        let capable_requests = test_plugin::requests(&capable_dir);
        assert!(
            capable_requests.iter().any(|line| line.starts_with("semanticPass")),
            "the capable language must have been asked: {capable_requests:?}"
        );

        assert!(
            test_plugin::spawns(&incapable_dir).is_empty(),
            "a plugin with no semantic_pass capability must never even be spawned for a pass"
        );
        let incapable_requests = test_plugin::requests(&incapable_dir);
        assert!(
            incapable_requests.is_empty(),
            "a plugin with no semantic_pass capability must never receive the request: \
             {incapable_requests:?}"
        );
    }

    /// Each capable, present language gets its own `language_state.
    /// semanticPassAt` - not a single project-wide flag standing in for both.
    #[test]
    fn two_capable_languages_each_get_their_own_semantic_pass_at() {
        let (_project, _plugins, _first_dir, _second_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", true, false);

        let run = run_with_registry(&registry, &conn);

        let mut completed = run.completed.clone();
        completed.sort();
        assert_eq!(completed, vec!["alpha".to_string(), "beta".to_string()], "{:?}", run.failed);

        let guard = conn.lock().unwrap();
        let alpha_at = semantic_pass_at(&guard, "alpha").expect("alpha's own row must be set");
        let beta_at = semantic_pass_at(&guard, "beta").expect("beta's own row must be set");
        // Both rows are real, non-empty timestamps a caller could tell apart -
        // not, say, one write mistakenly shared between the two languages.
        assert!(!alpha_at.is_empty());
        assert!(!beta_at.is_empty());
    }

    /// GM-270's other acceptance criterion: a pass interrupted for one
    /// language is retried, on the next call, without re-running a language
    /// that already succeeded.
    ///
    /// `alpha` is installed stalling - it answers its handshake normally,
    /// then never answers the very first framed request it ever receives
    /// (`test_plugin::install_stalling`'s own doc comment), which for a
    /// language with nothing else asked of it yet is exactly the
    /// `semanticPass` request this test sends. The
    /// `SEMANTIC_PASS_PROJECT_TIMEOUT_ENV` override here does not shorten the
    /// wait to its own value - with one `File` node present, `plugin::
    /// RoundTripTimeouts::semantic_pass_project_timeout` clamps to
    /// `max(floor, 1 * SEMANTIC_PASS_PER_FILE_BUDGET)`, and the 10s per-file
    /// budget is not itself overridable (see that type's own doc comment).
    /// What the override actually buys is dropping the *floor* from its
    /// 20-minute default down to (here) 150ms, so the 10s scaled value wins
    /// the `max` instead of the floor - turning a 20-minute wait into a
    /// bounded ~10s one, not a fast one.
    #[test]
    fn an_interrupted_pass_for_one_language_is_retried_without_rerunning_the_other() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(crate::daemon::plugin::SEMANTIC_PASS_PROJECT_TIMEOUT_ENV, "150");

        let (_project, _plugins, _alpha_dir, beta_dir, conn, registry) =
            two_language_registry("alpha", true, true, "beta", true, false);

        // `owed_semantic_pass_languages` sorts, so "alpha" is always asked
        // before "beta" within one sequential run - see that function's own
        // doc comment.
        let first_run = run_with_registry(&registry, &conn);
        // The override only has to be visible while `PluginProcess::spawn`
        // resolves it - every supervisor this registry creates captures its
        // own `RoundTripTimeouts` by value at spawn time, so removing it now
        // cannot affect a supervisor already spawned, and leaving it set any
        // longer than necessary would risk another test racing on this same
        // lock seeing it.
        std::env::remove_var(crate::daemon::plugin::SEMANTIC_PASS_PROJECT_TIMEOUT_ENV);

        assert_eq!(first_run.completed, vec!["beta".to_string()], "beta must have completed normally");
        assert_eq!(
            first_run.failed.iter().map(|(language, _)| language.clone()).collect::<Vec<_>>(),
            vec!["alpha".to_string()],
            "alpha must be reported as failed (timed out), not silently dropped"
        );

        {
            let guard = conn.lock().unwrap();
            assert!(
                semantic_pass_at(&guard, "alpha").is_none(),
                "alpha's row must stay unset after a failure"
            );
            assert!(semantic_pass_at(&guard, "beta").is_some(), "beta's row must already be set");
            let failures = schema::semantic_pass_failures(&guard).unwrap();
            assert_eq!(failures.len(), 1, "{failures:?}");
            assert_eq!(failures[0].0, "alpha");
            assert!(
                failures[0].1.contains("semanticPass"),
                "the timeout is recorded as the reason: {failures:?}"
            );
        }
        let lines = status_lines(&conn, &registry);
        assert!(
            lines.iter().any(|line| line.starts_with("  semantic pass:   alpha failed - ")
                && line.contains("semanticPass")),
            "status names the language and the reason: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("never completed")),
            "a recorded failure replaces the generic advice: {lines:?}"
        );
        assert_eq!(
            test_plugin::spawns(&beta_dir).len(),
            1,
            "beta must have been spawned exactly once by the first run"
        );

        // The retry: alpha's fixture answers normally after its one stall
        // (`install_stalling`'s "exactly one request, ever" contract), so a
        // second call must complete it - and must not touch beta again at
        // all, because beta is no longer owed.
        let second_run = run_with_registry(&registry, &conn);

        assert_eq!(second_run.completed, vec!["alpha".to_string()], "{:?}", second_run.failed);
        assert!(second_run.failed.is_empty(), "{:?}", second_run.failed);
        {
            let guard = conn.lock().unwrap();
            assert!(
                semantic_pass_at(&guard, "alpha").is_some(),
                "alpha must be recorded complete after the retry"
            );
        }
        assert_eq!(
            test_plugin::spawns(&beta_dir).len(),
            1,
            "beta must not have been spawned again - it was not owed a second pass"
        );
        assert!(
            schema::semantic_pass_completed(&conn.lock().unwrap()).unwrap(),
            "both languages have now completed - the project-wide roll-up must fire"
        );
        assert!(
            schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap().is_empty(),
            "the successful retry clears alpha's recorded failure"
        );
        assert_eq!(status_lines(&conn, &registry), vec!["  semantic pass:   complete".to_string()]);
    }

    /// A plugin whose language server errored answers the pass `incomplete`
    /// with a reason. The reason is recorded for that language and status
    /// shows it; the next pass that completes clears it.
    #[test]
    fn an_incomplete_pass_records_the_plugins_reason_until_a_pass_completes() {
        const REASON: &str = "the language server refused a question about src/a.alpha-src (internal error)";
        let (_project, plugins, _alpha_dir, _beta_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", false, false);
        test_plugin::install_incomplete_once(plugins.path(), "alpha", &[".alpha-src"], Some(REASON));

        let first_run = run_with_registry(&registry, &conn);
        assert!(first_run.completed.is_empty(), "{:?}", first_run.completed);
        assert_eq!(
            first_run.failed.iter().map(|(language, _)| language.as_str()).collect::<Vec<_>>(),
            vec!["alpha"]
        );
        {
            let guard = conn.lock().unwrap();
            assert!(semantic_pass_at(&guard, "alpha").is_none(), "an incomplete pass is not a completed one");
            let failures = schema::semantic_pass_failures(&guard).unwrap();
            assert_eq!(failures.len(), 1, "{failures:?}");
            assert!(failures[0].1.contains(REASON), "the plugin's own reason is recorded: {failures:?}");
        }
        let lines = status_lines(&conn, &registry);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("  semantic pass:   alpha failed - "), "{lines:?}");
        assert!(lines[0].contains(REASON), "{lines:?}");

        let second_run = run_with_registry(&registry, &conn);
        assert_eq!(second_run.completed, vec!["alpha".to_string()], "{:?}", second_run.failed);
        assert!(schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap().is_empty());
        assert_eq!(status_lines(&conn, &registry), vec!["  semantic pass:   complete".to_string()]);
    }

    /// A plugin that answers incomplete without saying why still leaves a
    /// reason behind: core's own, naming the plugin as the one that gave
    /// none.
    #[test]
    fn an_incomplete_pass_with_no_reason_records_cores_own() {
        let (_project, plugins, _alpha_dir, _beta_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", false, false);
        test_plugin::install_incomplete_once(plugins.path(), "alpha", &[".alpha-src"], None);

        let run = run_with_registry(&registry, &conn);
        assert!(run.completed.is_empty(), "{:?}", run.completed);
        let failures = schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, "alpha");
        assert!(failures[0].1.contains("the plugin gave no reason"), "{failures:?}");
    }

    /// A plugin that is asleep when its pass comes up is not woken, so the
    /// pass does not run - and status says so instead of "never completed".
    #[test]
    fn a_pass_not_run_because_the_plugin_is_asleep_is_recorded_with_its_reason() {
        let (_project, _plugins, _alpha_dir, _beta_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", false, false);
        registry
            .get_or_spawn("alpha")
            .expect("the fixture plugin spawns")
            .sleep_now("the test put it to sleep");

        let run = run_with_registry(&registry, &conn);
        assert!(run.completed.is_empty(), "{:?}", run.completed);
        assert!(run.failed.is_empty(), "a pass that did not run is not a failed one: {:?}", run.failed);
        {
            let guard = conn.lock().unwrap();
            assert!(semantic_pass_at(&guard, "alpha").is_none(), "the language stays owed");
            let failures = schema::semantic_pass_failures(&guard).unwrap();
            assert_eq!(failures, vec![("alpha".to_string(), NOT_RUN_REASON.to_string())]);
        }
        let lines = status_lines(&conn, &registry);
        assert!(!lines.iter().any(|line| line.contains("never completed")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("asleep or memory-suspended")), "{lines:?}");
    }

    /// A pass that ran but whose completion could not be written is recorded
    /// as a failure with the write's error, not lost.
    #[test]
    fn a_completion_that_cannot_be_written_is_recorded_as_the_reason() {
        let (_project, _plugins, _alpha_dir, _beta_dir, conn, registry) =
            two_language_registry("alpha", true, false, "beta", false, false);
        // Refuses exactly the write that sets `semanticPassAt`, and nothing
        // else - the failure's own write sets only `semanticPassError`.
        conn.lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_completion_insert BEFORE INSERT ON language_state
                 WHEN NEW.semanticPassAt IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'the disk is full'); END;
                 CREATE TRIGGER refuse_completion_update BEFORE UPDATE OF semanticPassAt ON language_state
                 WHEN NEW.semanticPassAt IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'the disk is full'); END;",
            )
            .unwrap();

        let run = run_with_registry(&registry, &conn);
        assert!(run.completed.is_empty(), "{:?}", run.completed);
        assert_eq!(run.failed.len(), 1, "{:?}", run.failed);
        let failures = schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, "alpha");
        assert!(failures[0].1.contains("the disk is full"), "{failures:?}");
    }
}
