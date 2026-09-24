//! Real logic behind the `find_definition` MCP tool. Kept out of `mcp/mod.rs`
//! so that file stays pure tool-router wiring - this is where the actual
//! "name or position -> node(s)" decision lives.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::{Connection, Row};
use serde::Serialize;

use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination;
use crate::graph::queries;
use crate::storage::write::NodeRecord;

use super::similarity;
use super::source;
use super::tool_result::{error, internal_error, success};
use super::FindDefinitionParams;

/// Nothing in the ticket specifies a page size for the ambiguous-candidate
/// list, so 20 is a plain, generous-enough default - there's no existing
/// constant for this shape of list to reuse.
const CANDIDATE_PAGE_SIZE: usize = 20;

/// The full node, returned whenever the lookup is unambiguous: a
/// file+position query, an exact qualifiedName match, or a bare name that
/// happens to match exactly one node.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionNode {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    end_line: i64,
    end_col: i64,
    signature: Option<String>,
    doc_comment: Option<String>,
    /// Which rung of the ladder reached this - see [`ResolvedBy`]. Absent on a
    /// file+position lookup, which cannot be anything but exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_by: Option<ResolvedBy>,
    /// The declaration's own text - see [`source`] for why this is worth its
    /// payload and how it is bounded.
    ///
    /// Absent when the caller opted out with `include_source: false`, or when
    /// the file cannot be read at those coordinates (deleted, or edited since
    /// the walk). Coordinates without text are still a correct answer, so a
    /// snippet that cannot be produced is omitted rather than failing the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<source::Snippet>,
}

impl DefinitionNode {
    /// The node with the rung that reached it, for the name-addressed path.
    fn resolved(node: NodeRecord, by: ResolvedBy) -> Self {
        Self { resolved_by: Some(by), ..Self::from(node) }
    }

    /// Attaches the declaration's text, if a root was given and the file can
    /// still be read at these coordinates.
    ///
    /// `None` for the root means the caller passed `include_source: false` -
    /// the opt-out and the failure both land here, deliberately: from the
    /// response's point of view they are the same thing, a node without a
    /// snippet, and giving them two different shapes would make every consumer
    /// handle two cases to learn nothing.
    fn with_source(mut self, project_root: Option<&Path>) -> Self {
        self.source = project_root
            .and_then(|root| source::read_span(root, &self.file_path, self.start_line, self.end_line));
        self
    }
}

impl From<NodeRecord> for DefinitionNode {
    fn from(n: NodeRecord) -> Self {
        Self {
            id: n.id,
            kind: n.kind,
            name: n.name,
            qualified_name: n.qualified_name,
            file_path: n.file_path,
            start_line: n.start_line,
            start_col: n.start_col,
            end_line: n.end_line,
            end_col: n.end_col,
            signature: n.signature,
            doc_comment: n.doc_comment,
            resolved_by: None,
            source: None,
        }
    }
}

/// One entry in a ranked candidate list for an ambiguous bare name - a
/// preview, not the full node, since the caller is expected to re-query once
/// it picks one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionCandidate {
    /// The handle to re-query with, and the only one guaranteed to resolve:
    /// `qualifiedName` is not unique either. Excalidraw has two distinct
    /// `getNonDeletedElements` functions - `packages/element/src/index.ts`
    /// and `packages/element/src/Scene.ts` - whose qualifiedName is bare
    /// `getNonDeletedElements` in both, so picking one and asking again by
    /// name returns this very page a second time. Anchoring on `id` always
    /// terminates.
    id: String,
    qualified_name: String,
    file_path: String,
    kind: String,
    /// Signature over docstring when both exist - it's denser and more
    /// identifying in a ranked list than prose.
    preview: Option<String>,
}

