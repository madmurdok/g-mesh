//! Assembles `get_info`'s `with_instructions` string (GM-262) from the
//! languages actually present in this project's index, instead of the
//! TypeScript-shaped constant it used to be.
//!
//! # Why this exists
//!
//! Before multi-language support, the instructions stated as a fixed fact
//! that a receiver call (`x.foo()`) produces no edge - true for every
//! language g-mesh indexed at the time (TypeScript, never). It stopped being
//! a fact once a second language could resolve that gap: Go and Rust both
//! resolve it in their *semantic* tier, once that language's whole-project
//! pass has completed (`language_state.semanticPassAt`) and not before - Go's
//! go/parser tier records an open site and emits nothing (GM-281), and Rust's
//! tree-sitter tier does the same; TypeScript never resolves it at all (see
//! `daemon::manifest::Capabilities`'s own doc comment for what
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
//! 1. **Every present language resolves receiver calls**
//!    ([`P4_STATIC_RECEIVER`]) - clause (1) narrows to what that resolution
//!    actually binds to (the receiver's declared type, never its run-time
//!    one) instead of dropping out. GM-385 measured why it must not drop
//!    out; that constant's own doc has the arms.
//! 2. **Nothing is known yet, or exactly one language has the gap**
//!    ([`P4_GENERIC`]) - the un-generated sentence: no language list is
//!    spliced in. Byte-for-byte what `get_info` said before this module
//!    existed until GM-394 rewrote its second clause (see that constant's own
//!    doc). A single present language is never ambiguous about which
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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::daemon::candidates::Detection;
use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};

/// Working ceiling this module renders under - see this module's doc comment
/// ("The byte budget") for what sits above it (Claude Code's independent 2KB
/// truncation) and why the margin exists at all. A named constant, not a
/// magic number repeated at each call site, because [`build`]'s fallback
/// decision and this module's own worst-case test both have to agree on
/// exactly the same figure.
pub const INSTRUCTIONS_BYTE_CEILING: usize = 1900;

/// Prefixed to a session's instructions while the project owes its cold
/// start - `Phase::Unindexed` or `Phase::Walking`
/// (`mcp::mod::GMeshMcpServer::instructions`, GM-394 and GM-395's D12 in
/// `docs/architecture/lazy-indexing.md`) - the one fact that is true only for
/// *this* moment, for *this* session: the wait itself, and what a caller
/// should do about it, is already stated in every [`build`] rendering's own
/// final paragraph (see [`P4_GENERIC`] and its siblings) - text that is
/// handed to *every* session, whether or not a walk happens to be running
/// right now. This line exists only to say that the walk this session's own
/// paragraph describes in the abstract is actually happening (or about to,
/// once the first tool call asks), right now, so a caller does not have to
/// notice a slow first call before connecting the two.
///
/// Before GM-395 slice 2b this was a fixed constant (`INDEXING_NOTE`) with no
/// project identity in it at all - true when a session could only ever be
/// talking to the one project a daemon indexed at startup. Lazy activation
/// means a caller can now see this state for a project that has not been
/// touched yet, not just one already mid-walk, and [`cold_start`] names the
/// project's own root (A2 in the architecture doc) so a caller juggling more
/// than one g-mesh session can tell which is which. [`cold_start_line_fallback`]
/// is what a very long root falls back to, so this line can never itself be
/// the reason the whole rendering exceeds [`INSTRUCTIONS_BYTE_CEILING`].
fn cold_start_line(root: &Path, walking: bool) -> String {
    if walking {
        format!(
            "Index root: {}. Being built now - the first tool call waits for it to finish before answering.",
            root.display()
        )
    } else {
        format!(
            "Index root: {}. Not indexed yet - the first tool call builds it (structural first; semantic \
             search after) and waits for it.",
            root.display()
        )
    }
}

/// [`cold_start_line`] without the root - what [`cold_start`] falls back to
/// when the root makes the rendered line too long for
/// [`INSTRUCTIONS_BYTE_CEILING`] to afford (D12's own fallback rule, the same
/// shape [`p4_fallback`] uses for a language list that does not fit).
fn cold_start_line_fallback(walking: bool) -> &'static str {
    if walking {
        "Being built now - the first tool call waits for it to finish before answering."
    } else {
        "Not indexed yet - the first tool call builds it (structural first; semantic search after) and \
         waits for it."
    }
}

