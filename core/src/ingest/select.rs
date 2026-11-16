//! Selection of the indexed files a resolution delta affects.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::protocol::types::{ImportMatch, ImportSelector, Matcher, PathScope, TargetScopeKind};

/// A selection of more than this share of the language's indexed files is
/// re-indexed whole instead: the staging swap is atomic and, at that size,
/// no slower than one round trip per file.
pub(crate) const FALLBACK_SHARE_PERCENT: usize = 30;

/// What a resolution delta selects for a re-extract.
#[derive(Debug)]
pub(crate) enum Selection {
    /// No indexed file.
    Nothing,
    /// More than [`FALLBACK_SHARE_PERCENT`] of the language's indexed files:
    /// the whole-language reindex is owed, for this reason.
    TooMany(String),
    /// These files, few enough to re-extract one by one.
    Files(BTreeSet<String>),
}

/// [`select_affected`] with the threshold applied, leaving out `trigger`
/// (a source edit's own file, extracted by the round trip that answered the
/// delta). The share counts the trigger out of the selection but not out of
/// the indexed files, which are counted only once something is selected.
pub(crate) fn select(
    conn: &Connection,
    language: &str,
    files: &[PathScope],
    imports: &[ImportSelector],
    trigger: Option<&str>,
) -> Result<Selection> {
    select_with(conn, language, files, imports, trigger, &[], &[])
}

/// [`select`] with `extra` files joined to the selection before the
/// threshold, so one rule bounds both: the importers of a gone file
/// (`Writer::apply_file_diff_linked`) ride along with what the plugin's own
/// `affected` answer selected for the same delete. `own_waiting` is what an
/// exact `File` key equal to `trigger` selects ([`select_affected`]).
pub(crate) fn select_with(
    conn: &Connection,
    language: &str,
    files: &[PathScope],
    imports: &[ImportSelector],
    trigger: Option<&str>,
    extra: &[String],
    own_waiting: &[String],
) -> Result<Selection> {
    let mut selected = select_affected(conn, language, files, imports, trigger.map(|t| (t, own_waiting)))?;
    selected.extend(extra.iter().cloned());
    if let Some(trigger) = trigger {
        selected.remove(trigger);
    }
    if selected.is_empty() {
        return Ok(Selection::Nothing);
    }
    let indexed = indexed_file_count(conn, language)?;
    if selected.len() * 100 > indexed * FALLBACK_SHARE_PERCENT {
        return Ok(Selection::TooMany(format!(
            "the edit affects {} of {indexed} file(s), more than {FALLBACK_SHARE_PERCENT}%",
            selected.len()
        )));
    }
    Ok(Selection::Files(selected))
}

/// The indexed files of `language` that `files` or `imports` select.
///
/// A file scope selects every indexed `File` of the language inside it. An
/// import selector selects the importing file of every `IMPORTS` edge of the
/// language whose importer lies in its `importers` scope and whose specifier
/// (`edges.specifier`) or target matches. The target of a linked edge is the
/// `File` path or the container key it points at; of an unlinked one, its
/// placeholder's `placeholder_targets.scope`.
///
/// A selector on one exact target is answered by [`indexed_importers`], whose
/// cost follows the edges pointing there; only the other selectors read every
/// import of the language, in one [`scan_imports`].
///
/// `own` is the edited file's path and the importers whose imports waited
/// unresolved at it until the edit's commit linked them. An exact `File` key
/// equal to that path selects only those importers (within the selector's
/// `importers` scope), not [`indexed_importers`]: every other import linked
/// to the file was linked before the edit and is unaffected by it.
pub(crate) fn select_affected(
    conn: &Connection,
    language: &str,
    files: &[PathScope],
    imports: &[ImportSelector],
    own: Option<(&str, &[String])>,
) -> Result<BTreeSet<String>> {
    let mut selected = BTreeSet::new();
    if !files.is_empty() {
        selected.extend(
            indexed_files_of(conn, language)?
                .into_iter()
                .filter(|path| files.iter().any(|scope| scope.contains(path))),
        );
    }

    let mut scanned = Vec::new();
    for selector in imports {
        match &selector.by {
            ImportMatch::Target { scope_kind: TargetScopeKind::File, matcher: Matcher::Exact(key) }
                if own.is_some_and(|(trigger, _)| trigger == key) =>
            {
                let waiting = own.map(|(_, waiting)| waiting).unwrap_or_default();
                selected.extend(waiting.iter().filter(|path| selector.importers.contains(path)).cloned());
            }
            ImportMatch::Target { scope_kind, matcher: Matcher::Exact(key) } => {
                for importer in indexed_importers(conn, language, *scope_kind, key)? {
                    if selector.importers.contains(&importer) {
                        selected.insert(importer);
                    }
                }
            }
            _ => scanned.push(selector),
        }
    }
    if !scanned.is_empty() {
        scan_imports(conn, language, &scanned, &mut selected)?;
    }
    Ok(selected)
}

