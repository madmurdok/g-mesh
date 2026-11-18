//! The base members a method overrides or implements, disclosed on its
//! caller page. Design: `docs/architecture/gm-502-override-callers-field.md`.
//!
//! A receiver call `x.m()` binds to the declared or inferred type of `x`,
//! never to the type it holds at run time, so a call through a base-typed
//! receiver is an edge onto the base member and is missing from an
//! override's caller page. This module names those base members, so the page
//! can point at where such calls sit instead of reading as complete.
//!
//! The source is per language, from its manifest's `member_overrides`
//! ([`MemberOverrides`]):
//! - `by_name` (D2): the anchor's owner type is found from its
//!   `qualifiedPath`, its supertypes are walked over resolved `SUPERTYPE_OF`
//!   edges, and the nearest supertype on each branch that declares a member
//!   of the anchor's name is reported;
//! - `declared` (D3): the plugin itself emits `SUPERTYPE_OF` from the method
//!   to the member it implements, and this module only reads those edges;
//! - `none`: nothing is said.

use std::collections::{HashSet, VecDeque};

use rusqlite::Connection;
use serde::Serialize;

use crate::graph::queries;
use crate::model::NodeRecord;
use crate::plugin_manifest::MemberOverrides;

/// Rows sent at most; `overridesTruncated` says when more were found.
const MAX_ROWS: usize = 8;
/// Supertype hops walked from the owner at most.
const MAX_DEPTH: usize = 8;
/// Supertypes visited at most, whatever their depth.
const MAX_VISITED: usize = 64;

/// One base member the anchor overrides or implements. `id` is what
/// `find_callers(symbol_id=...)` takes; `start_line` is zero-based, like
/// every other row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverriddenMember {
    pub(crate) id: String,
    pub(crate) qualified_name: String,
    pub(crate) file_path: String,
    pub(crate) start_line: i64,
}

/// What [`probe`] found: never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Overrides {
    /// Ordered by walk depth, then `qualifiedName`; at most [`MAX_ROWS`].
    pub(crate) members: Vec<OverriddenMember>,
    /// More than [`MAX_ROWS`] were found.
    pub(crate) truncated: bool,
}

impl Overrides {
    /// Every file the rows name, for a response's `touched` set.
    pub(crate) fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|member| member.file_path.as_str())
    }
}

/// The base members `anchor` overrides or implements under `mode`, or `None`
/// when there is none, the anchor is not a `Function`, or the mode is
/// `none`. Errors are swallowed to `None`, as in `untyped::probe`: a
/// footnote must not fail an answer that already succeeded.
pub(crate) fn probe(conn: &Connection, anchor: &NodeRecord, mode: MemberOverrides) -> Option<Overrides> {
    if anchor.kind != "Function" {
        return None;
    }
    let found = match mode {
        MemberOverrides::None => return None,
        MemberOverrides::ByName => {
            let owner = owner_of(conn, anchor)?;
            walk_up(conn, anchor, &owner)?
        }
        MemberOverrides::Declared => declared(conn, anchor)?,
    };
    if found.is_empty() {
        return None;
    }
    let truncated = found.len() > MAX_ROWS;
    let members = found
        .into_iter()
        .take(MAX_ROWS)
        .map(|node| OverriddenMember {
            id: node.id,
            qualified_name: node.qualified_name,
            file_path: node.file_path,
            start_line: node.start_line,
        })
        .collect();
    Some(Overrides { members, truncated })
}

/// The `Type` declaring `anchor`: the one whose `qualifiedName` is the
/// anchor's segments but the last, joined (as `unlinked::is_type_member`),
/// in the anchor's language and container, or its file when the anchor has
/// no container. `None` for a free function, or when no such type exists.
fn owner_of(conn: &Connection, anchor: &NodeRecord) -> Option<NodeRecord> {
    let segments = anchor.qualified_path.as_ref()?.segments();
    if segments.len() < 2 {
        return None;
    }
    let mut parent = String::new();
    for segment in &segments[..segments.len() - 1] {
        parent.push_str(segment.sep.as_deref().unwrap_or(""));
        parent.push_str(&segment.name);
    }
    queries::type_in_scope_of(conn, &parent, anchor).ok().flatten()
}

/// D2: breadth-first over `owner`'s supertypes (resolved `SUPERTYPE_OF`
/// edges onto `Type` declarations), reporting on each branch the nearest
/// supertype's member named like `anchor` and not climbing past it. A
/// supertype that does not declare one is walked through. Bounded by
/// [`MAX_DEPTH`] and [`MAX_VISITED`]; the visited set also stops cycles.
fn walk_up(conn: &Connection, anchor: &NodeRecord, owner: &NodeRecord) -> Option<Vec<NodeRecord>> {
    let mut visited: HashSet<String> = HashSet::from([owner.id.clone()]);
    let mut queue: VecDeque<(String, usize)> = VecDeque::from([(owner.id.clone(), 0)]);
    // (depth, member), sorted at the end.
    let mut found: Vec<(usize, NodeRecord)> = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    while let Some((type_id, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        for supertype in supertypes(conn, &type_id)? {
            if visited.len() >= MAX_VISITED {
                break;
            }
            if !visited.insert(supertype.id.clone()) {
                continue;
            }
            match member_of(conn, &supertype, &anchor.name, &anchor.language)? {
                Some(member) => {
                    if reported.insert(member.id.clone()) {
                        found.push((depth + 1, member));
                    }
                }
                None => queue.push_back((supertype.id, depth + 1)),
            }
        }
    }
    found.sort_by(|(da, a), (db, b)| da.cmp(db).then_with(|| a.qualified_name.cmp(&b.qualified_name)));
    Some(found.into_iter().map(|(_, member)| member).collect())
}

/// `type_id`'s direct supertypes: resolved `SUPERTYPE_OF` edges onto a `Type`
/// declaration. A placeholder base (an external `unittest.TestCase`) is
/// skipped: it has no member to name.
fn supertypes(conn: &Connection, type_id: &str) -> Option<Vec<NodeRecord>> {
    queries::resolved_supertypes(conn, type_id).ok()
}

/// The `Function` declaration named `name` that is a direct member of
/// `owner`: its `qualifiedPath` minus the last segment equals `owner`'s,
/// segment by segment (separators included, none guessed), in `owner`'s
/// container or file. `Ok(None)` when `owner` declares no such member;
/// `None` (outer) on a query error.
fn member_of(
    conn: &Connection,
    owner: &NodeRecord,
    name: &str,
    language: &str,
) -> Option<Option<NodeRecord>> {
    let Some(owner_path) = owner.qualified_path.as_ref() else {
        return Some(None);
    };
    let candidates = queries::functions_named_under(conn, owner, name, language).ok()?;
    Some(candidates.into_iter().find(|candidate| {
        candidate.qualified_path.as_ref().is_some_and(|path| {
            let segments = path.segments();
            segments.len() == owner_path.len() + 1 && segments[..owner_path.len()] == *owner_path.segments()
        })
    }))
}

/// D3: the members `anchor` states it implements - its outgoing resolved
/// `SUPERTYPE_OF` edges onto a `Function` declaration, by `qualifiedName`.
fn declared(conn: &Connection, anchor: &NodeRecord) -> Option<Vec<NodeRecord>> {
    queries::implemented_functions(conn, &anchor.id).ok()
}

#[cfg(test)]
#[path = "overrides_tests.rs"]
mod tests;
