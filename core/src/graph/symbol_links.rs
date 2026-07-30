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
//! ## Re-export chains
//!
//! The file a placeholder addresses very often does not declare the name at
//! all: it is a barrel that passes it through (`export * from "./y"`,
//! `export { x as y } from "./z"`), which is exactly what a bare workspace
//! specifier resolves to - `@excalidraw/element` is that package's
//! `src/index.ts`, and the function is a file over. So a name the target file
//! does not declare is looked for one hop further, through the *re-export*
//! placeholders the plugin records for that file (`REEXPORT_NATIVE_KIND`),
//! breadth-first until a declaration turns up or [`MAX_REEXPORT_DEPTH`] hops
//! are spent. Shallowest wins: a file that declares a name itself shadows what
//! it re-exports under that name, as it does in the language.
//!
//! The two shapes of re-export differ in what a hop knows. A named one carries
//! the name over there, which an alias may have renamed (`export { a as b }`
//! forwards `b` to `a`); a whole-module one carries no name at all, so the one
//! being looked for is passed through unchanged and the target's own exports
//! decide whether it is there.
//!
//! ## What stays unresolved
//!
//! Anything ambiguous or unconfirmed, on the project's standing rule that a
//! missing edge beats a wrong one (`lookupByName` in extract.ts):
//!
//!  - the target file is not in the index, or exports no such name - directly
//!    or through any re-export chain short enough to follow;
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

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, Statement};

use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a pending cross-file symbol with. Mirrors
/// `PENDING_SYMBOL_NATIVE_KIND` in plugins/js-ts/src/extract.ts - the two are
/// one wire contract and must be changed together.
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";

/// The `nativeKind` a plugin marks a re-export with: "this file publishes
/// `name`, which really lives at `qualifiedName`". Mirrors
/// `REEXPORT_NATIVE_KIND` in plugins/js-ts/src/extract.ts - the two are one
/// wire contract and must be changed together.
pub const REEXPORT_NATIVE_KIND: &str = "reexport";

/// The name a whole-module re-export (`export * from "./y"`) is recorded
/// under, at both ends of its address - it republishes every name the target
/// exports rather than one nameable one. Mirrors `REEXPORT_ALL_NAME` in
/// plugins/js-ts/src/extract.ts.
pub const REEXPORT_ALL_NAME: &str = "*";

/// The name a default export is imported under. A whole-module re-export is
/// the one hop that does *not* carry it: `export * from "./y"` republishes
/// every named export of `./y` and never its default, so a chain reaching this
/// name through one is a chain that ends there.
const DEFAULT_EXPORT_NAME: &str = "default";

