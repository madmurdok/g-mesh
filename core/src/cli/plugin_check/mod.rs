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
pub mod report;
pub(crate) mod session;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::daemon::manifest::read_manifest;
use crate::daemon::plugin::RoundTripTimeouts;
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
}

/// Runs `g-mesh plugins check`.
pub fn run(args: &PluginCheckArgs) -> Result<()> {
    let report = check(&args.plugin_dir, &args.fixture)?;
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
/// an `Ok` report.
pub fn check(plugin_dir: &Path, fixture: &Path) -> Result<Report> {
    // Canonicalized before `read_manifest`, which requires the directory's
    // own name to equal the manifest's language - `.` has no name to compare.
    let plugin_dir = fs::canonicalize(plugin_dir)
        .with_context(|| format!("plugin directory {} does not exist", plugin_dir.display()))?;
    let manifest = read_manifest(&plugin_dir)?;
    let fixture = fs::canonicalize(fixture)
        .with_context(|| format!("fixture directory {} does not exist", fixture.display()))?;
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

    let mut notes = vec![format!(
        "timeouts: fileChanged {:?}, per-file semanticPass {:?}, whole-project semanticPass and each bulk run {:?}",
        timeouts.file_changed, timeouts.semantic_pass_file, whole_project_timeout
    )];
    if let Some(target) = &target {
        notes.push(format!(
            "edited file: {} (whitespace-only edit: one space before the last newline, at the end of line {})",
            target.file_path, target.line
        ));
    }
    // What the checks below actually looked at, so a pass can be told apart
    // from a check that had nothing to judge (an emptied file whose diff
    // deleted nothing passes `deletes-known` trivially, and says so here).
    for (index, run) in [&bulk1, &bulk2].into_iter().enumerate() {
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
        session: session.as_ref(),
        failures,
        marker_exists_at_end: scratch.semantic_engine_marker().exists(),
    });

    Ok(Report {
        language: manifest.language.clone(),
        plugin_dir,
        fixture,
        notes,
        sections: vec![Section { title: "checks", results }],
    })
}
