use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::graph::containers::CONTAINER_NATIVE_KIND;
use crate::graph::imports::{EXTERNAL_MODULE_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND};
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

// Every "which symbol is this?" lookup below - by name, by qualifiedName, by
// position, or by the stem of the file it sits in ([`find_in_file_named`]) -
// answers with a *declaration* and nothing else, and so does
// `mcp::find_definition`'s candidate page built on top of them.
// [`NON_DECLARATION_NATIVE_KINDS`] is the one list of what that excludes, and
// [`declaration_only`] the one SQL condition built from it, so the four
// lookups cannot drift apart the way they had. `find_in_file_named` is the
// one GM-376 folded in last: until then it spelled two of the five kinds
// itself, as bound parameters rather than this module's interpolated
// constants - see that function's own doc for what else it excludes and why.
//
// Four of the five excluded kinds are addresses rather than declarations. A
// *pending symbol* placeholder carries an imported symbol's own name; a
// *re-export* one the name a barrel republishes; a *resolved module* one the
// specifier of an import some file of this project satisfies; an *external
// module* one the specifier of an import nothing here satisfies at all. Every
// one of them is the graph's record that a file reached for something. None
// has a body, a signature worth reading, or a definition site: their
// `filePath` and range point at the `import`/`use` line that produced them,
// so an answer built on one sends the caller to an import statement and calls
// it a definition.
//
// The fifth, core's own container node
// (`graph::containers::CONTAINER_NATIVE_KIND`), is excluded for a different
// reason with the same effect: it is a real node, but it has no source -
// `filePath` is `''` and its range is zero - so an answer built on one (a
// source snippet, a staleness check on its file, an anchor echo telling the
// caller where it lives) points at nothing. Its name is also its whole key
// (`github.com/x/app/server`), which no caller asking "where is `server`
// defined" writes.
//
// The file lookups below - [`find_file_node`], [`find_files_under`],
// [`find_files_ending_in_dir`] - need no such filter: they require `kind =
// 'File'` rather than refuse anything, so none of the five excluded native
// kinds (none of which is ever stored as `kind = 'File'`) can reach them.
//
// ## Excluded, not marked (GM-367)
//
// `resolved_module` and `external_module` were the two missing from this
// list, and the cost was measured rather than argued. On a gin index
// (schema 8, indexer 2), `find_definition("context")` answered with the
// import placeholder in `context_test.go` - unflagged, and labelled
// `resolvedBy: "qualifiedName"`, the strongest confidence marker this tool
// surface has - and `find_definition("http")` offered 63 of them, one per
// importing file, as a ranked candidate page.
//
// The alternative weighed was to keep them and *mark* them, the shape
// `resolvedFrom` and `excludedReferences` use elsewhere here: narrow the
// answer and say that you did. It lost on three counts.
//
//  - **A marker has nowhere to live.** Four of the five tools reading these
//    lookups anchor on a single node rather than listing rows, so there is no
//    per-row field to carry it; `mcp::anchor::AnchorInfo` would have to grow
//    one, on every response, for a value no tool can act on.
//  - **It would single out two of five kinds** for visibility while the other
//    three stayed silently excluded - a distinction with no principle behind
//    it, in the one place a caller is entitled to assume the list is
//    coherent.
//  - **A labelled placeholder is still not an answer.** It has no definition
//    site in this project, so the only move it leaves the caller is to
//    re-query - the round trip the label was supposed to save.
//
// What the caller genuinely wants said is said where it is actionable rather
// than on a row that should not be there: see
// `mcp::find_definition::import_only_refusal`, which turns the refusal these
// exclusions produce into a sentence naming the specifier and the tool that
// does answer for it.
//
// `graph::symbol_links::is_declaration` asks the same question one step
// earlier - which nodes may be *linked onto*, while the index is being
// written - and since GM-372 it reads this same list rather than a fourth
// copy of it. GM-367 left it a four-kind copy on purpose: it was missing
// `external_module`, and adding a kind there repoints edges during linking
// rather than narrowing an answer at query time, which is evidence of a
// different kind (a re-index and a before/after edge diff on real corpora,
// not a query sweep). GM-372 collected it - no edge moved on go-gin,
// rs-ripgrep or py-requests, because a plugin's import records are
// `file`-visible and the linker's visibility check was already refusing them
// one step later - and excluded the kind anyway, on the contract rather than
// on observed damage. That module's `is_declaration` carries the argument.

/// The `nativeKind`s that never answer a name, qualifiedName or position
/// lookup - see this module's header for what each one is and why it is here.
/// One list, because [`find_by_name`], [`find_by_qualified_name`],
/// [`find_by_position`] and `mcp::find_definition`'s candidate page all have
/// to agree about it.
pub(crate) const NON_DECLARATION_NATIVE_KINDS: [&str; 5] = [
    PENDING_SYMBOL_NATIVE_KIND,
    REEXPORT_NATIVE_KIND,
    RESOLVED_MODULE_NATIVE_KIND,
    EXTERNAL_MODULE_NATIVE_KIND,
    CONTAINER_NATIVE_KIND,
];

/// [`NON_DECLARATION_NATIVE_KINDS`] as a SQL condition on a `nodes` row.
/// `prefix` is the table alias and its dot (`"n."`), or `""` for an
/// unaliased `nodes`.
///
/// `nativeKind IS NULL OR ... NOT IN (...)` rather than the chain of `IS NOT`
/// this replaces: an ordinary declaration's `nativeKind` *is* NULL, and
/// `NULL NOT IN (...)` evaluates to NULL rather than true, so without the
/// explicit null arm the filter would exclude every real declaration. The
/// values interpolated are this crate's own `&'static str` constants and
/// never caller input, which is why they are written into the SQL rather than
/// bound - binding five of them at four call sites is what let the lists
/// drift in the first place.
pub(crate) fn declaration_only(prefix: &str) -> String {
    let kinds = NON_DECLARATION_NATIVE_KINDS.map(|kind| format!("'{kind}'")).join(", ");
    format!("({prefix}nativeKind IS NULL OR {prefix}nativeKind NOT IN ({kinds}))")
}

