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

/// TypeScript under a manifest with no semantic tier and receiver calls
/// unresolved in both tiers: a never-resolving language, which is what the
/// baseline rendering [`ORIGINAL_INSTRUCTIONS`] describes. The shipped
/// manifest is pass-dependent instead
/// ([`the_real_typescript_manifest_is_pass_dependent`]).
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
            files_created: false,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
            member_overrides: crate::daemon::manifest::MemberOverrides::None,
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

/// Nothing indexed: the coverage paragraph says so, there is no receiver
/// paragraph to render (ADR 0022, section 3), and P2 drops [`P2_GAP`],
/// which would point at it. Control: render `P2_GAP` whatever
/// `receiver_paragraph` returns.
#[test]
fn nothing_indexed_says_so_and_leaves_out_the_receiver_paragraph() {
    let rendered = build(&warm(Vec::new()));
    assert!(rendered.contains(HEAD_NONE), "{rendered}");
    assert!(!rendered.contains("The one legitimate reason to grep afterward"), "{rendered}");
    assert!(!rendered.contains(P2_GAP), "P2 never points at a missing paragraph:\n{rendered}");
    assert!(rendered.contains(&format!("{P2}\n\n{P5}")), "{rendered}");
}

/// What every shipped pass-dependent language renders: no language is
/// never-resolving and every one reports `overrides`, so the receiver
/// paragraph is [`S_PASS_ALONE`] - the static sentence about override
/// caller pages is gone, because the page itself now names the base members
/// (D7) - and P2 keeps [`P2_GAP`], which points at it. Checked once here
/// rather than transcribed per language.
///
/// `"may produce no edge"` is the never-resolving wording, so its absence
/// is what separates this rendering from TypeScript's.
fn assert_pass_dependent_receiver_clause(rendered: &str, language: &str) {
    assert!(
        rendered.contains(&format!("{P2} {P2_GAP}\n\n{S_PASS_ALONE}\n\n{P5}")),
        "{language}: P2 with its gap sentence, then the pass sentence alone:\n{rendered}"
    );
    assert!(!rendered.contains(P4_STATIC), "{language}: the page names overrides itself:\n{rendered}");
    assert!(!rendered.contains(S_PASS), "{language}: nothing precedes the pass sentence:\n{rendered}");
    assert!(!rendered.contains("One real gap"), "{language}: the withdrawn claim must not return");
    assert!(
        !rendered.contains("may produce no edge"),
        "{language}: that is the never-resolving wording:\n{rendered}"
    );
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "{language}: {} bytes exceeds the {INSTRUCTIONS_BYTE_CEILING}-byte ceiling",
        rendered.len()
    );
    println!("{language}-only bytes: {}", rendered.len());
}

/// Rust resolves receiver calls only through its semantic pass and declares
/// the member a trait-impl method implements, so it renders
/// [`S_PASS_ALONE`] whatever its pass state; that state reaches the caller
/// through `provenance` (ADR 0022, section 2).
///
/// The override gap the page now names in `overrides`, measured on this
/// plugin's own fixture:
/// `find_callers("shapes::Shape::area")` is
/// `{shapes::total_dyn, gaps::measure}` - `&dyn Shape` and `<S: Shape>`
/// both land on the trait's declaration - while
/// `find_callers("shapes::<Circle as Shape>::area")` is **empty**,
/// though either of those two call sites reaches it at run time.
/// `conformance/expect.toml` asserts both as exact sets.
#[test]
fn rust_only_renders_the_pass_sentence_alone() {
    let rendered = build(&warm(vec![rust_present()]));
    assert_pass_dependent_receiver_clause(&rendered, "rust");
}

/// Python, the same way (`by_name`): pyright resolves a receiver call
/// against its annotation, so `obj.describe()` for `obj: Base` is attributed to
/// `Base.describe` however the object was built. Measured:
/// `find_callers("Base.describe")` carries
/// `pkg/callers.py:through_a_base_annotation`, and
/// `find_callers("Deep.describe")` does not, though `Deep` overrides
/// `describe` and `find_implementations("Base")` names it.
#[test]
fn python_only_renders_the_pass_sentence_alone() {
    let rendered = build(&warm(vec![PresentLanguage {
        language: "python".to_string(),
        capabilities: bundled_python_capabilities(),
    }]));
    assert_pass_dependent_receiver_clause(&rendered, "python");
}

