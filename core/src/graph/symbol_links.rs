//! Cross-file symbol linking: the pass that turns a plugin's *pending
//! symbol* placeholder into a real `CALLS`/`REFERENCES`/`SUPERTYPE_OF` edge
//! onto the symbol another file (or another member of a container) declares.
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
//! [`PENDING_SYMBOL_NATIVE_KIND`] and hangs the usage edge on that (see
//! `importedSymbol` in plugins/typescript/src/extract.ts). This module is the
//! other half: it looks for the symbol the placeholder is waiting on among the
//! nodes actually in the index and, when exactly one fits, repoints the edge
//! and marks it `resolved`.
//!
//! ## The address is a row, not a string
//!
//! Through schema "7" the address was packed into the placeholder's own
//! `qualifiedName` as `<target file>#<name>`, and this module split it back
//! apart on the last `#`. That convention could only ever name a *file*, and
//! only by a bare name - no Go package, no Rust module, no "this exact
//! `Server.Close`, not `Client.Close`". Since GM-264 the address lives in
//! `placeholder_targets` (docs/architecture/multi-language-plugins.md, "Data
//! Model > Structured placeholder targets"), and since GM-266 this module
//! reads nothing else: no `qualifiedName` of a placeholder is parsed here any
//! more. A still-v1 plugin's `<file>#<name>` is turned into the same row once,
//! at the wire boundary (`protocol::types::derive_legacy_target`), so the TS
//! plugin keeps linking without knowing the table exists.
//!
//! A target is `(scope, key, fromContainer, fromFile)`:
//!
//!  - **scope** - `file` (a project-relative path) or `container` (a key in
//!    the placeholder's own language - `graph::containers`).
//!  - **key** - `name` (a bare name; `*` and `default` keep their re-export
//!    meanings) or `qualifiedName` (a semantic tier's exact answer).
//!  - **fromContainer / fromFile** - who is asking, for the visibility check.
//!
//! ## The contract, step by step
//!
//! For a placeholder and each usage edge of kind `K` hanging on it (design
//! doc: "Interfaces > Linker contract"):
//!
//!  1. **Candidates.** `scope = file`: the nodes whose `filePath` is the
//!     scope. `scope = container`: the nodes whose `(language, container)` is
//!     `(placeholder language, scope)`. Placeholders and core's container
//!     nodes are never candidates, whatever their visibility says.
//!  2. **Key filter.** `name`: `name = key`. `qualifiedName`: `qualifiedName =
//!     key`. Either way the candidate must be *visible* to the requester
//!     (below) - a semantic tier's mistake must not be able to link a private
//!     symbol from outside either.
//!  3. **Kind filter.** `CALLS` lands only on a `Function`, `SUPERTYPE_OF`
//!     only on a `Type`, `REFERENCES` on anything.
//!  4. **Exactly one.** One fitting candidate: repoint and set `resolved = 1`.
//!     None at all under a `name` key: walk the scope's re-exports (below).
//!     Anything else - several, or none of the right kind - leaves the edge
//!     alone. A `qualifiedName` key is held to the same rule: "there is no
//!     ambiguity to refuse" is the normal case, not a licence to pick one if a
//!     plugin ever sends two declarations under one qualifiedName.
//!
//! ## Visibility
//!
//!  - `public` - visible from anywhere.
//!  - `container(c)` - visible iff the requester is in the same language and
//!    its `fromContainer` is `c` or has `c` on its parent chain
//!    ([`containers::parent_chain`]). That is Go's unexported (own package),
//!    Rust's private (own module) and `pub(crate)`/`pub(super)` (an ancestor
//!    module). A requester with no container sees no container-private
//!    symbol. A chain with a gap only ever refuses a link a complete chain
//!    would allow, never the reverse - `parent_chain` documents why.
//!  - `file` - visible iff the candidate is being looked up in a **container**
//!    scope and `fromFile` is the candidate's own file (C++ `static` or an
//!    anonymous namespace inside a reopened namespace). In a **file** scope a
//!    `file`-visible node is never a candidate.
//!
//! That last clause is a deliberate narrowing of the design doc's "visible iff
//! `fromFile = node.filePath`", and it is what makes TS come out *exactly* as
//! it did before (GM-266's no-regression rule). TS maps `export` to `public`
//! and everything else to `file`, and the old lookup required `exported = 1`.
//! For a usage from another file the two rules agree anyway, since `fromFile`
//! differs from the scope. They differ only for a placeholder whose scope is
//! the requester's own file, and the TS plugin can produce one: `import {
//! x } from "./self"`, or - more plausibly - the semantic pass answering
//! `ns.x` with a declaration in the importing file itself, through a barrel
//! that re-exports it back (`askNamespaceUses` in semanticPass.ts). The
//! literal rule would then make every non-exported node of that file named
//! `x` a candidate - a class method `x`, a nested function `x` - and either
//! turn a link that used to land into an ambiguity or, under the kind filter,
//! land a `CALLS` edge on the method. Neither is right in the language: a
//! module that imports from itself sees its exports and nothing else. And no
//! language needs the literal reading: a file can see its own non-published
//! declarations only lexically, which the plugin resolves itself as a direct
//! same-file edge and never as a placeholder (Constraints: per-file
//! extraction). The one place `file` visibility carries information a
//! placeholder can ask about is a container shared by several files, which is
//! the case the narrowed rule keeps.
//!
//! ## Re-export chains
//!
//! The file a placeholder addresses very often does not declare the name at
//! all: it is a barrel that passes it through (`export * from "./y"`,
//! `export { x as y } from "./z"`), which is exactly what a bare workspace
//! specifier resolves to - `@excalidraw/element` is that package's
//! `src/index.ts`, and the function is a file over. So a name the scope does
//! not declare (visibly to this requester) is looked for one hop further,
//! through the *re-export* placeholders recorded for that scope
//! ([`REEXPORT_NATIVE_KIND`]), breadth-first until a declaration turns up or
//! [`MAX_REEXPORT_DEPTH`] hops are spent. Shallowest wins: a scope that
//! declares a name itself shadows what it re-exports under that name, as it
//! does in the language. Only `name` keys walk; a `qualifiedName` names a
//! declaration, never a pass-through.
//!
//! A re-export placeholder is two facts at once, and the row keeps them apart:
//! its node's `name` is what the re-exporting scope *publishes*, and its
//! target is what that name really is over there. The two differ exactly when
//! an alias renamed it (`export { a as b } from "./y"` publishes `b` and
//! targets `./y`'s `a` - see `protocol::types`' legacy derivation, which puts
//! the real name in the key for precisely this reason). A named re-export
//! therefore forwards the published name to its target key; a whole-module one
//! (`*` at both ends) carries no name of its own, so the one being looked for
//! passes through unchanged - except `default`, which `export *` never
//! republishes, so a chain reaching `default` through one ends there.
//!
//! **Which re-exports belong to a scope.** A file scope's are the re-export
//! placeholders whose node lives in that file (the TS shape). A container
//! scope's are the ones whose node's `(language, container)` is that
//! container - the shape a Rust `pub use` inside `mod prelude` takes. A
//! re-export node carrying both a file and a container publishes under both,
//! which [`republished_addresses`] mirrors. The re-export statement's *own*
//! visibility is not checked (a Rust `pub(crate) use` restricting an otherwise
//! public item is not modelled): the TS plugin sends every re-export as
//! `exported: false`, so checking it would unlink every barrel, and the
//! declaration at the end of the chain is still checked against the original
//! requester. A re-export with no target row is skipped, never guessed at.
//!
//! ## What stays unresolved
//!
//! Anything ambiguous or unconfirmed, on the project's standing rule that a
//! missing edge beats a wrong one (`lookupByName` in extract.ts):
//!
//!  - the scope is not in the index, or offers no visible such name - directly
//!    or through any re-export chain short enough to follow;
//!  - several visible nodes fit and the edge kind does not single one out;
//!  - the only fits are of the wrong kind for the edge;
//!  - the placeholder has no target row at all - a legacy v1 address
//!    `derive_legacy_target` could not read. It is left exactly as it is and
//!    reported once per pass, never guessed from its `qualifiedName`;
//!  - the imported name is `default` while the target exports its default
//!    under a declared name (`export default class Foo {}` is a node called
//!    `Foo`), which only a semantic layer can tie together.
//!
//! "Unresolved" is not always the last word on these. The JS/TS plugin's
//! semantic pass re-asks the ones whose target file does not declare the name
//! of the compiler itself and re-sends the edge with `source: "ts-compiler"`
//! when it gets a single answer (`plugins/typescript/src/semanticPass.ts`).
//! Two `export *` branches offering one name are ambiguous *here* and settled
//! in the language, which hands a consumer the first branch to offer it; and
//! `default` is a name no file ever declares, while `definition` at the
//! importer's own binding lands straight on `Foo`. That upgrade arrives as an
//! ordinary diff through [`link_diff`]'s own caller and needs nothing from
//! this pass but that it left the edge alone.
//!
//! ## Placeholders the semantic pass sends
//!
//! `import * as ns from "./mod"` followed by `ns.someExport()` is the one
//! shape the name-matching layer cannot produce a placeholder for at all: it
//! never sees the bare name at the use site, only a property access
//! (`recordImportBindings` in extract.ts). The semantic pass asks `tsserver`
//! where the member is declared and sends a placeholder addressed at *that*
//! answer, with the usage edge marked `source: "ts-compiler"`. Nothing here
//! treats one differently: the target is the entire contract, who worked it
//! out is the plugin's business, and repointing settles what an edge points
//! at, never who answered it. Where the checker stops at a re-export statement
//! instead of a declaration, that is an ordinary barrel address and the walk
//! above finishes the job.
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
//! would leave the edge pointing at nothing. Keeping the row (and its target
//! row) costs one isolated node per imported symbol and makes the pass safe to
//! run against any diff; `graph::queries` keeps those nodes out of the name
//! lookups so they never surface as a definition.
//!
//! ## Cost: every step is an indexed lookup
//!
//! [`link_all`] reads the placeholders with one pass over `nodes` (the same
//! single scan it always did - there is no `nativeKind` index, and one scan
//! per pass is not one per placeholder), joining each to its target by
//! `placeholder_targets`' primary key. Everything after that is keyed:
//!
//!  - pending edge kinds and the repoint: `idx_edges_toId`;
//!  - file-scope candidates: `idx_nodes_filePath`; container-scope ones by
//!    name: `idx_nodes_container`; by qualifiedName: `idx_nodes_qualifiedName`
//!    (the container columns are written `+language`/`+container` so the
//!    planner does not prefer the container index, whose range is the whole
//!    container, over the exact qualifiedName);
//!  - a scope's re-exports: `idx_nodes_filePath` / `idx_nodes_container`, then
//!    each target by primary key;
//!  - a requester's parent chain: `containers`' `UNIQUE (language, key)`.
//!
//! Each of those is asked once per distinct question per pass and memoized
//! ([`Resolver`]), not once per placeholder: every file importing the same
//! symbol from the same barrel asks the identical lookups. What is *not*
//! shared is the answer, because visibility depends on who asks - the
//! breadth-first walk itself runs per placeholder, in memory, over the
//! memoized rows. [`link_diff`]'s lookups use `idx_targets_scope` as well
//! (checked with `EXPLAIN QUERY PLAN` against this schema). The one lookup
//! that is linear in a scope's size is a container-scoped *name* lookup, which
//! walks the container's members: fine for a Go package or a Rust module; a
//! C++ namespace reopened across thousands of files would want a `(language,
//! container, name)` index, which is a schema bump left to that language.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Statement};

