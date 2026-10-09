//! `g-mesh plugins check` - the conformance kit (GM-276) - driven through the
//! real binary, against plugins that are wrong on purpose.
//!
//! # One fake, one defect, one failing check
//!
//! A check that has never been seen failing proves nothing: it may simply not
//! look at what it claims to. So every check the kit reports has a fake plugin
//! here that breaks exactly that rule, and each test asserts two things - that
//! check fails, and *no other* check does. The second half matters as much as
//! the first: a defect that trips three checks shows the checks overlap, and a
//! future plugin author reading a report could not tell which rule they broke.
//!
//! All the fakes are one program (`g-mesh-fake-plugin --toy <defect>`,
//! [`FAKE_PLUGIN_BIN`]) with a defect switch, rather than one hand-written plugin per check. The conformant
//! baseline (`--toy none`) is the thing every defect is measured
//! against, and one program guarantees each fake is that baseline plus one
//! change - not a second implementation that might be wrong in some other way
//! too. The baseline passing everything (`the_conformant_fake_passes_every_check`)
//! is what makes "only this check fails" meaningful for the rest.
//!
//! The fake speaks a toy language over `tests/fixtures/plugin_check/fake/`:
//! `fn NAME` declares a function, `call NAME` calls one in the same file, and
//! `use FILE NAME` references a name from another file (a `pending_symbol`
//! placeholder). That is enough to produce every kind of row the contract
//! talks about - `File` nodes, declarations, `DEFINES`/`EXPORTS`, a
//! same-file resolved edge, a placeholder edge, and (in its semantic pass) a
//! cross-file upgrade.
//!
//! # And the real plugin
//!
//! `the_typescript_plugin_passes_on_a_small_typescript_fixture` runs the
//! bundled TS plugin (built by `cargo build --workspace`) over
//! `../plugins/typescript/conformance/project/` - a cross-file import, both
//! re-export forms, a namespace member use (so its semantic pass actually
//! starts tsserver and the lazy-engine check has a marker to judge), an
//! overloaded function and an interface implementation. This is the same
//! fixture CI's per-plugin conformance job runs (GM-277's own module doc,
//! `plugins/typescript/conformance/expect.toml`) - one fixture rather than
//! two diverging copies, since `core/tests/fixtures/plugin_check/typescript/`
//! used to exist purely for this test and CI had nothing to iterate.
//!
//! `the_go_plugin_passes_on_its_own_fixture` and
//! `the_go_plugin_satisfies_its_own_expectations_file` do the same for the
//! bundled Go plugin (also built by `core/build.rs`) over
//! `../plugins/go/conformance/{project,expect.toml}` - a multi-file package,
//! a second package reached through an import, an external `_test` package
//! and a `go.work` naming two modules. They are what puts the Go plugin's
//! conformance run in core's own CI, which is what the design doc asks for
//! ("The same fixtures run in core's CI for every bundled plugin"), and they
//! are GM-280's end-to-end evidence that the container-scoped placeholders
//! that plugin emits are addresses core's linker actually resolves - plus,
//! since GM-281, that its `go/types` pass resolves receiver calls through a
//! variable, an embedded field and an interface value, and finds implicit
//! interface implementations. Both therefore need a Go toolchain on `PATH`:
//! without one `core/build.rs` cannot build the plugin at all.
//!
//! # `--expect` (GM-277)
//!
//! `the_typescript_plugin_satisfies_its_own_expectations_file` runs the same
//! fixture with `--expect plugins/typescript/conformance/expect.toml` and
//! asserts every expectation passes - the acceptance criterion "the TS
//! fixture passes". The rest of GM-277's own tests
//! (`a_wrong_expectation_reports_a_readable_diff`,
//! `an_ambiguous_symbol_fails_with_its_candidates_and_file_disambiguates_it`,
//! `an_unknown_expectation_key_is_a_hard_parse_error`,
//! `a_namespace_import_caller_needs_the_semantic_pass_to_resolve`) use small,
//! purpose-built fixtures of their own - the toy `fake` language for the
//! three that only exercise `expectations.rs`'s own logic (fast, no tsserver),
//! and the real TS plugin with a `semantic_pass = false` copy of its manifest
//! for the one that has to prove a real semantic-only fact.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh::daemon::manifest::ReceiverCallResolution;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Every check id the kit reports, in report order - asserted in full on
/// every run, so a check silently dropping out of the report fails a test
/// rather than passing every "only X fails" assertion vacuously.
const ALL_CHECKS: [&str; 17] = [
    "session",
    "shape",
    "stream-order",
    "same-file-rule",
    "id-stability.bulk-repeat",
    "id-stability.whitespace-edit",
    "id-stability.deletes-known",
    "id-stability.incremental-matches-bulk",
    "id-stability.declaration-edit-applies",
    "ownership.defines-exports-from-file",
    "ownership.language",
    "ownership.no-container",
    "ownership.diff-stays-in-file",
    "capabilities.semantic-pass-undeclared",
    "capabilities.semantic-engine-lazy",
    "capabilities.files-created-resolves",
    RESOLUTION_DELTA,
];

/// Run only for a manifest declaring `resolution_delta` (the TS plugin's
/// does); every other plugin here reports it `SKIP`.
const RESOLUTION_DELTA: &str = "capabilities.resolution-delta-version-bump";

/// The checks that report `SKIP` on a plugin declaring `semantic_pass =
/// true` run without an `--expect` pair: the undeclared-pass check does not
/// apply, and `files-created-resolves` has no `[files_created]` pair to run
/// (or the manifest does not declare the capability).
const SKIPPED_WITHOUT_PAIR: [&str; 2] =
    ["capabilities.semantic-pass-undeclared", "capabilities.files-created-resolves"];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin_check")
}

/// The bundled TS plugin's own directory - `plugins/typescript`, a sibling of
/// `core/`, named after its manifest's `language` as `read_manifest`
/// requires.
fn ts_plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript")
}

/// The TS plugin's own conformance fixture and expectations file (GM-277) -
/// `plugins/typescript/conformance/{project,expect.toml}`, the same pair
/// CI's per-plugin job runs. See this file's module doc for why this is the
/// one TS fixture rather than a second copy under `tests/fixtures/`.
fn ts_conformance_project() -> PathBuf {
    ts_plugin_dir().join("conformance/project")
}

fn ts_conformance_expect() -> PathBuf {
    ts_plugin_dir().join("conformance/expect.toml")
}

/// The bundled Go plugin (GM-279's scaffold, GM-280's real extractor) and its
/// own conformance pair, laid out exactly like the TS plugin's: the binary
/// `core/build.rs` builds, the fixture, and the expectations file.
fn go_plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/go")
}

fn go_conformance_project() -> PathBuf {
    go_plugin_dir().join("conformance/project")
}

fn go_conformance_expect() -> PathBuf {
    go_plugin_dir().join("conformance/expect.toml")
}

/// The fake plugin is `g-mesh-fake-plugin`'s toy persona
/// (`plugins/sdk/fake/toy.rs`): a conformant plugin for the toy `.fk`
/// language, plus one deliberate defect selected by `--toy <defect>`. See
/// this file's module doc. It is a workspace binary beside [`BIN`], which the
/// manifest reaches through `${G_MESH_BIN_DIR}`.
const FAKE_PLUGIN_BIN: &str = "g-mesh-fake-plugin";

struct FakePlugin {
    _root: tempfile::TempDir,
    dir: PathBuf,
}

/// Writes the fake as a discoverable plugin directory - `<tmp>/fake/` with a
/// `plugin.toml`, since `read_manifest` requires the directory to be named
/// after the language.
fn install_fake(defect: &str, semantic_pass: bool) -> FakePlugin {
    install_fake_with(defect, semantic_pass, false)
}

/// [`install_fake`] with the manifest's `files_created` capability switched
/// as given - GM-516's `capabilities.files-created-resolves`.
fn install_fake_with(defect: &str, semantic_pass: bool, files_created: bool) -> FakePlugin {
    let binary = Path::new(BIN)
        .parent()
        .expect("the g-mesh binary has a directory")
        .join(format!("{FAKE_PLUGIN_BIN}{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} does not exist - run `cargo build --workspace` first", binary.display());
    let root = tempfile::tempdir().expect("failed to create a temp dir for the fake plugin");
    let dir = root.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let protocol = g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION;
    fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"fake\"\nprotocol_version = {protocol}\nplugin_version = \"0.0.0-fake\"\n\n\
             [plugin.spawn]\ncommand = \"${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}\"\n\
             args = [\"--language\", \"fake\", \"--plugin-version\", \"0.0.0-fake\", \"--toy\", \"{defect}\"]\n\n\
             [plugin.languages]\nextensions = [\".fk\"]\n\n\
             [plugin.capabilities]\nsemantic_pass = {semantic_pass}\nfiles_created = {files_created}\n"
        ),
    )
    .unwrap();
    FakePlugin { _root: root, dir }
}

