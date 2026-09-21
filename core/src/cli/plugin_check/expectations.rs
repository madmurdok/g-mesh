//! `--expect <expect.toml>`: post-linking assertions against the exact query
//! code the MCP tools use - GM-277, "Conformance kit > Expectations" in
//! `docs/architecture/multi-language-plugins.md`.
//!
//! # What "the same handler code" means here
//!
//! Every expectation is answered by literally calling an MCP tool's own
//! handler function - [`find_callers_callees::handle_callers`],
//! [`find_references::handle`], [`find_implementations::dispatch`],
//! [`get_dependencies::handle`], [`find_definition::handle`] - the exact
//! functions `mcp::mod`'s `#[tool]` methods call, given the exact parameter
//! structs a real MCP client would send. This module never re-walks an edge
//! or re-runs a query of its own: it drives those five functions and reads
//! back their *wire JSON*, the same bytes a client would receive
//! (`rmcp::model::CallToolResult`'s `ContentBlock::Text`). That is also why
//! ambiguity is answered for free rather than needing its own code path: an
//! ambiguous `symbol_name` already makes four of these five handlers return
//! `find_definition`'s own ranked candidate page in place of their usual
//! result (`mcp::anchor`'s module doc), and this module only has to
//! recognize that shape - a top-level `resolvedBy` field, see
//! [`is_candidate_page`] - to report it as a failure with the real candidate
//! list, never a silent pick.
//!
//! # Decision 1: when expectations are evaluated
//!
//! Against the *same* connection the whole control-plane session
//! (`session::run_session`) left behind - bulk, every `fileChanged` step and
//! the whole-project `semanticPass` - once `run_session` has returned, not a
//! snapshot taken partway through. Two facts make this the only sound
//! choice:
//!
//! - **Namespace-import resolution needs the semantic pass to have run**
//!   (the task's own framing): `import * as ns` then `ns.fn()` produces no
//!   structural edge at all - see `core/tests/namespace_import_resolution.rs`'s
//!   own module doc - so an expectation over it can only pass once
//!   `semanticPass` (step 6 of `run_session`) has landed its upgrade through
//!   `link_diff`.
//! - **The session's own edit steps are safe to evaluate after**, because
//!   this module's answer format (decision 2) never carries a line or
//!   column - it is `file:qualifiedName` sets. `session::declaration_edit`
//!   inserts a line break before a declaration's last line, which moves
//!   *positions*, never which file a symbol lives in or its qualified name.
//!   So the one edit that outlives the session - the file "stays edited from
//!   here on", per that module's own doc comment - cannot change any
//!   expectation's answer, whichever file `choose_edit_target` happened to
//!   pick.
//!
//! Evaluating any earlier would either miss the semantic pass (breaking
//! every namespace-import expectation, which is the entire reason a
//! plugin's semantic tier exists) or would need a second, separate index
//! snapshot that nothing else in this kit reads.
//!
//! Because of this, expectations are only evaluated once that state
//! genuinely exists: bulk run 1 completed and the control-plane session ran
//! to its end without its own failure (`mod.rs`'s `session_ready` gate). A
//! session that failed already has the built-in `session` check failing the
//! whole report; skipping expectations rather than judging a partial index
//! keeps a `Skip` about missing evidence, never a false pass or a false
//! fail (`report::Outcome::Skip`'s own doc comment).
//!
//! # Decision 2: the answer format
//!
//! Every expectation's `expect` list is a *set* of strings - order never
//! matters (`BTreeSet` throughout, and the diff a failure prints is one too,
//! so a re-ordered fixture is never a spurious failure). Whether a *repeated*
//! row matters is decision 8, which is a per-category question rather than a
//! property of this format:
//!
//! - **`[[callers]]` / `[[references]]` / `[[implementations]]`**: each
//!   result row renders as `"{filePath}:{qualifiedName}"` - except a
//!   `File`-kind row (the usage sits outside any tracked symbol, so the row
//!   names a whole file - see `find_callers_callees`'s own module doc on
//!   this shape), which carries no `qualifiedName` at all and renders as
//!   `"{filePath}:"`, the trailing colon rather than the bare path chosen so
//!   the format is always exactly one colon and a reader can
//!   `split_once(':')` without a case for whether a qualifiedName is
//!   present.
//! - **`[[definition]]`**: the single resolved node, the same
//!   `"{filePath}:{qualifiedName}"` rule, as a one-element set. A *refusal*
//!   is not a zero-element set here, and this said for a while that it was
//!   (GM-371): a tool declining to answer is a different fact from a tool
//!   answering nothing, and conflating them is the mistake the whole 3.8.0
//!   batch was about. `[[refusal]]` is where a refusal is asserted -
//!   decision 9, which also has why `expect = []` was not made to mean it.
//! - **`[[refusal]]`**: no set at all. The entry names a tool and a symbol
//!   that tool must *decline* to resolve, plus the phrases its refusal has
//!   to carry - decision 9.
//! - **`[[imports]]`**: `get_dependencies`'s rows. A row with a `filePath`
//!   (an indexed file) renders as the bare file path, no prefix - the
//!   answer already *is* a file. A row with no `filePath` (a `Module`-kind
//!   placeholder identified only by its `qualifiedName`) renders as
//!   `"container:{qualifiedName}"`, the wire convention the design doc's own
//!   example uses (`expect = ["container:github.com/x/app/server"]`).
//!   `get_dependencies`'s `DependencyNode` does not distinguish a core-owned
//!   container node from an external-module placeholder tree-sitter could
//!   not resolve (`"react"`, a node builtin) - from this row's own fields
//!   both are exactly "a targetless row identified by a key" - so both get
//!   the same prefix. TS has no containers at all (`multi-language-plugins.md`'s
//!   Logical containers table: "none: the file is the scope"), so every
//!   `container:` row this kit's own TS fixture can produce is in fact an
//!   unresolved external import, never a container - documented here rather
//!   than distinguished, since telling them apart needs a wire field
//!   (`DependencyNode` carrying `nativeKind`) this task does not add, and no
//!   caller of `get_dependencies` needs the distinction either: both answer
//!   "this import didn't resolve to a file in the project".
//! - **`[[importers]]`**: the same rows, from the same tool, walked
//!   `Incoming` - see decision 7. In practice every row is a bare file path,
//!   because an `IMPORTS` edge starts at the file where the `use`/`import`
//!   line is written; the `container:` spelling is shared with `[[imports]]`
//!   rather than forbidden here, so that a plugin which one day points an
//!   import at a container renders the same way in both directions.
//!
//! # Decision 3: anchoring and ambiguity
//!
//! `symbol = "Server.Close"` is resolved by calling the target tool with
//! `symbol_name` set and nothing else - the tool's own resolution ladder
//! (`mcp::find_definition::resolve_symbol_name`, shared by all five
//! handlers). Three things can come back:
//!
//! 1. **An ordinary result page** (has an `"anchor"` key, for the four
//!    edge-walking tools) or **the resolved node itself** (for
//!    `find_definition`, which has no `"anchor"` wrapper - and which, on this
//!    rung, echoes its own `resolvedBy` at the top level too, exactly the
//!    shape case 2 below matches on; see [`is_candidate_page`] for the extra
//!    field that tells the two apart) - the happy path, used directly.
//! 2. **A candidate page** (a top-level `"resolvedBy"` key - `nameAmbiguous`,
//!    `fileName`, or `semanticNeighbours`; see `find_definition::ResolvedBy`
//!    - alongside a top-level `"results"` array; see [`is_candidate_page`]).
//!
//!    If the expectation gave an optional `file`, and *exactly one*
//!    candidate's `filePath` matches it, that candidate is the answer: for
//!    the four `symbol_id`-accepting tools, the handler is re-called with
//!    `symbol_id` set to the candidate's `id` - still the same handler code,
//!    now anchored precisely rather than by a name that turned out
//!    ambiguous. `find_definition` itself has no `symbol_id` parameter (a
//!    real limitation of the real tool, not something this module works
//!    around), so its own disambiguated re-call uses the candidate's exact
//!    `qualifiedName`, which takes `resolve_symbol_name`'s fast
//!    exact-match rung.
//!
//!    That rung is narrower since GM-360, and it bounds what a
//!    `[[definition]]` entry can say. A candidate whose `qualifiedName` is
//!    the bare name several declarations carry - a crate-root Rust type
//!    against a module-qualified namesake, or a package-level Go func
//!    against a method - is ambiguous *as a query*, so the re-call returns a
//!    candidate page a second time and [`resolve_definition`] fails the
//!    entry saying exactly that. Nor does a `qualifiedName` that two
//!    declarations share (`shapes::Gauge`, in two crates of
//!    `plugins/rust/conformance/project`) narrow to one. Both shapes are
//!    still expressible as `[[references]]`/`[[callers]]`/
//!    `[[implementations]]`, whose re-call is by `id` - which is why that
//!    fixture asserts its two-crate case through `[[references]]`.
//! 3. **No `file`, or `file` narrows to zero or more than one candidate**:
//!    the expectation fails, with every candidate's `id`/`qualifiedName`/
//!    `filePath`/`kind` printed - never a silent pick, per the task's own
//!    instruction.
//!
//! # Decision 4: paging
//!
//! Every call to a `results`-shaped tool asks for `limit =
//! pagination::MAX_PAGE_SIZE` (200, the tools' own ceiling - "raise it
//! rather than paging" is already every such tool's documented contract).
//! If the answer still reports `hasMore: true` (the four edge-walking
//! tools) or `truncated: true` (`get_dependencies`), the expectation fails
//! outright rather than comparing a partial page - a page that happens to
//! contain every expected entry despite being incomplete would otherwise
//! pass by accident, and a page missing one would be indistinguishable from
//! a real mismatch.
//!
//! # Decision 5: unknown keys and exit status
//!
//! [`ExpectFile`] and every expectation struct in it derive
//! `#[serde(deny_unknown_fields)]`, so a typo'd key (`[[calller]]`, `expect
//! = [...]` misspelled `expet`) is a hard parse failure, reported as the
//! single `expectations.file` check (see [`parse`] and `mod.rs`) - never
//! silently ignored the way plain `#[derive(Deserialize)]` would. Any
//! failing expectation is a `report::Outcome::Fail`, which already fails
//! the whole run's exit status through the same path every built-in check
//! does (`report::Report::failed`) - expectations need no exit-code
//! mechanism of their own.
//!
//! # Decision 6: a reduced set for a missing toolchain, from the same file
//!
//! Some plugins have a semantic tier that needs a real toolchain on `PATH`
//! (`go/types` for Go, `tsserver` for TypeScript) and degrade to
//! structural-only answers without one - a real, supported deployment shape
//! (`plugins/go/README.md`'s "Out of scope, deliberately"), not a broken one.
//! GM-282 needs a CI job that runs with `go` off `PATH` and still asserts
//! *something* - "the structural entries still pass" - without either
//! duplicating the file (a second `expect.toml` a fixture author can update
//! and forget to mirror) or hand-picking ids to skip (a list that drifts the
//! moment an entry is reordered or a new one inserted before it).
//!
//! So the same file carries the answer: [`SymbolExpectation`] and
//! [`ImportsExpectation`] both take an optional `tier = "semantic"` (default
//! `"structural"`, the common case, so most entries never spell it out).
//! `--skip-semantic-expectations` (`mod.rs`'s `PluginCheckArgs`) makes
//! [`evaluate`] answer every `tier = "semantic"` entry with `Outcome::Skip`
//! instead of calling its handler, in the same file-order position it would
//! otherwise occupy - so the *set* of entries this flag reduces to is
//! whatever the fixture author already tagged, computed once by [`evaluate`]
//! from the one file both CI invocations read, never copied by hand into a
//! second one. `plugins/go/conformance/expect.toml`'s four `go/types`-only
//! entries (three `[[callers]]`, one `[[implementations]]`) carry the tag;
//! everything else - bare and package-qualified calls, references,
//! `[[imports]]`, `[[importers]]`, `[[definition]]` - answers from the
//! structural tier alone and is untagged.
//!
//! # Decision 7: the incoming direction (GM-365)
//!
//! Until this task every category here asked a question in the *outgoing*
//! direction: who calls this, what does this file import, what implements
//! this. Three of the 3.8.0 batch's six wrong answers were about the other
//! one - GM-356 (an `Incoming` walk anchored on a file answered a
//! well-formed zero outside TypeScript) and GM-358 (`from <pkg> import
//! <submodule>` built no `IMPORTS` edge, so a 13-file answer silently
//! missed a fourteenth) - and no expectation in any of the four fixtures
//! could have caught either, because none of them ran a walk in that
//! direction at all. `[[importers]]` is that direction:
//! `get_dependencies(file, Incoming, max_depth = 1)`, the same handler
//! `[[imports]]` already drives with `Outgoing`.
//!
//! **A separate list rather than a `direction` key on `[[imports]]`**, for
//! two reasons that are both about what a failure says: the report id is
//! `expectations.importers[2]`, which names the direction without anyone
//! having to open the fixture, and `deny_unknown_fields` (decision 5) then
//! keeps [`ImportersExpectation`]'s `via_module` - which is meaningless
//! outgoing - out of an `[[imports]]` entry by construction.
//!
//! **`via_module` asserts GM-356's substitution, and its absence asserts
//! the lack of one.** Outside TypeScript an `IMPORTS` edge arrives at a
//! *container* (`pkg.helpers`, `alpha::internals`,
//! `github.com/example/app/server`), never at a file, so an `Incoming` walk
//! literally anchored on a file is empty by construction;
//! `get_dependencies`'s `incoming_from_file` substitutes the module that
//! file defines and reports it through `resolvedFrom.qualifiedName`. An
//! entry that gives `via_module` requires exactly that key; an entry that
//! omits it requires the response to carry no `resolvedFrom` at all - the
//! walk ran from the file itself, which is TypeScript's shape, where a
//! module *is* a file. Both directions are checked because only the pair
//! discriminates: a fixture asserting the importer set alone would pass
//! whether the answer came from the file or from a module chosen for it,
//! which is precisely the distinction GM-356 turned out to be about.
//!
//! **A language this does not apply to says so in its own file.** Nothing
//! here lets a fixture omit a category and still look complete, which is the
//! defect class the whole 3.8.0 batch is about - so the four bundled
//! `expect.toml`s each carry an `[[importers]]` entry and each say in prose
//! which arm of the substitution they are (three substituting, TypeScript
//! not). Two things this kit genuinely cannot assert are written down for
//! the same reason rather than left to be re-derived: a duplicate row
//! outside `[[implementations]]`/`[[imports]]`/`[[importers]]` (decision 8),
//! and anything about a *position*. GM-363 documented that `startLine` is
//! zero-based while `source.firstLine` beside it is one-based; no
//! expectation can check that, and not by omission - decision 1 evaluates
//! against an index whose chosen file has deliberately been left
//! line-shifted by `session::declaration_edit`, and decision 2's
//! `file:qualifiedName` answer format is what makes that safe. A category
//! carrying coordinates would have to give that up, so GM-363's coverage is
//! its unit tests and this paragraph.
//!
//! # Decision 8: a repeated row, where repeating is not an answer (GM-361)
//!
//! A `BTreeSet` cannot see a duplicate, and GM-361 found one that mattered:
//! `find_implementations` listed *edges*, so an implementor both of a
//! plugin's tiers had found appeared twice, and the measured ripgrep page
//! was 12 rows describing 7 implementors. That defect is invisible to a set
//! comparison - with GM-361's de-duplication disabled the whole rust
//! conformance suite still passed, which that fixture's own comment records.
//!
//! So the set is no longer the only thing compared. For the categories whose
//! tool promises one row per *answer* - `[[implementations]]`
//! (`Distinctness::OtherEndpoint`, one row per implementing type) and
//! `[[imports]]`/`[[importers]]` (a traversal visits a node once) - a
//! repeated `"{filePath}:{qualifiedName}"` row fails the entry, naming what
//! repeated and how often, even when the *set* matches exactly.
//!
//! `[[callers]]` and `[[references]]` are deliberately exempt, and that is
//! why the check is per category rather than global: those two run on
//! `Distinctness::Edges`, where two rows for one symbol are two usages, and
//! that is the contract (`pagination::Distinctness`' own doc: "two `f();
//! f();` in one function genuinely are two calls"). Failing them on a repeat
//! would assert the opposite of what `find_references` guarantees. Their
//! sets stay sets, and the limitation is stated here rather than silently
//! tolerated.
//!
//! One shape this check would misread, written down because it is a cost of
//! having it: two *distinct* declarations sharing a file and a
//! `qualifiedName`, both implementing the anchor, render as one string twice
//! and would be reported as a repeat. Decision 2's answer format already
//! cannot tell those two apart - the expected set can only name the row once
//! either way - so the fix if it ever happens is an answer format carrying
//! an id, not dropping this check. No bundled fixture has one (all four
//! suites pass), and a failure names the row, so the day one appears it is
//! visible rather than silent.
//!
//! # Decision 9: asserting that a tool refuses (GM-371)
//!
//! Every category above asserts what a name *is*. None could assert what it
//! is *not*, and GM-367 is precisely a change whose whole effect on four of
//! the five tools this kit drives is "a name that used to resolve now
//! refuses" - nothing in any fixture could have pinned it. `[[refusal]]` is
//! that assertion: a `tool`, a `symbol` that tool must decline to resolve,
//! and the phrases the refusal has to carry.
//!
//! ```toml
//! [[refusal]]
//! tool = "definition"
//! symbol = "strings"
//! contains = ["is declared in this project", "get_dependencies"]
//! ```
//!
//! **Why not `expect = []`**, which is what decision 2 promised until this
//! task and what the natural reading of "this name is not a definition here"
//! would be. Three reasons, in the order they decided it:
//!
//! - **An empty answer and a refused one are different facts, and this kit
//!   exists to keep them apart.** GM-356 was a walk that returned a
//!   *well-formed zero* where the truth was "you anchored on the wrong
//!   node", and decision 7 added `[[importers]]` so that an empty answer and
//!   an absent one stop reading alike. Spelling a refusal `[]` puts that
//!   same conflation back, in the one category that did not have it.
//! - **`[]` is unambiguous for `[[definition]]` only by accident of arity.**
//!   `find_definition` returns at most one node, so `[]` has no second
//!   reading there *today*. Everywhere else `expect = []` already means
//!   "resolved, and the answer is empty" - a shape bundled fixtures use
//!   (`plugins/python`'s `[[importers]]`, the `tier` parse tests). So the
//!   `[]`-means-refusal rule could never extend to `[[callers]]` /
//!   `[[references]]` / `[[implementations]]` without *weakening* the
//!   entries already written: one of those that began refusing would start
//!   to pass. A shape that can only ever cover one of the four tools
//!   GM-367 changed is the wrong shape for the defect that motivated it.
//! - **`[]` has nowhere to put the refusal's text**, and the text is the
//!   only thing that separates "refused because this is not a declaration"
//!   from "the call broke". An expectation that passes on a dead daemon is
//!   worse than no expectation.
//!
//! **What a `[[refusal]]` must not accept**, which is the whole of its
//! value. It passes on exactly one shape - a *tool-level* refusal
//! (`CallToolResult` with `is_error: true`) whose text carries every phrase
//! in `contains` - and fails on all four of the others:
//!
//! - **A protocol-level failure** - `ErrorData`, i.e. the outer `Err` that
//!   `mcp::anchor`'s own doc reserves for "genuine protocol-level failures":
//!   a poisoned mutex, a SQLite error, an index that is not there. Never a
//!   refusal, whatever it says, and the finding says which kind it was.
//!   [`tool_json`] used to flatten this distinction into one `Err(String)`,
//!   which is why the branch decision 2 described could not be reached;
//!   [`ToolOutcome`] is that distinction restored.
//! - **An answer.** The tool resolved the name, which is the opposite of
//!   what the entry claims; the resolved row is printed.
//! - **A candidate page.** Ambiguity is not refusal - the tool had answers
//!   and asked which one - so this fails with the candidates listed,
//!   consistent with decision 3's "never a silent pick".
//! - **A refusal missing a phrase.** Named individually, with the refusal
//!   quoted, so the fixture can be fixed against what was actually said.
//!
//! `contains` is required and must be non-empty, for the third reason
//! above: without it the entry would accept *any* refusal, including the
//! parameter-validation ones (`find_definition`'s "give either `symbol_name`,
//! or both `file_path` and `position`"). Those are unreachable from here -
//! this module builds every parameter struct itself and always sends
//! `symbol_name` alone - but "unreachable by construction" is a property of
//! today's call sites, not something a fixture should have to rely on.
//!
//! **No `file` key**, unlike the four `symbol`-anchored categories. `file`
//! exists to pick one of several candidates (decision 3); an entry claiming
//! the name resolves to nothing has nothing to disambiguate, and a candidate
//! page is a failure here rather than something to narrow.
//!
//! **Four tools, not five.** `[[imports]]`/`[[importers]]` anchor on a file
//! path rather than a name, so the only refusal they can produce is about
//! the fixture naming a path that is not in the project - a broken fixture,
//! not a claim about the language. The four that resolve a `symbol_name`
//! through `find_definition::resolve_symbol_name` are the ones where
//! refusing is an answer about the code.
//!
//! # Decision 10: asserting which tier answered (GM-382)
//!
//! Every category above asserts *what* a tool answered. None could assert
//! which tier produced the answer - and until GM-382 no response said,
//! which is the root under GM-356/358/360/361/362: the same query against a
//! Rust project with `rust-analyzer` installed and without it returns the
//! same shape, the same `resolved: true` rows, and means different things.
//!
//! `provenance` is that assertion, on the three `symbol`-anchored
//! edge-walking categories plus `[[definition]]`:
//!
//! ```toml
//! [[callers]]
//! symbol = "shapes::Square::perimeter"
//! tier = "semantic"
//! expect = ["..."]
//! provenance = "silent"
//! ```
//!
//! **Two values, and omission is the third spelling of the first.**
//! `"silent"` requires the response to carry no `provenance` key at all -
//! the plugin's semantic tier ran, so there is nothing to disclose. Any
//! other value is a language id and requires
//! `provenance = {language = "<id>", semanticTier = "absent"}`. Omitting
//! the key means `"silent"` too, the same way `ImportersExpectation::
//! via_module`'s absence is an assertion rather than a skip (decision 7) -
//! so every entry already written in every bundled fixture gained this
//! assertion without being edited, which is the point: a plugin that
//! started disclaiming its own semantic tier would fail its whole file, not
//! one opted-in entry.
//!
//! **`[[definition]]` accepts the key but can only ever satisfy
//! `"silent"`.** `find_definition` resolves a declaration, which is
//! structural work no semantic tier changes, so it carries no block by
//! design (`mcp::provenance`'s module doc, "scoped to four tools"). The key
//! is still evaluated there rather than ignored: a fixture that asked a
//! definition entry for a language id would otherwise get silence from a
//! typo, and silence is what this whole decision exists to remove.
//!
//! **Why the fixtures only ever say `"silent"`.** A conformance session
//! drives its plugin's whole-project semantic pass and only continues if it
//! completed (`session::run_session`), so a *passing* check run is by
//! construction one whose semantic tier was present. The other arm - engine
//! declared, engine unreachable - ends the session before expectations are
//! evaluated, so it is pinned in `mcp::find_callers_callees`' own tests
//! against a hand-built index instead. Giving the kit an arm that survives
//! a missing engine would let a fixture assert both halves here, and is
//! left as its own task rather than folded into this one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::cli::plugin_check::report::{CheckResult, Outcome};
use crate::daemon::manifest::Capabilities;
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::{self, Direction};
use crate::mcp::{
    find_callers_callees, find_definition, find_implementations, find_references, get_dependencies,
    FindDefinitionParams, FindImplementationsParams, GetDependenciesParams, SymbolQueryParams,
};