/// Every indexed `File` path of `language`.
pub(crate) fn indexed_files_of(conn: &Connection, language: &str) -> Result<Vec<String>> {
    #[cfg(test)]
    reads::bump(&reads::FILE_LISTS);
    conn.prepare_cached("SELECT filePath FROM nodes WHERE kind = 'File' AND language = ?1")
        .and_then(|mut statement| statement.query_map(params![language], |row| row.get(0))?.collect())
        .with_context(|| format!("failed to read {language}'s indexed files"))
}

/// How many `File` nodes `language` has indexed.
pub(crate) fn indexed_file_count(conn: &Connection, language: &str) -> Result<usize> {
    #[cfg(test)]
    reads::bump(&reads::FILE_LISTS);
    conn.prepare_cached("SELECT COUNT(*) FROM nodes WHERE kind = 'File' AND language = ?1")
        .and_then(|mut statement| statement.query_row(params![language], |row| row.get::<_, i64>(0)))
        .map(|count| count as usize)
        .with_context(|| format!("failed to count {language}'s indexed files"))
}

/// The importers in `language` of every `IMPORTS` edge whose target of
/// `scope_kind` is exactly `key`, by the same rule as [`StoredImport::target`]:
/// an unlinked edge through its placeholder's scope, a linked one through the
/// `File` path or container key of the node it points at; for a `File` key,
/// also an unlinked edge whose placeholder lists `key` among its extra
/// waiting scopes. Each part starts at an index on the key
/// (`idx_targets_scope`, `idx_also_scopes_scope`, `idx_nodes_filePath`, the
/// containers' `(language, key)`), then follows `idx_edges_toId`.
///
/// A linked container is looked up in the importer's language: the linker
/// resolves a container placeholder in the placeholder's language, which is
/// its importer's, since imports never cross a language boundary.
pub(crate) fn indexed_importers(
    conn: &Connection,
    language: &str,
    scope_kind: TargetScopeKind,
    key: &str,
) -> Result<Vec<String>> {
    let sql = match scope_kind {
        TargetScopeKind::File => {
            "SELECT src.filePath FROM placeholder_targets pt
             JOIN nodes t ON t.id = pt.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE pt.scopeKind = 'file' AND pt.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM placeholder_also_scopes a
             JOIN edges e ON e.toId = a.nodeId
             JOIN nodes src ON src.id = e.fromId
             WHERE a.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM nodes t
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE t.filePath = ?2 AND t.kind = 'File'
               AND NOT EXISTS (SELECT 1 FROM placeholder_targets pt WHERE pt.nodeId = t.id)
               AND e.kind = 'IMPORTS' AND src.language = ?1"
        }
        TargetScopeKind::Container => {
            "SELECT src.filePath FROM placeholder_targets pt
             JOIN nodes t ON t.id = pt.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE pt.scopeKind = 'container' AND pt.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM containers c
             JOIN nodes t ON t.id = c.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE c.language = ?1 AND c.key = ?2
               AND NOT EXISTS (SELECT 1 FROM placeholder_targets pt WHERE pt.nodeId = t.id)
               AND e.kind = 'IMPORTS' AND src.language = ?1"
        }
    };
    conn.prepare_cached(sql)
        .and_then(|mut statement| statement.query_map(params![language, key], |row| row.get(0))?.collect())
        .with_context(|| format!("failed to look up {language}'s importers of {key}"))
}

