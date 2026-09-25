//! The semantic tier: the engine trait, and the machinery that guarantees it
//! is not started until core asks a semantic question.
//!
//! # Why laziness is a contract and not an optimization
//!
//! A semantic engine is the expensive half of a plugin - `go/packages` over a
//! large repo and a cold `rust-analyzer` are gigabytes and minutes. Core
//! deliberately does not send `semanticPass` to a plugin it woke for
//! structural work only, and to a language it has suspended for blowing
//! `[plugin] memoryLimitMb` it never sends one at all. Both only work if a
//! plugin that is *not asked* does *not start*: a plugin that loads its
//! engine at startup turns "structural work only" into "load the engine
//! anyway", and turns the memory-limit suspension into a wake → load →
//! over-limit loop, which is exactly the failure the limit was added to
//! break.
//!
//! So `capabilities.semantic-engine-lazy` is a conformance check, and the SDK
//! is built so an SDK plugin cannot fail it by accident.
//!
//! # Why [`run`](crate::run) takes a factory and not an engine
//!
//! The design sketch in `docs/architecture/multi-language-plugins.md` has
//! `run(extractor, semantic: Option<Box<dyn SemanticEngine>>)`. That
//! signature cannot keep the promise above: constructing the `Box` is the
//! plugin's `main` spawning rust-analyzer, and by the time `run` is called
//! the engine is already running. The argument is therefore a
//! [`SemanticEngineFactory`] - a `FnOnce` the SDK calls at the moment the
//! first `semanticPass` arrives, and never before. A plugin author can still
//! do the wrong thing (start a server in `main` and have the closure capture
//! it), but they have to mean it, and the conformance kit will say so.
//!
//! # The marker
//!
//! `g-mesh plugins check` cannot see a plugin's memory, only its
//! filesystem, so the contract it checks is a file: when
//! [`MARKER_DIR_ENV`] is set and non-empty, a plugin appends to
//! `<dir>/`[`SEMANTIC_ENGINE_MARKER`] at the moment it *starts* its semantic
//! engine, and at no other moment. The kit records whether that file existed
//! when it wrote the first `semanticPass` frame; if it did, the engine was
//! started for structural work and the check fails.
//!
//! A plugin that never writes the marker is not failed - it is reported "not
//! instrumented", because the absence of a file is no evidence of anything.
//! Which is the real reason this lives in the SDK: every SDK plugin is
//! instrumented for free, so every SDK plugin's report says `PASS` where it
//! would otherwise say `SKIP`, and a regression in some later language's
//! plugin is a failing check rather than a check that never ran.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use g_mesh_wire::FileChangeDiff;

use crate::index::SdkIndex;
use crate::path::RelPath;

/// The directory `g-mesh plugins check` hands every plugin process it spawns
/// for the markers it defines.
///
/// Must match core's `cli::plugin_check::session::MARKER_DIR_ENV`. Spelled
/// out here rather than imported because this crate deliberately does not
/// depend on core; the conformance run itself is what catches a divergence -
/// the lazy check would report "not instrumented" instead of passing.
pub const MARKER_DIR_ENV: &str = "G_MESH_PLUGIN_CHECK_MARKER_DIR";

/// The marker file's name - see [`MARKER_DIR_ENV`]. Must match core's
/// `cli::plugin_check::session::SEMANTIC_ENGINE_MARKER`.
pub const SEMANTIC_ENGINE_MARKER: &str = "semantic-engine-started";

/// One semantic pass's answer: the diff, and whether the pass actually
/// covered what it was asked about.
///
/// # Why completeness is a separate field and not an `Err`
///
/// The two are different facts and core does different things with them. A
/// diff is committed; completeness decides whether
/// `language_state.semanticPassAt` is recorded
/// (`core::daemon::semantic`), which is what the MCP instructions read to
/// decide whether to keep listing that language's receiver-call gap. A pass
/// that answered nine thousand sites and ran out of budget before the last
/// hundred has *both* things to say, and an `Err` can only say the second:
/// core drops the diff of a failing pass entirely, so nine thousand real
/// upgrades would be thrown away to report that a hundred are missing.
///
/// So an incomplete pass still carries its diff, and core applies it and
/// leaves `semanticPassAt` unset - the next daemon start asks again (the
/// pass is only ever retried per daemon start, and only while still owed),
/// re-sends the same content-derived ids, and upserts them in place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticAnswer {
    /// What to upsert and retract - see [`SemanticEngine::answer`].
    pub diff: FileChangeDiff,
    /// `true` when every question this pass was asked was actually put to the
    /// engine and answered (or answered "nothing there", which is an answer).
    /// `false` when a budget ran out, a server died, or the engine never
    /// became ready - anything that leaves a question unasked.
    ///
    /// Default is `false`, deliberately: a value that has not been thought
    /// about must not claim completeness. [`SemanticAnswer::complete`] is how
    /// an engine that always finishes says so.
    pub complete: bool,
    /// Why an incomplete pass did not cover everything, in words - sent to
    /// core beside the `incomplete` flag, which records it per language and
    /// shows it in `g-mesh status`. `None` on a complete pass.
    pub reason: Option<String>,
}

