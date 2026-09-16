//! Assembles `get_info`'s `with_instructions` string (GM-262) from the
//! languages actually present in this project's index, instead of the
//! TypeScript-shaped constant it used to be.
//!
//! # Why this exists
//!
//! Before multi-language support, the instructions stated as a fixed fact
//! that a receiver call (`x.foo()`) produces no edge - true for every
//! language g-mesh indexed at the time (TypeScript, never). It stopped being
//! a fact once a second language could resolve that gap: Go resolves it from
//! its structural tier alone (`go/types`, no external process to wait for);
//! Rust resolves it only once its semantic pass has run
//! (`language_state.semanticPassAt`, because `rust-analyzer` is a child
//! process the structural tier cannot wait on); TypeScript never resolves it
//! at all (see `daemon::manifest::Capabilities`'s own doc comment for what
//! `receiver_calls`/`receiver_calls_structural` encode per language). A
//! constant sentence can be true for at most one of those, so [`build`]
//! renders it from what is actually discovered and indexed.
//!
//! See the architecture doc (`docs/architecture/multi-language-plugins.md`,
//! "Interfaces > MCP instructions") for the design this implements, and
//! GM-262's own tracker task for the measured byte numbers this module's
//! tests assert.
//!
//! # The byte budget, and how each rendering respects it
//!
//! `mcp::mod::GMeshMcpServer::get_info` documents the ceiling this module
//! enforces: Claude Code truncates `with_instructions` at 2KB, so
//! [`INSTRUCTIONS_BYTE_CEILING`] leaves working margin under that hard cut.
//! Four shapes exist, in increasing cost:
//!
//! 1. **No language has an open gap** ([`P4_NO_GAP`]) - shortest: the
//!    whole receiver-call paragraph drops to one sentence.
//! 2. **Nothing is known yet, or exactly one language has the gap**
//!    ([`P4_GENERIC`]) - the original, un-generated sentence, unchanged byte
//!    for byte. A single present language is never ambiguous about which
//!    language "no edge by design" refers to, so naming it would only spend
//!    bytes repeating what the sentence already means - see [`build`]'s own
//!    comment on this case.
//! 3. **Two or more languages are present and at least one has the gap**
//!    ([`p4_named`]) - names exactly the languages that still have it,
//!    because the moment a second language is in the room, "no edge by
//!    design" unqualified would be a false claim about whichever language
//!    already resolved it.
//! 4. **The named list does not fit** ([`p4_fallback`]) - consideration
//!    3 of GM-262: a generic sentence plus a pointer to check the project's
//!    own language mix, in place of a language list that has grown past the
//!    ceiling. [`build`]'s own doc comment covers why this repo does not yet
//!    have a per-response field to point at instead.
//!
//! Shape 3 is the only one whose cost grows with the number of bundled
//! languages (GM-262 consideration 2), which is exactly why shape 4 exists -
//! the worst case this module's tests assert against is every bundled and
//! planned language present and gapped at once (GM-262's own worst-case
//! scope note), not the common case of one or two.
//!
//! # Language names: manifest ids, not a display-name table
//!
//! [`format_language_list`] renders each language's own `language` id
//! (`"typescript"`, `"go"`, `"rust"`, ...) exactly as `daemon::manifest`
//! and `nodes.language` already spell it, rather than looking it up in a
//! capitalization table (`"TypeScript"`). Three reasons, not just one:
//! - A byte-identical rendering ("typescript" vs "TypeScript") costs
//!   nothing either way, so there is no budget argument for the table.
//! - A table is itself a place to hardcode a language name in core, which
//!   this task's own acceptance criterion rules out - every other value
//!   this module touches (which languages are present, which have the gap)
//!   already comes from data, and a display-name table would be the one
//!   exception.
//! - It needs no `plugin.toml` change (no new `display_name` field) for any
//!   bundled or future plugin to pick up correctly, and the id is already
//!   what every other tool surface (`nodes.language`, a future
//!   `get_file_outline` `language` field) would say for the same language,
//!   so an agent cross-referencing the two never sees two spellings of one
//!   language.
//!
//! # Staleness within one session
//!
//! `get_info` is read once, at MCP session start (`mcp::mod::serve_connection`
//! creates one `GMeshMcpServer` per connection, and `rmcp` calls `get_info`
//! during that session's `initialize`). A language's semantic pass can land
//! *after* that read - `language_state.semanticPassAt` only ever moves from
//! unset to set, never back - so the only possible drift is a session whose
//! cached instructions still name a language as gapped after that gap has
//! actually closed. That is the conservative direction on purpose (the same
//! reasoning `ReceiverCallResolution::default`'s doc comment already gives
//! for the manifest side of this: assuming resolved would silently drop
//! edges a caller was told to expect, assuming unresolved costs nothing but
//! an occasional unnecessary grep) and it is accepted rather than worked
//! around - a per-response field a page could read fresh instead
//! (GM-262 consideration 3's own alternative) is real future work, not
//! built here; see [`build`]'s doc comment for why not now.