use crate::graph::containers::{self, CONTAINER_NATIVE_KIND};
use crate::graph::imports::RESOLVED_MODULE_NATIVE_KIND;
use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a pending cross-file symbol with. Mirrors
/// `PENDING_SYMBOL_NATIVE_KIND` in plugins/typescript/src/extract.ts - the two are
/// one wire contract and must be changed together.
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";

/// The `nativeKind` a plugin marks a re-export with: "this scope publishes
/// `name`, which really is its target". Mirrors `REEXPORT_NATIVE_KIND` in
/// plugins/typescript/src/extract.ts - the two are one wire contract and must be
/// changed together.
pub const REEXPORT_NATIVE_KIND: &str = "reexport";

/// The name a whole-module re-export (`export * from "./y"`) is recorded
/// under, as both its published name and its target key - it republishes
/// every name the target exports rather than one nameable one. Mirrors
/// `REEXPORT_ALL_NAME` in plugins/typescript/src/extract.ts.
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

/// `placeholder_targets.scopeKind` / `keyKind` values and `nodes.visibility`
/// values, as `storage::schema`'s CHECK constraints spell them.
const SCOPE_FILE: &str = "file";
const SCOPE_CONTAINER: &str = "container";
const KEY_NAME: &str = "name";
const KEY_QUALIFIED_NAME: &str = "qualifiedName";
const VISIBILITY_PUBLIC: &str = "public";
const VISIBILITY_FILE: &str = "file";
const VISIBILITY_CONTAINER: &str = "container";

/// The edge kinds a pending-symbol placeholder can carry, and the node kind
/// each one demands of the symbol it is linked to. `CALLS` is Function ->
/// Function by definition and `SUPERTYPE_OF` relates two types; `REFERENCES`
/// is the catch-all usage edge and accepts whatever the scope offers.
const LINKABLE_EDGE_KINDS: [(&str, Option<&str>); 3] =
    [("CALLS", Some("Function")), ("SUPERTYPE_OF", Some("Type")), ("REFERENCES", None)];

fn required_target_kind(edge_kind: &str) -> Option<Option<&'static str>> {
    LINKABLE_EDGE_KINDS.iter().find(|(kind, _)| *kind == edge_kind).map(|(_, required)| *required)
}

/// Whether a node of this `nativeKind` declares something, i.e. may be a
/// candidate at all (contract step 1). Placeholders are addresses, and a
/// container node is core's bookkeeping with no source of its own.
fn is_declaration(native_kind: Option<&str>) -> bool {
    !matches!(
        native_kind,
        Some(
            PENDING_SYMBOL_NATIVE_KIND
                | REEXPORT_NATIVE_KIND
                | RESOLVED_MODULE_NATIVE_KIND
                | CONTAINER_NATIVE_KIND
        )
    )
}

