use super::*;

/// This module's own baseline rendering, byte for byte - transcribed by
/// hand rather than derived from `assemble(P4_GENERIC)`, so a
/// transcription slip in this module's own paragraph constants cannot
/// accidentally agree with itself. This is the fixture
/// [`ts_only_is_byte_identical_to_the_original_string`] checks [`build`]
/// against.
///
/// "Original" names what this constant has meant since GM-262: the text
/// `get_info` returns for the common, unqualified case (nothing known
/// yet, or exactly one present language), the one every later addition to
/// this module had to keep rendering unless it had a specific reason not
/// to. It stopped being *literally* the text `get_info` returned before
/// GM-262 at GM-394, which rewrote the second paragraph's closing clause
/// (see [`P4_GENERIC`]'s own doc comment for why: a tool call issued
/// while the index is being built now waits instead of erroring, so
/// telling an agent to grep around a "still building" error was no longer
/// honest). This constant was re-pinned to match, by the same hand
/// transcription rule, so it still catches a drift in the five paragraph
/// constants - just against the current baseline, not the GM-262 one.
const ORIGINAL_INSTRUCTIONS: &str =
    "Structural code-graph queries over this project's index. Prefer these over \
grepping when you need definitions, references, call edges or imports.\n\n\
A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
(excludes other same-named declarations' call sites, same guarantee either \
way), is already resolved per call site to that exact declaration - do not \
re-check it with grep as a routine habit. Only fall back to grep for the one \
specific gap below, never as a general double-check.\n\n\
`resolved: false` marks the one thing the indexer could not settle alone: an \
edge whose target is in *another* file, where whether that file exports the \
name isn't knowable from the usage alone. Every same-file edge is \
`resolved: true` - never a reason to grep. find_references/find_callers/\
find_callees/find_implementations also carry a response-level \
`allUnresolved: true` when *every* row in a non-empty page is unconfirmed - \
the page otherwise looks complete (`hasMore: false`, plausible results), so \
check this field, not just individual rows. Never set on an empty page.\n\n\
The one legitimate reason to grep afterward: a method call through a \
variable receiver (`x.foo()`) produces no edge by design, so caller/reference \
lists for methods can under-report; bare function calls and this/super/qualified-type \
calls have no such gap, and a `hasMore: false` page for those is exhaustive. On a \
project's first index, or a re-index after an upgrade, a tool call waits for the walk \
to finish before answering - slow, not wrong; do not abandon it for grep.\n\n\
Efficient usage: pass `symbol_name` directly to the four tools above instead \
of calling find_definition first, and raise `limit` for symbols with many \
results instead of paging.";

fn ts_only() -> Vec<PresentLanguage> {
    vec![PresentLanguage {
        language: "typescript".to_string(),
        capabilities: Capabilities::default(),
        semantic_pass_done: false,
    }]
}

/// The bundled Go plugin's own `[plugin.capabilities]`, read off
/// `plugins/go/plugin.toml` rather than transcribed - so the flip GM-281
/// made there (`receiver_calls = "resolved"`, structural still
/// `"unresolved"`) is what these tests actually render from, and a later
/// edit to that manifest changes what they assert instead of quietly
/// disagreeing with it.
fn bundled_go_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/go");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Go plugin's manifest must be readable")
        .capabilities
}

fn go_present(semantic_pass_done: bool) -> PresentLanguage {
    PresentLanguage {
        language: "go".to_string(),
        capabilities: bundled_go_capabilities(),
        semantic_pass_done,
    }
}

/// Rust as it is present in an index whose semantic pass has not landed:
/// the shipped capabilities, and `semantic_pass_done = false`.
///
/// It read a hand-written capability literal until GM-290, because the
/// manifest it was modelling did not exist yet - it was the *hypothetical*
/// future Rust, used to exercise the multi-language naming branch before
/// there was a rust-analyzer tier to produce it. GM-290 shipped exactly
/// those capabilities, so the literal is gone and this reads the manifest
/// like its Go counterpart: a test that models a manifest is a test that
/// can disagree with one.
fn rust_pre_semantic() -> PresentLanguage {
    PresentLanguage {
        language: "rust".to_string(),
        capabilities: bundled_rust_capabilities(),
        semantic_pass_done: false,
    }
}