impl SemanticAnswer {
    /// A pass that covered everything it was asked about.
    pub fn complete(diff: FileChangeDiff) -> Self {
        Self { diff, complete: true, reason: None }
    }

    /// A pass that did not - the diff is whatever it did manage.
    pub fn incomplete(diff: FileChangeDiff) -> Self {
        Self { diff, complete: false, reason: None }
    }

    /// [`SemanticAnswer::incomplete`], saying why.
    pub fn incomplete_because(diff: FileChangeDiff, reason: impl Into<String>) -> Self {
        Self { diff, complete: false, reason: Some(reason.into()) }
    }
}

/// A plugin's semantic tier: whatever can answer what the structural pass
/// could only leave open.
///
/// Implemented by an LSP bridge ([`crate::lsp::LspBridge`], GM-289), by an
/// in-process type checker, or by anything else that can turn an
/// [`OpenSite`](crate::OpenSite) into a target. The SDK owns when it is asked
/// and what happens to the answer; the engine owns only the answering.
pub trait SemanticEngine: Send {
    /// Answers for `files` - **empty means the whole project**, which is the
    /// wire's own convention for the pass that follows the cold-start walk
    /// (`ControlMessage::SemanticPass`); a one-entry list is a reparse that
    /// just settled.
    ///
    /// The answer is a diff in the same shape `fileChanged` answers with,
    /// because a semantic upgrade *is* a diff: an edge re-sent under the id
    /// the structural pass already gave it, with `source: semantic` and a
    /// real target, is upserted in place by core. Unlike a structural diff it
    /// may cross files - the design states the "edges never leave their file"
    /// invariant for the structural stream specifically - so an answer may
    /// carry the target node along with the edge.
    ///
    /// An `Err` is reported and turned into an empty *incomplete* answer, not
    /// propagated: core drops a failing semantic pass by design, since it is
    /// an upgrade over a graph that is already committed and serviceable.
    /// Answering here reaches the same outcome without making core parse a
    /// failure first. Prefer returning [`SemanticAnswer::incomplete`] with
    /// whatever was resolved before the trouble over an `Err` that discards
    /// it; an `Err` is for "this pass produced nothing usable".
    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<SemanticAnswer>;
}

/// Builds the semantic engine, called at most once and only on the first
/// `semanticPass` - see this module's doc for why this is a factory.
///
/// The argument is the project root, absolute, exactly as
/// [`Extractor::load_project`](crate::Extractor::load_project) is given it.
/// An engine that drives a language server needs it for `initialize`'s
/// `rootUri`, and it is the SDK that knows it: core passes the root in argv
/// and [`run`](crate::run) is what parses argv, so a factory built in a
/// plugin's `main` would otherwise have to re-derive it by repeating that
/// parsing rule - the kind of duplication that stays right until the day core
/// adds an argument.
pub type SemanticEngineFactory = Box<dyn FnOnce(&Path) -> Result<Box<dyn SemanticEngine>> + Send>;

/// Holds the factory until it is needed, then the engine.
pub(crate) struct LazyEngine {
    language: String,
    factory: Option<SemanticEngineFactory>,
    engine: Option<Box<dyn SemanticEngine>>,
    /// Set when the factory returned an `Err`. The factory is `FnOnce`, so a
    /// failed start cannot be retried anyway - and retrying an engine whose
    /// binary is missing once per edit would log the same failure forever.
    /// One report, then structural-only for this process's lifetime, which is
    /// exactly the "semantic engine missing" failure mode the design
    /// describes.
    failed: bool,
    /// Why the factory failed, reported as the reason of every pass after it.
    start_failure: Option<String>,
}

impl LazyEngine {
    pub(crate) fn new(language: &str, factory: Option<SemanticEngineFactory>) -> Self {
        Self { language: language.to_string(), factory, engine: None, failed: false, start_failure: None }
    }