struct Run {
    success: bool,
    stdout: String,
    /// Check id -> "PASS" / "FAIL" / "SKIP".
    outcomes: BTreeMap<String, String>,
}

fn run_check(plugin_dir: &Path, fixture: &Path, env: &[(&str, &str)]) -> Run {
    let output = Command::new(BIN)
        .args(["plugins", "check"])
        .arg(plugin_dir)
        .arg("--fixture")
        .arg(fixture)
        .envs(env.iter().copied())
        .output()
        .expect("failed to run g-mesh plugins check");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut outcomes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        for verdict in ["PASS", "FAIL", "SKIP"] {
            if let Some(id) = rest.strip_prefix(verdict).and_then(|r| r.strip_prefix("  ")) {
                let id = id.split_whitespace().next().unwrap_or_default().to_string();
                assert!(
                    outcomes.insert(id.clone(), verdict.to_string()).is_none(),
                    "{id} reported twice:\n{stdout}"
                );
            }
        }
    }
    let run = Run { success: output.status.success(), stdout, outcomes };
    let reported: Vec<&str> = run.outcomes.keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        reported,
        expected,
        "the report must carry every check exactly once:\n{}\nstderr:\n{}",
        run.stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    run
}

impl Run {
    fn failing(&self) -> Vec<&str> {
        self.outcomes.iter().filter(|(_, v)| *v == "FAIL").map(|(k, _)| k.as_str()).collect()
    }

    /// Panicking with the report attached, rather than `BTreeMap`'s own
    /// `no entry found for key` - which is what
    /// `a_namespace_import_caller_needs_the_semantic_pass_to_resolve` printed
    /// on Windows (CI run 35451298477), and it names neither the missing id
    /// nor the run that was missing it.
    fn outcome(&self, id: &str) -> &str {
        self.outcomes.get(id).map(String::as_str).unwrap_or_else(|| {
            panic!(
                "the report carries no {id:?}; it has {:?}:\n{}",
                self.outcomes.keys().collect::<Vec<_>>(),
                self.stdout
            )
        })
    }
}

/// Runs the fake with `defect` and asserts `expected` is the one failing
/// check, and that the run's exit status says so. Returns the run for any
/// further assertion on its findings.
fn assert_only_failure(defect: &str, semantic_pass: bool, expected: &str, env: &[(&str, &str)]) -> Run {
    assert_only_failure_on(&fixtures().join("fake"), defect, semantic_pass, expected, env)
}

fn assert_only_failure_on(
    fixture: &Path,
    defect: &str,
    semantic_pass: bool,
    expected: &str,
    env: &[(&str, &str)],
) -> Run {
    let fake = install_fake(defect, semantic_pass);
    let run = run_check(&fake.dir, fixture, env);
    assert_eq!(
        run.failing(),
        vec![expected],
        "defect {defect:?} must fail exactly {expected}:\n{}",
        run.stdout
    );
    assert!(!run.success, "a failing check must make the command exit non-zero:\n{}", run.stdout);
    run
}

#[test]
fn the_conformant_fake_passes_every_check() {
    let fixture = fixtures().join("fake");
    let before: Vec<(PathBuf, Vec<u8>)> =
        ["a.fk", "b.fk"].iter().map(|f| (fixture.join(f), fs::read(fixture.join(f)).unwrap())).collect();

    let fake = install_fake("none", true);
    let run = run_check(&fake.dir, &fixture, &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let skipped = SKIPPED_WITHOUT_PAIR.contains(&id) || id == RESOLUTION_DELTA;
        let expected = if skipped { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("WARN"), "a v2-speaking plugin gets no legacy warning:\n{}", run.stdout);
    // The emptied-file step really deleted something, so `deletes-known`
    // passed on evidence rather than on an empty list.
    assert!(run.stdout.contains("(a.fk emptied) -> fileChanged: +1 / -3 node(s)"), "{}", run.stdout);
    // Likewise the declaration edit really moved something - `alpha` is the
    // first declaration, on line 1, so the break goes before it and every
    // node of a.fk moves - so `declaration-edit-applies` compared real ranges.
    assert!(
        run.stdout
            .contains("declaration edit: a line break before line 1 of a.fk, the last line of \"alpha\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("the last line of \"alpha\") -> fileChanged: +4 / -0 node(s)"),
        "{}",
        run.stdout
    );

    for (path, contents) in before {
        assert_eq!(
            fs::read(&path).unwrap(),
            contents,
            "the kit must never modify the fixture ({})",
            path.display()
        );
    }
}

/// The same conformant fake with `semantic_pass = false`: core must never send
/// it the request (the wire half of the capability check), and the lazy-engine
/// check does not apply.
#[test]
fn the_conformant_fake_without_semantic_pass_passes_the_undeclared_capability_check() {
    let fake = install_fake("none", false);
    let run = run_check(&fake.dir, &fixtures().join("fake"), &[]);
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("capabilities.semantic-pass-undeclared"), "PASS", "{}", run.stdout);
    assert_eq!(run.outcome("capabilities.semantic-engine-lazy"), "SKIP", "{}", run.stdout);
    assert!(!run.stdout.contains("-> semanticPass"), "no semanticPass may be sent:\n{}", run.stdout);
}

#[test]
fn a_placeholder_without_a_target_fails_shape() {
    let run = assert_only_failure("shape", true, "shape", &[]);
    assert!(
        run.stdout.contains("\"a.fk#use:b.fk#gamma\" (nativeKind \"pending_symbol\") has no `target`"),
        "{}",
        run.stdout
    );
}

#[test]
fn an_edge_emitted_before_its_nodes_fails_stream_order() {
    let run = assert_only_failure("stream-order-late", true, "stream-order", &[]);
    assert!(
        run.stdout.contains("bulk run 1, NDJSON line 2: edge \"a.fk#file -DEFINES-> a.fk#fn:alpha\""),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("is only emitted later"), "{}", run.stdout);
}