/// The bundled Rust plugin's own `[plugin.capabilities]`, read off
/// `plugins/rust/plugin.toml` rather than transcribed - so a later edit
/// to that manifest changes what the two `rust_only_*` tests assert
/// instead of quietly disagreeing with it.
fn bundled_rust_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Rust plugin's manifest must be readable")
        .capabilities
}

/// The bundled Python plugin's own `[plugin.capabilities]`, read off
/// `plugins/python/plugin.toml` rather than transcribed - the same
/// `bundled_rust_capabilities`/`bundled_go_capabilities` pattern, so a
/// later edit to that manifest changes what the two `python_only_*` tests
/// assert instead of quietly disagreeing with it. That is not
/// hypothetical: GM-299 landed the pyright tier and flipped
/// `receiver_calls` to `"resolved"`, and the single test that used to read
/// this had to become the pair below - which is the failure mode this
/// helper exists to produce rather than avoid.
fn bundled_python_capabilities() -> Capabilities {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/python");
    crate::daemon::manifest::read_manifest(&dir)
        .expect("the bundled Python plugin's manifest must be readable")
        .capabilities
}

fn typescript_present() -> PresentLanguage {
    PresentLanguage {
        language: "typescript".to_string(),
        capabilities: Capabilities::default(),
        semantic_pass_done: false,
    }
}

/// A future language whose semantic tier resolves receiver calls only
/// through an LSP bridge - the same shape as Rust (design doc's "Paper
/// stress test" table: Roslyn/clangd/pyright/jdtls/kotlin-lsp all listed
/// as "semantic" only, never resolved structurally), gapped until its
/// own semantic pass has run.
fn bridge_semantic_pre_pass(language: &str) -> PresentLanguage {
    PresentLanguage {
        language: language.to_string(),
        capabilities: Capabilities {
            semantic_pass: true,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
        },
        semantic_pass_done: false,
    }
}

/// GM-262's own discrimination requirement, and this module's real
/// permanent regression guard for it - `ORIGINAL_INSTRUCTIONS` above is
/// transcribed independently of `P1`/`P2`/`P3`/`P4_GENERIC`/`P5`,
/// so this assertion fails the moment any of those five drifts from this
/// module's own current baseline (see `ORIGINAL_INSTRUCTIONS`'s own doc
/// comment for what "original" means since GM-394), not just on a change
/// to the receiver-call clause specifically. Proven by mutation, not
/// merely asserted: changing one byte of `P4_GENERIC` (`"by design"` to
/// `"by desigm"`) while leaving `ORIGINAL_INSTRUCTIONS` untouched turns
/// this failing, confirmed by hand while implementing GM-262 and reverted
/// afterward. This assertion is what stands in for repeating that
/// procedure on every future run, so the constant's own doc comment does
/// not.
#[test]
fn ts_only_is_byte_identical_to_the_original_string() {
    let rendered = build(&ts_only());
    assert_eq!(
        rendered, ORIGINAL_INSTRUCTIONS,
        "a TypeScript-only project must read exactly what it did before GM-262"
    );
    // GM-262's own measured baseline was 1804 bytes; GM-394 shortened the
    // second paragraph's closing clause (see `P4_GENERIC`'s own doc
    // comment), so this is the re-measured baseline, not a re-derivation.
    assert_eq!(rendered.len(), 1728, "this module's own current baseline, re-measured at GM-394");
}

#[test]
fn empty_present_falls_back_to_the_original_string() {
    assert_eq!(build(&[]), ORIGINAL_INSTRUCTIONS, "no index yet must read the same as it always has");
}

