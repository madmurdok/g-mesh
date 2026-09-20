//! `g-mesh plugins check <plugin-dir> --fixture <project-dir>`: the
//! conformance kit - GM-276, piece 3 of `docs/architecture/multi-language-plugins.md`.
//!
//! Runs any plugin binary against a fixture project the way the daemon runs
//! it, through the daemon's own apply and link code, and reports whether it
//! keeps the contract core depends on: shape, stream order, the same-file
//! rule, id stability, per-file ownership, and its declared capabilities.
//! The rules and the evidence each one reads are in `checks`' module doc; how
//! the plugin is driven, and where that deliberately differs from the daemon,
//! is in `session`'s.
//!
//! # Why this exists
//!
//! Until now the only thing standing between a plugin and core was
//! `protocol::conformance`, which checks JSON *shape*. Everything core
//! actually relies on - that an edge never leaves its file, that ids survive a
//! reparse, that a diff only touches its own file - was held up by the one
//! plugin that exists being written carefully, and by TS integration tests
//! that happen to cover it. Go and Rust plugins are next (and a Go one in a
//! different implementation language), so the contract has to be a thing a
//! plugin can be run against, not a thing its author has to have read.
//!
//! # `plugins check`, not `plugin check`
//!
//! The design doc and GM-276 spell it `g-mesh plugin check`. The CLI already
//! has a `plugins` group (`g-mesh plugins list`), and a second, singular group
//! for one more verb about plugins would split the surface for no reason - so
//! the command is `plugins check`, next to `list`, and `plugin` is accepted
//! as a hidden alias of the group so the spelling in the design doc (and in
//! anyone's muscle memory from reading it) still works.
//!
//! # Exit status
//!
//! The report always goes to stdout. A run with any failing check then
//! returns an error, which `main` turns into exit status 1 - the same status
//! a setup failure (unreadable manifest, missing fixture) gets, since both
//! mean "this plugin cannot be accepted as it is". Skipped checks and
//! warnings do not fail a run.

mod checks;
mod expectations;
pub mod report;
pub(crate) mod session;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use clap::Args;
use rusqlite::Connection;

use crate::daemon::manifest::{plain_spelling, read_manifest, PluginManifest};
use crate::daemon::plugin::RoundTripTimeouts;
use crate::embedding::EmbeddingPipeline;
use crate::protocol::ndjson::BulkItem;
pub use report::{CheckResult, Outcome, Report, Section};
pub use session::{MARKER_DIR_ENV, SEMANTIC_ENGINE_MARKER};

#[derive(Debug, Args)]
pub struct PluginCheckArgs {
    /// The plugin's directory: the one holding its `plugin.toml`, named after
    /// its language.
    pub plugin_dir: PathBuf,
    /// A small project in the plugin's language to run it against. Copied to
    /// a scratch directory first; never modified.
    #[arg(long)]
    pub fixture: PathBuf,
    /// Post-linking assertions against the linked index - `[[callers]]`,
    /// `[[references]]`, `[[implementations]]`, `[[imports]]`,
    /// `[[definition]]`. Answered by the same handler code the MCP tools
    /// use; see `expectations`' module doc for the format and when they run.
    /// Omit for the contract checks alone.
    #[arg(long)]
    pub expect: Option<PathBuf>,
    /// Answer every `--expect` entry tagged `tier = "semantic"` with `Skip`
    /// instead of running it - the reduced set a plugin's semantic tier
    /// being unavailable (no toolchain on `PATH`) still owes, read from the
    /// same `--expect` file rather than a second one (`expectations`' module
    /// doc, decision 6). Meaningless without `--expect`, and ignored then.
    #[arg(long)]
    pub skip_semantic_expectations: bool,
}