/// Go, the same way (`by_name`). On `plugins/go/conformance/project`, with the pass
/// complete, `find_callers("Conn.Close")` answers `results: []` while
/// `server/conn.go:CloseAll` closes a `Conn` through a `Closer` value and
/// `find_implementations("Closer")` names `Conn`.
#[test]
fn go_only_renders_the_pass_sentence_alone() {
    let rendered = build(&warm(vec![go_present()]));
    assert_pass_dependent_receiver_clause(&rendered, "go");
}

/// The silence half of the control: a TypeScript that declares
/// `receiver_calls = "unresolved"` in *both* tiers ([`typescript_present`])
/// is named as never resolving, and the sentence about binding to a
/// declared type must never appear for it.
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
    // All four real manifests resolve receiver calls through their semantic
    // pass and report `overrides`, so none is named in the gap list and the
    // pass sentence stands alone.
    assert!(!rendered.contains(&p4_perm("typescript")), "{rendered}");
    assert!(!rendered.contains(P4_STATIC), "{rendered}");
    assert!(rendered.contains(&format!("{P2} {P2_GAP}\n\n{S_PASS_ALONE}")), "{rendered}");
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

/// Failed: the language is named with the innermost cause of its error and
/// the `g-mesh reindex` retry, never as indexed, and the trailer follows.
/// Controls: drop the failed arm; render the whole chain instead of
/// `error_cause`.
#[test]
fn a_failed_language_names_its_error_cause_and_the_reindex_command() {
    let all = ["go", "python", "rust", "typescript"];
    let found = real_plugins(&all);
    let coverage = warm_real(
        &found,
        &["go", "python", "typescript"],
        vec![
            ("go", indexed()),
            ("python", indexed()),
            ("rust", failed("rust plugin exited during the handshake\nNo such file or directory")),
            ("typescript", indexed()),
        ],
    );

    let rendered = build(&coverage);
    println!("one failed: {} bytes", rendered.len());

    assert!(
        rendered.contains(
            "Not indexed, plugin failed: rust (No such file or directory) - fix the plugin, \
             then run `g-mesh reindex`."
        ),
        "{rendered}"
    );
    assert!(!rendered.contains("handshake"), "only the innermost cause: {rendered}");
    assert!(rendered.contains("Indexed here: go, python and typescript."), "{rendered}");
    assert!(rendered.contains(TRAILER), "{rendered}");
}