/// GM-287's own acceptance criterion, and the half of it GM-290 did not
/// change: a Rust-only index whose semantic pass has not landed still
/// lists the receiver-call gap, checked against the shipped manifest
/// rather than a hand-written capability literal
/// (`bundled_rust_capabilities`'s own doc).
///
/// Until GM-290 this ran for `semantic_pass_done` of *both* values,
/// because `plugins/rust/plugin.toml` declared `receiver_calls =
/// "unresolved"` and a pass that could never resolve one could never
/// close the gap either. It now declares `"resolved"`, so the two values
/// have genuinely different answers and each has its own test - the same
/// pair `go_present`'s two tests have had since GM-281.
///
/// This is also the permanent state of a machine with no rust-analyzer:
/// the plugin answers every `semanticPass` with an empty *incomplete*
/// diff, `semanticPassAt` is never set, and the gap stays listed. That is
/// the whole reason the degradation reports incomplete rather than
/// complete.
#[test]
fn rust_only_before_its_semantic_pass_lists_the_receiver_gap() {
    let rendered = build(&[rust_pre_semantic()]);
    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "a single gapped language reads as it always has");
    assert!(
        rendered.contains("The one legitimate reason to grep afterward"),
        "the gap is real until the pass has run"
    );
    assert!(rendered.contains("produces no edge by design"), "one present language is never named");
}

/// The four assertions every "its semantic tier has landed" arm makes,
/// so that the three languages that reach this rendering are checked
/// against one statement of it rather than three transcriptions.
///
/// The two negative assertions are the discrimination, not decoration.
/// `"One real gap"` is what this rendering said until GM-385, and it was
/// measured false in all three languages (see [`P4_STATIC_RECEIVER`]'s
/// own doc for the queries). `"produces no edge"` is the *pre-pass*
/// wording, so its absence is what separates this arm from the
/// `*_before_its_semantic_pass_*` test beside it - without it both arms
/// would pass on a `build` that ignored `semantic_pass_done` entirely.
fn assert_narrowed_receiver_clause(rendered: &str, language: &str) {
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
        !rendered.contains("produces no edge"),
        "{language}: that is the pre-pass wording, and this arm is past it:\n{rendered}"
    );
    assert!(
        rendered.contains("a tool call waits for the walk to finish before answering"),
        "{language}: the wait note survives"
    );
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "{language}: {} bytes exceeds the {INSTRUCTIONS_BYTE_CEILING}-byte ceiling",
        rendered.len()
    );
    println!("{language}-only (pass done) bytes: {}", rendered.len());
}

/// And what the same index reads once rust-analyzer has answered
/// (GM-290): the receiver-call gap *narrows* rather than closing.
///
/// This is the assertion behind "how long does the gap stay listed" - the
/// answer being "until this flips", which core sets from
/// `language_state.semanticPassAt` the moment a *complete* whole-project
/// pass lands. GM-385 changed what happens when it flips, not when.
///
/// Measured on this plugin's own fixture:
/// `find_callers("shapes::Shape::area")` is
/// `{shapes::total_dyn, gaps::measure}` - `&dyn Shape` and `<S: Shape>`
/// both land on the trait's declaration - while
/// `find_callers("shapes::<Circle as Shape>::area")` is **empty**,
/// though either of those two call sites reaches it at run time.
/// `conformance/expect.toml` asserts both as exact sets.
#[test]
fn rust_only_after_its_semantic_pass_narrows_the_receiver_gap_instead_of_closing_it() {
    let mut rust = rust_pre_semantic();
    rust.semantic_pass_done = true;
    let rendered = build(&[rust]);
    assert_narrowed_receiver_clause(&rendered, "rust");
}

/// GM-297's own acceptance criterion 2, and the half GM-299 did not
/// change: a Python-only index whose semantic pass has not landed still
/// lists the receiver-call gap, checked against the shipped manifest
/// rather than a hand-written capability literal
/// (`bundled_python_capabilities`'s own doc).
///
/// Until GM-299 this ran for `semantic_pass_done` of *both* values,
/// because `plugins/python/plugin.toml` declared `receiver_calls =
/// "unresolved"` and a pass that could never resolve one could never
/// close the gap either. It now declares `"resolved"`, so the two values
/// have genuinely different answers and each has its own test - the same
/// transition `rust_only_*` records for GM-290.
///
/// This is also the permanent state of a machine with no pyright: the
/// plugin answers every `semanticPass` with an empty *incomplete* diff,
/// `semanticPassAt` is never set, and the gap stays listed.
#[test]
fn python_only_before_its_semantic_pass_lists_the_receiver_gap() {
    let rendered = build(&[PresentLanguage {
        language: "python".to_string(),
        capabilities: bundled_python_capabilities(),
        semantic_pass_done: false,
    }]);
    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "a single gapped language reads as it always has");
    assert!(
        rendered.contains("The one legitimate reason to grep afterward"),
        "the gap is real until the pass has run"
    );
    assert!(rendered.contains("produces no edge by design"), "one present language is never named");
}

