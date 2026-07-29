//! Cross-file symbol linking: the pass that turns a plugin's *pending
//! symbol* placeholder into a real `CALLS`/`REFERENCES`/`SUPERTYPE_OF` edge
//! onto the symbol another file declares.
//!
//! `graph::imports` does this one level coarser, for whole modules; this is
//! the same handshake for the symbols reached through them. A language plugin
//! sees one file at a time, so `foo()` after `import { foo } from "./x"` has
//! no local declaration to point at - and dropping that edge, which is what
//! used to happen, is what made find_callers/find_references/
//! find_implementations answer "nothing" for any symbol used from a file
//! other than the one declaring it, i.e. the normal case in a real codebase.
//!
//! Instead the plugin emits a placeholder `Module` node marked
//! [`PENDING_SYMBOL_NATIVE_KIND`], addressed by `<target file>#<imported
//! name>` in its `qualifiedName` with the bare name repeated in `name`, and
//! hangs the usage edge on that (see `importedSymbol` in
//! plugins/js-ts/src/extract.ts). This module is the other half: it looks for
//! a symbol of that name *exported* by that file among the nodes actually in
//! the index and, when exactly one fits, repoints the edge and marks it
//! `resolved`.
//!
//! ## Why here and not in the plugin
//!
//! The same two reasons as `graph::imports`, which spells them out in full:
//! the target file may not have been walked yet (bulk-index order), and it
//! may never be in the index at all. On top of those there is a third,
//! specific to symbols: whether `./x` really exports a `foo`, and whether
//! that `foo` is a function or a type, is a fact about *another file's*
//! extraction - the one thing a per-file pass structurally cannot know.
//!
//! ## What stays unresolved
//!
//! Anything ambiguous or unconfirmed, on the project's standing rule that a
//! missing edge beats a wrong one (`lookupByName` in extract.ts):
//!
//!  - the target file is not in the index, or exports no such name;
//!  - the name is exported by several nodes at once and the edge kind does
//!    not single one out;
//!  - the export is not of the kind the edge needs - a `CALLS` edge only ever
//!    lands on a `Function`, a `SUPERTYPE_OF` edge only on a `Type`;
//!  - the imported name is `default` while the target exports its default
//!    under a declared name (`export default class Foo {}` is a node called
//!    `Foo`), which only a semantic layer can tie together.
//!
//! ## Why the placeholder is kept
//!
//! Unlike `graph::imports`, a linked-away placeholder is *not* deleted, even
//! once nothing points at it. An import placeholder can only ever carry the
//! one `IMPORTS` edge from its own file, so once that has moved it is
//! genuinely spent; a symbol placeholder carries one edge per *usage*, and a
//! later edit to the same file can add another one. The plugin diffs against
//! its previous extraction, so that later edit sends the new edge without
//! re-sending the unchanged placeholder node - which, had it been deleted,
//! would leave the edge pointing at nothing. Keeping the row costs one
//! isolated node per imported symbol and makes the pass safe to run against
//! any diff; `graph::queries` keeps those nodes out of the name lookups so
//! they never surface as a definition.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a pending cross-file symbol with. Mirrors
/// `PENDING_SYMBOL_NATIVE_KIND` in plugins/js-ts/src/extract.ts - the two are
/// one wire contract and must be changed together.
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";

const MODULE_KIND: &str = "Module";

/// The edge kinds a pending-symbol placeholder can carry, and the node kind
/// each one demands of the symbol it is linked to. `CALLS` is Function ->
/// Function by definition and `SUPERTYPE_OF` relates two types; `REFERENCES`
/// is the catch-all usage edge and accepts whatever the file exports.
const LINKABLE_EDGE_KINDS: [(&str, Option<&str>); 3] =
    [("CALLS", Some("Function")), ("SUPERTYPE_OF", Some("Type")), ("REFERENCES", None)];

fn required_target_kind(edge_kind: &str) -> Option<Option<&'static str>> {
    LINKABLE_EDGE_KINDS.iter().find(|(kind, _)| *kind == edge_kind).map(|(_, required)| *required)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkSummary {
    /// Usage edges repointed from a placeholder onto the real symbol.
    pub linked_edges: usize,
}

/// One pending placeholder, split back into the two things it names.
#[derive(Debug)]
struct Placeholder {
    id: String,
    /// Project-relative path of the file expected to export `symbol_name`.
    target_path: String,
    /// The name that file is expected to export.
    symbol_name: String,
}

