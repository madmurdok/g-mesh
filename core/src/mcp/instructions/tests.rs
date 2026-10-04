use super::*;

/// This module's own baseline rendering, byte for byte - transcribed by
/// hand rather than derived from the paragraph constants, so a
/// transcription slip in one of them cannot accidentally agree with
/// itself. This is the fixture
/// [`ts_only_is_byte_identical_to_the_original_string`] checks [`build`]
/// against: a TypeScript-only index with nothing absent or failed, the
/// common case (ADR 0022 re-pinned it).
const ORIGINAL_INSTRUCTIONS: &str =
    "Structural code-graph queries over this project's index. Prefer these over \
grepping when you need definitions, references, call edges or imports.\n\n\
Indexed here: typescript. g-mesh has no answers about files in any other language.\n\n\
A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
(excludes other same-named declarations' call sites, same guarantee either \
way), is already resolved per call site to that exact declaration - do not \
re-check it with grep as a routine habit. Only fall back to grep for the one \
specific gap below, never as a general double-check.\n\n\
The one legitimate reason to grep afterward: in typescript, a method call through a variable \
receiver (`x.foo()`) may produce no edge, so a method's caller/reference list there can \
under-report; bare function calls and this/super/qualified-type calls have no such gap, and \
for those `hasMore: false` without `unlinkedUsages` is exhaustive.\n\n\
Efficient usage: pass `symbol_name` directly to \
find_references/find_callers/find_callees/find_implementations instead of calling find_definition \
first, and raise `limit` for symbols with many results instead of paging.";

/// A warm session's coverage with `indexed` and nothing absent or failed.
fn warm(indexed: Vec<PresentLanguage>) -> Coverage {
    Coverage { covered: Covered::Indexed(indexed), uncovered: Uncovered::Nothing }
}

/// A cold start's coverage with `installed` and no catalogue language missing.
fn cold(installed: Vec<PresentLanguage>) -> Coverage {
    Coverage { covered: Covered::Installed(installed), uncovered: Uncovered::Nothing }
}

fn ts_only() -> Vec<PresentLanguage> {
    vec![typescript_present()]
}

/// The bundled Go plugin's own `[plugin.capabilities]`, read off
/// `plugins/go/plugin.toml` rather than transcribed - so a later edit to
/// that manifest changes what these tests assert instead of quietly
/// disagreeing with it.
fn bundled_go_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/go");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Go plugin's manifest must be readable")
        .capabilities
}

fn go_present() -> PresentLanguage {
    PresentLanguage { language: "go".to_string(), capabilities: bundled_go_capabilities() }
}

/// Rust with the shipped capabilities, read off its manifest like its Go
/// counterpart: a test that models a manifest is a test that can disagree
/// with one.
fn rust_present() -> PresentLanguage {
    PresentLanguage { language: "rust".to_string(), capabilities: bundled_rust_capabilities() }
}

/// The bundled Rust plugin's own `[plugin.capabilities]`, read off
/// `plugins/rust/plugin.toml` rather than transcribed.
fn bundled_rust_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Rust plugin's manifest must be readable")
        .capabilities
}

/// The bundled Python plugin's own `[plugin.capabilities]`, read off
/// `plugins/python/plugin.toml` rather than transcribed - the same
/// `bundled_rust_capabilities`/`bundled_go_capabilities` pattern.
fn bundled_python_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/python");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Python plugin's manifest must be readable")
        .capabilities
}

fn typescript_present() -> PresentLanguage {
    PresentLanguage { language: "typescript".to_string(), capabilities: Capabilities::default() }
}

/// A future language whose semantic tier resolves receiver calls only
/// through an LSP bridge - the same shape as Rust (design doc's "Paper
/// stress test" table: Roslyn/clangd/pyright/jdtls/kotlin-lsp all listed
/// as "semantic" only, never resolved structurally): pass-dependent.
fn bridge_semantic(language: &str) -> PresentLanguage {
    PresentLanguage {
        language: language.to_string(),
        capabilities: Capabilities {
            semantic_pass: true,
            semantic_sweep: false,
            semantic_prepare: false,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
        },
    }
}

/// This module's permanent regression guard for the common rendering:
/// `ORIGINAL_INSTRUCTIONS` above is transcribed independently of the
/// paragraph constants, so this assertion fails the moment any of them
/// drifts.
#[test]
fn ts_only_is_byte_identical_to_the_original_string() {
    let rendered = build(&warm(ts_only()));
    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "a TypeScript-only project must read exactly the baseline");
    // The measured length of the current rendering, not a re-derivation.
    assert_eq!(rendered.len(), 1141, "this module's own current baseline, re-measured");
}

/// Nothing indexed: the coverage paragraph says so, and there is no
/// receiver paragraph to render (ADR 0022, section 3).
#[test]
fn nothing_indexed_says_so_and_leaves_out_the_receiver_paragraph() {
    let rendered = build(&warm(Vec::new()));
    assert!(rendered.contains(HEAD_NONE), "{rendered}");
    assert!(!rendered.contains("The one legitimate reason to grep afterward"), "{rendered}");
}

/// The four assertions every pass-dependent language makes, so that the
/// three languages that reach this rendering are checked against one
/// statement of it rather than three transcriptions.
///
/// `"may produce no edge"` is the never-resolving wording, so its absence
/// is what separates this rendering from TypeScript's.
fn assert_pass_dependent_receiver_clause(rendered: &str, language: &str) {
    assert!(
        rendered.contains("The one legitimate reason to grep afterward"),
        "{language}: the gap narrows, it never closes"
    );
    assert!(!rendered.contains("One real gap"), "{language}: the withdrawn claim must not return");
    assert!(
        rendered.contains("binds to the receiver's declared or inferred type"),
        "{language}: the rendering has to say what the resolution actually binds to:\n{rendered}"
    );
    assert!(
        rendered.contains("find_implementations is the way across"),
        "{language}: a pointer to the missing calls, never a count of them:\n{rendered}"
    );
    assert!(
        !rendered.contains("may produce no edge"),
        "{language}: that is the never-resolving wording:\n{rendered}"
    );
    assert!(rendered.contains(S_PASS), "{language}: the pass-dependent sentence:\n{rendered}");
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "{language}: {} bytes exceeds the {INSTRUCTIONS_BYTE_CEILING}-byte ceiling",
        rendered.len()
    );
    println!("{language}-only bytes: {}", rendered.len());
}

/// Rust resolves receiver calls only through its semantic pass, so it
/// renders the static form plus [`S_PASS`] whatever its pass state; that
/// state reaches the caller through `provenance` (ADR 0022, section 2).
///
/// Measured on this plugin's own fixture:
/// `find_callers("shapes::Shape::area")` is
/// `{shapes::total_dyn, gaps::measure}` - `&dyn Shape` and `<S: Shape>`
/// both land on the trait's declaration - while
/// `find_callers("shapes::<Circle as Shape>::area")` is **empty**,
/// though either of those two call sites reaches it at run time.
/// `conformance/expect.toml` asserts both as exact sets.
#[test]
fn rust_only_renders_the_static_form_with_the_pass_sentence() {
    let rendered = build(&warm(vec![rust_present()]));
    assert_pass_dependent_receiver_clause(&rendered, "rust");
}

/// Python, the same way: pyright resolves a receiver call against its
/// annotation, so `obj.describe()` for `obj: Base` is attributed to
/// `Base.describe` however the object was built. Measured:
/// `find_callers("Base.describe")` carries
/// `pkg/callers.py:through_a_base_annotation`, and
/// `find_callers("Deep.describe")` does not, though `Deep` overrides
/// `describe` and `find_implementations("Base")` names it.
#[test]
fn python_only_renders_the_static_form_with_the_pass_sentence() {
    let rendered = build(&warm(vec![PresentLanguage {
        language: "python".to_string(),
        capabilities: bundled_python_capabilities(),
    }]));
    assert_pass_dependent_receiver_clause(&rendered, "python");
}