#[test]
fn an_edge_onto_another_files_node_fails_stream_order() {
    let run = assert_only_failure("stream-order-cross-file", true, "stream-order", &[]);
    assert!(
        run.stdout.contains("toId is a node of \"b.fk\", not of the edge's own file \"a.fk\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("fileChanged #1 (a.fk unmodified) fileChanged response"),
        "diffs are judged too:\n{}",
        run.stdout
    );
}

#[test]
fn an_unresolved_same_file_edge_fails_the_same_file_rule() {
    let run = assert_only_failure("same-file-rule", true, "same-file-rule", &[]);
    assert!(
        run.stdout.contains("edge \"a.fk#fn:beta -CALLS-> a.fk#fn:alpha\" (CALLS \"a.fk#fn:beta\" -> \"a.fk#fn:alpha\") is `resolved: false`"),
        "{}",
        run.stdout
    );
}

#[test]
fn ids_that_change_between_bulk_runs_fail_bulk_repeat() {
    let run = assert_only_failure("bulk-repeat", true, "id-stability.bulk-repeat", &[]);
    assert!(run.stdout.contains("node id \"a.fk#var:run-"), "{}", run.stdout);
    assert!(run.stdout.contains("is emitted by bulk run 1 but not by bulk run 2"), "{}", run.stdout);
}

#[test]
fn a_range_moved_by_whitespace_fails_the_whitespace_edit_check() {
    let run = assert_only_failure("whitespace-moves-range", true, "id-stability.whitespace-edit", &[]);
    assert!(run.stdout.contains("upserts node \"a.fk#file\""), "{}", run.stdout);
}

/// GM-527. A plugin whose `File` node keeps the old end, `(lines, 0)` one
/// past the last line after a final newline, answers the whitespace edit with
/// an empty diff - that end does not move either - but its end line is not
/// the line `a.fk`'s content ends on (3, of 4 lines and a final newline), so
/// `whitespace-edit` fails on that node alone.
///
/// Control: remove the `file_end_past_content` finding from
/// `checks::whitespace_edit`; every check passes and `assert_only_failure`
/// fails.
#[test]
fn a_file_end_past_the_last_line_fails_the_whitespace_edit_check() {
    let run = assert_only_failure("file-end-past-last-line", true, "id-stability.whitespace-edit", &[]);
    assert!(run.stdout.contains("\"a.fk#file\""), "{}", run.stdout);
    assert!(run.stdout.contains("ends at 4:0"), "the old end is named: {}", run.stdout);
    assert!(run.stdout.contains("ends on line 3"), "the content's end line is named: {}", run.stdout);
    assert!(!run.stdout.contains("upserts node"), "the diff itself is empty: {}", run.stdout);
}

#[test]
fn deleting_an_id_never_emitted_fails_deletes_known() {
    let run = assert_only_failure("deletes-unknown", true, "id-stability.deletes-known", &[]);
    assert!(run.stdout.contains("deleteNodeIds names \"never-emitted-node\""), "{}", run.stdout);
}

#[test]
fn incremental_ids_that_differ_from_bulk_ids_fail_incremental_matches_bulk() {
    let run = assert_only_failure("incremental-ids", true, "id-stability.incremental-matches-bulk", &[]);
    assert!(
        run.stdout
            .contains("node \"a.fk#func:alpha\" of \"a.fk\" is in the index after fileChanged restored"),
        "{}",
        run.stdout
    );
}

/// GM-294. A plugin whose diff never re-sends a declaration that moved keeps
/// every id right - `whitespace-edit`, `deletes-known` and
/// `incremental-matches-bulk` all pass - while the index goes on answering
/// with the old ranges: the user-visible half of GM-292, produced by the
/// plugin instead of by core. Only the declaration edit, judged against a
/// bulk walk of the edited file, sees it.
#[test]
fn a_diff_that_never_resends_a_moved_declaration_fails_declaration_edit_applies() {
    let run = assert_only_failure("stale-ranges", true, "id-stability.declaration-edit-applies", &[]);
    assert!(
        run.stdout.contains("node \"a.fk#fn:alpha\" is at 0:0-0:8 in the index, but a fresh bulk walk of the edited file puts it at 1:0-1:8"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_defines_edge_from_a_symbol_fails_ownership() {
    let run = assert_only_failure("defines-from-symbol", true, "ownership.defines-exports-from-file", &[]);
    assert!(run.stdout.contains("starts at a Function node"), "{}", run.stdout);
}

#[test]
fn a_node_in_another_language_fails_ownership_language() {
    let run = assert_only_failure("language", true, "ownership.language", &[]);
    assert!(
        run.stdout.contains("declares language \"fake-dialect\", but the manifest's language is \"fake\""),
        "{}",
        run.stdout
    );
}

#[test]
fn a_plugin_emitted_container_fails_ownership_no_container() {
    let run = assert_only_failure("container", true, "ownership.no-container", &[]);
    assert!(
        run.stdout.contains("node \"fake-container-pkg\" has nativeKind \"container\""),
        "{}",
        run.stdout
    );
}

#[test]
fn a_diff_touching_another_file_fails_diff_stays_in_file() {
    let run = assert_only_failure("diff-other-file", true, "ownership.diff-stays-in-file", &[]);
    assert!(
        run.stdout.contains("upserts node \"b.fk#file\" of \"b.fk\" in a diff for \"a.fk\""),
        "{}",
        run.stdout
    );
}

#[test]
fn an_engine_started_before_the_first_semantic_pass_fails_the_lazy_check() {
    let run = assert_only_failure("eager-engine", true, "capabilities.semantic-engine-lazy", &[]);
    assert!(
        run.stdout.contains("already existed when the first semanticPass request was written"),
        "{}",
        run.stdout
    );
}

#[test]
fn an_engine_started_without_the_capability_fails_the_undeclared_check() {
    let run = assert_only_failure("undeclared-engine", false, "capabilities.semantic-pass-undeclared", &[]);
    assert!(run.stdout.contains("although its manifest declares semantic_pass = false"), "{}", run.stdout);
}

/// A plugin that never answers `fileChanged` must cost one `session` failure
/// after `G_MESH_FILE_CHANGED_TIMEOUT_MS`, not a hung CLI - and every check
/// that needed the session's answers must be skipped, not passed.
#[test]
fn a_plugin_that_never_answers_fails_session_instead_of_hanging() {
    let started = std::time::Instant::now();
    let run = assert_only_failure("hang", false, "session", &[("G_MESH_FILE_CHANGED_TIMEOUT_MS", "1500")]);
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "took {:?}", started.elapsed());
    assert!(run.stdout.contains("no response within 1.5s"), "{}", run.stdout);
    for id in [
        "id-stability.whitespace-edit",
        "id-stability.deletes-known",
        "id-stability.incremental-matches-bulk",
    ] {
        assert_eq!(run.outcome(id), "SKIP", "{id}:\n{}", run.stdout);
    }
}

/// The bulk walk has no timeout in the daemon, but must have one here. Its
/// budget is `semantic_pass_project_timeout(file_count)`: the overridden floor
/// or 10s per claimed file, whichever is larger - so this runs on a one-file
/// copy of the fixture to keep the wait at 10s.
#[test]
fn a_bulk_index_that_never_finishes_fails_session_instead_of_hanging() {
    let fixture = tempfile::tempdir().unwrap();
    fs::copy(fixtures().join("fake/b.fk"), fixture.path().join("b.fk")).unwrap();
    let started = std::time::Instant::now();
    let run = assert_only_failure_on(
        fixture.path(),
        "bulk-hang",
        false,
        "session",
        &[("G_MESH_SEMANTIC_PASS_PROJECT_TIMEOUT_MS", "1000")],
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "took {:?}", started.elapsed());
    assert!(
        run.stdout.contains("bulk run 1: the bulk index did not finish within 10s and was killed"),
        "{}",
        run.stdout
    );
}

/// GM-337. A plugin that dies before writing a byte is the shape every
/// Windows conformance failure on CI run 35451298477 had, and the report
/// said only "the bulk index exited with exit code: 1" - because the kit
/// spawned it with `stderr` inherited, so the plugin's own account of why
/// went to the kit's stderr, which the failing assertion never printed.
/// Twenty-five failures, and the one process that knew the answer had been
/// told to say it where nobody was listening.
///
/// So: the `session` finding must quote what the plugin actually wrote. This
/// is the control for that - it fails (on the quoted line, not on the
/// verdict) if `run_bulk` goes back to `Stdio::inherit`.
#[test]
fn a_bulk_index_that_dies_quotes_what_the_plugin_said_about_it() {
    let run = assert_only_failure("bulk-dies", false, "session", &[]);
    // `ExitStatus`'s own wording differs by platform ("exit status: 3",
    // "exit code: 3"), so the assertion is that the status is still named at
    // all - the quoted stderr below is the part this test is about.
    assert!(
        run.stdout.contains("bulk run 1: the bulk index exited with exit") && run.stdout.contains(": 3"),
        "the exit status is still named:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("| fk-extractor: cannot open the toy grammar"),
        "the plugin's own stderr must be in the report:\n{}",
        run.stdout
    );
    // Both runs, not just the first: a reader comparing them needs each
    // one's own account rather than one line and a repetition.
    assert_eq!(
        run.stdout.matches("| fk-extractor: cannot open the toy grammar").count(),
        2,
        "both bulk runs quote their own stderr:\n{}",
        run.stdout
    );
}

/// The other half of the same contract: "it exited 1 and said nothing" is a
/// finding of its own, and must not read the same as "it exited 1 and I did
/// not look" - which is exactly what the report used to say either way.
#[test]
fn a_bulk_index_that_dies_silently_says_that_it_said_nothing() {
    let run = assert_only_failure("bulk-dies-silently", false, "session", &[]);
    assert!(run.stdout.contains(", having written nothing to stderr"), "{}", run.stdout);
    assert!(!run.stdout.contains("its stderr, last"), "nothing to quote, so nothing quoted:\n{}", run.stdout);
}

#[test]
fn the_typescript_plugin_passes_on_a_small_typescript_fixture() {
    let run = run_check(&ts_plugin_dir(), &ts_conformance_project(), &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        // The shipped manifest declares `semantic_pass = true`, so the
        // undeclared-pass check is the one that does not apply.
        let expected = if SKIPPED_WITHOUT_PAIR.contains(&id) { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    // GM-275: the TS plugin speaks wire v2 now, so there is nothing left for
    // `shape` to warn about (that WARN existed only while the plugin still
    // sent the v1 shape core's normalizing deserializer accepted).
    assert!(!run.stdout.contains("WARN"), "{}", run.stdout);
}

/// The bundled Go plugin against its own fixture - a multi-file package, a
/// second package reached through an import, an external `_test` package and
/// a `go.work` naming two modules.
///
/// `capabilities.semantic-pass-undeclared` is a `SKIP` and the only one:
/// the manifest declares `semantic_pass = true`, so that check does not
/// apply. `capabilities.semantic-engine-lazy` is a `PASS` as of GM-281 -
/// the plugin now has a real engine (`go/types` through
/// golang.org/x/tools/go/packages) and writes the kit's marker at the moment
/// it first calls `packages.Load`, which happens only on a `semanticPass`.
/// Before GM-281 it was a second `SKIP ... not instrumented`, which was the
/// honest report for a plugin with no engine to start.
#[test]
fn the_go_plugin_passes_on_its_own_fixture() {
    let run = run_check(&go_plugin_dir(), &go_conformance_project(), &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let skipped = SKIPPED_WITHOUT_PAIR.contains(&id) || id == RESOLUTION_DELTA;
        let expected = if skipped { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("WARN"), "{}", run.stdout);
}

// ============================================================================
// GM-277: `--expect <expect.toml>` - post-linking assertions against the
// same query code the MCP tools use. See `core/src/cli/plugin_check/
// expectations.rs`'s own module doc for the decisions these tests check.
// ============================================================================

/// Like [`run_check`], but for a run given `--expect`: the set of reported
/// check ids now includes a dynamic `"expectations"` section
/// (`expectations.callers[0]`, ...) that varies with the expectations file,
/// so this does not assert the fixed `ALL_CHECKS` set the way `run_check`
/// does - only that the command ran and captured output to inspect.
fn run_check_with_expect(plugin_dir: &Path, fixture: &Path, expect: &Path) -> Run {
    run_check_with_expect_env(plugin_dir, fixture, expect, &[], &[])
}

/// [`run_check_with_expect`], generalized with extra CLI args (GM-282's
/// `--skip-semantic-expectations`) and an environment override (GM-282's
/// `PATH` stripped of `go` - [`path_without_go`]). `env` is *added* to the
/// spawned process's environment (`Command::env`, not `env_clear`), so
/// everything not named here is inherited as usual; passing `PATH` replaces
/// the inherited one rather than merging with it, which is the point.
fn run_check_with_expect_env(
    plugin_dir: &Path,
    fixture: &Path,
    expect: &Path,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> Run {
    let output = Command::new(BIN)
        .args(["plugins", "check"])
        .arg(plugin_dir)
        .arg("--fixture")
        .arg(fixture)
        .arg("--expect")
        .arg(expect)
        .args(extra_args)
        .envs(env.iter().copied())
        .output()
        .expect("failed to run g-mesh plugins check --expect");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut outcomes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        for verdict in ["PASS", "FAIL", "SKIP"] {
            if let Some(id) = rest.strip_prefix(verdict).and_then(|r| r.strip_prefix("  ")) {
                let id = id.split_whitespace().next().unwrap_or_default().to_string();
                assert!(
                    outcomes.insert(id.clone(), verdict.to_string()).is_none(),
                    "{id} reported twice:\n{stdout}"
                );
            }
        }
    }
    Run { success: output.status.success(), stdout, outcomes }
}

/// The current process's `PATH` with every directory holding a `go`/`go.exe`
/// binary removed - lifted from `plugins/go/semantic_test.go`'s own way of
/// simulating "no toolchain installed" (`exec.LookPath` consults `PATH`
/// only, so filtering by directory contents is exact, not an approximation)
/// to the process this test spawns rather than the plugin's own unit tests.
/// Filters by directory rather than deleting one known entry, since which
/// directory holds `go` is a property of the machine running the test, not
/// something this file can assume.
fn path_without_go() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    let go_name = if cfg!(windows) { "go.exe" } else { "go" };
    let kept: Vec<String> = std::env::split_paths(&path)
        .filter(|dir| !dir.join(go_name).is_file())
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
    std::env::join_paths(kept).expect("filtered PATH must still join").to_string_lossy().into_owned()
}

/// Whether `go` resolves anywhere on `path` - [`path_without_go`]'s own
/// sanity check, so a pass downstream is not mistaken for a real
/// discrimination when it was actually still finding a toolchain some other
/// way.
fn path_has_go(path: &str) -> bool {
    let go_name = if cfg!(windows) { "go.exe" } else { "go" };
    std::env::split_paths(path).any(|dir| dir.join(go_name).is_file())
}

/// Writes a small `.fk` fixture project (the toy language `install_fake`'s
/// plugin speaks - see this file's module doc) into a fresh temp directory,
/// one file per `(name, contents)` pair, and returns its path.
fn write_fk_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a temp fixture dir");
    for (name, contents) in files {
        fs::write(dir.path().join(name), contents).unwrap();
    }
    dir
}

fn write_expect_file(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("expect.toml");
    fs::write(&path, contents).unwrap();
    path
}

/// The TS fixture's own `expect.toml` (`plugins/typescript/conformance/
/// expect.toml`) passes end to end through the real plugin, tsserver's
/// semantic pass included - the acceptance criterion "the TS fixture
/// passes".
#[test]
fn the_typescript_plugin_satisfies_its_own_expectations_file() {
    let run = run_check_with_expect(&ts_plugin_dir(), &ts_conformance_project(), &ts_conformance_expect());
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    let expectation_results: Vec<&String> = run
        .outcomes
        .keys()
        .filter(|id| id.starts_with("expectations.") && *id != "expectations.file")
        .collect();
    // Every kind in expect.toml is exercised, and none of them silently
    // vanished from the report.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.callers[")), "{}", run.stdout);
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.references[")),
        "{}",
        run.stdout
    );
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.implementations[")),
        "{}",
        run.stdout
    );
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.imports[")), "{}", run.stdout);
    // GM-365's incoming direction. The `[` above and here is load-bearing:
    // `expectations.importers[0]` starts with `expectations.imports` too, so
    // without it one category could stand in for the other and a fixture that
    // lost its outgoing entry would still look covered.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.importers[")), "{}", run.stdout);
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.definition[")),
        "{}",
        run.stdout
    );
    // GM-371's category: what a name is NOT. Asserted present for the same
    // reason as the two above - a fixture that quietly dropped it would still
    // look complete, and this is the one category whose whole history is that
    // nothing exercised it.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.refusal[")), "{}", run.stdout);
    for id in expectation_results {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}

/// The Go plugin's own expectations, through the real linker and the real
/// MCP handlers - GM-280's end-to-end proof that a container-scoped
/// placeholder is an address core actually resolves, GM-281's that a
/// `go/types` pass answers the three receiver shapes and finds implementers
/// nothing in the syntax declares, and GM-282's that the remaining two kinds
/// (`[[references]]`, `[[definition]]`) resolve too - the full set, every
/// kind the kit supports, the same acceptance criterion
/// `the_typescript_plugin_satisfies_its_own_expectations_file` checks for
/// that plugin's own file.
#[test]
fn the_go_plugin_satisfies_its_own_expectations_file() {
    let run = run_check_with_expect(&go_plugin_dir(), &go_conformance_project(), &go_conformance_expect());
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    let expectation_results: Vec<&String> = run
        .outcomes
        .keys()
        .filter(|id| id.starts_with("expectations.") && *id != "expectations.file")
        .collect();
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.callers[")), "{}", run.stdout);
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.references[")),
        "{}",
        run.stdout
    );
    // GM-281: implicit interface satisfaction is in the file now, and it is
    // the one kind no structural tier could ever have produced.
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.implementations[")),
        "{}",
        run.stdout
    );
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.imports[")), "{}", run.stdout);
    // GM-365's incoming direction. The `[` above and here is load-bearing:
    // `expectations.importers[0]` starts with `expectations.imports` too, so
    // without it one category could stand in for the other and a fixture that
    // lost its outgoing entry would still look covered.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.importers[")), "{}", run.stdout);
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.definition[")),
        "{}",
        run.stdout
    );
    // GM-371's category: what a name is NOT. Asserted present for the same
    // reason as the two above - a fixture that quietly dropped it would still
    // look complete, and this is the one category whose whole history is that
    // nothing exercised it.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.refusal[")), "{}", run.stdout);
    for id in expectation_results {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}

/// GM-384 rewrote this from GM-282's original discrimination test for
/// `--skip-semantic-expectations`, because that test pinned a bug: it
/// asserted the run *succeeded* with `expectations.file` `Pass` and the four
/// `tier = "semantic"` entries cleanly `Skip`. That could only happen
/// because the Go plugin's whole-project `semanticPass` answered an empty
/// diff with no `incomplete` field - wire/src/lib.rs's
/// `FileChangeResponse::incomplete`, which core's `watcher::apply::
/// apply_semantic_pass` reads to decide whether a whole-project pass gets to
/// leave `language_state.semanticPassAt` set. Absent means "this pass
/// finished," so a Go index built with no toolchain on `PATH` looked exactly
/// like one whose semantic tier had just run to completion - `mcp::
/// provenance` (GM-382) had nothing to warn about, and `mcp::instructions`
/// (GM-262) stopped listing Go's receiver-call gap. Two consumers misled by
/// one missing field.
///
/// Now the plugin reports `incomplete: true` (`plugins/go/semantic.go`'s
/// `run`), and that is load-bearing here in a way `--skip-semantic-
/// expectations` cannot route around: `apply_semantic_pass` treats an
/// incomplete *whole-project* pass as an error - the diff it carried is
/// still committed, but the round trip that carried it fails - and the
/// conformance session (`core/src/cli/plugin_check/session.rs`) stops at the
/// first failing step, before `fileChanged #6` and before expectations are
/// ever evaluated. `--skip-semantic-expectations` only changes how `[[
/// callers]]`/`[[implementations]]` entries tagged `tier = "semantic"` are
/// scored once expectations run; it has no say over whether the session
/// reaches them. So passing it or not now produces the identical report:
/// `expectations.file` is `Skip` ("session failed") either way, and the
/// per-entry `Skip`/`Pass` split this test used to check no longer exists to
/// check, because no expectation is evaluated at all.
///
/// That is the correct outcome, not a regression to route around: a
/// conformance run has no basis to certify an index whose one semantic pass
/// never completed, `--skip-semantic-expectations` or not. What this test
/// now proves is narrower and more honest - the plugin says so on the wire,
/// the kit refuses to certify past that point, and nothing *else* the kit
/// checks (shape, stream order, id stability, ownership) is collateral
/// damage from a `go`-free `PATH`.
#[test]
fn the_go_plugin_without_a_toolchain_fails_the_session_check_with_an_incomplete_whole_project_pass() {
    let path = path_without_go();
    // Sanity first (this repo's own rule: a comparison must be shown capable
    // of telling the arms apart before it is relied on) - `go` really is
    // gone from this PATH, so a failure below is not an accident of the
    // toolchain still being reachable some other way (GOROOT, a cached
    // `go/packages` driver, ...).
    assert!(
        !path_has_go(&path),
        "the filtered PATH still resolves `go` - this test would not be discriminating anything: {path}"
    );

    for extra_args in [&["--skip-semantic-expectations"][..], &[][..]] {
        let run = run_check_with_expect_env(
            &go_plugin_dir(),
            &go_conformance_project(),
            &go_conformance_expect(),
            extra_args,
            &[("PATH", &path)],
        );
        assert!(
            !run.success,
            "a whole-project semantic pass that never ran must not be certifiable, \
             extra_args={extra_args:?}:\n{}",
            run.stdout
        );
        assert_eq!(run.outcome("session"), "FAIL", "extra_args={extra_args:?}:\n{}", run.stdout);
        assert!(
            run.stdout.contains("incomplete whole-project semantic pass"),
            "the session must fail for the reason this test is about, not some other regression, \
             extra_args={extra_args:?}:\n{}",
            run.stdout
        );
        assert_eq!(
            run.outcome("expectations.file"),
            "SKIP",
            "expectations need a session that reached the end, extra_args={extra_args:?}:\n{}",
            run.stdout
        );
        // The rest of the contract is unaffected: an honest `incomplete`
        // fails exactly the one check whose job is to notice it, not
        // everything downstream of a `go`-free `PATH`.
        for id in [
            "shape",
            "stream-order",
            "same-file-rule",
            "id-stability.bulk-repeat",
            "id-stability.whitespace-edit",
            "id-stability.deletes-known",
            "id-stability.incremental-matches-bulk",
            "id-stability.declaration-edit-applies",
            "ownership.defines-exports-from-file",
            "ownership.language",
            "ownership.no-container",
            "ownership.diff-stays-in-file",
        ] {
            assert_eq!(run.outcome(id), "PASS", "{id}, extra_args={extra_args:?}:\n{}", run.stdout);
        }
    }
}

/// The bundled Go manifest declares the semantic tier GM-281 built, and
/// core's own generated instructions change accordingly.
///
/// This is the cheap hook GM-281 asked for: the flip to `receiver_calls =
/// "resolved"` is only meaningful through what `mcp::instructions` renders
/// from it, and `instructions::build` is private to `mcp`, so the
/// manifest-side half is asserted here and the rendering half in
/// `core/src/mcp/instructions.rs`'s own unit tests
/// (`go_only_after_its_semantic_pass_omits_the_receiver_gap_entirely` and
/// `go_only_before_its_semantic_pass_still_lists_the_receiver_gap`), which
/// read this same file rather than a transcription of it.
#[test]
fn the_go_manifest_declares_a_semantic_tier_that_resolves_receiver_calls() {
    let manifest = g_mesh::daemon::manifest::read_manifest(&go_plugin_dir())
        .expect("the bundled Go plugin's manifest must be readable");
    assert_eq!(manifest.language, "go");
    assert!(manifest.capabilities.semantic_pass, "core must keep sending semanticPass to the Go plugin");
    assert_eq!(
        manifest.capabilities.receiver_calls,
        ReceiverCallResolution::Resolved,
        "go/types resolves x.M() once the whole-project pass has run"
    );
    assert_eq!(
        manifest.capabilities.receiver_calls_structural,
        ReceiverCallResolution::Unresolved,
        "go/parser alone still emits nothing for a receiver call - it records an open site"
    );
}

/// A deliberately wrong `[[callers]]` entry - one expected caller that does
/// not exist ("ghost"), and the real caller ("user") left off `expect` -
/// fails with a diff naming both the missing and the extra entry, and
/// nothing else in the report is affected.
#[test]
fn a_wrong_expectation_reports_a_readable_diff() {
    let fixture = write_fk_fixture(&[("a.fk", "fn helper\nfn user\ncall helper\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"helper\"\nfile = \"a.fk\"\nexpect = [\"a.fk:ghost\"]\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "a failing expectation must make the command exit non-zero:\n{}", run.stdout);
    assert_eq!(run.outcome("expectations.callers[0]"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("expected: {a.fk:ghost}"), "{}", run.stdout);
    assert!(run.stdout.contains("actual:   {a.fk:user}"), "{}", run.stdout);
    assert!(run.stdout.contains("missing (expected, not found): a.fk:ghost"), "{}", run.stdout);
    assert!(run.stdout.contains("extra (found, not expected): a.fk:user"), "{}", run.stdout);
}

/// Two same-named declarations across files make `symbol_name` resolution
/// ambiguous - the expectation fails with every candidate's id/
/// qualifiedName/filePath/kind printed, never a silent pick (decision 3).
///
/// Once `file` narrows the candidate list to one, the re-call is by that
/// candidate's own `symbol_id` and lands on the one meant. Both
/// `[[definition]]` and `[[callers]]` are exercised, since every tool the
/// expectations call accepts a `symbol_id`.
#[test]
fn an_ambiguous_symbol_fails_with_its_candidates_and_file_disambiguates_it() {
    let fixture = write_fk_fixture(&[("a.fk", "fn shared\n"), ("b.fk", "fn shared\nfn user\ncall shared\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[definition]]\nsymbol = \"shared\"\nexpect = [\"a.fk:shared\"]\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.definition[0]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout.contains("did not resolve to one symbol (resolvedBy = nameAmbiguous)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("no `file` was given to disambiguate"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=a.fk"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=b.fk"), "{}", run.stdout);

    // `file` narrows to one candidate and the re-call by its `symbol_id`
    // resolves it.
    let expect_by_file = write_expect_file(
        fixture.path(),
        "[[definition]]\nsymbol = \"shared\"\nfile = \"a.fk\"\nexpect = [\"a.fk:shared\"]\n",
    );
    let run_by_file = run_check_with_expect(&fake.dir, fixture.path(), &expect_by_file);
    assert_eq!(run_by_file.outcome("expectations.definition[0]"), "PASS", "{}", run_by_file.stdout);

    // `[[callers]]` disambiguates cleanly: it re-calls by the winning
    // candidate's own `symbol_id`, so `file` alone is enough.
    let expect_callers = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"shared\"\nfile = \"b.fk\"\nexpect = [\"b.fk:user\"]\n",
    );
    let run_callers = run_check_with_expect(&fake.dir, fixture.path(), &expect_callers);
    assert_eq!(run_callers.outcome("expectations.callers[0]"), "PASS", "{}", run_callers.stdout);
}

/// Decision 9 (GM-371), both ways round in one run: `[[refusal]]` passes when
/// the tool genuinely declines to resolve a name, and fails on every other
/// shape a call can come back in.
///
/// This is the branch that had no fixture at all - `expectations.rs`'s
/// decision 2 has claimed since GM-277 landed - the 3.5.0, 3.6.0, 3.7.0 and
/// 3.8.0 batches - that a refusal rendered as a zero-element
/// `[[definition]]` set, and it never could: `tool_json` turned
/// `is_error: true` into the same `Err` a dead index produces, so every
/// refusal was a failure and nothing exercised the claim either way.
///
/// The five entries are one per outcome, deliberately in one file so the
/// passing arm and the failing ones are measured against the same index:
///
/// 0. `find_definition` refuses `ghost` (no declaration, no file of that
///    name, and the kit runs with embeddings disabled so the semantic rung
///    cannot offer neighbours) - PASS, the arm that could not exist before.
/// 1. the same tool against `helper`, which *does* resolve - FAIL, naming
///    what it resolved to. This is the half that makes the entry an
///    assertion rather than a wish: a `[[refusal]]` on a name the tool
///    happily answers must not pass.
/// 2. `find_callers` refuses `ghost` too, but the entry demands a phrase no
///    refusal carries - FAIL. Without this, "it errored" would be the whole
///    test, and a dead daemon or a malformed anchor would satisfy it.
/// 3. `find_definition` on `shared`, declared in two files - a candidate
///    page, which is ambiguity rather than absence - FAIL with the
///    candidates listed (decision 3's "never a silent pick").
/// 4. `find_references` refuses `ghost` - PASS, so the category is shown
///    working on more than the one tool, which is the point: GM-367's whole
///    effect on four of five tools was "a name that used to resolve now
///    refuses".
#[test]
fn a_refusal_expectation_passes_only_on_a_real_refusal() {
    let fixture =
        write_fk_fixture(&[("a.fk", "fn helper\n"), ("b.fk", "fn shared\n"), ("c.fk", "fn shared\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[refusal]]\ntool = \"definition\"\nsymbol = \"ghost\"\n\
         contains = [\"no symbol named 'ghost' found\"]\n\n\
         [[refusal]]\ntool = \"definition\"\nsymbol = \"helper\"\n\
         contains = [\"no symbol named\"]\n\n\
         [[refusal]]\ntool = \"callers\"\nsymbol = \"ghost\"\n\
         contains = [\"a phrase no refusal of this tool ever carries\"]\n\n\
         [[refusal]]\ntool = \"definition\"\nsymbol = \"shared\"\n\
         contains = [\"no symbol named\"]\n\n\
         [[refusal]]\ntool = \"references\"\nsymbol = \"ghost\"\n\
         contains = [\"no symbol named 'ghost' found\"]\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);

    assert_eq!(run.outcome("expectations.refusal[0]"), "PASS", "{}", run.stdout);
    assert_eq!(run.outcome("expectations.refusal[4]"), "PASS", "{}", run.stdout);

    assert_eq!(run.outcome("expectations.refusal[1]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout.contains("find_definition resolved the symbol and answered rather than refusing"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("it resolved to: a.fk:helper"), "{}", run.stdout);

    assert_eq!(run.outcome("expectations.refusal[2]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout
            .contains("missing 1 required phrase(s): \"a phrase no refusal of this tool ever carries\""),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("the tool refused with: g-mesh: no symbol named"), "{}", run.stdout);

    assert_eq!(run.outcome("expectations.refusal[3]"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("returned a candidate page rather than refusing"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=b.fk"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=c.fk"), "{}", run.stdout);

    assert!(!run.success, "three failing expectations must exit non-zero:\n{}", run.stdout);
}

/// A typo'd key in `expect.toml` is a hard parse error - `expectations.file`
/// fails with the unknown field named, and no per-expectation check runs at
/// all (decision 5: typos must not pass silently).
#[test]
fn an_unknown_expectation_key_is_a_hard_parse_error() {
    let fixture = write_fk_fixture(&[("a.fk", "fn helper\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"helper\"\nexpect = []\nbogus_field = true\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("bogus_field"), "{}", run.stdout);
    assert!(run.stdout.contains("unknown field"), "{}", run.stdout);
    assert!(
        !run.outcomes.keys().any(|id| id.starts_with("expectations.callers[")),
        "an unparsed file must run no per-expectation check:\n{}",
        run.stdout
    );
}

/// The discrimination case: `[[callers]] symbol = "double"` in the TS
/// fixture's own `expect.toml` expects a caller reached only through a
/// namespace import (`import * as m from "./math"; m.double(4)` in
/// main.ts's `useNamespaceImport`), which the structural pass cannot see at
/// all - only the semantic pass (tsserver) upgrades it to a real edge. This
/// test proves that dependency is real, not assumed: it runs the identical
/// fixture and `expect.toml` against a copy of the TS plugin whose manifest
/// declares `semantic_pass = false`, so `session::run_session` never sends
/// `semanticPass` at all, and shows that this expectation now fails, missing
/// exactly `useNamespaceImport`. Every other `[[callers]]` entry (the
/// same-file call, the cross-file import, both barrel re-export forms - all
/// structural) keeps passing, which is what proves the failure is specific to
/// the semantic-only case and not a blanket breakage from disabling the
/// capability.
///
/// GM-386 added one more entry to the specific side of that line rather than
/// to the unaffected side: the overloaded `format`, whose row set is
/// structural but whose `files` tally is not. It is asserted below by name,
/// with the finding that must accompany it, so "two entries fail here, for
/// two stated reasons" stays a claim this test makes rather than a fact it
/// tolerates.
#[test]
fn a_namespace_import_caller_needs_the_semantic_pass_to_resolve() {
    let plugin = ts_plugin_dir();
    let scratch = tempfile::tempdir().expect("failed to create a temp dir for the no-semantic-pass plugin");
    let dir = scratch.path().join("typescript");
    fs::create_dir_all(&dir).unwrap();
    // Only `plugin.toml` is copied: its `command` names the plugin binary by
    // `${G_MESH_BIN_DIR}`, which does not depend on the manifest's directory.
    // The copy declares `semantic_pass = false` whatever the shipped one says.
    let manifest = fs::read_to_string(plugin.join("plugin.toml")).unwrap();
    let manifest = manifest.replace("semantic_pass = true", "semantic_pass = false");
    assert!(manifest.contains("semantic_pass = false"), "the copy must declare it off: {manifest}");
    fs::write(dir.join("plugin.toml"), manifest).unwrap();

    let run = run_check_with_expect(&dir, &ts_conformance_project(), &ts_conformance_expect());
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.callers[1]"), "FAIL", "{}", run.stdout);
    // The entry's structural half: `run` reaches `double` through the named
    // re-export with no semantic pass, so it is the whole actual set here.
    assert!(
        run.stdout.contains("- actual:   {src/main.ts:run}\n"),
        "callers[1] must still find run through the named re-export:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("missing (expected, not found): src/main.ts:useNamespaceImport"),
        "{}",
        run.stdout
    );

    // GM-386 gave this arm a second, differently-shaped failure, and it is
    // asserted by name rather than dropped from the list below - a list that
    // quietly lost an entry would stop saying anything about it.
    // `expectations.callers[2]` is the overloaded `format`, whose caller SET
    // is structural (one row either way) but whose `files` tally is not: the
    // two overload call sites are two CALLS edges only once tsserver has
    // bound them, and with one edge the tally is not worth sending at all.
    // So that entry asserts the tally, carries `tier = "semantic"` for it,
    // and fails here on the tally alone - with its row set still matching,
    // which is exactly what the finding has to say for this to be evidence
    // rather than noise. See `plugins/typescript/conformance/expect.toml`'s
    // own comment on why no single spelling of that entry is true in both
    // arms.
    assert_eq!(run.outcome("expectations.callers[2]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout.contains("the response carries no files tally at all"),
        "callers[2] must fail on the tally, not on its rows:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("the set itself matched: {src/main.ts:useOverloads}"),
        "callers[2]'s row set is structural and must still match:\n{}",
        run.stdout
    );

    // The other `tier = "semantic"` entries, each missing exactly the rows
    // only tsserver binds: a receiver call on an interface-typed parameter
    // (`Greetable#greet`), a default import under another local name
    // (`MenuGroup`), the branch an ambiguous `export *` barrel picks
    // (`mutate` in src/amb/a.ts), an inherited method reached through `this`
    // and `super` (`Base#hello`), and a non-call namespace member read
    // (`target`).
    for (id, missing) in [
        ("expectations.callers[3]", "src/shapes.ts:viaGreetable"),
        ("expectations.callers[8]", "src/defaults/use.ts:renderGroup"),
        ("expectations.callers[9]", "src/amb/use.ts:useMutate"),
        (
            "expectations.callers[13]",
            "src/inherit/derived.ts:Child#viaThis, src/inherit/derived.ts:Other#viaSuper",
        ),
        ("expectations.references[2]", "src/nsref/use.ts:keep"),
    ] {
        assert_eq!(run.outcome(id), "FAIL", "{id}:\n{}", run.stdout);
        assert!(
            run.stdout.contains(&format!("missing (expected, not found): {missing}")),
            "{id} must fail on {missing}:\n{}",
            run.stdout
        );
    }

    // Every other expectation - the structural ones - is unaffected.
    for id in [
        "expectations.callers[0]",
        "expectations.references[0]",
        "expectations.implementations[0]",
        "expectations.imports[0]",
        // GM-365: the incoming direction is structural too - an `IMPORTS`
        // edge needs no toolchain in any of these languages - so it belongs
        // among the entries a missing semantic tier must not disturb.
        "expectations.importers[0]",
        "expectations.definition[0]",
        "expectations.definition[1]",
        // GM-371: a name the index does not carry is refused whether or not
        // tsserver ran, so this one is structural too.
        "expectations.refusal[0]",
        // Structural behaviours the TypeScript plugin must keep: workspace,
        // `dist/` fallback and `extends`-inherited `paths` resolution
        // (`pointOf`, the two `packages/` imports/importers entries), `.js`
        // and directory specifiers, `Owner.member()`, member spelling,
        // generic heritage, callback/arrow attribution, parameter
        // shadowing, folded dynamic imports, package.json `imports`, and
        // one node per merged or overloaded declaration.
        "expectations.callers[4]",
        "expectations.callers[5]",
        "expectations.callers[6]",
        "expectations.callers[7]",
        "expectations.callers[11]",
        "expectations.callers[12]",
        "expectations.references[1]",
        "expectations.implementations[1]",
        "expectations.imports[1]",
        "expectations.imports[2]",
        "expectations.imports[3]",
        "expectations.imports[4]",
        "expectations.importers[1]",
        "expectations.definition[3]",
        "expectations.definition[4]",
        "expectations.definition[5]",
        "expectations.definition[6]",
        // `mutate` in src/amb/b.ts is empty in both arms: the semantic tier
        // binds the ambiguous import to src/amb/a.ts, the structural pass to
        // neither branch.
        "expectations.callers[10]",
    ] {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}

// --- capabilities.files-created-resolves (GM-516) ---------------------------

const FILES_CREATED: &str = "capabilities.files-created-resolves";

/// The `[files_created]` pair every test below uses unless it says
/// otherwise: `d.fk` imports `c.fk`, both new to the fake fixture.
const FK_PAIR: &str = "[files_created]\ntarget = \"c.fk\"\ntarget_text = \"fn created\\n\"\n\
                       importer = \"d.fk\"\nimporter_text = \"import c.fk\\n\"\n";

/// The fake with `files_created` as given, on `fixture`, with an
/// expectations file holding `expect` - written outside the fixture, so the
/// pair's validation sees exactly the fixture's files.
fn run_files_created(defect: &str, files_created: bool, fixture: &Path, expect: &str) -> Run {
    let fake = install_fake_with(defect, true, files_created);
    let expect_dir = tempfile::tempdir().unwrap();
    let expect = write_expect_file(expect_dir.path(), expect);
    run_check_with_expect(&fake.dir, fixture, &expect)
}

/// No step of the files-created session ran: no warm-up, no `filesCreated`
/// frame, no `fileChanged` of a pair file - so no second plugin process was
/// spawned for it either (the session's first step after its handshake is
/// the warm-up, which is always reported).
fn assert_no_files_created_session(run: &Run) {
    for needle in ["filesCreated", "files-created warm-up", "files-created: "] {
        assert!(
            !run.stdout.contains(needle),
            "no files-created session may run ({needle:?}):\n{}",
            run.stdout
        );
    }
}

/// The position of `needle` in the report, panicking with the report.
fn position(run: &Run, needle: &str) -> usize {
    run.stdout.find(needle).unwrap_or_else(|| panic!("the report carries no {needle:?}:\n{}", run.stdout))
}

/// AC 1, behaviours 1, 4 and 5: a conformant declaring plugin resolves the
/// same-batch new importer, and the session drives it in D5's order - one
/// id-less `filesCreated` listing both paths (importer first), the warm-up
/// `fileChanged` before the pair's, the importer's before the target's. The
/// rest of the report is the baseline, and the fixture is untouched.
///
/// Controls: drop `driver.notify` in `run_files_created_session`, or move it
/// after the pair's `fileChanged`s (the toy then resolves `import c.fk`
/// against a file set without `c.fk`: `FAIL`); drop the warm-up step (its
/// line is missing); route the target before the importer (the order
/// assertion fails); send the notification with an id (`record` no longer
/// counts it as a notification, so its line is missing).
#[test]
fn a_declaring_fake_resolves_an_importer_created_with_its_target() {
    let fixture = fixtures().join("fake");
    let before: Vec<(PathBuf, Vec<u8>)> =
        ["a.fk", "b.fk"].iter().map(|f| (fixture.join(f), fs::read(fixture.join(f)).unwrap())).collect();

    let run = run_files_created("none", true, &fixture, FK_PAIR);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let skipped = id == "capabilities.semantic-pass-undeclared" || id == RESOLUTION_DELTA;
        let expected = if skipped { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);

    assert!(
        run.stdout.contains("files-created: filesCreated (d.fk, c.fk): notification listing d.fk, c.fk"),
        "one id-less filesCreated listing both paths, importer first:\n{}",
        run.stdout
    );
    assert_eq!(run.stdout.matches("notification listing").count(), 1, "{}", run.stdout);
    let warm_up = position(&run, "files-created warm-up: fileChanged (");
    let importer = position(&run, "files-created: fileChanged (d.fk, the new importer) -> fileChanged");
    let target = position(&run, "files-created: fileChanged (c.fk, the new target) -> fileChanged");
    assert!(warm_up < importer && importer < target, "warm-up, importer, then target:\n{}", run.stdout);

    for (path, contents) in before {
        assert_eq!(
            fs::read(&path).unwrap(),
            contents,
            "the kit must never modify the fixture ({})",
            path.display()
        );
    }
    assert!(!fixture.join("c.fk").exists() && !fixture.join("d.fk").exists());
}

/// Behaviour 2: a declaring plugin that ignores the notification fails this
/// check and only this one, and the finding names where the import landed.
/// Control: route the target's `fileChanged` before the importer's (the
/// toy's own `fileChanged` then adds `c.fk` to its file set first, the
/// import resolves, and the check passes).
#[test]
fn a_declaring_fake_that_ignores_files_created_fails_only_that_check() {
    let run = run_files_created("files-created-ignored", true, &fixtures().join("fake"), FK_PAIR);
    assert!(!run.success, "a failing check must make the command exit non-zero:\n{}", run.stdout);
    assert_eq!(run.failing(), vec![FILES_CREATED], "{}", run.stdout);
    assert!(
        run.stdout.contains("notification listing d.fk, c.fk"),
        "the notification was sent:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(
            "IMPORTS lands on Module \"c.fk\" in \"d.fk\" nativeKind external_module (resolved: false)"
        ),
        "{}",
        run.stdout
    );
}

/// AC 2, behaviour 3: a plugin that does not declare the capability is
/// sent nothing - no files-created session runs at all - and the check is
/// `SKIP` "not applicable", even with a pair configured and a defect that
/// would fail it. Control: remove the `manifest.capabilities.files_created`
/// gate in `plugin_check::check` (the session runs and its lines appear).
#[test]
fn a_non_declaring_fake_is_never_sent_files_created() {
    for defect in ["none", "files-created-ignored"] {
        let run = run_files_created(defect, false, &fixtures().join("fake"), FK_PAIR);
        assert!(run.success, "{defect}:\n{}", run.stdout);
        assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{defect}:\n{}", run.stdout);
        assert!(
            run.stdout.contains("not applicable: the manifest declares files_created = false"),
            "{defect}:\n{}",
            run.stdout
        );
        assert_no_files_created_session(&run);
    }
}

/// Behaviour 6: declared but not configured - no `--expect`, an
/// expectations file without the table, or one that does not parse - is
/// `SKIP` "not configured", and no files-created session runs. Control:
/// return `Pass` for `FilesCreatedConfig::Absent`/`Unparsed`.
#[test]
fn a_declaring_fake_without_a_pair_is_skipped_as_not_configured() {
    let fixture = fixtures().join("fake");
    let fake = install_fake_with("none", true, true);
    let run = run_check(&fake.dir, &fixture, &[]);
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{}", run.stdout);
    assert!(run.stdout.contains("not configured: the manifest declares files_created"), "{}", run.stdout);
    assert_no_files_created_session(&run);

    let run = run_files_created(
        "none",
        true,
        &fixture,
        "[[definition]]\nsymbol = \"alpha\"\nexpect = [\"a.fk:alpha\"]\n",
    );
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{}", run.stdout);
    assert!(run.stdout.contains("not configured: the manifest declares files_created"), "{}", run.stdout);
    assert_no_files_created_session(&run);

    let run = run_files_created("none", true, &fixture, "this is not = = toml\n");
    assert_eq!(run.outcome("expectations.file"), "FAIL", "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{}", run.stdout);
    assert!(run.stdout.contains("not configured: the expectations file did not parse"), "{}", run.stdout);
    assert_no_files_created_session(&run);
}

/// Behaviour 7: a pair that cannot run - a target that already exists in
/// the fixture, an importer with no extension the manifest claims - fails
/// the check naming each field, runs no session, and leaves the rest of the
/// expectations file running. Control: skip
/// `session::files_created_pair_findings` in `plugin_check::check`.
#[test]
fn an_invalid_pair_fails_the_check_and_not_the_parse() {
    let expect = "[[definition]]\nsymbol = \"alpha\"\nexpect = [\"a.fk:alpha\"]\n\n\
                  [files_created]\ntarget = \"a.fk\"\ntarget_text = \"fn created\\n\"\n\
                  importer = \"d.txt\"\nimporter_text = \"import a.fk\\n\"\n";
    let run = run_files_created("none", true, &fixtures().join("fake"), expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.failing(), vec![FILES_CREATED], "{}", run.stdout);
    assert!(
        run.stdout.contains("[files_created] target = \"a.fk\" already exists in the fixture"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("[files_created] importer = \"d.txt\" has none of the manifest's extensions"),
        "{}",
        run.stdout
    );
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    assert_eq!(run.outcome("expectations.definition[0]"), "PASS", "{}", run.stdout);
    assert_no_files_created_session(&run);
}

/// Behaviour 8: a files-created session that fails - here writing the
/// target under `a.fk`, which is a file - fails `session` with the reason,
/// skips this check as not reached, and still runs the expectations.
/// Control: drop the push of the run's failure into `failures` in
/// `plugin_check::check` (`session` then passes).
#[test]
fn a_failed_files_created_session_fails_session_and_skips_the_check() {
    let expect = "[[definition]]\nsymbol = \"alpha\"\nexpect = [\"a.fk:alpha\"]\n\n\
                  [files_created]\ntarget = \"a.fk/c.fk\"\ntarget_text = \"fn created\\n\"\n\
                  importer = \"d.fk\"\nimporter_text = \"import a.fk/c.fk\\n\"\n";
    let run = run_files_created("none", true, &fixtures().join("fake"), expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.failing(), vec!["session"], "{}", run.stdout);
    assert!(run.stdout.contains("files-created session: files-created: failed to write"), "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{}", run.stdout);
    assert!(run.stdout.contains("not reached: the files-created session failed"), "{}", run.stdout);
    assert_eq!(run.outcome("expectations.definition[0]"), "PASS", "{}", run.stdout);
}

/// Behaviour 9: the pair never reaches the index the expectations read.
/// `d.fk` imports the fixture's `a.fk` as well as its target, yet
/// `[[importers]]` of `a.fk` stays `b.fk` alone. Control: run the
/// files-created steps on the main index (`conn`) instead of a fresh one.
#[test]
fn the_pair_never_reaches_the_expectations_index() {
    let fixture = write_fk_fixture(&[("a.fk", "fn alpha\n"), ("b.fk", "import a.fk\nfn beta\n")]);
    let expect = "[[importers]]\nfile = \"a.fk\"\nexpect = [\"b.fk\"]\n\n\
                  [files_created]\ntarget = \"c.fk\"\ntarget_text = \"fn created\\n\"\n\
                  importer = \"d.fk\"\nimporter_text = \"import c.fk\\nimport a.fk\\n\"\n";
    let run = run_files_created("none", true, fixture.path(), expect);
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "PASS", "the pair did run:\n{}", run.stdout);
    assert_eq!(run.outcome("expectations.importers[0]"), "PASS", "{}", run.stdout);
}

/// Behaviour 10: `[files_created]` accepts no key beyond its four, like
/// every other table - a parse error, so the check is "did not parse".
/// Control: drop `deny_unknown_fields` from `FilesCreatedPair`.
#[test]
fn an_unknown_files_created_key_is_a_parse_error() {
    let run =
        run_files_created("none", true, &fixtures().join("fake"), &format!("{FK_PAIR}bogus_field = 1\n"));
    assert_eq!(run.outcome("expectations.file"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("bogus_field") && run.stdout.contains("unknown field"), "{}", run.stdout);
    assert_eq!(run.outcome(FILES_CREATED), "SKIP", "{}", run.stdout);
    assert_no_files_created_session(&run);
}

/// The TS plugin's manifest as shipped (`resolution_delta = true`) but with
/// `semantic_pass` off, so no language server is involved, in a directory
/// named after its language.
fn ts_plugin_without_semantic_pass() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a temp dir for the plugin");
    let dir = root.path().join("typescript");
    fs::create_dir_all(&dir).unwrap();
    let shipped = fs::read_to_string(ts_plugin_dir().join("plugin.toml")).unwrap();
    assert!(shipped.contains("resolution_delta = true"), "the shipped manifest declares resolution_delta");
    assert!(shipped.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
    fs::write(dir.join("plugin.toml"), shipped.replace("semantic_pass = true", "semantic_pass = false"))
        .unwrap();
    root
}

/// The TS plugin answers `unchanged` to a version-only edit of the fixture's
/// `package.json`, and the report names the bumped file and the answer. The
/// fixture itself is never modified.
///
/// Control: project the whole manifest in the TS plugin's
/// `facts::package_facts` (the bump answers `affected`: FAIL).
#[test]
fn the_typescript_plugin_answers_unchanged_to_a_version_bump() {
    let fixture = ts_conformance_project();
    let package_json = fixture.join("package.json");
    let before = fs::read(&package_json).unwrap();

    let plugins = ts_plugin_without_semantic_pass();
    let run = run_check(&plugins.path().join("typescript"), &fixture, &[]);
    assert_eq!(run.outcome(RESOLUTION_DELTA), "PASS", "{}", run.stdout);
    assert!(run.stdout.contains("resolution-delta: version bump of package.json"), "{}", run.stdout);
    assert!(
        run.stdout.contains(r#"resolution-delta: resolutionChanged -> {"kind":"unchanged"}"#),
        "{}",
        run.stdout
    );
    assert_eq!(fs::read(&package_json).unwrap(), before, "the kit must never modify the fixture");
}

/// A plugin declaring `resolution_delta` whose bulk walk writes no
/// `resolutionFacts` trailer fails this check, and only this one.
///
/// Control: treat a missing trailer as `NoWatchFile` in `plugin_check::check`
/// (the check reports SKIP).
#[test]
fn a_declaring_plugin_without_a_facts_trailer_fails_only_the_resolution_delta_check() {
    let fake = install_fake("none", true);
    let manifest = fake.dir.join("plugin.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    assert!(text.trim_end().ends_with("files_created = false"), "the capabilities table is last:\n{text}");
    text.push_str("resolution_delta = true\n\n[plugin.workspace]\nwatch_files = [\"package.json\"]\n");
    fs::write(&manifest, text).unwrap();

    let source = fixtures().join("fake");
    let fixture = write_fk_fixture(&[
        ("a.fk", &fs::read_to_string(source.join("a.fk")).unwrap()),
        ("b.fk", &fs::read_to_string(source.join("b.fk")).unwrap()),
        ("package.json", r#"{"name":"fk","version":"1.0.0"}"#),
    ]);
    let run = run_check(&fake.dir, fixture.path(), &[]);
    assert!(!run.success, "a failing check must make the command exit non-zero:\n{}", run.stdout);
    assert_eq!(run.failing(), vec![RESOLUTION_DELTA], "{}", run.stdout);
    assert!(run.stdout.contains("without a resolutionFacts line"), "{}", run.stdout);
}