impl Placeholder {
    /// Undoes `pendingSymbolQualifiedName`. The split is on the *last* `#`
    /// rather than the first, and driven by the `name` column rather than by
    /// scanning: a symbol name never contains a `#`, whatever a file path
    /// might. A row that does not fit the convention is not one of ours and
    /// is skipped rather than guessed at.
    fn parse(id: String, qualified_name: &str, name: String) -> Option<Self> {
        let target_path = qualified_name.strip_suffix(&format!("#{name}"))?;
        if target_path.is_empty() {
            return None;
        }
        Some(Placeholder { id, target_path: target_path.to_string(), symbol_name: name })
    }
}

/// Links every pending symbol in the index. The whole-project pass, run once
/// a bulk index has committed its last batch - at which point every symbol
/// the walk will ever produce is in, so a placeholder that finds no export
/// here has none to find.
pub fn link_all(conn: &mut Connection) -> Result<LinkSummary> {
    let placeholders = {
        let mut stmt = conn
            .prepare("SELECT id, qualifiedName, name FROM nodes WHERE kind = ?1 AND nativeKind = ?2")
            .context("failed to prepare the pending-symbol scan")?;
        let rows = stmt
            .query_map(params![MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND], |row| {
                Ok(Placeholder::parse(row.get(0)?, &row.get::<_, String>(1)?, row.get(2)?))
            })
            .context("failed to scan for pending symbols")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read pending symbols")?
            .into_iter()
            .flatten()
            .collect()
    };

    link(conn, placeholders)
}

/// Links what one just-applied diff could have changed, without rescanning
/// the whole index. Three things can newly become linkable:
///
///  - a placeholder the diff itself added (the reindexed file's own imports);
///  - an *exported symbol* the diff added, which may be what other files'
///    placeholders have been waiting for - the cross-file case, and the
///    reason a newly written export does not stay uncallable until each of
///    its callers happens to be edited;
///  - a usage edge the diff added onto a placeholder that is already in the
///    index and already linked once. That placeholder is not in the diff (it
///    did not change), so nothing else here would look at it, and its brand
///    new edge would otherwise sit unresolved forever.
///
/// Not covered, deliberately, and for the same reason `graph::imports` does
/// not cover it: a symbol *deleted* from the project. Edges into it go when
/// something deletes it; the importers' edges come back as fresh placeholders
/// the next time those importers are reindexed.
pub fn link_diff(conn: &mut Connection, diff: &Diff) -> Result<LinkSummary> {
    let mut placeholders: Vec<Placeholder> = diff
        .upsert_nodes
        .iter()
        .filter(|node| is_placeholder(&node.kind, node.native_kind.as_deref()))
        .filter_map(|node| Placeholder::parse(node.id.clone(), &node.qualified_name, node.name.clone()))
        .collect();

    {
        // Indexed by idx_nodes_qualifiedName, so both of these are lookups
        // rather than scans of the index.
        let mut waiting_on_symbol = conn
            .prepare(
                "SELECT id, qualifiedName, name FROM nodes \
                 WHERE kind = ?1 AND nativeKind = ?2 AND qualifiedName = ?3",
            )
            .context("failed to prepare the waiting-placeholder lookup")?;
        for node in diff.upsert_nodes.iter().filter(|node| node.exported) {
            let key = format!("{}#{}", node.file_path, node.name);
            let rows = waiting_on_symbol
                .query_map(params![MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND, key], |row| {
                    Ok(Placeholder::parse(row.get(0)?, &row.get::<_, String>(1)?, row.get(2)?))
                })
                .context("failed to look up placeholders waiting on a new export")?;
            for row in rows {
                placeholders.extend(row.context("failed to read a waiting placeholder")?);
            }
        }

        let mut placeholder_by_id = conn
            .prepare(
                "SELECT id, qualifiedName, name FROM nodes WHERE id = ?1 AND kind = ?2 AND nativeKind = ?3",
            )
            .context("failed to prepare the placeholder-by-id lookup")?;
        for edge in diff.upsert_edges.iter().filter(|edge| required_target_kind(&edge.kind).is_some()) {
            let rows = placeholder_by_id
                .query_map(params![edge.to_id, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND], |row| {
                    Ok(Placeholder::parse(row.get(0)?, &row.get::<_, String>(1)?, row.get(2)?))
                })
                .context("failed to look up the placeholder a new usage points at")?;
            for row in rows {
                placeholders.extend(row.context("failed to read a re-used placeholder")?);
            }
        }
    }

    placeholders.sort_by(|a, b| a.id.cmp(&b.id));
    placeholders.dedup_by(|a, b| a.id == b.id);
    link(conn, placeholders)
}

