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
