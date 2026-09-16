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

use std::path::PathBuf;

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

/// A plugin's semantic tier: whatever can answer what the structural pass
/// could only leave open.
///
/// Implemented by an LSP bridge (GM-289), by an in-process type checker, or
/// by anything else that can turn an [`OpenSite`](crate::OpenSite) into a
/// target. The SDK owns when it is asked and what happens to the answer; the
/// engine owns only the answering.
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
    /// An `Err` is reported and turned into an empty diff, not propagated:
    /// core drops a failing semantic pass by design, since it is an upgrade
    /// over a graph that is already committed and serviceable. Answering with
    /// an empty diff reaches the same outcome without making core parse a
    /// failure first, and is what a plugin whose engine is simply not
    /// installed does (no `go` binary, no `rust-analyzer`) - the receiver gap
    /// stays in the MCP instructions for that language, which is honest.
    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<FileChangeDiff>;
}

/// Builds the semantic engine, called at most once and only on the first
/// `semanticPass` - see this module's doc for why this is a factory.
pub type SemanticEngineFactory = Box<dyn FnOnce() -> Result<Box<dyn SemanticEngine>> + Send>;

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
}

impl LazyEngine {
    pub(crate) fn new(language: &str, factory: Option<SemanticEngineFactory>) -> Self {
        Self { language: language.to_string(), factory, engine: None, failed: false }
    }

    /// Answers a `semanticPass`, starting the engine if this is the first
    /// one. Never fails: see [`SemanticEngine::answer`] on why an empty diff
    /// is the right shape for every failure here.
    pub(crate) fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> FileChangeDiff {
        if self.engine.is_none() && !self.failed {
            let Some(factory) = self.factory.take() else {
                // No semantic tier at all. The manifest should say
                // `semantic_pass = false`, in which case core never sends this
                // - but answering an empty diff costs nothing and is what a
                // plugin whose manifest overclaims should do.
                return FileChangeDiff::default();
            };
            // Before the factory runs, not after: the marker records the
            // *attempt* to start. A factory that spawns a server and then
            // fails its handshake has still started a process, and a marker
            // written only on success would hide exactly that.
            write_semantic_engine_marker(&self.language);
            match factory() {
                Ok(engine) => self.engine = Some(engine),
                Err(err) => {
                    self.failed = true;
                    eprintln!(
                        "[{}] the semantic engine could not be started ({err:#}) - answering structurally \
                         only for the rest of this process's life",
                        self.language
                    );
                }
            }
        }

        let Some(engine) = self.engine.as_mut() else { return FileChangeDiff::default() };
        match engine.answer(files, index) {
            Ok(diff) => diff,
            Err(err) => {
                eprintln!(
                    "[{}] the semantic pass failed ({err:#}) - answering with an empty diff",
                    self.language
                );
                FileChangeDiff::default()
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
        fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> Result<FileChangeDiff> {
            self.answers.fetch_add(1, Ordering::SeqCst);
            Ok(FileChangeDiff::default())
        }
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
            Some(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Counting { answers: answered }) as Box<dyn SemanticEngine>)
            })),
        );

        assert_eq!(starts.load(Ordering::SeqCst), 0, "constructing LazyEngine must start nothing");
        assert!(!lazy.started());

        lazy.answer(&[], &SdkIndex::new());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(answers.load(Ordering::SeqCst), 1);
        assert!(lazy.started());

        // And exactly once, however many passes follow.
        lazy.answer(&[], &SdkIndex::new());
        lazy.answer(&[RelPath::new("a.toy")], &SdkIndex::new());
        assert_eq!(starts.load(Ordering::SeqCst), 1, "the engine must be started once, not per pass");
        assert_eq!(answers.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_plugin_with_no_engine_answers_empty_and_never_claims_to_have_started_one() {
        let mut lazy = LazyEngine::new("toy", None);
        let diff = lazy.answer(&[], &SdkIndex::new());
        assert_eq!(diff, FileChangeDiff::default());
        assert!(!lazy.started());
    }

    /// An engine that cannot start is reported once and then stays out of the
    /// way - it must not be retried on every later pass.
    #[test]
    fn a_failed_start_degrades_to_structural_rather_than_retrying_forever() {
        let starts = Arc::new(AtomicUsize::new(0));
        let attempted = Arc::clone(&starts);
        let mut lazy = LazyEngine::new(
            "toy",
            Some(Box::new(move || {
                attempted.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("no rust-analyzer on PATH")
            })),
        );

        for _ in 0..3 {
            assert_eq!(lazy.answer(&[], &SdkIndex::new()), FileChangeDiff::default());
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(lazy.started(), "a failed start still counts as started - it must not be retried");
    }

    struct Failing;

    impl SemanticEngine for Failing {
        fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> Result<FileChangeDiff> {
            anyhow::bail!("the server timed out")
        }
    }

    #[test]
    fn a_failing_pass_answers_an_empty_diff_rather_than_an_error() {
        let mut lazy =
            LazyEngine::new("toy", Some(Box::new(|| Ok(Box::new(Failing) as Box<dyn SemanticEngine>))));
        assert_eq!(lazy.answer(&[], &SdkIndex::new()), FileChangeDiff::default());
    }
}
