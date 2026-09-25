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

use crate::graph::containers;
use crate::graph::queries::{declaration_only, NON_DECLARATION_NATIVE_KINDS};
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
///
/// The list is `graph::queries`' [`NON_DECLARATION_NATIVE_KINDS`] and not one
/// of this module's own - see that module's header for what each of the five
/// kinds is. Its SQL twin here is [`declaration_only`], the same builder the
/// name lookups use.
///
/// ## `external_module` joined the list (GM-372)
///
/// It was the one kind this filter admitted while the lookups refused it.
/// GM-367 excluded it from the *read* path - a `find_definition("context")`
/// on gin was answering with `context_test.go`'s import line - and left this
/// side alone on purpose, because excluding a kind here repoints edges while
/// the index is being written, which is evidence of a different kind to
/// collect. GM-372 collected it and excluded the kind:
///
///  - **A link is a claim, and this one would be false.** A pending symbol
///    says "the declaration of `name` in scope `S`". An `external_module`
///    row in `S` is `S`'s own record that it imported `name` from somewhere
///    outside this project - the same node the lookups refuse - so repointing
///    onto it answers a different question and marks the edge `resolved = 1`
///    while doing so. There is no reading under which an import record is the
///    declaration a usage meant, which is why "link it and mark it" was never
///    an option here the way it was weighed and rejected for the read path.
///  - **Nothing wanted the edge.** A `CALLS` or `SUPERTYPE_OF` edge cannot
///    land on one anyway (contract step 3: kind `Module` is neither a
///    `Function` nor a `Type`), so the only edge at stake is `REFERENCES`,
///    and a `REFERENCES` edge onto one import record of one file is precisely
///    the per-file fragmentation GM-367 measured as useless (63 rows for
///    `net/http` on gin). What a caller genuinely wants said about an
///    external specifier is said by the `IMPORTS` edge `graph::imports`
///    deliberately leaves on the node, and by
///    `mcp::find_definition::import_only_refusal`.
///  - **The one list is the point.** Two lists answering "is this a
///    declaration" is what GM-367 removed from three lookups; leaving a
///    fourth copy here with one kind's worth of difference would keep the
///    drift alive in the one place it writes to the graph rather than reads
///    from it.
///
/// **What it changed on real corpora: nothing.** Re-indexing go-gin,
/// rs-ripgrep and py-requests from scratch with and without this exclusion
/// produced identical edge sets - 14,205, 18,343 and 4,760 edges, zero
/// differing rows on each - and no edge on any of the three had landed on an
/// `external_module` node before it either, nor on one in any of the 366
/// indexes on the machine this was measured on, which hold 68,079 import
/// records between them and carry only the `IMPORTS` edges that belong on
/// them.
///
/// The gap was latent, then, and what kept it latent is *not* this filter:
/// every shipped plugin emits its import records `file`-visible and with no
/// container, and a `file`-visible node is never a candidate in a file scope
/// (module doc, "Visibility"), so the check one step later was refusing them.
/// All 68,079 are `file`-visible and containerless, so the convention is
/// real - but it is a convention, not something core states or enforces: the
/// wire lets a plugin send a `Module` node with any `nativeKind` and any
/// visibility it likes. The failure it is one edit away from is concrete
/// rather than imagined: `file` visibility *is* accepted in a container
/// scope for the requester's own file, so a plugin that gave its import
/// records the container they sit in - the obvious thing to do to make them
/// show up as members of a Go package or a Rust module - would hand every
/// same-file container-scoped placeholder a rival candidate spelled exactly
/// like what it was looking for. The kind filter is where that line belongs,
/// because the kind is what core knows about the node.
///
/// The tests below pin it at both ends:
/// `no_kind_the_lookups_refuse_can_be_linked_onto` for the refusal,
/// `an_import_record_no_longer_makes_a_name_ambiguous` for the edge that
/// lands instead.
fn is_declaration(native_kind: Option<&str>) -> bool {
    !native_kind.is_some_and(|kind| NON_DECLARATION_NATIVE_KINDS.contains(&kind))
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
    // `graph::containers`. The count below is compared against the
    // `containers.memberCount` that module maintains, so the two have to
    // exclude the same kinds - `graph::containers::membership` is the other
    // half of this filter, and it still spells its own four-kind list (it
    // does not exclude `external_module`, which GM-372 excluded here). The
    // two agree on everything any plugin actually sends, because an import
    // record carries no container at all and so is filtered out one line
    // above by the `is_empty` check, whichever list is consulted.
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
        // Unaliased: `CANDIDATE` selects from `nodes` directly.
        let declarations = declaration_only("");
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
mod tests;
