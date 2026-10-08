//! Real logic behind the `find_definition` MCP tool. Kept out of `mcp/mod.rs`
//! so that file stays pure tool-router wiring - this is where the actual
//! "name or position -> node(s)" decision lives.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::{Connection, Row};
use serde::Serialize;

use crate::daemon::registry::PathCoverage;
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination;
use crate::graph::queries;
use crate::storage::index_store::IndexStore;
use crate::storage::write::NodeRecord;

use super::not_indexed;
use super::query_shapes::QueryShapes;
use super::similarity;
use super::source;
use super::tool_result::{error, internal_error, success};
use super::FindDefinitionParams;

/// Nothing in the ticket specifies a page size for the ambiguous-candidate
/// list, so 20 is a plain, generous-enough default - there's no existing
/// constant for this shape of list to reuse.
const CANDIDATE_PAGE_SIZE: usize = 20;

/// The most candidates an ambiguous page carries source for. Past this the
/// page is a list to choose from rather than a set of readings to compare,
/// and every candidate's text would be paid for to answer one of them.
const SOURCED_CANDIDATES: usize = 3;

/// A sourced candidate's caps: a quarter of a resolved answer's
/// ([`source::MAX_LINES`], [`source::MAX_CHARS`]), since up to
/// [`SOURCED_CANDIDATES`] of them share one page. Enough for a signature and
/// the start of a body, which is what tells two same-named declarations apart.
const CANDIDATE_SOURCE_LINES: usize = 20;
const CANDIDATE_SOURCE_CHARS: usize = 1_500;

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
    /// The name looked up in place of the query - see [`Resolved::queried_as`].
    #[serde(skip_serializing_if = "Option::is_none")]
    queried_as: Option<String>,
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
    fn resolved(resolved: Resolved) -> Self {
        Self { resolved_by: Some(resolved.by), queried_as: resolved.queried_as, ..Self::from(resolved.node) }
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
        self.source = project_root.and_then(|root| {
            source::read_span(root, &self.file_path, self.start_line, self.end_line, Some(self.end_col))
        });
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
            queried_as: None,
            source: None,
        }
    }
}

/// One entry in a ranked candidate list for an ambiguous bare name - a
/// preview, not the full node: the caller re-queries the one it picks, unless
/// the page is small enough to carry every candidate's source itself.
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
    /// Zero-based, as on a resolved answer, so a caller can go straight to
    /// `filePath`+`position` without reading the file to find the line.
    /// Absent on a semantic guess, whose search row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<i64>,
    /// The span's end column, for reading its source only (an old index's
    /// whole-file end - see [`source::read_span`]); never serialized, so the
    /// page's shape is unchanged.
    #[serde(skip)]
    end_col: Option<i64>,
    kind: String,
    /// Signature over docstring when both exist - it's denser and more
    /// identifying in a ranked list than prose.
    preview: Option<String>,
    /// The candidate's own text, on a small, complete ambiguous page only -
    /// see [`CandidatePage::ambiguous`].
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<source::Snippet>,
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
            start_line: None,
            end_line: None,
            end_col: None,
            kind: hit.kind,
            preview: None,
            source: None,
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
    /// How to pick from this page: `session_hints::AMBIGUOUS`,
    /// `AMBIGUOUS_SOURCED` when every candidate carries its source, or
    /// `AMBIGUOUS_PARTLY_SOURCED` when only some do.
    explanation: &'static str,
    /// The name looked up in place of the query - see [`Resolved::queried_as`].
    #[serde(skip_serializing_if = "Option::is_none")]
    queried_as: Option<String>,
    results: Vec<DefinitionCandidate>,
    has_more: bool,
    next_cursor: Option<String>,
}