/// [`is_declaration`] as a SQL condition on `nodes`. `IS NULL OR`, because
/// `NOT IN` on an ordinary node's NULL `nativeKind` is NULL, not true.
fn declaration_filter() -> String {
    format!(
        "(nativeKind IS NULL OR nativeKind NOT IN ('{PENDING_SYMBOL_NATIVE_KIND}', '{REEXPORT_NATIVE_KIND}', \
         '{RESOLVED_MODULE_NATIVE_KIND}', '{CONTAINER_NATIVE_KIND}'))"
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkSummary {
    /// Usage edges repointed from a placeholder onto the real symbol.
    pub linked_edges: usize,
}

/// Where a target's key is looked up. A container key is only unique within
/// its language (`graph::containers`), so the language travels with it; a
/// file path is global, and a file scope deliberately does not filter by
/// language - a `.js` file importing from a `.ts` one is one project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Scope {
    File(String),
    Container { language: String, key: String },
}

impl Scope {
    /// A `placeholder_targets` row's scope. `language` is the language of the
    /// node the row belongs to: a container target is always in its
    /// requester's own language.
    fn from_row(scope_kind: &str, scope: String, language: &str) -> Option<Self> {
        match scope_kind {
            SCOPE_FILE => Some(Scope::File(scope)),
            SCOPE_CONTAINER => Some(Scope::Container { language: language.to_string(), key: scope }),
            _ => None,
        }
    }

    /// The scopes a node published from `file` (and, if it has one,
    /// `container`) is addressable under.
    fn of_node(file: &str, language: &str, container: Option<&str>) -> Vec<Scope> {
        let mut scopes = vec![Scope::File(file.to_string())];
        scopes.extend(Self::container_of(language, container));
        scopes
    }

    fn container_of(language: &str, container: Option<&str>) -> Option<Scope> {
        container
            .filter(|key| !key.is_empty())
            .map(|key| Scope::Container { language: language.to_string(), key: key.to_string() })
    }

    /// `(scopeKind, scope, language filter)` as bound into the
    /// `placeholder_targets` lookups, which carry no language column: a
    /// container scope filters on the placeholder node's own language instead.
    fn bind(&self) -> (&'static str, &str, Option<&str>) {
        match self {
            Scope::File(path) => (SCOPE_FILE, path, None),
            Scope::Container { language, key } => (SCOPE_CONTAINER, key, Some(language)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Name(String),
    QualifiedName(String),
}

impl Key {
    fn from_row(key_kind: &str, key: String) -> Option<Self> {
        match key_kind {
            KEY_NAME => Some(Key::Name(key)),
            KEY_QUALIFIED_NAME => Some(Key::QualifiedName(key)),
            _ => None,
        }
    }
}

/// A scope and a key string as [`link_diff`] probes `placeholder_targets`
/// with - a name, a qualifiedName, or `*` for "anything in that scope".
type Address = (Scope, String);

/// Who a placeholder is asking on behalf of - the half of its target the
/// visibility check reads.
#[derive(Debug)]
struct Requester {
    file: String,
    container: Option<String>,
    language: String,
}

/// One pending placeholder with its target read back out of
/// `placeholder_targets`.
#[derive(Debug)]
struct Placeholder {
    id: String,
    scope: Scope,
    key: Key,
    requester: Requester,
}

/// Every placeholder read in this module is read through this projection, so
/// the one row shape [`placeholder_from_row`] understands is the only one.
/// `LEFT JOIN`, so a placeholder with no target row still comes back - and is
/// counted rather than silently invisible.
const PLACEHOLDER_SELECT: &str = "SELECT n.id, n.language, t.scopeKind, t.scope, t.keyKind, t.key, \
     t.fromContainer, t.fromFile FROM nodes n LEFT JOIN placeholder_targets t ON t.nodeId = n.id";

/// `None` for a placeholder with no usable target: no row at all (an
/// underivable legacy address), or one whose kinds this build does not know.
fn placeholder_from_row(row: &Row) -> rusqlite::Result<Option<Placeholder>> {
    let id: String = row.get(0)?;
    let language: String = row.get(1)?;
    let (Some(scope_kind), Some(scope), Some(key_kind), Some(key), Some(from_file)) = (
        row.get::<_, Option<String>>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, Option<String>>(5)?,
        row.get::<_, Option<String>>(7)?,
    ) else {
        return Ok(None);
    };
    let from_container: Option<String> = row.get(6)?;
    let (Some(scope), Some(key)) =
        (Scope::from_row(&scope_kind, scope, &language), Key::from_row(&key_kind, key))
    else {
        return Ok(None);
    };
    Ok(Some(Placeholder {
        id,
        scope,
        key,
        requester: Requester {
            file: from_file,
            container: from_container.filter(|key| !key.is_empty()),
            language,
        },
    }))
}

/// The placeholders one pass will try, plus how many it had to skip for want
/// of a target.
#[derive(Default)]
struct Pending {
    placeholders: Vec<Placeholder>,
    untargeted: usize,
}

impl Pending {
    fn collect<I>(&mut self, rows: I) -> Result<()>
    where
        I: Iterator<Item = rusqlite::Result<Option<Placeholder>>>,
    {
        for row in rows {
            match row.context("failed to read a pending symbol")? {
                Some(placeholder) => self.placeholders.push(placeholder),
                None => self.untargeted += 1,
            }
        }
        Ok(())
    }
}

/// Links every pending symbol in the index. The whole-project pass, run once
/// a bulk index has committed its last batch - at which point every symbol
/// the walk will ever produce is in, so a placeholder that finds no target
/// here has none to find.
pub fn link_all(conn: &mut Connection) -> Result<LinkSummary> {
    let mut pending = Pending::default();
    {
        let mut stmt = conn
            .prepare(&format!("{PLACEHOLDER_SELECT} WHERE n.kind = ?1 AND n.nativeKind = ?2"))
            .context("failed to prepare the pending-symbol scan")?;
        let rows = stmt
            .query_map(params![MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND], placeholder_from_row)
            .context("failed to scan for pending symbols")?;
        pending.collect(rows)?;
    }
    link(conn, pending)
}

/// Links what one just-applied diff could have changed, without rescanning
/// the whole index. Six things can newly make a placeholder linkable:
///
///  1. a placeholder the diff itself added or re-targeted (the reindexed
///     file's own imports);
///  2. a *declaration* the diff added that some placeholder may be waiting for
///     under its file - the cross-file case, and the reason a newly written
///     export does not stay uncallable until each of its callers happens to be
///     edited;
///  3. a *re-export* the diff added, which answers the same waiting
///     placeholders a declaration would: a barrel that starts forwarding a
///     name is, to everything importing through it, the name appearing;
///  4. a usage edge the diff added onto a placeholder that is already in the
///     index and already linked once. That placeholder is not in the diff (it
///     did not change), so nothing else here would look at it, and its brand
///     new edge would otherwise sit unresolved forever;
///  5. a declaration the diff put *into a container*, which answers the
///     placeholders scoped to that container - the container counterpart of
///     the second trigger, and the design doc's one new trigger. It fires for
///     a node that moved into a container as much as for a new one;
///  6. a container that just came into existence, which can complete the
///     parent chain of every container below it - a requester in
///     `crate::net::http` cannot see a `pub(crate)` item until
///     `crate::net`'s row (and its `parentKey`) exists, and in an incremental
///     build that row may arrive after the requester's own file. See
///     [`requesters_below_new_containers`].
///
/// The second, third and fifth are not looked up under their own address
/// alone. A placeholder reaching a symbol through a barrel is addressed at
/// the *barrel*, so [`republished_addresses`] walks the re-export chains back
/// up from what changed to every address that now answers differently - the
/// mirror of the walk [`Resolver::resolve`] runs down from an importer.
///
/// Every trigger is keyed off `placeholder_targets` (by `idx_targets_scope`)
/// or a primary key, and every one is deliberately over-inclusive: a
/// placeholder it revisits that turns out not to be answerable is simply left
/// as it was, so the only way to get one wrong is to *miss* a placeholder.
///
/// Not covered, deliberately, and for the same reason `graph::imports` does
/// not cover it: a symbol *deleted* from the project, and more generally any
/// change that makes an already-linked edge's answer worse (a second
/// candidate appearing, a shallower declaration shadowing the linked one, a
/// visibility narrowing). Linking only ever moves an edge that still hangs on
/// its placeholder; edges into a deleted node go when something deletes it,
/// and the importers' edges come back as fresh placeholders the next time
/// those importers are reindexed. That is also the one sense in which this and
/// [`link_all`] can disagree: on a sequence of diffs in which an answer only
/// ever *improves*, the two produce the same edges (asserted by
/// `link_all_and_link_diff_agree_on_the_same_end_state`).
pub fn link_diff(conn: &mut Connection, diff: &Diff) -> Result<LinkSummary> {
    let mut ids: BTreeSet<String> = diff
        .upsert_nodes
        .iter()
        .filter(|node| is_placeholder(&node.kind, node.native_kind.as_deref()))
        .map(|node| node.id.clone())
        .collect();

    let (name_seeds, exact_seeds) = seeds(diff);
    let mut addresses = republished_addresses(conn, name_seeds)?;
    addresses.extend(exact_seeds);
    waiting_placeholders(conn, &addresses, &mut ids)?;

    ids.extend(
        diff.upsert_edges
            .iter()
            .filter(|edge| required_target_kind(&edge.kind).is_some())
            .map(|edge| edge.to_id.clone()),
    );
    ids.extend(requesters_below_new_containers(conn, diff)?);

    let mut pending = Pending::default();
    {
        // The id set above is a superset - edge targets that are ordinary
        // declarations, waiting rows that are re-exports - and this is where
        // it narrows to pending symbols, by primary key.
        let mut by_id = conn
            .prepare(&format!("{PLACEHOLDER_SELECT} WHERE n.id = ?1 AND n.kind = ?2 AND n.nativeKind = ?3"))
            .context("failed to prepare the placeholder-by-id lookup")?;
        for id in &ids {
            let rows = by_id
                .query_map(params![id, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND], placeholder_from_row)
                .context("failed to look up a placeholder a diff may have made linkable")?;
            pending.collect(rows)?;
        }
    }
    link(conn, pending)
}

fn is_placeholder(kind: &str, native_kind: Option<&str>) -> bool {
    kind == MODULE_KIND && native_kind == Some(PENDING_SYMBOL_NATIVE_KIND)
}

fn is_reexport(kind: &str, native_kind: Option<&str>) -> bool {
    kind == MODULE_KIND && native_kind == Some(REEXPORT_NATIVE_KIND)
}

/// The addresses a diff's declarations and re-exports publish, for
/// [`link_diff`]'s second, third and fifth triggers: `(scope, name)` pairs to
/// walk back up re-export chains from, and `(scope, qualifiedName)` pairs to
/// look up as they are (a qualifiedName key never walks).
///
/// A declaration is a seed under every scope a placeholder could find it in
/// *and* see it from: its file and its container for `public` and
/// `container(..)` visibility, its container alone for `file` visibility,
/// which no file-scoped lookup ever accepts (see the module doc). A TS
/// non-exported symbol has no container, so it is no seed at all - exactly
/// the old "exported symbols only" rule; the only lookups added for TS are
/// the exact ones for an exported symbol whose qualifiedName is not its name.
fn seeds(diff: &Diff) -> (Vec<Address>, Vec<Address>) {
    let mut named = Vec::new();
    let mut exact = Vec::new();
    for node in &diff.upsert_nodes {
        let native_kind = node.native_kind.as_deref();
        if is_reexport(&node.kind, native_kind) {
            for scope in Scope::of_node(&node.file_path, &node.language, node.container.as_deref()) {
                named.push((scope, node.name.clone()));
            }
            continue;
        }
        if !is_declaration(native_kind) {
            continue;
        }
        let scopes = match node.visibility.as_str() {
            VISIBILITY_PUBLIC | VISIBILITY_CONTAINER => {
                Scope::of_node(&node.file_path, &node.language, node.container.as_deref())
            }
            VISIBILITY_FILE => {
                Scope::container_of(&node.language, node.container.as_deref()).into_iter().collect()
            }
            _ => Vec::new(),
        };
        for scope in scopes {
            if node.qualified_name != node.name {
                exact.push((scope.clone(), node.qualified_name.clone()));
            }
            named.push((scope, node.name.clone()));
        }
    }
    (named, exact)
}

/// Adds to `ids` every placeholder whose target is one of `addresses`. The
/// key column is matched as a string, whichever key kind it is: a name and a
/// qualifiedName that happen to be spelled alike just means one placeholder
/// more is tried. A `*` address - a whole-module re-export appearing, which
/// does not say what it forwards - is answered by trying every placeholder
/// scoped there.
fn waiting_placeholders(conn: &Connection, addresses: &[Address], ids: &mut BTreeSet<String>) -> Result<()> {
    if addresses.is_empty() {
        return Ok(());
    }
    let mut exact = conn
        .prepare(
            "SELECT t.nodeId FROM placeholder_targets t JOIN nodes n ON n.id = t.nodeId \
             WHERE t.scopeKind = ?1 AND t.scope = ?2 AND t.key = ?3 \
               AND n.kind = ?4 AND n.nativeKind = ?5 AND (?6 IS NULL OR n.language = ?6)",
        )
        .context("failed to prepare the waiting-placeholder lookup")?;
    let mut whole_scope = conn
        .prepare(
            "SELECT t.nodeId FROM placeholder_targets t JOIN nodes n ON n.id = t.nodeId \
             WHERE t.scopeKind = ?1 AND t.scope = ?2 \
               AND n.kind = ?3 AND n.nativeKind = ?4 AND (?5 IS NULL OR n.language = ?5)",
        )
        .context("failed to prepare the waiting-placeholder scope lookup")?;

    let node_id = |row: &Row| row.get::<_, String>(0);
    for (scope, key) in addresses {
        let (scope_kind, scope, language) = scope.bind();
        let rows = if key == REEXPORT_ALL_NAME {
            whole_scope.query_map(
                params![scope_kind, scope, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND, language],
                node_id,
            )
        } else {
            exact.query_map(
                params![scope_kind, scope, key, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND, language],
                node_id,
            )
        }
        .context("failed to look up placeholders waiting on a new symbol")?;
        for row in rows {
            ids.insert(row.context("failed to read a waiting placeholder")?);
        }
    }
    Ok(())
}

/// Every `(scope, name)` address whose placeholders `seeds` could have made
/// resolvable: the seeds themselves, plus each address a re-export republishes
/// them under, transitively. The mirror image of [`Resolver::resolve`]'s walk -
/// that one walks a chain down from an importer, this one walks the same
/// chains back up from what just changed.
///
/// A re-export republishes `(scope, name)` when its target is that exact
/// name, or the whole scope (`*`, which never carries `default`). Its
/// republished address is its own file and container, under its published
/// name - or, for a whole-module one, under the name passed through.
///
/// A `*` seed (a whole-module re-export appearing) does not say which names
/// it forwards, so from one *every* re-export targeting that scope is taken:
/// a named `export { x } from "./barrel"` newly answers `x` when the barrel
/// starts `export *`-ing it. The old `<file>#<name>` walk only ever followed
/// `*` onwards from a `*`, which missed that case; the over-inclusion costs a
/// few placeholders tried in vain.
///
/// Bounded like the downward walk, and for the same reasons: a visited set for
/// cycles, [`MAX_REEXPORT_DEPTH`] for length.
fn republished_addresses(conn: &Connection, seeds: Vec<Address>) -> Result<Vec<Address>> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    const COLUMNS: &str = "SELECT n.filePath, n.language, n.container, n.name \
         FROM placeholder_targets t JOIN nodes n ON n.id = t.nodeId";
    let mut by_key = conn
        .prepare(&format!(
            "{COLUMNS} WHERE t.scopeKind = ?1 AND t.scope = ?2 AND t.key = ?3 AND t.keyKind = '{KEY_NAME}' \
             AND n.kind = ?4 AND n.nativeKind = ?5 AND (?6 IS NULL OR n.language = ?6)"
        ))
        .context("failed to prepare the re-exporter lookup")?;
    let mut by_scope = conn
        .prepare(&format!(
            "{COLUMNS} WHERE t.scopeKind = ?1 AND t.scope = ?2 AND t.keyKind = '{KEY_NAME}' \
             AND n.kind = ?3 AND n.nativeKind = ?4 AND (?5 IS NULL OR n.language = ?5)"
        ))
        .context("failed to prepare the whole-scope re-exporter lookup")?;

    let mut visited: HashSet<Address> = HashSet::new();
    let mut addresses = Vec::new();
    let mut frontier = seeds;

    for depth in 0..=MAX_REEXPORT_DEPTH {
        let mut next = Vec::new();
        for (scope, name) in frontier {
            if !visited.insert((scope.clone(), name.clone())) {
                continue;
            }
            addresses.push((scope.clone(), name.clone()));
            if depth == MAX_REEXPORT_DEPTH {
                continue;
            }

            let (scope_kind, scope_value, language) = scope.bind();
            let map = |row: &Row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            };
            let mut reexporters = Vec::new();
            if name == REEXPORT_ALL_NAME {
                let rows = by_scope
                    .query_map(
                        params![scope_kind, scope_value, MODULE_KIND, REEXPORT_NATIVE_KIND, language],
                        map,
                    )
                    .context("failed to look up a scope's re-exporters")?;
                for row in rows {
                    reexporters.push(row.context("failed to read a re-exporter")?);
                }
            } else {
                // Named after this exact symbol, or swept up by a whole-module
                // re-export of the scope - which, per the language, never
                // carries a default export.
                let mut keys = vec![name.as_str()];
                if name != DEFAULT_EXPORT_NAME {
                    keys.push(REEXPORT_ALL_NAME);
                }
                for key in keys {
                    let rows = by_key
                        .query_map(
                            params![
                                scope_kind,
                                scope_value,
                                key,
                                MODULE_KIND,
                                REEXPORT_NATIVE_KIND,
                                language
                            ],
                            map,
                        )
                        .context("failed to look up a symbol's re-exporters")?;
                    for row in rows {
                        reexporters.push(row.context("failed to read a re-exporter")?);
                    }
                }
            }

            for (file, language, container, published) in reexporters {
                // A whole-module re-export publishes what it forwards under
                // the same name; a named one under its own.
                let published = if published == REEXPORT_ALL_NAME { name.clone() } else { published };
                for scope in Scope::of_node(&file, &language, container.as_deref()) {
                    next.push((scope, published.clone()));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Ok(addresses)
}

/// [`link_diff`]'s sixth trigger: the pending placeholders of every file in a
/// container the diff just brought into existence, or in any container below
/// one.
///
/// A requester's parent chain is read from `containers.parentKey`, and it
/// stops at the first ancestor with no row of its own ([`containers::
/// parent_chain`]). So a container's row appearing can extend the chain of
/// that container and of every container whose chain used to stop at it -
/// its descendants - and a `container(..)`-visible symbol one of their
/// placeholders was refused may now be visible. Nothing else in the diff
/// names those placeholders: they live in other files, and their targets
/// point somewhere else entirely.
///
/// "Just brought into existence" is read off what the diff leaves behind: a
/// container whose `memberCount` equals the number of its members this diff
/// upserted had no other member before it, so its row is new (or every member
/// was re-sent, which a restarted plugin does - over-inclusive, and harmless).
/// An ordinary edit re-sends only what changed, so it does not fire this.
///
/// A container's *parent changing* while it keeps existing is not covered:
/// that is two members disagreeing (a plugin bug `graph::containers` logs) or
/// a workspace change, which core follows with a per-language reindex and so
/// with [`link_all`].
///
/// Every step is keyed: the member count by `containers`' `UNIQUE (language,
/// key)`, the children by its `language` prefix (the table has a row per
/// package or module, not per symbol), a container's files by
/// `idx_nodes_container`, a file's placeholders by `idx_nodes_filePath`.
fn requesters_below_new_containers(conn: &Connection, diff: &Diff) -> Result<Vec<String>> {
    // Each id's last record decides its membership, as it does in
    // `graph::containers`.
    let mut last: HashMap<&str, Option<(&str, &str)>> = HashMap::new();
    for node in &diff.upsert_nodes {
        let key = node
            .container
            .as_deref()
            .filter(|key| !key.is_empty() && is_declaration(node.native_kind.as_deref()))
            .map(|key| (node.language.as_str(), key));
        last.insert(node.id.as_str(), key);
    }
    let mut upserted_members: HashMap<(&str, &str), i64> = HashMap::new();
    for key in last.into_values().flatten() {
        *upserted_members.entry(key).or_default() += 1;
    }
    if upserted_members.is_empty() {
        return Ok(Vec::new());
    }

    let mut member_count = conn
        .prepare("SELECT memberCount FROM containers WHERE language = ?1 AND key = ?2")
        .context("failed to prepare the container member-count lookup")?;
    let mut new_containers: Vec<(&str, &str)> = Vec::new();
    for (&(language, key), &upserted) in &upserted_members {
        let stored: Option<i64> = member_count
            .query_row(params![language, key], |row| row.get(0))
            .optional()
            .context("failed to read a container's member count")?;
        if stored == Some(upserted) {
            new_containers.push((language, key));
        }
    }
    if new_containers.is_empty() {
        return Ok(Vec::new());
    }
    new_containers.sort_unstable();

    let mut children_of = conn
        .prepare("SELECT key, parentKey FROM containers WHERE language = ?1 AND parentKey IS NOT NULL")
        .context("failed to prepare the container-children scan")?;
    let mut children: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut below: BTreeSet<(String, String)> = BTreeSet::new();
    for (language, key) in new_containers {
        if !children.contains_key(language) {
            let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
            let rows = children_of
                .query_map(params![language], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .context("failed to read a language's container tree")?;
            for row in rows {
                let (child, parent) = row.context("failed to read a container's parent")?;
                by_parent.entry(parent).or_default().push(child);
            }
            children.insert(language.to_string(), by_parent);
        }
        let tree = &children[language];
        let mut stack = vec![key.to_string()];
        while let Some(container) = stack.pop() {
            if !below.insert((language.to_string(), container.clone())) {
                continue;
            }
            stack.extend(tree.get(&container).into_iter().flatten().cloned());
        }
    }

    let mut files_of = conn
        .prepare("SELECT DISTINCT filePath FROM nodes WHERE language = ?1 AND container = ?2")
        .context("failed to prepare the container-files lookup")?;
    let mut placeholders_in = conn
        .prepare("SELECT id FROM nodes WHERE filePath = ?1 AND kind = ?2 AND nativeKind = ?3")
        .context("failed to prepare the file-placeholders lookup")?;
    let mut files: BTreeSet<String> = BTreeSet::new();
    for (language, container) in &below {
        let rows = files_of
            .query_map(params![language, container], |row| row.get::<_, String>(0))
            .context("failed to look up a container's files")?;
        for row in rows {
            files.insert(row.context("failed to read a container's file")?);
        }
    }
    let mut ids = Vec::new();
    for file in &files {
        let rows = placeholders_in
            .query_map(params![file, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND], |row| row.get::<_, String>(0))
            .context("failed to look up a file's placeholders")?;
        for row in rows {
            ids.push(row.context("failed to read a file's placeholder")?);
        }
    }
    Ok(ids)
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
fn link(conn: &mut Connection, pending: Pending) -> Result<LinkSummary> {
    let Pending { placeholders, untargeted } = pending;
    let mut summary = LinkSummary::default();
    if placeholders.is_empty() {
        report_untargeted(untargeted, 0);
        return Ok(summary);
    }

    let tx = conn.transaction().context("failed to start the symbol-linking transaction")?;
    let untargeted_reexports;
    {
        let mut pending_kinds = tx
            .prepare("SELECT DISTINCT kind FROM edges WHERE toId = ?1")
            .context("failed to prepare the pending-edge-kind scan")?;
        let mut repoint = tx
            .prepare("UPDATE edges SET toId = ?1, resolved = 1 WHERE toId = ?2 AND kind = ?3")
            .context("failed to prepare the edge repoint")?;
        let mut resolver = Resolver::new(&tx)?;

        for placeholder in placeholders {
            let edge_kinds: Vec<String> = pending_kinds
                .query_map(params![placeholder.id], |row| row.get(0))
                .context("failed to read a placeholder's edge kinds")?
                .collect::<rusqlite::Result<_>>()
                .context("failed to collect a placeholder's edge kinds")?;
            if edge_kinds.is_empty() {
                continue; // already linked, and nothing new points here
            }

            let candidates = resolver.resolve(&placeholder)?;
            // Nothing visible under that key, here or anywhere the scope
            // forwards to: the scope is not in the index (gitignored,
            // excluded, another language), does not offer it to this
            // requester, or ends its chain somewhere that does not. Leaving
            // the placeholder alone *is* the graceful fallback.
            if candidates.is_empty() {
                continue;
            }

            for edge_kind in edge_kinds {
                let Some(required) = required_target_kind(&edge_kind) else {
                    continue; // not a usage edge - nothing here linked it, so nothing here moves it
                };
                let mut fitting = candidates.iter().filter(|candidate| match required {
                    Some(required) => candidate.kind == required,
                    None => true,
                });
                let target_id = match (fitting.next(), fitting.next()) {
                    (Some(candidate), None) => candidate.id.clone(),
                    // Nothing of the right kind, or several equally good
                    // candidates: a missing edge beats a wrong one.
                    _ => continue,
                };

                summary.linked_edges += repoint
                    .execute(params![target_id, placeholder.id, edge_kind])
                    .context("failed to repoint a usage edge")?;
            }
        }
        untargeted_reexports = resolver.untargeted_reexports.len();
    }
    tx.commit().context("failed to commit the symbol-linking transaction")?;

    report_untargeted(untargeted, untargeted_reexports);
    Ok(summary)
}

/// At most one line per pass, however many placeholders it concerns: an
/// underivable legacy address is a plugin bug worth seeing, and a message per
/// placeholder per edit is noise that hides it.
fn report_untargeted(placeholders: usize, reexports: usize) {
    if placeholders == 0 && reexports == 0 {
        return;
    }
    eprintln!(
        "g-mesh: left {placeholders} pending-symbol placeholder(s) and skipped {reexports} re-export(s) with no \
         placeholder target (an address the plugin sent in a shape core could not read) - unlinked, not guessed"
    );
}

/// One node a target's key matched, with what the visibility and kind checks
/// need of it.
#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    kind: String,
    file_path: String,
    language: String,
    visibility: String,
    visibility_container: Option<String>,
}

/// One re-export hop: the scope and key a re-export forwards to.
type Hop = (Scope, Key);

/// The lookups one linking pass needs, prepared once and memoized per
/// distinct question - see the module doc's "Cost" section. Everything cached
/// here is a fact about the index inside one transaction, which cannot change
/// under it; only [`Resolver::resolve`]'s walk depends on who is asking.
struct Resolver<'c> {
    conn: &'c Connection,
    in_file_by_name: Statement<'c>,
    in_file_by_qualified_name: Statement<'c>,
    in_container_by_name: Statement<'c>,
    in_container_by_qualified_name: Statement<'c>,
    reexports_in_file: Statement<'c>,
    reexports_in_container: Statement<'c>,
    declared: HashMap<(Scope, Key), Vec<Candidate>>,
    hops: HashMap<(Scope, String), Vec<Hop>>,
    /// `(language, container)` -> that container plus its parent chain.
    visible_from: HashMap<(String, String), HashSet<String>>,
    untargeted_reexports: HashSet<String>,
}

impl<'c> Resolver<'c> {
    fn new(conn: &'c Connection) -> Result<Self> {
        const CANDIDATE: &str =
            "SELECT id, kind, filePath, language, visibility, visibilityContainer FROM nodes";
        const REEXPORT: &str = "SELECT n.id, n.name, n.language, t.scopeKind, t.scope, t.keyKind, t.key \
             FROM nodes n LEFT JOIN placeholder_targets t ON t.nodeId = n.id";
        let declarations = declaration_filter();
        let prepare = |sql: String| conn.prepare(&sql).context("failed to prepare a symbol-linking lookup");
        Ok(Resolver {
            conn,
            in_file_by_name: prepare(format!("{CANDIDATE} WHERE filePath = ?1 AND name = ?2 AND {declarations}"))?,
            in_file_by_qualified_name: prepare(format!(
                "{CANDIDATE} WHERE filePath = ?1 AND qualifiedName = ?2 AND {declarations}"
            ))?,
            in_container_by_name: prepare(format!(
                "{CANDIDATE} WHERE language = ?1 AND container = ?2 AND name = ?3 AND {declarations}"
            ))?,
            // `+` keeps the planner on idx_nodes_qualifiedName: the container
            // index would match every member of the container first.
            in_container_by_qualified_name: prepare(format!(
                "{CANDIDATE} WHERE +language = ?1 AND +container = ?2 AND qualifiedName = ?3 AND {declarations}"
            ))?,
            reexports_in_file: prepare(format!(
                "{REEXPORT} WHERE n.filePath = ?1 AND n.kind = ?2 AND n.nativeKind = ?3 AND n.name IN (?4, ?5)"
            ))?,
            reexports_in_container: prepare(format!(
                "{REEXPORT} WHERE n.language = ?1 AND n.container = ?2 AND n.kind = ?3 AND n.nativeKind = ?4 \
                 AND n.name IN (?5, ?6)"
            ))?,
            declared: HashMap::new(),
            hops: HashMap::new(),
            visible_from: HashMap::new(),
            untargeted_reexports: HashSet::new(),
        })
    }

    /// The nodes `placeholder` may be linked to: every visible declaration
    /// matching its key at the shallowest level of its scope's re-export
    /// chains that has any, deduplicated.
    ///
    /// Breadth-first, so a name a scope both declares and re-exports resolves
    /// to the declaration - the language's own rule. Bounded twice over: the
    /// visited set makes a re-export cycle terminate, [`MAX_REEXPORT_DEPTH`]
    /// bounds an acyclic chain, and both are needed since one does not imply
    /// the other.
    ///
    /// Ambiguity is not resolved here. Several branches of a barrel can each
    /// answer, and they are all returned: the caller refuses to move an edge
    /// that more than one candidate fits, which is the same rule as for a name
    /// a single scope declares twice.
    fn resolve(&mut self, placeholder: &Placeholder) -> Result<Vec<Candidate>> {
        let mut frontier: Vec<Hop> = vec![(placeholder.scope.clone(), placeholder.key.clone())];
        let mut visited: HashSet<Hop> = frontier.iter().cloned().collect();

        for depth in 0..=MAX_REEXPORT_DEPTH {
            let mut candidates: Vec<Candidate> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for (scope, key) in &frontier {
                for candidate in self.declared(scope, key)? {
                    // The same node can be reached under its file *and* its
                    // container; it is still one candidate, not an ambiguity.
                    if !seen.contains(&candidate.id)
                        && self.visible(&candidate, scope, &placeholder.requester)?
                    {
                        seen.insert(candidate.id.clone());
                        candidates.push(candidate);
                    }
                }
            }
            if !candidates.is_empty() || depth == MAX_REEXPORT_DEPTH {
                return Ok(candidates);
            }

            let mut next = Vec::new();
            for (scope, key) in &frontier {
                let Key::Name(name) = key else {
                    continue; // a qualifiedName names a declaration, never a pass-through
                };
                for hop in self.hops(scope, name)? {
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

    /// Every declaration in `scope` matching `key`, visible or not.
    fn declared(&mut self, scope: &Scope, key: &Key) -> Result<Vec<Candidate>> {
        let cache_key = (scope.clone(), key.clone());
        if let Some(found) = self.declared.get(&cache_key) {
            return Ok(found.clone());
        }

        let map = |row: &Row| {
            Ok(Candidate {
                id: row.get(0)?,
                kind: row.get(1)?,
                file_path: row.get(2)?,
                language: row.get(3)?,
                visibility: row.get(4)?,
                visibility_container: row.get(5)?,
            })
        };
        let rows = match (scope, key) {
            (Scope::File(path), Key::Name(name)) => self.in_file_by_name.query_map(params![path, name], map),
            (Scope::File(path), Key::QualifiedName(qualified_name)) => {
                self.in_file_by_qualified_name.query_map(params![path, qualified_name], map)
            }
            (Scope::Container { language, key }, Key::Name(name)) => {
                self.in_container_by_name.query_map(params![language, key, name], map)
            }
            (Scope::Container { language, key }, Key::QualifiedName(qualified_name)) => {
                self.in_container_by_qualified_name.query_map(params![language, key, qualified_name], map)
            }
        }
        .context("failed to look up a pending symbol's candidates")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read a pending symbol's candidates")?;

        self.declared.insert(cache_key, rows.clone());
        Ok(rows)
    }

    /// Where `scope`'s re-exports forward `name` to.
    ///
    /// A named re-export answers with its target key, which is the name before
    /// the alias (`export { a as b } from "./y"` forwards `b` to `./y`'s `a`);
    /// a whole-module one has no name of its own to answer with, so it passes
    /// the one being looked for straight through - never `default`.
    fn hops(&mut self, scope: &Scope, name: &str) -> Result<Vec<Hop>> {
        let cache_key = (scope.clone(), name.to_string());
        if let Some(found) = self.hops.get(&cache_key) {
            return Ok(found.clone());
        }

        type ReexportRow =
            (String, String, String, Option<String>, Option<String>, Option<String>, Option<String>);
        let map = |row: &Row| -> rusqlite::Result<ReexportRow> {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
        };
        let rows: Vec<ReexportRow> = match scope {
            Scope::File(path) => self
                .reexports_in_file
                .query_map(params![path, MODULE_KIND, REEXPORT_NATIVE_KIND, name, REEXPORT_ALL_NAME], map),
            Scope::Container { language, key } => self.reexports_in_container.query_map(
                params![language, key, MODULE_KIND, REEXPORT_NATIVE_KIND, name, REEXPORT_ALL_NAME],
                map,
            ),
        }
        .context("failed to look up a scope's re-exports")?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read a scope's re-exports")?;

        let mut hops = Vec::new();
        for (id, published, language, scope_kind, target_scope, key_kind, key) in rows {
            let (Some(scope_kind), Some(target_scope), Some(key_kind), Some(key)) =
                (scope_kind, target_scope, key_kind, key)
            else {
                self.untargeted_reexports.insert(id);
                continue;
            };
            let Some(target_scope) = Scope::from_row(&scope_kind, target_scope, &language) else {
                self.untargeted_reexports.insert(id);
                continue;
            };
            let whole_module = published == REEXPORT_ALL_NAME;
            let hop_key = match key_kind.as_str() {
                KEY_NAME if !whole_module => Key::Name(key),
                KEY_NAME if name != DEFAULT_EXPORT_NAME => Key::Name(name.to_string()),
                KEY_NAME => continue, // `export *` never carries a default
                KEY_QUALIFIED_NAME if !whole_module => Key::QualifiedName(key),
                _ => {
                    self.untargeted_reexports.insert(id);
                    continue;
                }
            };
            hops.push((target_scope, hop_key));
        }

        self.hops.insert(cache_key, hops.clone());
        Ok(hops)
    }

    /// Contract step 5, as the module doc's "Visibility" section states it -
    /// including the narrowing of `file` visibility to container scopes.
    fn visible(&mut self, candidate: &Candidate, scope: &Scope, requester: &Requester) -> Result<bool> {
        match candidate.visibility.as_str() {
            VISIBILITY_PUBLIC => Ok(true),
            VISIBILITY_FILE => {
                Ok(matches!(scope, Scope::Container { .. }) && candidate.file_path == requester.file)
            }
            VISIBILITY_CONTAINER => {
                // A container-private node with no container named is a
                // malformed row; refusing it is the missing-edge side.
                let (Some(visible_in), Some(from)) = (&candidate.visibility_container, &requester.container)
                else {
                    return Ok(false);
                };
                if candidate.language != requester.language {
                    return Ok(false);
                }
                let cache_key = (requester.language.clone(), from.clone());
                if !self.visible_from.contains_key(&cache_key) {
                    let mut chain: HashSet<String> =
                        containers::parent_chain(self.conn, &requester.language, from)?.into_iter().collect();
                    chain.insert(from.clone());
                    self.visible_from.insert(cache_key.clone(), chain);
                }
                Ok(self.visible_from[&cache_key].contains(visible_in))
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

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
        let mut node = NodeRecord::new(format!("{kind}:{file}:{name}"), kind, name, name, file, "typescript");
        // Both fields, kept in lockstep by hand here the same way every
        // other `NodeRecord` constructor in this codebase has to - see
        // `NodeRecord.exported`'s own doc comment on why the database no
        // longer can disagree with `visibility`, but this in-memory struct
        // still technically could if only one of the two were set.
        node.exported = exported;
        node.visibility = if exported { "public" } else { "file" }.to_string();
        node
    }

    /// A `placeholder_targets` row as a plugin states it (`fromFile` is not
    /// part of it: `apply_diff` fills that from the placeholder's own file).
    fn target(
        scope_kind: &str,
        scope: &str,
        key_kind: &str,
        key: &str,
        from: Option<&str>,
    ) -> PlaceholderTargetRecord {
        PlaceholderTargetRecord {
            scope_kind: scope_kind.to_string(),
            scope: scope.to_string(),
            key_kind: key_kind.to_string(),
            key: key.to_string(),
            from_container: from.map(str::to_string),
        }
    }

    /// The plugin's own shape for a pending symbol: `filePath` is the
    /// *importing* file (that is where the usage is written), and its target
    /// is the export it is waiting for. `qualifiedName` still spells the v1
    /// `<file>#<name>` address, as the TS plugin still sends it - nothing in
    /// the linker reads it any more; the target is the address.
    fn placeholder_node(importer: &str, target_file: &str, name: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("pending:{importer}:{target_file}#{name}"),
            MODULE_KIND,
            name,
            format!("{target_file}#{name}"),
            importer,
            "typescript",
        );
        node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        node.target = Some(target(SCOPE_FILE, target_file, KEY_NAME, name, None));
        node
    }

    /// The plugin's own shape for a re-export: it lives in the file that
    /// *publishes* `published` (its `name`), and targets the name the file it
    /// forwards to exports (`export { mutate as change } from "./target"` in
    /// `index.ts` is `reexport_node("index.ts", "change", "target.ts",
    /// "mutate")`) - the same split `protocol::types`' legacy derivation
    /// makes.
    fn reexport_node(file: &str, published: &str, target_file: &str, exported: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("reexport:{file}:{target_file}#{exported}"),
            MODULE_KIND,
            published,
            format!("{target_file}#{exported}"),
            file,
            "typescript",
        );
        node.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
        node.target = Some(target(SCOPE_FILE, target_file, KEY_NAME, exported, None));
        node
    }

    /// `export * from "./target"` in `file`: it publishes every name the
    /// target exports, so neither end of the address can be spelled out.
    fn reexport_all(file: &str, target: &str) -> NodeRecord {
        reexport_node(file, REEXPORT_ALL_NAME, target, REEXPORT_ALL_NAME)
    }

    fn usage_edge(from: &str, kind: &str, placeholder: &NodeRecord) -> EdgeRecord {
        usage_edge_from(from, kind, placeholder, "tree-sitter")
    }

    fn usage_edge_from(from: &str, kind: &str, placeholder: &NodeRecord, source: &str) -> EdgeRecord {
        EdgeRecord::new(
            format!("edge:{from}:{kind}:{}", placeholder.id),
            from,
            placeholder.id.clone(),
            kind,
            source,
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

    /// Which layer last answered for an edge - the other half of what a
    /// semantic upgrade changes about a row.
    fn edge_source(conn: &Connection, edge_id: &str) -> String {
        conn.query_row("SELECT source FROM edges WHERE id = ?1", params![edge_id], |row| row.get(0)).unwrap()
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
            &Diff { upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)], ..Default::default() },
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
            &Diff { upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)], ..Default::default() },
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
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                placeholder_node("caller.ts", "target.ts", "mutate"),
            ],
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

    /// Two `export *` branches offering the same name: this pass calls it
    /// ambiguous - a missing edge beats a wrong one, exactly as for a name one
    /// file exports twice - and the semantic layer, which has the compiler's
    /// own module resolution, settles it afterwards.
    ///
    /// The two halves are asserted in order on purpose. The first is the
    /// contract of the fast layer, which has to keep behaving exactly as it
    /// did: whatever the checker later says, an index that has only been
    /// through the structural pass must not claim to know which branch won.
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
        assert_eq!(edge_source(&conn, &edge), "syntactic");

        // What the semantic pass answers, in the shape it answers it: the edge
        // re-sent under its own id (`plugins/typescript/src/semanticPass.ts`), which
        // is why this needed no storage path of its own - `apply_diff`'s
        // ON CONFLICT rewrites the row in place.
        //
        // `a.ts` and not `b.ts` because that is what TypeScript's own module
        // resolution hands a consumer, measured rather than assumed (tsc /
        // tsserver 5.9.3 on exactly this fixture): `tsc --noEmit` reports the
        // TS2308 ambiguity against the *second* `export *`, a barrel-level
        // diagnostic, while `definition` at the importer's `mutate` returns
        // exactly one location - the first branch's - and swapping the two
        // statements swaps the answer. See semanticPass.ts's module comment.
        let upgrade = Diff {
            upsert_edges: vec![EdgeRecord::new(
                edge.clone(),
                "Function:caller.ts:run",
                "Function:a.ts:mutate",
                "CALLS",
                "ts-compiler",
                true,
            )],
            ..Default::default()
        };
        apply_diff(&mut conn, &upgrade).unwrap();

        // Nothing left for this pass to link: the edge no longer hangs on the
        // placeholder, so the two layers cannot fight over it.
        assert_eq!(link_diff(&mut conn, &upgrade).unwrap(), LinkSummary::default());
        assert_eq!(edge_target(&conn, &edge), ("Function:a.ts:mutate".to_string(), true));
        assert_eq!(edge_source(&conn, &edge), "semantic");
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
            let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "barrel0.ts", "mutate");
            (conn, edge)
        }

        let (mut at_cap, edge) = chain(MAX_REEXPORT_DEPTH);
        assert_eq!(link_all(&mut at_cap).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&at_cap, &edge).0, format!("Function:barrel{MAX_REEXPORT_DEPTH}.ts:mutate"));

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

    // --- placeholders the semantic pass sends ------------------------------

    /// `import * as ns from "./mod"` then `ns.someExport()`. The structural
    /// pass emits nothing at all for that site - it never sees the bare name
    /// `someExport`, only a property access - so the placeholder and its edge
    /// arrive from the semantic pass instead, addressed at the declaration
    /// `tsserver` bound. Nothing here treats them differently: the address is
    /// the whole contract, and `source` is the plugin's to set.
    #[test]
    fn a_namespace_import_usage_from_the_semantic_pass_is_linked_like_any_other() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("app.ts", "run", "Function", true),
                    symbol("mod.ts", "someExport", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        let placeholder = placeholder_node("app.ts", "mod.ts", "someExport");
        let edge = usage_edge_from("Function:app.ts:run", "CALLS", &placeholder, "ts-compiler");
        let edge_id = edge.id.clone();
        let diff = Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(
            edge_target(&conn, &edge_id),
            ("Function:mod.ts:someExport".to_string(), true),
            "the call must land on the real declaration, and say so"
        );
        // Repointing settles *what* the edge points at; it never overwrites who
        // worked it out. An edge a checker answered must keep saying so.
        assert_eq!(edge_source(&conn, &edge_id), "semantic");
    }

    /// The same usage where the module `ns` names is a barrel: the semantic
    /// pass addresses whatever declaration site the checker reported, so a
    /// re-export statement is a legitimate address and the chain walk finishes
    /// from there exactly as it does for a named import.
    #[test]
    fn a_semantic_placeholder_addressed_at_a_barrel_is_followed_to_the_declaration() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    symbol("app.ts", "run", "Function", true),
                    reexport_node("barrel.ts", "someExport", "impl.ts", "realName"),
                    symbol("impl.ts", "realName", "Function", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        let placeholder = placeholder_node("app.ts", "barrel.ts", "someExport");
        let edge = usage_edge_from("Function:app.ts:run", "CALLS", &placeholder, "ts-compiler");
        let edge_id = edge.id.clone();
        let diff = Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() };
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge_id).0, "Function:impl.ts:realName");
        assert_eq!(edge_source(&conn, &edge_id), "semantic");
    }

    #[test]
    fn an_empty_diff_is_a_no_op() {
        let mut conn = setup();
        assert_eq!(link_diff(&mut conn, &Diff::default()).unwrap(), LinkSummary::default());
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    }

    // --- structured targets: what the old `<file>#<name>` string could not say --

    /// Where a declaration or a requester of a containered language sits.
    #[derive(Clone, Copy)]
    struct At<'a> {
        file: &'a str,
        language: &'a str,
        container: &'a str,
        parent: Option<&'a str>,
    }

    enum Vis<'a> {
        Public,
        File,
        Container(&'a str),
    }

    fn last_segment(qualified_name: &str) -> &str {
        qualified_name.rsplit(['.', ':']).next().expect("rsplit always yields one item")
    }

    /// A member of `at.container`, id `<kind>:<file>:<qualifiedName>`, named by
    /// the qualifiedName's last segment (`Server.Close` is `Close`,
    /// `mycrate::util::helper` is `helper`).
    fn member(at: At, kind: &str, qualified_name: &str, visibility: Vis) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("{kind}:{}:{qualified_name}", at.file),
            kind,
            last_segment(qualified_name),
            qualified_name,
            at.file,
            at.language,
        );
        node.container = Some(at.container.to_string());
        node.container_parent = at.parent.map(str::to_string);
        match visibility {
            Vis::Public => {
                node.visibility = VISIBILITY_PUBLIC.to_string();
                node.exported = true;
            }
            Vis::File => node.visibility = VISIBILITY_FILE.to_string(),
            Vis::Container(key) => {
                node.visibility = VISIBILITY_CONTAINER.to_string();
                node.visibility_container = Some(key.to_string());
            }
        }
        node
    }

    /// A placeholder in `at.file`, asking from `at.container`, scoped to the
    /// container `scope`. Its `qualifiedName` is deliberately *not* the v1
    /// `<file>#<name>` shape: nothing may parse it.
    fn container_placeholder(at: At, scope: &str, key_kind: &str, key: &str) -> NodeRecord {
        let id = format!("pending:{}:{scope}:{key_kind}:{key}", at.file);
        let mut node = NodeRecord::new(id.clone(), MODULE_KIND, last_segment(key), id, at.file, at.language);
        node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        node.target = Some(target(SCOPE_CONTAINER, scope, key_kind, key, Some(at.container)));
        node
    }

    /// A re-export living in `at` (its file and its container) that publishes
    /// `published`, forwarding to `key` in the container `scope` - a Rust
    /// `pub use crate::util::helper;` inside `mod prelude`.
    fn container_reexport(at: At, published: &str, scope: &str, key: &str) -> NodeRecord {
        let id = format!("reexport:{}:{scope}:{key}", at.file);
        let mut node = NodeRecord::new(id.clone(), MODULE_KIND, published, id, at.file, at.language);
        node.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
        node.container = Some(at.container.to_string());
        node.target = Some(target(SCOPE_CONTAINER, scope, KEY_NAME, key, Some(at.container)));
        node
    }

    /// `caller` (a node already in the index, or in `with`) uses `placeholder`
    /// through a `kind` edge; everything in one diff, as one file's
    /// extraction would send it. Returns the edge id.
    fn use_through(
        conn: &mut Connection,
        with: Vec<NodeRecord>,
        caller: &str,
        kind: &str,
        placeholder: NodeRecord,
    ) -> String {
        let edge = usage_edge(caller, kind, &placeholder);
        let edge_id = edge.id.clone();
        let mut nodes = with;
        nodes.push(placeholder);
        apply_diff(conn, &Diff { upsert_nodes: nodes, upsert_edges: vec![edge], ..Default::default() })
            .unwrap();
        edge_id
    }

    fn upsert(conn: &mut Connection, nodes: Vec<NodeRecord>) {
        apply_diff(conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
    }

    const GO_UTIL: &str = "example.com/app/util";

    fn go_util_file(file: &str) -> At<'_> {
        At { file, language: "go", container: GO_UTIL, parent: None }
    }

    fn go_cmd() -> At<'static> {
        At { file: "cmd/main.go", language: "go", container: "example.com/app/cmd", parent: None }
    }

    /// A Go package spread over two files, called from another package by a
    /// container-scoped name: the edge lands wherever in the package the name
    /// is declared, which a file-scoped address could only have said by
    /// knowing the file - the one thing an importer of a package never writes
    /// down.
    #[test]
    fn a_container_scoped_name_links_to_the_member_whichever_file_declares_it() {
        let mut conn = setup();
        upsert(
            &mut conn,
            vec![
                member(go_util_file("util/point.go"), "Function", "NewPoint", Vis::Public),
                member(go_util_file("util/rect.go"), "Function", "NewRect", Vis::Public),
            ],
        );
        let main = member(go_cmd(), "Function", "main", Vis::Public);
        let point = use_through(
            &mut conn,
            vec![main],
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "NewPoint"),
        );
        let rect = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "NewRect"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
        assert_eq!(edge_target(&conn, &point), ("Function:util/point.go:NewPoint".to_string(), true));
        assert_eq!(edge_target(&conn, &rect), ("Function:util/rect.go:NewRect".to_string(), true));
    }

    /// `Server.Close` and `Client.Close` share a name; a name key cannot tell
    /// them apart and refuses, a qualifiedName key names one and lands.
    #[test]
    fn a_qualified_name_key_disambiguates_two_same_named_methods() {
        let mut conn = setup();
        let server = go_util_file("util/server.go");
        upsert(
            &mut conn,
            vec![
                member(server, "Function", "Server.Close", Vis::Public),
                member(server, "Function", "Client.Close", Vis::Public),
                member(go_cmd(), "Function", "main", Vis::Public),
            ],
        );
        let by_name = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Close"),
        );
        let exact = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "Client.Close"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert!(!edge_target(&conn, &by_name).1, "two Close methods: a name key must refuse");
        assert_eq!(edge_target(&conn, &exact), ("Function:util/server.go:Client.Close".to_string(), true));
    }

    /// Go's unexported: visible to its own package, and only to it - under a
    /// name key, and under a qualifiedName key too, which is what stops a
    /// semantic tier's mistake from linking a private symbol from outside.
    #[test]
    fn a_go_unexported_symbol_links_within_its_package_and_not_from_outside() {
        let mut conn = setup();
        let helper = go_util_file("util/helper.go");
        let sibling = go_util_file("util/use.go");
        upsert(
            &mut conn,
            vec![
                member(helper, "Function", "helper", Vis::Container(GO_UTIL)),
                member(sibling, "Function", "useHelper", Vis::Public),
                member(go_cmd(), "Function", "main", Vis::Public),
            ],
        );
        let inside = use_through(
            &mut conn,
            Vec::new(),
            "Function:util/use.go:useHelper",
            "CALLS",
            container_placeholder(sibling, GO_UTIL, KEY_NAME, "helper"),
        );
        let outside = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "helper"),
        );
        let outside_exact = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "REFERENCES",
            container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "helper"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &inside), ("Function:util/helper.go:helper".to_string(), true));
        assert!(!edge_target(&conn, &outside).1, "another package must not see an unexported symbol");
        assert!(!edge_target(&conn, &outside_exact).1, "not even by its exact qualifiedName");
    }

    /// The Rust crate the tests below share: `mycrate` with `mod util`
    /// (holding a `pub(crate) fn helper` and a private `fn private_helper`)
    /// and `mod net { mod http; }`, every module with a member so every parent
    /// is recorded - as a Rust plugin emits each `mod child;` item as a member
    /// of its parent (`graph::containers::parent_chain`).
    fn rust_lib() -> At<'static> {
        At { file: "src/lib.rs", language: "rust", container: "mycrate", parent: None }
    }
    fn rust_net() -> At<'static> {
        At { file: "src/net/mod.rs", language: "rust", container: "mycrate::net", parent: Some("mycrate") }
    }
    fn rust_util() -> At<'static> {
        At { file: "src/util.rs", language: "rust", container: "mycrate::util", parent: Some("mycrate") }
    }
    fn rust_http() -> At<'static> {
        At {
            file: "src/net/http.rs",
            language: "rust",
            container: "mycrate::net::http",
            parent: Some("mycrate::net"),
        }
    }
    fn rust_prelude() -> At<'static> {
        At {
            file: "src/prelude.rs",
            language: "rust",
            container: "mycrate::prelude",
            parent: Some("mycrate"),
        }
    }
    fn rust_other() -> At<'static> {
        At { file: "other/src/lib.rs", language: "rust", container: "othercrate", parent: None }
    }

    fn rust_lib_nodes() -> Vec<NodeRecord> {
        vec![
            member(rust_lib(), "Module", "mycrate::net", Vis::Public),
            member(rust_lib(), "Module", "mycrate::util", Vis::Public),
            member(rust_lib(), "Module", "mycrate::prelude", Vis::Public),
        ]
    }
    fn rust_net_nodes() -> Vec<NodeRecord> {
        vec![member(rust_net(), "Module", "mycrate::net::http", Vis::Public)]
    }
    fn rust_util_nodes() -> Vec<NodeRecord> {
        vec![
            member(rust_util(), "Function", "mycrate::util::helper", Vis::Container("mycrate")),
            member(rust_util(), "Function", "mycrate::util::private_helper", Vis::Container("mycrate::util")),
        ]
    }
    fn rust_get() -> NodeRecord {
        member(rust_http(), "Function", "mycrate::net::http::get", Vis::Container("mycrate::net::http"))
    }

    const RUST_GET: &str = "Function:src/net/http.rs:mycrate::net::http::get";
    const RUST_RUN: &str = "Function:other/src/lib.rs:othercrate::run";
    const RUST_HELPER: &str = "Function:src/util.rs:mycrate::util::helper";

    #[test]
    fn a_rust_pub_crate_symbol_is_visible_from_a_descendant_module_and_not_from_another_crate() {
        let mut conn = setup();
        let mut nodes = rust_lib_nodes();
        nodes.extend(rust_net_nodes());
        nodes.extend(rust_util_nodes());
        nodes.push(rust_get());
        nodes.push(member(rust_other(), "Function", "othercrate::run", Vis::Public));
        upsert(&mut conn, nodes);

        let descendant = use_through(
            &mut conn,
            Vec::new(),
            RUST_GET,
            "CALLS",
            container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
        );
        let private_from_elsewhere = use_through(
            &mut conn,
            Vec::new(),
            RUST_GET,
            "CALLS",
            container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "private_helper"),
        );
        let other_crate = use_through(
            &mut conn,
            Vec::new(),
            RUST_RUN,
            "CALLS",
            container_placeholder(rust_other(), "mycrate::util", KEY_NAME, "helper"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(
            edge_target(&conn, &descendant),
            (RUST_HELPER.to_string(), true),
            "pub(crate) is visible from mycrate::net::http, whose chain reaches mycrate"
        );
        assert!(
            !edge_target(&conn, &private_from_elsewhere).1,
            "a module-private item is not visible from outside its module"
        );
        assert!(!edge_target(&conn, &other_crate).1, "pub(crate) is not visible from another crate");
    }

    /// `pub use crate::util::helper;` inside `mod prelude`: a container's
    /// re-export is followed like a barrel's, and the declaration at the end
    /// is still checked against the original requester.
    #[test]
    fn a_container_reexport_is_followed_into_the_container_it_forwards_to() {
        let mut conn = setup();
        let mut nodes = rust_lib_nodes();
        nodes.extend(rust_net_nodes());
        nodes.extend(rust_util_nodes());
        nodes.push(rust_get());
        nodes.push(member(rust_other(), "Function", "othercrate::run", Vis::Public));
        nodes.push(member(rust_prelude(), "Function", "mycrate::prelude::marker", Vis::Public));
        nodes.push(container_reexport(rust_prelude(), "helper", "mycrate::util", "helper"));
        upsert(&mut conn, nodes);

        let inside = use_through(
            &mut conn,
            Vec::new(),
            RUST_GET,
            "CALLS",
            container_placeholder(rust_http(), "mycrate::prelude", KEY_NAME, "helper"),
        );
        let outside = use_through(
            &mut conn,
            Vec::new(),
            RUST_RUN,
            "CALLS",
            container_placeholder(rust_other(), "mycrate::prelude", KEY_NAME, "helper"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &inside).0, RUST_HELPER);
        assert!(
            !edge_target(&conn, &outside).1,
            "re-exporting does not widen the declaration's own visibility"
        );
    }

    /// The container counterpart of `adding_an_export_links_the_usages_that_
    /// were_waiting_for_it`: a usage waits on a package, and the symbol shows
    /// up in it later - first as a brand new node, then (second half) as an
    /// existing node re-sent as a member.
    #[test]
    fn a_node_joining_a_container_links_the_placeholders_waiting_on_it() {
        let mut conn = setup();
        let edge = use_through(
            &mut conn,
            vec![member(go_cmd(), "Function", "main", Vis::Public)],
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Helper"),
        );
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "the package is empty");

        let diff = Diff {
            upsert_nodes: vec![member(go_util_file("util/helper.go"), "Function", "Helper", Vis::Public)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();
        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge), ("Function:util/helper.go:Helper".to_string(), true));

        let waiting = use_through(
            &mut conn,
            Vec::new(),
            "Function:cmd/main.go:main",
            "CALLS",
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Moved"),
        );
        let mut loose = member(go_util_file("util/moved.go"), "Function", "Moved", Vis::Public);
        loose.container = None;
        upsert(&mut conn, vec![loose]);
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "not a member yet");

        let diff = Diff {
            upsert_nodes: vec![member(go_util_file("util/moved.go"), "Function", "Moved", Vis::Public)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();
        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &waiting).0, "Function:util/moved.go:Moved");
    }

    /// The parent chain completing after the requester: `mycrate::net::http`
    /// is indexed before `mycrate::net` has a row, so its chain stops at
    /// `mycrate::net` and `pub(crate)` is refused - correctly, for what the
    /// index knew. When `net`'s file arrives the chain reaches `mycrate`, and
    /// the placeholder in `http.rs`, which that diff does not mention at all,
    /// has to be revisited.
    #[test]
    fn a_container_arriving_above_a_requester_links_what_its_chain_now_reaches() {
        let mut conn = setup();
        let mut nodes = rust_lib_nodes();
        nodes.extend(rust_util_nodes());
        upsert(&mut conn, nodes);
        let edge = use_through(
            &mut conn,
            vec![rust_get()],
            RUST_GET,
            "CALLS",
            container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
        );
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "mycrate::net has no row yet");

        let diff = Diff { upsert_nodes: rust_net_nodes(), ..Default::default() };
        apply_diff(&mut conn, &diff).unwrap();
        assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &edge).0, RUST_HELPER);
    }

    /// `file` visibility inside a container (C++'s `static` or anonymous
    /// namespace in a namespace reopened across files): visible through the
    /// container from its own file, not from a sibling file.
    #[test]
    fn a_file_private_member_is_visible_through_its_container_only_from_its_own_file() {
        let mut conn = setup();
        let a = At { file: "lib/a.cpp", language: "cpp", container: "llvm", parent: None };
        let b = At { file: "lib/b.cpp", language: "cpp", container: "llvm", parent: None };
        upsert(
            &mut conn,
            vec![
                member(a, "Function", "llvm::helper", Vis::File),
                member(a, "Function", "llvm::fromA", Vis::Public),
                member(b, "Function", "llvm::fromB", Vis::Public),
            ],
        );
        let same_file = use_through(
            &mut conn,
            Vec::new(),
            "Function:lib/a.cpp:llvm::fromA",
            "CALLS",
            container_placeholder(a, "llvm", KEY_NAME, "helper"),
        );
        let other_file = use_through(
            &mut conn,
            Vec::new(),
            "Function:lib/b.cpp:llvm::fromB",
            "CALLS",
            container_placeholder(b, "llvm", KEY_NAME, "helper"),
        );

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(edge_target(&conn, &same_file).0, "Function:lib/a.cpp:llvm::helper");
        assert!(!edge_target(&conn, &other_file).1);
    }

    /// TS equivalence at its one edge: a placeholder addressed at the
    /// requester's *own* file (`import { render } from "./self"`, or the
    /// semantic pass answering `ns.render` through a barrel that re-exports
    /// the importer). The old lookup required `exported = 1`, so a
    /// non-exported method `render` in the same file was never a candidate;
    /// it must not become one - neither as an ambiguity beside the export,
    /// nor as the only fit when there is no export.
    #[test]
    fn a_self_addressed_placeholder_sees_only_what_its_file_exports() {
        let mut conn = setup();
        let mut method = symbol("view.ts", "View.render", "Function", false);
        method.name = "render".to_string();
        upsert(
            &mut conn,
            vec![
                symbol("view.ts", "run", "Function", true),
                symbol("view.ts", "render", "Function", true),
                method,
                symbol("view.ts", "paint", "Function", false),
            ],
        );

        let exported = seed_usage(&mut conn, "Function:view.ts:run", "CALLS", "view.ts", "render");
        let private = seed_usage(&mut conn, "Function:view.ts:run", "CALLS", "view.ts", "paint");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert_eq!(
            edge_target(&conn, &exported).0,
            "Function:view.ts:render",
            "the export, not an ambiguity"
        );
        assert!(
            !edge_target(&conn, &private).1,
            "a non-exported symbol never answers a file-scoped placeholder"
        );
    }

    /// The target is the address: a placeholder whose `qualifiedName` still
    /// looks like a v1 address but has no target row stays unlinked (reported,
    /// not guessed from the string), and one whose `qualifiedName` says
    /// something else entirely links by its target.
    #[test]
    fn the_target_row_is_the_address_and_the_qualified_name_is_never_parsed() {
        let mut conn = setup();
        seed_caller_and_target(&mut conn);

        let mut untargeted = placeholder_node("caller.ts", "target.ts", "mutate");
        untargeted.target = None;
        let untargeted_edge =
            use_through(&mut conn, Vec::new(), "Function:caller.ts:run", "CALLS", untargeted);

        let mut misleading = placeholder_node("caller.ts", "target.ts", "mutate");
        misleading.id = "pending:caller.ts:misleading".to_string();
        misleading.qualified_name = "elsewhere.ts#other".to_string();
        let misleading_edge =
            use_through(&mut conn, Vec::new(), "Function:caller.ts:run", "REFERENCES", misleading);

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
        assert!(!edge_target(&conn, &untargeted_edge).1, "no target row: left alone, never guessed");
        assert_eq!(edge_target(&conn, &misleading_edge).0, "Function:target.ts:mutate");
    }

    // --- link_all == link_diff ----------------------------------------------

    /// What an equivalence fixture expects of one usage edge: the node it
    /// lands on, or `None` for staying on its placeholder.
    type Expected = Vec<(String, Option<&'static str>)>;

    /// One file's extraction per diff - TS barrels, a Go package used from
    /// inside and outside, a Rust crate with a container re-export and a
    /// second crate - plus what each usage edge must end up pointing at.
    /// Every answer here only ever improves as files arrive, in any order: the
    /// condition under which [`link_diff`] promises to agree with
    /// [`link_all`] (see its doc on what is deliberately not covered).
    fn equivalence_fixture() -> (Vec<Diff>, Expected) {
        let mut expected: Expected = Vec::new();
        let mut uses = |edges: &mut Vec<EdgeRecord>,
                        from: &str,
                        kind: &str,
                        to: &NodeRecord,
                        lands: Option<&'static str>| {
            let edge = usage_edge(from, kind, to);
            expected.push((edge.id.clone(), lands));
            edges.push(edge);
        };
        let diff = |upsert_nodes: Vec<NodeRecord>, upsert_edges: Vec<EdgeRecord>| Diff {
            upsert_nodes,
            upsert_edges,
            ..Default::default()
        };
        let mut diffs = Vec::new();

        // TS: a declaring file, a barrel (whole-module and renaming), a caller.
        diffs.push(diff(
            vec![
                symbol("target.ts", "mutate", "Function", true),
                symbol("target.ts", "helper", "Function", false),
            ],
            Vec::new(),
        ));
        diffs.push(diff(
            vec![
                reexport_all("index.ts", "target.ts"),
                reexport_node("index.ts", "change", "target.ts", "mutate"),
            ],
            Vec::new(),
        ));
        let ts = [
            placeholder_node("caller.ts", "index.ts", "mutate"),
            placeholder_node("caller.ts", "index.ts", "change"),
            placeholder_node("caller.ts", "target.ts", "helper"),
            placeholder_node("caller.ts", "target.ts", "mutate"),
        ];
        let mut edges = Vec::new();
        uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[0], Some("Function:target.ts:mutate"));
        uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[1], Some("Function:target.ts:mutate"));
        uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[2], None);
        uses(&mut edges, "Function:caller.ts:run", "REFERENCES", &ts[3], Some("Function:target.ts:mutate"));
        let mut nodes = vec![symbol("caller.ts", "run", "Function", true)];
        nodes.extend(ts);
        diffs.push(diff(nodes, edges));

        // Go: a package in three files, used from inside and from outside.
        let helper_go = go_util_file("util/helper.go");
        diffs.push(diff(
            vec![
                member(helper_go, "Function", "Helper", Vis::Public),
                member(helper_go, "Function", "helper", Vis::Container(GO_UTIL)),
            ],
            Vec::new(),
        ));
        let server_go = go_util_file("util/server.go");
        diffs.push(diff(
            vec![
                member(server_go, "Function", "Server.Close", Vis::Public),
                member(server_go, "Function", "Client.Close", Vis::Public),
            ],
            Vec::new(),
        ));
        let go = [
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Helper"),
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "helper"),
            container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "Server.Close"),
            container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Close"),
        ];
        let main = "Function:cmd/main.go:main";
        let mut edges = Vec::new();
        uses(&mut edges, main, "CALLS", &go[0], Some("Function:util/helper.go:Helper"));
        uses(&mut edges, main, "CALLS", &go[1], None);
        uses(&mut edges, main, "CALLS", &go[2], Some("Function:util/server.go:Server.Close"));
        uses(&mut edges, main, "REFERENCES", &go[3], None);
        let mut nodes = vec![member(go_cmd(), "Function", "main", Vis::Public)];
        nodes.extend(go);
        diffs.push(diff(nodes, edges));
        let use_go = go_util_file("util/use.go");
        let sibling = container_placeholder(use_go, GO_UTIL, KEY_NAME, "helper");
        let mut edges = Vec::new();
        uses(
            &mut edges,
            "Function:util/use.go:useHelper",
            "CALLS",
            &sibling,
            Some("Function:util/helper.go:helper"),
        );
        diffs.push(diff(
            vec![member(use_go, "Function", "useHelper", Vis::Container(GO_UTIL)), sibling],
            edges,
        ));

        // Rust: one diff per module file, plus a second crate.
        diffs.push(diff(rust_lib_nodes(), Vec::new()));
        diffs.push(diff(rust_net_nodes(), Vec::new()));
        diffs.push(diff(rust_util_nodes(), Vec::new()));
        diffs.push(diff(
            vec![
                member(rust_prelude(), "Function", "mycrate::prelude::marker", Vis::Public),
                container_reexport(rust_prelude(), "helper", "mycrate::util", "helper"),
            ],
            Vec::new(),
        ));
        let rust = [
            container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
            container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "private_helper"),
            container_placeholder(rust_http(), "mycrate::prelude", KEY_NAME, "helper"),
        ];
        let mut edges = Vec::new();
        uses(&mut edges, RUST_GET, "CALLS", &rust[0], Some(RUST_HELPER));
        uses(&mut edges, RUST_GET, "CALLS", &rust[1], None);
        uses(&mut edges, RUST_GET, "CALLS", &rust[2], Some(RUST_HELPER));
        let mut nodes = vec![rust_get()];
        nodes.extend(rust);
        diffs.push(diff(nodes, edges));
        let other = container_placeholder(rust_other(), "mycrate::util", KEY_NAME, "helper");
        let mut edges = Vec::new();
        uses(&mut edges, RUST_RUN, "CALLS", &other, None);
        diffs
            .push(diff(vec![member(rust_other(), "Function", "othercrate::run", Vis::Public), other], edges));

        // Last, always: a second usage in caller.ts of an existing placeholder,
        // which the diff does not re-send.
        let reused = placeholder_node("caller.ts", "index.ts", "mutate");
        let mut edges = Vec::new();
        uses(&mut edges, "Function:caller.ts:again", "CALLS", &reused, Some("Function:target.ts:mutate"));
        diffs.push(diff(vec![symbol("caller.ts", "again", "Function", false)], edges));

        (diffs, expected)
    }

    /// Every usage edge (every edge but core's `DEFINES`), as `(id, toId,
    /// resolved)` in id order.
    fn usage_edges(conn: &Connection) -> Vec<(String, String, bool)> {
        conn.prepare("SELECT id, toId, resolved FROM edges WHERE kind <> 'DEFINES' ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The same end state built two ways - every diff applied and then one
    /// `link_all`, versus `link_diff` after each diff - must link the same
    /// edges, whatever order the files arrive in. The orders are the fixture's
    /// own, its reverse, and 100 deterministic shuffles; the fixture's last
    /// diff (a new usage onto an existing placeholder) always goes last, since
    /// that placeholder has to exist first.
    #[test]
    fn link_all_and_link_diff_agree_on_the_same_end_state() {
        let (diffs, expected) = equivalence_fixture();
        let count = diffs.len();

        let mut bulk = setup();
        for diff in &diffs {
            apply_diff(&mut bulk, diff).unwrap();
        }
        link_all(&mut bulk).unwrap();
        let reference = usage_edges(&bulk);

        // The reference has to be the right answer itself, or agreeing with
        // it proves nothing.
        assert_eq!(reference.len(), expected.len());
        for (id, to, resolved) in &reference {
            let (_, lands) = expected.iter().find(|(edge, _)| edge == id).expect("every edge is expected");
            match lands {
                Some(target) => assert_eq!((to.as_str(), *resolved), (*target, true), "{id}"),
                None => {
                    assert!(!resolved && to.starts_with("pending:"), "{id} must stay unresolved, is on {to}")
                }
            }
        }

        let movable = count - 1;
        let mut orders: Vec<Vec<usize>> = vec![(0..movable).collect(), (0..movable).rev().collect()];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..100 {
            let mut order: Vec<usize> = (0..movable).collect();
            for i in (1..movable).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                order.swap(i, (state % (i as u64 + 1)) as usize);
            }
            orders.push(order);
        }

        for mut order in orders {
            order.push(movable);
            let mut diffs: Vec<Option<Diff>> = equivalence_fixture().0.into_iter().map(Some).collect();
            let mut incremental = setup();
            for &index in &order {
                let diff = diffs[index].take().unwrap();
                apply_diff(&mut incremental, &diff).unwrap();
                link_diff(&mut incremental, &diff).unwrap();
            }
            assert_eq!(usage_edges(&incremental), reference, "diff order {order:?}");
        }
    }
}
