//! The TypeScript plugin's acceptance test: the Rust binary run through the
//! real `g-mesh plugins check`, against `conformance/project` and
//! `conformance/expect.toml`, in the three configurations
//! `plugins/python/tests/conformance.rs` runs for the Python plugin.
//!
//! - [`semantic`] - the manifest this plugin ships: `semantic_pass = true`
//!   and `[plugin.semantic]` read from `plugin.toml`, with the command
//!   pointed at a real vtsls. Every expectation runs and passes.
//! - [`structural`] - the same binary under a manifest with no semantic
//!   tier: core never sends a `semanticPass`. Run with
//!   `--skip-semantic-expectations`, where every structural entry passes, and
//!   without it, where every `tier = "semantic"` entry must **fail**.
//! - [`missing_toolchain`] - the shipped manifest with the command pointed at
//!   a path that does not exist: the plugin really fails to find a server,
//!   reports it once, and answers an empty, incomplete pass. As in Python's
//!   arm, that incomplete whole-project pass fails `session` and skips the
//!   expectations, so the arm asserts the shape of the degradation instead.
//!
//! vtsls is a test dependency of this crate, installed by
//! `scripts/test-deps.sh typescript` into `plugins/typescript/node_modules`
//! (not into the fixture, whose copy would drop npm's `.bin` symlinks). When
//! there is none the tests fail naming that command rather than skipping.
//!
//! `g-mesh` itself is found as the kit finds it: `G_MESH_BIN` if set, else the
//! binary beside this test's own target directory (`cargo build -p g-mesh`).

use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::testing::{CheckOutcome, PluginCheck, Verdict};
use g_mesh_plugin_typescript::extractor::grammar::EXTENSIONS;
use g_mesh_plugin_typescript::project::{EXCLUDE_DIRS, WATCH_FILES};

const EXPECT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/expect.toml");
const MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml");

/// Every check `g-mesh plugins check` reports outside `expectations.*`.
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

/// Exactly one of the two applies to a given manifest; the other reports
/// `Skip`.
const CAPABILITY_CHECKS: [&str; 2] =
    ["capabilities.semantic-pass-undeclared", "capabilities.semantic-engine-lazy"];

/// How many entries `conformance/expect.toml` carries, and how many are
/// tagged `tier = "semantic"`. Hand-written so an entry added without a
/// decision about its tier fails
/// [`the_expectation_constants_describe_the_file_they_count`] first.
///
/// GM-325: `Greetable#greet` became semantic (vtsls resolves the receiver
/// call), `mutate` in `src/amb/b.ts` lost its tag (empty in both arms), and
/// `Base#hello` was added, semantic.
const EXPECTATIONS: usize = 35;
const SEMANTIC_EXPECTATIONS: usize = 7;

fn always_pass() -> Vec<&'static str> {
    ALL_CHECKS.iter().copied().filter(|id| !CAPABILITY_CHECKS.contains(id)).collect()
}

/// `plugin.toml`'s query tables (`non_symbol_queries`,
/// `symbol_query_prefixes`, `reexports`) and, when `server` is given, its
/// `[plugin.semantic]` with `command` set to `server` - re-serialized from the
/// shipped file rather than copied, so a run checks what users get.
fn manifest_extra(server: Option<&Path>) -> String {
    let text = std::fs::read_to_string(MANIFEST).expect("plugins/typescript/plugin.toml is readable");
    let parsed: toml::Value = toml::from_str(&text).expect("plugins/typescript/plugin.toml parses");
    let mut plugin = toml::map::Map::new();
    for table in ["non_symbol_queries", "symbol_query_prefixes", "reexports"] {
        plugin.insert(table.to_string(), parsed["plugin"][table].clone());
    }
    if let Some(server) = server {
        let mut section = parsed["plugin"]["semantic"].clone();
        section["command"] = toml::Value::String(server.to_string_lossy().into_owned());
        plugin.insert("semantic".to_string(), section);
    }
    let mut root = toml::map::Map::new();
    root.insert("plugin".to_string(), toml::Value::Table(plugin));
    toml::to_string(&toml::Value::Table(root)).expect("the tables re-serialize")
}