pub(crate) fn map_node_row(row: &Row) -> rusqlite::Result<NodeRecord> {
    Ok(NodeRecord {
        id: row.get("id")?,
        kind: row.get("kind")?,
        name: row.get("name")?,
        qualified_name: row.get("qualifiedName")?,
        file_path: row.get("filePath")?,
        start_line: row.get("startLine")?,
        start_col: row.get("startCol")?,
        end_line: row.get("endLine")?,
        end_col: row.get("endCol")?,
        signature: row.get("signature")?,
        // `exported` is read straight off the database's own `GENERATED
        // ALWAYS` column (`storage::schema`'s DDL) - it is guaranteed to
        // already agree with `visibility` below, since nothing can write it
        // any other way. See `NodeRecord.exported`'s own doc comment.
        exported: row.get("exported")?,
        visibility: row.get("visibility")?,
        visibility_container: row.get("visibilityContainer")?,
        container: row.get("container")?,
        // Not a `nodes` column - it lives on the container's own
        // `containers.parentKey` - so there is nothing to read it from here.
        // See `NodeRecord.container_parent` for why a record read this way
        // must not be written straight back.
        container_parent: None,
        // Deliberately not joined, same reasoning as `declarations` just
        // below: a read via this function is never handed back to
        // `apply_diff` expecting an existing `placeholder_targets` row to be
        // preserved (see `NodeRecord.target`'s own doc comment for why that
        // would be actively wrong - `apply_diff` reads a `None` here as
        // "delete this node's target").
        target: None,
        doc_comment: row.get("docComment")?,
        language: row.get("language")?,
        native_kind: row.get("nativeKind")?,
        has_syntax_errors: row.get("hasSyntaxErrors")?,
        // Deliberately not joined: almost no node has declaration rows, and
        // every reader of this function today asks about the symbol as a
        // whole, which the flat fields above already answer. See the field's
        // own doc comment for why a record read this way must not be written
        // straight back.
        declarations: Vec::new(),
    })
}

fn map_edge_row(row: &Row) -> rusqlite::Result<EdgeRecord> {
    Ok(EdgeRecord {
        id: row.get("id")?,
        from_id: row.get("fromId")?,
        to_id: row.get("toId")?,
        kind: row.get("kind")?,
        source: row.get("source")?,
        engine: row.get("engine")?,
        resolved: row.get("resolved")?,
        to_declaration: row.get("toDeclaration")?,
    })
}

pub fn upsert_node(conn: &mut Connection, node: NodeRecord) -> Result<()> {
    write::apply_diff(conn, &Diff { upsert_nodes: vec![node], ..Default::default() })
}

pub fn get_node(conn: &Connection, id: &str) -> Result<Option<NodeRecord>> {
    conn.query_row("SELECT * FROM nodes WHERE id = ?1", params![id], map_node_row)
        .optional()
        .context("failed to look up node by id")
}