/// How many re-export hops a lookup follows before giving up. Bounded for the
/// same reason `graph::traversal` bounds its walk: the chain is read out of
/// project sources, so nothing but this stops a pathological (or hand-written
/// adversarial) barrel web from making one lookup walk the whole project.
/// Cycles are already ruled out by the visited set - this is the guard on
/// *length*, and eight is comfortably above what real code does: the deepest
/// chain in the excalidraw monorepo, whose packages are barrels almost to a
/// file, is two.
const MAX_REEXPORT_DEPTH: usize = 8;

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
/// the whole index. Four things can newly become linkable:
///
///  - a placeholder the diff itself added (the reindexed file's own imports);
///  - an *exported symbol* the diff added, which may be what other files'
///    placeholders have been waiting for - the cross-file case, and the
///    reason a newly written export does not stay uncallable until each of
///    its callers happens to be edited;
///  - a *re-export* the diff added, which answers the same waiting
///    placeholders a declaration would: a barrel that starts forwarding a
///    name is, to everything importing through it, the name appearing;
///  - a usage edge the diff added onto a placeholder that is already in the
///    index and already linked once. That placeholder is not in the diff (it
///    did not change), so nothing else here would look at it, and its brand
///    new edge would otherwise sit unresolved forever.
///
/// The second and third are not looked up under their own address alone. A
/// placeholder reaching a symbol through a barrel is addressed at the
/// *barrel*, so [`republished_addresses`] walks the re-export chains back up
/// from what changed to every address that now answers differently - the
/// mirror of the walk [`exported_symbols`] runs down from an importer.
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
        // A new export is not only waited on under its own address: every
        // barrel re-exporting it publishes it under an address of its own, and
        // that is the one an importer going through the package wrote down.
        let seeds = diff
            .upsert_nodes
            .iter()
            .filter(|node| node.exported || is_reexport(&node.kind, node.native_kind.as_deref()))
            .map(|node| (node.file_path.clone(), node.name.clone()))
            .collect();

        // Indexed by idx_nodes_qualifiedName, so all three of these are
        // lookups (or, for the last, one contiguous range) rather than scans
        // of the index.
        let mut waiting_on_symbol = conn
            .prepare(
                "SELECT id, qualifiedName, name FROM nodes \
                 WHERE kind = ?1 AND nativeKind = ?2 AND qualifiedName = ?3",
            )
            .context("failed to prepare the waiting-placeholder lookup")?;
        let mut waiting_on_file = conn
            .prepare(
                "SELECT id, qualifiedName, name FROM nodes \
                 WHERE kind = ?1 AND nativeKind = ?2 AND qualifiedName >= ?3 AND qualifiedName < ?4",
            )
            .context("failed to prepare the waiting-placeholder range lookup")?;

        for (file, name) in republished_addresses(conn, seeds)? {
            let waiting = if name == REEXPORT_ALL_NAME {
                // A whole-module re-export does not name what it forwards, so
                // which of the file's waiting placeholders it newly answers is
                // only knowable by trying them: everything from `<file>#` up
                // to `<file>$`, the character after `#`, i.e. every address
                // under that file and nothing else.
                waiting_placeholders(
                    &mut waiting_on_file,
                    params![
                        MODULE_KIND,
                        PENDING_SYMBOL_NATIVE_KIND,
                        format!("{file}#"),
                        format!("{file}$")
                    ],
                )?
            } else {
                waiting_placeholders(
                    &mut waiting_on_symbol,
                    params![MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND, format!("{file}#{name}")],
                )?
            };
            placeholders.extend(waiting);
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

/// The pending-symbol placeholders `stmt` selects, in the `(id,
/// qualifiedName, name)` column order every waiting-placeholder lookup here
/// uses. Rows that do not fit the `<file>#<name>` convention are skipped
/// rather than guessed at, as in [`Placeholder::parse`].
fn waiting_placeholders(
    stmt: &mut Statement,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Placeholder>> {
    let rows = stmt
        .query_map(params, |row| {
            Ok(Placeholder::parse(row.get(0)?, &row.get::<_, String>(1)?, row.get(2)?))
        })
        .context("failed to look up placeholders waiting on a new export")?;

    let mut found = Vec::new();
    for row in rows {
        found.extend(row.context("failed to read a waiting placeholder")?);
    }
    Ok(found)
}

fn is_placeholder(kind: &str, native_kind: Option<&str>) -> bool {
    kind == MODULE_KIND && native_kind == Some(PENDING_SYMBOL_NATIVE_KIND)
}

fn is_reexport(kind: &str, native_kind: Option<&str>) -> bool {
    kind == MODULE_KIND && native_kind == Some(REEXPORT_NATIVE_KIND)
}

/// Every `<file>#<name>` address whose placeholders `seeds` could have made
/// resolvable: the seeds themselves, plus each address a re-export republishes
/// them under, transitively. The mirror image of [`exported_symbols`] - that
/// one walks a chain down from an importer, this one walks the same chains
/// back up from what just changed, which is what [`link_diff`] needs to find
/// the placeholders to revisit without rescanning the index.
///
/// A seed is a file's newly exported symbol *or* a re-export the diff brought
/// with it, since a barrel that just started forwarding a name answers exactly
/// the same waiting placeholders as the declaration itself appearing would.
///
/// Bounded like the downward walk, and for the same reasons: a visited set for
/// cycles, [`MAX_REEXPORT_DEPTH`] for length.
fn republished_addresses(
    conn: &Connection,
    seeds: Vec<(String, String)>,
) -> Result<Vec<(String, String)>> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn
        .prepare(
            "SELECT filePath, name FROM nodes \
             WHERE kind = ?1 AND nativeKind = ?2 AND qualifiedName IN (?3, ?4)",
        )
        .context("failed to prepare the re-exporter lookup")?;

    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut addresses = Vec::new();
    let mut frontier = seeds;

    for depth in 0..=MAX_REEXPORT_DEPTH {
        let mut next = Vec::new();
        for (file, name) in frontier {
            if !visited.insert((file.clone(), name.clone())) {
                continue;
            }
            addresses.push((file.clone(), name.clone()));
            if depth == MAX_REEXPORT_DEPTH {
                continue;
            }

            // Two ways to be republished: named after this exact symbol, or
            // swept up by a whole-module re-export of the file - which,
            // per the language, never carries a default export.
            let named = format!("{file}#{name}");
            let whole = if name == DEFAULT_EXPORT_NAME {
                named.clone()
            } else {
                format!("{file}#{REEXPORT_ALL_NAME}")
            };
            let rows = stmt
                .query_map(params![MODULE_KIND, REEXPORT_NATIVE_KIND, named, whole], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("failed to look up a symbol's re-exporters")?;
            for row in rows {
                let (reexporter, published) = row.context("failed to read a re-exporter")?;
                // A whole-module re-export publishes what it forwards under
                // the same name; a named one under its own.
                let published =
                    if published == REEXPORT_ALL_NAME { name.clone() } else { published };
                next.push((reexporter, published));
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Ok(addresses)
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
        // A re-export placeholder is never `exported`, so it can never be
        // mistaken for a candidate by the lookup above - it is only ever the
        // next hop of one that came up empty.
        let mut reexports = tx
            .prepare(
                "SELECT name, qualifiedName FROM nodes \
                 WHERE filePath = ?1 AND kind = ?2 AND nativeKind = ?3 AND name IN (?4, ?5)",
            )
            .context("failed to prepare the re-export lookup")?;
        let mut repoint = tx
            .prepare("UPDATE edges SET toId = ?1, resolved = 1 WHERE toId = ?2 AND kind = ?3")
            .context("failed to prepare the edge repoint")?;

        // One walk per address, not per placeholder: every file importing the
        // same symbol from the same barrel asks the identical question, and
        // the index cannot change under an answer computed inside this
        // transaction.
        let mut walked: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();

        for placeholder in placeholders {
            let edge_kinds: Vec<String> = pending_kinds
                .query_map(params![placeholder.id], |row| row.get(0))
                .context("failed to read a placeholder's edge kinds")?
                .collect::<rusqlite::Result<_>>()
                .context("failed to collect a placeholder's edge kinds")?;
            if edge_kinds.is_empty() {
                continue; // already linked, and nothing new points here
            }

            let key = (placeholder.target_path.clone(), placeholder.symbol_name.clone());
            if !walked.contains_key(&key) {
                let found = exported_symbols(&mut exports, &mut reexports, &key.0, &key.1)?;
                walked.insert(key.clone(), found);
            }
            let candidates = &walked[&key];
            // Nothing exported under that name, here or anywhere the file
            // forwards to: the target is not in the index (gitignored,
            // excluded, another language), does not export it, or ends its
            // chain somewhere that does not. Leaving the placeholder alone
            // *is* the graceful fallback.
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

/// Splits a `<file>#<name>` address, on the *last* `#` - a symbol name never
/// contains one, whatever a file path might. The mirror image of
/// `pendingSymbolQualifiedName` in extract.ts, used where the `name` column
/// holds something else than the address's own name (a re-export's `name` is
/// what it *publishes*, which an alias may have renamed), so
/// [`Placeholder::parse`]'s suffix trick does not apply.
fn split_address(address: &str) -> Option<(&str, &str)> {
    let (file, name) = address.rsplit_once('#')?;
    if file.is_empty() || name.is_empty() {
        return None;
    }
    Some((file, name))
}

/// The `(id, kind)` of every symbol `file` exports under `name`, following
/// re-export chains when it does not declare one itself.
///
/// Breadth-first, so the shallowest declaration wins and a name a file both
/// declares and re-exports resolves to the declaration - the language's own
/// rule. Bounded twice over: the visited set makes a re-export cycle
/// terminate, [`MAX_REEXPORT_DEPTH`] bounds an acyclic chain, and both are
/// needed since one does not imply the other.
///
/// Ambiguity is not resolved here. Several branches of a barrel can each
/// answer, and they are all returned: the caller already refuses to move an
/// edge that more than one candidate fits, which is the same rule as for a
/// name a single file exports twice.
fn exported_symbols(
    exports: &mut Statement,
    reexports: &mut Statement,
    file: &str,
    name: &str,
) -> Result<Vec<(String, String)>> {
    let mut frontier = vec![(file.to_string(), name.to_string())];
    let mut visited: HashSet<(String, String)> = frontier.iter().cloned().collect();

    for depth in 0..=MAX_REEXPORT_DEPTH {
        let mut candidates = Vec::new();
        for (file, name) in &frontier {
            // Only *exported* symbols are candidates: a name an import
            // brought in is by definition one the target file publishes, and
            // requiring it rules out matching some unrelated local that
            // happens to share the name.
            let rows = exports
                .query_map(params![file, name], |row| Ok((row.get(0)?, row.get(1)?)))
                .context("failed to look up a pending symbol's target")?;
            for row in rows {
                candidates.push(row.context("failed to read a pending symbol's target")?);
            }
        }
        if !candidates.is_empty() || depth == MAX_REEXPORT_DEPTH {
            return Ok(candidates);
        }

        let mut next = Vec::new();
        for (file, name) in &frontier {
            for hop in reexport_hops(reexports, file, name)? {
                if visited.insert(hop.clone()) {
                    next.push(hop);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Ok(Vec::new())
}

/// Where `file`'s re-exports forward `name` to, as `(file, name)` addresses.
///
/// A named re-export answers with the name the *target* file exports, which is
/// the one before the alias (`export { a as b } from "./y"` forwards `b` to
/// `./y`'s `a`); a whole-module one has no name of its own to answer with, so
/// it passes the one being looked for straight through.
fn reexport_hops(stmt: &mut Statement, file: &str, name: &str) -> Result<Vec<(String, String)>> {
    let rows = stmt
        .query_map(
            params![file, MODULE_KIND, REEXPORT_NATIVE_KIND, name, REEXPORT_ALL_NAME],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .context("failed to look up a file's re-exports")?;

    let mut hops = Vec::new();
    for row in rows {
        let (published, address) = row.context("failed to read a re-export")?;
        let Some((target_file, target_name)) = split_address(&address) else {
            continue; // not one of ours - skipped rather than guessed at
        };
        if published != REEXPORT_ALL_NAME {
            hops.push((target_file.to_string(), target_name.to_string()));
        } else if name != DEFAULT_EXPORT_NAME {
            hops.push((target_file.to_string(), name.to_string()));
        }
    }
    Ok(hops)
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

    /// The plugin's own shape for a re-export: it lives in the file that
    /// *publishes* `published`, and is addressed by the name the file it
    /// forwards to exports (`export { mutate as change } from "./target"` in
    /// `index.ts` is `reexport_node("index.ts", "change", "target.ts",
    /// "mutate")`).
    fn reexport_node(file: &str, published: &str, target: &str, exported: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("reexport:{file}:{target}#{exported}"),
            MODULE_KIND,
            published,
            format!("{target}#{exported}"),
            file,
            "typescript",
        );
        node.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
        node
    }

    /// `export * from "./target"` in `file`: it publishes every name the
    /// target exports, so neither end of the address can be spelled out.
    fn reexport_all(file: &str, target: &str) -> NodeRecord {
        reexport_node(file, REEXPORT_ALL_NAME, target, REEXPORT_ALL_NAME)
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

    // --- re-export chains -------------------------------------------------

    /// The bug this whole chain-following exists for, in miniature: the
    /// importer wrote `@pkg`, which resolves to the package's barrel, and the
    /// function is one file further on.
    #[test]
    fn a_call_through_a_whole_module_reexport_lands_on_the_declaration() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("index.ts", "index.ts", "File", false),
                    reexport_all("index.ts", "target.ts"),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge), ("Function:target.ts:mutate".to_string(), true));
    }

    #[test]
    fn a_named_reexport_is_followed_to_the_file_that_declares_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_node("index.ts", "mutate", "target.ts", "mutate"),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:mutate");
    }

    /// `export { mutate as change } from "./target"`: the importer knows the
    /// published name, the target file only the original one, and the hop is
    /// the only place the two are ever written down together.
    #[test]
    fn a_renaming_reexport_is_followed_under_the_name_the_target_declares() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_node("index.ts", "change", "target.ts", "mutate"),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "change");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:mutate");
    }

    /// A barrel re-exporting a barrel, which real monorepos do: excalidraw's
    /// own package entry points reach two hops deep.
    #[test]
    fn a_chain_of_barrels_is_followed_to_its_end() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_all("index.ts", "inner/index.ts"),
                    reexport_node("inner/index.ts", "mutate", "inner/mutate.ts", "mutate"),
                    symbol("inner/mutate.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:inner/mutate.ts:mutate");
    }

    /// A name a barrel both declares and re-exports is the barrel's own, as it
    /// is in the language - the breadth-first walk answers from the shallowest
    /// level that has anything.
    #[test]
    fn a_declaration_shadows_what_the_same_file_reexports() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("index.ts", "mutate", "Function", true),
                    reexport_all("index.ts", "target.ts"),
                    symbol("target.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:index.ts:mutate");
    }

    /// Two `export *` branches offering the same name: the language calls that
    /// an error, and this pass calls it ambiguous - a missing edge beats a
    /// wrong one, exactly as for a name one file exports twice.
    #[test]
    fn two_reexport_branches_offering_one_name_leave_the_edge_unresolved() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_all("index.ts", "a.ts"),
                    reexport_all("index.ts", "b.ts"),
                    symbol("a.ts", "mutate", "Function", true),
                    symbol("b.ts", "mutate", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1);
    }

    /// `export * from "./x"` republishes every *named* export of `./x` and
    /// never its default, so a chain reaching `default` through one ends there.
    #[test]
    fn a_whole_module_reexport_does_not_carry_a_default_export() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_all("index.ts", "target.ts"),
                    symbol("target.ts", "default", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "default");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1);

        // A *named* re-export of it is a different statement and does carry it.
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![reexport_node("index.ts", "default", "target.ts", "default")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:default");
    }

    /// Two barrels re-exporting each other, which a half-finished refactor
    /// produces. Terminating at all is the assertion; the summary only says
    /// nothing was invented on the way out.
    #[test]
    fn a_reexport_cycle_terminates_without_linking() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_all("a.ts", "b.ts"),
                    reexport_all("b.ts", "a.ts"),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "a.ts", "mutate");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert!(!edge_target(&conn, &edge).1);
    }

    /// The length guard: a chain of exactly `MAX_REEXPORT_DEPTH` hops still
    /// resolves, one hop more is left alone rather than walked forever.
    #[test]
    fn a_chain_longer_than_the_depth_cap_is_left_unresolved() {
        fn chain(hops: usize) -> (Connection, String) {
            let mut conn = setup();
            let mut nodes = vec![symbol("caller.ts", "run", "Function", true)];
            for hop in 0..hops {
                nodes.push(reexport_all(&format!("barrel{hop}.ts"), &format!("barrel{}.ts", hop + 1)));
            }
            nodes.push(symbol(&format!("barrel{hops}.ts"), "mutate", "Function", true));
            apply_diff(&mut conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
            let edge =
                seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "barrel0.ts", "mutate");
            (conn, edge)
        }

        let (mut at_cap, edge) = chain(MAX_REEXPORT_DEPTH);
        assert_eq!(link_all(&mut at_cap).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(
            edge_target(&at_cap, &edge).0,
            format!("Function:barrel{MAX_REEXPORT_DEPTH}.ts:mutate")
        );

        let (mut past_cap, edge) = chain(MAX_REEXPORT_DEPTH + 1);
        assert_eq!(link_all(&mut past_cap).unwrap(), LinkSummary::default());
        assert!(!edge_target(&past_cap, &edge).1);
    }

    /// A re-export placeholder is not a definition of anything: it must never
    /// be what a usage edge lands on, however well its name fits.
    #[test]
    fn a_usage_never_lands_on_the_reexport_placeholder_itself() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_node("index.ts", "mutate", "target.ts", "mutate"),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "index.ts", "mutate");

        // target.ts is not in the index, so the chain runs out one hop in.
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert_eq!(edge_target(&conn, &edge).0, "pending:caller.ts:index.ts#mutate");
    }

    /// The cross-file half through a barrel: the declaration shows up after
    /// everything else, and the usage waiting on the *barrel's* address - not
    /// on this file's - still has to be linked.
    #[test]
    fn adding_a_declaration_links_the_usages_waiting_on_a_barrel_for_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    reexport_all("index.ts", "inner/index.ts"),
                    reexport_all("inner/index.ts", "inner/mutate.ts"),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "nothing declares it yet");

        let diff = Diff {
            upsert_nodes: vec![symbol("inner/mutate.ts", "mutate", "Function", true)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, "Function:inner/mutate.ts:mutate");
    }

    /// The other way round: everything is in the index and it is the *barrel*
    /// that is written, which is what re-exporting an existing module from a
    /// new index file looks like to the watcher.
    #[test]
    fn adding_a_barrel_links_the_usages_that_were_waiting_on_it() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("caller.ts", "run", "Function", true),
                    symbol("target.ts", "mutate", "Function", true),
                    symbol("target.ts", "change", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let whole = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");
        let named = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "renamed");
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "there is no barrel yet");

        let diff = Diff {
            upsert_nodes: vec![
                reexport_all("index.ts", "target.ts"),
                reexport_node("index.ts", "renamed", "target.ts", "change"),
            ],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 2 });
        assert_eq!(edge_target(&conn, &whole).0, "Function:target.ts:mutate");
        assert_eq!(edge_target(&conn, &named).0, "Function:target.ts:change");
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