/// `get_info`'s `with_instructions` string for a session that connects while
/// the project still owes its cold start (`Phase::Unindexed` or
/// `Phase::Walking`) - [`cold_start_line`] (or its no-path fallback) stacked
/// on top of the ordinary [`build`] rendering for `present`, exactly the way
/// `INDEXING_NOTE` used to sit on top of it before GM-395 slice 2b.
///
/// `present` is capabilities-only (every discovered language,
/// `semantic_pass_done: false`) in both cases - `mcp::mod::
/// GMeshMcpServer::instructions` never takes the connection lock to build a
/// real one while either phase holds, for the same GM-394 reason it never did
/// for `Walking` alone before this slice: the walk (or the batch commit that
/// follows it) may be holding that lock for as long as its embedding
/// inference takes, and a caller mid-handshake cannot be made to wait on it.
pub fn cold_start(root: &Path, walking: bool, present: &[PresentLanguage]) -> String {
    let built = build(present);
    let with_path = format!("{}\n\n{built}", cold_start_line(root, walking));
    if with_path.len() <= INSTRUCTIONS_BYTE_CEILING {
        with_path
    } else {
        format!("{}\n\n{built}", cold_start_line_fallback(walking))
    }
}

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
/// own - a plugin whose *structural* tier already emits the edge has no gap
/// to wait out - regardless of `semantic_pass_done`. Otherwise the gap is
/// open unless a semantic tier both *can* resolve it (`receiver_calls =
/// Resolved`) and *has* (`semantic_pass_done`). That second shape is what
/// both Go and Rust actually ship: Go's `go/types` pass runs in-process and
/// is quick, Rust's waits on a `rust-analyzer` child, but until either one
/// has completed once, the edges simply are not in the index yet and saying
/// otherwise would promise a caller rows it cannot find.
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