fn is_placeholder(kind: &str, native_kind: Option<&str>) -> bool {
    kind == MODULE_KIND && native_kind == Some(PENDING_SYMBOL_NATIVE_KIND)
}

/// The linking itself, in one transaction, edge kind by edge kind: a
/// placeholder can carry a call *and* a type reference to the same name, and
/// the two do not necessarily land on the same node - `export class Foo` and
/// an overloaded `export function Foo` can coexist in one file.
///
/// Idempotent, which is what makes it safe to run after every write: an edge
/// that was already repointed no longer points at the placeholder, so a
/// second pass finds nothing to move, and a reindex that resets one (a
/// restarted plugin re-sends its full extraction, `resolved: false` and all)
/// is simply linked again.
fn link(conn: &mut Connection, placeholders: Vec<Placeholder>) -> Result<LinkSummary> {
    let mut summary = LinkSummary::default();
    if placeholders.is_empty() {
        return Ok(summary);
    }

    let tx = conn.transaction().context("failed to start the symbol-linking transaction")?;
    {
        let mut pending_kinds = tx
            .prepare("SELECT DISTINCT kind FROM edges WHERE toId = ?1")
            .context("failed to prepare the pending-edge-kind scan")?;
        let mut exports = tx
            .prepare("SELECT id, kind FROM nodes WHERE filePath = ?1 AND name = ?2 AND exported = 1")
            .context("failed to prepare the export lookup")?;
        let mut repoint = tx
            .prepare("UPDATE edges SET toId = ?1, resolved = 1 WHERE toId = ?2 AND kind = ?3")
            .context("failed to prepare the edge repoint")?;

        for placeholder in placeholders {
            let edge_kinds: Vec<String> = pending_kinds
                .query_map(params![placeholder.id], |row| row.get(0))
                .context("failed to read a placeholder's edge kinds")?
                .collect::<rusqlite::Result<_>>()
                .context("failed to collect a placeholder's edge kinds")?;
            if edge_kinds.is_empty() {
                continue; // already linked, and nothing new points here
            }

            // Only *exported* symbols are candidates: a name an import
            // brought in is by definition one the target file publishes, and
            // requiring it rules out matching some unrelated local that
            // happens to share the name.
            let candidates: Vec<(String, String)> = exports
                .query_map(params![placeholder.target_path, placeholder.symbol_name], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .context("failed to look up a pending symbol's target")?
                .collect::<rusqlite::Result<_>>()
                .context("failed to read a pending symbol's target")?;
            // Nothing exported under that name: the target file is not in the
            // index (gitignored, excluded, another language), does not export
            // it, or exports it only by re-exporting someone else's. Leaving
            // the placeholder alone *is* the graceful fallback.
            if candidates.is_empty() {
                continue;
            }

            for edge_kind in edge_kinds {
                let Some(required) = required_target_kind(&edge_kind) else {
                    continue; // not a usage edge - nothing here linked it, so nothing here moves it
                };
                let mut fitting = candidates.iter().filter(|(_, kind)| match required {
                    Some(required) => kind == required,
                    None => true,
                });
                let target_id = match (fitting.next(), fitting.next()) {
                    (Some((id, _)), None) => id.clone(),
                    // Nothing of the right kind, or several equally good
                    // candidates: a missing edge beats a wrong one.
                    _ => continue,
                };

                summary.linked_edges += repoint
                    .execute(params![target_id, placeholder.id, edge_kind])
                    .context("failed to repoint a usage edge")?;
            }
        }
    }
    tx.commit().context("failed to commit the symbol-linking transaction")?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // On, so that an edge left pointing at a node that is not there - the
        // exact failure this module exists to prevent - is a hard error here
        // rather than a silently dangling row.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn symbol(file: &str, name: &str, kind: &str, exported: bool) -> NodeRecord {
        let mut node =
            NodeRecord::new(format!("{kind}:{file}:{name}"), kind, name, name, file, "typescript");
        node.exported = exported;
        node
    }

    /// The plugin's own shape for a pending symbol: `filePath` is the
    /// *importing* file (that is where the usage is written), while
    /// `qualifiedName` addresses the export it is waiting for.
    fn placeholder_node(importer: &str, target: &str, name: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("pending:{importer}:{target}#{name}"),
            MODULE_KIND,
            name,
            format!("{target}#{name}"),
            importer,
            "typescript",
        );
        node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        node
    }

    fn usage_edge(from: &str, kind: &str, placeholder: &NodeRecord) -> EdgeRecord {
        EdgeRecord::new(
            format!("edge:{from}:{kind}:{}", placeholder.id),
            from,
            placeholder.id.clone(),
            kind,
            "tree-sitter",
            false,
        )
    }

    /// `caller` (a symbol already in the index) uses `name` imported from
    /// `target`, via a `kind` edge onto a fresh placeholder. The importing
    /// file is read back out of the caller's id, which `symbol` builds as
    /// `<kind>:<file>:<name>`.
    fn seed_usage(conn: &mut Connection, caller: &str, kind: &str, target: &str, name: &str) -> String {
        let importer = caller.split(':').nth(1).expect("a caller id is <kind>:<file>:<name>");
        let placeholder = placeholder_node(importer, target, name);
        let edge = usage_edge(caller, kind, &placeholder);
        let edge_id = edge.id.clone();
        apply_diff(
            conn,
            &Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() },
        )
        .unwrap();
        edge_id
    }

    fn edge_target(conn: &Connection, edge_id: &str) -> (String, bool) {
        conn.query_row("SELECT toId, resolved FROM edges WHERE id = ?1", params![edge_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    /// One caller in `caller.ts` and one exported `mutate` in `target.ts`,
    /// which is the whole bug in miniature.
    fn seed_caller_and_target(conn: &mut Connection) {
        apply_diff(
            conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn a_pending_call_becomes_an_edge_onto_the_exported_function() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1 });
        assert_eq!(
            edge_target(&conn, &edge),
            ("Function:target.ts:mutate".to_string(), true),
            "the call must land on the real function, and say so"
        );
    }

    /// The placeholder survives on purpose - a later edit to the same file
    /// can add another usage edge onto it, and a deleted node would leave
    /// that edge pointing at nothing.
    #[test]
    fn the_placeholder_survives_being_linked_away() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);
        seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

        link_all(&mut conn).unwrap();

        assert_eq!(
            count(&conn, "nodes"),
            3,
            "the placeholder row must outlive the edge that was hanging on it"
        );
    }

    #[test]
    fn a_symbol_the_target_does_not_export_is_left_unresolved() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    // Same name, same file - but private to it.
                    symbol("target.ts", "mutate", "Function", false),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1, "nothing was confirmed, so nothing is resolved");
    }

    #[test]
    fn a_target_file_that_is_not_in_the_index_stays_a_placeholder() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "generated.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert_eq!(edge_target(&conn, &edge).0, "pending:caller.ts:generated.ts#mutate");
    }

    /// A `CALLS` edge is Function -> Function; an exported type of the same
    /// name is not a thing you can call, so the edge waits rather than
    /// landing on it.
    #[test]
    fn a_call_does_not_link_to_a_non_function_export() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("target.ts", "Shape", "Type", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "Shape");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1);
    }

    /// The `find_implementations` case: a class in one file implementing an
    /// interface imported from another.
    #[test]
    fn a_supertype_edge_links_to_the_exported_type() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("laser.ts", "LaserTrails", "Type", true),
                    symbol("trail.ts", "Trail", "Type", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Type:laser.ts:LaserTrails", "SUPERTYPE_OF", "trail.ts", "Trail");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge), ("Type:trail.ts:Trail".to_string(), true));
    }

    /// One placeholder, two usages of different kinds, two different targets:
    /// the class is what gets referenced, the function is what gets called.
    #[test]
    fn each_edge_kind_picks_the_export_that_fits_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("target.ts", "Widget", "Function", true),
                    symbol("target.ts", "Widget", "Type", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let calls = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "Widget");
        let references =
            seed_usage(&mut conn, "Function:caller.ts:run", "SUPERTYPE_OF", "target.ts", "Widget");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
        assert_eq!(edge_target(&conn, &calls).0, "Function:target.ts:Widget");
        assert_eq!(edge_target(&conn, &references).0, "Type:target.ts:Widget");
    }

    /// A `REFERENCES` edge takes any kind of export - which is exactly why it
    /// has to refuse when there are two of them.
    #[test]
    fn an_ambiguous_reference_is_left_unresolved() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("target.ts", "Widget", "Function", true),
                    symbol("target.ts", "Widget", "Type", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "target.ts", "Widget");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1, "a missing edge beats a wrong one");
    }

    #[test]
    fn linking_twice_changes_nothing_the_second_time() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);
        seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert_eq!(count(&conn, "edges"), 1);
    }

    /// Several files calling the same exported symbol each have their own
    /// placeholder (ids are per importing file), and all of them land.
    #[test]
    fn every_importer_is_linked_onto_the_one_definition() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("a.ts", "run", "Function", true),
                    symbol("b.ts", "run", "Function", true),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        seed_usage(&mut conn, "Function:a.ts:run", "CALLS", "target.ts", "mutate");
        seed_usage(&mut conn, "Function:b.ts:run", "CALLS", "target.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
        let callers: Vec<String> = conn
            .prepare("SELECT fromId FROM edges WHERE toId = 'Function:target.ts:mutate' ORDER BY fromId")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(callers, vec!["Function:a.ts:run", "Function:b.ts:run"]);
    }

    /// The scoped pass must see the placeholders in the diff it is handed,
    /// without a whole-index scan.
    #[test]
    fn a_diff_links_the_usages_it_brought_with_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![symbol("target.ts", "mutate", "Function", true)],
                ..Default::default()
            },
        )
        .unwrap();

        let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
        let edge = usage_edge("Function:caller.ts:run", "CALLS", &placeholder);
        let edge_id = edge.id.clone();
        let diff = Diff {
            upsert_nodes: vec![symbol("caller.ts", "run", "Function", true), placeholder],
            upsert_edges: vec![edge],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge_id).0, "Function:target.ts:mutate");
    }

    /// The cross-file half: a caller indexed while the symbol it calls did
    /// not exist yet gets linked when that symbol finally shows up, rather
    /// than waiting for the caller to be edited again.
    #[test]
    fn adding_an_export_links_the_usages_that_were_waiting_for_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "nothing exports it yet");

        let diff = Diff {
            upsert_nodes: vec![symbol("target.ts", "mutate", "Function", true)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge), ("Function:target.ts:mutate".to_string(), true));
    }

    /// The reason placeholders are kept: a second usage in an already-linked
    /// file arrives on its own, with the (unchanged) placeholder nowhere in
    /// the diff, and still has to be linked.
    #[test]
    fn a_new_usage_of_an_already_linked_placeholder_is_linked_too() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);
        seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
        link_all(&mut conn).unwrap();

        // A second function in the same file, calling the same import. The
        // placeholder it points at is unchanged, so the plugin does not
        // re-send it.
        let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
        let edge = usage_edge("Function:caller.ts:again", "CALLS", &placeholder);
        let edge_id = edge.id.clone();
        let diff = Diff {
            upsert_nodes: vec![symbol("caller.ts", "again", "Function", false)],
            upsert_edges: vec![edge],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge_id), ("Function:target.ts:mutate".to_string(), true));
    }

    /// A reindex resends a file's whole extraction with `resolved: false`,
    /// which un-links every one of its edges - the next pass has to put them
    /// back.
    #[test]
    fn a_full_reindex_of_the_importer_is_linked_again() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);
        let edge_id = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
        link_all(&mut conn).unwrap();

        let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
        let diff = Diff {
            upsert_nodes: vec![symbol("caller.ts", "run", "Function", true), placeholder_node("caller.ts", "target.ts", "mutate")],
            upsert_edges: vec![usage_edge("Function:caller.ts:run", "CALLS", &placeholder)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();
        assert!(!edge_target(&conn, &edge_id).1, "the resend reset it");

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge_id), ("Function:target.ts:mutate".to_string(), true));
    }

    /// An import placeholder is a different handshake with a different owner
    /// (`graph::imports`); this pass must not touch one.
    #[test]
    fn a_module_import_placeholder_is_left_exactly_as_it_was() {
        let mut conn = setup();
        let mut module = NodeRecord::new("mod:a.ts:b.ts", MODULE_KIND, "./b", "b.ts", "a.ts", "typescript");
        module.native_kind = Some("resolved_module".to_string());
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![symbol("a.ts", "a.ts", "File", false), module],
                upsert_edges: vec![EdgeRecord::new(
                    "e_imports",
                    "File:a.ts:a.ts",
                    "mod:a.ts:b.ts",
                    "IMPORTS",
                    "tree-sitter",
                    false,
                )],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert_eq!(edge_target(&conn, "e_imports"), ("mod:a.ts:b.ts".to_string(), false));
    }

    #[test]
    fn an_empty_diff_is_a_no_op() {
        let mut conn = setup();
        assert_eq!(link_diff(&mut conn, &Diff::default()).unwrap(), LinkSummary::default());
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    }
}