/// The whole `--expect` file. Every list defaults to empty, so a fixture
/// that only cares about, say, `[[callers]]` never has to spell out the
/// other four as `[]`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectFile {
    #[serde(default)]
    callers: Vec<SymbolExpectation>,
    #[serde(default)]
    references: Vec<SymbolExpectation>,
    #[serde(default)]
    implementations: Vec<SymbolExpectation>,
    #[serde(default)]
    imports: Vec<ImportsExpectation>,
    #[serde(default)]
    importers: Vec<ImportersExpectation>,
    #[serde(default)]
    definition: Vec<SymbolExpectation>,
    #[serde(default)]
    refusal: Vec<RefusalExpectation>,
}

/// Which of a plugin's tiers an expectation needs answered before it can
/// possibly pass - decision 6. `Structural` is the default: most
/// expectations are answerable by the always-available tier, so most entries
/// never spell this field out at all.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Tier {
    #[default]
    Structural,
    /// Needs the plugin's semantic tier (a real toolchain on `PATH`) to have
    /// resolved the site - `--skip-semantic-expectations` answers this entry
    /// with `Outcome::Skip` rather than running it (decision 6).
    Semantic,
}

/// One `[[callers]]` / `[[references]]` / `[[implementations]]` /
/// `[[definition]]` entry - they share a shape because all four anchor on a
/// `symbol` the same way (decision 3) and compare the same kind of set
/// (decision 2).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolExpectation {
    symbol: String,
    /// Disambiguates an ambiguous `symbol` by the file one of its
    /// candidates lives in - decision 3. Optional, and only ever consulted
    /// when the plain name turns out ambiguous.
    #[serde(default)]
    file: Option<String>,
    expect: Vec<String>,
    /// Decision 6. Absent means `Structural`.
    #[serde(default)]
    tier: Tier,
    /// Decision 10 (GM-382): what the response's `provenance` block must
    /// say. **Absence is itself an assertion** - the same rule
    /// `ImportersExpectation::via_module` documents - so an entry that does
    /// not mention this field still requires the response to carry no
    /// `provenance` at all, which is what every already-written entry in
    /// every bundled fixture means and why adding the field needed no edit
    /// to any of them. `"silent"` spells that same requirement out loud, for
    /// the one entry per fixture that exists to say so; any other value is a
    /// language id, and requires a block naming that language with
    /// `semanticTier: "absent"`.
    #[serde(default)]
    provenance: Option<String>,
}

