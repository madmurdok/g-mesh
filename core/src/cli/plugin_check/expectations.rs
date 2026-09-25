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
//!   present. A row the linker could not confirm takes a leading
//!   `unresolved:` marker - decision 11, which has why the row's own
//!   `resolved` bit is spelled inside the row rather than in a key beside
//!   it; the `split_once(':')` reading then applies to what follows the
//!   marker, exactly as it already does after `[[imports]]`' `container:`.
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
//!
//! # Decision 11: a row's own `resolved` bit, and `allUnresolved` (GM-386)
//!
//! Decisions 4 and 10 made two of the six promises the shipped agent
//! guidance (`cli::agent_instructions`) actually tells a caller to rely on
//! into executable expectations: `hasMore: false`/`truncated: false`, and
//! which tier answered. The other four - a row's own `resolved` flag, the
//! response-level `allUnresolved` marker, the `files` tally (decision 12)
//! and `excludedReferences` (decision 13) - were checked nowhere but core's
//! own unit tests against hand-built indexes. That is GM-380's shape one
//! layer out: a plugin that started leaving a same-file edge
//! `resolved: false` would break the promise in all four languages and still
//! pass every conformance suite, because decision 2's answer format reads
//! `filePath` and `qualifiedName` and no other field of a row.
//!
//! **A row's `resolved` flag is spelled inside the row, not in a key beside
//! it.** A row the linker could not confirm renders as
//! `"unresolved:{filePath}:{qualifiedName}"`. Two properties follow, and
//! they are why this beat a second list (`unresolved = [...]`) alongside
//! `expect`:
//!
//! - **Every entry already written asserts the healthy case, unedited** -
//!   exactly what decision 10's omitted `provenance` buys. No bundled
//!   fixture spells the marker, so every one of them now requires every row
//!   it names to be a confirmed edge.
//! - **One list means the two facts cannot drift apart.** A separate
//!   `unresolved` list would have to decide whether its entries *also*
//!   belong in `expect`, and under either answer a fixture naming a row in
//!   one but not the other means something a reader has to look up.
//!
//! The cost is that an unresolved row carries two colons rather than one.
//! A row whose `resolved` field is missing altogether is treated as
//! unresolved rather than assumed healthy: all three row types declare it
//! non-optional, so its absence is itself a regression, and the reading that
//! reports is better than the one that hides.
//!
//! **`allUnresolved: true` fails the entry outright, with no key to opt out
//! of it** - decision 4's rule, for decision 4's reason. The marker means
//! every row of a non-empty page came from an edge the linker could not
//! confirm (`pagination::Page::all_unresolved`): the shape that reads as an
//! ordinary complete page and is not. Comparing a set against it would pass
//! on an answer nothing stands behind. A response carrying no
//! `allUnresolved` field at all fails for the same reason its rows' missing
//! `resolved` does.
//!
//! The marker is not made redundant by the row prefix. A page where every
//! row is unresolved *and* whose fixture spells every row with the prefix
//! matches as a set, and is caught here alone - which is the arm this
//! decision's own control was built on.
//!
//! **How far a plugin can move a row's `resolved` bit, measured rather than
//! assumed.** GM-386's controls were run by wrapping the bundled Go plugin
//! in a shim that rewrote its own stdout, so the violating arm was genuinely
//! a *plugin* rather than a patched core. Flipping the bit does reach the
//! index (`storage::write::apply_diff` upserts `resolved =
//! excluded.resolved`), but on the two shapes a conformant plugin can
//! actually emit, something else gets there first:
//!
//! - **A same-file edge sent `resolved: false`** is what the built-in
//!   `same-file-rule` check already reports, off the stream, before
//!   expectations are evaluated at all (`checks::same_file_violation`). Run
//!   against this kit's Go fixture it fails that check *and* two entries
//!   here, whose diffs name `unresolved:helper.go:init` and the three
//!   `unresolved:server/server.go:...` rows.
//! - **A cross-file edge** cannot be sent pointing at the far file's
//!   declaration at all - `stream-order` requires both endpoints to be nodes
//!   of the edge's own file - so it travels as a placeholder, and a
//!   placeholder edge the linker repoints is set `resolved = 1` by
//!   `graph::symbol_links` whatever the plugin said. Measured: flipping all
//!   ten edges the Go plugin's `go/types` tier answers with changes nothing
//!   in the index, because every one of them lands on a placeholder core
//!   then resolves.
//!
//! So an `unresolved:` row is today a *second* fence rather than the first,
//! and that is worth saying out loud rather than leaving a reader to assume
//! this check is the only thing standing between a plugin and a wrong
//! answer. It is still the fence in the right place: the two checks above
//! read the stream, this one reads what the MCP tool finally *answers*,
//! which is the promise `cli::agent_instructions` actually makes to a
//! caller; and a shape neither stream rule covers - a semantic tier allowed
//! to name a cross-file declaration directly, which no bundled plugin emits
//! today - would land here and nowhere else.
//!
//! # Decision 12: the `files` tally, present and absent (GM-386)
//!
//! `find_callers`/`find_references` attach `files` exactly when it says
//! something `results` does not: the page is incomplete, or its rows repeat
//! a file (`pagination::tally_is_worth_sending`). Decision 4 has already
//! ruled the first half out here, so within this kit the field is present
//! precisely when the rows repeat a file - which makes both of its states
//! worth pinning:
//!
//! - **Absent is an assertion**, the rule decisions 7 and 10 already run on.
//!   An entry that does not mention `files` requires the response to carry
//!   none, so an entry whose rows sit one per file today fails the day a
//!   plugin emits one usage twice and the response grows a tally.
//! - **Present is the only check that sees that duplicate at all** on
//!   `[[callers]]`/`[[references]]`. Decision 8 deliberately exempts those
//!   two from the duplicate-row check, because two rows for one symbol are
//!   two usages there - so a plugin emitting the *same* usage twice is
//!   invisible to the set, invisible to decision 8, and visible only in this
//!   tally's per-file count.
//!
//! The value is a set of `"{path}:{refs}"` - `pagination::FileTally`'s two
//! fields in decision 2's own one-colon spelling, compared as a set so the
//! tally's highest-count-first ordering is never a spurious failure.
//!
//! `[[implementations]]` and `[[definition]]` carry no tally by design, so
//! the key is accepted on them and can only ever fail - the treatment
//! decision 10 gives `provenance` on `[[definition]]`, for its reason: a
//! fixture that asked for one by typo should hear about it rather than be
//! quietly ignored.
//!
//! **Where the `provenance` analogy breaks, and what it costs.** Decision
//! 10's healthy state is the same for every entry in every arm - the block is
//! absent - so making its omission an assertion couples an entry to nothing.
//! A tally is a count of *edges*, so its healthy state depends both on the
//! anchor and on which tiers ran: an anchor whose edge set grows with the
//! semantic tier has a tally that moves with it, and for some anchors the
//! tally is not sent at all in the structural arm. Once `files` is asserted
//! there is then no single spelling of that entry that is true in both arms,
//! and the entry has to carry `tier = "semantic"` for the tally even when its
//! rows are structural. Two of the bundled fixtures' entries are in exactly
//! that position and say so in their own comments -
//! `plugins/typescript`'s overloaded `format` (two call sites only once
//! tsserver binds them) and `plugins/rust`'s `[[references]] shapes::Shape`
//! (six usages in `shapes.rs` with rust-analyzer, four without) - and the
//! second of them is why that suite's `SEMANTIC_EXPECTATIONS` went from five
//! to six. The price is paid in the structural-only arm, which now skips
//! those entries rather than passing them; the design keeps the absent arm
//! anyway, because the absent arm is the half that catches a plugin emitting
//! one usage twice, and that is the regression this decision exists for.
//!
//! # Decision 13: `excludedReferences`, how a CALLS-only page says so (GM-386)
//!
//! `find_callers` walks `CALLS` edges alone and discloses what that left
//! behind - a count and a per-file tally of the `REFERENCES`-kind usages it
//! did not list (`find_callers_callees::ExcludedReferences`). The shipped
//! guidance tells an agent to read that instead of re-asking the same anchor
//! through `find_references`, so a fixture has to be able to pin it:
//!
//! ```toml
//! [[callers]]
//! symbol = "Placeholder"
//! expect = ["app_test.go:TestPlaceholder"]
//! excluded_references = { count = 1, files = ["main.go:1"] }
//! ```
//!
//! Both keys are required once the block is given, and its absence is an
//! assertion too: an entry that does not mention it requires the response to
//! carry no `excludedReferences` at all. That absent arm is what catches a
//! plugin that *stops* emitting a `REFERENCES` edge for a usage shape it
//! used to see - a change which leaves `[[callers]]`' own set untouched and
//! is therefore invisible to every other category in this file.
//!
//! `count` is asserted beside `files` rather than derived from it because
//! the wire keeps them apart for a reason: the count is exact and uncapped,
//! the tally is cut at `pagination::MAX_EXCLUDED_FILE_TALLY`, and
//! understating the gap is the one thing this disclosure must never do. A
//! response that sets `filesTruncated` fails the entry rather than being
//! compared against a capped list - decision 4's rule once more, and no
//! conformance fixture is within two orders of magnitude of that cap.
//! `hint` is a core constant carrying no plugin behaviour, and is not
//! asserted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
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
use crate::storage::index_store::IndexStore;

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
    /// Decision 12 (GM-386): the response's `files` tally, as a set of
    /// `"{path}:{refs}"`. **Absence is itself an assertion** - the response
    /// must then carry no `files` at all, which is what every entry whose
    /// rows already sit one per file means.
    #[serde(default)]
    files: Option<Vec<String>>,
    /// Decision 13 (GM-386): the `excludedReferences` block `find_callers`
    /// attaches when its `CALLS` walk left `REFERENCES`-kind usages behind.
    /// **Absence is itself an assertion**, same as [`Self::files`]: the
    /// response must carry no block at all.
    #[serde(default)]
    excluded_references: Option<ExcludedExpectation>,
}

