//! The Python plugin's own acceptance test: run through the real `g-mesh
//! plugins check`, the same way `plugins/rust`'s `tests/conformance.rs`
//! proves the Rust plugin and `plugins/sdk`'s own toy plugin proves the SDK.
//!
//! # Three configurations, because a semantic tier has three things to say
//!
//! Since GM-299 this plugin has a pyright tier, and the kit is run against the
//! same fixture and the same `conformance/expect.toml` in three shapes - the
//! arrangement `plugins/rust/tests/conformance.rs` settled for GM-290, kept
//! deliberately identical so the two plugins' reports can be read side by
//! side:
//!
//! - [`semantic`] - the manifest this plugin ships, read from
//!   `plugins/python/plugin.toml` rather than copied, so a run checks the
//!   configuration a user actually gets. All ten expectations run and pass.
//! - [`structural_3_4_0`] - the manifest as 3.4.0 declared it:
//!   `semantic_pass = false`, no `[plugin.semantic]` at all. Core never sends
//!   a `semanticPass`, so this is the same plugin binary answering with its
//!   structural tier alone. Run two ways - with
//!   `--skip-semantic-expectations`, where every structural entry must pass
//!   ([`without_a_semantic_tier_the_structural_expectations_still_hold`]),
//!   and without it, where every semantic entry must **fail**
//!   ([`the_semantic_tier_is_what_closes_the_receiver_call_gap`]).
//! - [`missing_toolchain`] - `semantic_pass = true` with the server command
//!   pointed at a path that does not exist. Nothing is mocked: the plugin
//!   really does fail to resolve a server, and what is asserted is the
//!   degradation's exact shape.
//!
//! # Why the degradation arm does not assert expectations
//!
//! It cannot, and that is the kit's rule rather than a gap here. A plugin
//! with no engine reports its whole-project `semanticPass` **incomplete** -
//! deliberately, since that is what leaves `language_state.semanticPassAt`
//! unset and Python's receiver gap listed - and `watcher::apply` turns an
//! incomplete *whole-project* pass into a session failure, after which the kit
//! skips the entire expectations section. So the missing-toolchain arm asserts
//! the log line, the diff and the report; the *structural results* are
//! asserted by [`structural_3_4_0`], which is a stronger statement anyway - it
//! is the 3.4.0 manifest, unchanged, answering the 3.4.0 expectations.
//!
//! One consequence worth stating plainly, because it is a real cost: with
//! `semantic_pass = true` and no pyright installed, `g-mesh plugins check
//! plugins/python` reports FAIL on `session`. That is the kit telling the
//! truth about the environment rather than about the plugin, and
//! [`a_missing_pyright_is_reported_once_and_degrades_to_structural`] pins it
//! to exactly that one check so a second failure could never hide behind it.
//!
//! # pyright is a test dependency of this crate
//!
//! [`semantic`]'s arm needs one, and when there is none these tests fail
//! naming `npm install pyright` rather than skipping. The repository already
//! treats Node, Go and rust-analyzer that way, and the kit's own doctrine is
//! that a conformance check which passes because it did not run is the failure
//! this whole thing exists to remove.
//!
//! **Where it is installed matters, and not for taste.** It goes in
//! `plugins/python/node_modules` (gitignored), *not* in
//! `conformance/project/node_modules`, because the kit runs a plugin against a
//! scratch **copy** of the fixture and `session::copy_tree` skips symlinks
//! deliberately - and an npm `.bin` directory on unix is nothing but symlinks,
//! so a project-local install would not survive the copy. The resolution
//! branch that reads a project's own `node_modules/.bin` is therefore proved
//! against a real install in `src/semantic.rs`'s
//! `a_project_local_install_is_found_and_probed_for_real`, which builds its own
//! tree, and the arms here pass the server's absolute path the same way
//! `plugins/rust` passes rust-analyzer's.

use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::testing::{CheckOutcome, PluginCheck, Verdict};

/// Every check `g-mesh plugins check` reports, in no particular order -
/// asserted as a set so a check dropping out of the report (this crate's own
/// regression, not a plugin defect) fails loudly rather than shrinking the
/// loop below silently.
const ALL_CHECKS: [&str; 15] = [
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
];

/// The two capability checks are each other's alternative: exactly one applies
/// to a given manifest, and the other reports `Skip`. Which one runs is the
/// most compressed statement of what GM-299 changed - through 3.4.0 this
/// plugin ran `semantic-pass-undeclared`, and it now runs
/// `semantic-engine-lazy`.
const CAPABILITY_CHECKS: [&str; 2] =
    ["capabilities.semantic-pass-undeclared", "capabilities.semantic-engine-lazy"];