/// One `[[imports]]` entry: `get_dependencies` from `file`, one hop
/// (`Outgoing`), compared as the set decision 2 defines.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportsExpectation {
    file: String,
    expect: Vec<String>,
    /// Decision 6. Absent means `Structural`; no bundled fixture's
    /// `[[imports]]` entry needs the semantic tier today, but the field
    /// exists here too so one that does never has to duplicate the file to
    /// say so.
    #[serde(default)]
    tier: Tier,
}

/// One `[[importers]]` entry: the same tool and the same hop, walked the
/// other way - `get_dependencies` from `file`, one hop (`Incoming`) - which
/// is decision 7's whole subject.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportersExpectation {
    file: String,
    /// The container key the walk is required to have run from, reported by
    /// `get_dependencies` as `resolvedFrom.qualifiedName` (GM-356).
    ///
    /// **Absent is an assertion too**, not "don't check": it requires the
    /// response to carry no `resolvedFrom` at all, i.e. that the anchor was
    /// taken literally. Decision 7 has why the pair is what discriminates.
    #[serde(default)]
    via_module: Option<String>,
    expect: Vec<String>,
    /// Decision 6. Absent means `Structural` - an `IMPORTS` edge is
    /// structural in every bundled plugin, so no entry needs the tag today.
    #[serde(default)]
    tier: Tier,
}

/// One `[[refusal]]` entry - decision 9. Its own struct rather than a key on
/// [`SymbolExpectation`], for the reason decision 7 gave `[[importers]]` one:
/// `deny_unknown_fields` then keeps `contains` off a `[[callers]]` entry,
/// where it would mean nothing, by construction rather than by review.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefusalExpectation {
    /// Which of the four `symbol_name`-resolving tools must refuse.
    tool: RefusedTool,
    /// The name it must decline to resolve. No `file` companion: decision 9
    /// has why there is nothing here to disambiguate.
    symbol: String,
    /// Every phrase the refusal's own text has to carry. Required and
    /// checked non-empty (decision 9): an entry that asserted only "it
    /// refused" would be satisfied by a refusal about something else.
    contains: Vec<String>,
    /// Decision 6. Absent means `Structural`.
    #[serde(default)]
    tier: Tier,
}

/// Decision 9's `tool` key: which handler a `[[refusal]]` entry drives.
/// Deliberately the four that resolve a `symbol_name` - see decision 9 on why
/// `imports`/`importers` are not among them.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RefusedTool {
    Definition,
    Callers,
    References,
    Implementations,
}

/// Reads and parses `path`. The one hard-error path in this module - see
/// decision 5: a missing file, invalid TOML, or an unknown key is not a
/// check finding, it is why no check ran at all.
pub(crate) fn parse(path: &Path) -> Result<ExpectFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read the expectations file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse the expectations file {}", path.display()))
}

/// Everything [`evaluate`] needs to drive the five handlers - one connection
/// (decision 1), the disabled embedding pipeline every other step of this
/// kit already uses (semantic-neighbour candidates never enter the picture,
/// which keeps ambiguity handling to the two rungs decision 3 documents),
/// the fixture's scratch root (`find_definition::handle`'s `project_root`,
/// unused here since every call passes `include_source: false`, but still a
/// required argument of the real handler), and the manifest's own declared
/// entry points (`get_dependencies`' miss-path fallback).
pub(crate) struct EvalContext<'a> {
    pub(crate) conn: &'a Arc<Mutex<Connection>>,
    pub(crate) embedding: &'a EmbeddingPipeline,
    pub(crate) project_root: &'a Path,
    pub(crate) entry_points: &'a [String],
    /// The manifest's own `[plugin.capabilities]`, keyed by language -
    /// what the four edge-walking handlers need to decide whether this
    /// plugin declares a semantic tier at all (`mcp::provenance::resolve`).
    /// A one-entry map, because a check run is always about exactly one
    /// plugin; built from `manifest.capabilities` rather than from a
    /// registry, since the kit discovers no registry.
    pub(crate) capabilities: &'a HashMap<String, Capabilities>,
}