/// [`error_cause`]: the innermost cause only, at most [`ERROR_BYTES`]
/// bytes, cut on a char boundary when byte 97 falls inside a multi-byte
/// char. Controls: cut at `&line[..97]` without the boundary walk (panics on
/// the multi-byte inputs); drop the length check (the long line comes back
/// whole).
#[test]
fn error_cause_keeps_the_innermost_cause_within_100_bytes_on_a_char_boundary() {
    assert_eq!(error_cause("first\nsecond"), "second");
    assert_eq!(error_cause(""), "");

    let exactly = "e".repeat(ERROR_BYTES);
    assert_eq!(error_cause(&exactly), exactly, "100 bytes is kept whole");

    let over = "e".repeat(ERROR_BYTES + 1);
    let cut = error_cause(&over);
    assert_eq!(cut, format!("{}...", "e".repeat(ERROR_BYTES - 3)));
    assert_eq!(cut.len(), ERROR_BYTES);

    // A two-byte char spanning bytes 96-97: byte 97 is inside it.
    let two_byte = format!("{}é{}", "a".repeat(96), "z".repeat(20));
    let cut = error_cause(&two_byte);
    assert_eq!(cut, format!("{}...", "a".repeat(96)));
    assert!(cut.len() <= ERROR_BYTES);

    // Three-byte chars only: 97 is not a multiple of 3.
    let three_byte = "日".repeat(40);
    let cut = error_cause(&three_byte);
    assert_eq!(cut, format!("{}...", "日".repeat(32)));
    assert!(cut.len() <= ERROR_BYTES);

    // The cut applies to the innermost cause, not to the whole chain.
    let chain = format!("outer\n{}", "日".repeat(40));
    assert_eq!(error_cause(&chain), format!("{}...", "日".repeat(32)));
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

/// TypeScript's real manifest: its semantic tier (vtsls) resolves receiver
/// calls and the structural tier does not, so it is pass-dependent - never
/// named in the gap list; `P4_STATIC` plus `S_PASS`. Control: set the
/// manifest's `receiver_calls` back to `"unresolved"` (it becomes never and
/// is named).
#[test]
fn the_real_typescript_manifest_is_pass_dependent() {
    let found = real_plugins(&["typescript"]);
    let present = real_present(&found, &["typescript"]);
    assert_eq!(receiver_class(&present[0].capabilities), ReceiverClass::PassDependent);
    assert_eq!(present[0].capabilities.member_overrides, MemberOverrides::ByName);
    let rendered = build(&warm(present));
    assert!(!rendered.contains(&p4_perm("typescript")), "{rendered}");
    assert_pass_dependent_receiver_clause(&rendered, "typescript");
}

/// Go, Python and Rust from their real manifests are pass-dependent and
/// report `overrides` (Go and Python by name, Rust declared): never named in
/// the gap list, and [`S_PASS_ALONE`] instead of `P4_STATIC` plus `S_PASS`.
/// Controls: drop the `PassDependent` arm (they become never and are named);
/// restore the `(true, _) => P4_STATIC` arm in `receiver_paragraph`.
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
    let modes: Vec<(String, MemberOverrides)> = real_present(&found, &three)
        .into_iter()
        .map(|present| (present.language, present.capabilities.member_overrides))
        .collect();
    assert_eq!(
        modes,
        vec![
            ("go".to_string(), MemberOverrides::ByName),
            ("python".to_string(), MemberOverrides::ByName),
            ("rust".to_string(), MemberOverrides::Declared),
        ]
    );
    let rendered = build(&warm(real_present(&found, &three)));
    assert_pass_dependent_receiver_clause(&rendered, "go, python and rust");
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
        files_created: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Unresolved,
        member_overrides: crate::daemon::manifest::MemberOverrides::None,
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
        files_created: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Resolved,
        member_overrides: crate::daemon::manifest::MemberOverrides::None,
    };
    assert_eq!(receiver_class(&capabilities), ReceiverClass::Static);
    let rendered = build(&warm(vec![PresentLanguage { language: "zig".to_string(), capabilities }]));
    assert!(rendered.contains(&format!("{P2} {P2_GAP}\n\n{P4_STATIC}\n\n{P5}")), "{rendered}");
    assert!(!rendered.contains(S_PASS), "{rendered}");
}

// --- D7: which receiver paragraph, by `member_overrides` -------------------------

/// A pass-dependent language whose manifest names no overrides (`none`).
fn silent_pass_dependent(language: &str) -> PresentLanguage {
    bridge_semantic(language)
}

/// A language that resolves receiver calls structurally and reports
/// overrides: no gap at all.
fn static_reporting(language: &str) -> PresentLanguage {
    PresentLanguage {
        language: language.to_string(),
        capabilities: Capabilities {
            semantic_pass: false,
            semantic_sweep: false,
            semantic_prepare: false,
            files_created: false,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Resolved,
            member_overrides: MemberOverrides::ByName,
        },
    }
}

/// One covered language with `member_overrides = "none"` keeps the static
/// sentence and `S_PASS` for the whole project, beside languages that report
/// overrides: its override pages say nothing. Control: drop the
/// `silent_on_overrides` arm of `receiver_paragraph` (`S_PASS_ALONE`).
#[test]
fn a_language_that_reports_no_overrides_keeps_the_static_sentence() {
    for present in [vec![silent_pass_dependent("zig")], vec![go_present(), silent_pass_dependent("zig")]] {
        let rendered = build(&warm(present));
        assert!(rendered.contains(&format!("{P2} {P2_GAP}\n\n{P4_STATIC} {S_PASS}\n\n{P5}")), "{rendered}");
        assert!(!rendered.contains(S_PASS_ALONE), "{rendered}");
    }
}

