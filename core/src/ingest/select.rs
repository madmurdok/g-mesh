//! Selection of the indexed files a resolution delta affects.

use std::collections::BTreeSet;

use anyhow::Result;
use rusqlite::Connection;

use crate::graph::files;
use crate::graph::importers::{self, StoredImport};
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
/// A selector on one exact target is answered by [`importers::importers_by_key`], whose
/// cost follows the edges pointing there; only the other selectors read every
/// import of the language, in one [`scan_imports`].
///
/// `own` is the edited file's path and the importers whose imports waited
/// unresolved at it until the edit's commit linked them. An exact `File` key
/// equal to that path selects only those importers (within the selector's
/// `importers` scope), not [`importers::importers_by_key`]: every other import linked
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
                for importer in importers::importers_by_key(conn, language, *scope_kind, key)? {
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
    files::indexed_file_paths_of(conn, language)
}

/// How many `File` nodes `language` has indexed.
pub(crate) fn indexed_file_count(conn: &Connection, language: &str) -> Result<usize> {
    #[cfg(test)]
    reads::bump(&reads::FILE_LISTS);
    files::indexed_file_count(conn, language)
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
    importers::scan_import_rows(conn, language, |import| {
        if selected.contains(&import.importer) {
            return;
        }
        if selectors.iter().any(|selector| import.matches(selector)) {
            selected.insert(import.importer);
        }
    })
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