/// Runs every expectation in `expect`, in file order, and returns one
/// [`CheckResult`] per entry - report.rs's own module doc calls for "one per
/// expectation the fixture's author wrote, not one per rule". `skip_semantic`
/// is decision 6's `--skip-semantic-expectations`: a `tier = "semantic"`
/// entry is answered with `Outcome::Skip` in place rather than run, so the id
/// numbering and the count of entries in the report is identical whether or
/// not the flag is set - only the outcome of the tagged ones changes.
pub(crate) fn evaluate(ctx: &EvalContext, expect: &ExpectFile, skip_semantic: bool) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (index, item) in expect.callers.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "callers", index, item.tier, || {
            eval_symbol_expectation(ctx, "callers", index, item, SymbolTool::Callers)
        }));
    }
    for (index, item) in expect.references.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "references", index, item.tier, || {
            eval_symbol_expectation(ctx, "references", index, item, SymbolTool::References)
        }));
    }
    for (index, item) in expect.implementations.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "implementations", index, item.tier, || {
            eval_symbol_expectation(ctx, "implementations", index, item, SymbolTool::Implementations)
        }));
    }
    for (index, item) in expect.imports.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "imports", index, item.tier, || {
            eval_imports_expectation(ctx, index, item)
        }));
    }
    for (index, item) in expect.importers.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "importers", index, item.tier, || {
            eval_importers_expectation(ctx, index, item)
        }));
    }
    for (index, item) in expect.definition.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "definition", index, item.tier, || {
            eval_definition_expectation(ctx, index, item)
        }));
    }
    // Decision 9, last so that adding the category left every already-written
    // fixture's `expectations.{kind}[{index}]` ids exactly where they were.
    for (index, item) in expect.refusal.iter().enumerate() {
        results.push(skip_or_eval(skip_semantic, "refusal", index, item.tier, || {
            eval_refusal_expectation(ctx, index, item)
        }));
    }
    results
}

/// Decision 6: when `skip_semantic` is set and `tier` is `Semantic`, returns
/// the `Skip` this entry gets instead of running `eval` at all - same id
/// (`expectations.{kind}[{index}]`) either way, so the report shape and the
/// entry's position never move, only whether it was actually evaluated.
fn skip_or_eval(
    skip_semantic: bool,
    kind: &str,
    index: usize,
    tier: Tier,
    eval: impl FnOnce() -> CheckResult,
) -> CheckResult {
    if skip_semantic && tier == Tier::Semantic {
        return CheckResult {
            id: format!("expectations.{kind}[{index}]").into(),
            outcome: Outcome::Skip(
                "tier = \"semantic\" and --skip-semantic-expectations was passed - this entry needs the \
                 plugin's semantic tier, which is assumed unavailable (decision 6)"
                    .to_string(),
            ),
            warnings: Vec::new(),
        };
    }
    eval()
}

// --- the three symbol_id-anchored tools -------------------------------------

/// Which of the three `SymbolQueryParams`-shaped handlers a `[[callers]]` /
/// `[[references]]` / `[[implementations]]` entry drives.
enum SymbolTool {
    Callers,
    References,
    Implementations,
}

impl SymbolTool {
    fn label(&self) -> &'static str {
        match self {
            SymbolTool::Callers => "callers",
            SymbolTool::References => "references",
            SymbolTool::Implementations => "implementations",
        }
    }

    /// Calls the real handler this tool's kind uses. `Implementations`
    /// dispatches through `find_implementations::dispatch` - the function
    /// `mcp::mod`'s `find_implementations` tool method calls - with
    /// `transitive: Some(false)`, which that function's own doc comment
    /// says defers *entirely* to the unmodified single-hop `handle`, so this
    /// is not a second, weaker path into that module.
    fn call(&self, ctx: &EvalContext, params: SymbolQueryParams) -> Result<ToolOutcome, String> {
        let result = match self {
            SymbolTool::Callers => {
                find_callers_callees::handle_callers(ctx.conn, ctx.embedding, ctx.capabilities, params)
            }
            SymbolTool::References => {
                find_references::handle(ctx.conn, ctx.embedding, ctx.capabilities, params)
            }
            SymbolTool::Implementations => {
                let SymbolQueryParams { symbol_id, symbol_name, cursor, limit, file_paths } = params;
                find_implementations::dispatch(
                    ctx.conn,
                    ctx.embedding,
                    ctx.capabilities,
                    FindImplementationsParams {
                        symbol_id,
                        symbol_name,
                        cursor,
                        limit,
                        file_paths,
                        transitive: Some(false),
                        max_depth: None,
                        resume_token: None,
                    },
                )
            }
        };
        tool_outcome(result)
    }
}

/// `find_definition`'s own one-line call, shared by [`resolve_definition`]
/// and decision 9's [`eval_refusal_expectation`] so both reach the handler
/// through the same parameters.
fn call_definition(ctx: &EvalContext, symbol_name: String) -> Result<ToolOutcome, String> {
    let params = FindDefinitionParams {
        symbol_name: Some(symbol_name),
        file_path: None,
        position: None,
        cursor: None,
        include_source: Some(false),
    };
    tool_outcome(find_definition::handle(ctx.conn, ctx.project_root, ctx.embedding, params))
}

fn eval_symbol_expectation(
    ctx: &EvalContext,
    kind: &str,
    index: usize,
    item: &SymbolExpectation,
    tool: SymbolTool,
) -> CheckResult {
    let id = format!("expectations.{kind}[{index}]");
    match resolve_and_call(ctx, &tool, &item.symbol, item.file.as_deref()) {
        Err(findings) => fail(id, context_lines(&item.symbol, item.file.as_deref(), findings)),
        Ok(value) => {
            if let Some(finding) = page_truncation_finding(&value, tool.label()) {
                return fail(id, vec![finding]);
            }
            let rows = rows_to_list(&value);
            // Decision 8, and only for the one tool of the three whose
            // contract is one row per answer. `Callers`/`References` walk
            // `Distinctness::Edges`, where a repeat is a second usage.
            let mut findings: Vec<String> = match tool {
                SymbolTool::Implementations => duplicate_row_finding(&rows).into_iter().collect(),
                SymbolTool::Callers | SymbolTool::References => Vec::new(),
            };
            // Decision 10, on all three tools rather than only one: unlike a
            // duplicate row, a provenance claim means the same thing
            // whichever of them made it.
            findings.extend(provenance_finding(&value, item.provenance.as_deref()));
            let actual: BTreeSet<String> = rows.into_iter().collect();
            let expected: BTreeSet<String> = item.expect.iter().cloned().collect();
            let outcome = outcome_with(findings, &expected, &actual);
            CheckResult { id: id.into(), outcome, warnings: Vec::new() }
        }
    }
}

/// Calls `tool` for `symbol`, resolving via the tool's own `symbol_name`
/// handling (decision 3). If that comes back as a candidate page, narrows
/// it by `file` and re-calls by the winning candidate's `symbol_id` -
/// genuinely the same handler, just anchored precisely, never a hand-built
/// answer. Returns the finished result JSON, or the findings to report
/// instead.
fn resolve_and_call(
    ctx: &EvalContext,
    tool: &SymbolTool,
    symbol: &str,
    file: Option<&str>,
) -> Result<Value, Vec<String>> {
    let limit = Some(pagination::MAX_PAGE_SIZE as u32);
    let first = SymbolQueryParams {
        symbol_id: None,
        symbol_name: Some(symbol.to_string()),
        cursor: None,
        limit,
        file_paths: None,
    };
    let value = tool_json(tool.call(ctx, first)).map_err(|e| vec![e])?;
    if !is_candidate_page(&value) {
        return Ok(value);
    }

    let candidate_id = {
        let candidate =
            pick_candidate(&value, file).map_err(|reason| candidate_failure(symbol, &value, &reason))?;
        candidate.get("id").and_then(Value::as_str).unwrap_or_default().to_string()
    };
    let retry = SymbolQueryParams {
        symbol_id: Some(candidate_id),
        symbol_name: None,
        cursor: None,
        limit,
        file_paths: None,
    };
    let retried = tool_json(tool.call(ctx, retry)).map_err(|e| vec![e])?;
    if is_candidate_page(&retried) {
        return Err(vec![
            "re-querying by the disambiguated candidate's own id still returned a candidate page - this \
             should not happen for an exact id lookup; the index may be inconsistent"
                .to_string(),
        ]);
    }
    Ok(retried)
}

// --- [[imports]] --------------------------------------------------------------

fn eval_imports_expectation(ctx: &EvalContext, index: usize, item: &ImportsExpectation) -> CheckResult {
    let id = format!("expectations.imports[{index}]");
    let value = match dependency_walk(ctx, &item.file, Direction::Outgoing) {
        Ok(value) => value,
        Err(err) => return fail(id, vec![format!("file = \"{}\"", item.file), err]),
    };
    if let Some(finding) = walk_truncation_finding(&value) {
        return fail(id, vec![finding]);
    }
    compare_walk_rows(id, &value, &item.expect, Vec::new())
}

// --- [[importers]] ------------------------------------------------------------

/// Decision 7. The same walk `[[imports]]` runs, in the other direction, plus
/// the one thing only this direction has: whether the anchor was taken
/// literally or substituted for the module the file defines
/// (`resolvedFrom.qualifiedName`, GM-356).
fn eval_importers_expectation(ctx: &EvalContext, index: usize, item: &ImportersExpectation) -> CheckResult {
    let id = format!("expectations.importers[{index}]");
    let value = match dependency_walk(ctx, &item.file, Direction::Incoming) {
        Ok(value) => value,
        Err(err) => return fail(id, vec![format!("file = \"{}\"", item.file), err]),
    };
    if let Some(finding) = walk_truncation_finding(&value) {
        return fail(id, vec![finding]);
    }
    let findings = match resolved_from_finding(&value, item.via_module.as_deref()) {
        None => Vec::new(),
        Some(finding) => vec![format!("file = \"{}\"", item.file), finding],
    };
    // A wrong anchor and a wrong set are reported together rather than one
    // hiding the other: when both are wrong the set is the more informative
    // half (an empty one is GM-356's own signature), and it costs a line
    // rather than a second run to have both.
    compare_walk_rows(id, &value, &item.expect, findings)
}

/// `[[imports]]`/`[[importers]]`' shared call into the real
/// `get_dependencies` handler - one hop, and everything else at this
/// module's own defaults.
fn dependency_walk(ctx: &EvalContext, file: &str, direction: Direction) -> Result<Value, String> {
    let params = GetDependenciesParams {
        file_path: Some(file.to_string()),
        module_id: None,
        direction,
        max_depth: Some(1),
        // Generous rather than exact: this is a conformance-kit fixture, not
        // a production repo, so nothing here relies on `get_dependencies`'
        // own `max_fanout` default (50) being right; asking wide and still
        // checking `truncated` (decision 4) is simpler than tuning it.
        max_fanout: Some(pagination::MAX_PAGE_SIZE as u32),
        resume_token: None,
    };
    tool_json(tool_outcome(get_dependencies::handle(ctx.conn, ctx.entry_points, params)))
}