/// A never-resolving language is named whatever its `member_overrides`, and
/// `S_PASS` follows the named paragraph for a pass-dependent one beside it.
/// Control: test `silent_on_overrides` before the `never` arms.
#[test]
fn a_never_resolving_language_is_named_even_when_it_reports_overrides() {
    let never = PresentLanguage {
        language: "typescript".to_string(),
        capabilities: Capabilities { member_overrides: MemberOverrides::ByName, ..Capabilities::default() },
    };
    assert_eq!(receiver_class(&never.capabilities), ReceiverClass::Never);
    let rendered = build(&warm(vec![never, rust_present()]));
    assert!(rendered.contains(&format!("{} {S_PASS}", p4_perm("typescript"))), "{rendered}");
    assert!(rendered.contains(&format!("{P2} {P2_GAP}")), "{rendered}");
    assert!(!rendered.contains(S_PASS_ALONE), "{rendered}");
}

/// Every language static and reporting overrides: no gap to name, so no
/// receiver paragraph, and P2 without [`P2_GAP`]. Control: render `P2_GAP`
/// whatever `receiver_paragraph` returns (or return `P4_STATIC` there).
#[test]
fn no_gap_to_name_renders_no_receiver_paragraph_and_no_gap_pointer() {
    let rendered = build(&warm(vec![static_reporting("zig"), static_reporting("odin")]));
    assert!(!rendered.contains(RECEIVER_OPENING), "{rendered}");
    assert!(!rendered.contains(P2_GAP), "{rendered}");
    assert!(rendered.contains(&format!("{P2}\n\n{P5}")), "{rendered}");
}

/// Every D7 form, at its realistic size, renders within the ceiling; the
/// byte counts are printed for ADR 0022's table (`-- --nocapture`).
#[test]
fn every_receiver_form_renders_within_the_ceiling() {
    let all = ["go", "python", "rust", "typescript"];
    let found = real_plugins(&all);
    let forms = [
        ("nothing indexed", warm(Vec::new())),
        ("four real manifests (S_PASS_ALONE)", warm(real_present(&found, &all))),
        ("four real + one silent (P4_STATIC + S_PASS)", {
            let mut present = real_present(&found, &all);
            present.push(silent_pass_dependent("zig"));
            warm(present)
        }),
        ("never + rust (p4_perm + S_PASS)", warm(vec![typescript_present(), rust_present()])),
        ("all static, reporting (no paragraph)", warm(vec![static_reporting("zig")])),
        ("worst case", warm(worst_case_present())),
    ];
    for (form, coverage) in forms {
        let rendered = build(&coverage);
        println!("{form}: {} bytes", rendered.len());
        assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING, "{form}: {} bytes", rendered.len());
    }
    println!(
        "P4_STATIC + S_PASS: {} bytes, S_PASS_ALONE: {} bytes",
        P4_STATIC.len() + 1 + S_PASS.len(),
        S_PASS_ALONE.len()
    );
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

/// Ladder step 2: sixteen failed languages with long errors, a
/// never-resolving TypeScript and three absent languages overflow at step 1;
/// dropping the errors fits, TypeScript still named (step 3 not reached).
/// Control: start the ladder at step 3 (TypeScript is no longer named) or
/// skip step 2.
#[test]
fn ladder_step_2_drops_failed_errors_and_keeps_every_name() {
    let found = real_plugins(&["typescript"]);
    let failed = many_failed(16, 120);
    let coverage = Coverage {
        covered: Covered::Indexed(ts_only()),
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
        files_created: false,
        receiver_calls: ReceiverCallResolution::Resolved,
        receiver_calls_structural: ReceiverCallResolution::Resolved,
        member_overrides: crate::daemon::manifest::MemberOverrides::None,
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
/// Rust's semantic pass is recorded, and the capability sentence
/// `S_PASS_ALONE` is in both. Control: make `instructions` drop pass-done languages' `S_PASS`
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
    assert!(before.contains(S_PASS_ALONE), "{before}");
    assert!(!before.contains(P4_STATIC), "{before}");
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
    assert!(rendered.contains("go (more), python (more), rust (more), typescript (more)"), "{rendered}");
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

// ---------------------------------------------------------------------------
// GM-330/S10: the failed item's cause (ADR 0022, section 3 failed row and
// "Resolved at review" item 2) against errors stored by
// `languages::failed_error`, one cause per line, outermost first.
// ---------------------------------------------------------------------------

/// The failed item `{lang} ({cause})` for `language` out of `rendered`.
fn failed_item<'a>(rendered: &'a str, language: &str) -> &'a str {
    let start = rendered
        .find(&format!("plugin failed: {language} ("))
        .unwrap_or_else(|| panic!("no failed item for {language}: {rendered}"));
    let item = &rendered[start + "plugin failed: ".len()..];
    let end = item.find(") - fix the plugin").unwrap_or_else(|| panic!("unterminated item: {rendered}"));
    &item[..=end]
}