/// And what the same index reads once pyright has answered (GM-299): the
/// receiver-call gap narrows rather than dropping out.
///
/// Worth reading beside `plugins/python/README.md`, which says at length
/// that pyright resolves a receiver call only when it can infer the
/// receiver's type - an unannotated parameter stays unresolved for ever.
/// That list is still the README's, and still cannot fit here.
///
/// What GM-385 moved *into* this rendering is the one part of it that is
/// not Python-specific at all. GM-299 read the whole thing as "a switch,
/// and Python's answer is a paragraph", and the paragraph is only
/// Python's because it enumerates what pyright cannot infer. The
/// narrowing - that what pyright *does* infer is the receiver's
/// annotation, so `obj.describe()` for `obj: Base` is attributed to
/// `Base.describe` however the object was built - is one sentence and is
/// true of `go/types` and rust-analyzer in exactly the same words.
/// Measured: `find_callers("Base.describe")` carries
/// `pkg/callers.py:through_a_base_annotation`, and
/// `find_callers("Deep.describe")` does not, though `Deep` overrides
/// `describe` and `find_implementations("Base")` names it.
#[test]
fn python_only_after_its_semantic_pass_narrows_the_receiver_gap_instead_of_closing_it() {
    let rendered = build(&[PresentLanguage {
        language: "python".to_string(),
        capabilities: bundled_python_capabilities(),
        semantic_pass_done: true,
    }]);
    assert_narrowed_receiver_clause(&rendered, "python");
}

/// What a Go-only index reads once Go's whole-project `semanticPass` has
/// landed - GM-281's own "check what the generated instructions then say"
/// criterion, asserted against the shipped manifest rather than a
/// hand-written capability literal.
///
/// The `go/types` pass resolved every `x.M()` in the index, and GM-385's
/// point is what it resolved them *to*. This is the rendering measured
/// against a real index: on `plugins/go/conformance/project`, with the
/// pass complete, `find_callers("Conn.Close")` answers `results: []`,
/// `hasMore: false` in 240 bytes, while `server/conn.go:CloseAll` closes
/// a `Conn` through a `Closer` value and `find_implementations("Closer")`
/// names `Conn`. The session that returns that empty page used to also
/// say "One real gap" and "do not re-check it with grep".
#[test]
fn go_only_after_its_semantic_pass_narrows_the_receiver_gap_instead_of_closing_it() {
    let rendered = build(&[go_present(true)]);
    assert_narrowed_receiver_clause(&rendered, "go");
}

/// The silence half of the control: TypeScript declares
/// `receiver_calls = "unresolved"` in *both* tiers, so no amount of
/// semantic-pass progress can reach the narrowed rendering, and the
/// sentence about binding to a declared type must never appear for it.
///
/// Asserted with `semantic_pass_done: true` deliberately - the flag that
/// moves the other three languages into the narrowed arm is set here and
/// changes nothing, which is what makes this a control rather than a
/// restatement of `ts_only_is_byte_identical_to_the_original_string`.
/// Measured on a probe fixture carrying three real receiver calls
/// (`g.greet()` on a parameter typed by the interface, by the base
/// class, and on a local of a subclass): every one of the three `greet`
/// declarations answers `find_callers` with an empty set, because this
/// plugin emits no receiver-call edge for either tier to narrow.
#[test]
fn typescript_never_reaches_the_narrowed_rendering_however_its_pass_goes() {
    let mut ts = typescript_present();
    ts.semantic_pass_done = true;

    let rendered = build(&[ts]);

    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "typescript's gap never narrows, because it never resolves");
    assert!(rendered.contains("produces no edge by design"), "the open-gap wording is the right one here");
    assert!(
        !rendered.contains("binds to the receiver's declared"),
        "the narrowed clause must not fire for a language with no receiver-call edges:\n{rendered}"
    );
}