/// Go, the same way. On `plugins/go/conformance/project`, with the pass
/// complete, `find_callers("Conn.Close")` answers `results: []` while
/// `server/conn.go:CloseAll` closes a `Conn` through a `Closer` value and
/// `find_implementations("Closer")` names `Conn`.
#[test]
fn go_only_renders_the_static_form_with_the_pass_sentence() {
    let rendered = build(&warm(vec![go_present()]));
    assert_pass_dependent_receiver_clause(&rendered, "go");
}

/// The silence half of the control: TypeScript declares
/// `receiver_calls = "unresolved"` in *both* tiers, so it is named as
/// never resolving, and the sentence about binding to a declared type
/// must never appear for it. Measured on a probe fixture carrying three
/// real receiver calls (`g.greet()` on a parameter typed by the
/// interface, by the base class, and on a local of a subclass): every one
/// of the three `greet` declarations answers `find_callers` with an empty
/// set, because this plugin emits no receiver-call edge.
#[test]
fn typescript_never_reaches_the_narrowed_rendering() {
    let rendered = build(&warm(ts_only()));

    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "typescript's gap never narrows, because it never resolves");
    assert!(rendered.contains("in typescript, a method call"), "the never-resolving wording names it");
    assert!(
        !rendered.contains("binds to the receiver's declared"),
        "the narrowed clause must not fire for a language with no receiver-call edges:\n{rendered}"
    );
    assert!(!rendered.contains(S_PASS), "typescript has no semantic pass to wait for:\n{rendered}");
}

/// A mixed project names its never-resolving languages and appends
/// [`S_PASS`] for its pass-dependent ones; it does not carry the static
/// form's override sentence, which would be false for the named ones.
#[test]
fn a_mixed_project_keeps_the_named_open_gap_and_does_not_carry_the_narrowing() {
    let rendered = build(&warm(vec![typescript_present(), go_present()]));

    assert!(rendered.contains("in typescript, a method call"), "{rendered}");
    assert!(!rendered.contains("binds to the receiver's declared"), "{rendered}");
    assert!(rendered.contains(S_PASS), "{rendered}");
    assert!(
        rendered.contains("a method's caller/reference list there can under-report"),
        "the warning this rendering keeps:\n{rendered}"
    );
}

/// A pass-dependent language is never named in the gap list, so TypeScript
/// plus Rust names only TypeScript.
#[test]
fn ts_plus_rust_names_only_typescript() {
    let rendered = build(&warm(vec![typescript_present(), rust_present()]));
    assert!(rendered.contains("in typescript, a method call"), "{rendered}");
    assert!(rendered.contains("Indexed here: rust and typescript."), "{rendered}");
    assert!(rendered.contains(S_PASS), "{rendered}");
    println!("ts+rust bytes: {}", rendered.len());
}

/// The worst-case fixture: typescript and go plus rust and the five
/// languages the architecture doc's "Paper stress test" section names
/// (C#, C++, Python, Java, Kotlin), all present at once.
fn worst_case_present() -> Vec<PresentLanguage> {
    vec![
        typescript_present(),
        bridge_semantic("go"),
        bridge_semantic("rust"),
        bridge_semantic("csharp"),
        bridge_semantic("cpp"),
        bridge_semantic("python"),
        bridge_semantic("java"),
        bridge_semantic("kotlin"),
    ]
}

/// This is the case the byte ceiling is actually checked against, not the
/// common one or two-language case.
#[test]
fn worst_case_every_bundled_and_planned_language_gapped_at_once() {
    let rendered = build(&warm(worst_case_present()));
    println!("worst-case bytes: {}", rendered.len());
    println!("worst-case text: {rendered}");
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "worst case must stay under the ceiling (or the ladder must have engaged): {} bytes",
        rendered.len()
    );
}

/// [`format_language_list`] on its own, independent of [`build`]'s byte
/// arithmetic - the three arities the lists can actually need.
#[test]
fn format_language_list_covers_one_two_and_several() {
    assert_eq!(format_language_list(&["go".to_string()]), "go");
    assert_eq!(format_language_list(&["go".to_string(), "rust".to_string()]), "go and rust");
    assert_eq!(
        format_language_list(&["go".to_string(), "rust".to_string(), "typescript".to_string()]),
        "go, rust and typescript"
    );
}

/// The ladder's step 3 wording must still fit under the ceiling on its own,
/// and makes no claim that a page discloses the gap: TypeScript reports no
/// per-page field for it (ADR 0022, section 1, row 12).
#[test]
fn fallback_wording_fits_under_the_ceiling() {
    let rendered = render(&warm(ts_only()), 3);
    println!("fallback bytes: {}", rendered.len());
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    assert!(rendered.contains(P4_PERM_FALLBACK));
    assert!(!rendered.contains("untypedReceiverCalls"), "{rendered}");
}

/// [`build`]'s ladder, proven rather than merely present: a list of
/// never-resolving languages long enough that naming them all overflows
/// the ceiling renders the generic step-3 paragraph, not a truncated name
/// list. Sixty synthetic languages stand in for "more than the design doc
/// plans for" rather than a real count.
#[test]
fn a_present_list_too_long_to_name_falls_back_instead_of_exceeding_the_ceiling() {
    let present: Vec<PresentLanguage> = (0..60)
        .map(|i| PresentLanguage { language: format!("lang{i:02}"), capabilities: Capabilities::default() })
        .collect();
    let coverage = warm(present);

    // Sanity check on the test fixture itself: naming them all really
    // would overflow the ceiling, or this test would exercise step 1.
    let would_be_named = render(&coverage, 2);
    assert!(
        would_be_named.len() > INSTRUCTIONS_BYTE_CEILING,
        "test fixture must actually overflow the ceiling to exercise the fallback branch: {} bytes",
        would_be_named.len()
    );

    let rendered = build(&coverage);
    assert_eq!(rendered, render(&coverage, 3), "must render the fallback, not a truncated name list");
    assert!(rendered.contains(P4_PERM_FALLBACK));
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
}

/// A synthetic absolute root of exactly `len` bytes when [`Path::display`]
/// renders it - ASCII only, so `String::len` and the displayed byte count
/// agree exactly, which is what lets [`cold_start_unindexed_at_the_worst_case_with_a_103_byte_root_fits_the_ceiling`]
/// and its siblings target the specific budget D12 asks for without
/// depending on this machine's own checkout path.
fn root_of_byte_len(len: usize) -> std::path::PathBuf {
    let mut root = String::from("/");
    while root.len() < len {
        root.push('r');
    }
    assert_eq!(root.len(), len, "test fixture construction must produce exactly the requested length");
    std::path::PathBuf::from(root)
}

/// GM-395 slice 2b, test 5 (D12 in `docs/architecture/lazy-indexing.md`):
/// the `Unindexed` rendering at the eight-language worst case
/// ([`worst_case_present`]) with a 103-byte root must still fit under
/// [`INSTRUCTIONS_BYTE_CEILING`] - the exact case the design doc's own
/// ceiling test targets.
#[test]
fn cold_start_unindexed_at_the_worst_case_with_a_103_byte_root_fits_the_ceiling() {
    let root = root_of_byte_len(103);
    let rendered = cold_start(&root, false, &cold(worst_case_present()));
    println!("cold-start (unindexed, 103-byte root) bytes: {}", rendered.len());
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "must fit under the ceiling, falling back to the no-path line if it would not otherwise: \
         {} bytes",
        rendered.len()
    );
}

/// The `Walking`-phase sibling of the test above, and GM-395's own
/// "existing indexing variant" this task asked to check: before D12's
/// root-aware line, this phase's rendering was the fixed `INDEXING_NOTE`
/// constant plus the same worst-case body, which the phase-1 design
/// measured at about 1,867 bytes with nothing pinning it to the ceiling.
/// This is that missing test, now against the line D12 replaced
/// `INDEXING_NOTE` with.
#[test]
fn cold_start_walking_at_the_worst_case_with_a_103_byte_root_fits_the_ceiling() {
    let root = root_of_byte_len(103);
    let rendered = cold_start(&root, true, &cold(worst_case_present()));
    println!("cold-start (walking, 103-byte root) bytes: {}", rendered.len());
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "must fit under the ceiling, falling back to the no-path line if it would not otherwise: \
         {} bytes",
        rendered.len()
    );
}