/// `rendered` for the four bundled languages with `rust` failed with
/// `error` and the other three indexed.
fn rendered_with_rust_failed(error: &str) -> String {
    let found = real_plugins(&["go", "python", "rust", "typescript"]);
    build(&warm_real(
        &found,
        &["go", "python", "typescript"],
        vec![("go", indexed()), ("python", indexed()), ("rust", failed(error)), ("typescript", indexed())],
    ))
}

/// The real failure (GM-330/S4): a plugin laid out as an installed one
/// (`command = "./g-mesh-plugin-rust"`) whose binary is gone, under a deep
/// root, walked by the real `bulk_index::run`. The stored error is the whole
/// chain, the path in an outer cause; the rendered item shows only the OS
/// cause, read from `std::io::Error` rather than written here, and no path.
///
/// Control: make `error_cause` take the first non-empty line instead of the
/// last (the item becomes the outermost context and the exact-item
/// assertion fails).
#[test]
fn a_real_missing_plugin_binary_renders_the_os_cause_without_the_path() {
    use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let plugins =
        home.path().join("a-plugin-root-deep-enough-that-its-path-alone-fills-the-hundred-byte-budget");
    let rust = plugins.join("rust");
    std::fs::create_dir_all(&rust).unwrap();
    std::fs::write(
        rust.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"rust\"\nprotocol_version = {CURRENT_PROTOCOL_VERSION}\n\
             plugin_version = \"0.0.0-test\"\n\n[plugin.spawn]\ncommand = \"./g-mesh-plugin-rust\"\n\n\
             [plugin.languages]\nextensions = [\".rs\"]\n"
        ),
    )
    .unwrap();
    crate::daemon::test_plugin::install(&plugins, "alpha", &[".alpha-src"]);
    let discovered = discover(std::slice::from_ref(&plugins)).expect("the fixture plugins must discover");
    let store = IndexStore::new(store_with_files(&[]));

    let summary = crate::daemon::bulk_index::run(project.path(), &store, None, &discovered)
        .expect("one failed language must not fail the walk");

    let error = match summary.outcomes.get("rust") {
        Some(LanguageOutcome::Failed { error }) => error.clone(),
        other => panic!("rust must be Failed, got {other:?}"),
    };
    // ENOENT on Unix and ERROR_FILE_NOT_FOUND on Windows are both 2.
    let os_message = std::io::Error::from_raw_os_error(2).to_string();
    assert!(error.lines().count() >= 2, "a chain, one cause per line: {error}");
    assert!(error.contains(&rust.display().to_string()), "an outer cause names the path: {error}");
    assert_eq!(error.lines().last(), Some(os_message.as_str()), "the innermost cause is the OS one: {error}");

    let rendered = rendered_with_rust_failed(&error);

    assert_eq!(failed_item(&rendered, "rust"), format!("rust ({os_message})"), "{rendered}");
    #[cfg(unix)]
    assert!(rendered.contains("rust (No such file or directory"), "{rendered}");
    assert!(!rendered.contains(&home.path().display().to_string()), "{rendered}");
    assert!(!failed_item(&rendered, "rust").contains('/'), "{rendered}");
}

/// A cause whose own text contains ": " (a serde-like message) is shown
/// whole: the store's line, not a separator, bounds a cause.
///
/// Control: reintroduce a last-": " split in `error_cause`
/// (`cause.rsplit(": ").next()`) - the item becomes `rust (map, expected a
/// string at line 3)` and the exact comparison fails.
#[test]
fn a_cause_containing_colon_space_survives_whole_in_the_failed_item() {
    use anyhow::Context;

    let inner: Result<(), std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid type: map, expected a string at line 3",
    ));
    let err = inner.context("reading the manifest").context("loading the rust plugin").unwrap_err();
    let stored = languages::failed_error(&err);

    assert_eq!(error_cause(&stored), "invalid type: map, expected a string at line 3");
    let rendered = rendered_with_rust_failed(&stored);
    assert_eq!(
        failed_item(&rendered, "rust"),
        "rust (invalid type: map, expected a string at line 3)",
        "{rendered}"
    );
}