fn base() -> PluginCheck {
    PluginCheck::new(
        "typescript",
        env!("CARGO_BIN_EXE_g-mesh-plugin-typescript"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/project"),
    )
    .extensions(&EXTENSIONS)
    .exclude_dirs(&EXCLUDE_DIRS)
    .watch_files(&WATCH_FILES)
    .entry_points(&["index"])
}

/// The shipped configuration, with a real vtsls.
fn semantic() -> PluginCheck {
    base()
        .semantic_pass(true)
        .receiver_calls("resolved", "unresolved")
        .manifest_extra(manifest_extra(Some(&vtsls())))
}

/// No semantic tier: core never asks for one and the structural tier answers
/// alone.
fn structural() -> PluginCheck {
    base().semantic_pass(false).receiver_calls("unresolved", "unresolved").manifest_extra(manifest_extra(None))
}

/// The shipped configuration on a machine with no vtsls.
fn missing_toolchain() -> PluginCheck {
    let nowhere = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/there-is-no-vtsls-here");
    base()
        .semantic_pass(true)
        .receiver_calls("resolved", "unresolved")
        .manifest_extra(manifest_extra(Some(&nowhere)))
}

/// The vtsls these tests run: this crate's `node_modules/.bin` first, then
/// `PATH`, each bare and (on Windows) as the `.cmd` npm writes, proved by
/// `vtsls --version` - spelled here rather than through the plugin's own
/// candidate list, so a bug in that list cannot pass the test built to catch
/// it.
fn vtsls() -> PathBuf {
    let local = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    let extensions: &[&str] = if cfg!(windows) { &["", ".cmd"] } else { &[""] };
    let mut tried = Vec::new();
    for dir in [Some(local), None] {
        for extension in extensions {
            let server = match dir {
                Some(dir) => dir.join(format!("vtsls{extension}")),
                None => PathBuf::from(format!("vtsls{extension}")),
            };
            tried.push(server.clone());
            let usable = std::process::Command::new(&server)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success());
            if usable {
                return server;
            }
        }
    }
    panic!(
        "these tests drive a real vtsls and there is none that works (tried {tried:?}). Install it \
         with `scripts/test-deps.sh typescript` from the repository root (`npm ci` of the version \
         plugins/typescript/package.json pins, as CI does)."
    )
}

/// `(entries, entries tagged tier = "semantic")` in [`EXPECT`], parsed rather
/// than grepped: the tag also appears in the file's comments.
fn count_expectations() -> (usize, usize) {
    let text = std::fs::read_to_string(EXPECT).expect("conformance/expect.toml is readable");
    let parsed: toml::Value = toml::from_str(&text).expect("conformance/expect.toml parses");
    let mut total = 0;
    let mut semantic = 0;
    for value in parsed.as_table().expect("expect.toml is a table").values() {
        let Some(entries) = value.as_array() else { continue };
        for entry in entries {
            total += 1;
            if entry.get("tier").and_then(toml::Value::as_str) == Some("semantic") {
                semantic += 1;
            }
        }
    }
    (total, semantic)
}

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

/// The same fifteen checks for every configuration, with exactly one
/// capability check skipping: the one that does not match the manifest.
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
fn the_expectation_constants_describe_the_file_they_count() {
    let (entries, semantic) = count_expectations();
    assert_eq!(
        entries, EXPECTATIONS,
        "conformance/expect.toml carries {entries} entries but EXPECTATIONS says {EXPECTATIONS}: decide \
         whether the new entry needs the semantic tier (`tier = \"semantic\"`), then set EXPECTATIONS \
         and SEMANTIC_EXPECTATIONS to match"
    );
    assert_eq!(
        semantic, SEMANTIC_EXPECTATIONS,
        "conformance/expect.toml tags {semantic} entries `tier = \"semantic\"` but \
         SEMANTIC_EXPECTATIONS says {SEMANTIC_EXPECTATIONS}"
    );
}