/// D12's own fallback rule: a root long enough that including it would
/// push the rendering past the ceiling must drop the path entirely
/// rather than let the whole session lose its instructions. Checked by
/// actually confirming the path is gone from the output, not just that
/// the byte count happens to fit - a silently truncated root would also
/// measure short.
#[test]
fn cold_start_falls_back_to_the_no_path_line_once_the_root_is_too_long() {
    let root = root_of_byte_len(600);
    let rendered = cold_start(&root, false, &cold(worst_case_present()));
    println!("cold-start (unindexed, 600-byte root) bytes: {}", rendered.len());
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "the no-path fallback itself must fit under the ceiling: {} bytes",
        rendered.len()
    );
    assert!(!rendered.contains("Index root:"), "a root this long must trigger the no-path fallback");
    assert!(
        rendered.starts_with("Not indexed yet"),
        "the fallback line replaces the whole prefix, not just the path: {rendered}"
    );
}

fn front_detection(names: &[String], truncated: bool) -> Detection {
    use crate::daemon::candidates::{Candidate, Mode};
    Detection {
        mode: Mode::Multi,
        candidates: names
            .iter()
            .map(|name| Candidate {
                rel_path: name.clone(),
                abs_path: std::path::PathBuf::from("/x").join(name),
                markers: vec![".git"],
                is_worktree: false,
            })
            .collect(),
        entries_read: names.len(),
        elapsed: std::time::Duration::ZERO,
        truncated,
        walked: true,
    }
}

/// GM-399 slice 4 test 3 (D12): the worst case the design names - 64
/// candidates with 60-byte names under a 103-byte root - fits under the
/// ceiling, still lists at least one name, and points at the rest.
#[test]
fn build_front_with_64_long_names_under_a_103_byte_root_fits_the_ceiling() {
    let root = root_of_byte_len(103);
    let names: Vec<String> = (0..64).map(|n| format!("{n:02}{}", "n".repeat(58))).collect();
    assert!(names.iter().all(|name| name.len() == 60));
    let rendered = build_front(&root, &front_detection(&names, false), &HashSet::new());
    println!("front (64 x 60-byte names, 103-byte root) bytes: {}", rendered.len());
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{} bytes", rendered.len());
    assert!(rendered.contains(&names[0]), "at least one name must be listed: {rendered}");
    assert!(rendered.contains(" (+"), "the names that did not fit must be pointed at: {rendered}");
    assert!(rendered.contains(" more - call select_project with no argument"));
    assert!(rendered.contains(&root.display().to_string()), "a 103-byte root still fits: {rendered}");
    assert!(rendered.contains("is a folder of 64 projects"));
}

/// GM-399 follow-up: with no candidate indexed, the front keeps D12's
/// original wording and lists the names unmarked, in walk order.
///
/// Control: make `front_sentence` always take the "some indexed" branch
/// (`if false`): "has indexed none of them" disappears.
#[test]
fn build_front_with_nothing_indexed_says_none() {
    let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let rendered = build_front(Path::new("/f"), &front_detection(&names, false), &HashSet::new());
    assert!(
        rendered.contains(
            "/f is a folder of 3 projects; g-mesh serves one at a time and has indexed none of them. Before"
        ),
        "{rendered}"
    );
    assert!(!rendered.contains("(indexed)"), "{rendered}");
    assert!(rendered.ends_with("Projects: a, b, c."), "{rendered}");
}

/// GM-399 follow-up (defect 1 of `docs/results/gm399-multi-project-measurements.md`):
/// with some candidates already indexed, the sentence no longer claims
/// "none", counts them, and lists them first with a mark.
///
/// Control: pass `0` instead of `done.len()` to `front_sentence` in
/// `build_front`: the text says "has indexed none of them" again.
#[test]
fn build_front_with_some_indexed_names_them() {
    let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let indexed: HashSet<&str> = ["c", "b"].into_iter().collect();
    let rendered = build_front(Path::new("/f"), &front_detection(&names, false), &indexed);
    assert!(!rendered.contains("indexed none"), "{rendered}");
    assert!(
        rendered.contains(
            "/f is a folder of 3 projects; g-mesh serves one at a time and has already indexed 2 of them, \
             listed first and marked (indexed). Before"
        ),
        "{rendered}"
    );
    assert!(rendered.ends_with("Projects: b (indexed), c (indexed), a."), "{rendered}");
}

/// Indexed candidates come first, so the ceiling cuts unindexed names
/// before an indexed one: `63` is the last name in walk order and still
/// makes the list under the worst-case root.
///
/// Control: drop the `partition` in `build_front` (list names in walk
/// order, marking in place): `63 (indexed)` falls past the ceiling.
#[test]
fn build_front_keeps_indexed_names_within_the_ceiling() {
    let root = root_of_byte_len(103);
    let names: Vec<String> = (0..64).map(|n| format!("{n:02}{}", "n".repeat(58))).collect();
    let indexed: HashSet<&str> = [names[63].as_str()].into_iter().collect();
    let rendered = build_front(&root, &front_detection(&names, false), &indexed);
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{} bytes", rendered.len());
    assert!(rendered.contains(&format!("Projects: {} (indexed), ", names[63])), "{rendered}");
    assert!(rendered.contains("has already indexed 1 of them"), "{rendered}");
    assert!(rendered.contains(&root.display().to_string()), "{rendered}");
}

/// D12's no-path fallback: a root too long for the ceiling is dropped
/// from the text rather than truncated or allowed to overflow.
#[test]
fn build_front_drops_a_root_too_long_for_the_ceiling() {
    // 1,400 bytes rather than `cold_start`'s 600: the front's text has
    // no language paragraphs, so a 600-byte root still fits beside it.
    let root = root_of_byte_len(1400);
    let names = vec!["a".to_string(), "b".to_string()];
    let rendered = build_front(&root, &front_detection(&names, true), &HashSet::new());
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{} bytes", rendered.len());
    assert!(!rendered.contains("/rrrr"), "the root must not appear: {rendered}");
    assert!(rendered.contains("This folder holds 2+ projects"), "{rendered}");
    assert!(rendered.ends_with("Projects: a, b."), "{rendered}");
}

/// Under a cold-start line the body is built against the ceiling less the
/// no-path line, whatever the root's length: a coverage whose full text
/// fits [`build`]'s ceiling but not that smaller budget renders ladder
/// step 3 under every root. Control: build `cold_start`'s body with
/// `build(coverage)` again (a 103-byte root renders the step-1 body under
/// the no-path line, over the ceiling).
#[test]
fn the_worst_case_cold_start_falls_back_to_the_generic_receiver_paragraph() {
    let line = cold_start_line_fallback(false).len().min(cold_start_line_fallback(true).len()) + 2;
    let coverage = (1..200)
        .map(|n| {
            cold(
                (0..n)
                    .map(|i| PresentLanguage {
                        language: format!("lang{i:03}"),
                        capabilities: Capabilities::default(),
                    })
                    .collect(),
            )
        })
        .find(|coverage| {
            let full = render(coverage, 1).len();
            full <= INSTRUCTIONS_BYTE_CEILING && full > INSTRUCTIONS_BYTE_CEILING - line
        })
        .expect("some list length lands between the two budgets");
    let named = build(&coverage);
    assert_eq!(named, render(&coverage, 1), "the full text fits the plain ceiling");
    let fallback = render(&coverage, 3);
    println!("named {} bytes, fallback {} bytes", named.len(), fallback.len());

    for walking in [false, true] {
        for root_len in [10, 103, 600] {
            let rendered = cold_start(&root_of_byte_len(root_len), walking, &coverage);
            println!("walking={walking} root={root_len}: {} bytes", rendered.len());
            assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{} bytes", rendered.len());
            assert!(rendered.ends_with(&fallback), "walking={walking} root={root_len}: {rendered}");
        }
    }
}