/// [`shorten_paths`]: an absolute or `~/` path token becomes its last
/// component, with brackets, quotes and trailing punctuation kept; "/" alone
/// and relative paths are left as they are. The last assertion goes through
/// [`error_cause`], so it pins that the cause is shortened at all.
///
/// Control: drop the `shorten_paths` call from `error_cause` - the
/// `error_cause` assertion fails (the direct `shorten_paths` assertions
/// catch a `shorten_paths` that returns `text` unchanged).
#[test]
fn shorten_paths_keeps_only_the_last_component_of_rooted_paths() {
    assert_eq!(shorten_paths("/usr/local/lib/g-mesh/plugins/rust/plugin.toml"), "plugin.toml");
    assert_eq!(shorten_paths("~/.g-mesh/plugins/rust/g-mesh-plugin-rust"), "g-mesh-plugin-rust");
    assert_eq!(shorten_paths("spawn (/private/tmp/x/plugin.js) failed"), "spawn (plugin.js) failed");
    assert_eq!(shorten_paths("open \"/a/b/c.toml\" failed"), "open \"c.toml\" failed");
    assert_eq!(shorten_paths("open '/a/b/c.toml'"), "open 'c.toml'");
    assert_eq!(shorten_paths("[/a/b/c.rs]"), "[c.rs]");
    assert_eq!(shorten_paths("`~/x/y.js`"), "`y.js`");
    assert_eq!(shorten_paths("in /a/b/c.rs, then /d/e.rs; and /f/g.rs."), "in c.rs, then e.rs; and g.rs.");
    assert_eq!(shorten_paths("reading /a/b/c.toml: bad"), "reading c.toml: bad");
    assert_eq!(shorten_paths("/"), "/");
    assert_eq!(shorten_paths("a / b"), "a / b");
    assert_eq!(
        shorten_paths("src/lib.rs and ./plugin.js and ../x/y"),
        "src/lib.rs and ./plugin.js and ../x/y"
    );
    assert_eq!(shorten_paths("no paths here"), "no paths here");

    // Windows roots, on every host: drive with either separator, UNC and
    // extended-length; a bare drive or a relative backslash path is kept.
    assert_eq!(
        shorten_paths(r"C:\Users\RUNNER~1\target\debug\g-mesh-plugin-rust.exe"),
        "g-mesh-plugin-rust.exe"
    );
    assert_eq!(shorten_paths("d:/a/b/plugin.toml"), "plugin.toml");
    assert_eq!(shorten_paths(r"spawn (C:\x\plugin.js) failed"), "spawn (plugin.js) failed");
    assert_eq!(shorten_paths(r"\\srv\share\dir\c.toml"), "c.toml");
    assert_eq!(shorten_paths(r"\\?\C:\a\b.rs"), "b.rs");
    assert_eq!(shorten_paths(r"in C:\ and C: and a\b\c"), r"in C:\ and C: and a\b\c");

    assert_eq!(
        error_cause("spawning the plugin\n/opt/g-mesh/plugins/rust/g-mesh-plugin-rust is missing"),
        "g-mesh-plugin-rust is missing"
    );
    // The Windows CI shape of the unbuilt-workspace hint: no directory of
    // the runner's temp path survives into the item.
    let windows = error_cause(
        "failed to spawn the rust plugin's bulk index\n\
         Run `cargo build --workspace` in C:\\Users\\RUNNER~1\\AppData\\Local\\Temp\\.tmpRhPq7a: \
         the plugin binary C:\\Users\\RUNNER~1\\AppData\\Local\\Temp\\.tmpRhPq7a\\target\\debug\\g-mesh-plugin-rust.exe \
         has not been built yet",
    );
    assert!(
        windows.starts_with(
            "Run `cargo build --workspace` in .tmpRhPq7a: the plugin binary g-mesh-plugin-rust.exe has"
        ),
        "{windows}"
    );
    assert!(!windows.contains("RUNNER~1"), "{windows}");
}