/// The shipped manifest with a real vtsls: every check that applies passes,
/// and every expectation is judged and passes.
///
/// `useSyntaxServer = "never"` in `plugin.toml` is load-bearing here: with
/// the `[plugin.semantic.settings.""]` table deleted, a cold vtsls answers a
/// default import's and an ambiguous barrel's hop site with the import
/// binding itself, the bridge upholds the structural edge, and the
/// `MenuGroup` and `src/amb/a.ts` `mutate` entries fail (GM-325/S9 item 19).
#[test]
fn the_linked_index_answers_every_expectation_with_vtsls() {
    let outcome = semantic().expect(EXPECT).run().expect("the conformance kit could not be run");
    outcome.assert_conformant();
    assert_report_shape(
        &outcome,
        "capabilities.semantic-engine-lazy",
        "capabilities.semantic-pass-undeclared",
    );

    let judged = expectation_verdicts(&outcome);
    assert_eq!(judged.len(), EXPECTATIONS + 1, "every expectation must be reported:\n{}", outcome.stdout);
    for (id, verdict) in judged {
        assert_eq!(verdict, Verdict::Pass, "{id} was not judged and passed:\n{}", outcome.stdout);
    }
}

#[test]
fn without_a_semantic_tier_the_structural_expectations_still_hold() {
    let outcome = structural()
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
    assert_eq!(judged.len(), EXPECTATIONS + 1, "every expectation must be reported:\n{}", outcome.stdout);
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
}

/// Every entry tagged semantic really needs the tier: against the structural
/// manifest, without the skip flag, each one fails, and nothing else does.
/// The rows only the semantic tier finds are named, by the row each entry is
/// missing rather than by an index a later edit would renumber.
#[test]
fn the_semantic_tier_is_what_the_semantic_entries_need() {
    let outcome = structural().expect(EXPECT).run().expect("the conformance kit could not be run");

    let judged = expectation_verdicts(&outcome);
    assert_eq!(judged.len(), EXPECTATIONS + 1, "every expectation must be judged:\n{}", outcome.stdout);
    assert_eq!(
        verdicts_of(&judged, Verdict::Fail),
        SEMANTIC_EXPECTATIONS,
        "every semantic entry must fail without the tier, and only those:\n{}",
        outcome.stdout
    );
    assert!(!outcome.success, "and the run as a whole must fail:\n{}", outcome.stdout);

    for row in [
        // a namespace member call, and a namespace member read
        "missing (expected, not found): src/main.ts:useNamespaceImport",
        "missing (expected, not found): src/nsref/use.ts:keep",
        // a default import under another name, and an ambiguous barrel
        "missing (expected, not found): src/defaults/use.ts:renderGroup",
        "missing (expected, not found): src/amb/use.ts:useMutate",
        // a receiver call on an interface-typed parameter
        "missing (expected, not found): src/shapes.ts:viaGreetable",
        // an inherited method through `this` and through `super`
        "missing (expected, not found): src/inherit/derived.ts:Child#viaThis",
        "missing (expected, not found): src/inherit/derived.ts:Other#viaSuper",
        // the overloaded `format`: one row either way, two edges only bound
        "the response carries no files tally at all",
    ] {
        assert!(outcome.stdout.contains(row), "the report must say `{row}`:\n{}", outcome.stdout);
    }
}

/// A missing server degrades to structural and says so once, with an empty
/// diff and no hang. One check fails, `session`, because the incomplete
/// whole-project pass is a session failure in the kit; the lazy check still
/// passes, since nothing was started before the first `semanticPass`.
#[test]
fn a_missing_vtsls_is_reported_once_and_degrades_to_structural() {
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
    assert!(report.contains("no usable vtsls"), "{report}");
    assert!(report.contains("npm install -g @vtsls/language-server"), "the remedy is named: {report}");
    assert!(report.contains("plugins/typescript/plugin.toml"), "and where to point it: {report}");

    assert!(
        outcome.stdout.contains("semanticPass: +0 / -0 node(s), +0 / -0 edge(s)"),
        "the pass answered an empty diff:\n{}",
        outcome.stdout
    );
}