/// Adds to `selected` the importer of every `IMPORTS` edge of `language` that
/// one of `selectors` matches, reading every such edge once.
pub(crate) fn scan_imports(
    conn: &Connection,
    language: &str,
    selectors: &[&ImportSelector],
    selected: &mut BTreeSet<String>,
) -> Result<()> {
    #[cfg(test)]
    reads::bump(&reads::IMPORT_SCANS);
    let mut statement = conn
        .prepare(
            "SELECT src.filePath, e.specifier, t.kind, t.filePath, c.key, pt.scopeKind, pt.scope
             FROM edges e
             JOIN nodes src ON src.id = e.fromId
             JOIN nodes t ON t.id = e.toId
             LEFT JOIN containers c ON c.nodeId = t.id
             LEFT JOIN placeholder_targets pt ON pt.nodeId = t.id
             WHERE e.kind = 'IMPORTS' AND src.language = ?1",
        )
        .context("failed to prepare the import scan")?;
    let rows = statement
        .query_map(params![language], |row| {
            Ok(StoredImport {
                importer: row.get(0)?,
                specifier: row.get(1)?,
                target_kind: row.get(2)?,
                target_file: row.get(3)?,
                container_key: row.get(4)?,
                placeholder_scope_kind: row.get(5)?,
                placeholder_scope: row.get(6)?,
            })
        })
        .with_context(|| format!("failed to scan {language}'s imports"))?;
    for row in rows {
        let import = row.with_context(|| format!("failed to read one of {language}'s imports"))?;
        if selected.contains(&import.importer) {
            continue;
        }
        if selectors.iter().any(|selector| import.matches(selector)) {
            selected.insert(import.importer);
        }
    }
    Ok(())
}

/// One `IMPORTS` edge as [`select_affected`] reads it.
struct StoredImport {
    importer: String,
    specifier: Option<String>,
    target_kind: String,
    target_file: String,
    container_key: Option<String>,
    placeholder_scope_kind: Option<String>,
    placeholder_scope: Option<String>,
}

impl StoredImport {
    fn matches(&self, selector: &ImportSelector) -> bool {
        if !selector.importers.contains(&self.importer) {
            return false;
        }
        match &selector.by {
            ImportMatch::Specifier(matcher) => self.specifier.as_deref().is_some_and(|s| matcher.matches(s)),
            ImportMatch::Target { scope_kind, matcher } => {
                self.target(*scope_kind).is_some_and(|target| matcher.matches(target))
            }
        }
    }

    /// The stored target of `kind`: the placeholder's own scope while the
    /// edge is unlinked, otherwise the node it was linked onto.
    fn target(&self, kind: TargetScopeKind) -> Option<&str> {
        let wanted = match kind {
            TargetScopeKind::File => "file",
            TargetScopeKind::Container => "container",
        };
        if let Some(scope_kind) = self.placeholder_scope_kind.as_deref() {
            return (scope_kind == wanted).then_some(self.placeholder_scope.as_deref()).flatten();
        }
        match kind {
            TargetScopeKind::File => (self.target_kind == "File").then_some(self.target_file.as_str()),
            TargetScopeKind::Container => self.container_key.as_deref(),
        }
    }
}

/// How often this thread has run the language-wide reads of this module, so a
/// test can tell a keyed lookup from a scan.
#[cfg(test)]
pub(crate) mod reads {
    use std::cell::Cell;
    use std::thread::LocalKey;

    thread_local! {
        /// [`super::scan_imports`] runs.
        pub(crate) static IMPORT_SCANS: Cell<usize> = const { Cell::new(0) };
        /// [`super::indexed_files_of`] and [`super::indexed_file_count`] runs.
        pub(crate) static FILE_LISTS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn bump(counter: &'static LocalKey<Cell<usize>>) {
        counter.with(|count| count.set(count.get() + 1));
    }
}