/// Decision 13's `excluded_references` block. Both fields are required once
/// the key is given - see that decision on why the exact, uncapped `count`
/// is asserted beside the capped tally rather than derived from it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExcludedExpectation {
    count: u64,
    /// The same `"{path}:{refs}"` spelling [`SymbolExpectation::files`] uses,
    /// compared as a set.
    files: Vec<String>,
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
    pub(crate) conn: &'a Arc<IndexStore>,
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
            // Decisions 11-13, in the order a reader of the response meets
            // them: the whole page's own marker first, then the two
            // summaries beside `results`. The rows' `resolved` bits are not
            // here at all - decision 11 spells them into `rows` above, so
            // they reach the reader through the set diff.
            findings.extend(all_unresolved_finding(&value));
            findings.extend(files_finding(&value, item.files.as_deref()));
            findings.extend(excluded_references_finding(&value, item.excluded_references.as_ref()));
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

/// Decision 11's row marker: what a `resolved: false` row is prefixed with,
/// so the bit travels inside decision 2's one string per row instead of in a
/// second list beside `expect`.
const UNRESOLVED_PREFIX: &str = "unresolved:";

/// Decision 11's response-level half. `None` when the page says every row it
/// carries stands on a confirmed edge; `Some(finding)` when it says the
/// opposite, or does not say at all.
fn all_unresolved_finding(value: &Value) -> Option<String> {
    match value.get("allUnresolved").and_then(Value::as_bool) {
        Some(false) => None,
        Some(true) => Some(
            "the response set allUnresolved: true - every row on this page came from an edge the \
             linker could not confirm, so the page reads as an ordinary complete answer and is \
             not one; decision 11 never compares a set against it"
                .to_string(),
        ),
        None => Some(
            "the response carries no allUnresolved field at all, which every result page declares \
             non-optional - decision 11 reports that rather than reading its absence as \"nothing \
             to worry about\""
                .to_string(),
        ),
    }
}