impl From<super::search_code::SearchResult> for DefinitionCandidate {
    /// `preview` is `None` rather than fetched: a semantic hit is already
    /// being offered as a guess, and paying an extra query per candidate to
    /// dress a guess up would spend payload on the rung least likely to be
    /// right. The `id` is what the caller needs, and re-querying it returns
    /// the declaration's own source (GM-231) - which is a better preview than
    /// a signature and costs nothing until the caller asks for it.
    fn from(hit: super::search_code::SearchResult) -> Self {
        Self {
            id: hit.symbol_id,
            qualified_name: hit.qualified_name,
            file_path: hit.file_path,
            kind: hit.kind,
            preview: None,
        }
    }
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePage {
    /// Always `true`, and the reason this field exists at all: the four
    /// symbol-anchored tools return this page *in place of* their own result
    /// shape when a `symbol_name` turns out ambiguous, and both shapes are
    /// `{results, hasMore, nextCursor}`. Without a marker a caller would have
    /// to sniff item fields to tell "here are your callers" from "say which
    /// symbol you meant".
    ambiguous: bool,
    /// Which rung produced this page - always `nameAmbiguous` here. Present so
    /// a caller reads one field to tell an ambiguity from the other kind of
    /// candidate page (`fileName`), rather than inferring it from `ambiguous`.
    resolved_by: ResolvedBy,
    results: Vec<DefinitionCandidate>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// The file-name rung's page. Shares `{ambiguous, resolvedBy, results}` with
/// [`CandidatePage`] so a caller re-queries either the same way, and adds the
/// one thing that page cannot carry: why a name that is plainly in the source
/// resolved to nothing here.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileNamePage {
    resolved_by: ResolvedBy,
    /// Always `false`: these are not competing readings of one name.
    ambiguous: bool,
    explanation: String,
    results: Vec<DefinitionCandidate>,
}

/// Which column an ambiguous query matched on, and so which set of
/// declarations the candidate page ranks. See [`resolve_symbol_name`] for
/// when each is reached.
#[derive(Clone, Copy)]
enum NameColumn {
    Name,
    QualifiedName,
}

impl NameColumn {
    /// Interpolated into the SQL below rather than bound: a column name
    /// cannot be a parameter, and these two literals are the only values
    /// this type has.
    fn sql(self) -> &'static str {
        match self {
            NameColumn::Name => "n.name",
            NameColumn::QualifiedName => "n.qualifiedName",
        }
    }
}

/// Ranks candidates by inbound `REFERENCES`+`CALLS` edge count, descending -
/// a rough proxy for "how central is this symbol", since nothing else in the
/// graph orders same-named definitions across files.
///
/// # Test-only declarations are ranked, not excluded (GM-360)
///
/// The case that raised the question: bare `RegexMatcher` names four ripgrep
/// declarations, two `pub` production matchers wired into `core/search.rs`
/// and two `pub(crate)` test helpers, one of which lives under `tests/`. It
/// is tempting to drop a non-public declaration in a test directory from a
/// bare-name page when public ones compete, and this deliberately does not:
///
/// - A declaration that is in the index and carries the name is an answer to
///   "where is this defined", and someone reading `crates/matcher/tests/util.rs`
///   asking about the type in front of them would get a list that omits it.
///   The standing rule is that a missing edge beats a wrong one - but a
///   dropped candidate is not a missing edge, it is a wrong answer to a
///   question the index can answer.
/// - "Test-only" is not a fact this module has. `visibility` is normalized
///   (`public`/`container`/`file`), but what `container` *means* is the
///   plugin's business, and "under `tests/`" is a per-language convention -
///   Rust's integration tests, Go's `_test.go` siblings, Python's `tests/`
///   package - that core would have to encode a list of. A resolution ladder
///   that silently applies language conventions is how the tool got here.
/// - Ranking already separates them, measured rather than assumed. On
///   ripgrep the inbound counts are: `crates/regex`'s production matcher 11,
///   `crates/searcher`'s `pub(crate)` helper 7, `crates/pcre2`'s production
///   matcher 5, and the `tests/` fixture 2 - so the fixture that used to be
///   returned as *the* answer now ranks last of four, and the caller can
///   still reach it.
///
/// The ranking rule is therefore load-bearing, and it is this sentence and
/// the `ORDER BY` in [`pagination::paginate_by_score`] - nothing else in the
/// graph orders same-named declarations.
fn find_candidates_by_name(
    conn: &Connection,
    column: NameColumn,
    name: &str,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    // The `nativeKind` filter is `graph::queries`' own, shared rather than
    // restated (GM-367): the lookups that decide whether a candidate page is
    // needed and the page itself have to agree about what counts as a
    // declaration, and while they were two copies of one list they did not -
    // the copy here was three kinds where that one is five.
    let base_sql = format!(
        "SELECT n.id AS id, n.qualifiedName AS qualifiedName, n.filePath AS filePath, \
         n.kind AS kind, n.signature AS signature, n.docComment AS docComment, \
         CAST((SELECT COUNT(*) FROM edges e WHERE e.toId = n.id AND e.kind IN ('REFERENCES', 'CALLS')) AS REAL) AS score \
         FROM nodes n WHERE {} = ?1 AND {}",
        column.sql(),
        queries::declaration_only("n.")
    );

    fn map_row(row: &Row) -> rusqlite::Result<(DefinitionCandidate, f64, String)> {
        let id: String = row.get("id")?;
        let score: f64 = row.get("score")?;
        let candidate = DefinitionCandidate {
            id: id.clone(),
            qualified_name: row.get("qualifiedName")?,
            file_path: row.get("filePath")?,
            kind: row.get("kind")?,
            preview: row
                .get::<_, Option<String>>("signature")?
                .or(row.get::<_, Option<String>>("docComment")?),
        };
        Ok((candidate, score, id))
    }

    pagination::paginate_by_score(conn, &base_sql, &[&name], CANDIDATE_PAGE_SIZE, cursor, map_row)
}