use std::collections::HashMap;

use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};

/// Working ceiling this module renders under - see this module's doc comment
/// ("The byte budget") for what sits above it (Claude Code's independent 2KB
/// truncation) and why the margin exists at all. A named constant, not a
/// magic number repeated at each call site, because [`build`]'s fallback
/// decision and this module's own worst-case test both have to agree on
/// exactly the same figure.
pub const INSTRUCTIONS_BYTE_CEILING: usize = 1900;

/// One language present in the index (`storage::schema::
/// present_languages_with_semantic_state`), paired with the two facts
/// [`build`] needs to decide whether its receiver-call gap is still open:
/// its manifest capabilities, and whether its semantic pass has completed at
/// least once project-wide.
///
/// A plain data struct rather than passing three parallel slices - the
/// combination is what every decision in this module is made from, and a
/// caller assembling it (`mcp::mod::GMeshMcpServer::get_info`) already has to
/// zip a DB read with a registry read to produce it, which is exactly the
/// kind of pairing a struct exists to make impossible to get out of order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentLanguage {
    /// The manifest's own `language` id, e.g. `"typescript"` - see this
    /// module's doc comment ("Language names") for why this is rendered
    /// as-is rather than through a display-name lookup.
    pub language: String,
    /// This language's `[plugin.capabilities]`, or
    /// [`Capabilities::default`] if presence in the index outran discovery
    /// (a language the index has files for but whose plugin was since
    /// removed) - the same conservative default the manifest side already
    /// uses for "nothing declared".
    pub capabilities: Capabilities,
    /// Whether `language_state.semanticPassAt` is set for this language -
    /// irrelevant unless `capabilities.receiver_calls_structural` is
    /// [`ReceiverCallResolution::Unresolved`] and `capabilities.receiver_calls`
    /// is [`ReceiverCallResolution::Resolved`] (see
    /// [`has_open_receiver_gap`]): a language whose structural tier alone
    /// already resolves receiver calls, or one that never resolves them at
    /// all, does not need this fact to answer the question.
    pub semantic_pass_done: bool,
}

/// Whether `language`'s receiver-call gap ((1) in the instructions text) is
/// still open, given its capabilities and whether its semantic pass has run.
///
/// Mirrors `daemon::manifest::Capabilities::receiver_calls`'s own doc
/// comment exactly: `receiver_calls_structural = Resolved` settles it on its
/// own (Go: `go/types` runs in-process, no external readiness to wait on -
/// see the architecture doc's Go plugin section), regardless of
/// `semantic_pass_done`. Otherwise the gap is open unless a semantic tier
/// both *can* resolve it (`receiver_calls = Resolved`) and *has*
/// (`semantic_pass_done`) - Rust's shape: `rust-analyzer` is a real gap until
/// its pass actually completes.
fn has_open_receiver_gap(present: &PresentLanguage) -> bool {
    if present.capabilities.receiver_calls_structural == ReceiverCallResolution::Resolved {
        return false;
    }
    !(present.capabilities.receiver_calls == ReceiverCallResolution::Resolved && present.semantic_pass_done)
}