    /// Answers a `semanticPass`, starting the engine if this is the first
    /// one. Never fails: see [`SemanticEngine::answer`] on why an answer is
    /// the right shape for every failure here.
    ///
    /// # What each failure says about completeness
    ///
    /// - **No factory at all** - a plugin with no semantic tier, whose
    ///   manifest should have said `semantic_pass = false` and which core
    ///   should therefore never have asked. Reported *complete*: there is no
    ///   engine that could ever make this pass cover more, so leaving the
    ///   language permanently owed a pass would be a retry loop with no
    ///   possible end.
    /// - **The factory failed** - the "semantic engine missing" failure mode
    ///   (no `rust-analyzer`, no `go` on `PATH`). Reported *incomplete*, which
    ///   is what keeps `semanticPassAt` unset and the receiver gap listed for
    ///   that language, exactly as the design doc's failure-mode table
    ///   specifies. Installing the toolchain and restarting the daemon is then
    ///   enough to get the pass - a completed-but-empty pass would have
    ///   recorded "done" and never asked again.
    /// - **The engine returned `Err`** - it produced nothing usable. Empty
    ///   diff, incomplete.
    pub(crate) fn answer(&mut self, files: &[RelPath], index: &SdkIndex, root: &Path) -> SemanticAnswer {
        if self.engine.is_none() && !self.failed {
            let Some(factory) = self.factory.take() else {
                return SemanticAnswer::complete(FileChangeDiff::default());
            };
            // Before the factory runs, not after: the marker records the
            // *attempt* to start. A factory that spawns a server and then
            // fails its handshake has still started a process, and a marker
            // written only on success would hide exactly that.
            write_semantic_engine_marker(&self.language);
            match factory(root) {
                Ok(engine) => self.engine = Some(engine),
                Err(err) => {
                    self.failed = true;
                    self.start_failure = Some(format!("the semantic engine could not be started: {err:#}"));
                    eprintln!(
                        "[{}] the semantic engine could not be started ({err:#}) - answering structurally \
                         only for the rest of this process's life",
                        self.language
                    );
                }
            }
        }

        let Some(engine) = self.engine.as_mut() else {
            let reason = self
                .start_failure
                .clone()
                .unwrap_or_else(|| "the semantic engine could not be started".to_string());
            return SemanticAnswer::incomplete_because(FileChangeDiff::default(), reason);
        };
        match engine.answer(files, index) {
            Ok(answer) => answer,
            Err(err) => {
                eprintln!(
                    "[{}] the semantic pass failed ({err:#}) - answering with an empty, incomplete diff",
                    self.language
                );
                SemanticAnswer::incomplete_because(
                    FileChangeDiff::default(),
                    format!("the semantic pass failed: {err:#}"),
                )
            }
        }
    }

    /// Whether an engine has been started - for the startup log and for
    /// tests.
    pub(crate) fn started(&self) -> bool {
        self.engine.is_some() || self.failed
    }

    /// Whether this plugin has a semantic tier at all.
    ///
    /// Asked *before* [`LazyEngine::answer`], to decide whether the work of
    /// filling the index for the engine to read is worth doing at all - and
    /// deliberately answerable without starting anything, which is the whole
    /// difference between this and [`LazyEngine::started`].
    pub(crate) fn configured(&self) -> bool {
        self.factory.is_some() || self.engine.is_some()
    }
}

/// Appends this process's pid to the conformance kit's semantic-engine
/// marker, if the kit is what spawned us. A no-op otherwise, which is every
/// real run.
///
/// Best-effort: a marker that cannot be written costs the lazy check its
/// evidence (it reports "not instrumented"), and nothing else. Failing a
/// plugin's semantic pass because a test directory was read-only would be a
/// worse trade by a wide margin.
pub fn write_semantic_engine_marker(language: &str) {
    let Some(dir) = std::env::var_os(MARKER_DIR_ENV).filter(|dir| !dir.is_empty()) else { return };
    let path = PathBuf::from(dir).join(SEMANTIC_ENGINE_MARKER);
    if let Err(err) = append_pid(&path) {
        eprintln!("[{language}] could not write the semantic-engine marker {}: {err:#}", path.display());
    }
}