/// Resolves `find_definition`'s file+position input - always unambiguous by
/// construction, so the answer is a single node, never a candidate list.
fn by_position(
    conn: &Connection,
    project_root: Option<&Path>,
    file_path: &str,
    line: u32,
    col: u32,
) -> Result<CallToolResult, ErrorData> {
    let found = queries::find_by_position(conn, file_path, line, col)
        .map_err(|e| internal_error("failed to resolve file+position", e))?;

    match found {
        Some(node) => success(&DefinitionNode::from(node).with_source(project_root)),
        None => error(format!("g-mesh: no symbol found at {file_path}:{line}:{col}")),
    }
}

/// Which rung of the resolution ladder produced an answer - see
/// `docs/architecture/symbol-resolution-ladder.md`.
///
/// Echoed on every response so a *suggestion* can never be read as a
/// *resolution*. `Id`, `QualifiedName` and `Name` establish that this is the
/// symbol asked for; `NameAmbiguous` and `FileName` establish only that these
/// are candidates worth re-querying. Without the label the two are
/// indistinguishable in the response, and this codebase's standing rule is
/// that a missing edge beats a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ResolvedBy {
    /// An exact node id, given by the caller.
    Id,
    /// An exact `qualifiedName` - how a caller re-queries a candidate it picked.
    QualifiedName,
    /// A bare name matching exactly one declaration.
    Name,
    /// A name matching several declarations - bare (`RegexMatcher`, four
    /// times in ripgrep) or qualified (`matcher::RegexMatcher`, twice: a Rust
    /// qualifiedName is a module path *within* a crate, and two crates can
    /// hold the same one). Either way the page is ranked candidates, not an
    /// answer, and either way the caller re-queries by a candidate's `id` -
    /// which is why both wear this one label rather than two.
    NameAmbiguous,
    /// No declaration carries the name, but a file does - so these are that
    /// file's declarations, offered because a default import binds an export
    /// under a local name this index never sees.
    FileName,
    /// Nothing structural matched, and the semantic index scored these highly
    /// enough to be worth offering. The one rung whose answers are similarity,
    /// not resolution - see [`by_semantic_neighbours`].
    SemanticNeighbours,
}

/// A resolved anchor and the rung that reached it.
pub(super) struct Resolved {
    pub(super) node: NodeRecord,
    pub(super) by: ResolvedBy,
}