// ---------------------------------------------------------------------------
// GM-330/S3: every coverage state of ADR 0022, against the real bundled
// manifests (`plugins/*/plugin.toml` through `discover`) and the real
// catalogue (`languages::CATALOGUE`), not hand-built literals.
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::Connection;

use crate::daemon::indexing_status::{IndexingStatus, Phase};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::{discover, DiscoveredPlugins};
use crate::daemon::registry::PluginRegistry;
use crate::embedding::EmbeddingPipeline;
use crate::graph::queries::upsert_node;
use crate::languages::{self, CATALOGUE};
use crate::mcp::GMeshMcpServer;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::NodeRecord;

/// The generic "unsupported" sentence (ADR 0022, section 3): said once,
/// naming no catalogue.
const UNSUPPORTED: &str = "g-mesh has no answers about files in any other language.";

/// The bundled plugins as `discover` finds them under `plugins/`, kept to
/// `keep`: the other bundled languages are then exactly as missing as on a
/// machine without their plugin.
fn real_plugins(keep: &[&str]) -> DiscoveredPlugins {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let mut found = discover(&[root]).expect("the bundled plugins must be discoverable");
    for language in keep {
        assert!(found.manifests.contains_key(*language), "no bundled manifest for {language}");
    }
    found.manifests.retain(|language, _| keep.contains(&language.as_str()));
    found.routing.retain(|_, language| keep.contains(&language.as_str()));
    found
}

/// Each discovered language's manifest capabilities, as
/// `PluginRegistry::receiver_call_capabilities` hands them to the server.
fn real_capabilities(found: &DiscoveredPlugins) -> HashMap<String, Capabilities> {
    found.manifests.iter().map(|(language, manifest)| (language.clone(), manifest.capabilities)).collect()
}

fn real_present(found: &DiscoveredPlugins, languages: &[&str]) -> Vec<PresentLanguage> {
    present_languages(languages.iter().map(|language| language.to_string()), &real_capabilities(found))
}

/// `languages::missing` over `found`, as `PluginRegistry::missing_languages`
/// returns it.
fn real_missing(found: &DiscoveredPlugins) -> Vec<String> {
    languages::missing(found).iter().map(|entry| entry.language.to_string()).collect()
}

/// The catalogue's own install command for `language`, backticked as the
/// text renders it.
fn command(language: &str) -> String {
    let entry = languages::entry(language).unwrap_or_else(|| panic!("{language} is not catalogued"));
    format!("`{}`", entry.install_command())
}

/// `outcomes` sorted by language, as `schema::language_outcomes` returns them.
fn sorted(outcomes: Vec<(&str, LanguageOutcome)>) -> Vec<(String, LanguageOutcome)> {
    let mut outcomes: Vec<(String, LanguageOutcome)> =
        outcomes.into_iter().map(|(language, outcome)| (language.to_string(), outcome)).collect();
    outcomes.sort_by(|a, b| a.0.cmp(&b.0));
    outcomes
}

fn indexed() -> LanguageOutcome {
    LanguageOutcome::Indexed { files: 10 }
}

fn absent(files: Option<usize>) -> LanguageOutcome {
    LanguageOutcome::PluginAbsent { files }
}

fn failed(error: &str) -> LanguageOutcome {
    LanguageOutcome::Failed { error: error.to_string() }
}

/// A warm coverage exactly as `GMeshMcpServer::instructions` assembles it:
/// `indexed` languages paired with `found`'s manifests, the recorded
/// `outcomes`, and `found`'s missing catalogue languages.
fn warm_real(
    found: &DiscoveredPlugins,
    indexed: &[&str],
    outcomes: Vec<(&str, LanguageOutcome)>,
) -> Coverage {
    Coverage::from_outcomes(real_present(found, indexed), sorted(outcomes), real_missing(found))
}

/// The receiver paragraph's opening, said by every one of its forms.
const RECEIVER_OPENING: &str = "The one legitimate reason to grep afterward";

/// Working: the four bundled languages indexed, nothing absent or failed.
/// The covered list names them, the unsupported sentence follows, and
/// nothing says "not indexed". Controls: emit `Uncovered::Recorded` with
/// empty lists in `from_outcomes` (the trailer appears); drop the list from
/// `head_indexed`.
#[test]
fn working_names_every_indexed_language_from_the_real_manifests() {
    let all = ["go", "python", "rust", "typescript"];
    let found = real_plugins(&all);
    assert!(real_missing(&found).is_empty(), "every catalogue language has a bundled plugin");
    let coverage = warm_real(&found, &all, all.iter().map(|language| (*language, indexed())).collect());

    let rendered = build(&coverage);
    println!("working, four languages: {} bytes", rendered.len());

    assert!(
        rendered.contains(&format!("Indexed here: go, python, rust and typescript. {UNSUPPORTED}")),
        "{rendered}"
    );
    assert!(!rendered.contains("Not indexed"), "{rendered}");
    assert!(!rendered.contains(TRAILER), "{rendered}");
    assert!(!rendered.contains("If this project has"), "{rendered}");
    // TypeScript's real manifest resolves receiver calls in no tier; the
    // other three resolve them through their semantic pass.
    assert!(rendered.contains(&p4_perm("typescript")), "{rendered}");
    assert!(rendered.contains(S_PASS), "{rendered}");
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
}

/// Plugin absent: each absent catalogue language is named with its file
/// count and the exact `g-mesh plugins install <language>` command, and the
/// trailer follows. Controls: drop the `Some(n)` count from the item; derive
/// the command from anything but the language id (e.g. `g-mesh install`).
#[test]
fn plugin_absent_names_the_file_count_and_the_exact_install_command() {
    let found = real_plugins(&["typescript"]);
    let missing = real_missing(&found);
    assert_eq!(missing, ["python", "rust", "go"], "the catalogue order, less the one installed");
    let coverage = warm_real(
        &found,
        &["typescript"],
        vec![
            ("typescript", indexed()),
            ("python", absent(Some(214))),
            ("rust", absent(Some(3))),
            ("go", absent(Some(12345))),
        ],
    );

    let rendered = build(&coverage);
    println!("typescript + three absent: {} bytes", rendered.len());

    let expected = format!(
        "Not indexed, no plugin installed: go (12345 files; {}), python (214 files; {}), rust (3 files; {}).",
        command("go"),
        command("python"),
        command("rust")
    );
    assert!(rendered.contains(&expected), "{rendered}");
    assert!(rendered.contains("python (214 files; `g-mesh plugins install python`)"), "{rendered}");
    assert!(rendered.contains(&format!("Indexed here: typescript. {UNSUPPORTED}")), "{rendered}");
    assert!(rendered.contains(TRAILER), "{rendered}");
    assert_eq!(rendered, render(&coverage, 1), "a realistic case fits at ladder step 1");
}

/// An absent language whose files were not counted says so instead of a
/// count, with the same command. Control: render `None` as `0 files`.
#[test]
fn an_absent_language_with_no_count_says_files_not_counted() {
    let found = real_plugins(&["typescript"]);
    let coverage =
        warm_real(&found, &["typescript"], vec![("typescript", indexed()), ("python", absent(None))]);

    let rendered = build(&coverage);

    assert!(
        rendered.contains(&format!(
            "Not indexed, no plugin installed: python (files not counted; {}).",
            command("python")
        )),
        "{rendered}"
    );
    assert!(!rendered.contains("0 files"), "{rendered}");
    assert!(rendered.contains(TRAILER), "{rendered}");
}