fn append_pid(path: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{}", std::process::id()).context("failed to append")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counting {
        answers: Arc<AtomicUsize>,
    }

    impl SemanticEngine for Counting {
        fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> Result<SemanticAnswer> {
            self.answers.fetch_add(1, Ordering::SeqCst);
            Ok(SemanticAnswer::complete(FileChangeDiff::default()))
        }
    }

    fn root() -> &'static Path {
        Path::new("/nowhere")
    }

    /// The whole point of the type: constructing the engine is deferred until
    /// something asks a semantic question.
    #[test]
    fn the_factory_does_not_run_until_the_first_pass() {
        let starts = Arc::new(AtomicUsize::new(0));
        let answers = Arc::new(AtomicUsize::new(0));
        let (started, answered) = (Arc::clone(&starts), Arc::clone(&answers));
        let mut lazy = LazyEngine::new(
            "toy",
            Some(Box::new(move |_root| {
                started.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Counting { answers: answered }) as Box<dyn SemanticEngine>)
            })),
        );

        assert_eq!(starts.load(Ordering::SeqCst), 0, "constructing LazyEngine must start nothing");
        assert!(!lazy.started());

        lazy.answer(&[], &SdkIndex::new(), root());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(answers.load(Ordering::SeqCst), 1);
        assert!(lazy.started());

        // And exactly once, however many passes follow.
        lazy.answer(&[], &SdkIndex::new(), root());
        lazy.answer(&[RelPath::new("a.toy")], &SdkIndex::new(), root());
        assert_eq!(starts.load(Ordering::SeqCst), 1, "the engine must be started once, not per pass");
        assert_eq!(answers.load(Ordering::SeqCst), 3);
    }

    /// The factory is handed the project root, so an engine that needs one
    /// does not have to re-derive it from argv.
    #[test]
    fn the_factory_is_given_the_project_root() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let recorded = Arc::clone(&seen);
        let mut lazy = LazyEngine::new(
            "toy",
            Some(Box::new(move |root: &Path| {
                *recorded.lock().unwrap() = Some(root.to_path_buf());
                Ok(Box::new(Counting { answers: Arc::new(AtomicUsize::new(0)) }) as Box<dyn SemanticEngine>)
            })),
        );
        lazy.answer(&[], &SdkIndex::new(), Path::new("/projects/thing"));
        assert_eq!(seen.lock().unwrap().as_deref(), Some(Path::new("/projects/thing")));
    }

    /// A plugin with no semantic tier at all is *complete*: nothing it could
    /// be asked again would ever answer more - see [`LazyEngine::answer`].
    #[test]
    fn a_plugin_with_no_engine_answers_empty_and_never_claims_to_have_started_one() {
        let mut lazy = LazyEngine::new("toy", None);
        let answer = lazy.answer(&[], &SdkIndex::new(), root());
        assert_eq!(answer.diff, FileChangeDiff::default());
        assert!(answer.complete, "a plugin that will never have an engine must not stay owed a pass");
        assert!(!lazy.started());
    }

    /// An engine that cannot start is reported once and then stays out of the
    /// way - it must not be retried on every later pass. The pass is reported
    /// *incomplete* every time, which is what keeps `semanticPassAt` unset
    /// and the language's receiver gap listed until the toolchain is there.
    #[test]
    fn a_failed_start_degrades_to_structural_rather_than_retrying_forever() {
        let starts = Arc::new(AtomicUsize::new(0));
        let attempted = Arc::clone(&starts);
        let mut lazy = LazyEngine::new(
            "toy",
            Some(Box::new(move |_root| {
                attempted.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("no language server on PATH")
            })),
        );

        for _ in 0..3 {
            let answer = lazy.answer(&[], &SdkIndex::new(), root());
            assert_eq!(answer.diff, FileChangeDiff::default());
            assert!(!answer.complete, "an engine that never started has not completed a pass");
            let reason = answer.reason.expect("every pass after a failed start says why");
            assert!(reason.contains("no language server on PATH"), "{reason}");
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(lazy.started(), "a failed start still counts as started - it must not be retried");
    }

    struct Failing;

    impl SemanticEngine for Failing {
        fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> Result<SemanticAnswer> {
            anyhow::bail!("the server timed out")
        }
    }

    #[test]
    fn a_failing_pass_answers_an_empty_incomplete_diff_rather_than_an_error() {
        let mut lazy =
            LazyEngine::new("toy", Some(Box::new(|_root| Ok(Box::new(Failing) as Box<dyn SemanticEngine>))));
        let answer = lazy.answer(&[], &SdkIndex::new(), root());
        assert_eq!(answer.diff, FileChangeDiff::default());
        assert!(!answer.complete);
        let reason = answer.reason.expect("a failed pass says why");
        assert!(reason.contains("the server timed out"), "{reason}");
    }
}