/// The set comparison both walk categories share, with decision 8's
/// duplicate check in front of it and any findings the caller has already
/// collected kept ahead of both.
fn compare_walk_rows(id: String, value: &Value, expect: &[String], mut findings: Vec<String>) -> CheckResult {
    let rows = import_rows(value);
    findings.extend(duplicate_row_finding(&rows));
    let actual: BTreeSet<String> = rows.into_iter().collect();
    let expected: BTreeSet<String> = expect.iter().cloned().collect();
    CheckResult { id: id.into(), outcome: outcome_with(findings, &expected, &actual), warnings: Vec::new() }
}

/// Decision 7's `via_module`, both ways round: `None` when the response says
/// exactly what the entry claims, `Some(finding)` naming the disagreement
/// otherwise.
fn resolved_from_finding(value: &Value, via_module: Option<&str>) -> Option<String> {
    let actual = value.get("resolvedFrom");
    match (via_module, actual) {
        (None, None) => None,
        (Some(expected), Some(resolved))
            if resolved.get("qualifiedName").and_then(Value::as_str) == Some(expected) =>
        {
            None
        }
        (None, Some(resolved)) => Some(format!(
            "expected no substitution (no `via_module`), but the walk ran from another node: \
             resolvedFrom = {resolved}"
        )),
        (Some(expected), None) => Some(format!(
            "via_module = \"{expected}\", but the response carries no resolvedFrom - the walk ran \
             from the file itself, which outside TypeScript is the empty-by-construction answer \
             GM-356 is about (decision 7)"
        )),
        (Some(expected), Some(resolved)) => {
            Some(format!("via_module = \"{expected}\", but the walk ran from resolvedFrom = {resolved}"))
        }
    }
}

/// Decision 10's `provenance`, both ways round - deliberately the same
/// four-arm shape as [`resolved_from_finding`], because it is the same kind
/// of claim: a response field that is present exactly when the answer is
/// narrower than the question, asserted in both its present and its absent
/// state so that neither can drift unnoticed.
///
/// The `"silent"` sentinel and an omitted key mean the same thing (see
/// [`SymbolExpectation::provenance`]); both land in the `None` arms below.
fn provenance_finding(value: &Value, expected: Option<&str>) -> Option<String> {
    let expected = expected.filter(|e| *e != SILENT_PROVENANCE);
    let actual = value.get("provenance");
    match (expected, actual) {
        (None, None) => None,
        (Some(language), Some(block))
            if block.get("language").and_then(Value::as_str) == Some(language)
                && block.get("semanticTier").and_then(Value::as_str) == Some("absent") =>
        {
            None
        }
        (None, Some(block)) => Some(format!(
            "expected no provenance block - this plugin's semantic tier ran, so the response has \
             nothing to disclose - but the response carries provenance = {block} (decision 10)"
        )),
        (Some(language), None) => Some(format!(
            "provenance = \"{language}\", but the response carries no provenance block at all: it \
             claims its semantic tier contributed, which is the exact silence GM-382 exists to \
             remove (decision 10)"
        )),
        (Some(language), Some(block)) => Some(format!(
            "provenance = \"{language}\" with semanticTier = \"absent\", but the response says \
             provenance = {block}"
        )),
    }
}

/// The value [`SymbolExpectation::provenance`] accepts for "this response
/// must carry no provenance block" - the same requirement omitting the key
/// already has, written out so a fixture can say it on purpose rather than
/// only by saying nothing.
const SILENT_PROVENANCE: &str = "silent";

/// Decision 2's `[[imports]]`/`[[importers]]` row mapping, as a list - the
/// set is built from it, and decision 8's duplicate check needs the rows
/// before that collapse.
fn import_rows(value: &Value) -> Vec<String> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| match row.get("filePath").and_then(Value::as_str) {
            Some(file_path) => file_path.to_string(),
            None => {
                format!("container:{}", row.get("qualifiedName").and_then(Value::as_str).unwrap_or_default())
            }
        })
        .collect()
}

// --- [[definition]] -------------------------------------------------------------

fn eval_definition_expectation(ctx: &EvalContext, index: usize, item: &SymbolExpectation) -> CheckResult {
    let id = format!("expectations.definition[{index}]");
    match resolve_definition(ctx, &item.symbol, item.file.as_deref()) {
        Err(findings) => fail(id, context_lines(&item.symbol, item.file.as_deref(), findings)),
        Ok(value) => {
            let actual: BTreeSet<String> = match (
                value.get("filePath").and_then(Value::as_str),
                value.get("qualifiedName").and_then(Value::as_str),
            ) {
                (Some(file_path), Some(qualified_name)) => {
                    [format!("{file_path}:{qualified_name}")].into_iter().collect()
                }
                _ => BTreeSet::new(),
            };
            // Decision 10. `find_definition` never carries a block, so this
            // can only ever pass for `"silent"`/omitted - which is exactly
            // why it runs here rather than being skipped: an entry that
            // asked for a language id would otherwise be ignored silently.
            let findings: Vec<String> =
                provenance_finding(&value, item.provenance.as_deref()).into_iter().collect();
            let expected: BTreeSet<String> = item.expect.iter().cloned().collect();
            CheckResult {
                id: id.into(),
                outcome: outcome_with(findings, &expected, &actual),
                warnings: Vec::new(),
            }
        }
    }
}

/// `find_definition`'s own version of [`resolve_and_call`] - see decision 3
/// for why the disambiguated re-call is by exact `qualifiedName` rather than
/// by `symbol_id`: `find_definition` has no such parameter, unlike the other
/// four tools.
fn resolve_definition(ctx: &EvalContext, symbol: &str, file: Option<&str>) -> Result<Value, Vec<String>> {
    let call = |symbol_name: String| tool_json(call_definition(ctx, symbol_name));

    let value = call(symbol.to_string()).map_err(|e| vec![e])?;
    if !is_candidate_page(&value) {
        return Ok(value);
    }

    let qualified_name = {
        let candidate =
            pick_candidate(&value, file).map_err(|reason| candidate_failure(symbol, &value, &reason))?;
        candidate.get("qualifiedName").and_then(Value::as_str).unwrap_or_default().to_string()
    };
    let retried = call(qualified_name.clone()).map_err(|e| vec![e])?;
    if is_candidate_page(&retried) {
        return Err(vec![format!(
            "candidate qualifiedName '{qualified_name}' is still ambiguous after filtering by file - \
             find_definition has no symbol_id parameter to disambiguate further (decision 3); refine the \
             fixture so `file` narrows to a candidate whose qualifiedName is unique"
        )]);
    }
    Ok(retried)
}

// --- [[refusal]] (decision 9) ---------------------------------------------------

impl RefusedTool {
    /// The real tool's own name, so a finding reads as something a reader can
    /// go and call rather than as this file's category label.
    fn label(self) -> &'static str {
        match self {
            RefusedTool::Definition => "find_definition",
            RefusedTool::Callers => "find_callers",
            RefusedTool::References => "find_references",
            RefusedTool::Implementations => "find_implementations",
        }
    }

    /// Calls the real handler with `symbol_name` alone - decision 3's first
    /// rung, and the only one that can refuse. There is deliberately no
    /// candidate-narrowing re-call here (decision 9: nothing to disambiguate).
    fn call(self, ctx: &EvalContext, symbol: &str) -> Result<ToolOutcome, String> {
        let tool = match self {
            RefusedTool::Definition => return call_definition(ctx, symbol.to_string()),
            RefusedTool::Callers => SymbolTool::Callers,
            RefusedTool::References => SymbolTool::References,
            RefusedTool::Implementations => SymbolTool::Implementations,
        };
        tool.call(
            ctx,
            SymbolQueryParams {
                symbol_id: None,
                symbol_name: Some(symbol.to_string()),
                cursor: None,
                limit: Some(pagination::MAX_PAGE_SIZE as u32),
                file_paths: None,
            },
        )
    }
}

/// Decision 9. Passes on exactly one shape - a tool-level refusal carrying
/// every phrase the entry named - and says which of the other four it got
/// instead.
fn eval_refusal_expectation(ctx: &EvalContext, index: usize, item: &RefusalExpectation) -> CheckResult {
    let id = format!("expectations.refusal[{index}]");
    let label = item.tool.label();
    let findings = match item.tool.call(ctx, &item.symbol) {
        // Never a refusal, whatever it says: this is the arm a dead daemon or
        // an unreadable index lands in, and an entry that accepted it would
        // pass for reasons that have nothing to do with the code under test.
        Err(err) => Some(vec![format!(
            "{label} failed at the protocol level rather than refusing, which decision 9 never \
             accepts as a refusal: {err}"
        )]),
        Ok(ToolOutcome::Answer(value)) if is_candidate_page(&value) => {
            let mut findings = vec![format!(
                "{label} returned a candidate page rather than refusing - the name resolves to \
                 several declarations, which is ambiguity, not absence (decision 9)"
            )];
            findings.extend(candidate_lines(&value));
            Some(findings)
        }
        Ok(ToolOutcome::Answer(value)) => Some(vec![
            format!("{label} resolved the symbol and answered rather than refusing (decision 9)"),
            format!("it resolved to: {}", resolved_anchor_line(&value)),
        ]),
        Ok(ToolOutcome::Refusal(text)) => refusal_text_findings(&text, &item.contains),
    };
    match findings {
        None => CheckResult { id: id.into(), outcome: Outcome::Pass, warnings: Vec::new() },
        Some(findings) => {
            let mut lines = vec![format!("tool = \"{}\", symbol = \"{}\"", tool_key(item.tool), item.symbol)];
            lines.extend(findings);
            fail(id, lines)
        }
    }
}

/// The `tool` key as the fixture spells it - `label` names the handler, this
/// names the TOML value, and a failure wants both so the line can be found in
/// the file it came from.
fn tool_key(tool: RefusedTool) -> &'static str {
    match tool {
        RefusedTool::Definition => "definition",
        RefusedTool::Callers => "callers",
        RefusedTool::References => "references",
        RefusedTool::Implementations => "implementations",
    }
}