/// The 100-byte cap applies after shortening: a cause over 100 bytes only
/// because of its path is kept whole, and one still over 100 bytes after
/// shortening is cut on a char boundary when byte 97 falls inside a
/// multi-byte char.
///
/// Controls: cut with `&cause[..97]` and no boundary walk (panics on the
/// second input); cap before shortening (the first input comes back cut).
#[test]
fn the_cap_applies_after_shortening_and_cuts_on_a_char_boundary() {
    let deep = format!("/{}/file.rs", "d".repeat(150));
    let short = format!("outer context\n{deep} is missing");
    assert!(short.lines().last().unwrap().len() > ERROR_BYTES);
    assert_eq!(error_cause(&short), "file.rs is missing", "shortened first, so nothing to cut");

    // Shortened: "file.rs " (8 bytes) + 88 'a' = 96 bytes, then 'é' spans
    // bytes 96-97, so byte 97 is inside it.
    let long = format!("outer context\n{deep} {}é{}", "a".repeat(88), "z".repeat(40));
    let cut = error_cause(&long);
    assert_eq!(cut, format!("file.rs {}...", "a".repeat(88)));
    assert!(cut.len() <= ERROR_BYTES);
}

/// GM-330/S12: an unbuilt cargo-workspace plugin binary
/// (`plugin::missing_workspace_binary_hint`: `command` under
/// `<root>/target/debug/`, `<root>/Cargo.toml` present, the binary absent),
/// walked by the real `bulk_index::run`. The stored error keeps the step and
/// the hint as two causes, the hint innermost, so the rendered failed item
/// shows the hint - what to build - and not the step.
///
/// Control: revert ce33132 (`bail!("failed to spawn the {} plugin's bulk
/// index: {hint}")` in `walk_one_language_in`) - the stored error is one
/// line and the two-cause assertion fails; with that assertion removed, the
/// item reads "rust (failed to spawn the rust plugin's bulk index: the
/// plugin binary ...)" and the `starts_with`/"failed to spawn" assertions
/// fail. On Windows the item also needs `shorten_paths` to recognise a
/// drive-rooted path (pinned host-independently in
/// [`shorten_paths_keeps_only_the_last_component_of_rooted_paths`]).
#[test]
fn an_unbuilt_workspace_plugin_binary_renders_the_build_hint_not_the_step() {
    use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

    let project = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    let plugins = workspace.path().join("plugins");
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-rust");
    let rust = plugins.join("rust");
    std::fs::create_dir_all(&rust).unwrap();
    std::fs::write(
        rust.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"rust\"\nprotocol_version = {CURRENT_PROTOCOL_VERSION}\n\
             plugin_version = \"0.0.0-test\"\n\n[plugin.spawn]\ncommand = '{}'\n\n\
             [plugin.languages]\nextensions = [\".rs\"]\n",
            binary.display()
        ),
    )
    .unwrap();
    crate::daemon::test_plugin::install(&plugins, "alpha", &[".alpha-src"]);
    let discovered = discover(std::slice::from_ref(&plugins)).expect("the fixture plugins must discover");
    let store = IndexStore::new(store_with_files(&[]));

    let summary = crate::daemon::bulk_index::run(project.path(), &store, None, &discovered)
        .expect("one failed language must not fail the walk");

    let error = match summary.outcomes.get("rust") {
        Some(LanguageOutcome::Failed { error }) => error.clone(),
        other => panic!("rust must be Failed, got {other:?}"),
    };
    let hint = crate::daemon::plugin::missing_workspace_binary_hint(&binary)
        .expect("the fixture must be the unbuilt-workspace-binary shape");
    assert!(hint.contains("cargo build --workspace"), "{hint}");
    assert_eq!(
        error.lines().collect::<Vec<_>>(),
        ["failed to spawn the rust plugin's bulk index", hint.as_str()],
        "the step and the hint are separate causes, the hint innermost: {error}"
    );

    let rendered = rendered_with_rust_failed(&error);
    let item = failed_item(&rendered, "rust");

    // The hint names the spelling cargo builds on this platform
    // (`g-mesh-plugin-rust.exe` on Windows).
    let named = crate::daemon::manifest::exe_suffixed(&binary, std::env::consts::EXE_SUFFIX)
        .unwrap_or_else(|| binary.clone());
    let name = named.file_name().unwrap().to_str().unwrap();
    assert!(item.starts_with("rust (Run `cargo build --workspace` in "), "{rendered}");
    // The cap may cut the tail after the binary's name, never the command.
    assert!(item.contains(&format!("the plugin binary {name}")), "{rendered}");
    assert!(!item.contains("failed to spawn"), "{rendered}");
    assert!(!rendered.contains(&workspace.path().display().to_string()), "{rendered}");

    // The Windows spelling of the same hint, on any host: the `.exe` name
    // still leaves the whole build command inside the cause's byte cap.
    let windows = crate::daemon::plugin::missing_workspace_binary_hint_with_suffix(&binary, ".exe")
        .expect("the fixture must be the unbuilt-workspace-binary shape");
    let cause = error_cause(&windows);
    assert!(cause.starts_with("Run `cargo build --workspace` in "), "{cause}");
    assert!(cause.contains("the plugin binary g-mesh-plugin-rust.exe"), "{cause}");
}