/// The acceptance criterion's distinction: a project where Python has no
/// plugin, and one where Python is indexed (so an empty answer about a `.py`
/// file really means nothing was found). Both renderings name `python`, so
/// the language name tells the agent nothing; what tells them apart is the
/// trailer, "no plugin installed" and the install command, all present in
/// the first and absent from the second. Controls: drop the trailer push;
/// reword the absent sentence without "no plugin installed"; leave the
/// command out of the item.
#[test]
fn plugin_absent_cannot_be_mistaken_for_nothing_found() {
    let without = real_plugins(&["typescript"]);
    let plugin_absent = build(&warm_real(
        &without,
        &["typescript"],
        vec![("typescript", indexed()), ("python", absent(Some(214)))],
    ));
    let with = real_plugins(&["python", "typescript"]);
    let nothing_found = build(&warm_real(
        &with,
        &["python", "typescript"],
        vec![("python", indexed()), ("typescript", indexed())],
    ));

    assert!(plugin_absent.contains("python") && nothing_found.contains("python"), "both name python");
    for distinguishing in
        ["not evidence of absence", "no plugin installed", "`g-mesh plugins install python`"]
    {
        assert!(
            plugin_absent.contains(distinguishing),
            "plugin absent must say {distinguishing:?}:\n{plugin_absent}"
        );
        assert!(
            !nothing_found.contains(distinguishing),
            "an indexed python must not say {distinguishing:?}:\n{nothing_found}"
        );
    }
    assert!(plugin_absent.contains("Indexed here: typescript."), "{plugin_absent}");
    assert!(nothing_found.contains("Indexed here: python and typescript."), "{nothing_found}");
}

/// Failed: the language is named with the first line of its error and the
/// `g-mesh reindex` retry, never as indexed, and the trailer follows.
/// Controls: drop the failed arm; render the whole chain instead of
/// `error_first_line`.
#[test]
fn a_failed_language_names_its_first_error_line_and_the_reindex_command() {
    let all = ["go", "python", "rust", "typescript"];
    let found = real_plugins(&all);
    let coverage = warm_real(
        &found,
        &["go", "python", "typescript"],
        vec![
            ("go", indexed()),
            ("python", indexed()),
            ("rust", failed("rust plugin exited during the handshake\ncaused by: No such file or directory")),
            ("typescript", indexed()),
        ],
    );

    let rendered = build(&coverage);
    println!("one failed: {} bytes", rendered.len());

    assert!(
        rendered.contains(
            "Not indexed, plugin failed: rust (rust plugin exited during the handshake) - fix the plugin, \
             then run `g-mesh reindex`."
        ),
        "{rendered}"
    );
    assert!(!rendered.contains("caused by"), "only the first line: {rendered}");
    assert!(rendered.contains("Indexed here: go, python and typescript."), "{rendered}");
    assert!(rendered.contains(TRAILER), "{rendered}");
}

/// [`error_first_line`]: the first line only, at most [`ERROR_BYTES`]
/// bytes, cut on a char boundary when byte 97 falls inside a multi-byte
/// char. Controls: cut at `&line[..97]` without the boundary walk (panics on
/// the multi-byte inputs); drop the length check (the long line comes back
/// whole).
#[test]
fn error_first_line_keeps_the_first_line_within_100_bytes_on_a_char_boundary() {
    assert_eq!(error_first_line("first\nsecond"), "first");
    assert_eq!(error_first_line(""), "");

    let exactly = "e".repeat(ERROR_BYTES);
    assert_eq!(error_first_line(&exactly), exactly, "100 bytes is kept whole");

    let over = "e".repeat(ERROR_BYTES + 1);
    let cut = error_first_line(&over);
    assert_eq!(cut, format!("{}...", "e".repeat(ERROR_BYTES - 3)));
    assert_eq!(cut.len(), ERROR_BYTES);

    // A two-byte char spanning bytes 96-97: byte 97 is inside it.
    let two_byte = format!("{}é{}", "a".repeat(96), "z".repeat(20));
    let cut = error_first_line(&two_byte);
    assert_eq!(cut, format!("{}...", "a".repeat(96)));
    assert!(cut.len() <= ERROR_BYTES);

    // Three-byte chars only: 97 is not a multiple of 3.
    let three_byte = "日".repeat(40);
    let cut = error_first_line(&three_byte);
    assert_eq!(cut, format!("{}...", "日".repeat(32)));
    assert!(cut.len() <= ERROR_BYTES);

    // The cut applies to the first line, not to the whole chain.
    let chain = format!("{}\nshort", "日".repeat(40));
    assert_eq!(error_first_line(&chain), format!("{}...", "日".repeat(32)));
}

/// The trailer is said exactly when something is absent or failed.
/// Controls: push the trailer for `Uncovered::Nothing` too; push it only
/// when `absent` is non-empty.
#[test]
fn the_trailer_is_said_iff_a_language_is_absent_or_failed() {
    let found = real_plugins(&["typescript"]);
    let only_absent = build(&warm_real(
        &found,
        &["typescript"],
        vec![("typescript", indexed()), ("python", absent(Some(1)))],
    ));
    let all = real_plugins(&["go", "python", "rust", "typescript"]);
    let only_failed = build(&warm_real(
        &all,
        &["go", "python", "typescript"],
        vec![("go", indexed()), ("python", indexed()), ("rust", failed("boom")), ("typescript", indexed())],
    ));
    let neither = build(&warm_real(
        &all,
        &["go", "python", "rust", "typescript"],
        ["go", "python", "rust", "typescript"].iter().map(|language| (*language, indexed())).collect(),
    ));

    assert_eq!(only_absent.matches(TRAILER).count(), 1, "{only_absent}");
    assert_eq!(only_failed.matches(TRAILER).count(), 1, "{only_failed}");
    assert_eq!(neither.matches(TRAILER).count(), 0, "{neither}");
}

/// The unsupported state is one generic sentence, present exactly once in
/// every rendering that lists covered languages, warm or cold, with or
/// without absent and failed languages - and it names no catalogue language
/// beyond those listed. Control: drop the sentence from `head_indexed` (or
/// `head_installed`).
#[test]
fn the_unsupported_sentence_is_said_once_in_every_rendering_with_a_list() {
    let ts = real_plugins(&["typescript"]);
    let all = real_plugins(&["go", "python", "rust", "typescript"]);
    let cold_ts = Coverage {
        covered: Covered::Installed(real_present(&ts, &["typescript"])),
        uncovered: Uncovered::missing(real_missing(&ts)),
    };
    let cold_all = Coverage {
        covered: Covered::Installed(real_present(&all, &["go", "python", "rust", "typescript"])),
        uncovered: Uncovered::missing(real_missing(&all)),
    };
    let renderings = [
        ("warm, working", build(&warm_real(&ts, &["typescript"], vec![("typescript", indexed())]))),
        (
            "warm, absent and failed",
            build(&warm_real(
                &all,
                &["go", "typescript"],
                vec![
                    ("go", indexed()),
                    ("python", absent(Some(9))),
                    ("rust", failed("boom")),
                    ("typescript", indexed()),
                ],
            )),
        ),
        ("cold, three missing", cold_start(&root_of_byte_len(40), false, &cold_ts)),
        ("cold walking, three missing", cold_start(&root_of_byte_len(40), true, &cold_ts)),
        ("cold, none missing", cold_start(&root_of_byte_len(40), false, &cold_all)),
    ];
    for (name, rendered) in renderings {
        assert_eq!(rendered.matches(UNSUPPORTED).count(), 1, "{name}: {rendered}");
    }
}