/// The scope decision [`P4_STATIC_RECEIVER`]'s doc argues for, pinned so
/// that it is a decision rather than an omission: a mixed project renders
/// [`p4_named`], which keeps the open-gap clause for the languages that
/// have it and does *not* carry the narrowing for the ones that do not.
///
/// That rendering is unspecific rather than false - it still warns that a
/// method's caller/reference lists can under-report, and never claims a
/// method page is exhaustive. The reason it is left alone is the byte
/// budget measured in that constant's doc: this rendering's worst case is
/// already 1,856 of 1,900 bytes.
#[test]
fn a_mixed_project_keeps_the_named_open_gap_and_does_not_carry_the_narrowing() {
    let mut go_done = go_present(true);
    go_done.semantic_pass_done = true;

    let rendered = build(&[typescript_present(), go_done]);

    assert!(rendered.contains("produces no edge in typescript"), "{rendered}");
    assert!(
        !rendered.contains("binds to the receiver's declared"),
        "scoped out by budget, deliberately - see P4_STATIC_RECEIVER:\n{rendered}"
    );
    assert!(
        rendered.contains("caller/reference lists for methods can under-report"),
        "the warning this rendering does keep is why leaving it alone is not a falsehood:\n{rendered}"
    );
}

/// And before that pass - the cold-start window, and the *permanent*
/// state of a machine with no Go toolchain, where the plugin answers
/// every `semanticPass` with an empty diff and `semanticPassAt` is never
/// set. The gap is real then, so it stays listed: `receiver_calls =
/// "resolved"` is a statement about what the semantic tier *can* do, and
/// `receiver_calls_structural = "unresolved"` is what stops that from
/// being read as a promise about the index as it stands.
#[test]
fn go_only_before_its_semantic_pass_still_lists_the_receiver_gap() {
    let rendered = build(&[go_present(false)]);
    assert!(
        rendered.contains("The one legitimate reason to grep afterward"),
        "the gap is real until the pass has run"
    );
    assert!(rendered.contains("produces no edge by design"), "one present language is never named");
    assert_eq!(rendered, ORIGINAL_INSTRUCTIONS, "a single gapped language reads as it always has");
}

#[test]
fn ts_plus_rust_pre_semantic_names_both() {
    let rendered = build(&[typescript_present(), rust_pre_semantic()]);
    assert!(rendered.contains("produces no edge in rust and typescript"));
    println!("ts+rust-pre-semantic bytes: {}", rendered.len());
}

#[test]
fn ts_plus_rust_after_rusts_semantic_pass_drops_rust_from_the_list() {
    let mut rust_done = rust_pre_semantic();
    rust_done.semantic_pass_done = true;
    let rendered = build(&[typescript_present(), rust_done]);
    assert!(rendered.contains("produces no edge in typescript"));
    assert!(!rendered.contains("rust"), "rust's own gap is closed once its semantic pass has run");
}

/// GM-262's own worst-case scope note, factored out so
/// [`worst_case_every_bundled_and_planned_language_gapped_at_once`] and
/// GM-395 slice 2b's cold-start byte-budget tests below render from the
/// exact same fixture rather than two copies that could drift apart:
/// typescript, go and rust plus the five languages the architecture
/// doc's "Paper stress test" section names (C#, C++, Python, Java,
/// Kotlin), all present and all still gapped at once - a monorepo where
/// nothing's semantic pass has finished yet.
fn worst_case_present() -> Vec<PresentLanguage> {
    vec![
        PresentLanguage {
            language: "typescript".to_string(),
            capabilities: Capabilities::default(),
            semantic_pass_done: false,
        },
        PresentLanguage {
            language: "go".to_string(),
            capabilities: Capabilities {
                semantic_pass: true,
                receiver_calls: ReceiverCallResolution::Resolved,
                receiver_calls_structural: ReceiverCallResolution::Unresolved,
            },
            semantic_pass_done: false,
        },
        bridge_semantic_pre_pass("rust"),
        bridge_semantic_pre_pass("csharp"),
        bridge_semantic_pre_pass("cpp"),
        bridge_semantic_pre_pass("python"),
        bridge_semantic_pre_pass("java"),
        bridge_semantic_pre_pass("kotlin"),
    ]
}