/// Decision 9's text assertion, as its own function so what it will and will
/// not accept is testable without a live index: `None` when `text` carries
/// every required phrase, `Some(findings)` naming each one it does not - and
/// quoting the refusal, since a fixture can only be fixed against what was
/// actually said.
///
/// An empty `contains` fails rather than passing vacuously. It is the one
/// degenerate shape `deny_unknown_fields` cannot catch (the key is present
/// and well-typed), and accepting it would turn the entry into "any refusal
/// will do" - which decision 9 exists to rule out.
fn refusal_text_findings(text: &str, contains: &[String]) -> Option<Vec<String>> {
    if contains.is_empty() {
        return Some(vec![
            "`contains` is empty, so this entry would accept any refusal at all - decision 9 \
             requires at least one phrase the refusal has to carry"
                .to_string(),
            format!("the tool refused with: {text}"),
        ]);
    }
    let missing: Vec<&str> = contains.iter().map(String::as_str).filter(|p| !text.contains(p)).collect();
    if missing.is_empty() {
        return None;
    }
    Some(vec![
        format!(
            "the tool refused, but its refusal is missing {} required phrase(s): {}",
            missing.len(),
            missing.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", ")
        ),
        format!("the tool refused with: {text}"),
    ])
}

/// What an answer says it anchored on, for a `[[refusal]]` entry that got one:
/// the `anchor` wrapper the four edge-walking tools carry, or
/// `find_definition`'s own top-level node, which has no wrapper at all
/// (decision 3 case 1).
fn resolved_anchor_line(value: &Value) -> String {
    let node = value.get("anchor").unwrap_or(value);
    format!(
        "{}:{}",
        node.get("filePath").and_then(Value::as_str).unwrap_or("?"),
        node.get("qualifiedName").and_then(Value::as_str).unwrap_or("?")
    )
}

// --- shared: candidate pages, JSON extraction, set diffs ----------------------

/// Decision 4 for the three `results`-shaped tools: `None` when the page is
/// complete, `Some(finding)` when it reported `hasMore: true` even at the
/// maximum limit and must not be compared.
fn page_truncation_finding(value: &Value, tool_label: &str) -> Option<String> {
    (value.get("hasMore").and_then(Value::as_bool) == Some(true)).then(|| {
        format!(
            "{tool_label} reported hasMore: true even at limit={} (pagination::MAX_PAGE_SIZE) - decision 4: \
             a partial page must never be compared; narrow the fixture",
            pagination::MAX_PAGE_SIZE
        )
    })
}

/// Decision 4 for `get_dependencies`: the same rule as
/// [`page_truncation_finding`], over that tool's own `truncated`/
/// `truncatedBy` fields instead of `hasMore`.
fn walk_truncation_finding(value: &Value) -> Option<String> {
    (value.get("truncated").and_then(Value::as_bool) == Some(true)).then(|| {
        let cause = value.get("truncatedBy").and_then(Value::as_str).unwrap_or("unknown");
        format!(
            "imports reported truncated: true, truncatedBy: {cause:?} - decision 4: a partial dependency \
             walk must never be compared; narrow the fixture"
        )
    })
}

/// A top-level `"resolvedBy"` key *plus* a top-level `"results"` array marks
/// a candidate/suggestion page - see `mcp::find_definition::CandidatePage`/
/// `FileNamePage`. Neither field alone is enough to tell a candidate page
/// from a real answer, for two different reasons on the two shapes of real
/// answer this module calls:
///
/// - The four edge-walking tools' ordinary result page has a top-level
///   `"results"` too, but never a top-level `"resolvedBy"` - that field only
///   ever appears nested under `"anchor"` there (`mcp::anchor::AnchorInfo`),
///   which is why requiring `resolvedBy` at the top level alone already
///   ruled these out.
/// - `find_definition`'s own *direct*, unambiguous answer
///   (`mcp::find_definition::DefinitionNode`) is the one case that field
///   alone gets wrong: it echoes which rung resolved it as a top-level
///   `resolvedBy` (`Id`/`QualifiedName`/`Name`) exactly the way a candidate
///   page does, but it is the resolved node itself, not a list of
///   candidates to pick from - and, tellingly, it has no `"results"` field
///   at all. Requiring both together is what a candidate page has that a
///   direct answer never does, on every one of the five handlers.
fn is_candidate_page(value: &Value) -> bool {
    value.get("resolvedBy").is_some() && value.get("results").is_some_and(Value::is_array)
}

/// Picks the one candidate `file` names, or explains why it can't - decision
/// 3. Never guesses when `file` is absent or does not narrow to exactly one.
fn pick_candidate<'a>(value: &'a Value, file: Option<&str>) -> Result<&'a Value, String> {
    let results = value.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let Some(file) = file else {
        return Err("ambiguous, and no `file` was given to disambiguate".to_string());
    };
    let matches: Vec<&Value> =
        results.iter().filter(|c| c.get("filePath").and_then(Value::as_str) == Some(file)).collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!("no candidate has filePath == \"{file}\"")),
        n => Err(format!("{n} candidates still share filePath == \"{file}\"")),
    }
}

/// The findings for a candidate page that could not be resolved to one
/// answer: why (`reason`, already naming `file` when one was given - see
/// [`pick_candidate`]), plus every candidate's identifying fields, so a
/// human (or the wrong-expectation test) can see exactly what was on offer.
fn candidate_failure(symbol: &str, value: &Value, reason: &str) -> Vec<String> {
    let resolved_by = value.get("resolvedBy").and_then(Value::as_str).unwrap_or("unknown");
    let mut findings =
        vec![format!("'{symbol}' did not resolve to one symbol (resolvedBy = {resolved_by}): {reason}")];
    findings.extend(candidate_lines(value));
    findings
}

/// Every candidate a page offered, one identifying line each - shared by
/// [`candidate_failure`] and decision 9's own candidate-page arm, which
/// report the same list under different headlines.
fn candidate_lines(value: &Value) -> Vec<String> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|candidate| {
            format!(
                "candidate: id={} qualifiedName={} filePath={} kind={}",
                candidate.get("id").and_then(Value::as_str).unwrap_or("?"),
                candidate.get("qualifiedName").and_then(Value::as_str).unwrap_or("?"),
                candidate.get("filePath").and_then(Value::as_str).unwrap_or("?"),
                candidate.get("kind").and_then(Value::as_str).unwrap_or("?"),
            )
        })
        .collect()
}

/// What a handler said, with the one distinction the categories that compare
/// sets do not need and decision 9 lives on: a tool that *declined* to answer
/// is not the same event as a tool that could not be reached.
///
/// The outer `Err` of the `Result` this is carried in keeps its old meaning
/// and `mcp::anchor`'s - "genuine protocol-level failures", plus this
/// module's own two ways of getting an unusable body (no text content, text
/// that is not JSON). Those are never a refusal, whatever they say.
#[derive(Debug)]
enum ToolOutcome {
    /// `is_error` unset or false: the parsed wire JSON. May still be a
    /// candidate page - [`is_candidate_page`] is a separate question.
    Answer(Value),
    /// `is_error: true`: a tool-level refusal, carrying prose rather than
    /// JSON ("g-mesh: no symbol named 'X' found", GM-367's import-only
    /// message, a bad parameter combination).
    Refusal(String),
}

/// Extracts an MCP tool's answer, keeping [`ToolOutcome`]'s distinction:
/// `ErrorData` and an unusable body stay `Err`, `is_error: true` becomes
/// `Refusal` with the prose it carried, and anything else is parsed as JSON.
fn tool_outcome(result: Result<CallToolResult, ErrorData>) -> Result<ToolOutcome, String> {
    let result = result.map_err(|e| e.to_string())?;
    let text = match result.content.first() {
        Some(ContentBlock::Text(text)) => text.text.clone(),
        other => return Err(format!("tool returned no text content: {other:?}")),
    };
    if result.is_error == Some(true) {
        return Ok(ToolOutcome::Refusal(text));
    }
    serde_json::from_str(&text)
        .map(ToolOutcome::Answer)
        .map_err(|e| format!("tool returned content that is not JSON: {e}"))
}

/// The view every category except `[[refusal]]` wants: a refusal is simply a
/// failure with the tool's own words in it, exactly as it was before
/// [`ToolOutcome`] existed. Only decision 9 needs the two apart.
fn tool_json(outcome: Result<ToolOutcome, String>) -> Result<Value, String> {
    match outcome? {
        ToolOutcome::Answer(value) => Ok(value),
        ToolOutcome::Refusal(text) => Err(text),
    }
}

/// Decision 2's `[[callers]]`/`[[references]]`/`[[implementations]]` row
/// mapping, as a list - the set is built from it, and decision 8's duplicate
/// check needs the rows before that collapse.
fn rows_to_list(value: &Value) -> Vec<String> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            let file_path = row.get("filePath").and_then(Value::as_str).unwrap_or_default();
            let qualified_name = row.get("qualifiedName").and_then(Value::as_str).unwrap_or_default();
            format!("{file_path}:{qualified_name}")
        })
        .collect()
}

/// Decision 8: `None` when every row is its own answer, `Some(finding)`
/// naming each repeated row and how many times it came back. Only called for
/// the categories whose tool promises one row per answer - see decision 8 for
/// why `[[callers]]`/`[[references]]` are exempt rather than overlooked.
fn duplicate_row_finding(rows: &[String]) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows {
        *counts.entry(row.as_str()).or_default() += 1;
    }
    let repeated: Vec<String> =
        counts.iter().filter(|(_, n)| **n > 1).map(|(row, n)| format!("{row} (x{n})")).collect();
    (!repeated.is_empty()).then(|| {
        format!(
            "{} row(s) came back more than once, and for this category a repeat is the same answer \
             twice rather than a second fact (decision 8): {}",
            repeated.len(),
            repeated.join(", ")
        )
    })
}