/// Deletes a node and every edge incident to it (fromId or toId), atomically.
pub fn delete_node(conn: &mut Connection, id: &str) -> Result<()> {
    let mut stmt = conn.prepare("SELECT id FROM edges WHERE fromId = ?1 OR toId = ?1")?;
    let incident_edge_ids: Vec<String> = stmt
        .query_map(params![id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()
        .context("failed to look up incident edges")?;
    drop(stmt);

    write::apply_diff(
        conn,
        &Diff {
            delete_edge_ids: incident_edge_ids,
            delete_node_ids: vec![id.to_string()],
            ..Default::default()
        },
    )
}

pub fn find_by_name(conn: &Connection, name: &str, file_path: Option<&str>) -> Result<Vec<NodeRecord>> {
    let declaration = declaration_only("");
    let mut stmt = match file_path {
        Some(_) => {
            conn.prepare(&format!("SELECT * FROM nodes WHERE name = ?1 AND {declaration} AND filePath = ?2"))?
        }
        None => conn.prepare(&format!("SELECT * FROM nodes WHERE name = ?1 AND {declaration}"))?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(params![name, fp], map_node_row)?,
        None => stmt.query_map(params![name], map_node_row)?,
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by name")
}

/// Nodes declared in a file whose stem is `name` - `DropdownMenuGroup` finds
/// `.../DropdownMenuGroup.tsx`.
///
/// Exists for one failure that looks like a bug to whoever hits it: a default
/// import binds the exporting file's declaration under whatever local name the
/// importer chose (`import DropdownMenuGroup from "./DropdownMenuGroup"`), and
/// that local name is never indexed - see `graph::symbol_links`' module doc,
/// "the local name never reaches this index at all". So the name a caller is
/// reading at every use site resolves to nothing, while the declaration it
/// binds sits in the index under a different name. The file's own name is the
/// one link between them that the index does hold.
///
/// Only ever called on the miss path, which is why a `LIKE` with a leading
/// wildcard is acceptable here and would not be on a hot one. Case-insensitive
/// by SQLite's default ASCII `LIKE`, deliberately: `import Foo from "./foo"` is
/// the same situation.
///
/// # Ranked by inbound edges, not by source order (GM-373)
///
/// A file's declarations arrive here as a *page*, and `limit` cuts it, so the
/// order decides what the caller sees at all. It used to be `exported DESC,
/// startLine ASC`, which is not so much wrong as uninformed: source order is a
/// proxy for importance only by accident, and `exported` does not separate a
/// constant from the type the file is named after. On a gin index,
/// `find_definition("context")` reached this rung and answered with
/// `MIMEJSON`, `MIMEHTML`, `MIMEXML`, `MIMEXML2`, `MIMEPlain` - `context.go`'s
/// first five exported constants - while `Context`, the type the file exists
/// for, sat 90-odd declarations further down and never made the page.
///
/// So the primary key is now the inbound `REFERENCES`+`CALLS` count, the same
/// "how central is this symbol" proxy `mcp::find_definition`'s
/// `find_candidates_by_name` ranks its own candidate page by (GM-360) - the
/// two rungs that answer with candidates now order them by one rule rather
/// than two. It separates this case by a wide margin, measured on the three
/// probe indexes: in `context.go`, `Context` has 335 inbound edges against
/// `MIMEJSON`'s 15 and `MIMEXML`'s 17.
///
/// `exported DESC, startLine ASC` stays as the tie-break, which is what makes
/// the change a refinement rather than a replacement: where the graph has
/// nothing to say - every candidate at zero, as in ripgrep's `disabled.rs` -
/// the page is byte for byte the one this function returned before.
///
/// # How many queries this is actually about
///
/// 107, swept rather than guessed, because a rung tuned on one query is a rung
/// measured on one query. The set that can reach here is exactly "a file stem,
/// or a case variant of one, that no declaration carries as its `name` or
/// `qualifiedName`"; enumerated over the three probe indexes it is 84 queries
/// on gin, 12 on requests and 11 on ripgrep. Of those 107 pages, 27 come back
/// identical, 13 are reordered behind an unchanged first row, and 67 have a
/// new first row. No query changes *rung*: the row set is untouched and only
/// its order moves, so nothing that resolved starts refusing or the reverse.
///
/// # What lost, and why it is written down
///
/// Preferring a declaration whose own name *is* the file's stem (`Context` in
/// `context.go`) is the other obvious rule. It is not a bad rule, which is why
/// the reason it lost has to be the measured one rather than a slogan: over
/// the 107 it disagrees with the edge count about the first row 11 times, and
/// it is better in 7 of them - `Binding` over `BindingBody`, `RouterGroup`
/// over `RouterGroup.GET`, requests' `Server` over `consume_socket_content`.
///
/// It lost on the other 4, and on what the 7 are worth given the ranking:
///
/// - **A stem match cannot tell a subject from a member or a fixture**, and
///   the graph has no language-agnostic way to say which it has - `name` vs
///   `qualifiedName` separates the two in Go and not in Rust, where every
///   top-level declaration is module-qualified too. So the rule puts
///   `errorMsgs.Errors` above the `errorMsgs` type, and - the case that
///   decides it - ripgrep's `literal::tests::literal`, a test function with
///   **0** inbound edges, above `literal::TSeq` with 20. Handing back a
///   test fixture as *the* answer is the exact failure GM-360 removed from
///   the bare-name rung; re-creating it here, one rung down, for a spelling
///   coincidence, is not a trade worth making.
/// - **The ranking already carries those 7 onto the page**, which is the
///   number that shrinks the win. Among them the stem-named declaration ranks
///   in the top 5 in 5 cases after this change (`Binding` 15th -> 2nd,
///   `RouterGroup` 22nd -> 3rd) against 2 before it, and in no case does one
///   that was on the page fall off. The tie-break would move a row the caller
///   can already see to the top; the cost is the bullet above.
///
/// The residue is worth naming rather than hiding: the two the ranking does
/// push off the page, gin's `Logger` (29th) and `Mode` (16th), are *public
/// API a project barely uses itself*. Inbound edges measure internal use, so
/// an exported entry point with one caller in-tree will always rank below a
/// well-used helper. That is the known blind spot of this proxy, here and in
/// `find_candidates_by_name`, and the honest bound on what it claims.
///
/// The `limit` its caller passes was also re-examined and deliberately left at
/// five. An order that means something makes the *marginal* row worth less,
/// not more: under source order row 6 was as good a guess as row 1, so the cap
/// was where the arbitrariness showed, whereas now row 6 is by construction
/// the sixth most-referenced. Widening it would add payload to a page that is
/// a "did you mean", not an answer.
///
/// # What is excluded, and why `kind = 'Module'` is not part of it (GM-376)
///
/// This lookup's `WHERE` matches on `filePath` alone, so unlike
/// [`find_by_name`]/[`find_by_qualified_name`]/[`find_by_position`] it can
/// return *anything* declared in the file - which is exactly why it needs
/// [`declaration_only`] too: an import placeholder or a container row sitting
/// in the same file is not a declaration this rung may offer, any more than
/// it is one those three may. Before GM-376 this spelled two of the five
/// excluded native kinds itself (`pending_symbol`, `reexport`, bound as
/// `?2`/`?3`) and missed the other three (`resolved_module`,
/// `external_module`, `container`) - not a wrong answer for any of these
/// today, but for a reason this function did not itself hold: see the module
/// header's "What is excluded" for why a fifth kind of row, `kind = 'File'`,
/// still needs its own clause.
///
/// `kind = 'Module'` is *not* folded into that clause, and its removal is a
/// correctness fix, not a no-op: every one of the five excluded native kinds
/// happens to be stored under `kind = 'Module'` today (a plugin's placeholder
/// rows and `graph::containers`' own container rows both are), so
/// `declaration_only` already refuses all of them without any help from
/// `kind`. But `kind = 'Module'` refused more than those five - a TypeScript
/// `namespace` is a real declaration, with a `DEFINES` edge from its file and
/// members of its own, stored as `kind: "Module", nativeKind: "namespace"`
/// (plugins/typescript/src/extract.ts). Before this change, asking this rung
/// for a name that only a namespace in some file carried came back empty;
/// `tests::a_namespace_stored_as_kind_module_is_still_offered` and
/// `tests::an_import_record_stored_as_a_non_module_kind_is_still_excluded`
/// below pin the two directions of this: dropping `kind = 'Module'` lets the
/// namespace through, and `declaration_only` alone still refuses an import
/// record that happens not to be stored as `kind = 'Module'`.
pub fn find_in_file_named(conn: &Connection, name: &str, limit: usize) -> Result<Vec<NodeRecord>> {
    // A name carrying a separator or an extension is not a module stem, and
    // would turn the patterns below into something that matches far too much.
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Ok(Vec::new());
    }
    let declaration = declaration_only("");
    let mut stmt = conn.prepare(&format!(
        "SELECT * FROM nodes \
         WHERE (filePath LIKE '%/' || ?1 || '.%' OR filePath LIKE ?1 || '.%') \
           AND {declaration} \
           AND kind IS NOT 'File' \
         ORDER BY (SELECT COUNT(*) FROM edges e \
                   WHERE e.toId = nodes.id AND e.kind IN ('REFERENCES', 'CALLS')) DESC, \
                  exported DESC, startLine ASC \
         LIMIT ?2",
    ))?;
    let rows = stmt.query_map(params![name, limit as i64], map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by file name")
}

/// Builds the boolean `ORDER BY` expression (and the `?` parameter values it
/// binds, in the order they appear in it) that ranks a `filePath` matching
/// any of `entry_points` ahead of one that does not - the shared core behind
/// [`find_files_under`]'s and [`find_files_ending_in_dir`]'s ordering.
///
/// # Where `entry_points` comes from, and why this function does not know
///
/// `entry_points` is the caller's job to assemble, concretely the union of
/// every discovered plugin's `[plugin.workspace] entry_points`
/// (`daemon::manifest::WorkspaceConfig`,
/// `daemon::registry::PluginRegistry::entry_points`), passed in rather than
/// looked up here so this module, which is a thin SQL layer under
/// `storage`/`graph`, never has to depend on `daemon::manifest` to answer a
/// query. A caller with no manifest available at all (this module's own
/// tests below; a hypothetical CLI path that never starts a daemon) passes an
/// empty slice, which this function turns into an always-false expression:
/// nothing ranks first, and both callers fall back to their older,
/// entry-point-blind `LENGTH(filePath)` ordering. The one place this must
/// stay identical to the pre-task behaviour is the bundled setup: the
/// bundled TS plugin's own manifest already declares `entry_points =
/// ["index"]` (`plugins/typescript/plugin.toml`), so a real daemon feeds this
/// function exactly the one-element list that reproduces the old hardcoded
/// `index.*` ordering, byte for byte.
///
/// # Matching semantics: one rule, two shapes
///
/// Each entry is tested one of two ways, chosen by whether it contains a `.`:
///
/// - **No dot** (`"index"`): matches the file's *stem*, any extension -
///   `index.ts`, `index.tsx`, `index.d.ts` all qualify. This is the exact
///   `%/index.%` shape the code being replaced hardcoded.
/// - **Has a dot** (`"mod.rs"`, `"main.rs"`, `"lib.rs"` - Rust's own
///   convention, once a Rust plugin declares it): matches the *exact* file
///   name, with no trailing wildcard after it - an exact entry must not also
///   match `mod.rs.bak` or `mod.rs2`, which a `LIKE 'mod.rs%'` pattern would.
///
/// One rule rather than two independently configurable modes, because a
/// manifest author choosing an entry point only has one real degree of
/// freedom: whether their convention is extension-agnostic (TS's `index`,
/// which must cover `.ts`/`.tsx`/`.d.ts`) or a fixed file name (Rust's
/// `mod.rs`, which must not smear onto neighbouring names).
///
/// # Every entry is escaped before it ever reaches a `LIKE`
///
/// `entry_points` is manifest content, not a literal this module wrote, so it
/// cannot be trusted to contain no LIKE metacharacters - `%` and `_` are both
/// wildcards to SQLite's `LIKE` (`_` matches exactly one arbitrary character),
/// and a manifest is free to declare an entry point that legitimately
/// contains one: Python's own `__init__.py` convention is the motivating
/// case. Left unescaped, `__init__.py` would rank `pkg/abinitcd.py` as if it
/// were the declared entry point - each of its two `_` wildcards consuming
/// one arbitrary character - which is exactly the "language #N+1 pays for
/// core surgery" trap this task exists to close, just moved from a missing
/// parameter to a missed escape. [`escape_like`] backslash-escapes `\`, `%`
/// and `_` in every entry before it is bound, and every `LIKE` clause below
/// carries the matching `ESCAPE '\'`; the equality clause does not, because
/// `=` has no wildcards to escape in the first place.
///
/// # Cost: not an index range scan today, and this does not make it one
///
/// It would be convenient to say this stays "an indexed prefix lookup", but
/// `EXPLAIN QUERY PLAN` on the query both callers run
/// (`WHERE kind = 'File' AND filePath LIKE ?1 || '/%' ORDER BY ...`) says
/// otherwise: it is a full `SCAN nodes`, index or no index, and always has
/// been - `idx_nodes_filePath` (`storage::schema`) never fires here, because
/// SQLite's LIKE-to-range-scan optimization only applies to a *case-sensitive*
/// LIKE (`PRAGMA case_sensitive_like = ON`, or `GLOB`), and this project sets
/// neither; measured directly against a populated `nodes` table with
/// `ANALYZE` run, `filePath LIKE 'pkg1/%'` alone still plans as `SCAN nodes`,
/// while the equivalent `filePath GLOB 'pkg1/*'` plans as
/// `SEARCH nodes USING INDEX idx_nodes_filePath`. Both `find_files_under` and
/// `find_files_ending_in_dir` are miss-path-only (see their own doc comments)
/// and already paid for a full scan before this task. What this function
/// must not do - and does not - is turn one full scan into several, or into
/// one whose per-row cost grows with the project: it stays one query, and the
/// only new per-row cost is evaluating up to `entry_points.len()` extra
/// `LIKE` tests (in practice 1-3, one convention per language actually
/// discovered) instead of the previous single hardcoded one - a constant
/// factor, not a new scan.
fn entry_point_rank_expr(entry_points: &[String]) -> (String, Vec<String>) {
    if entry_points.is_empty() {
        // Not a bare `"0"`: SQLite's `ORDER BY` treats a literal integer as a
        // 1-based reference to a column of the result set ("ORDER BY 1" means
        // "by the first selected column"), so a bare `0` is a "column out of
        // range" error, not a constant `false` - `(1 = 0)`, an expression
        // rather than an integer literal, is what actually means "always
        // false" here.
        return ("(1 = 0)".to_string(), Vec::new());
    }
    let mut clauses: Vec<&'static str> = Vec::with_capacity(entry_points.len());
    let mut params = Vec::with_capacity(entry_points.len() * 2);
    for entry in entry_points {
        let escaped = escape_like(entry);
        if entry.contains('.') {
            // Exact name: the file's own path either ends in "/<entry>" or,
            // for a root-level file, equals <entry> outright. The first
            // branch is a plain `=`, so the raw (unescaped) entry is bound
            // there - only the `LIKE` branch needs the escaped form.
            clauses.push("(filePath = ? OR filePath LIKE '%/' || ? ESCAPE '\\')");
            params.push(entry.clone());
            params.push(escaped);
        } else {
            // Bare stem: the file's own name starts with "<entry>." at the
            // root, or "/<entry>." under some directory - any extension.
            clauses
                .push("(filePath LIKE ? || '.%' ESCAPE '\\' OR filePath LIKE '%/' || ? || '.%' ESCAPE '\\')");
            params.push(escaped.clone());
            params.push(escaped);
        }
    }
    (format!("({})", clauses.join(" OR ")), params)
}

/// Escapes `\`, `%` and `_` in `entry` so it can be interposed into a `LIKE`
/// pattern as a literal string rather than a pattern of its own - paired with
/// `ESCAPE '\'` on every clause that binds the result. See
/// [`entry_point_rank_expr`]'s own doc comment for why an unescaped entry is
/// a real bug and not a theoretical one (`__init__.py`), not just a stylistic
/// nicety.
fn escape_like(entry: &str) -> String {
    let mut escaped = String::with_capacity(entry.len());
    for ch in entry.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Indexed files sitting under `prefix`, entry points first.
///
/// For the caller who asked about a package or a directory rather than a
/// file: `packages/math` is not a node, but `packages/math/src/index.ts` is,
/// and it is what they meant. Ordering puts a declared entry point first -
/// see [`entry_point_rank_expr`] for the matching rule, where `entry_points`
/// comes from, and this query's actual cost - then shortest path, so the head
/// of the list is the entry point rather than whichever file sorted first.
///
/// Miss path only, like `find_in_file_named` above - a `LIKE` anchored on a
/// prefix can use no index here and does not need to (see
/// [`entry_point_rank_expr`]'s cost section for the measurement behind that).
pub fn find_files_under(
    conn: &Connection,
    prefix: &str,
    entry_points: &[String],
    limit: usize,
) -> Result<Vec<NodeRecord>> {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let (rank_expr, rank_params) = entry_point_rank_expr(entry_points);
    let sql = format!(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND filePath LIKE ? || '/%' \
         ORDER BY {rank_expr} DESC, LENGTH(filePath) ASC \
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    // A `Vec<rusqlite::types::Value>` bound via `params_from_iter`, rather
    // than `params![...]`/a `Vec<&dyn ToSql>`, because the parameter count
    // varies with `entry_points.len()` - `Value` is rusqlite's own "any bound
    // type, decided at runtime" wrapper, exactly what a variable-length
    // parameter list needs.
    let mut bound: Vec<rusqlite::types::Value> = Vec::with_capacity(2 + rank_params.len());
    bound.push(trimmed.to_string().into());
    bound.extend(rank_params.into_iter().map(Into::into));
    bound.push((limit as i64).into());
    let rows = stmt.query_map(rusqlite::params_from_iter(bound), map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files under a prefix")
}

/// Indexed files under any directory named `dir`, entry points first.
///
/// The second half of the package-name case: `@excalidraw/math` is not a path,
/// but a directory called `math` exists and holds the files. Matches a path
/// segment, not a substring - `/math/` - so `mathutils` does not qualify.
/// Entry-point ordering, `entry_points`'s meaning and this query's cost are
/// exactly [`find_files_under`]'s - see [`entry_point_rank_expr`].
pub fn find_files_ending_in_dir(
    conn: &Connection,
    dir: &str,
    entry_points: &[String],
    limit: usize,
) -> Result<Vec<NodeRecord>> {
    if dir.is_empty() || dir.contains('/') {
        return Ok(Vec::new());
    }
    let (rank_expr, rank_params) = entry_point_rank_expr(entry_points);
    let sql = format!(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND (filePath LIKE '%/' || ? || '/%' OR filePath LIKE ? || '/%') \
         ORDER BY {rank_expr} DESC, LENGTH(filePath) ASC \
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut bound: Vec<rusqlite::types::Value> = Vec::with_capacity(3 + rank_params.len());
    bound.push(dir.to_string().into());
    bound.push(dir.to_string().into());
    bound.extend(rank_params.into_iter().map(Into::into));
    bound.push((limit as i64).into());
    let rows = stmt.query_map(rusqlite::params_from_iter(bound), map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files by directory name")
}

pub fn find_by_qualified_name(
    conn: &Connection,
    qualified_name: &str,
    file_path: Option<&str>,
) -> Result<Vec<NodeRecord>> {
    let declaration = declaration_only("");
    let mut stmt = match file_path {
        Some(_) => conn.prepare(&format!(
            "SELECT * FROM nodes WHERE qualifiedName = ?1 AND {declaration} AND filePath = ?2"
        ))?,
        None => conn.prepare(&format!("SELECT * FROM nodes WHERE qualifiedName = ?1 AND {declaration}"))?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(params![qualified_name, fp], map_node_row)?,
        None => stmt.query_map(params![qualified_name], map_node_row)?,
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by qualifiedName")
}

/// The distinct specifiers of the *import placeholders* a spelling matches,
/// each with how many of them carry it - most first.
///
/// The mirror image of [`find_by_name`]: it answers over exactly the module
/// placeholder rows that one excludes (`resolved_module` and
/// `external_module`; the two symbol placeholders carry a symbol's name, not
/// a specifier, and belong to `graph::symbol_links`' own lookups). Its only
/// caller is `mcp::find_definition::import_only_refusal`, which is why it
/// returns counts rather than rows: `net/http` on gin is 63 identical
/// placeholders, and the useful fact about them is that there are 63, not
/// where each one sits.
///
/// Matches either column, because a caller types either half of the same
/// import - Go's `http` and `net/http`, Python's `path` and `os.path` - and
/// both are equally not a definition.
pub fn import_specifiers_named(conn: &Connection, name: &str) -> Result<Vec<(String, usize)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT qualifiedName AS specifier, COUNT(*) AS carriers FROM nodes \
         WHERE (name = ?1 OR qualifiedName = ?1) \
           AND nativeKind IN ('{RESOLVED_MODULE_NATIVE_KIND}', '{EXTERNAL_MODULE_NATIVE_KIND}') \
         GROUP BY qualifiedName \
         ORDER BY carriers DESC, specifier ASC"
    ))?;
    let rows = stmt.query_map(params![name], |row| {
        Ok((row.get::<_, String>("specifier")?, row.get::<_, i64>("carriers")? as usize))
    })?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up import placeholders by name")
}

/// Finds the `File` node for a project-relative path, e.g. resolving
/// `get_file_outline`'s anchor. `File` nodes' own `filePath` is the path
/// itself (see the js-ts plugin's extractor), so this is a plain lookup, not
/// a join.
pub fn find_file_node(conn: &Connection, file_path: &str) -> Result<Option<NodeRecord>> {
    conn.query_row(
        "SELECT * FROM nodes WHERE kind = 'File' AND filePath = ?1",
        params![file_path],
        map_node_row,
    )
    .optional()
    .context("failed to look up file node")
}

/// Whether any edge of `edge_kind` arrives at `node_id` - one keyed probe
/// (`idx_edges_toId`), not a count and not a walk.
///
/// `get_dependencies::from_file` asks this of a `File` node before deciding
/// that an `Incoming` walk from it could only ever be empty (GM-356). The
/// question is deliberately about the *graph*, not about a walk's result: a
/// file that is genuinely an import target keeps its literal anchor, and a
/// substitution can therefore never replace an answer that had rows in it.
pub fn has_incoming_edge(conn: &Connection, node_id: &str, edge_kind: &str) -> Result<bool> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM edges WHERE toId = ?1 AND kind = ?2 LIMIT 1",
            params![node_id, edge_kind],
            |row| row.get(0),
        )
        .optional()
        .context("failed to probe a node's incoming edges")?;
    Ok(hit.is_some())
}

/// Finds the container node(s) whose own key is exactly `key`, across every
/// language a container of that key exists in - the counterpart of
/// [`find_file_node`] for a logical container (a Go import path, a Rust
/// module path, ... - Data Model > Logical containers) rather than a file.
/// Used by `get_dependencies::from_file` (GM-267) to let a caller anchor a
/// walk on a package name directly instead of hunting for one of its files.
///
/// A key is only unique *within* a language (`containers.key`'s own `UNIQUE
/// (language, key)`), so an exact key can legitimately name more than one
/// container project-wide - a Go package and a Rust module happening to
/// share the string. This returns every match rather than picking one; the
/// caller decides what "more than one" means (refuse and name the languages,
/// for `get_dependencies`). One indexed lookup (`containers.key`'s own
/// implicit index, part of `UNIQUE (language, key)`) joined back onto
/// `nodes` by primary key - not a scan of either table.
pub fn find_containers_by_key(conn: &Connection, key: &str) -> Result<Vec<NodeRecord>> {
    let mut stmt = conn
        .prepare("SELECT n.* FROM nodes n JOIN containers c ON c.nodeId = n.id WHERE c.key = ?1 ORDER BY c.language")
        .context("failed to prepare the container lookup")?;
    let rows = stmt.query_map(params![key], map_node_row).context("failed to look up containers by key")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read containers by key")
}

/// Finds the innermost node enclosing a cursor position, e.g. resolving
/// `find_definition`'s file+position input. Multiple nodes can contain a
/// position (a `File` spans the whole file, a `Function` inside it spans
/// just itself) - ordering by span size ascending picks the smallest one
/// first, which is always the most specific.
pub fn find_by_position(
    conn: &Connection,
    file_path: &str,
    line: u32,
    col: u32,
) -> Result<Option<NodeRecord>> {
    let (line, col) = (line as i64, col as i64);
    conn.query_row(
        &format!(
            "SELECT * FROM nodes \
             WHERE filePath = ?1 \
               AND {} \
               AND (startLine < ?2 OR (startLine = ?2 AND startCol <= ?3)) \
               AND (endLine > ?2 OR (endLine = ?2 AND endCol >= ?3)) \
             ORDER BY (endLine - startLine) ASC, (endCol - startCol) ASC \
             LIMIT 1",
            declaration_only("")
        ),
        params![file_path, line, col],
        map_node_row,
    )
    .optional()
    .context("failed to look up node by position")
}

pub fn upsert_edge(conn: &mut Connection, edge: EdgeRecord) -> Result<()> {
    write::apply_diff(conn, &Diff { upsert_edges: vec![edge], ..Default::default() })
}

pub fn get_edge(conn: &Connection, id: &str) -> Result<Option<EdgeRecord>> {
    conn.query_row("SELECT * FROM edges WHERE id = ?1", params![id], map_edge_row)
        .optional()
        .context("failed to look up edge by id")
}

pub fn delete_edge(conn: &mut Connection, id: &str) -> Result<()> {
    write::apply_diff(conn, &Diff { delete_edge_ids: vec![id.to_string()], ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_then_lookup_by_id() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();

        let found = get_node(&conn, "n1").unwrap().unwrap();
        assert_eq!(found.name, "foo");
        assert_eq!(found.qualified_name, "m::foo");

        assert!(get_node(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_existing_node() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("n1", "Function", "renamed", "m::renamed", "src/lib.rs", "rust"),
        )
        .unwrap();

        let found = get_node(&conn, "n1").unwrap().unwrap();
        assert_eq!(found.name, "renamed");
        assert_eq!(found.qualified_name, "m::renamed");

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "upsert must not create a duplicate row");
    }

    #[test]
    fn delete_removes_node_and_incident_edges() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n3", "Function", "baz", "m::baz", "src/lib.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e2", "n3", "n1", "CALLS", "tree-sitter", false)).unwrap();

        delete_node(&mut conn, "n1").unwrap();

        assert!(get_node(&conn, "n1").unwrap().is_none());
        assert!(get_edge(&conn, "e1").unwrap().is_none(), "outgoing edge from deleted node must be gone");
        assert!(get_edge(&conn, "e2").unwrap().is_none(), "incoming edge to deleted node must be gone");
        assert!(get_node(&conn, "n2").unwrap().is_some(), "unrelated node must survive");
        assert!(get_node(&conn, "n3").unwrap().is_some(), "unrelated node must survive");
    }

    #[test]
    fn find_file_node_looks_up_by_file_path_not_name() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new("file1", "File", "lib.rs", "src/lib.rs", "src/lib.rs", "rust"),
        )
        .unwrap();
        upsert_node(&mut conn, NodeRecord::new("fn1", "Function", "run", "pkg::run", "src/lib.rs", "rust"))
            .unwrap();

        let found = find_file_node(&conn, "src/lib.rs").unwrap().unwrap();
        assert_eq!(
            found.id, "file1",
            "must return the File node, not the unrelated symbol sharing its filePath"
        );

        assert!(find_file_node(&conn, "missing.rs").unwrap().is_none());
    }

    #[test]
    fn delete_edge_removes_only_that_edge() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)).unwrap();

        delete_edge(&mut conn, "e1").unwrap();

        assert!(get_edge(&conn, "e1").unwrap().is_none());
        assert!(get_node(&conn, "n1").unwrap().is_some(), "deleting an edge must not delete its nodes");
        assert!(get_node(&conn, "n2").unwrap().is_some());
    }

    #[test]
    fn name_and_qualified_name_lookup_returns_all_ambiguous_matches() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "run", "pkg_a::run", "a/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "run", "pkg_b::run", "b/lib.rs", "rust"))
            .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("n3", "Function", "other", "pkg_a::other", "a/lib.rs", "rust"),
        )
        .unwrap();

        let by_name = find_by_name(&conn, "run", None).unwrap();
        assert_eq!(by_name.len(), 2, "ambiguous name must return every matching node");

        let scoped = find_by_name(&conn, "run", Some("a/lib.rs")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "n1");

        let by_qualified = find_by_qualified_name(&conn, "pkg_a::run", None).unwrap();
        assert_eq!(by_qualified.len(), 1);
        assert_eq!(by_qualified[0].id, "n1");
    }

    fn node_with_span(
        id: &str,
        kind: &str,
        file_path: &str,
        start: (i64, i64),
        end: (i64, i64),
    ) -> NodeRecord {
        let mut node = NodeRecord::new(id, kind, id, id, file_path, "rust");
        node.start_line = start.0;
        node.start_col = start.1;
        node.end_line = end.0;
        node.end_col = end.1;
        node
    }

    #[test]
    fn find_by_position_picks_the_innermost_enclosing_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("file1", "File", "a/lib.rs", (0, 0), (20, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("fn1", "Function", "a/lib.rs", (5, 0), (10, 1))).unwrap();

        let found = find_by_position(&conn, "a/lib.rs", 7, 2).unwrap().unwrap();
        assert_eq!(found.id, "fn1", "the nested function must win over the enclosing file");
    }

    /// A container node (`graph::containers`) is a real row with no source:
    /// no name, qualifiedName or position lookup may answer with it, even
    /// when its key is exactly the string asked for. Materialized the way the
    /// daemon does it, through `apply_diff`, rather than inserted by hand.
    #[test]
    fn container_nodes_are_not_answers_to_name_qualified_name_or_position_lookups() {
        let mut conn = setup();
        let mut member = NodeRecord::new("fn1", "Function", "run", "app::run", "app/run.rs", "rust");
        member.container = Some("app".to_string());
        upsert_node(&mut conn, member).unwrap();
        let container = crate::graph::containers::container_id("rust", "app");
        assert!(
            get_node(&conn, &container).unwrap().is_some(),
            "the container must exist for this to test anything"
        );

        assert!(find_by_name(&conn, "app", None).unwrap().is_empty());
        assert!(find_by_name(&conn, "app", Some("")).unwrap().is_empty());
        assert!(find_by_qualified_name(&conn, "app", None).unwrap().is_empty());
        assert!(find_by_qualified_name(&conn, "app", Some("")).unwrap().is_empty());
        assert!(find_by_position(&conn, "", 0, 0).unwrap().is_none());
        assert_eq!(find_by_name(&conn, "run", None).unwrap().len(), 1, "its member is still found");
    }

    /// An import placeholder is the graph's record that a file imported
    /// something - it has no body and no definition site here - so no name,
    /// qualifiedName or position lookup may answer with one, however exactly
    /// its spelling matches (GM-367).
    ///
    /// Modelled on the measured gin case: `context_test.go` imports
    /// `context`, whose placeholder is named and qualified `context` and sits
    /// on the import line, while the declaration a caller means is elsewhere.
    /// Both module placeholder kinds are asserted - `external_module` for an
    /// import this project does not satisfy, `resolved_module` for one it
    /// does - because they were both missing from the filter and the two
    /// arrive by different routes.
    ///
    /// The last assertion is the control inside the test: a real declaration
    /// in the same file, under a name the placeholders do not carry, is still
    /// found by all three lookups. Without it this test would also pass on a
    /// filter that excluded everything.
    #[test]
    fn import_placeholders_are_not_answers_to_name_qualified_name_or_position_lookups() {
        let mut conn = setup();
        for (id, native_kind) in [("ext", EXTERNAL_MODULE_NATIVE_KIND), ("res", RESOLVED_MODULE_NATIVE_KIND)]
        {
            let mut placeholder =
                NodeRecord::new(id, "Module", "context", "context", "app/context_test.go", "go");
            placeholder.native_kind = Some(native_kind.to_string());
            placeholder.start_line = 8;
            placeholder.end_line = 8;
            placeholder.end_col = 10;
            upsert_node(&mut conn, placeholder).unwrap();
        }
        let mut declaration =
            NodeRecord::new("decl", "Type", "Context", "Context", "app/context_test.go", "go");
        declaration.start_line = 20;
        declaration.end_line = 30;
        upsert_node(&mut conn, declaration).unwrap();

        assert!(find_by_name(&conn, "context", None).unwrap().is_empty(), "by name");
        assert!(find_by_qualified_name(&conn, "context", None).unwrap().is_empty(), "by qualifiedName");
        assert!(
            find_by_position(&conn, "app/context_test.go", 8, 4).unwrap().is_none(),
            "by position, on the import line the placeholder covers"
        );

        assert_eq!(find_by_name(&conn, "Context", None).unwrap().len(), 1, "the declaration is still found");
        assert_eq!(find_by_qualified_name(&conn, "Context", None).unwrap().len(), 1);
        assert_eq!(
            find_by_position(&conn, "app/context_test.go", 25, 0).unwrap().map(|n| n.id),
            Some("decl".to_string())
        );
    }

    /// The counts are the point: `net/http` on gin is 63 placeholders of one
    /// specifier, and a refusal that says "63" is saying something a caller
    /// can act on, where a list of 63 identical rows was not. Either column
    /// matches, because a caller types either half of the same import.
    #[test]
    fn import_specifiers_named_groups_by_specifier_and_counts_its_carriers() {
        let mut conn = setup();
        for (id, file) in [("h1", "a.go"), ("h2", "b.go"), ("h3", "c.go")] {
            let mut placeholder = NodeRecord::new(id, "Module", "http", "net/http", file, "go");
            placeholder.native_kind = Some(EXTERNAL_MODULE_NATIVE_KIND.to_string());
            upsert_node(&mut conn, placeholder).unwrap();
        }
        let mut other = NodeRecord::new("h4", "Module", "http", "github.com/x/http", "d.go", "go");
        other.native_kind = Some(EXTERNAL_MODULE_NATIVE_KIND.to_string());
        upsert_node(&mut conn, other).unwrap();
        // A declaration of the same name is not an import record and must not
        // be counted as one.
        upsert_node(&mut conn, NodeRecord::new("d", "Function", "http", "http", "e.go", "go")).unwrap();

        assert_eq!(
            import_specifiers_named(&conn, "http").unwrap(),
            vec![("net/http".to_string(), 3), ("github.com/x/http".to_string(), 1)],
            "grouped by specifier, most-carried first"
        );
        assert_eq!(
            import_specifiers_named(&conn, "net/http").unwrap(),
            vec![("net/http".to_string(), 3)],
            "the qualifiedName half of the same import finds it too"
        );
        assert!(import_specifiers_named(&conn, "nothing").unwrap().is_empty());
    }

    /// The case GM-376's fold exists for: before it, `find_in_file_named`
    /// refused only `pending_symbol`/`reexport` by `nativeKind`, catching the
    /// other three excluded kinds (`resolved_module`, `external_module`,
    /// `container`) only because every one of them happens to be stored under
    /// `kind = 'Module'` and the old SQL refused that outright. A row that is
    /// one of the three *without* being `kind = 'Module'` is not a shape any
    /// shipped plugin sends - constructed here the same way GM-372's own
    /// tests construct their traps, by building the `NodeRecord` directly -
    /// but it is exactly what the old SQL had no defence against: this test
    /// fails against that SQL (the row is offered as a candidate) and passes
    /// once `find_in_file_named` consults `declaration_only`/
    /// `NON_DECLARATION_NATIVE_KINDS` instead of its own two-kind copy.
    #[test]
    fn an_import_record_stored_as_a_non_module_kind_is_still_excluded() {
        let mut conn = setup();
        let mut leak = NodeRecord::new("leak", "Type", "widget", "widget", "src/widget.rs", "rust");
        leak.native_kind = Some(EXTERNAL_MODULE_NATIVE_KIND.to_string());
        upsert_node(&mut conn, leak).unwrap();

        assert!(
            find_in_file_named(&conn, "widget", 5).unwrap().is_empty(),
            "an import record is not a declaration to offer as a candidate, whatever `kind` it is stored under"
        );
    }

    /// The control for the test above, and GM-376's other discrimination
    /// case: a real declaration stored as `kind: "Module"` - a TypeScript
    /// `namespace` is the shape (`plugins/typescript/src/extract.ts`) - used
    /// to be refused here too, by the `kind IS NOT 'Module'` clause this task
    /// removed. It must still be offered once that clause is gone, or the
    /// fold traded one leak for a new exclusion.
    #[test]
    fn a_namespace_stored_as_kind_module_is_still_offered() {
        let mut conn = setup();
        let mut namespace = NodeRecord::new("ns", "Module", "Config", "Config", "src/config.rs", "rust");
        namespace.native_kind = Some("namespace".to_string());
        upsert_node(&mut conn, namespace).unwrap();

        let found = find_in_file_named(&conn, "config", 5).unwrap();
        assert_eq!(found.len(), 1, "the namespace declaration must be offered as a candidate");
        assert_eq!(found[0].id, "ns");
    }

    #[test]
    fn find_by_position_returns_none_outside_every_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("file1", "File", "a/lib.rs", (0, 0), (20, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("fn1", "Function", "a/lib.rs", (5, 0), (10, 1))).unwrap();

        assert!(find_by_position(&conn, "a/lib.rs", 50, 0).unwrap().is_none());
    }

    fn file(path: &str) -> NodeRecord {
        NodeRecord::new(path, "File", path, path, path, "rust")
    }

    fn entry_points(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `NodeRecord` carries no `Debug` impl, so a failing assertion below
    /// prints candidates by path instead - all a debugging message here
    /// needs.
    fn paths(nodes: &[NodeRecord]) -> Vec<&str> {
        nodes.iter().map(|n| n.file_path.as_str()).collect()
    }

    /// TS behaviour, preserved byte for byte: a bare, dot-free entry
    /// (`"index"`, exactly what the bundled TS plugin's manifest declares)
    /// still matches the file's stem under any extension, and still outranks
    /// a shorter path - the same property the old hardcoded `%/index.%`
    /// check gave `find_files_under`.
    #[test]
    fn a_bare_entry_point_matches_the_stem_under_any_extension_and_ranks_first() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/a.ts")).unwrap();
        upsert_node(&mut conn, file("pkg/index.tsx")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["index"]), 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/index.tsx"),
            "a stem match must lead even though pkg/a.ts is the shorter path: {:?}",
            paths(&found)
        );
    }

    /// GM-273's acceptance case: a fake manifest declaring `entry_points =
    /// ["mod.rs"]` (Rust's own convention, not yet backed by a real plugin -
    /// see `daemon::manifest::WorkspaceConfig::entry_points`'s doc comment)
    /// must rank `mod.rs` first in a directory lookup, exactly the way
    /// `"index"` already does for TypeScript. `pkg/a.rs` is deliberately the
    /// *shorter* path, so this only passes if the entry-point rank - not
    /// `LENGTH(filePath)` - decided the order.
    #[test]
    fn a_declared_rust_entry_point_ranks_first_over_a_shorter_path() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/a.rs")).unwrap();
        upsert_node(&mut conn, file("pkg/mod.rs")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["mod.rs"]), 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/mod.rs"),
            "pkg/mod.rs is 2 bytes longer than pkg/a.rs, so only the declared \
             entry point can be why it leads: {:?}",
            paths(&found)
        );
    }

    /// The exact-name form's whole reason to exist: unlike the stem form, it
    /// must not smear onto a file that merely starts with the same bytes.
    /// `pkg/mod.rs.bak` sorts after `pkg/mod.rs` here regardless (it is
    /// longer), so this asserts the stronger property directly - the rank
    /// expression itself, not just the final order.
    #[test]
    fn an_exact_entry_point_does_not_match_a_file_that_only_shares_its_prefix() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/mod.rs.bak")).unwrap();

        let (rank_expr, rank_params) = entry_point_rank_expr(&entry_points(&["mod.rs"]));
        // `rank_expr`'s own `?` placeholders come first in the SQL text, so
        // its params are bound first too - unnumbered `?` throughout, so the
        // two lists stay in the same left-to-right order the query text has.
        let mut bound: Vec<rusqlite::types::Value> = rank_params.into_iter().map(Into::into).collect();
        bound.push("pkg/mod.rs.bak".to_string().into());
        let matches: bool = conn
            .query_row(
                &format!("SELECT {rank_expr} FROM nodes WHERE filePath = ?"),
                rusqlite::params_from_iter(bound),
                |row| row.get(0),
            )
            .unwrap();
        assert!(!matches, "pkg/mod.rs.bak must not count as the mod.rs entry point");
    }

    /// Code-review fix: `_` and `%` are `LIKE` wildcards, and `entry_points`
    /// is manifest content this module does not control - a future Python
    /// manifest declaring `__init__.py` (two literal underscores) must not
    /// have those underscores reinterpreted as "any one character". Proves
    /// both halves: the real entry point still ranks first, and a file that
    /// only matches because `_` was treated as a wildcard -
    /// `pkg/abinitcd.py`'s file name is `abinitcd.py`, 11 characters, one per
    /// literal/wildcard position in `__init__.py` (`_` `_` `i` `n` `i` `t` `_`
    /// `_` `.` `p` `y`) - does not count as the entry point at all.
    #[test]
    fn an_entry_point_containing_like_wildcard_characters_is_matched_literally() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/other.py")).unwrap();
        upsert_node(&mut conn, file("pkg/__init__.py")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["__init__.py"]), 5).unwrap();
        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/__init__.py"),
            "the real entry point must still rank first: {:?}",
            paths(&found)
        );

        // A separate file, present only so the second check below has a real
        // row to query - unescaped, this is exactly the path the old bug
        // would have misidentified as the __init__.py entry point.
        upsert_node(&mut conn, file("pkg/abinitcd.py")).unwrap();

        let (rank_expr, rank_params) = entry_point_rank_expr(&entry_points(&["__init__.py"]));
        let mut bound: Vec<rusqlite::types::Value> = rank_params.into_iter().map(Into::into).collect();
        bound.push("pkg/abinitcd.py".to_string().into());
        let matches: bool = conn
            .query_row(
                &format!("SELECT {rank_expr} FROM nodes WHERE filePath = ?"),
                rusqlite::params_from_iter(bound),
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !matches,
            "pkg/abinitcd.py must not count as the __init__.py entry point - each `_` is a literal \
             underscore, not a single-character wildcard"
        );
    }

    /// No manifest available at all (this task's documented fallback) must
    /// not error, and must degrade to the pre-task ordering: shortest path
    /// wins, with no entry point ranked ahead of it.
    #[test]
    fn an_empty_entry_point_list_falls_back_to_shortest_path_only() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/index.ts")).unwrap();
        upsert_node(&mut conn, file("pkg/a.ts")).unwrap();

        let found = find_files_under(&conn, "pkg", &[], 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/a.ts"),
            "with no entry points declared, the shortest path must win: {:?}",
            paths(&found)
        );
    }

    /// `find_files_ending_in_dir` shares `entry_point_rank_expr` with
    /// `find_files_under` - one pass over this same Rust-entry-point case is
    /// enough to prove the wiring, not a full re-run of every case above.
    #[test]
    fn find_files_ending_in_dir_also_ranks_a_declared_entry_point_first() {
        let mut conn = setup();
        upsert_node(&mut conn, file("workspace/pkg/a.rs")).unwrap();
        upsert_node(&mut conn, file("workspace/pkg/mod.rs")).unwrap();

        let found = find_files_ending_in_dir(&conn, "pkg", &entry_points(&["mod.rs"]), 5).unwrap();

        assert_eq!(found.first().map(|n| n.file_path.as_str()), Some("workspace/pkg/mod.rs"));
    }
}