/// Every language in `present` with an open receiver-call gap, sorted by
/// language id - sorted for the same reason
/// `daemon::manifest::semantic_pass_capable_languages` sorts its own output:
/// determinism against a `HashMap`-backed capability lookup, so the rendered
/// language list does not depend on iteration order two otherwise-identical
/// daemons happened to see.
fn languages_with_open_receiver_gap(present: &[PresentLanguage]) -> Vec<String> {
    let mut gapped: Vec<String> =
        present.iter().filter(|p| has_open_receiver_gap(p)).map(|p| p.language.clone()).collect();
    gapped.sort();
    gapped
}

/// Renders `names` as an English list - `"go"`, `"go and rust"`, `"go, rust
/// and typescript"` - the join [`p4_named`] needs to turn a sorted
/// language-id list into one clause. No Oxford comma before the final `and`:
/// it reads fine without one and costs a byte per rendering that
/// [`INSTRUCTIONS_BYTE_CEILING`] has no reason to spend.
fn format_language_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = names.split_last().expect("non-empty per the two arms above");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

const P1: &str = "Structural code-graph queries over this project's index. Prefer these over \
     grepping when you need definitions, references, call edges or imports.";

/// Paired with [`P2_ONE_GAP`]: both describe the paragraph right after this
/// one ("Two real gaps" / "One real gap"), so the count here has to agree
/// with whichever `P4_*` rendering [`build`] pairs it with.
const P2_TWO_GAPS: &str = "A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
     (excludes other same-named declarations' call sites, same guarantee either \
     way), is already resolved per call site to that exact declaration - do not \
     re-check it with grep as a routine habit. Only fall back to grep for one of \
     the two specific gaps below, never as a general double-check.";

/// [`P2_TWO_GAPS`], worded for exactly one remaining gap - paired only with
/// [`P4_NO_GAP`], where the receiver-call gap has closed for
/// every present language and only the "still building" gap is left below.
const P2_ONE_GAP: &str = "A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
     (excludes other same-named declarations' call sites, same guarantee either \
     way), is already resolved per call site to that exact declaration - do not \
     re-check it with grep as a routine habit. Only fall back to grep for the \
     one specific gap below, never as a general double-check.";

const P3: &str = "`resolved: false` marks the one thing the indexer could not settle alone: an \
     edge whose target is in *another* file, where whether that file exports the \
     name isn't knowable from the usage alone. Every same-file edge is \
     `resolved: true` - never a reason to grep. find_references/find_callers/\
     find_callees/find_implementations also carry a response-level \
     `allUnresolved: true` when *every* row in a non-empty page is unconfirmed - \
     the page otherwise looks complete (`hasMore: false`, plausible results), so \
     check this field, not just individual rows. Never set on an empty page.";

/// The original, un-generated receiver-call paragraph - byte-for-byte what
/// `get_info` always said, before this module existed. [`build`] reaches for
/// this whenever naming a language would add nothing a reader does not
/// already get for free: nothing is known about the project yet, or exactly
/// one language is present (see [`build`]'s own doc comment for why that
/// case is never ambiguous), or every present language happens to still have
/// the gap in a project where that is not this module's chosen case to name
/// (see [`build`] for the exact branching).
const P4_GENERIC: &str = "Two real gaps - the only legitimate reasons to grep afterward: (1) a method \
     call through a variable receiver (`x.foo()`) produces no edge by design, so \
     caller/reference lists for methods can under-report; bare function calls and \
     this/super/qualified-type calls have no such gap, and a `hasMore: false` \
     page for those is exhaustive. (2) On a project's first index, or a re-index \
     after an upgrade, every tool errors with a \"still building\" message - that \
     is temporary, retry after a few seconds rather than concluding the symbol \
     does not exist.";

/// Every present language has resolved its receiver-call gap: the whole
/// clause (1) drops out, "Two real gaps" becomes "One", and what is left is
/// only the still-building gap, renumbered out of its `(2)` since there is no
/// longer a `(1)` beside it.
const P4_NO_GAP: &str = "One real gap - the only legitimate reason to grep afterward: on a \
     project's first index, or a re-index after an upgrade, every tool errors \
     with a \"still building\" message - that is temporary, retry after a few \
     seconds rather than concluding the symbol does not exist.";

/// [`P4_GENERIC`] with `"by design"` replaced by `"in {list}"` - the only
/// difference, so that everything this clause says about bare/this/super/
/// qualified-type calls having no such gap, and a `hasMore: false` page being
/// exhaustive, still reads as universally true (GM-262 consideration 7): it
/// was never phrased as TypeScript-specific syntax in the first place, so
/// naming which languages the *first* half applies to needs no matching
/// per-language rewrite of the second half. A per-language equivalent (Rust's
/// `Self::f()`/path calls, Go's package-qualified calls) was considered and
/// rejected on the same two grounds GM-262 asks to weigh: it would hardcode
/// per-language syntax in core, which this task's own acceptance criterion
/// rules out, and it would cost bytes exactly where consideration 2 says
/// there are none to spend once several languages are already named.
fn p4_named(list: &str) -> String {
    format!(
        "Two real gaps - the only legitimate reasons to grep afterward: (1) a method \
         call through a variable receiver (`x.foo()`) produces no edge in {list}, so \
         caller/reference lists for methods can under-report; bare function calls and \
         this/super/qualified-type calls have no such gap, and a `hasMore: false` \
         page for those is exhaustive. (2) On a project's first index, or a re-index \
         after an upgrade, every tool errors with a \"still building\" message - that \
         is temporary, retry after a few seconds rather than concluding the symbol \
         does not exist."
    )
}

/// GM-262 consideration 3's fallback: when the named list does not fit under
/// [`INSTRUCTIONS_BYTE_CEILING`], replace it with one generic sentence plus a
/// pointer to check the project's own language mix, rather than truncating a
/// language list mid-name.
///
/// The doc comment's two candidate pointers - a `get_file_outline` `language`
/// field, or a per-row `receiverCallsResolved` field on `find_callers`/
/// `find_references` - are not built by this task: neither exists on any
/// tool's response today, and adding one is a schema-and-query-builder change
/// to a different tool surface, not an instructions-text change. Pointing at
/// a field that does not exist would be worse than the generic sentence
/// alone, so this fallback names no field and says only what is already
/// true - check the project's own languages before trusting a method page as
/// exhaustive - leaving the field itself for whichever task actually adds it.
///
/// Drops the "bare function calls ... have no such gap" reassurance
/// [`p4_named`] keeps (GM-262 consideration 7's second half) - defensible
/// only *because* this tier is the one already past the point of naming
/// individual languages: a caller reading "check which" already knows not to
/// trust the receiver-call answer uniformly, so the extra reassurance about
/// which *other* calls are always safe buys less here than the bytes cost.
///
/// Deliberately shorter than [`p4_named`] ever gets for today's eight
/// languages, not merely under the ceiling by a few bytes - measured by this
/// module's own tests: 481 bytes against the eight-language sentence's 594.
/// The eight-language case does not actually need this fallback yet (see
/// [`build`]'s worst-case test, which stays under the ceiling without it), so
/// the margin here is headroom for whichever *later* language makes it not
/// fit, not headroom this fallback is spending on today's set.
fn p4_fallback() -> String {
    "Two real gaps - the only legitimate reasons to grep afterward: (1) a method call \
     through a variable receiver (`x.foo()`) produces no edge in some of this project's \
     languages until their semantic layer finishes - check which before trusting a \
     method's page as exhaustive. (2) On a project's first index, or a re-index after an \
     upgrade, every tool errors with a \"still building\" message - that is temporary, \
     retry after a few seconds rather than concluding the symbol does not exist."
        .to_string()
}

const P5: &str = "Efficient usage: pass `symbol_name` directly to the four tools above instead \
     of calling find_definition first, and raise `limit` for symbols with many \
     results instead of paging.";

/// Joins the five paragraphs the same way the original single string literal
/// always did: blank-line separated, nothing trimmed or reflowed - so a
/// caller only ever varies `p2`/`p4`, never how they meet the fixed
/// paragraphs around them.
fn assemble(p2: &str, p4: &str) -> String {
    [P1, p2, P3, p4, P5].join("\n\n")
}

/// Builds `get_info`'s `with_instructions` string for this session, from
/// every language [`PresentLanguage`] names.
///
/// # The four cases, in the order they are checked
///
/// 1. **`present` is empty** - no index yet (a fresh project whose cold-start
///    walk has not committed a single `File` node), or the DB read that
///    would have populated it failed (see
///    `mcp::mod::GMeshMcpServer::get_info`'s call site for the
///    capabilities-only fallback that covers the second case *before*
///    calling this function, by synthesizing `present` from every
///    discovered manifest with `semantic_pass_done: false` instead of
///    calling with an empty slice). Either way there is nothing to name yet,
///    and GM-262's own scope note picks the conservative, already-familiar
///    default: render exactly what `get_info` always said, so a project
///    mid-cold-start reads no differently than it read before this module
///    existed.
/// 2. **No present language has an open gap** - [`P4_NO_GAP`]: the
///    shortest rendering, and the only one whose paragraph count actually
///    drops from two to one.
/// 3. **Exactly one language is present** (and it has the gap, since case 2
///    already handled "it doesn't") - the original wording, unchanged. A
///    single present language is never ambiguous about which language "no
///    edge by design" describes, so naming it would spend bytes to restate
///    what the unqualified sentence already means; this is also what keeps
///    a TypeScript-only project's instructions byte-for-byte identical to
///    what `get_info` returned before this task (this module's own
///    `ts_only_is_byte_identical_to_the_original_string` test).
/// 4. **Two or more languages are present, and at least one has an open
///    gap** - [`p4_named`], which names every gapped language (whether
///    that is some of the present languages or, per GM-262's own worst-case
///    scope note, every one of them at once) and falls back to
///    [`p4_fallback`] if the result would exceed
///    [`INSTRUCTIONS_BYTE_CEILING`].
///
/// Case 4 naming *every* gapped language even when that happens to be all of
/// them (rather than falling back to case 3's unnamed wording, which would
/// also be textually true) is a deliberate choice, not an oversight: a
/// reader who has seen this sentence once with one language, and now sees it
/// with several, should not have to notice a *change in phrasing* to learn
/// that "in X and Y" no longer means "every language" the way the unnamed
/// form implicitly did for a one-language project - naming stays consistent
/// with case 4 the moment ambiguity is possible at all, at the cost this
/// module's fallback exists to bound.
pub fn build(present: &[PresentLanguage]) -> String {
    if present.is_empty() {
        return assemble(P2_TWO_GAPS, P4_GENERIC);
    }

    let gapped = languages_with_open_receiver_gap(present);
    if gapped.is_empty() {
        return assemble(P2_ONE_GAP, P4_NO_GAP);
    }
    if present.len() == 1 {
        return assemble(P2_TWO_GAPS, P4_GENERIC);
    }

    let list = format_language_list(&gapped);
    let named = assemble(P2_TWO_GAPS, &p4_named(&list));
    if named.len() <= INSTRUCTIONS_BYTE_CEILING {
        named
    } else {
        assemble(P2_TWO_GAPS, &p4_fallback())
    }
}

/// Turns a registry's capability map and the index's present-language/
/// semantic-state pairs into the [`PresentLanguage`] list [`build`] wants -
/// the zip `mcp::mod::GMeshMcpServer::get_info` would otherwise repeat inline
/// at its one call site, plus the capabilities-only fallback
/// (GM-262's own scope note: "if the index isn't open yet, fall back to
/// capabilities only") that call site also needs when the DB read fails
/// outright rather than merely returning zero rows.
///
/// `capabilities` is a `HashMap` (from `PluginRegistry::
/// receiver_call_capabilities`) rather than a slice: the lookup here is by
/// language id per present row, and a present language with no matching
/// manifest (the index has files for a language whose plugin was since
/// removed) falls back to [`Capabilities::default`] - the same "says
/// nothing, assumed to do the least" rule the manifest side already applies
/// to a missing `[plugin.capabilities]` table.
pub fn present_languages(
    present: Vec<(String, bool)>,
    capabilities: &HashMap<String, Capabilities>,
) -> Vec<PresentLanguage> {
    present
        .into_iter()
        .map(|(language, semantic_pass_done)| {
            let capabilities = capabilities.get(&language).copied().unwrap_or_default();
            PresentLanguage { language, capabilities, semantic_pass_done }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact string `get_info` returned before GM-262, byte for byte -
    /// copied from the git history of `mcp::mod::GMeshMcpServer::get_info`
    /// rather than re-derived, so a transcription slip in this module's own
    /// paragraph constants cannot accidentally agree with itself. This is
    /// the fixture [`ts_only_is_byte_identical_to_the_original_string`]
    /// checks [`build`] against.
    const ORIGINAL_INSTRUCTIONS: &str =
        "Structural code-graph queries over this project's index. Prefer these over \
grepping when you need definitions, references, call edges or imports.\n\n\
A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
(excludes other same-named declarations' call sites, same guarantee either \
way), is already resolved per call site to that exact declaration - do not \
re-check it with grep as a routine habit. Only fall back to grep for one of \
the two specific gaps below, never as a general double-check.\n\n\
`resolved: false` marks the one thing the indexer could not settle alone: an \
edge whose target is in *another* file, where whether that file exports the \
name isn't knowable from the usage alone. Every same-file edge is \
`resolved: true` - never a reason to grep. find_references/find_callers/\
find_callees/find_implementations also carry a response-level \
`allUnresolved: true` when *every* row in a non-empty page is unconfirmed - \
the page otherwise looks complete (`hasMore: false`, plausible results), so \
check this field, not just individual rows. Never set on an empty page.\n\n\
Two real gaps - the only legitimate reasons to grep afterward: (1) a method \
call through a variable receiver (`x.foo()`) produces no edge by design, so \
caller/reference lists for methods can under-report; bare function calls and \
this/super/qualified-type calls have no such gap, and a `hasMore: false` \
page for those is exhaustive. (2) On a project's first index, or a re-index \
after an upgrade, every tool errors with a \"still building\" message - that \
is temporary, retry after a few seconds rather than concluding the symbol \
does not exist.\n\n\
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

    fn go_resolved() -> PresentLanguage {
        PresentLanguage {
            language: "go".to_string(),
            capabilities: Capabilities {
                semantic_pass: true,
                receiver_calls: ReceiverCallResolution::Resolved,
                receiver_calls_structural: ReceiverCallResolution::Resolved,
            },
            semantic_pass_done: true,
        }
    }

    fn rust_pre_semantic() -> PresentLanguage {
        PresentLanguage {
            language: "rust".to_string(),
            capabilities: Capabilities {
                semantic_pass: true,
                receiver_calls: ReceiverCallResolution::Resolved,
                receiver_calls_structural: ReceiverCallResolution::Unresolved,
            },
            semantic_pass_done: false,
        }
    }

    /// The bundled Rust plugin's own `[plugin.capabilities]`, read off
    /// `plugins/rust/plugin.toml` rather than transcribed - so a later edit
    /// to that manifest changes what [`rust_only_lists_the_receiver_gap_with_no_semantic_tier_yet`]
    /// asserts instead of quietly disagreeing with it. Unlike
    /// [`rust_pre_semantic`] above (a *hypothetical* future manifest, used to
    /// exercise the multi-language naming branch before GM-290 exists), this
    /// reads the manifest as GM-286 actually shipped it.
    fn bundled_rust_capabilities() -> Capabilities {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust");
        crate::daemon::manifest::read_manifest(&dir)
            .expect("the bundled Rust plugin's manifest must be readable")
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
    /// transcribed independently of `P1`/`P2_TWO_GAPS`/`P3`/`P4_GENERIC`/`P5`,
    /// so this assertion fails the moment any of those five drifts from what
    /// `get_info` said before GM-262, not just on a change to the receiver-
    /// call clause specifically. Proven by mutation, not merely asserted:
    /// changing one byte of `P4_GENERIC` (`"by design"` to `"by desigm"`)
    /// while leaving `ORIGINAL_INSTRUCTIONS` untouched turns this failing,
    /// confirmed by hand while implementing this task and reverted afterward.
    /// This assertion is what stands in for repeating that procedure on
    /// every future run, so the constant's own doc comment does not.
    #[test]
    fn ts_only_is_byte_identical_to_the_original_string() {
        let rendered = build(&ts_only());
        assert_eq!(
            rendered, ORIGINAL_INSTRUCTIONS,
            "a TypeScript-only project must read exactly what it did before GM-262"
        );
        assert_eq!(rendered.len(), 1804, "GM-262's own measured baseline");
    }

    #[test]
    fn empty_present_falls_back_to_the_original_string() {
        assert_eq!(build(&[]), ORIGINAL_INSTRUCTIONS, "no index yet must read the same as it always has");
    }

    /// GM-287's own acceptance criterion: a Rust-only index still lists the
    /// receiver-call gap, checked against the shipped manifest rather than a
    /// hand-written capability literal (`bundled_rust_capabilities`'s own
    /// doc). `plugins/rust/plugin.toml` declares both `receiver_calls` and
    /// `receiver_calls_structural` as `"unresolved"` - there is no semantic
    /// tier at all yet (GM-290, R4 of the design doc's rollout), so
    /// `semantic_pass_done` cannot close the gap either way, unlike Go's
    /// `go_present`/`bundled_go_capabilities` pair on the sibling release
    /// branch this module's own git history shows once GM-281/GM-282 land
    /// here.
    #[test]
    fn rust_only_lists_the_receiver_gap_with_no_semantic_tier_yet() {
        for semantic_pass_done in [false, true] {
            let rendered = build(&[PresentLanguage {
                language: "rust".to_string(),
                capabilities: bundled_rust_capabilities(),
                semantic_pass_done,
            }]);
            assert_eq!(
                rendered, ORIGINAL_INSTRUCTIONS,
                "a single gapped language reads as it always has, semantic_pass_done = {semantic_pass_done}"
            );
            assert!(rendered.contains("produces no edge by design"));
        }
    }

    #[test]
    fn go_only_resolved_omits_the_receiver_gap_entirely() {
        let rendered = build(&[go_resolved()]);
        assert!(
            rendered.contains("One real gap"),
            "go/types resolves receiver calls with no external process to wait on"
        );
        assert!(!rendered.contains("Two real gaps"));
        assert!(!rendered.contains("variable receiver"));
        assert!(rendered.contains("still building"), "the second gap must survive renumbering");
        println!("go-only bytes: {}", rendered.len());
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

    /// GM-262's own worst-case scope note: typescript, go and rust plus the
    /// five languages the architecture doc's "Paper stress test" section
    /// names (C#, C++, Python, Java, Kotlin), all present and all still
    /// gapped at once - a monorepo where nothing's semantic pass has
    /// finished yet. This is the case the byte ceiling is actually checked
    /// against, not the common one or two-language case.
    #[test]
    fn worst_case_every_bundled_and_planned_language_gapped_at_once() {
        let present = vec![
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
        ];
        let rendered = build(&present);
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
        let rendered = assemble(P2_TWO_GAPS, &p4_fallback());
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
        ];
        let present: Vec<PresentLanguage> =
            extra_languages.iter().map(|language| bridge_semantic_pre_pass(language)).collect();

        // Sanity check on the test fixture itself: naming all sixteen really
        // would overflow the ceiling, or this test would silently exercise
        // the same branch as the worst-case test above instead of the one
        // it means to.
        let would_be_named = assemble(
            P2_TWO_GAPS,
            &p4_named(&format_language_list(&languages_with_open_receiver_gap(&present))),
        );
        assert!(
            would_be_named.len() > INSTRUCTIONS_BYTE_CEILING,
            "test fixture must actually overflow the ceiling to exercise the fallback branch: {} bytes",
            would_be_named.len()
        );

        let rendered = build(&present);
        assert_eq!(
            rendered,
            assemble(P2_TWO_GAPS, &p4_fallback()),
            "must render the fallback, not a truncated name list"
        );
        assert!(rendered.len() <= INSTRUCTIONS_BYTE_CEILING);
    }
}
