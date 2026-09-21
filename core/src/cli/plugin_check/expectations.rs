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
//!   `"{filePath}:{qualifiedName}"` rule, as a one-element (or zero-element,
//!   on a refusal) set.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::cli::plugin_check::report::{CheckResult, Outcome};
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
    fn call(&self, ctx: &EvalContext, params: SymbolQueryParams) -> Result<Value, String> {
        let result = match self {
            SymbolTool::Callers => find_callers_callees::handle_callers(ctx.conn, ctx.embedding, params),
            SymbolTool::References => find_references::handle(ctx.conn, ctx.embedding, params),
            SymbolTool::Implementations => {
                let SymbolQueryParams { symbol_id, symbol_name, cursor, limit, file_paths } = params;
                find_implementations::dispatch(
                    ctx.conn,
                    ctx.embedding,
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
        tool_json(result)
    }
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
            let findings = match tool {
                SymbolTool::Implementations => duplicate_row_finding(&rows).into_iter().collect(),
                SymbolTool::Callers | SymbolTool::References => Vec::new(),
            };
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
    let value = tool.call(ctx, first).map_err(|e| vec![e])?;
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
    let retried = tool.call(ctx, retry).map_err(|e| vec![e])?;
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
    tool_json(get_dependencies::handle(ctx.conn, ctx.entry_points, params))
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
            let expected: BTreeSet<String> = item.expect.iter().cloned().collect();
            CheckResult { id: id.into(), outcome: set_diff_outcome(&expected, &actual), warnings: Vec::new() }
        }
    }
}

/// `find_definition`'s own version of [`resolve_and_call`] - see decision 3
/// for why the disambiguated re-call is by exact `qualifiedName` rather than
/// by `symbol_id`: `find_definition` has no such parameter, unlike the other
/// four tools.
fn resolve_definition(ctx: &EvalContext, symbol: &str, file: Option<&str>) -> Result<Value, Vec<String>> {
    let call = |symbol_name: String| {
        let params = FindDefinitionParams {
            symbol_name: Some(symbol_name),
            file_path: None,
            position: None,
            cursor: None,
            include_source: Some(false),
        };
        tool_json(find_definition::handle(ctx.conn, ctx.project_root, ctx.embedding, params))
    };

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
    for candidate in value.get("results").and_then(Value::as_array).into_iter().flatten() {
        findings.push(format!(
            "candidate: id={} qualifiedName={} filePath={} kind={}",
            candidate.get("id").and_then(Value::as_str).unwrap_or("?"),
            candidate.get("qualifiedName").and_then(Value::as_str).unwrap_or("?"),
            candidate.get("filePath").and_then(Value::as_str).unwrap_or("?"),
            candidate.get("kind").and_then(Value::as_str).unwrap_or("?"),
        ));
    }
    findings
}

/// Extracts an MCP tool's answer as parsed JSON, or the message to report
/// instead: `ErrorData` (a protocol-level failure), or `is_error: true` (a
/// tool-level refusal - "no symbol named X found", a bad parameter
/// combination) both become a plain string, since neither carries JSON on
/// the wire for this module to parse.
fn tool_json(result: Result<CallToolResult, ErrorData>) -> Result<Value, String> {
    let result = result.map_err(|e| e.to_string())?;
    let text = match result.content.first() {
        Some(ContentBlock::Text(text)) => text.text.clone(),
        other => return Err(format!("tool returned no text content: {other:?}")),
    };
    if result.is_error == Some(true) {
        return Err(text);
    }
    serde_json::from_str(&text).map_err(|e| format!("tool returned content that is not JSON: {e}"))
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