impl CandidatePage {
    /// The ambiguous page over `page`'s ranked candidates.
    ///
    /// When `source_root` is given and the whole candidate set is this one
    /// small page (first page, no more, at most [`SOURCED_CANDIDATES`]),
    /// every candidate carries its own source, so the caller can tell the
    /// readings apart without a second call. Every candidate or none: the
    /// page answers each reading equally and still picks none of them, which
    /// is what keeps it from reading as a confident answer to the first one.
    ///
    /// "Every" is what is attempted, not what is promised: a candidate whose
    /// span cannot be read (a file edited since the walk) comes back without
    /// `source`, and the explanation then says "some", so it never claims a
    /// source the page does not carry.
    fn ambiguous(
        mut page: pagination::Page<DefinitionCandidate>,
        queried_as: Option<&str>,
        cursor: Option<&str>,
        source_root: Option<&Path>,
    ) -> Self {
        let whole_set_here = cursor.is_none() && !page.has_more && page.results.len() <= SOURCED_CANDIDATES;
        let mut sourced = 0;
        if let (true, Some(root)) = (whole_set_here, source_root) {
            for candidate in &mut page.results {
                if let (Some(start), Some(end)) = (candidate.start_line, candidate.end_line) {
                    candidate.source = source::read_span_within(
                        root,
                        &candidate.file_path,
                        start,
                        end,
                        candidate.end_col,
                        CANDIDATE_SOURCE_LINES,
                        CANDIDATE_SOURCE_CHARS,
                    );
                    sourced += usize::from(candidate.source.is_some());
                }
            }
        }
        Self {
            ambiguous: true,
            resolved_by: ResolvedBy::NameAmbiguous,
            explanation: match sourced {
                0 => super::session_hints::AMBIGUOUS,
                n if n == page.results.len() => super::session_hints::AMBIGUOUS_SOURCED,
                _ => super::session_hints::AMBIGUOUS_PARTLY_SOURCED,
            },
            queried_as: queried_as.map(str::to_string),
            results: page.results,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        }
    }
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
    lookups: &[Lookup<'_>],
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    let (filter, params) = Lookup::filter(lookups, |key| format!("{} = {key}", column.sql()));
    rank_candidates(conn, &filter, &params, cursor)
}

/// The same ranked page over the declarations one of whose stored partial
/// paths (`qualified_suffixes`) is exactly a lookup's key - the set
/// [`queries::find_by_qualified_suffix`] returns.
fn find_candidates_by_qualified_suffix(
    conn: &Connection,
    lookups: &[Lookup<'_>],
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    let (filter, params) = Lookup::filter(lookups, |key| {
        format!("n.id IN (SELECT s.nodeId FROM qualified_suffixes s WHERE s.suffix = {key})")
    });
    rank_candidates(conn, &filter, &params, cursor)
}

/// One spelling a candidate page matches: `key`, among `languages`' declarations
/// when given, among every language's otherwise.
struct Lookup<'a> {
    key: &'a str,
    languages: Option<Vec<&'a str>>,
}

impl<'a> Lookup<'a> {
    /// The query exactly as given, in every language.
    fn any(key: &'a str) -> Self {
        Self { key, languages: None }
    }