/// Every check that must pass whatever the manifest declares.
fn always_pass() -> Vec<&'static str> {
    ALL_CHECKS.iter().copied().filter(|id| !CAPABILITY_CHECKS.contains(id)).collect()
}

const EXPECT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/expect.toml");

/// How many entries `conformance/expect.toml` carries, and how many of them
/// are tagged `tier = "semantic"`. Asserted rather than assumed, so an entry
/// added without a decision about its tier fails here first.
const EXPECTATIONS: usize = 10;
const SEMANTIC_EXPECTATIONS: usize = 3;

/// The `[plugin.semantic]` section of the manifest this plugin ships, as TOML.
///
/// Read from `plugin.toml` and re-serialized rather than written out again
/// here, so these tests check the configuration a user gets - the `--stdio`
/// pyright cannot start without, the deliberately empty `implementation_kinds`,
/// and the `settings` table that is the only channel pyright reads at all. A
/// copy would drift, and the first sign of the drift would be a conformance
/// run passing against a configuration nobody ships.
///
/// `command` is overridden when `server` is given, which is the whole of how
/// the semantic and missing-toolchain arms differ.
fn semantic_section(server: Option<&Path>) -> String {
    let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    let text = std::fs::read_to_string(manifest).expect("plugins/python/plugin.toml is readable");
    let parsed: toml::Value = toml::from_str(&text).expect("plugins/python/plugin.toml parses");
    let mut section = parsed["plugin"]["semantic"].clone();
    if let Some(server) = server {
        section["command"] = toml::Value::String(server.to_string_lossy().into_owned());
    }
    let mut plugin = toml::map::Map::new();
    plugin.insert("semantic".to_string(), section);
    let mut root = toml::map::Map::new();
    root.insert("plugin".to_string(), toml::Value::Table(plugin));
    toml::to_string(&toml::Value::Table(root)).expect("the section re-serializes")
}