/// Nothing indexed because no plugin is installed: the nothing-indexed head,
/// every catalogue language named with its command, and no receiver
/// paragraph (it would describe languages that have no answers). Controls:
/// let `receiver_paragraph` render `P4_STATIC` for an empty list; render the
/// `Indexed here` head for an empty list.
#[test]
fn nothing_indexed_with_no_plugin_installed_names_every_catalogue_language() {
    let found = real_plugins(&[]);
    let missing = real_missing(&found);
    let catalogue: Vec<&str> = CATALOGUE.iter().map(|entry| entry.language).collect();
    assert_eq!(missing, catalogue, "with no plugin, the whole catalogue is missing");
    let outcomes = missing.iter().map(|language| (language.as_str(), absent(Some(10_000)))).collect();
    let coverage = warm_real(&found, &[], outcomes);

    let rendered = build(&coverage);
    println!("zero plugins, all four absent: {} bytes", rendered.len());

    assert!(rendered.contains(HEAD_NONE), "{rendered}");
    assert!(!rendered.contains("Indexed here"), "{rendered}");
    assert!(!rendered.contains(RECEIVER_OPENING), "{rendered}");
    assert!(!rendered.contains(S_PASS), "{rendered}");
    for language in &catalogue {
        assert!(rendered.contains(&format!("{language} (10000 files; {})", command(language))), "{rendered}");
    }
    assert!(rendered.contains(TRAILER), "{rendered}");
    assert_eq!(rendered, render(&coverage, 1));
}

/// Nothing indexed because every discovered plugin failed (the warm text of
/// `Phase::Failed`): the same nothing-indexed head and no receiver
/// paragraph, every language named as failed. Control: as above.
#[test]
fn nothing_indexed_with_every_plugin_failed_names_each_as_failed() {
    let all = ["go", "python", "rust", "typescript"];
    let found = real_plugins(&all);
    let coverage = warm_real(&found, &[], all.iter().map(|language| (*language, failed("exited"))).collect());

    let rendered = build(&coverage);
    println!("all four failed: {} bytes", rendered.len());

    assert!(rendered.contains(HEAD_NONE), "{rendered}");
    assert!(!rendered.contains(RECEIVER_OPENING), "{rendered}");
    assert!(
        rendered.contains(
            "Not indexed, plugin failed: go (exited), python (exited), rust (exited), typescript (exited) - \
             fix the plugin, then run `g-mesh reindex`."
        ),
        "{rendered}"
    );
    assert!(rendered.contains(TRAILER), "{rendered}");
}

/// Cold start: the installed plugins from the real manifests, and the
/// catalogue languages with none, said conditionally (no count, no I/O)
/// with each command, joined with "or"; then the "slow, not wrong" wait in
/// both phases. With every plugin installed the conditional sentence is
/// gone. Controls: join the missing list with "and"; drop the `Missing` arm;
/// drop `WAIT_IS_NOT_WRONG` from the cold-start line.
#[test]
fn cold_start_names_the_installed_plugins_and_the_missing_ones_conditionally() {
    let ts = real_plugins(&["typescript"]);
    let coverage = Coverage {
        covered: Covered::Installed(real_present(&ts, &["typescript"])),
        uncovered: Uncovered::missing(real_missing(&ts)),
    };
    for walking in [false, true] {
        let rendered = cold_start(&root_of_byte_len(40), walking, &coverage);
        assert!(rendered.contains(&format!("Plugins installed: typescript. {UNSUPPORTED}")), "{rendered}");
        assert!(
            rendered.contains(&format!(
                "If this project has python, rust or go files, they are not indexed: no plugin installed \
                 ({}, {}, {}).",
                command("python"),
                command("rust"),
                command("go")
            )),
            "{rendered}"
        );
        assert!(rendered.contains("slow, not wrong; do not abandon it for grep."), "{rendered}");
        assert!(!rendered.contains("Indexed here"), "a cold start has read no index: {rendered}");
    }

    let all = real_plugins(&["go", "python", "rust", "typescript"]);
    let everything = Coverage {
        covered: Covered::Installed(real_present(&all, &["go", "python", "rust", "typescript"])),
        uncovered: Uncovered::missing(real_missing(&all)),
    };
    let rendered = cold_start(&root_of_byte_len(40), false, &everything);
    assert!(rendered.contains("Plugins installed: go, python, rust and typescript."), "{rendered}");
    assert!(!rendered.contains("If this project has"), "{rendered}");

    let none = real_plugins(&[]);
    let nothing = Coverage {
        covered: Covered::Installed(Vec::new()),
        uncovered: Uncovered::missing(real_missing(&none)),
    };
    let rendered = cold_start(&root_of_byte_len(40), false, &nothing);
    assert!(rendered.contains(HEAD_INSTALLED_NONE), "{rendered}");
    assert!(rendered.contains("If this project has typescript, python, rust or go files"), "{rendered}");
    assert!(!rendered.contains(RECEIVER_OPENING), "{rendered}");
}

/// A warm rendering never says the walk's wait: an index that is read is
/// already walked (ADR 0022, section 1, row 10). Control: append
/// `WAIT_IS_NOT_WRONG` to the warm text.
#[test]
fn a_warm_rendering_never_says_the_cold_start_wait() {
    let found = real_plugins(&["rust", "typescript"]);
    let rendered = build(&warm_real(
        &found,
        &["rust", "typescript"],
        vec![("rust", indexed()), ("typescript", indexed()), ("python", absent(Some(2)))],
    ));
    assert!(!rendered.contains("slow, not wrong"), "{rendered}");
}

/// TypeScript's real manifest: no tier resolves receiver calls, so it is
/// named (never), and nothing waits on its pass. Control: classify by
/// `semantic_pass` alone (TypeScript declares one).
#[test]
fn the_real_typescript_manifest_is_never_resolving() {
    let found = real_plugins(&["typescript"]);
    let present = real_present(&found, &["typescript"]);
    assert_eq!(receiver_class(&present[0].capabilities), ReceiverClass::Never);
    let rendered = build(&warm(present));
    assert!(rendered.contains(&p4_perm("typescript")), "{rendered}");
    assert!(!rendered.contains(S_PASS), "{rendered}");
    assert!(!rendered.contains(P4_STATIC), "{rendered}");
}

/// Go, Python and Rust from their real manifests are pass-dependent: never
/// named in the gap list; `P4_STATIC` (no never-language) plus `S_PASS`.
/// Control: drop the `PassDependent` arm (they become never and are named).
#[test]
fn the_real_go_python_and_rust_manifests_are_pass_dependent() {
    let three = ["go", "python", "rust"];
    let found = real_plugins(&three);
    for language in real_present(&found, &three) {
        assert_eq!(
            receiver_class(&language.capabilities),
            ReceiverClass::PassDependent,
            "{}",
            language.language
        );
    }
    let rendered = build(&warm(real_present(&found, &three)));
    assert!(rendered.contains(&format!("{P4_STATIC} {S_PASS}")), "{rendered}");
    assert!(!rendered.contains("may produce no edge"), "{rendered}");
}

/// A plugin that claims `receiver_calls = resolved` with no semantic tier
/// cannot deliver it, so it is never-resolving and named, and `S_PASS` is
/// not appended for it. Control: classify by `receiver_calls` alone.
#[test]
fn resolved_receiver_calls_without_a_semantic_pass_are_never_resolving() {
    let capabilities = Capabilities {
        semantic_pass: false,
        semantic_sweep: false,
        semantic_prepare: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Unresolved,
    };
    assert_eq!(receiver_class(&capabilities), ReceiverClass::Never);
    let rendered = build(&warm(vec![PresentLanguage { language: "zig".to_string(), capabilities }]));
    assert!(rendered.contains(&p4_perm("zig")), "{rendered}");
    assert!(!rendered.contains(S_PASS), "{rendered}");
}

/// A structurally resolving language is static: `P4_STATIC` and no
/// `S_PASS`, even with a semantic pass declared. Control: test
/// `PassDependent` before `Static` in `receiver_class`.
#[test]
fn a_structurally_resolving_language_is_static_without_the_pass_sentence() {
    let capabilities = Capabilities {
        semantic_pass: true,
        semantic_sweep: false,
        semantic_prepare: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Resolved,
    };
    assert_eq!(receiver_class(&capabilities), ReceiverClass::Static);
    let rendered = build(&warm(vec![PresentLanguage { language: "zig".to_string(), capabilities }]));
    assert!(rendered.contains(P4_STATIC), "{rendered}");
    assert!(!rendered.contains(S_PASS), "{rendered}");
}