/// Decision 12's `files` tally, both ways round - the same four-arm shape as
/// [`resolved_from_finding`] and [`provenance_finding`], because it is the
/// same kind of claim: a field present exactly when it says something the
/// rows do not, asserted in both its states so neither can drift unnoticed.
fn files_finding(value: &Value, expected: Option<&[String]>) -> Option<String> {
    match (expected, value.get("files")) {
        (None, None) => None,
        (None, Some(tally)) => Some(format!(
            "expected no files tally - this entry's rows are meant to sit one per file, so the \
             tally would restate the filePath column - but the response carries files = {{{}}} \
             (decision 12)",
            format_set(&tally_set(tally))
        )),
        (Some(expected), None) => Some(format!(
            "files = {{{}}}, but the response carries no files tally at all: it is claiming its \
             rows already sit one per file (pagination::tally_is_worth_sending), which is the \
             opposite of what this entry says (decision 12)",
            format_set(&expected.iter().cloned().collect())
        )),
        (Some(expected), Some(tally)) => {
            let actual = tally_set(tally);
            let expected: BTreeSet<String> = expected.iter().cloned().collect();
            (actual != expected).then(|| {
                format!(
                    "files tally mismatch (decision 12): expected {{{}}}, actual {{{}}}",
                    format_set(&expected),
                    format_set(&actual)
                )
            })
        }
    }
}