    /// The SQL condition matching any of `lookups` (each one `matches(key
    /// placeholder)`, ANDed with its language filter) and its bound values,
    /// numbered from `?1`. The language filter sits inside the query, never
    /// after it, so the page's `hasMore` and cursor count only rows it keeps.
    fn filter(lookups: &[Lookup<'_>], matches: impl Fn(&str) -> String) -> (String, Vec<String>) {
        let mut params = Vec::new();
        let mut placeholder = |value: &str| {
            params.push(value.to_string());
            format!("?{}", params.len())
        };
        let conditions: Vec<String> = lookups
            .iter()
            .map(|lookup| {
                let key = matches(&placeholder(lookup.key));
                match &lookup.languages {
                    None => key,
                    Some(languages) => {
                        let list: Vec<String> =
                            languages.iter().map(|language| placeholder(language)).collect();
                        format!("({key} AND n.language IN ({}))", list.join(", "))
                    }
                }
            })
            .collect();
        let filter = match conditions.as_slice() {
            [one] => one.clone(),
            many => format!("({})", many.join(" OR ")),
        };
        (filter, params)
    }

    fn admits(&self, node: &NodeRecord) -> bool {
        self.languages.as_ref().is_none_or(|languages| languages.contains(&node.language.as_str()))
    }
}

/// Declarations satisfying `filter` (a condition on alias `n`, bound by
/// `params`), ranked as [`find_candidates_by_name`] documents.
fn rank_candidates(
    conn: &Connection,
    filter: &str,
    params: &[String],
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    let params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    // The `nativeKind` filter is `graph::queries`' own, shared rather than
    // restated (GM-367): the lookups that decide whether a candidate page is
    // needed and the page itself have to agree about what counts as a
    // declaration, and while they were two copies of one list they did not -
    // the copy here was three kinds where that one is five.
    let base_sql = format!(
        "SELECT n.id AS id, n.qualifiedName AS qualifiedName, n.filePath AS filePath, \
         n.startLine AS startLine, n.endLine AS endLine, n.endCol AS endCol, n.kind AS kind, n.signature AS signature, n.docComment AS docComment, \
         CAST((SELECT COUNT(*) FROM edges e WHERE e.toId = n.id AND e.kind IN ('REFERENCES', 'CALLS')) AS REAL) AS score \
         FROM nodes n WHERE {filter} AND {}",
        queries::declaration_only("n.")
    );

    fn map_row(row: &Row) -> rusqlite::Result<(DefinitionCandidate, f64, String)> {
        let id: String = row.get("id")?;
        let score: f64 = row.get("score")?;
        let candidate = DefinitionCandidate {
            id: id.clone(),
            qualified_name: row.get("qualifiedName")?,
            file_path: row.get("filePath")?,
            start_line: Some(row.get("startLine")?),
            end_line: Some(row.get("endLine")?),
            end_col: Some(row.get("endCol")?),
            kind: row.get("kind")?,
            preview: row
                .get::<_, Option<String>>("signature")?
                .or(row.get::<_, Option<String>>("docComment")?),
            source: None,
        };
        Ok((candidate, score, id))
    }

    pagination::paginate_by_score(conn, &base_sql, &params, CANDIDATE_PAGE_SIZE, cursor, map_row)
}

/// Resolves `find_definition`'s file+position input - always unambiguous by
/// construction, so the answer is a single node, never a candidate list.
///
/// `coverage` says whether `file_path`'s language is indexed at all; a miss
/// in an absent or failed language carries the reason ([`not_indexed::miss`]).
fn by_position(
    conn: &Connection,
    project_root: Option<&Path>,
    coverage: Option<&PathCoverage>,
    file_path: &str,
    line: u32,
    col: u32,
) -> Result<CallToolResult, ErrorData> {
    let found = queries::find_by_position(conn, file_path, line, col)
        .map_err(|e| internal_error("failed to resolve file+position", e))?;

    match found {
        Some(node) => success(&DefinitionNode::from(node).with_source(project_root)),
        None => {
            not_indexed::miss(conn, coverage, format!("g-mesh: no symbol found at {file_path}:{line}:{col}"))
        }
    }
}

/// Which rung of the resolution ladder produced an answer - see
/// `docs/architecture/symbol-resolution-ladder.md`.
///
/// Echoed on every response so a *suggestion* can never be read as a
/// *resolution*. `Id`, `QualifiedName`, `Name` and `QualifiedNameSuffix`
/// establish that this is the symbol asked for; `NameAmbiguous` and
/// `FileName` establish only that these
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
    /// A partial path (`IndexStore::read`) that exactly one declaration's
    /// qualifiedName ends in at a segment boundary. The answer carries the
    /// full qualifiedName, so the caller sees what the tail matched.
    QualifiedNameSuffix,
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
    /// The name the ladder looked up in place of the query, when the query
    /// matched nothing and one of its language's strip prefixes was retried
    /// ([`by_stripped_prefix`]); `None` when the query itself matched. `by`
    /// stays the rung the remainder resolved at.
    pub(super) queried_as: Option<String>,
}

impl Resolved {
    fn new(node: NodeRecord, by: ResolvedBy) -> Self {
        Self { node, by, queried_as: None }
    }
}

/// How the ladder's last rung, [`by_semantic_neighbours`], gets its query
/// vector.
///
/// Invariant: inference (and the model's load on first use) never runs under
/// the store lock or on an async worker - every other session, `tools/list`
/// included, would wait behind it. Nor does it run before the ladder, since
/// most resolutions end on an earlier rung. So a handler runs in up to two
/// passes, driven by [`resolve_lazily`] or, in the server,
/// [`resolve_lazily_off_worker`]:
///
/// 1. [`Deferred`](Self::Deferred): the structural rungs run as always; if
///    the ladder falls through to the semantic rung, the rung records the name
///    and the pass's answer is discarded.
/// 2. Only then, with the first pass's lock released, the name is embedded,
///    and the handler runs again with [`Embedded`](Self::Embedded). It reads
///    the index afresh, so the answer comes from one snapshot.
///
/// The cost on that path is the structural rungs read twice (a few indexed
/// lookups); on every other path, nothing.
///
/// Both passes carry the discovered languages' [`QueryShapes`], which decide
/// which queries and candidates the rung sets aside.
pub(crate) enum SemanticRung<'a> {
    /// First pass: reaching the rung records the name in `reached`.
    Deferred { embedding: &'a EmbeddingPipeline, shapes: &'a QueryShapes, reached: Cell<Option<String>> },
    /// Second pass: `name`'s query vector, embedded with no lock held. `None`
    /// when the model could not embed it, which refuses as before.
    Embedded { name: &'a str, query: Option<&'a [f32]>, shapes: &'a QueryShapes },
}

impl<'a> SemanticRung<'a> {
    pub(crate) fn deferred(embedding: &'a EmbeddingPipeline, shapes: &'a QueryShapes) -> Self {
        Self::Deferred { embedding, shapes, reached: Cell::new(None) }
    }