/// The only P2 there is, and "two" is not a number [`build`] ever varied -
/// until GM-394. Before it, this constant was named `P2_TWO_GAPS`, because
/// every `P4_*` rendering stated exactly two gaps (the receiver-call one and
/// a second: "on a project's first index...every tool errors with a 'still
/// building' message"), and its own "one of the two specific gaps below"
/// matched that count word for word.
///
/// GM-394 removed the second gap by removing what made it a *gap*: a tool
/// call issued while the index is being built no longer errors at all - it
/// waits, unconditionally, for the walk to finish and answers in full (see
/// `mcp::mod::GMeshMcpServer::still_indexing`'s own doc comment) - so there is
/// nothing left to grep around there. What every `P4_*` rendering says about
/// it moved out of the "reasons to grep" enumeration entirely, into its own
/// sentence describing the wait; this constant's own count, and its name,
/// followed it down to one.
///
/// Not renamed to `P2_ONE_GAP`: that name already had a meaning here, for a
/// *different* one-gap state GM-385 considered and rejected - every present
/// language's receiver-call resolution fully closed rather than merely
/// narrowing, which no bundled plugin can actually reach (see
/// [`P4_STATIC_RECEIVER`]'s own doc). Reusing that name for this constant,
/// which describes an unrelated state, would read as if that closed-gap case
/// had come back. Plain `P2` says only what is still true unconditionally:
/// this is the one and only P2.
const P2: &str = "A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
     (excludes other same-named declarations' call sites, same guarantee either \
     way), is already resolved per call site to that exact declaration - do not \
     re-check it with grep as a routine habit. Only fall back to grep for the one \
     specific gap below, never as a general double-check.";

const P3: &str = "`resolved: false` marks the one thing the indexer could not settle alone: an \
     edge whose target is in *another* file, where whether that file exports the \
     name isn't knowable from the usage alone. Every same-file edge is \
     `resolved: true` - never a reason to grep. find_references/find_callers/\
     find_callees/find_implementations also carry a response-level \
     `allUnresolved: true` when *every* row in a non-empty page is unconfirmed - \
     the page otherwise looks complete (`hasMore: false`, plausible results), so \
     check this field, not just individual rows. Never set on an empty page.";

/// The receiver-call paragraph for the common cases (nothing known yet, or
/// exactly one present language) - un-generated, in the sense that no
/// language list is spliced into it. [`build`] reaches for this whenever
/// naming a language would add nothing a reader does not already get for
/// free: nothing is known about the project yet, or exactly one language is
/// present (see [`build`]'s own doc comment for why that case is never
/// ambiguous), or every present language happens to still have the gap in a
/// project where that is not this module's chosen case to name (see
/// [`build`] for the exact branching).
///
/// # GM-394: the second clause is a wait, not a gap
///
/// Until GM-394 this was byte-for-byte what `get_info` always said, and it
/// numbered two "legitimate reasons to grep afterward": the receiver-call
/// clause below, and "on a project's first index...every tool errors with a
/// 'still building' message - retry". That second clause stopped being true
/// the moment `mcp::mod::GMeshMcpServer::still_indexing` stopped erroring at
/// all: a tool call issued while the index is being built now waits,
/// unconditionally, for the walk to finish and answers in full - so grepping
/// around it was never the right move, and telling an agent to "retry" a
/// call that was never going to fail is actively misleading about what a slow
/// first call means. The sentence moved out of the "reasons to grep"
/// enumeration entirely, into a plain statement of the wait, and this
/// constant's own count followed it down to one - see [`P2`]'s doc comment
/// for the equivalent change one paragraph up.
const P4_GENERIC: &str = "The one legitimate reason to grep afterward: a method call through a \
     variable receiver (`x.foo()`) produces no edge by design, so caller/reference \
     lists for methods can under-report; bare function calls and this/super/qualified-type \
     calls have no such gap, and a `hasMore: false` page for those is exhaustive. On a \
     project's first index, or a re-index after an upgrade, a tool call waits for the walk \
     to finish before answering - slow, not wrong; do not abandon it for grep.";

/// Every present language *resolves* receiver calls - so clause (1) states
/// what "resolved" actually bought, instead of dropping out.
///
/// # What GM-385 measured, and why the gap narrows rather than closes
///
/// A semantic tier resolves `x.foo()` against the receiver's **declared or
/// inferred** type, because that is the only type a static analysis has. It
/// is therefore answering a narrower question than the caller asked, and
/// until GM-385 nothing said so anywhere a caller reads. Measured through
/// the real MCP handlers on each bundled fixture, the three tiers that
/// resolve receiver calls at all agree exactly:
///
/// - **go** (`go/types`) - `find_callers("Closer.Close")` is
///   `{server/conn.go:CloseAll}`; a dispatch through an interface value
///   lands on the *interface method's* declaration.
/// - **rust** (`rust-analyzer`) - `find_callers("shapes::Shape::area")` is
///   `{shapes::total_dyn, gaps::measure}`; `&dyn Shape` and `<S: Shape>`
///   both land on the *trait's* declaration.
/// - **python** (`pyright`) - `find_callers("Base.describe")` contains
///   `pkg/callers.py:through_a_base_annotation`, which is `obj.describe()`
///   for `obj: Base`; the annotation decides, so it lands on the *base's*
///   declaration.
///
/// `typescript` is the fourth and behaves differently: it declares
/// `receiver_calls = "unresolved"` in both tiers, emits no receiver-call
/// edge at all, and therefore keeps clause (1) in its original
/// [`P4_GENERIC`] wording for ever. Nothing here applies to it.
///
/// The half that makes this worth a paragraph is the *other* end of the
/// same edge. Because the call site was attributed to the base, the
/// override's own page loses it - and loses it silently, since a page that
/// never received a row looks exactly like a symbol nobody calls. Measured
/// on the Go fixture, with `go/types` having completed a whole-project
/// pass, `find_callers` on `Conn.Close` answers in 240 bytes with
/// `results: []`, `hasMore: false` and `allUnresolved: false`. `CloseAll`
/// closes a `Conn` whenever the slice it walks holds one, and
/// `find_implementations("Closer")` names `Conn` as an implementor in that
/// same index. Before GM-385 the session that returned that empty page also
/// said "One real gap" and "do not re-check it with grep", which is the
/// exact claim this constant exists to withdraw.
///
/// # Why a pointer and never a number (GM-382's rule, one layer out)
///
/// `mcp::provenance`'s module doc refuses to estimate what an absent tier
/// would have found, because a manufactured number a caller acts on is
/// worse than silence. The same refusal applies here, and the temptation is
/// sharper because a count looks computable: the index really does know how
/// many implementors a type has, so a response *could* say "3 other types
/// implement this". It must not. That number counts implementors, not the
/// call sites this page is missing - a caller who reads it on
/// `find_callers(Conn.Close)` acts on "3 more callers", a quantity nothing
/// computed - and the true count is unknowable by construction, since which
/// override runs is a run-time fact.
///
/// So this sentence says only what is knowable, and spends the bytes a
/// count would have taken on something strictly better: *where the missing
/// calls are*. They are on the base's page, exactly, and
/// `find_implementations` is the edge that gets there. A caller who follows
/// it reads real rows instead of acting on an estimate.
///
/// # Why one sentence here rather than a field on the response
///
/// Both per-answer shapes were measured, and both lost.
///
/// **Per-row** cannot carry it at all. The disclosure is about call sites
/// *absent* from the page, and no property of a row that is present can
/// state one. The 240-byte answer above has zero rows, so a per-row marker
/// would be missing from precisely the page that needs it most. Bytes agree
/// independently: on excalidraw's `pointFrom` at `limit: 200` - 51 rows,
/// the established worst case - a 30-byte per-row marker costs 1,530 bytes
/// against a 44-byte response-level block, 34.8x for one fact about the
/// call. That is `mcp::provenance`'s own argument against `edges.source`,
/// reaching the same answer from a second direction.
///
/// **Per-response** is affordable but fires on *every* answer in go, rust
/// and python once their pass lands, which is `mcp::provenance`'s first
/// rule ("a disclosure that fires everywhere is noise") straight through.
/// Firing it only on the narrow interesting case - an anchor that overrides
/// a supertype's member - is the thing the index cannot do: `nodes.
/// container` holds a package/module key, `SUPERTYPE_OF` joins *types* and
/// never members, `DEFINES` runs file-to-member and container-to-member but
/// never type-to-member, and no `OVERRIDES` edge exists. Core would have to
/// recover a member's owning type by parsing its `qualifiedName` in four
/// different grammars (`Server.Close`, `Base.describe`,
/// `shapes::<Square as Shape>::area`, `Greeter#greet`) - a per-language
/// name table in core, which this module's own doc ("Language names")
/// already rules out for a far smaller thing.
///
/// What is left is a property of the *tier*: constant for a language and a
/// session, which is exactly what this module renders.
///
/// # Why the other renderings are left alone
///
/// [`P4_GENERIC`] and [`p4_named`] both keep clause (1), and both already
/// warn that a method's caller/reference lists can under-report. They are
/// less specific about *why* for a language whose tier has landed, but
/// neither tells a caller a method page is exhaustive, so neither is false.
/// This branch is the only one that withdrew the warning, and the only one
/// fixed.
///
/// That scope is also what the byte budget affords, measured rather than
/// assumed. Before GM-394 shortened the second clause across every `P4_*`
/// constant, [`p4_named`]'s eight-language worst case rendered at 1,856 bytes
/// against [`INSTRUCTIONS_BYTE_CEILING`]'s 1,900 - only 44 bytes of headroom,
/// not enough to also add this constant's narrowing detail once per named
/// language. GM-394's rewrite (the "still building" clause replaced by a
/// short, no-longer-per-language wait note) measured the same eight-language
/// case at 1,780 bytes, which would fit the narrowing detail too - but the
/// scope decision below was never purely a budget one. Naming several
/// languages while also describing what each one's resolution binds to would
/// restate this whole doc comment's paragraph once per language in
/// [`p4_named`]'s output, which is a complexity cost independent of whatever
/// the ceiling currently allows, so it stays out.
const P4_STATIC_RECEIVER: &str =
    "The one legitimate reason to grep afterward: a method call through a variable \
     receiver (`x.foo()`) binds to the receiver's declared or inferred type, not \
     the one it holds at run time, so an override's caller page under-reports - \
     calls reaching it through a base or interface sit on that base's page, and \
     find_implementations is the way across. On a project's first index, or a \
     re-index after an upgrade, a tool call waits for the walk to finish before \
     answering - slow, not wrong; do not abandon it for grep.";

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
        "The one legitimate reason to grep afterward: a method call through a \
         variable receiver (`x.foo()`) produces no edge in {list}, so caller/reference \
         lists for methods can under-report; bare function calls and this/super/qualified-type \
         calls have no such gap, and a `hasMore: false` page for those is exhaustive. On a \
         project's first index, or a re-index after an upgrade, a tool call waits for the walk \
         to finish before answering - slow, not wrong; do not abandon it for grep."
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
/// module's own tests: 413 bytes against the eight-language sentence's 526
/// (both re-measured after GM-394 shortened the second clause; they were 481
/// and 594 before it). The eight-language case does not actually need this
/// fallback yet (see [`build`]'s worst-case test, which stays under the
/// ceiling without it), so the margin here is headroom for whichever *later*
/// language makes it not fit, not headroom this fallback is spending on
/// today's set.
fn p4_fallback() -> String {
    "The one legitimate reason to grep afterward: a method call through a variable \
     receiver (`x.foo()`) produces no edge in some of this project's languages until \
     their semantic layer finishes - check which before trusting a method's page as \
     exhaustive. On a project's first index, or a re-index after an upgrade, a tool \
     call waits for the walk to finish before answering - slow, not wrong; do not \
     abandon it for grep."
        .to_string()
}

const P5: &str = "Efficient usage: pass `symbol_name` directly to the four tools above instead \
     of calling find_definition first, and raise `limit` for symbols with many \
     results instead of paging.";

/// Joins the five paragraphs the same way the original single string literal
/// always did: blank-line separated, nothing trimmed or reflowed - so a
/// caller only ever varies `p2`/`p4`, never how they meet the fixed
/// paragraphs around them.
fn assemble(p4: &str) -> String {
    [P1, P2, P3, p4, P5].join("\n\n")
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
///    default: render [`P4_GENERIC`], the same sentence case 3 below renders,
///    so a project mid-cold-start reads no differently than a project with
///    exactly one present language does.
/// 2. **No present language has an *open* gap** - [`P4_STATIC_RECEIVER`]:
///    every language here resolves receiver calls, so clause (1) says what
///    that resolution binds to rather than disappearing. Until GM-385 this
///    case dropped the clause and announced "One real gap", which measured
///    false in all three of the languages that can reach it.
/// 3. **Exactly one language is present** (and it has the gap, since case 2
///    already handled "it doesn't") - [`P4_GENERIC`] again, unchanged from
///    case 1. A single present language is never ambiguous about which
///    language "no edge by design" describes, so naming it would spend bytes
///    to restate what the unqualified sentence already means; this is also
///    what keeps a TypeScript-only project's instructions identical to what
///    an empty index renders (this module's own
///    `ts_only_is_byte_identical_to_the_original_string` test - "original"
///    there meaning this module's own current baseline, re-pinned at GM-394,
///    not literally the text `get_info` returned before GM-262 - see that
///    test's own doc comment).
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
        return assemble(P4_GENERIC);
    }

    let gapped = languages_with_open_receiver_gap(present);
    if gapped.is_empty() {
        return assemble(P4_STATIC_RECEIVER);
    }
    if present.len() == 1 {
        return assemble(P4_GENERIC);
    }

    let list = format_language_list(&gapped);
    let named = assemble(&p4_named(&list));
    if named.len() <= INSTRUCTIONS_BYTE_CEILING {
        named
    } else {
        assemble(&p4_fallback())
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

/// How a front counts its projects: `N`, or `N+` when the walk stopped at a
/// limit and more may exist.
pub(crate) fn project_count(count: usize, truncated: bool) -> String {
    if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    }
}

/// What the front's list appends to a candidate that already has a completed
/// index of its own.
const INDEXED_MARK: &str = " (indexed)";

/// The front's sentence (D12), with `subject` naming the folder: either the
/// root path (`"<root> is a folder of"`) or the no-path fallback (`"This
/// folder holds"`). Worded to stay true after a session switch (D11 step 5):
/// it says "before any other tool", never "nothing is selected".
///
/// `indexed` is how many candidates already have a completed index (rule 2's
/// check, `candidates::has_completed_index`): none keeps the original
/// wording; otherwise the sentence counts them and says they are listed
/// first and marked with [`INDEXED_MARK`].
fn front_sentence(subject: &str, count: &str, indexed: usize) -> String {
    let state = if indexed == 0 {
        "has indexed none of them".to_string()
    } else {
        format!("has already indexed {indexed} of them, listed first and marked (indexed)")
    };
    format!(
        "{subject} {count} projects; g-mesh serves one at a time and {state}. \
         Before any other g-mesh tool, call select_project with the one you are working on (ask the \
         user if unclear). Its result names the project this session then serves and carries that \
         project's guidance; call it again to switch, or to re-read that guidance. Projects: "
    )
}

/// `head` followed by as many of `names` as fit under
/// [`INSTRUCTIONS_BYTE_CEILING`], then either `.` (all listed) or the `(+K
/// more ...)` pointer, with how many names made it. `None` when not even the
/// head and its ending fit.
fn front_with_names(head: &str, names: &[&str]) -> Option<(String, usize)> {
    let ending = |listed: usize| {
        if listed == names.len() {
            ".".to_string()
        } else {
            format!(
                " (+{} more - call select_project with no argument for the full list).",
                names.len() - listed
            )
        }
    };
    let mut listed = 0;
    let mut body = String::new();
    while listed < names.len() {
        let separator = if listed == 0 { "" } else { ", " };
        let next = format!("{body}{separator}{}", names[listed]);
        if head.len() + next.len() + ending(listed + 1).len() > INSTRUCTIONS_BYTE_CEILING {
            break;
        }
        body = next;
        listed += 1;
    }
    let rendered = format!("{head}{body}{}", ending(listed));
    (rendered.len() <= INSTRUCTIONS_BYTE_CEILING).then_some((rendered, listed))
}

/// `get_info`'s instructions for a front (D12 in
/// `docs/architecture/lazy-indexing.md`, GM-399): [`P1`] (true before and
/// after a switch), then the folder sentence and as many candidate
/// `rel_path`s as fit under [`INSTRUCTIONS_BYTE_CEILING`]. The language
/// paragraphs are left out: no language is known yet, and the selected
/// project's own guidance arrives in the `select_project` result.
///
/// `indexed` holds the `rel_path`s of the candidates that already have a
/// completed index; they are listed first (so the ceiling cuts unindexed
/// names before indexed ones), each followed by [`INDEXED_MARK`], and the
/// rest keep their walk order.
///
/// When the root path would cost the list its first name (or break the
/// ceiling outright), the path is dropped - the same fallback [`cold_start`]
/// uses.
pub fn build_front(root: &Path, detection: &Detection, indexed: &HashSet<&str>) -> String {
    let (done, rest): (Vec<&str>, Vec<&str>) =
        detection.candidates.iter().map(|c| c.rel_path.as_str()).partition(|name| indexed.contains(name));
    let marked: Vec<String> = done.iter().map(|name| format!("{name}{INDEXED_MARK}")).collect();
    let names: Vec<&str> = marked.iter().map(String::as_str).chain(rest).collect();
    let count = project_count(names.len(), detection.truncated);
    let with_path = format!(
        "{P1}\n\n{}",
        front_sentence(&format!("{} is a folder of", root.display()), &count, done.len())
    );
    if let Some((rendered, listed)) = front_with_names(&with_path, &names) {
        if listed > 0 || names.is_empty() {
            return rendered;
        }
    }
    let without_path = format!("{P1}\n\n{}", front_sentence("This folder holds", &count, done.len()));
    front_with_names(&without_path, &names).map(|(rendered, _)| rendered).unwrap_or(without_path)
}

#[cfg(test)]
mod tests;