/// Runs `g-mesh plugins check`.
pub fn run(args: &PluginCheckArgs) -> Result<()> {
    let report =
        check(&args.plugin_dir, &args.fixture, args.expect.as_deref(), args.skip_semantic_expectations)?;
    print!("{}", report.render());
    if report.failed() {
        let failed = report.failed_ids();
        bail!(
            "the {} plugin failed {} conformance check(s): {}",
            report.language,
            failed.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

/// The library entry point behind [`run`]: everything but printing and the
/// exit status. `Err` only for a run that could not be set up at all; a
/// plugin that fails to spawn or hangs is a failing `session` check inside
/// an `Ok` report. `expect` is GM-277's `--expect <expect.toml>` - `None`
/// means no `"expectations"` section at all, not an empty one (report.rs's
/// own module doc: "a flag that parses and does nothing would read as
/// 'expectations passed'"). `skip_semantic` is GM-282's
/// `--skip-semantic-expectations` (`expectations`' module doc, decision 6);
/// it does nothing when `expect` is `None`.
pub fn check(
    plugin_dir: &Path,
    fixture: &Path,
    expect: Option<&Path>,
    skip_semantic: bool,
) -> Result<Report> {
    // Canonicalized before `read_manifest`, which requires the directory's
    // own name to equal the manifest's language - `.` has no name to compare.
    //
    // ...and then spelled back the ordinary way, because on Windows
    // `canonicalize` returns the extended-length (`\\?\`) form, and this is
    // the directory every manifest-relative `command`/`args` entry is joined
    // onto: under that prefix Windows resolves nothing, so the plugin's own
    // entry point arrives at it spelled `...\typescript\dist/src/index.js`.
    // `manifest::plain_win32_path` carries the evidence for what that costs.
    // The report prints both paths too, and the extended-length spelling of a
    // path is nobody's idea of where their plugin is.
    let plugin_dir = plain_spelling(
        fs::canonicalize(plugin_dir)
            .with_context(|| format!("plugin directory {} does not exist", plugin_dir.display()))?,
    );
    let manifest = read_manifest(&plugin_dir)?;
    let fixture = plain_spelling(
        fs::canonicalize(fixture)
            .with_context(|| format!("fixture directory {} does not exist", fixture.display()))?,
    );
    if !fixture.is_dir() {
        bail!("fixture {} is not a directory", fixture.display());
    }

    let scratch = session::Scratch::create()?;
    session::copy_tree(&fixture, &scratch.workspace()).with_context(|| {
        format!("failed to copy the fixture {} to a scratch workspace", fixture.display())
    })?;

    let timeouts = RoundTripTimeouts::from_env();
    let file_count = session::count_claimed_files(&manifest, &scratch.workspace());
    let whole_project_timeout = timeouts.semantic_pass_project_timeout(file_count);

    let mut failures = Vec::new();
    let conn = session::open_index()?;

    let bulk1 = session::run_bulk(&manifest, &scratch, whole_project_timeout);
    match &bulk1.failure {
        Some(failure) => failures.push(format!("bulk run 1: {failure}")),
        None => {
            if let Err(err) = session::ingest_and_link(&conn, &bulk1.bytes) {
                failures.push(format!("bulk run 1: committing and linking its stream failed: {err:#}"));
            }
        }
    }
    let target = bulk1
        .complete()
        .then(|| session::choose_edit_target(&manifest, &scratch.workspace(), &bulk1.lines))
        .flatten();
    // Before anything else touches the index: the baseline the session's
    // restored file is compared against.
    let bulk_file_ids = match &target {
        Some(target) => match session::file_node_ids(&conn, &target.file_path) {
            Ok(ids) => Some(ids),
            Err(err) => {
                failures
                    .push(format!("reading {}'s node ids back from the index: {err:#}", target.file_path));
                None
            }
        },
        None => None,
    };

    let bulk2 = session::run_bulk(&manifest, &scratch, whole_project_timeout);
    if let Some(failure) = &bulk2.failure {
        failures.push(format!("bulk run 2: {failure}"));
    }
    let session = match (bulk1.complete(), &target) {
        (true, Some(target)) => {
            Some(session::run_session(&manifest, &scratch, &conn, target, timeouts, whole_project_timeout))
        }
        (true, None) => {
            failures.push(format!(
                "the fixture gave the plugin no file to edit: bulk run 1 emitted no File node for a file with one of \
                 the manifest's extensions ({}) that contains a newline",
                manifest.extensions.join(", ")
            ));
            None
        }
        (false, _) => None,
    };
    if let Some(failure) = session.as_ref().and_then(|s| s.failure.clone()) {
        failures.push(failure);
    }

    // Bulk run 3 walks the tree the session left behind - the edited file
    // still holding its declaration edit - into an index of its own, so the
    // session's index after that edit has a ground truth that no prediction
    // of what the edit "should" do is standing in for.
    let edited_file =
        target.as_ref().zip(session.as_ref()).filter(|(_, s)| s.declaration_edit_ranges.is_some());
    let mut bulk3 = None;
    let mut bulk_edited_ranges = None;
    if let Some((target, _)) = edited_file {
        let run = session::run_bulk(&manifest, &scratch, whole_project_timeout);
        match &run.failure {
            Some(failure) => failures.push(format!("bulk run 3 (after the declaration edit): {failure}")),
            None => {
                let fresh = session::open_index()?;
                match session::ingest_and_link(&fresh, &run.bytes)
                    .and_then(|()| session::file_node_ranges(&fresh, &target.file_path))
                {
                    Ok(ranges) => bulk_edited_ranges = Some(ranges),
                    Err(err) => failures.push(format!(
                        "bulk run 3 (after the declaration edit): committing, linking or reading back its stream \
                         failed: {err:#}"
                    )),
                }
            }
        }
        bulk3 = Some(run);
    }

    let mut notes = vec![format!(
        "timeouts: fileChanged {:?}, per-file semanticPass {:?}, whole-project semanticPass and each bulk run {:?}",
        timeouts.file_changed, timeouts.semantic_pass_file, whole_project_timeout
    )];
    if let Some(target) = &target {
        notes.push(format!(
            "edited file: {} (whitespace-only edit: one space before the last newline, at the end of line {})",
            target.file_path, target.line
        ));
        notes.push(match &target.declaration {
            Some(edit) => format!(
                "declaration edit: a line break before line {} of {}, the last line of {:?} ({})",
                edit.line, target.file_path, edit.node_name, edit.node_id
            ),
            None => {
                format!("declaration edit: none - bulk run 1 emitted no declaration for {}", target.file_path)
            }
        });
    }
    // What the checks below actually looked at, so a pass can be told apart
    // from a check that had nothing to judge (an emptied file whose diff
    // deleted nothing passes `deletes-known` trivially, and says so here).
    for (index, run) in [Some(&bulk1), Some(&bulk2), bulk3.as_ref()].into_iter().enumerate() {
        let Some(run) = run else { continue };
        let nodes = run.lines.iter().filter(|l| matches!(l.item, Ok(BulkItem::Node(_)))).count();
        let edges = run.lines.iter().filter(|l| matches!(l.item, Ok(BulkItem::Edge(_)))).count();
        notes.push(format!("bulk run {}: {nodes} node(s), {edges} edge(s)", index + 1));
    }
    for exchange in session.iter().flat_map(|s| &s.exchanges) {
        let answer = match exchange.response.as_ref().map(|r| &r.diff) {
            None => "no answer".to_string(),
            Some(Err(_)) => "unparseable answer".to_string(),
            Some(Ok(diff)) => format!(
                "+{} / -{} node(s), +{} / -{} edge(s)",
                diff.upsert_nodes.len(),
                diff.delete_node_ids.len(),
                diff.upsert_edges.len(),
                diff.delete_edge_ids.len()
            ),
        };
        notes.push(format!("{} -> {}: {answer}", exchange.step, exchange.method.name()));
    }

    let results = checks::evaluate(&checks::RunData {
        manifest: &manifest,
        bulk: [&bulk1, &bulk2],
        target: target.as_ref(),
        bulk_file_ids: bulk_file_ids.as_ref(),
        bulk_edited_ranges: bulk_edited_ranges.as_ref(),
        session: session.as_ref(),
        failures,
        marker_exists_at_end: scratch.semantic_engine_marker().exists(),
    });

    let mut sections = vec![Section { title: "checks", results }];
    if let Some(expect_path) = expect {
        sections.push(expectations_section(
            expect_path,
            &manifest,
            &conn,
            &scratch,
            bulk1.complete(),
            session.as_ref(),
            skip_semantic,
        ));
    }

    Ok(Report { language: manifest.language.clone(), plugin_dir, fixture, notes, sections })
}

/// Builds the `"expectations"` section for `--expect <expect_path>` -
/// `expectations`' module doc has the full reasoning (decisions 1-5); this
/// is just the plumbing: gate on session readiness (decision 1), parse
/// (decision 5), then hand off to `expectations::evaluate`.
///
/// One `CheckResult` id, `expectations.file`, always leads the section - a
/// `Skip` when the session never reached the state expectations need, a
/// `Fail` when the file could not be read or parsed, or a `Pass` followed by
/// one result per expectation the file declared.
fn expectations_section(
    expect_path: &Path,
    manifest: &PluginManifest,
    conn: &Arc<Mutex<Connection>>,
    scratch: &session::Scratch,
    bulk1_complete: bool,
    session: Option<&session::Session>,
    skip_semantic: bool,
) -> Section {
    const FILE_CHECK: &str = "expectations.file";

    let session_ready = bulk1_complete && session.is_some_and(|s| s.failure.is_none());
    if !session_ready {
        return Section {
            title: "expectations",
            results: vec![CheckResult {
                id: FILE_CHECK.into(),
                outcome: Outcome::Skip(
                    "not reached: bulk run 1 did not complete, or the control-plane session failed (see \
                     `checks`) - expectations need the fully linked index a completed session leaves behind \
                     (expectations' module doc, decision 1)"
                        .to_string(),
                ),
                warnings: Vec::new(),
            }],
        };
    }

    let expect_file = match expectations::parse(expect_path) {
        Ok(expect_file) => expect_file,
        Err(err) => {
            return Section {
                title: "expectations",
                results: vec![CheckResult {
                    id: FILE_CHECK.into(),
                    outcome: Outcome::Fail(vec![format!("{err:#}")]),
                    warnings: Vec::new(),
                }],
            };
        }
    };

    let embedding = EmbeddingPipeline::disabled();
    let entry_points = manifest.workspace.entry_points.clone();
    let project_root = scratch.workspace();
    let ctx = expectations::EvalContext {
        conn,
        embedding: &embedding,
        project_root: &project_root,
        entry_points: &entry_points,
    };

    let mut results =
        vec![CheckResult { id: FILE_CHECK.into(), outcome: Outcome::Pass, warnings: Vec::new() }];
    results.extend(expectations::evaluate(&ctx, &expect_file, skip_semantic));
    Section { title: "expectations", results }
}