/// `from_outcomes` with no recorded outcome at all (an index from before
/// outcomes were recorded) falls back to the conditional `missing` wording;
/// with outcomes that are all indexed it says nothing more. Control: drop
/// the empty-outcomes check (the first comes back `Nothing`).
#[test]
fn from_outcomes_without_any_outcome_falls_back_to_the_missing_wording() {
    let found = real_plugins(&["typescript"]);
    let coverage = warm_real(&found, &["typescript"], Vec::new());
    assert_eq!(coverage.uncovered, Uncovered::Missing(real_missing(&found)));
    let rendered = build(&coverage);
    assert!(
        rendered.contains("If this project has python, rust or go files, they are not indexed"),
        "{rendered}"
    );

    let coverage = warm_real(&found, &["typescript"], vec![("typescript", indexed())]);
    assert_eq!(coverage.uncovered, Uncovered::Nothing);
}

/// `count` synthetic failed languages with `error_bytes`-byte errors.
fn many_failed(count: usize, error_bytes: usize) -> Vec<(String, String)> {
    (0..count).map(|i| (format!("fail{i:02}"), "x".repeat(error_bytes))).collect()
}

/// Asserts the names and commands the ladder must never drop.
fn assert_uncovered_named(rendered: &str, absent: &[&str], failed: &[(String, String)]) {
    for language in absent {
        assert!(rendered.contains(language), "{language}: {rendered}");
        assert!(rendered.contains(&command(language)), "{language}'s command: {rendered}");
    }
    for (language, _) in failed {
        assert!(rendered.contains(language.as_str()), "{language}: {rendered}");
    }
    assert!(rendered.contains("`g-mesh reindex`"), "{rendered}");
    assert!(rendered.contains(TRAILER), "{rendered}");
}

/// Ladder step 2: sixteen failed languages with long errors, TypeScript and
/// three absent languages overflow at step 1; dropping the errors fits,
/// TypeScript still named (step 3 not reached). Control: start the ladder at
/// step 3 (TypeScript is no longer named) or skip step 2.
#[test]
fn ladder_step_2_drops_failed_errors_and_keeps_every_name() {
    let found = real_plugins(&["typescript"]);
    let failed = many_failed(16, 120);
    let coverage = Coverage {
        covered: Covered::Indexed(real_present(&found, &["typescript"])),
        uncovered: Uncovered::Recorded {
            absent: real_missing(&found).into_iter().map(|language| (language, Some(99_999))).collect(),
            failed: failed.clone(),
        },
    };
    assert!(render(&coverage, 1).len() > INSTRUCTIONS_BYTE_CEILING, "the fixture must overflow step 1");

    let rendered = build(&coverage);
    println!("16 failed + typescript + 3 absent: {} bytes (step 2)", rendered.len());

    assert_eq!(rendered, render(&coverage, 2));
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    assert!(!rendered.contains("xxxxxxxx"), "the errors are dropped: {rendered}");
    assert!(rendered.contains(&p4_perm("typescript")), "the never-list is still named: {rendered}");
    assert_uncovered_named(&rendered, &["python", "rust", "go"], &failed);
}

/// Ladder step 3: never-resolving languages too many to name alongside
/// absent and failed ones; the generic paragraph replaces the list and
/// absent/failed names and commands stay. Control: skip step 3 (step 4's
/// rendering comes back, or the ceiling breaks).
#[test]
fn ladder_step_3_keeps_absent_and_failed_names() {
    let failed = many_failed(2, 20);
    let never: Vec<PresentLanguage> = (0..40)
        .map(|i| PresentLanguage { language: format!("never{i:02}"), capabilities: Capabilities::default() })
        .collect();
    let found = real_plugins(&["typescript"]);
    let coverage = Coverage {
        covered: Covered::Indexed(never),
        uncovered: Uncovered::Recorded {
            absent: real_missing(&found).into_iter().map(|language| (language, Some(7))).collect(),
            failed: failed.clone(),
        },
    };
    assert!(render(&coverage, 2).len() > INSTRUCTIONS_BYTE_CEILING, "the fixture must overflow step 2");

    let rendered = build(&coverage);
    println!("40 never + 3 absent + 2 failed: {} bytes (step 3)", rendered.len());

    assert_eq!(rendered, render(&coverage, 3));
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    assert!(rendered.contains(P4_PERM_FALLBACK), "{rendered}");
    assert!(rendered.contains("Indexed here: never00"), "the covered list survives step 3: {rendered}");
    assert_uncovered_named(&rendered, &["python", "rust", "go"], &failed);
}

/// Ladder step 4: a covered list too long even with the generic receiver
/// paragraph; the list gives way to `HEAD_NO_LIST`, and the absent and
/// failed names and commands stay. Static languages, so step 3 frees
/// nothing. Control: drop step 4 (the ceiling breaks).
#[test]
fn ladder_step_4_replaces_the_covered_list_and_keeps_absent_and_failed_names() {
    let failed = many_failed(2, 20);
    let static_tier = Capabilities {
        semantic_pass: false,
        semantic_sweep: false,
        semantic_prepare: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Resolved,
    };
    let many: Vec<PresentLanguage> = (0..80)
        .map(|i| PresentLanguage { language: format!("static{i:02}"), capabilities: static_tier })
        .collect();
    let found = real_plugins(&["typescript"]);
    let coverage = Coverage {
        covered: Covered::Indexed(many),
        uncovered: Uncovered::Recorded {
            absent: real_missing(&found).into_iter().map(|language| (language, None)).collect(),
            failed: failed.clone(),
        },
    };
    assert!(render(&coverage, 3).len() > INSTRUCTIONS_BYTE_CEILING, "the fixture must overflow step 3");

    let rendered = build(&coverage);
    println!("80 static + 3 absent + 2 failed: {} bytes (step 4)", rendered.len());

    assert_eq!(rendered, render(&coverage, 4));
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    assert!(rendered.contains(HEAD_NO_LIST), "{rendered}");
    assert!(!rendered.contains("static00"), "{rendered}");
    assert_uncovered_named(&rendered, &["python", "rust", "go"], &failed);
}

/// The byte table of ADR 0022, section 4, re-measured against the real
/// manifests and catalogue: every realistic scenario fits at ladder step 1
/// under the ceiling. Run with `--nocapture` for the table. Control: any
/// template growing past the ceiling for one of these.
#[test]
fn adr_0022_byte_table_every_realistic_scenario_fits_at_step_1() {
    let all = ["go", "python", "rust", "typescript"];
    let rust_error = format!("rust plugin failed: {}", "e".repeat(162));
    assert_eq!(rust_error.len(), 182);
    let long_error = "x".repeat(182);
    let ts = real_plugins(&["typescript"]);
    let rust = real_plugins(&["rust"]);
    let both = real_plugins(&["rust", "typescript"]);
    let every = real_plugins(&all);
    let none = real_plugins(&[]);

    let warm_scenarios: Vec<(&str, Coverage)> = vec![
        ("TypeScript only", warm_real(&ts, &["typescript"], vec![("typescript", indexed())])),
        ("Rust only", warm_real(&rust, &["rust"], vec![("rust", indexed())])),
        (
            "TypeScript + Rust",
            warm_real(&both, &["rust", "typescript"], vec![("rust", indexed()), ("typescript", indexed())]),
        ),
        (
            "TypeScript + Python absent (214 files)",
            warm_real(&ts, &["typescript"], vec![("typescript", indexed()), ("python", absent(Some(214)))]),
        ),
        (
            "Four states",
            warm_real(
                &real_plugins(&["rust", "typescript"]),
                &["typescript"],
                vec![
                    ("typescript", indexed()),
                    ("rust", failed(&rust_error)),
                    ("go", absent(Some(12_345))),
                    ("python", absent(Some(54_321))),
                ],
            ),
        ),
        (
            "Zero plugins, all four absent",
            warm_real(&none, &[], all.iter().map(|language| (*language, absent(Some(10_000)))).collect()),
        ),
        (
            "Four discovered, three failed with long errors",
            warm_real(
                &every,
                &["typescript"],
                vec![
                    ("go", failed(&long_error)),
                    ("python", failed(&long_error)),
                    ("rust", failed(&long_error)),
                    ("typescript", indexed()),
                ],
            ),
        ),
        (
            "All four failed",
            warm_real(&every, &[], all.iter().map(|language| (*language, failed(&long_error))).collect()),
        ),
    ];
    for (name, coverage) in &warm_scenarios {
        let rendered = build(coverage);
        println!("{name}: {} bytes", rendered.len());
        assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{name}: {} bytes", rendered.len());
        assert_eq!(&rendered, &render(coverage, 1), "{name} must fit at step 1");
    }

    let root = root_of_byte_len(103);
    for (name, found, installed) in [
        ("0 missing", &every, &all[..]),
        ("3 missing", &ts, &["typescript"][..]),
        ("4 missing", &none, &[][..]),
    ] {
        let coverage = Coverage {
            covered: Covered::Installed(real_present(found, installed)),
            uncovered: Uncovered::missing(real_missing(found)),
        };
        for walking in [false, true] {
            let rendered = cold_start(&root, walking, &coverage);
            println!("Cold start, 103-byte root, {name}, walking={walking}: {} bytes", rendered.len());
            assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{} bytes", rendered.len());
            assert!(rendered.starts_with("Index root: /"), "the root fits: {rendered}");
            assert!(rendered.ends_with(&render(&coverage, 1)), "{name} must fit at step 1: {rendered}");
        }
    }
}