/// Decision 13's `excludedReferences` block, both ways round. The count and
/// the tally are compared separately because the wire keeps them separate -
/// the count is exact and uncapped, the tally is not.
fn excluded_references_finding(value: &Value, expected: Option<&ExcludedExpectation>) -> Option<String> {
    match (expected, value.get("excludedReferences")) {
        (None, None) => None,
        (None, Some(block)) => Some(format!(
            "expected no excludedReferences block - this CALLS walk is meant to have left nothing \
             behind - but the response discloses count = {}, files = {{{}}} (decision 13)",
            block.get("count").and_then(Value::as_u64).unwrap_or_default(),
            format_set(&tally_set(block.get("files").unwrap_or(&Value::Null)))
        )),
        (Some(expected), None) => Some(format!(
            "excluded_references = {{ count = {} }}, but the response carries no \
             excludedReferences block: the walk found no REFERENCES-kind usage of this anchor to \
             disclose, which is the usage shape disappearing rather than the answer changing \
             (decision 13)",
            expected.count
        )),
        (Some(expected), Some(block)) => {
            if block.get("filesTruncated").and_then(Value::as_bool) == Some(true) {
                return Some(
                    "the response set excludedReferences.filesTruncated - its tally was cut at \
                     pagination::MAX_EXCLUDED_FILE_TALLY, so decision 13 will not compare it \
                     against a fixture's whole list; narrow the fixture"
                        .to_string(),
                );
            }
            let actual_count = block.get("count").and_then(Value::as_u64);
            let actual_files = tally_set(block.get("files").unwrap_or(&Value::Null));
            let expected_files: BTreeSet<String> = expected.files.iter().cloned().collect();
            if actual_count == Some(expected.count) && actual_files == expected_files {
                return None;
            }
            Some(format!(
                "excludedReferences mismatch (decision 13): expected count = {}, files = {{{}}}; \
                 actual count = {}, files = {{{}}}",
                expected.count,
                format_set(&expected_files),
                actual_count.map(|c| c.to_string()).unwrap_or_else(|| "(absent)".to_string()),
                format_set(&actual_files)
            ))
        }
    }
}

/// A `pagination::FileTally` array as decision 12's `"{path}:{refs}"` set.
/// Anything that is not an array reads as the empty set, which is what the
/// callers above want: a block without the field they asked for fails with
/// `{(none)}` beside what was expected, rather than silently comparing
/// nothing against nothing.
fn tally_set(tally: &Value) -> BTreeSet<String> {
    tally
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            format!(
                "{}:{}",
                entry.get("path").and_then(Value::as_str).unwrap_or("?"),
                entry.get("refs").and_then(Value::as_i64).map(|n| n.to_string()).unwrap_or("?".to_string())
            )
        })
        .collect()
}

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
            //
            // Decisions 12 and 13 run here for that same reason and can only
            // ever pass omitted: a definition is one node, so there is
            // nothing to tally and no `CALLS` walk to have left anything
            // behind. `allUnresolved` is deliberately *not* checked - this
            // response is not a result page and declares no such field, so
            // decision 11's "a missing marker is a regression" rule would be
            // false here.
            let mut findings: Vec<String> =
                provenance_finding(&value, item.provenance.as_deref()).into_iter().collect();
            findings.extend(files_finding(&value, item.files.as_deref()));
            findings.extend(excluded_references_finding(&value, item.excluded_references.as_ref()));
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
///
/// Decision 11's `unresolved:` marker is applied here, which is the whole of
/// how a row's own `resolved` bit becomes assertable: a row that does not say
/// it is resolved - `false`, or the field missing entirely - carries the
/// prefix, so an entry that does not spell it is already asserting the row
/// stands on a confirmed edge.
fn rows_to_list(value: &Value) -> Vec<String> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            let file_path = row.get("filePath").and_then(Value::as_str).unwrap_or_default();
            let qualified_name = row.get("qualifiedName").and_then(Value::as_str).unwrap_or_default();
            let marker = match row.get("resolved").and_then(Value::as_bool) {
                Some(true) => "",
                Some(false) | None => UNRESOLVED_PREFIX,
            };
            format!("{marker}{file_path}:{qualified_name}")
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
mod tests;