/// Folds decision 7's and decision 8's findings into decision 2's set diff.
/// A finding alongside a *matching* set still fails, and says so in as many
/// words: otherwise the report reads as a set mismatch and sends a reader
/// looking for one that isn't there.
fn outcome_with(
    mut findings: Vec<String>,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> Outcome {
    match set_diff_outcome(expected, actual) {
        Outcome::Pass if findings.is_empty() => Outcome::Pass,
        Outcome::Pass => {
            findings.push(format!("the set itself matched: {{{}}}", format_set(actual)));
            Outcome::Fail(findings)
        }
        Outcome::Fail(diff) => {
            findings.extend(diff);
            Outcome::Fail(findings)
        }
        // `set_diff_outcome` returns only `Pass`/`Fail`; kept total rather
        // than `unreachable!` so a third outcome can never be lost here.
        other => other,
    }
}

fn format_set(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        return "(none)".to_string();
    }
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Compares `expected` against `actual` as sets and, on any difference,
/// prints both sets in full plus the two one-sided diffs by name - what the
/// acceptance criteria call "a diff of expected vs actual sets".
fn set_diff_outcome(expected: &BTreeSet<String>, actual: &BTreeSet<String>) -> Outcome {
    if expected == actual {
        return Outcome::Pass;
    }
    let missing: Vec<String> = expected.difference(actual).cloned().collect();
    let extra: Vec<String> = actual.difference(expected).cloned().collect();
    let mut findings = vec![
        format!("expected: {{{}}}", format_set(expected)),
        format!("actual:   {{{}}}", format_set(actual)),
    ];
    if !missing.is_empty() {
        findings.push(format!("missing (expected, not found): {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        findings.push(format!("extra (found, not expected): {}", extra.join(", ")));
    }
    Outcome::Fail(findings)
}

fn context_lines(symbol: &str, file: Option<&str>, findings: Vec<String>) -> Vec<String> {
    let mut out = vec![format!(
        "symbol = \"{symbol}\"{}",
        file.map(|f| format!(", file = \"{f}\"")).unwrap_or_default()
    )];
    out.extend(findings);
    out
}

fn fail(id: String, findings: Vec<String>) -> CheckResult {
    CheckResult { id: id.into(), outcome: Outcome::Fail(findings), warnings: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sets_pass_regardless_of_order() {
        let a: BTreeSet<String> = ["a", "b"].into_iter().map(String::from).collect();
        let b: BTreeSet<String> = ["b", "a"].into_iter().map(String::from).collect();
        assert_eq!(set_diff_outcome(&a, &b), Outcome::Pass);
    }

    /// The diff test's core assertion, in miniature: one missing, one extra
    /// entry, both named in the findings.
    #[test]
    fn a_mismatched_set_names_both_the_missing_and_the_extra_entry() {
        let expected: BTreeSet<String> = ["src/a.ts:f", "src/b.ts:g"].into_iter().map(String::from).collect();
        let actual: BTreeSet<String> = ["src/a.ts:f", "src/c.ts:h"].into_iter().map(String::from).collect();
        let Outcome::Fail(findings) = set_diff_outcome(&expected, &actual) else {
            panic!("expected a failure");
        };
        let text = findings.join("\n");
        assert!(text.contains("missing (expected, not found): src/b.ts:g"), "{text}");
        assert!(text.contains("extra (found, not expected): src/c.ts:h"), "{text}");
        assert!(text.contains("expected: {src/a.ts:f, src/b.ts:g}"), "{text}");
        assert!(text.contains("actual:   {src/a.ts:f, src/c.ts:h}"), "{text}");
    }

    #[test]
    fn a_real_result_page_is_not_mistaken_for_a_candidate_page() {
        let page = serde_json::json!({
            "anchor": {"id": "n1", "qualifiedName": "f", "kind": "Function", "filePath": "a.ts", "startLine": 1},
            "results": [],
            "hasMore": false,
            "nextCursor": null,
            "allUnresolved": false,
        });
        assert!(!is_candidate_page(&page));
    }

    /// The regression this module actually hit end to end: `find_definition`'s
    /// own direct, unambiguous answer (`DefinitionNode`) echoes a top-level
    /// `resolvedBy` too - "Name"/"QualifiedName"/"Id" - but has no top-level
    /// `results` array, unlike a real candidate page. Checking `resolvedBy`
    /// alone misclassified this shape as a candidate page and refused every
    /// unambiguous `[[definition]]` expectation.
    #[test]
    fn a_find_definition_direct_answer_is_not_mistaken_for_a_candidate_page() {
        let node = serde_json::json!({
            "id": "n1",
            "kind": "Function",
            "name": "format",
            "qualifiedName": "format",
            "filePath": "src/overload.ts",
            "startLine": 1,
            "startCol": 0,
            "endLine": 3,
            "endCol": 1,
            "resolvedBy": "qualifiedName",
        });
        assert!(!is_candidate_page(&node));
    }

    #[test]
    fn a_candidate_page_is_recognized_by_its_top_level_resolved_by() {
        let page = serde_json::json!({
            "ambiguous": true,
            "resolvedBy": "nameAmbiguous",
            "results": [{"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"}],
            "hasMore": false,
            "nextCursor": null,
        });
        assert!(is_candidate_page(&page));
    }

    #[test]
    fn pick_candidate_refuses_without_a_file_even_with_one_candidate() {
        let page = serde_json::json!({
            "resolvedBy": "nameAmbiguous",
            "results": [{"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"}],
        });
        assert!(pick_candidate(&page, None).is_err());
    }

    #[test]
    fn pick_candidate_narrows_by_file_to_exactly_one() {
        let page = serde_json::json!({
            "resolvedBy": "nameAmbiguous",
            "results": [
                {"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
                {"id": "n2", "qualifiedName": "f", "filePath": "b.ts", "kind": "Function"},
            ],
        });
        let picked = pick_candidate(&page, Some("b.ts")).unwrap();
        assert_eq!(picked.get("id").and_then(Value::as_str), Some("n2"));
    }

    #[test]
    fn pick_candidate_refuses_a_file_matching_more_than_one_candidate() {
        let page = serde_json::json!({
            "resolvedBy": "nameAmbiguous",
            "results": [
                {"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
                {"id": "n2", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
            ],
        });
        assert!(pick_candidate(&page, Some("a.ts")).is_err());
    }

    #[test]
    fn rows_to_set_blanks_the_qualified_name_of_a_file_level_row() {
        let value = serde_json::json!({
            "results": [
                {"filePath": "a.ts", "qualifiedName": "f"},
                {"filePath": "b.ts"},
            ],
        });
        let set: BTreeSet<String> = rows_to_list(&value).into_iter().collect();
        assert!(set.contains("a.ts:f"), "{set:?}");
        assert!(set.contains("b.ts:"), "{set:?}");
    }

    #[test]
    fn import_rows_to_set_prefixes_a_targetless_row_with_container() {
        let value = serde_json::json!({
            "results": [
                {"filePath": "a.ts", "kind": "File"},
                {"qualifiedName": "react", "kind": "Module"},
            ],
        });
        let set: BTreeSet<String> = import_rows(&value).into_iter().collect();
        assert!(set.contains("a.ts"), "{set:?}");
        assert!(set.contains("container:react"), "{set:?}");
    }

    /// Decision 7's parsing half: `[[importers]]` is its own list, `via_module`
    /// is optional, and `deny_unknown_fields` keeps it out of `[[imports]]` -
    /// the property that made a separate list worth having over a `direction`
    /// key.
    #[test]
    fn importers_parse_with_and_without_via_module_and_imports_reject_it() {
        let file: ExpectFile = toml::from_str(
            "[[importers]]\nfile = \"a.ts\"\nexpect = [\"b.ts\"]\n\n\
             [[importers]]\nfile = \"pkg/helpers.py\"\nvia_module = \"pkg.helpers\"\nexpect = []\n",
        )
        .unwrap();
        assert_eq!(file.importers.len(), 2);
        assert_eq!(file.importers[0].via_module, None);
        assert_eq!(file.importers[1].via_module.as_deref(), Some("pkg.helpers"));
        assert_eq!(file.importers[0].tier, Tier::Structural);

        let err =
            toml::from_str::<ExpectFile>("[[imports]]\nfile = \"a.ts\"\nvia_module = \"a\"\nexpect = []\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("via_module") || err.contains("unknown field"), "{err}");
    }

    /// Decision 7: `via_module` is checked both ways round. The case that
    /// matters is the third - a response with no `resolvedFrom` against an
    /// entry that named a module is GM-356's own defect, and it must not read
    /// as "not checked".
    #[test]
    fn via_module_is_an_assertion_in_both_of_its_states() {
        let substituted = serde_json::json!({
            "results": [],
            "resolvedFrom": {"requested": "pkg/helpers.py", "qualifiedName": "pkg.helpers"},
        });
        let literal = serde_json::json!({"results": []});

        assert!(resolved_from_finding(&substituted, Some("pkg.helpers")).is_none());
        assert!(resolved_from_finding(&literal, None).is_none());

        let missing = resolved_from_finding(&literal, Some("pkg.helpers")).expect("must flag it");
        assert!(missing.contains("no resolvedFrom"), "{missing}");

        let unexpected = resolved_from_finding(&substituted, None).expect("must flag it");
        assert!(unexpected.contains("expected no substitution"), "{unexpected}");

        let wrong = resolved_from_finding(&substituted, Some("pkg.other")).expect("must flag it");
        assert!(wrong.contains("pkg.helpers"), "{wrong}");
    }

    /// Decision 8: the duplicate GM-361's de-duplication removed, which a
    /// `BTreeSet` cannot see. Two rows of one implementor is one answer twice.
    #[test]
    fn a_repeated_row_is_named_with_its_count() {
        let rows: Vec<String> = ["a.rs:A", "a.rs:A", "b.rs:B", "a.rs:C", "a.rs:C", "a.rs:C"]
            .into_iter()
            .map(String::from)
            .collect();
        let finding = duplicate_row_finding(&rows).expect("must flag the repeats");
        assert!(finding.contains("a.rs:A (x2)"), "{finding}");
        assert!(finding.contains("a.rs:C (x3)"), "{finding}");
        assert!(!finding.contains("b.rs:B"), "{finding}");

        assert!(duplicate_row_finding(&["a.rs:A".to_string(), "b.rs:B".to_string()]).is_none());
        assert!(duplicate_row_finding(&[]).is_none());
    }

    /// Decision 8's reporting rule: a duplicate fails the entry *even when the
    /// set matches*, and says the set matched - otherwise the failure reads as
    /// a set mismatch and sends a reader hunting for one that is not there.
    #[test]
    fn a_finding_against_a_matching_set_fails_and_says_the_set_matched() {
        let expected: BTreeSet<String> = ["a.rs:A"].into_iter().map(String::from).collect();
        let actual = expected.clone();
        let Outcome::Fail(findings) = outcome_with(vec!["dup".to_string()], &expected, &actual) else {
            panic!("a finding must fail the entry");
        };
        let text = findings.join("\n");
        assert!(text.contains("dup"), "{text}");
        assert!(text.contains("the set itself matched: {a.rs:A}"), "{text}");

        assert_eq!(outcome_with(Vec::new(), &expected, &actual), Outcome::Pass);
    }

    /// Decision 4: a page reporting `hasMore: true` even at the maximum
    /// limit must fail rather than be silently compared as if complete.
    #[test]
    fn a_page_reporting_has_more_is_never_treated_as_complete() {
        let page = serde_json::json!({"results": [], "hasMore": true});
        let finding = page_truncation_finding(&page, "callers").expect("must flag an incomplete page");
        assert!(finding.contains("hasMore: true"), "{finding}");
        assert!(finding.contains(&pagination::MAX_PAGE_SIZE.to_string()), "{finding}");
    }

    #[test]
    fn a_complete_page_is_not_flagged_as_truncated() {
        let page = serde_json::json!({"results": [], "hasMore": false});
        assert!(page_truncation_finding(&page, "callers").is_none());
    }

    #[test]
    fn a_truncated_dependency_walk_is_never_treated_as_complete() {
        let walk = serde_json::json!({"results": [], "truncated": true, "truncatedBy": "maxFanout"});
        let finding = walk_truncation_finding(&walk).expect("must flag a truncated walk");
        assert!(finding.contains("maxFanout"), "{finding}");
    }

    #[test]
    fn an_untruncated_dependency_walk_is_not_flagged() {
        let walk = serde_json::json!({"results": [], "truncated": false});
        assert!(walk_truncation_finding(&walk).is_none());
    }

    /// Decision 6, the parsing half: `tier` defaults to `Structural` when
    /// absent, and a `[[callers]]` entry that does spell `tier = "semantic"`
    /// parses to `Tier::Semantic` - the two states `skip_or_eval` branches
    /// on.
    #[test]
    fn tier_defaults_to_structural_and_parses_semantic_when_given() {
        let file: ExpectFile = toml::from_str(
            "[[callers]]\nsymbol = \"a\"\nexpect = []\n\n\
             [[callers]]\nsymbol = \"b\"\nexpect = []\ntier = \"semantic\"\n",
        )
        .unwrap();
        assert_eq!(file.callers[0].tier, Tier::Structural);
        assert_eq!(file.callers[1].tier, Tier::Semantic);
    }

    /// Decision 6: with `skip_semantic = true`, a `Semantic`-tier entry never
    /// calls `eval` at all (it would panic if it did) and reports `Skip`
    /// under the same id `eval_symbol_expectation` would have used; a
    /// `Structural` one is unaffected.
    #[test]
    fn skip_or_eval_skips_only_the_semantic_tier_entries() {
        let skipped = skip_or_eval(true, "callers", 2, Tier::Semantic, || panic!("must not run"));
        assert_eq!(skipped.id, "expectations.callers[2]");
        assert!(matches!(skipped.outcome, Outcome::Skip(_)), "{:?}", skipped.outcome);

        let ran = skip_or_eval(true, "callers", 0, Tier::Structural, || CheckResult {
            id: "expectations.callers[0]".into(),
            outcome: Outcome::Pass,
            warnings: Vec::new(),
        });
        assert_eq!(ran.outcome, Outcome::Pass);

        let ran_without_the_flag = skip_or_eval(false, "callers", 2, Tier::Semantic, || CheckResult {
            id: "expectations.callers[2]".into(),
            outcome: Outcome::Pass,
            warnings: Vec::new(),
        });
        assert_eq!(ran_without_the_flag.outcome, Outcome::Pass);
    }

    /// Decision 9's root cause, pinned at the exact place it was lost:
    /// `tool_json` collapsed a tool-level refusal and a protocol-level failure
    /// into one `Err(String)`, so nothing downstream could tell "this name is
    /// not a declaration" from "the call broke" - which is why decision 2's
    /// zero-element branch was unreachable. [`tool_outcome`] keeps them apart.
    #[test]
    fn a_tool_level_refusal_is_a_refusal_and_a_protocol_error_is_not() {
        let refusal = CallToolResult::error(vec![ContentBlock::text("g-mesh: no symbol named 'x' found")]);
        let Ok(ToolOutcome::Refusal(text)) = tool_outcome(Ok(refusal)) else {
            panic!("is_error: true must be a Refusal, not an Err");
        };
        assert_eq!(text, "g-mesh: no symbol named 'x' found");

        let answer = CallToolResult::success(vec![ContentBlock::text("{\"results\":[]}")]);
        let Ok(ToolOutcome::Answer(value)) = tool_outcome(Ok(answer)) else {
            panic!("a successful result must parse as an Answer");
        };
        assert!(value.get("results").is_some(), "{value}");

        // The arm an entry must never accept: the handler never produced a
        // result at all.
        let protocol: Result<CallToolResult, ErrorData> =
            Err(ErrorData::internal_error("g-mesh: the index is gone".to_string(), None));
        assert!(tool_outcome(protocol).is_err(), "a protocol-level failure must stay an Err");
    }

    /// The four categories that compare sets must be unaffected by the split:
    /// a refusal is still exactly the `Err(text)` they have always reported.
    #[test]
    fn tool_json_still_reports_a_refusal_as_a_plain_error_string() {
        let refusal = CallToolResult::error(vec![ContentBlock::text("g-mesh: nope")]);
        assert_eq!(tool_json(tool_outcome(Ok(refusal))), Err("g-mesh: nope".to_string()));
    }

    /// Decision 9's text assertion: every phrase must be present, each missing
    /// one is named, and the refusal itself is quoted so a fixture can be
    /// fixed against what was said rather than against a guess.
    #[test]
    fn a_refusal_must_carry_every_phrase_the_entry_named() {
        let text = "g-mesh: nothing named 'strings' is declared in this project. It names something \
                    this project imports: 'strings' (1) - 1 import record(s), which have no definition \
                    site here. For what a file imports, or what imports it, use get_dependencies.";
        let phrases = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert!(refusal_text_findings(text, &phrases(&["is declared in this project", "get_dependencies"]))
            .is_none());

        let findings = refusal_text_findings(text, &phrases(&["is declared in this project", "ghost"]))
            .expect("a missing phrase must fail the entry");
        let joined = findings.join("\n");
        assert!(joined.contains("missing 1 required phrase(s): \"ghost\""), "{joined}");
        assert!(joined.contains("use get_dependencies"), "the refusal itself must be quoted: {joined}");
    }

    /// The degenerate shape `deny_unknown_fields` cannot catch: `contains` is
    /// present and well-typed but empty, which would make the entry accept any
    /// refusal at all - including one about a parameter mistake. Decision 9
    /// fails it instead.
    #[test]
    fn an_empty_contains_never_passes_vacuously() {
        let findings = refusal_text_findings("g-mesh: anything at all", &[])
            .expect("an empty `contains` must fail rather than accept everything");
        assert!(findings.join("\n").contains("`contains` is empty"), "{findings:?}");
    }

    /// Decision 9's `[[refusal]]` parses with its three required keys, rejects
    /// a `file` companion (there is nothing to disambiguate), and its
    /// `contains` cannot appear on a `[[callers]]` entry - the property that
    /// made a separate struct worth having, exactly as decision 7 argued for
    /// `via_module`.
    #[test]
    fn refusal_entries_parse_and_their_keys_stay_out_of_the_other_categories() {
        let file: ExpectFile = toml::from_str(
            "[[refusal]]\ntool = \"definition\"\nsymbol = \"strings\"\ncontains = [\"imports\"]\n\n\
             [[refusal]]\ntool = \"references\"\nsymbol = \"ghost\"\ncontains = [\"no symbol named\"]\n\
             tier = \"semantic\"\n",
        )
        .unwrap();
        assert_eq!(file.refusal.len(), 2);
        assert_eq!(file.refusal[0].tool, RefusedTool::Definition);
        assert_eq!(file.refusal[0].tier, Tier::Structural);
        assert_eq!(file.refusal[1].tool, RefusedTool::References);
        assert_eq!(file.refusal[1].tier, Tier::Semantic);

        for (source, needle) in [
            ("[[refusal]]\ntool = \"definition\"\nsymbol = \"x\"\ncontains = []\nfile = \"a.rs\"\n", "file"),
            ("[[callers]]\nsymbol = \"x\"\nexpect = []\ncontains = [\"y\"]\n", "contains"),
            ("[[refusal]]\ntool = \"imports\"\nsymbol = \"x\"\ncontains = [\"y\"]\n", "imports"),
        ] {
            let err = toml::from_str::<ExpectFile>(source).unwrap_err().to_string();
            assert!(err.contains(needle), "expected {needle:?} to be rejected, got: {err}");
        }

        // `contains` is required, not defaulted - an entry that forgot it is a
        // parse error rather than an entry that accepts any refusal.
        let err = toml::from_str::<ExpectFile>("[[refusal]]\ntool = \"callers\"\nsymbol = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("contains"), "{err}");
    }

    /// The line a `[[refusal]]` prints when it was answered instead, on both
    /// shapes of answer decision 3 case 1 describes: the four edge-walking
    /// tools wrap the node in `anchor`, `find_definition` returns it bare.
    #[test]
    fn an_answered_refusal_names_what_it_resolved_to_on_either_shape() {
        let walk = serde_json::json!({
            "anchor": {"id": "n1", "qualifiedName": "helper", "filePath": "a.fk", "kind": "Function"},
            "results": [],
        });
        assert_eq!(resolved_anchor_line(&walk), "a.fk:helper");

        let definition = serde_json::json!({"qualifiedName": "helper", "filePath": "a.fk"});
        assert_eq!(resolved_anchor_line(&definition), "a.fk:helper");
    }

    #[test]
    fn an_unparsable_expect_file_reports_a_readable_parse_error() {
        let dir = std::env::temp_dir().join(format!("g-mesh-expectations-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("expect.toml");
        std::fs::write(&path, "[[callers]]\nsymbol = \"f\"\nexpect = [\"a.ts:f\"]\nbogus_key = true\n")
            .unwrap();
        let err = parse(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("bogus_key") || text.contains("unknown field"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