fn base() -> PluginCheck {
    PluginCheck::new(
        "python",
        env!("CARGO_BIN_EXE_g-mesh-plugin-python"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/project"),
    )
    .extensions(&[".py", ".pyi"])
    .exclude_dirs(&[".venv", "venv", "__pycache__", ".tox", ".mypy_cache", "site-packages", "node_modules"])
    .watch_files(&["pyproject.toml", "setup.cfg", "setup.py"])
    .entry_points(&["__init__"])
}

/// The shipped configuration, with a real pyright.
fn semantic() -> PluginCheck {
    let server = pyright_langserver();
    base()
        .semantic_pass(true)
        .receiver_calls("resolved", "unresolved")
        .manifest_extra(semantic_section(Some(&server)))
}

/// The manifest as 3.4.0 declared it: no semantic tier, so core never asks for
/// one and the structural tier answers alone.
fn structural_3_4_0() -> PluginCheck {
    base().semantic_pass(false).receiver_calls("unresolved", "unresolved")
}

/// The shipped configuration on a machine with no pyright.
fn missing_toolchain() -> PluginCheck {
    let nowhere = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/there-is-no-pyright-here");
    base()
        .semantic_pass(true)
        .receiver_calls("resolved", "unresolved")
        .manifest_extra(semantic_section(Some(&nowhere)))
}

/// The pyright-langserver these tests need.
///
/// Resolved from this crate's own `node_modules` first (where the module doc
/// says to install it, and the only place a checkout can be sure of), and from
/// `PATH` otherwise, for a machine with a global install. Each is proved the
/// way the plugin itself proves one: by running the *CLI twin*, because
/// `pyright-langserver --version` exits 1 with "Connection input stream is not
/// set" and would reject a perfectly good server.
///
/// Deliberately not a `None` that turns into a skip: see this file's module
/// doc.
fn pyright_langserver() -> PathBuf {
    let local = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    let candidates = [local.join("pyright-langserver"), PathBuf::from("pyright-langserver")];
    for candidate in &candidates {
        let twin = candidate.with_file_name("pyright");
        let usable = std::process::Command::new(&twin)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if usable {
            return candidate.clone();
        }
    }
    panic!(
        "these tests drive a real pyright and there is none that works: tried {candidates:?} through \
         their `pyright --version` twin. Install it with `npm install pyright` run in \
         plugins/python (node_modules/ is gitignored there). Note that `pyright-langserver \
         --version` is NOT a way to check - it has no such flag and always exits 1."
    )
}

/// Every `expectations.*` verdict the report carries, by check id.
fn expectation_verdicts(outcome: &CheckOutcome) -> Vec<(&str, Verdict)> {
    outcome
        .outcomes
        .iter()
        .filter(|(id, _)| id.starts_with("expectations."))
        .map(|(id, verdict)| (id.as_str(), *verdict))
        .collect()
}

fn verdicts_of(judged: &[(&str, Verdict)], want: Verdict) -> usize {
    judged.iter().filter(|(_, verdict)| *verdict == want).count()
}

/// The whole report, for every configuration: the same fifteen checks, with
/// exactly one capability check skipping, and it must be the one that matches
/// the manifest.
fn assert_report_shape(outcome: &CheckOutcome, applicable: &str, not_applicable: &str) {
    let mut reported: Vec<&str> = outcome.outcomes.keys().map(String::as_str).collect();
    reported.retain(|id| !id.starts_with("expectations."));
    reported.sort_unstable();
    let mut expected = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(reported, expected, "the report must carry every check exactly once:\n{}", outcome.stdout);

    for id in always_pass() {
        assert_eq!(outcome.verdict(id), Some(Verdict::Pass), "{id} did not pass:\n{}", outcome.stdout);
    }
    assert_eq!(
        outcome.verdict(applicable),
        Some(Verdict::Pass),
        "{applicable} applies to this manifest and must pass:\n{}",
        outcome.stdout
    );
    assert_eq!(
        outcome.verdict(not_applicable),
        Some(Verdict::Skip),
        "{not_applicable} does not apply to this manifest:\n{}",
        outcome.stdout
    );
}

#[test]
fn the_plugin_passes_every_check_that_applies_to_it() {
    let outcome = semantic().run().expect("the conformance kit could not be run");
    outcome.assert_conformant();
    assert_report_shape(
        &outcome,
        "capabilities.semantic-engine-lazy",
        "capabilities.semantic-pass-undeclared",
    );
}

/// The acceptance criteria, as assertions against the *linked* index: see
/// `conformance/expect.toml`, which says what each one proves.
#[test]
fn the_linked_index_answers_the_acceptance_criteria() {
    let outcome = semantic().expect(EXPECT).run().expect("the conformance kit could not be run");
    outcome.assert_conformant();

    // `assert_conformant` fails on a FAIL and says nothing about a SKIP, and
    // the whole expectations section is skipped when the session did not reach
    // a state worth judging. So each entry is asserted to have been *judged*:
    // `expectations.file` (the file parsed at all) plus every entry
    // `conformance/expect.toml` carries, each reported under its own
    // `expectations.<kind>[<index>]` id.
    let judged = expectation_verdicts(&outcome);
    assert_eq!(judged.len(), EXPECTATIONS + 1, "every expectation must be reported:\n{}", outcome.stdout);
    for (id, verdict) in judged {
        assert_eq!(verdict, Verdict::Pass, "{id} was not judged and passed:\n{}", outcome.stdout);
    }
}

/// Without a semantic tier the structural results are the 3.4.0 results: every
/// entry `expect.toml` leaves untagged still passes, and the three semantic
/// ones report `Skip` rather than being quietly dropped.
///
/// The manifest here is 3.4.0's own - `semantic_pass = false`, no
/// `[plugin.semantic]` - so this is the plugin binary at this commit answering
/// the release before it, with its structural tier alone. It needs no
/// toolchain and cannot be affected by one being present.
#[test]
fn without_a_semantic_tier_the_structural_expectations_still_hold() {
    let outcome = structural_3_4_0()
        .expect(EXPECT)
        .skip_semantic_expectations(true)
        .run()
        .expect("the conformance kit could not be run");
    outcome.assert_conformant();
    assert_report_shape(
        &outcome,
        "capabilities.semantic-pass-undeclared",
        "capabilities.semantic-engine-lazy",
    );

    let judged = expectation_verdicts(&outcome);
    assert_eq!(
        judged.len(),
        EXPECTATIONS + 1,
        "every expectation must still be reported:\n{}",
        outcome.stdout
    );
    assert_eq!(
        verdicts_of(&judged, Verdict::Skip),
        SEMANTIC_EXPECTATIONS,
        "exactly the semantic entries skip:\n{}",
        outcome.stdout
    );
    assert_eq!(
        verdicts_of(&judged, Verdict::Pass),
        EXPECTATIONS + 1 - SEMANTIC_EXPECTATIONS,
        "the file plus every structural entry passes:\n{}",
        outcome.stdout
    );
    assert_eq!(verdicts_of(&judged, Verdict::Fail), 0, "nothing may fail:\n{}", outcome.stdout);
}

/// The discrimination: the entries this file calls semantic really do need the
/// semantic tier.
///
/// Same expectation file, same fixture, same structural-only manifest - and no
/// `--skip-semantic-expectations`, so every `tier = "semantic"` entry is
/// evaluated against a purely structural index and must **fail**. A suite where
/// this passed would be one whose semantic expectations were being answered by
/// something else, and the whole of GM-299 would be unmeasured.
#[test]
fn the_semantic_tier_is_what_closes_the_receiver_call_gap() {
    let outcome = structural_3_4_0().expect(EXPECT).run().expect("the conformance kit could not be run");

    let judged = expectation_verdicts(&outcome);
    assert_eq!(judged.len(), EXPECTATIONS + 1, "every expectation must be judged:\n{}", outcome.stdout);
    assert_eq!(
        verdicts_of(&judged, Verdict::Fail),
        SEMANTIC_EXPECTATIONS,
        "every semantic entry must fail without the tier, and only those:\n{}",
        outcome.stdout
    );
    assert!(!outcome.success, "and the run as a whole must fail:\n{}", outcome.stdout);

    // Named, not only counted - and named by the row each entry is *missing*
    // rather than by the entry's own index, which is what the report prints and
    // what a later edit to the file would renumber. These three rows are this
    // task's own acceptance criteria, one per shape of receiver:
    //
    //   - a local whose type comes from its initializer;
    //   - a parameter annotated with a base class, and a receiver that is a
    //     call result;
    //   - the same call spelling resolved to a *subclass's* override.
    for row in [
        "missing (expected, not found): pkg/callers.py:through_a_variable",
        "missing (expected, not found): pkg/callers.py:through_a_base_annotation",
        "missing (expected, not found): pkg/callers.py:through_a_subclass",
    ] {
        assert!(outcome.stdout.contains(row), "the report must say `{row}`:\n{}", outcome.stdout);
    }
}

/// A missing server degrades to structural and says so **once**, with an empty
/// diff and no hang.
///
/// Each half of that promise, checked against the plugin's own stderr as the
/// kit passed it through: one report, an actionable one, and no second report
/// however many passes the session drives (it sends a whole-project
/// `semanticPass` and then a `fileChanged` through the semantic gate). The
/// absence of a hang is checked by the run finishing at all - the kit's own
/// timeouts are what would otherwise have caught it.
///
/// One check fails, and only one: see this file's module doc on why an
/// incomplete whole-project pass is a session failure and why that is the
/// honest report.
#[test]
fn a_missing_pyright_is_reported_once_and_degrades_to_structural() {
    let outcome = missing_toolchain().run().expect("the conformance kit could not be run");

    assert_eq!(
        outcome.failures(),
        vec!["session"],
        "exactly one check may fail, and only because the pass could not complete:\n{}",
        outcome.stdout
    );
    for id in always_pass().into_iter().filter(|id| *id != "session") {
        assert_eq!(outcome.verdict(id), Some(Verdict::Pass), "{id} did not pass:\n{}", outcome.stdout);
    }
    // The engine was never started, so the lazy check still has its evidence:
    // a degraded plugin must still be a well-behaved one.
    assert_eq!(
        outcome.verdict("capabilities.semantic-engine-lazy"),
        Some(Verdict::Pass),
        "{}",
        outcome.stdout
    );

    let reports: Vec<&str> = outcome
        .stderr
        .lines()
        .filter(|line| line.contains("the semantic engine could not be started"))
        .collect();
    assert_eq!(reports.len(), 1, "exactly one report, not one per pass:\n{}", outcome.stderr);
    let report = reports[0];
    assert!(report.contains("no usable pyright"), "{report}");
    assert!(report.contains("npm install pyright"), "the remedy is named: {report}");
    assert!(
        report.contains("structurally only for the rest of this process's life"),
        "and the consequence: {report}"
    );

    // An empty diff, not a partial one: the pass upserted nothing at all.
    assert!(
        outcome.stdout.contains("semanticPass: +0 / -0 node(s), +0 / -0 edge(s)"),
        "the pass answered an empty diff:\n{}",
        outcome.stdout
    );
}