/// This is the case the byte ceiling is actually checked against, not the
/// common one or two-language case.
#[test]
fn worst_case_every_bundled_and_planned_language_gapped_at_once() {
    let rendered = build(&worst_case_present());
    println!("worst-case bytes: {}", rendered.len());
    println!("worst-case text: {rendered}");
    assert!(
        rendered.len() <= INSTRUCTIONS_BYTE_CEILING,
        "worst case must stay under the ceiling (or the fallback must have engaged): {} bytes",
        rendered.len()
    );
}

/// [`format_language_list`] on its own, independent of [`build`]'s byte
/// arithmetic - the three arities the receiver-gap clause can actually
/// need.
#[test]
fn format_language_list_covers_one_two_and_several() {
    assert_eq!(format_language_list(&["go".to_string()]), "go");
    assert_eq!(format_language_list(&["go".to_string(), "rust".to_string()]), "go and rust");
    assert_eq!(
        format_language_list(&["go".to_string(), "rust".to_string(), "typescript".to_string()]),
        "go, rust and typescript"
    );
}

/// The fallback wording itself must (a) exist as a real, shorter
/// alternative and (b) still fit under the ceiling on its own - a
/// fallback that itself blew the budget would defeat the point.
#[test]
fn fallback_wording_fits_under_the_ceiling() {
    let rendered = assemble(&p4_fallback());
    println!("fallback bytes: {}", rendered.len());
    assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    assert!(rendered.contains("semantic layer finishes"));
}

/// [`build`]'s own fallback branch, proven rather than merely present:
/// today's eight-language worst case fits under the ceiling on its own
/// (see [`worst_case_every_bundled_and_planned_language_gapped_at_once`]),
/// so nothing in this module's other tests actually exercises the `else`
/// arm of `build`'s ceiling check. This test forces it with a present
/// list long enough that naming every gapped language would overflow -
/// more languages than the design doc plans for today, standing in for
/// "language 9, 10, ..." rather than a real one - and asserts the
/// rendering that comes back is the fallback, not a truncated name list.
///
/// Sixteen languages sufficed before GM-394; its shorter second clause
/// (see [`P4_GENERIC`]'s own doc comment) freed up enough headroom that
/// sixteen no longer overflows [`INSTRUCTIONS_BYTE_CEILING`] (measured:
/// 1,836 bytes, under the 1,900 ceiling) - so the fixture below has
/// twenty-eight, re-measured to overflow at 1,924 bytes, still standing in
/// for "more than the design doc plans for" rather than a real count.
#[test]
fn a_present_list_too_long_to_name_falls_back_instead_of_exceeding_the_ceiling() {
    let extra_languages = [
        "typescript",
        "go",
        "rust",
        "csharp",
        "cpp",
        "python",
        "java",
        "kotlin",
        "swift",
        "ruby",
        "scala",
        "haskell",
        "elixir",
        "erlang",
        "dart",
        "lua",
        "clojure",
        "fsharp",
        "ocaml",
        "perl",
        "zig",
        "nim",
        "prolog",
        "fortran",
        "cobol",
        "pascal",
        "delphi",
        "groovy",
    ];
    let present: Vec<PresentLanguage> =
        extra_languages.iter().map(|language| bridge_semantic_pre_pass(language)).collect();

    // Sanity check on the test fixture itself: naming all twenty-eight
    // really would overflow the ceiling, or this test would silently
    // exercise the same branch as the worst-case test above instead of
    // the one it means to.
    let would_be_named =
        assemble(&p4_named(&format_language_list(&languages_with_open_receiver_gap(&present))));
    assert!(
        would_be_named.len() > INSTRUCTIONS_BYTE_CEILING,
        "test fixture must actually overflow the ceiling to exercise the fallback branch: {} bytes",
        would_be_named.len()
    );

    let rendered = build(&present);
    assert_eq!(rendered, assemble(&p4_fallback()), "must render the fallback, not a truncated name list");
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
    let rendered = cold_start(&root, false, &worst_case_present());
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
    let rendered = cold_start(&root, true, &worst_case_present());
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
    let rendered = cold_start(&root, false, &worst_case_present());
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