/// GM-351: the 100-byte cap on a failed language's cause never cuts the
/// build command out of the unbuilt-workspace-binary hint. Worst case on
/// any host: the longest bundled workspace plugin binary name, read from the
/// real manifests, in its Windows `.exe` spelling, under both profiles
/// (`--release` is the longer command) and both hint variants (a workspace
/// root with `Cargo.toml`, and none: "the repository root"). Every case is
/// long enough to be cut, so the assertions are about where the cut lands:
/// after the whole command.
///
/// Control: restore the old order in
/// `plugin::missing_workspace_binary_hint_with_suffix` (binary first, then
/// "Run `{build}` in ...") - the cut lands inside the command and the
/// `contains` assertion fails (already for the first case, debug with a
/// root: 71 bytes of path and wording leave no room for the command).
#[test]
fn the_cap_never_cuts_the_build_command_out_of_the_longest_windows_hint() {
    let found = real_plugins(&["go", "python", "rust", "typescript"]);
    // The workspace-built plugins: those whose command sits under a cargo
    // `target/<profile>/` directory, as `${G_MESH_BIN_DIR}` resolves.
    let (language, name) = found
        .manifests
        .values()
        .filter(|manifest| {
            let profile = manifest.command.parent().and_then(|dir| dir.file_name());
            profile.is_some_and(|profile| profile == "debug" || profile == "release")
        })
        .map(|manifest| {
            let name = manifest.command.file_name().unwrap().to_str().unwrap();
            let name = name.strip_suffix(std::env::consts::EXE_SUFFIX).unwrap_or(name);
            (manifest.language.clone(), name.to_string())
        })
        .max_by_key(|(_, name)| name.len())
        .expect("at least one bundled plugin is workspace-built");
    let others: Vec<&str> =
        ["go", "python", "rust", "typescript"].into_iter().filter(|other| *other != language).collect();

    for with_root in [true, false] {
        for (profile, cargo_build) in
            [("debug", "`cargo build --workspace`"), ("release", "`cargo build --workspace --release`")]
        {
            let workspace = tempfile::tempdir().unwrap();
            if with_root {
                std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
            }
            let binary = workspace.path().join("target").join(profile).join(&name);
            let hint = crate::daemon::plugin::missing_workspace_binary_hint_with_suffix(&binary, ".exe")
                .expect("the fixture must be the unbuilt-workspace-binary shape");
            let error = format!("failed to spawn the {language} plugin's bulk index\n{hint}");

            let mut outcomes = vec![(language.as_str(), failed(&error))];
            outcomes.extend(others.iter().map(|other| (*other, indexed())));
            let rendered = build(&warm_real(&found, &others, outcomes));
            let item = failed_item(&rendered, &language);
            let cause = item
                .strip_prefix(&format!("{language} ("))
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or_else(|| panic!("malformed item: {item}"));

            let case = format!("{profile}, with_root={with_root}: {cause}");
            assert!(cause.ends_with("..."), "the case must reach the cap: {case}");
            assert!(cause.len() <= ERROR_BYTES, "{case}");
            assert!(cause.starts_with(&format!("Run {cargo_build} in ")), "{case}");
            assert!(cause.contains(cargo_build), "the whole command survives the cap: {case}");
            if !with_root {
                assert!(cause.starts_with(&format!("Run {cargo_build} in the repository root: ")), "{case}");
            }
        }
    }
}