// --- The server path: `GMeshMcpServer::instructions` over a real store. ---

/// A store holding one `File` node per language in `languages`.
fn store_with_files(languages: &[&str]) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for language in languages {
        let file = format!("src/main.{language}");
        upsert_node(
            &mut conn,
            NodeRecord::new(format!("file-{language}"), "File", &file, &file, &file, *language),
        )
        .unwrap();
    }
    conn
}

fn record_outcomes(store: &IndexStore, outcomes: Vec<(&str, LanguageOutcome)>) {
    let outcomes: BTreeMap<String, LanguageOutcome> =
        outcomes.into_iter().map(|(language, outcome)| (language.to_string(), outcome)).collect();
    store.with(|conn| schema::record_language_outcomes(conn, &outcomes)).unwrap();
}

/// A server over `store`, with `found` as its discovered plugins, at
/// `phase`.
fn server_over(
    dir: &tempfile::TempDir,
    found: DiscoveredPlugins,
    store: Arc<IndexStore>,
    phase: Phase,
) -> GMeshMcpServer {
    let root = dir.path().join("project");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let indexing = IndexingStatus::structural();
    indexing.set_phase(phase);
    let registry = Arc::new(PluginRegistry::new(
        &root,
        state,
        found,
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    ));
    GMeshMcpServer::new(
        store,
        registry,
        CoreActivity::new(),
        indexing,
        Arc::new(EmbeddingPipeline::disabled()),
    )
}

/// Absent languages come from the recorded outcomes, not re-derived from the
/// catalogue: Python, recorded absent with 7 files, is named with its count;
/// Rust and Go, missing from discovery but with no recorded outcome (no
/// files), are not mentioned. Control: build the absent list from
/// `missing_languages()` in `instructions`.
#[test]
fn the_server_names_absent_languages_from_the_recorded_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(IndexStore::new(store_with_files(&["typescript"])));
    record_outcomes(&store, vec![("typescript", indexed()), ("python", absent(Some(7)))]);
    let server = server_over(&dir, real_plugins(&["typescript"]), Arc::clone(&store), Phase::Ready);

    let rendered = server.instructions();

    assert!(rendered.contains(&format!("Indexed here: typescript. {UNSUPPORTED}")), "{rendered}");
    assert!(rendered.contains(&format!("python (7 files; {})", command("python"))), "{rendered}");
    assert!(!rendered.contains(&command("rust")) && !rendered.contains(&command("go")), "{rendered}");
}

/// An outcomes read that fails (here: the table is gone) falls back to the
/// cold-start `missing()` wording for the discovered plugins, with no panic,
/// and the indexed list still read. Control: on that error, use
/// `Uncovered::Nothing` (the conditional sentence disappears).
#[test]
fn an_outcomes_read_error_falls_back_to_the_missing_wording() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(IndexStore::new(store_with_files(&["typescript"])));
    store.with(|conn| conn.execute("DROP TABLE language_outcome", [])).unwrap();
    let server = server_over(&dir, real_plugins(&["typescript"]), Arc::clone(&store), Phase::Ready);

    let rendered = server.instructions();

    assert!(rendered.contains("Indexed here: typescript."), "{rendered}");
    assert!(
        rendered.contains(&format!(
            "If this project has python, rust or go files, they are not indexed: no plugin installed \
             ({}, {}, {}).",
            command("python"),
            command("rust"),
            command("go")
        )),
        "{rendered}"
    );
}

/// No stale gap: the same index renders byte-identical text before and after
/// Rust's semantic pass is recorded, and the capability sentence `S_PASS` is
/// in both. Control: make `instructions` drop pass-done languages' `S_PASS`
/// (or reintroduce the pre-pass gap sentence keyed on the pass bool).
#[test]
fn the_server_text_is_the_same_before_and_after_a_semantic_pass() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(IndexStore::new(store_with_files(&["rust", "typescript"])));
    record_outcomes(&store, vec![("rust", indexed()), ("typescript", indexed())]);
    let server = server_over(&dir, real_plugins(&["rust", "typescript"]), Arc::clone(&store), Phase::Ready);

    let before = server.instructions();
    store.with(|conn| schema::record_language_semantic_pass(conn, "rust")).unwrap();
    assert!(store.with(|conn| schema::language_semantic_pass_done(conn, "rust")).unwrap());
    let after = server.instructions();

    assert_eq!(before, after);
    assert!(before.contains(S_PASS), "{before}");
    assert!(before.contains(&p4_perm("typescript")), "{before}");
}

/// `Phase::Failed` with every discovered plugin failed renders the
/// nothing-indexed text, no receiver paragraph, each language failed.
/// Control: render `Phase::Failed` through the cold-start path (the
/// "Plugins installed" head appears).
#[test]
fn the_server_renders_a_failed_walk_as_nothing_indexed() {
    let all = ["go", "python", "rust", "typescript"];
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(IndexStore::new(store_with_files(&[])));
    record_outcomes(&store, all.iter().map(|language| (*language, failed("exited\nmore"))).collect());
    let server = server_over(
        &dir,
        real_plugins(&all),
        Arc::clone(&store),
        Phase::Failed("every plugin failed".into()),
    );

    let rendered = server.instructions();

    assert!(rendered.contains(HEAD_NONE), "{rendered}");
    assert!(!rendered.contains(RECEIVER_OPENING), "{rendered}");
    assert!(
        rendered.contains("go (exited), python (exited), rust (exited), typescript (exited)"),
        "{rendered}"
    );
    assert!(!rendered.contains("Plugins installed"), "{rendered}");
}

/// The cold start through the server: installed plugins from discovery, the
/// missing catalogue languages conditionally, the wait wording. Control:
/// pass `Uncovered::Nothing` at the cold start.
#[test]
fn the_server_cold_start_names_installed_and_missing_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(IndexStore::new(store_with_files(&[])));
    for phase in [Phase::Unindexed, Phase::Walking] {
        let server = server_over(&dir, real_plugins(&["typescript"]), Arc::clone(&store), phase);
        let rendered = server.instructions();
        assert!(rendered.contains("Plugins installed: typescript."), "{rendered}");
        assert!(rendered.contains("If this project has python, rust or go files"), "{rendered}");
        assert!(rendered.contains("slow, not wrong"), "{rendered}");
    }
}