    /// A rung with no model behind it, for tests that pin the structural
    /// ladder: it refuses where the semantic rung would answer. It carries
    /// the shipped plugins' shapes.
    #[cfg(test)]
    pub(crate) fn off() -> SemanticRung<'static> {
        static DISABLED: std::sync::LazyLock<EmbeddingPipeline> =
            std::sync::LazyLock::new(EmbeddingPipeline::disabled);
        SemanticRung::deferred(&DISABLED, QueryShapes::shipped())
    }

    fn shapes(&self) -> &'a QueryShapes {
        match self {
            Self::Deferred { shapes, .. } | Self::Embedded { shapes, .. } => shapes,
        }
    }

    /// The name a [`Deferred`](Self::Deferred) pass stopped at the rung with,
    /// if it did.
    pub(crate) fn reached(&self) -> Option<String> {
        match self {
            Self::Deferred { reached, .. } => reached.take(),
            Self::Embedded { .. } => None,
        }
    }
}

/// Runs `handler` with the semantic rung deferred, then - only if the ladder
/// reached that rung - embeds the name with no lock held and runs it again
/// with the vector (see [`SemanticRung`]). `handler` must take and release
/// the store itself, so nothing is held between the passes. Synchronous: for
/// callers already off the async workers (the CLI, tests).
pub(crate) fn resolve_lazily(
    embedding: &EmbeddingPipeline,
    shapes: &QueryShapes,
    handler: impl Fn(&SemanticRung<'_>) -> Result<CallToolResult, ErrorData>,
) -> Result<CallToolResult, ErrorData> {
    let first = SemanticRung::deferred(embedding, shapes);
    let answer = handler(&first)?;
    let Some(name) = first.reached() else { return Ok(answer) };
    let query = embedding.embed_query(&name);
    handler(&SemanticRung::Embedded { name: &name, query: query.as_deref(), shapes })
}

/// [`resolve_lazily`] for the server: the first pass, plain store reads, runs
/// on the async worker as every structural tool does; the inference and the
/// second pass run on the blocking pool, so the model's load and the vector
/// scan never stall the other calls the daemon is serving. As with
/// `search_code::handle_off_worker`, dropping the future does not stop the
/// blocking work.
pub(super) async fn resolve_lazily_off_worker<H>(
    embedding: Arc<EmbeddingPipeline>,
    shapes: Arc<QueryShapes>,
    handler: H,
) -> Result<CallToolResult, ErrorData>
where
    H: Fn(&SemanticRung<'_>) -> Result<CallToolResult, ErrorData> + Send + 'static,
{
    let name = {
        let first = SemanticRung::deferred(&embedding, &shapes);
        let answer = handler(&first)?;
        match first.reached() {
            Some(name) => name,
            None => return Ok(answer),
        }
    };
    tokio::task::spawn_blocking(move || {
        let query = embedding.embed_query(&name);
        handler(&SemanticRung::Embedded { name: &name, query: query.as_deref(), shapes: &shapes })
    })
    .await
    .map_err(|e| internal_error("symbol resolution task failed", e.into()))?
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
/// # No specifier guard on the second arm
///
/// The `qualifiedName` arm needs no check for specifier-shaped names.
/// Import placeholders are excluded from `graph::queries`' lookups, and
/// every remaining declaration whose qualifiedName contains `/` or starts
/// with `@` is a `File` node, whose path is unique within a project. So
/// `exact.len() >= 2` cannot arise for a specifier-shaped query, and a
/// guard there could never fire. The shape check lives where it decides
/// something a score cannot: [`by_semantic_neighbours`].
///
/// `Ok(Ok(node))` is that node. `Ok(Err(result))` is a finished response the
/// caller must return unchanged - the ranked candidate page when the name is
/// ambiguous, or the not-found tool error - which is what lets the four
/// symbol-anchored tools accept a `symbol_name` (see `mcp::anchor`) and mean
/// exactly what `find_definition` means by it, down to the error text.
/// Shaped like `find_callers_callees`' old `resolve_anchor` rather than a
/// bespoke enum so every call site is the same two-line `match`.
///
/// `source_root` is where an ambiguous page reads its candidates' source from
/// (see [`CandidatePage::ambiguous`]); `None` leaves the candidates without
/// it. Only `find_definition` passes one: an anchored tool's answer per
/// candidate is a whole page of edges, not the declaration's text.
pub(super) fn resolve_symbol_name(
    conn: &Connection,
    semantic: &SemanticRung<'_>,
    name: &str,
    cursor: Option<&str>,
    source_root: Option<&Path>,
) -> Result<Result<Resolved, CallToolResult>, ErrorData> {
    let mut exact = queries::find_by_qualified_name(conn, name, None)
        .map_err(|e| internal_error("failed to look up node by qualifiedName", e))?;

    // The fast path, and the only single-query one: a genuinely qualified
    // spelling that exactly one declaration carries. `name != its own name`
    // is what "genuinely qualified" means here without this module having to
    // know any language's path separator - and it is the whole cost of the
    // change on the happy path, one string comparison on a row already read.
    if exact.len() == 1 && exact[0].name != name {
        return Ok(Ok(Resolved::new(exact.remove(0), ResolvedBy::QualifiedName)));
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
        let page = find_candidates_by_name(conn, column, &[Lookup::any(name)], cursor)
            .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
        return success(&CandidatePage::ambiguous(page, None, cursor, source_root)).map(Err);
    }

    match matches.into_iter().next() {
        // With one match and one exact row they are the same node - the query
        // is both its name and its whole qualifiedName - so the stronger rung
        // is reported, which is what keeps every language whose qualifiedNames
        // are bare by construction (TypeScript) answering exactly as it did.
        Some(node) => {
            let by = if exact.len() == 1 { ResolvedBy::QualifiedName } else { ResolvedBy::Name };
            Ok(Ok(Resolved::new(node, by)))
        }
        None => match by_qualified_name_suffix(conn, name, cursor, source_root)? {
            Some(answer) => Ok(answer),
            None => match by_stripped_prefix(conn, semantic.shapes(), name, cursor, source_root)? {
                Some(answer) => Ok(answer),
                None => by_file_name(conn, semantic, name),
            },
        },
    }
}

/// The qualifiedName-suffix rung: a partial path such as `IndexStore::read`,
/// which is neither a whole qualifiedName nor a declaration's name. Reached
/// only when the rungs above found nothing, so it never changes an answer
/// they give. The query is looked up as given, by exact equality, in the
/// partial paths the plugins' segments produced (`qualified_suffixes`, ADR
/// 0015): core never splits it and knows no language's separators. One
/// match resolves; several are the same ranked candidate page as an
/// ambiguous name. `None` means this rung has nothing to say and the ladder
/// goes on.
fn by_qualified_name_suffix(
    conn: &Connection,
    name: &str,
    cursor: Option<&str>,
    source_root: Option<&Path>,
) -> Result<Option<Result<Resolved, CallToolResult>>, ErrorData> {
    let mut matched = queries::find_by_qualified_suffix(conn, name)
        .map_err(|e| internal_error("failed to look up nodes by qualifiedName suffix", e))?;
    match matched.len() {
        0 => Ok(None),
        1 => Ok(Some(Ok(Resolved::new(matched.remove(0), ResolvedBy::QualifiedNameSuffix)))),
        _ => {
            let page = find_candidates_by_qualified_suffix(conn, &[Lookup::any(name)], cursor)
                .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
            success(&CandidatePage::ambiguous(page, None, cursor, source_root)).map(|page| Some(Err(page)))
        }
    }
}

/// The strip-prefix rung (`[plugin.symbol_query_prefixes]`, ADR 0019): the
/// query matched nothing as given, so each `(language, remainder)` pair from
/// [`QueryShapes::rewrites`] is looked up on the structural rungs above -
/// exact qualifiedName, name, qualifiedName suffix - among that language's
/// declarations only, and the rows are unioned over the languages. One row
/// resolves, labelled with its rung and `queriedAs`; several are the ranked
/// candidate page; none is `None`, and the ladder goes on with the original
/// query.
///
/// Invariant: reached only after every structural rung missed the original
/// query, so it never changes an answer they give; and it consults no rung
/// after them (file name, import note, semantic), which all see the
/// original query.
fn by_stripped_prefix(
    conn: &Connection,
    shapes: &QueryShapes,
    name: &str,
    cursor: Option<&str>,
    source_root: Option<&Path>,
) -> Result<Option<Result<Resolved, CallToolResult>>, ErrorData> {
    let mut lookups: Vec<Lookup<'_>> = Vec::new();
    for (language, remainder) in shapes.rewrites(name) {
        match lookups.iter_mut().find(|lookup| lookup.key == remainder) {
            Some(lookup) => lookup.languages.get_or_insert_with(Vec::new).push(language),
            None => lookups.push(Lookup { key: remainder, languages: Some(vec![language]) }),
        }
    }
    if lookups.is_empty() {
        return Ok(None);
    }
    // One label for a page only when every language stripped to the same name.
    let page_label = match lookups.as_slice() {
        [one] => Some(one.key),
        _ => None,
    };
    let rows =
        |find: &dyn Fn(&str) -> anyhow::Result<Vec<NodeRecord>>| -> anyhow::Result<Vec<(NodeRecord, &str)>> {
            let mut rows = Vec::new();
            for lookup in &lookups {
                rows.extend(
                    find(lookup.key)?
                        .into_iter()
                        .filter(|node| lookup.admits(node))
                        .map(|node| (node, lookup.key)),
                );
            }
            Ok(rows)
        };
    let answer = |(node, key): (NodeRecord, &str), by| {
        Ok(Some(Ok(Resolved { node, by, queried_as: Some(key.to_string()) })))
    };

    let mut exact = rows(&|key| queries::find_by_qualified_name(conn, key, None))
        .map_err(|e| internal_error("failed to look up node by qualifiedName", e))?;
    if exact.len() == 1 && exact[0].0.name != exact[0].1 {
        return answer(exact.remove(0), ResolvedBy::QualifiedName);
    }
    let mut matches = rows(&|key| queries::find_by_name(conn, key, None))
        .map_err(|e| internal_error("failed to look up node by name", e))?;
    // The same choice of page as `resolve_symbol_name` makes for the query.
    let ambiguous_over = match (matches.len(), exact.len()) {
        (2.., _) => Some(NameColumn::Name),
        (_, 2..) => Some(NameColumn::QualifiedName),
        _ => None,
    };
    if let Some(column) = ambiguous_over {
        let page = find_candidates_by_name(conn, column, &lookups, cursor)
            .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
        return success(&CandidatePage::ambiguous(page, page_label, cursor, source_root))
            .map(|page| Some(Err(page)));
    }
    if !matches.is_empty() {
        let by = if exact.len() == 1 { ResolvedBy::QualifiedName } else { ResolvedBy::Name };
        return answer(matches.remove(0), by);
    }

    let mut suffixed = rows(&|key| queries::find_by_qualified_suffix(conn, key))
        .map_err(|e| internal_error("failed to look up nodes by qualifiedName suffix", e))?;
    match suffixed.len() {
        0 => Ok(None),
        1 => answer(suffixed.remove(0), ResolvedBy::QualifiedNameSuffix),
        _ => {
            let page = find_candidates_by_qualified_suffix(conn, &lookups, cursor)
                .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
            success(&CandidatePage::ambiguous(page, page_label, cursor, source_root))
                .map(|page| Some(Err(page)))
        }
    }
}

/// How many neighbours to offer. The calibration found the correct hit ranked
/// first in 19 of 21 cases, so a long list would be payload without value.
const SEMANTIC_CANDIDATES: usize = 3;

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
/// 154 MiB weights), a query every discovered language declares is never its
/// symbol, or nothing scoring above its language's [`similarity::floor`]
/// once each language's own refused shapes are set aside: all three fall
/// through to the terse refusal this rung was added in front of, never to an
/// error.
///
/// # Shapes that are never a symbol
///
/// Each plugin declares, in `[plugin.non_symbol_queries]`, the query shapes
/// that are never its language's symbols ([`QueryShapes`]). A candidate of
/// language L is dropped when the query has one of L's shapes, next to the
/// floor; when every discovered language refuses the query, the rung stops
/// before the query is embedded. Core knows no shape itself
/// (`docs/adr/0018-non-symbol-query-shapes.md`).
///
/// This is checked by shape, not by score, because the score cannot catch
/// it. Package specifiers are the only kind of junk query that approaches
/// the threshold - `@excalidraw/element` scores 0.699 - and the reason is
/// structural: only doc comments and signatures are embedded, so a specifier
/// has nothing to match and similarity is computed against unrelated text.
/// The same string scores 0.566 against an index where that package does not
/// exist at all, which is the proof that the score describes the query's
/// shape and not the corpus. Raising the threshold to 0.70 would exclude it
/// too, and cost 42 points of recall to do so. Specifiers already have a
/// rung of their own - `get_dependencies`' path matching.
///
/// It is a spelling rule and not a lookup. Every specifier the index stores
/// is answered before this rung: a file path is a `File` node's
/// `qualifiedName` and resolves at the first rung, and an import
/// placeholder's specifier is answered by [`import_only_refusal`]. Of the
/// stored module keys, only a container key (`containers.key`) gets this
/// far. The specifiers the shapes exist for are exactly the ones no table
/// holds: a relative specifier such as `./extract.js`, which the index
/// records only as the file it resolves to, and a package this project never
/// imports. Measured on the TypeScript plugin's own sources, a lookup in
/// place of the shapes turned 55 of 401 queries' refusals into candidate
/// pages, 47 of them import specifiers written in those sources; on the Go
/// plugin's sources, 1 of 335, a synthetic `./extract`
/// (`docs/architecture/gm-474-qualified-name-segments.md`, section 3.5).
///
/// The cost is the other direction: a symbol-name query with a refused shape
/// that is not a specifier (`@Component`) is refused instead of being
/// offered neighbours.
///
/// # The floor is shared with `search_code`
///
/// This rung reads the per-language table in `mcp::similarity::floor`
/// rather than holding a constant of its own, although it only ever sees a
/// *symbol name* while `search_code` takes free text. The table must keep
/// this rung's false refusals (a top-3 page that held the right answer,
/// refused) at or under about 3% on name queries: a refusal is the expensive
/// error here, and a labelled "did you mean" on a hopeless query is cheap.
/// Changing the table changes this rung, so a change is re-checked on name
/// queries as well.
///
/// On name queries for the shipped model and text (int8, structured), the
/// table 0.57 / 0.59 / 0.57 / 0.53 (go / python / rust / typescript) refuses
/// 2.3% / 3.1% / 2.9% / 1.5% of such pages and offers candidates on 12% /
/// 11% / 26% / 24% of hopeless queries; a table fitted on names alone
/// differs by one to three queries per language
/// (`docs/results/gm-468-name-query-floors.md`).
///
/// # Never embeds itself
///
/// It runs under the store lock, so the query vector comes from outside: a
/// [`SemanticRung::Deferred`] pass only notes that it got here, and the
/// [`SemanticRung::Embedded`] pass that follows brings the vector.
fn by_semantic_neighbours(
    conn: &Connection,
    semantic: &SemanticRung<'_>,
    name: &str,
) -> Option<Result<CallToolResult, ErrorData>> {
    let shapes = semantic.shapes();
    if shapes.refused_by_all(name) {
        return None;
    }
    let query = match semantic {
        SemanticRung::Deferred { embedding, reached, .. } => {
            // A model already known to be absent embeds nothing, so there is
            // no second pass to defer to: refuse now, as `embed_query`'s
            // `None` would.
            if embedding.known_unavailable() {
                return None;
            }
            reached.set(Some(name.to_owned()));
            // A placeholder the driver discards for the second pass's answer.
            return None;
        }
        SemanticRung::Embedded { name: embedded, query, .. } => {
            debug_assert_eq!(*embedded, name, "the second pass resolves the name the first one stopped at");
            if *embedded != name {
                return None;
            }
            (*query)?
        }
    };
    let page = super::search_code::search(conn, query, SEMANTIC_CANDIDATES, None).ok()?;
    let results: Vec<DefinitionCandidate> = page
        .results
        .into_iter()
        .filter(|hit| hit.score >= similarity::floor(&hit.language) && !shapes.refuses(&hit.language, name))
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
    semantic: &SemanticRung<'_>,
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
        return match by_semantic_neighbours(conn, semantic, name) {
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
                start_line: Some(n.start_line),
                end_line: Some(n.end_line),
                end_col: Some(n.end_col),
                kind: n.kind.clone(),
                preview: n.signature.clone().or_else(|| n.doc_comment.clone()),
                source: None,
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
    semantic: &SemanticRung<'_>,
    name: &str,
    cursor: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    match resolve_symbol_name(conn, semantic, name, cursor, project_root)? {
        Ok(resolved) => success(&DefinitionNode::resolved(resolved).with_source(project_root)),
        Err(finished) => Ok(finished),
    }
}

/// `find_definition` by an exact node id - the follow-up an ambiguous page
/// asks for. The same lookup the anchored tools make for their `symbol_id`,
/// so the two refuse an unknown id in the same words.
fn by_symbol_id(
    conn: &Connection,
    project_root: Option<&Path>,
    symbol_id: &str,
) -> Result<CallToolResult, ErrorData> {
    match super::anchor::by_id(conn, symbol_id)? {
        Ok(resolved) => success(&DefinitionNode::resolved(resolved).with_source(project_root)),
        Err(finished) => Ok(finished),
    }
}

pub(crate) fn handle(
    store: &Arc<IndexStore>,
    project_root: &Path,
    embedding: &EmbeddingPipeline,
    shapes: &QueryShapes,
    params: FindDefinitionParams,
) -> Result<CallToolResult, ErrorData> {
    resolve_lazily(embedding, shapes, |semantic| {
        handle_in(store, project_root, semantic, None, params.clone())
    })
}

/// One pass of [`handle`] - see [`SemanticRung`]. `coverage` is
/// `params.file_path`'s (`PluginRegistry::path_coverage`), read only by the
/// `file_path` + `position` mode.
pub(super) fn handle_in(
    store: &Arc<IndexStore>,
    project_root: &Path,
    semantic: &SemanticRung<'_>,
    coverage: Option<&PathCoverage>,
    params: FindDefinitionParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = store.read();
    // Defaults to on. The snippet is the point of the field - a caller who
    // wants coordinates alone has to say so, rather than every caller having
    // to ask for the thing that saves them a round trip.
    let project_root = params.include_source.unwrap_or(true).then_some(project_root);

    // One addressing mode per call, as the anchored tools require: an id
    // given beside a name or a position would leave one of them unread, and
    // which one wins is not something a caller should have to guess.
    if let Some(symbol_id) = params.symbol_id {
        return match (params.symbol_name, params.file_path, params.position) {
            (None, None, None) => by_symbol_id(&conn, project_root, &symbol_id),
            _ => error("g-mesh: give `symbol_id` alone, without `symbol_name`, `file_path` or `position`"),
        };
    }

    match (params.file_path, params.position, params.symbol_name) {
        (Some(file_path), Some(position), _) => {
            by_position(&conn, project_root, coverage, &file_path, position.line, position.col)
        }
        (None, None, Some(name)) => by_name(&conn, project_root, semantic, &name, params.cursor.as_deref()),
        (None, None, None) if params.cursor.is_some() => {
            error("g-mesh: `cursor` continues a previous ambiguous symbol_name lookup - give the same symbol_name again")
        }
        (None, None, None) => {
            error("g-mesh: give `symbol_id`, `symbol_name`, or both `file_path` and `position`")
        }
        _ => error("g-mesh: `file_path` and `position` must be given together"),
    }
}

#[cfg(test)]
mod tests;