/// Resolves a symbol name to the single node it names: an exact
/// qualifiedName match is tried first as a fast path (this is how a caller
/// re-queries a candidate it picked off a previous ambiguous page), then
/// falls back to a bare-name lookup.
///
/// # Neither key is unique, and the fast path has to say so (GM-360)
///
/// A qualifiedName is a *spelling*, not a primary key, and a bare one is not
/// evidence of anything. Both halves were measured on ripgrep, which declares
/// `RegexMatcher` four times:
///
/// | spelling | declarations | 3.7.0's answer |
/// |---|---|---|
/// | `RegexMatcher` | 4 (2 `pub`, 2 `pub(crate)`) | the one `pub(crate)` fixture under `tests/`, `resolvedBy: qualifiedName`, unflagged |
/// | `matcher::RegexMatcher` | 2 (`crates/regex`, `crates/pcre2`) | `resolvedBy: semanticNeighbours` - nothing structural matched at all |
///
/// The fixture won the first because a Rust qualifiedName is a module path
/// *within its crate*, so a crate-root declaration carries the bare string
/// and the other three do not - the fast path then matched exactly one row
/// and stopped. That is an accident of where a declaration sits, not a
/// reason to prefer it, so a query that is also some declaration's own name
/// goes through the bare-name rung with all of its namesakes.
///
/// The second failed for the mirror reason: two crates each have a `matcher`
/// module, the fast path matched *two* rows, and "not exactly one" fell
/// through to a bare-name lookup that no qualified spelling can ever match,
/// and from there to the semantic rung. Several exact matches is an
/// ambiguity, not a miss.
///
/// Go's `Binding` in gin - two `interface` declarations behind build tags,
/// both with bare qualifiedNames - is the control: it was already ambiguous
/// on both counts and is reached by the same first arm of the match below,
/// unchanged.
///
/// # The specifier guard on the second arm is gone, because it is now dead (GM-367)
///
/// GM-360 added `if !is_module_specifier(name)` to the `qualifiedName` arm
/// for one reason: gin's `net/http` was the qualifiedName of 63
/// `external_module` import placeholders, so without it a package name was
/// answered with a page of 20 rows that are not declarations. GM-367 removed
/// those rows from `graph::queries`' lookups instead, one layer down, which
/// is where the problem always was - and with them gone, nothing this guard
/// could catch reaches it.
///
/// Measured on the three probe indexes (gin, ripgrep, requests - schema 8,
/// indexer 2) and on a fourth, older excalidraw one: after the exclusion, no
/// specifier-shaped spelling is the `qualifiedName` of two or more
/// declarations, in any of them. The reason is structural rather than a
/// property of these four codebases - every declaration whose qualifiedName
/// contains `/` or starts with `@` is a `File` node, and a file path is
/// unique within a project by construction, so `exact.len() >= 2` cannot
/// arise for a specifier-shaped query at all. The guard's arm is never
/// entered, so its condition is never evaluated.
///
/// It is removed rather than left as a belt-and-braces second defence,
/// because a guard that cannot fire still reads as the place the problem is
/// handled, and the next person to work here would have to re-derive that it
/// is not. `is_module_specifier` itself stays: its *other* caller,
/// [`by_semantic_neighbours`], is where shape genuinely decides something
/// a score cannot (`@excalidraw/element` scores 0.699 against an index it has
/// nothing to do with).
///
/// `Ok(Ok(node))` is that node. `Ok(Err(result))` is a finished response the
/// caller must return unchanged - the ranked candidate page when the name is
/// ambiguous, or the not-found tool error - which is what lets the four
/// symbol-anchored tools accept a `symbol_name` (see `mcp::anchor`) and mean
/// exactly what `find_definition` means by it, down to the error text.
/// Shaped like `find_callers_callees`' old `resolve_anchor` rather than a
/// bespoke enum so every call site is the same two-line `match`.
pub(super) fn resolve_symbol_name(
    conn: &Connection,
    embedding: Option<&EmbeddingPipeline>,
    name: &str,
    cursor: Option<&str>,
) -> Result<Result<Resolved, CallToolResult>, ErrorData> {
    let mut exact = queries::find_by_qualified_name(conn, name, None)
        .map_err(|e| internal_error("failed to look up node by qualifiedName", e))?;

    // The fast path, and the only single-query one: a genuinely qualified
    // spelling that exactly one declaration carries. `name != its own name`
    // is what "genuinely qualified" means here without this module having to
    // know any language's path separator - and it is the whole cost of the
    // change on the happy path, one string comparison on a row already read.
    if exact.len() == 1 && exact[0].name != name {
        return Ok(Ok(Resolved { node: exact.remove(0), by: ResolvedBy::QualifiedName }));
    }

    let matches = queries::find_by_name(conn, name, None)
        .map_err(|e| internal_error("failed to look up node by name", e))?;

    // The name set is consulted first, and subsumes the other whenever the
    // query is bare: a declaration whose qualifiedName *is* the bare string
    // is named that too, so `matches` is then a superset of `exact` and
    // ranking the smaller set would drop real candidates - excalidraw's two
    // bare `getNonDeletedElements` would hide any module-qualified third.
    // `exact` decides only when nothing is *named* the query, i.e. when the
    // spelling is qualified.
    let ambiguous_over = match (matches.len(), exact.len()) {
        (2.., _) => Some(NameColumn::Name),
        (_, 2..) => Some(NameColumn::QualifiedName),
        _ => None,
    };
    if let Some(column) = ambiguous_over {
        let page = find_candidates_by_name(conn, column, name, cursor)
            .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
        return success(&CandidatePage {
            ambiguous: true,
            resolved_by: ResolvedBy::NameAmbiguous,
            results: page.results,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
        .map(Err);
    }

    match matches.into_iter().next() {
        // With one match and one exact row they are the same node - the query
        // is both its name and its whole qualifiedName - so the stronger rung
        // is reported, which is what keeps every language whose qualifiedNames
        // are bare by construction (TypeScript) answering exactly as it did.
        Some(node) => {
            let by = if exact.len() == 1 { ResolvedBy::QualifiedName } else { ResolvedBy::Name };
            Ok(Ok(Resolved { node, by }))
        }
        None => by_file_name(conn, embedding, name),
    }
}

/// How many neighbours to offer. The calibration found the correct hit ranked
/// first in 19 of 21 cases, so a long list would be payload without value.
const SEMANTIC_CANDIDATES: usize = 3;

/// Whether `name` is a module specifier rather than a symbol name.
///
/// Checked *before* the score, because the score cannot catch this. Package
/// specifiers are the only kind of junk query that approaches the threshold -
/// `@excalidraw/element` scores 0.699 - and the reason is structural: only doc
/// comments and signatures are embedded, so a specifier has nothing to match
/// and similarity is computed against unrelated text. The same string scores
/// 0.566 against an index where that package does not exist at all, which is
/// the proof that the score describes the query's shape and not the corpus.
///
/// Raising the threshold to 0.70 would exclude it too, and cost 42 points of
/// recall to do so. This costs nothing, and specifiers already have a rung of
/// their own - `get_dependencies`' path matching.
fn is_module_specifier(name: &str) -> bool {
    name.starts_with('@') || name.contains('/')
}

/// The rung between "no file carries this name either" and a refusal: ask the
/// semantic index, and offer what it returns as *candidates*.
///
/// # Why this exists
///
/// Measured against a fully indexed excalidraw, with no model in the loop:
/// `find_definition("DropdownMenuGroup")` refused while
/// `search_code("DropdownMenuGroup")` answered correctly on its first result.
/// The same index, the same question, one tool refusing and the other right -
/// and the caller pays a whole round trip (18,000-22,000 tokens at an MCP
/// client's prompt prefix) to discover that the other tool would have worked.
///
/// # Candidates, never an answer
///
/// Similarity is not resolution, and this codebase's rule is that a missing
/// edge beats a wrong one. The calibration makes the reason concrete: `AppState`
/// returns `createAppState` at 0.845 and `ExcalidrawImperativeAPI` returns
/// `App#createExcalidrawAPI` at 0.839 - closely related declarations,
/// confidently scored, not the thing asked for. A confident wrong answer is
/// worse than a refusal; a labelled "did you mean" is not. So this returns the
/// same page shape the ambiguous and file-name rungs already use, with its own
/// `resolvedBy`, and every candidate carries the `id` to re-query with.
///
/// # When it stays silent
///
/// No model (`embed_query` is `None` on a machine that never downloaded the
/// 612 MiB weights), a specifier-shaped query, or nothing scoring above its
/// language's [`similarity::floor`]: all three fall through to the terse
/// refusal this rung was added in front of, never to an error.
///
/// # The floor moved, and it moved *down* here - on purpose
///
/// GM-381 needed the same judgement for `search_code`, measured it over four
/// languages, and found one constant could not serve them; the table now
/// lives in `mcp::similarity::floor` and this rung reads it per hit rather
/// than holding a constant of its own. That is not a free refactor, because
/// the two call sites do not see the same kind of query: this rung is only
/// ever reached with a *symbol name*, while `search_code` takes free text,
/// and the safe floor for free text sits lower. So the shared table changes
/// what this rung does, and the change is worth stating rather than
/// discovering.
///
/// Calibrated on name queries alone (149 go, 145 python, 143 rust, 291
/// typescript positives against 150-300 absent-name negatives each), the
/// floor that keeps this rung's false refusals at or under 3% is 0.648 for
/// go, 0.628 for python, 0.555 for rust and 0.538 for typescript. The shared
/// table ships 0.59 / 0.57 / 0.55 / 0.50 - at or below every one of those.
/// **Every deviation is in the direction of offering candidates rather than
/// refusing**, which is the direction this rung's own argument asks for: a
/// labelled "did you mean" is cheap and a refusal is what GM-234 existed to
/// stop. Concretely, on TypeScript name queries the old 0.60 wrongly refused
/// 6.9% of pages that held the right answer; at 0.50 that is 0.0%, paid for
/// by offering candidates on 35% of hopeless queries instead of 10%.
///
/// Tightening this rung with a *name-query* table of its own is a real
/// improvement left undone here, because it is a different calibration with
/// a different cost matrix and it would have ridden in unmeasured on a task
/// about `search_code`.
fn by_semantic_neighbours(
    conn: &Connection,
    embedding: Option<&EmbeddingPipeline>,
    name: &str,
) -> Option<Result<CallToolResult, ErrorData>> {
    if is_module_specifier(name) {
        return None;
    }
    let query = embedding?.embed_query(name)?;
    let page = super::search_code::search(conn, &query, SEMANTIC_CANDIDATES, None).ok()?;
    let results: Vec<DefinitionCandidate> = page
        .results
        .into_iter()
        .filter(|hit| hit.score >= similarity::floor(&hit.language))
        .map(DefinitionCandidate::from)
        .collect();
    if results.is_empty() {
        return None;
    }

    Some(success(&FileNamePage {
        resolved_by: ResolvedBy::SemanticNeighbours,
        ambiguous: false,
        explanation: format!(
            "Nothing is named '{name}'. These are the closest declarations by meaning, not by \
             name - they may be what you meant, or may merely be nearby. Check one before \
             relying on it, and re-query by its id."
        ),
        results,
    }))
}

/// How many specifiers a refusal names before it stops. Three, because the
/// list is prose in an error message rather than a page: gin's `json` is one
/// specifier and ripgrep's `regex` is two, and a spelling that is four
/// different imports is a fact about the query, not a list worth reading.
const MAX_NAMED_SPECIFIERS: usize = 3;

/// The rung GM-367 owes the caller: nothing *declares* this name, but the
/// index knows exactly what it is - something this project imports.
///
/// # Why a rung and not just a better string
///
/// Excluding import placeholders from `graph::queries`' lookups is what stops
/// `find_definition("context")` answering with an `import` line and calling
/// it a declaration. It is not by itself what makes the answer useful: what
/// is left over is "g-mesh: no symbol named 'http' found", which is true, and
/// which hides that the index has 63 records of `net/http` being imported and
/// could have said so.
///
/// So the exclusion says that it happened, at the one place a caller sees it.
/// That is the `resolvedFrom` / `excludedReferences` shape this tool surface
/// already uses - narrow the answer, and say that you did - without adding a
/// field to a row that should not be in the answer at all, or to four tools
/// that have nowhere to put one (`graph::queries`' header has that argument
/// in full).
///
/// # Where it sits, and why there
///
/// After the file-name rung and before the semantic one.
///
/// *After* file names, because a file that declares things is a better answer
/// than a note about an import: gin's `context` *is* `context.go`, and that
/// file's declarations are what the caller was reaching for. That rung keeps
/// the query.
///
/// *Before* semantics, because the two are not the same kind of claim. The
/// semantic rung offers "the closest declarations by meaning ... they may be
/// what you meant, or may merely be nearby"; this one states what the index
/// records. A resemblance does not improve on a fact, and this codebase's
/// standing rule - a missing edge beats a wrong one - is the same preference
/// one layer up.
///
/// # An error, not a page
///
/// There is genuinely no symbol to return, so this stays `is_error: true` and
/// carries prose, exactly as the refusal it replaces did. That also keeps the
/// blast radius of GM-367 on `find_references`/`find_callers`/`find_callees`/
/// `find_implementations`, which share this resolution, to the *text* of a
/// refusal rather than the shape of a response.
fn import_only_refusal(conn: &Connection, name: &str) -> Result<Option<CallToolResult>, ErrorData> {
    let specifiers = queries::import_specifiers_named(conn, name)
        .map_err(|e| internal_error("failed to look up import placeholders by name", e))?;
    if specifiers.is_empty() {
        return Ok(None);
    }

    let carriers: usize = specifiers.iter().map(|(_, count)| count).sum();
    let named: Vec<String> = specifiers
        .iter()
        .take(MAX_NAMED_SPECIFIERS)
        .map(|(specifier, count)| format!("'{specifier}' ({count})"))
        .collect();
    let rest = specifiers.len().saturating_sub(named.len());
    let and_more = if rest > 0 { format!(", and {rest} more") } else { String::new() };
    let message = format!(
        "g-mesh: nothing named '{name}' is declared in this project. It names something this \
         project imports: {}{and_more} - {carriers} import record(s), which have no definition \
         site here. For what a file imports, or what imports it, use get_dependencies.",
        named.join(", ")
    );
    error(message).map(Some)
}

/// The last rung before a refusal: no declaration carries the name, but a file
/// does.
///
/// 2.9.0 answered this case with prose in the error text and measured well -
/// `ex-default-export-dropdownmenu-group` went from 4 turns to 2. This returns
/// the same facts as the candidate page the ambiguous rung already produces,
/// because the caller then has one contract to know rather than two: re-query
/// by a candidate's `id`. The prose said "re-query by one of those"; the page
/// *is* those.
///
/// `ambiguous: false`, because these are not several readings of one name -
/// they are what a differently-named thing declares. The rung label is what
/// says so.
///
/// The five it offers are the file's five most-referenced declarations, not
/// its first five - `graph::queries::find_in_file_named`'s own doc has the
/// measurement and what the alternatives cost. The explanation below says so
/// in the response, because an order the caller cannot see is an order the
/// caller cannot use: "these are the first five" and "these are the five the
/// rest of the project leans on" are different claims about the same page.
///
/// `find_in_file_named`'s stem match is directory-agnostic (GM-377), so more
/// than one file can share a stem - gin's `fs.go` and `internal/fs/fs.go`
/// both answer for `fs`. Measured across every stem gin, ripgrep and requests
/// can reach this rung with (GM-373's enumeration, reused rather than
/// rebuilt): 12 of 107 pages mix rows from more than one file. That is real
/// but modest, and every row already carries its own `filePath` - so the
/// fix is the sentence, not a ranking rule to crown one file or a new field
/// to carry the rest: when the rows span more than one file the explanation
/// names all of them instead of asserting the first row's path as if it were
/// the only one.
fn by_file_name(
    conn: &Connection,
    embedding: Option<&EmbeddingPipeline>,
    name: &str,
) -> Result<Result<Resolved, CallToolResult>, ErrorData> {
    const MAX_SUGGESTIONS: usize = 5;
    let in_file = queries::find_in_file_named(conn, name, MAX_SUGGESTIONS)
        .map_err(|e| internal_error("failed to look up nodes by file name", e))?;
    if in_file.is_empty() {
        // Before the semantic rung, because this is a fact the index holds
        // and that one offers a resemblance - see [`import_only_refusal`].
        if let Some(refusal) = import_only_refusal(conn, name)? {
            return Ok(Err(refusal));
        }
        // The last rung before giving up. It returns `None` for every way of
        // having nothing useful to say - no model, a specifier-shaped query,
        // nothing scoring high enough - so the terse refusal below stays the
        // answer in all of them.
        return match by_semantic_neighbours(conn, embedding, name) {
            Some(page) => page.map(Err),
            None => error(format!("g-mesh: no symbol named '{name}' found")).map(Err),
        };
    }

    // Distinct files, in the order their rows first appear on the page - not
    // sorted, so this reads as "the files behind the rows above" rather than
    // implying a ranking between files that the query never computed.
    let mut file_paths: Vec<&str> = Vec::new();
    for n in &in_file {
        if !file_paths.contains(&n.file_path.as_str()) {
            file_paths.push(&n.file_path);
        }
    }
    // The singular case keeps the exact sentence this rung has always used -
    // this branch changes only what a *multi-file* page says about itself.
    let (source_sentence, pronoun) = match file_paths.as_slice() {
        [only] => (format!("The file {only} is, and declares these"), "its"),
        many => (format!("These files are, and between them declare these: {}", many.join(", ")), "their"),
    };
    success(&FileNamePage {
        resolved_by: ResolvedBy::FileName,
        ambiguous: false,
        explanation: format!(
            "No declaration is named '{name}'. {source_sentence} - {pronoun} \
             most-referenced declarations first. A default import binds a file's export under \
             whatever local name the importing file chose, and that local name is not indexed - \
             so a name read at a use site can be absent here while the declaration it refers to \
             is present under its own name. Re-query by one of these ids."
        ),
        results: in_file
            .iter()
            .map(|n| DefinitionCandidate {
                id: n.id.clone(),
                qualified_name: n.qualified_name.clone(),
                file_path: n.file_path.clone(),
                kind: n.kind.clone(),
                preview: n.signature.clone().or_else(|| n.doc_comment.clone()),
            })
            .collect(),
    })
    .map(Err)
}

/// `find_definition`'s own symbol-name input: the shared resolution above,
/// with the resolved case formatted as the full node this tool promises.
fn by_name(
    conn: &Connection,
    project_root: Option<&Path>,
    embedding: Option<&EmbeddingPipeline>,
    name: &str,
    cursor: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    match resolve_symbol_name(conn, embedding, name, cursor)? {
        Ok(resolved) => {
            success(&DefinitionNode::resolved(resolved.node, resolved.by).with_source(project_root))
        }
        Err(finished) => Ok(finished),
    }
}

pub(crate) fn handle(
    conn: &Arc<Mutex<Connection>>,
    project_root: &Path,
    embedding: &EmbeddingPipeline,
    params: FindDefinitionParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();
    // Defaults to on. The snippet is the point of the field - a caller who
    // wants coordinates alone has to say so, rather than every caller having
    // to ask for the thing that saves them a round trip.
    let project_root = params.include_source.unwrap_or(true).then_some(project_root);

    match (params.file_path, params.position, params.symbol_name) {
        (Some(file_path), Some(position), _) => {
            by_position(&conn, project_root, &file_path, position.line, position.col)
        }
        (None, None, Some(name)) => by_name(&conn, project_root, Some(embedding), &name, params.cursor.as_deref()),
        (None, None, None) if params.cursor.is_some() => {
            error("g-mesh: `cursor` continues a previous ambiguous symbol_name lookup - give the same symbol_name again")
        }
        (None, None, None) => {
            error("g-mesh: give either `symbol_name`, or both `file_path` and `position`")
        }
        _ => error("g-mesh: `file_path` and `position` must be given together"),
    }
}

#[cfg(test)]
mod tests;
