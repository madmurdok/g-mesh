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
//! `imported_symbol` in plugins/typescript/src/extractor/imports.rs). This module is the
//! other half: it looks for the symbol the placeholder is waiting on among the
//! nodes actually in the index and, when exactly one fits, repoints the edge
//! and marks it `resolved`. A linked edge keeps the placeholder it came from
//! in `edges.linkedFrom`, so a woken placeholder is decided again: its edge
//! moves to a new answer or goes back onto the placeholder (GM-491).
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
//!     One exception, under a `name` key only: when several fit and exactly
//!     one of them is not a type member (its qualifiedPath's parent is a
//!     `Type` of its container), that one is linked. A name in a module scope
//!     never denotes a field or method, which plugins store in the module's
//!     container beside the free declarations ([`Resolver::sole_non_member`]).
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
//! does in the language. Within one scope a named re-export shadows its `*`
//! ones only where the scope's language says so ([`LinkRules`], declared by
//! its plugin as `[plugin.reexports] named_shadows_glob`): an explicit
//! `use`/`export { x }` beats a glob in Rust and ES modules, even when the
//! named one leads nowhere - a Rust `use std::io::Error;` beside
//! `use self::x::*;` is a row to the external crate, so the walk stops there
//! rather than linking `x::Error`. A language that declares `[plugin.reexports]
//! later_import_binds` instead (Python, where each import statement rebinds
//! the name) orders one scope's rows by statement - the row node's start
//! position, compared within one file only - and the latest row that binds
//! the name wins: a named row always binds (even when it leads nowhere), a
//! `*` row binds only when a sub-walk from it finds the name, so a later star
//! import that does not provide the name never hides an earlier binding.
//! Rows from different files, or two at one position, keep the old answer:
//! side by side at one depth, with no winner, which is also what a language
//! declaring neither rule gets. Decision:
//! `docs/architecture/gm-496-python-later-import-binds.md`. Only `name` keys
//! walk; a `qualifiedName` names a declaration, never a pass-through - except
//! through its head, below.
//!
//! ## Members of a re-exported head
//!
//! `use crate::named::T; T::m()` is addressed by `qualifiedName`
//! (`named::T::m`) at `named`, which declares no `T`: it re-exports one. A
//! `qualifiedName` placeholder that finds nothing in its scope and carries a
//! `keyPath` of at least two segments is split on those segments, never on
//! its string: the **head** is every segment but the last, the **member** is
//! the last one with its separator. The head's last name is walked by name
//! through the scope's re-exports, exactly as a `name` key is (renames,
//! globs, chains, the visited set, the depth cap, visibility against the
//! original requester). Only a head found at depth 1 or deeper counts - a
//! scope that declares the head itself simply lacks the member - and only
//! exactly one visible head: two are refused, even when only one has the
//! member. The member is then looked up once, by the exact `qualifiedName`
//! `head.qualifiedName + sep + name` in the head's own scope (its container,
//! or its file when it has none), and goes through the ordinary kind filter
//! and exactly-one rule. A key with no `keyPath` is not split. Design:
//! docs/architecture/gm-472-reexport-links.md.
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
//! visibility is checked only when the row says `container(..)`: such a hop
//! is followed only by a requester in that container or below it, as the
//! declaration rule below has it - the shape of a Rust private `use`, which a
//! child module's `use super::*` reaches and a sibling's glob does not. `file`
//! and `public` rows are walkable by anyone: the TS plugin sends every
//! re-export as `exported: false`, so checking those would unlink every
//! barrel, and a Rust `pub(crate) use` restricting an otherwise public item is
//! not modelled. The declaration at the end of the chain is still checked
//! against the original requester. A re-export with no target row is skipped,
//! never guessed at. Design: docs/architecture/gm-479-use-super-private-imports.md.
//!
//! ## What stays unresolved
//!
//! Anything ambiguous or unconfirmed, on the project's standing rule that a
//! missing edge beats a wrong one (`lookupByName` in extract.ts):
//!
//!  - the scope is not in the index, or offers no visible such name - directly
//!    or through any re-export chain short enough to follow;
//!  - several visible nodes fit and neither the edge kind nor the type-member
//!    exception (contract step 4) singles one out;
//!  - the only fits are of the wrong kind for the edge;
//!  - the placeholder has no target row at all - a legacy v1 address
//!    `derive_legacy_target` could not read. It is left exactly as it is and
//!    reported once per pass, never guessed from its `qualifiedName`;
//!  - the imported name is `default` while the target exports its default
//!    under a declared name (`export default class Foo {}` is a node called
//!    `Foo`), which only a semantic layer can tie together.
//!
//! "Unresolved" is not always the last word on these. The TypeScript plugin's
//! semantic pass re-asks the language server (vtsls) about the ones whose
//! target file does not declare the name itself, and re-sends the edge with
//! `source: semantic`, engine `vtsls`, when it gets a single answer.
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
//!  - pending edge kinds, the repoint and the unlink: `idx_edges_toId`, and
//!    `idx_edges_linkedFrom` for the edges a placeholder already linked;
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
use crate::protocol::types::QualifiedPath;
use crate::storage::qualified_path;
use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a pending cross-file symbol with. Mirrors
/// `PENDING_SYMBOL_NATIVE_KIND` in plugins/typescript/src/extractor/keys.rs - the
/// two are one wire contract and must be changed together.
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";

/// The `nativeKind` a plugin marks a re-export with: "this scope publishes
/// `name`, which really is its target". Mirrors `REEXPORT_NATIVE_KIND` in
/// plugins/typescript/src/extractor/keys.rs - the two are one wire contract and must be
/// changed together.
pub const REEXPORT_NATIVE_KIND: &str = "reexport";

/// The name a whole-module re-export (`export * from "./y"`) is recorded
/// under, as both its published name and its target key - it republishes
/// every name the target exports rather than one nameable one. Mirrors
/// `REEXPORT_ALL_NAME` in plugins/typescript/src/extractor/keys.rs.
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
const TYPE_KIND: &str = "Type";

/// `placeholder_targets.scopeKind` / `keyKind` values and `nodes.visibility`
/// values, as `storage::schema`'s CHECK constraints spell them.
const SCOPE_FILE: &str = "file";
const SCOPE_CONTAINER: &str = "container";
const KEY_NAME: &str = "name";
const KEY_QUALIFIED_NAME: &str = "qualifiedName";
const VISIBILITY_PUBLIC: &str = "public";
const VISIBILITY_FILE: &str = "file";
const VISIBILITY_CONTAINER: &str = "container";

/// The edge kinds a pending-symbol placeholder can carry, and the node kinds
/// each one accepts for the symbol it is linked to, in order of preference.
/// `CALLS` is Function -> Function by definition. `SUPERTYPE_OF` relates two
/// types, and also a trait-impl method to the trait method it implements
/// (`<Square as Shape>::area -> Shape::area`), so it accepts a `Function` -
/// but only where no `Type` fits, so a placeholder that can link to a type
/// still links to that type. `REFERENCES` is
/// the catch-all usage edge and accepts whatever the scope offers.
const LINKABLE_EDGE_KINDS: [(&str, Option<&[&str]>); 3] =
    [("CALLS", Some(&["Function"])), ("SUPERTYPE_OF", Some(&["Type", "Function"])), ("REFERENCES", None)];

fn required_target_kind(edge_kind: &str) -> Option<Option<&'static [&'static str]>> {
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// The segments of a `qualifiedName` key, when the plugin sent them.
    key_path: Option<QualifiedPath>,
    requester: Requester,
}

/// Every placeholder read in this module is read through this projection, so
/// the one row shape [`placeholder_from_row`] understands is the only one.
/// `LEFT JOIN`, so a placeholder with no target row still comes back - and is
/// counted rather than silently invisible.
const PLACEHOLDER_SELECT: &str = "SELECT n.id, n.language, t.scopeKind, t.scope, t.keyKind, t.key, \
     t.fromContainer, t.fromFile, t.keyPath FROM nodes n LEFT JOIN placeholder_targets t ON t.nodeId = n.id";

/// The name a `qualifiedName` key's head is walked under: the second-to-last
/// segment of its `keyPath`. `None` without a path of at least two segments.
fn head_name(key_path: &QualifiedPath) -> Option<&str> {
    let segments = key_path.segments();
    (segments.len() >= 2).then(|| segments[segments.len() - 2].name.as_str())
}

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
    // An undecodable path is no path: the key is then looked up whole.
    let key_path = row.get::<_, Option<String>>(8)?.as_deref().and_then(qualified_path::decode);
    let (Some(scope), Some(key)) =
        (Scope::from_row(&scope_kind, scope, &language), Key::from_row(&key_kind, key))
    else {
        return Ok(None);
    };
    Ok(Some(Placeholder {
        id,
        scope,
        key,
        key_path,
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

/// Per-language linking rules, declared by each language's plugin manifest
/// and handed in by whoever owns the index (`storage::index_store`). Core
/// holds no language's rule itself: a language absent here gets the base
/// behaviour, which is no shadowing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkRules {
    /// Languages whose plugin declares `[plugin.reexports]
    /// named_shadows_glob = true`: in one scope, a named re-export of a name
    /// hides every `*` re-export for it.
    named_shadows_glob: HashSet<String>,
    /// Languages whose plugin declares `[plugin.reexports]
    /// later_import_binds = true`: in one scope, the latest row that binds a
    /// name wins (a named row, or a `*` row that provides the name).
    later_import_binds: HashSet<String>,
}

impl LinkRules {
    /// Rules under which exactly `languages` have a named re-export shadow
    /// the same scope's globs.
    pub fn with_named_shadows_glob<I, S>(languages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self { named_shadows_glob: languages.into_iter().map(Into::into).collect(), ..Self::default() }
    }

    /// These rules, plus exactly `languages` binding a name by the later of
    /// the scope's import statements.
    pub fn with_later_import_binds<I, S>(mut self, languages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.later_import_binds = languages.into_iter().map(Into::into).collect();
        self
    }

    /// Whether, in a scope of `language`, the later of the scope's re-export
    /// rows that binds a name wins over the earlier ones.
    pub fn later_import_binds(&self, language: &str) -> bool {
        self.later_import_binds.contains(language)
    }

    /// Whether, in a scope of `language`, a named re-export shadows the
    /// scope's `*` re-exports for the same name.
    pub fn named_shadows_glob(&self, language: &str) -> bool {
        self.named_shadows_glob.contains(language)
    }
}

/// Links every pending symbol in the index. The whole-project pass, run once
/// a bulk index has committed its last batch - at which point every symbol
/// the walk will ever produce is in, so a placeholder that finds no target
/// here has none to find.
pub fn link_all(conn: &mut Connection, rules: &LinkRules) -> Result<LinkSummary> {
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
    link(conn, pending, rules)
}

/// Links what one just-applied diff could have changed, without rescanning
/// the whole index. Seven things can newly make a placeholder linkable:
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
///     [`requesters_below_new_containers`];
///  7. a head or a member appearing for a `qualifiedName` placeholder that
///     reaches the member through a re-export of the head (the module doc's
///     "Members of a re-exported head"). A head is a declaration or
///     re-export like the second, third and fifth triggers, so its
///     republished addresses also wake the `qualifiedName` placeholders in
///     those scopes whose `keyPath` head is that name ([`waiting_on_a_head`]);
///     a member adds its head's addresses to the same walk
///     ([`heads_of_members`]).
///
/// The second, third, fifth and seventh are not looked up under their own address
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
/// A placeholder that is woken is decided again even if its edges were
/// already linked: a linked edge keeps the placeholder it came from in
/// `edges.linkedFrom`, so a change that makes its answer different (a second
/// candidate appearing, a shallower declaration shadowing the linked one, a
/// later provider through a star import) moves it to the new answer or
/// unlinks it back onto its placeholder, as [`link`] describes (GM-491).
///
/// Not covered, deliberately, and for the same reason `graph::imports` does
/// not cover it: anything that seeds no trigger, so wakes no placeholder.
/// Two known cases: a symbol *deleted* from the project (edges into a deleted
/// node go when something deletes it, and the importers' edges come back as
/// fresh placeholders the next time those importers are reindexed), and a
/// visibility narrowing, which publishes no scope. Those are the only sense
/// in which this and [`link_all`] can disagree: whenever every change in a
/// sequence of diffs seeds a trigger, the two produce the same edges
/// (asserted by `link_all_and_link_diff_agree_on_the_same_end_state`).
/// [`link_all`] re-decides every placeholder, linked or not, so its next run
/// heals both.
pub fn link_diff(conn: &mut Connection, diff: &Diff, rules: &LinkRules) -> Result<LinkSummary> {
    let mut ids: BTreeSet<String> = diff
        .upsert_nodes
        .iter()
        .filter(|node| is_placeholder(&node.kind, node.native_kind.as_deref()))
        .map(|node| node.id.clone())
        .collect();

    let (mut name_seeds, exact_seeds) = seeds(diff);
    name_seeds.extend(heads_of_members(conn, diff)?);
    let mut addresses = republished_addresses(conn, name_seeds)?;
    waiting_on_a_head(conn, &addresses, &mut ids)?;
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
    link(conn, pending, rules)
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
        for scope in
            published_scopes(&node.visibility, &node.file_path, &node.language, node.container.as_deref())
        {
            if node.qualified_name != node.name {
                exact.push((scope.clone(), node.qualified_name.clone()));
            }
            named.push((scope, node.name.clone()));
        }
    }
    (named, exact)
}

/// The scopes a declaration with `visibility` is a seed under: those a
/// placeholder could find it in *and* see it from (see [`seeds`]).
fn published_scopes(visibility: &str, file: &str, language: &str, container: Option<&str>) -> Vec<Scope> {
    match visibility {
        VISIBILITY_PUBLIC | VISIBILITY_CONTAINER => Scope::of_node(file, language, container),
        VISIBILITY_FILE => Scope::container_of(language, container).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// For every declaration in the diff with a `qualifiedPath` of at least two
/// segments - a member - the `(scope, name)` seeds of its head, so that
/// [`waiting_on_a_head`] wakes the placeholders that reach this member
/// through a re-export of the head. The head is the declaration whose
/// `qualifiedName` is the member's path without its last segment, looked up
/// exactly beside the member (its container, or its file when it has none).
/// A member is a declaration like any other, so its own address is already a
/// seed in [`seeds`]; what is added here is the head's.
fn heads_of_members(conn: &Connection, diff: &Diff) -> Result<Vec<Address>> {
    const HEAD: &str = "SELECT filePath, language, container, name, visibility FROM nodes";
    let declarations = declaration_only("");
    let mut in_container = conn
        .prepare(&format!(
            "{HEAD} WHERE +language = ?1 AND +container = ?2 AND qualifiedName = ?3 AND {declarations}"
        ))
        .context("failed to prepare the member-head lookup")?;
    let mut in_file = conn
        .prepare(&format!("{HEAD} WHERE filePath = ?1 AND qualifiedName = ?2 AND {declarations}"))
        .context("failed to prepare the member-head lookup")?;

    let mut asked: HashSet<(Scope, String)> = HashSet::new();
    let mut seeds = Vec::new();
    for node in &diff.upsert_nodes {
        if !is_declaration(node.native_kind.as_deref()) {
            continue;
        }
        let Some(head) = node.qualified_path.as_ref().and_then(QualifiedPath::head) else {
            continue;
        };
        let scope = Scope::container_of(&node.language, node.container.as_deref())
            .unwrap_or_else(|| Scope::File(node.file_path.clone()));
        let head = head.display();
        if !asked.insert((scope.clone(), head.clone())) {
            continue;
        }
        let map = |row: &Row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        };
        let rows = match &scope {
            Scope::Container { language, key } => in_container.query_map(params![language, key, head], map),
            Scope::File(path) => in_file.query_map(params![path, head], map),
        }
        .context("failed to look up a member's head")?;
        for row in rows {
            let (file, language, container, name, visibility) =
                row.context("failed to read a member's head")?;
            for scope in published_scopes(&visibility, &file, &language, container.as_deref()) {
                seeds.push((scope, name.clone()));
            }
        }
    }
    Ok(seeds)
}

/// Adds to `ids` every `qualifiedName` placeholder scoped at one of
/// `addresses`' scopes whose `keyPath` head ([`head_name`]) is that address's
/// name: the placeholders [`Resolver::through_head`] may now answer. Read by
/// scope (`idx_targets_scope`'s leading columns) and filtered on the decoded
/// path in memory; a `*` address is left to [`waiting_placeholders`], which
/// already takes its whole scope.
fn waiting_on_a_head(conn: &Connection, addresses: &[Address], ids: &mut BTreeSet<String>) -> Result<()> {
    let mut by_scope: HashMap<&Scope, HashSet<&str>> = HashMap::new();
    for (scope, name) in addresses {
        if name != REEXPORT_ALL_NAME {
            by_scope.entry(scope).or_default().insert(name.as_str());
        }
    }
    if by_scope.is_empty() {
        return Ok(());
    }
    let mut in_scope = conn
        .prepare(&format!(
            "SELECT t.nodeId, t.keyPath FROM placeholder_targets t JOIN nodes n ON n.id = t.nodeId \
             WHERE t.scopeKind = ?1 AND t.scope = ?2 AND t.keyKind = '{KEY_QUALIFIED_NAME}' \
               AND t.keyPath IS NOT NULL \
               AND n.kind = ?3 AND n.nativeKind = ?4 AND (?5 IS NULL OR n.language = ?5)"
        ))
        .context("failed to prepare the waiting-member lookup")?;
    for (scope, names) in by_scope {
        let (scope_kind, scope, language) = scope.bind();
        let rows = in_scope
            .query_map(params![scope_kind, scope, MODULE_KIND, PENDING_SYMBOL_NATIVE_KIND, language], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("failed to look up placeholders waiting on a head")?;
        for row in rows {
            let (id, key_path) = row.context("failed to read a placeholder waiting on a head")?;
            let waits = qualified_path::decode(&key_path)
                .as_ref()
                .and_then(head_name)
                .is_some_and(|head| names.contains(head));
            if waits {
                ids.insert(id);
            }
        }
    }
    Ok(())
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
/// Every placeholder it is handed is decided again, including one whose edges
/// it already linked (GM-491). A linked edge keeps the placeholder it came
/// from in `edges.linkedFrom`, so when a new provider wakes that placeholder
/// the edge ends where [`link_all`] would leave it: moved to the new single
/// answer, or - nothing found, nothing of the right kind, several equally good
/// candidates - unlinked back onto its placeholder (`resolved = 0`,
/// `linkedFrom` cleared). Edges still on the placeholder are left alone in
/// that case. Without the provenance a repoint overwrote the only link back,
/// and a woken placeholder was skipped as "already linked".
///
/// Idempotent, which is what makes it safe to run after every write: the
/// repoint skips an edge already on the chosen target (`toId != ?target`), so
/// an unchanged answer writes and counts nothing, and a reindex that resets an
/// edge (a restarted plugin re-sends its full extraction, `resolved: false`
/// and all, which also clears `linkedFrom`) is simply linked again.
/// `linked_edges` counts edges moved onto a target, including moves from one
/// target to another; unlinks are not counted.
fn link(conn: &mut Connection, pending: Pending, rules: &LinkRules) -> Result<LinkSummary> {
    let Pending { placeholders, untargeted } = pending;
    let mut summary = LinkSummary::default();
    if placeholders.is_empty() {
        report_untargeted(untargeted, 0);
        return Ok(summary);
    }

    let tx = conn.transaction().context("failed to start the symbol-linking transaction")?;
    let untargeted_reexports;
    {
        // The kinds still on the placeholder, and the kinds it already linked.
        let mut pending_kinds = tx
            .prepare(
                "SELECT kind FROM edges WHERE toId = ?1
                 UNION SELECT kind FROM edges WHERE linkedFrom = ?1",
            )
            .context("failed to prepare the pending-edge-kind scan")?;
        let mut repoint = tx
            .prepare(
                "UPDATE edges SET toId = ?1, resolved = 1, linkedFrom = ?2
                 WHERE (toId = ?2 OR linkedFrom = ?2) AND kind = ?3 AND toId != ?1",
            )
            .context("failed to prepare the edge repoint")?;
        let mut unlink = tx
            .prepare("UPDATE edges SET toId = ?1, resolved = 0, linkedFrom = NULL WHERE linkedFrom = ?1 AND kind = ?2")
            .context("failed to prepare the edge unlink")?;
        let mut resolver = Resolver::new(&tx, rules)?;

        for placeholder in placeholders {
            let edge_kinds: Vec<String> = pending_kinds
                .query_map(params![placeholder.id], |row| row.get(0))
                .context("failed to read a placeholder's edge kinds")?
                .collect::<rusqlite::Result<_>>()
                .context("failed to collect a placeholder's edge kinds")?;
            if edge_kinds.is_empty() {
                continue; // no usages, on the placeholder or linked from it
            }

            // Empty when nothing is visible under that key, here or anywhere
            // the scope forwards to: the scope is not in the index
            // (gitignored, excluded, another language), does not offer it to
            // this requester, or ends its chain somewhere that does not.
            // Leaving the usage on its placeholder *is* the graceful fallback.
            let candidates = resolver.resolve(&placeholder)?;

            for edge_kind in edge_kinds {
                let Some(required) = required_target_kind(&edge_kind) else {
                    continue; // not a usage edge - nothing here linked it, so nothing here moves it
                };
                // The first accepted kind any candidate has decides; the
                // others are not considered beside it.
                let fitting: Vec<&Candidate> = match required {
                    Some(kinds) => kinds
                        .iter()
                        .map(|kind| {
                            candidates.iter().filter(|candidate| candidate.kind == *kind).collect::<Vec<_>>()
                        })
                        .find(|fitting| !fitting.is_empty())
                        .unwrap_or_default(),
                    None => candidates.iter().collect(),
                };
                let target_id = match fitting.as_slice() {
                    [candidate] => Some(candidate.id.clone()),
                    // Nothing (of the right kind): a missing edge beats a wrong one.
                    [] => None,
                    // Several equally good candidates: a missing edge beats a
                    // wrong one.
                    several => resolver
                        .sole_non_member(&placeholder.key, several)?
                        .map(|candidate| candidate.id.clone()),
                };

                match target_id {
                    Some(target_id) => {
                        summary.linked_edges += repoint
                            .execute(params![target_id, placeholder.id, edge_kind])
                            .context("failed to repoint a usage edge")?;
                    }
                    None => {
                        unlink
                            .execute(params![placeholder.id, edge_kind])
                            .context("failed to unlink a usage edge")?;
                    }
                }
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
    crate::log_line!(
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
    qualified_name: String,
    container: Option<String>,
    /// `nodes.qualifiedPath`, still encoded: only an ambiguity decodes it.
    qualified_path: Option<String>,
}

/// A scope and key the walk looks a declaration up at.
type Step = (Scope, Key);

/// One re-export hop: where a re-export forwards to, and who may follow it.
#[derive(Debug, Clone)]
struct Hop {
    to: Step,
    /// Whether the row forwards one name (`use a::T;`, `export { T }`) rather
    /// than a whole scope (`*`).
    named: bool,
    /// Whether the row's language makes a named row shadow every `*` row for
    /// that name in the same scope ([`LinkRules::named_shadows_glob`]) - an
    /// explicit import beats a glob in Rust, as an explicit export beats
    /// `export *` in ES modules, while in Python the later import binds.
    named_shadows_glob: bool,
    /// Whether the row's language binds a name by the later import statement
    /// ([`LinkRules::later_import_binds`]) - Python.
    later_import_binds: bool,
    /// The row node's `(filePath, startLine, startCol)`: its statement's
    /// order, comparable only between rows of one file.
    position: (String, i64, i64),
    /// `Some((language, container))` for a row of `container` visibility: only
    /// a requester of that language in that container or below it may follow
    /// it. `None` for every other row, which anyone may follow.
    restricted_to: Option<(String, Option<String>)>,
}

/// The lookups one linking pass needs, prepared once and memoized per
/// distinct question - see the module doc's "Cost" section. Everything cached
/// here is a fact about the index inside one transaction, which cannot change
/// under it; only [`Resolver::resolve`]'s walk depends on who is asking.
struct Resolver<'c> {
    conn: &'c Connection,
    rules: &'c LinkRules,
    in_file_by_name: Statement<'c>,
    in_file_by_qualified_name: Statement<'c>,
    in_container_by_name: Statement<'c>,
    in_container_by_qualified_name: Statement<'c>,
    reexports_in_file: Statement<'c>,
    reexports_in_container: Statement<'c>,
    type_in_file: Statement<'c>,
    type_in_container: Statement<'c>,
    suffixes_of: Statement<'c>,
    declared: HashMap<(Scope, Key), Vec<Candidate>>,
    hops: HashMap<(Scope, String), Vec<Hop>>,
    /// `(language, container)` -> that container plus its parent chain.
    visible_from: HashMap<(String, String), HashSet<String>>,
    /// Candidate id -> whether it is a type member ([`Resolver::is_type_member`]).
    type_members: HashMap<String, bool>,
    untargeted_reexports: HashSet<String>,
    /// `(step, cap, requester)` -> whether a walk from `step` of at most
    /// `cap` hops finds a visible declaration ([`Resolver::provides`]).
    provides: HashMap<(Step, usize, Requester), bool>,
}

impl<'c> Resolver<'c> {
    fn new(conn: &'c Connection, rules: &'c LinkRules) -> Result<Self> {
        const CANDIDATE: &str = "SELECT id, kind, filePath, language, visibility, visibilityContainer, \
             qualifiedName, container, qualifiedPath FROM nodes";
        const REEXPORT: &str = "SELECT n.id, n.name, n.language, t.scopeKind, t.scope, t.keyKind, t.key, \
             n.visibility, n.visibilityContainer, n.filePath, n.startLine, n.startCol \
             FROM nodes n LEFT JOIN placeholder_targets t ON t.nodeId = n.id";
        // Unaliased: `CANDIDATE` selects from `nodes` directly.
        let declarations = declaration_only("");
        let prepare = |sql: String| conn.prepare(&sql).context("failed to prepare a symbol-linking lookup");
        Ok(Resolver {
            conn,
            rules,
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
            // `+` keeps both on idx_nodes_qualifiedName, as above.
            type_in_file: prepare(format!(
                "SELECT 1 FROM nodes WHERE qualifiedName = ?1 AND +filePath = ?2 AND +kind = '{TYPE_KIND}' LIMIT 1"
            ))?,
            type_in_container: prepare(format!(
                "SELECT 1 FROM nodes WHERE qualifiedName = ?1 AND +language = ?2 AND +container = ?3 \
                 AND +kind = '{TYPE_KIND}' LIMIT 1"
            ))?,
            suffixes_of: prepare("SELECT suffix FROM qualified_suffixes WHERE nodeId = ?1".to_string())?,
            declared: HashMap::new(),
            hops: HashMap::new(),
            visible_from: HashMap::new(),
            type_members: HashMap::new(),
            untargeted_reexports: HashSet::new(),
            provides: HashMap::new(),
        })
    }

    /// The nodes `placeholder` may be linked to: every visible declaration
    /// matching its key at the shallowest level of its scope's re-export
    /// chains that has any, deduplicated - or, for a `qualifiedName` key that
    /// finds nothing, the member of its re-exported head
    /// ([`Resolver::through_head`]).
    ///
    /// Ambiguity is not resolved here. Several branches of a barrel can each
    /// answer, and they are all returned: the caller refuses to move an edge
    /// that more than one candidate fits, which is the same rule as for a name
    /// a single scope declares twice.
    fn resolve(&mut self, placeholder: &Placeholder) -> Result<Vec<Candidate>> {
        let (candidates, _) = self.walk(&placeholder.scope, &placeholder.key, &placeholder.requester)?;
        if !candidates.is_empty() {
            return Ok(candidates);
        }
        match (&placeholder.key, &placeholder.key_path) {
            (Key::QualifiedName(_), Some(key_path)) => self.through_head(placeholder, key_path),
            _ => Ok(Vec::new()),
        }
    }

    /// The visible declarations matching `key` at the shallowest level of
    /// `scope`'s re-export chains that has any, with that level's depth (0:
    /// `scope` itself).
    ///
    /// Breadth-first, so a name a scope both declares and re-exports resolves
    /// to the declaration - the language's own rule - except where the
    /// declaration's language binds a name by the later statement
    /// ([`LinkRules::later_import_binds`]): there a re-export written after
    /// the declaration in the same file, and providing the name, binds it
    /// instead. Bounded twice over: the
    /// visited set makes a re-export cycle terminate, [`MAX_REEXPORT_DEPTH`]
    /// bounds an acyclic chain, and both are needed since one does not imply
    /// the other.
    fn walk(&mut self, scope: &Scope, key: &Key, requester: &Requester) -> Result<(Vec<Candidate>, usize)> {
        self.walk_capped(scope, key, requester, MAX_REEXPORT_DEPTH)
    }

    /// [`Resolver::walk`], following at most `cap` re-export hops: the full
    /// walk spends [`MAX_REEXPORT_DEPTH`], a [`Resolver::provides`] probe what
    /// is left of it.
    fn walk_capped(
        &mut self,
        scope: &Scope,
        key: &Key,
        requester: &Requester,
        cap: usize,
    ) -> Result<(Vec<Candidate>, usize)> {
        let mut frontier: Vec<Step> = vec![(scope.clone(), key.clone())];
        let mut visited: HashSet<Step> = frontier.iter().cloned().collect();

        for depth in 0..=cap {
            let mut candidates: Vec<Candidate> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            // Binding hops already computed for a frontier step, by index, so
            // the expansion below does not compute them twice.
            let mut computed: Vec<Option<Vec<Hop>>> = vec![None; frontier.len()];
            for (index, (scope, key)) in frontier.iter().enumerate() {
                let mut found = Vec::new();
                for candidate in self.declared(scope, key)? {
                    if self.visible(&candidate, scope, requester)? {
                        found.push(candidate);
                    }
                }
                if depth < cap {
                    if let Key::Name(name) = key {
                        if found.iter().any(|c| self.rules.later_import_binds(&c.language)) {
                            let hops = self.binding_hops(scope, name, requester, cap - depth - 1)?;
                            let rebound = self.rebinds(&found, &hops)?;
                            computed[index] = Some(hops);
                            if rebound {
                                continue;
                            }
                        }
                    }
                }
                for candidate in found {
                    // The same node can be reached under its file *and* its
                    // container; it is still one candidate, not an ambiguity.
                    if seen.insert(candidate.id.clone()) {
                        candidates.push(candidate);
                    }
                }
            }
            if !candidates.is_empty() || depth == cap {
                return Ok((candidates, depth));
            }

            let mut next = Vec::new();
            for (index, (scope, key)) in frontier.iter().enumerate() {
                let Key::Name(name) = key else {
                    continue; // a qualifiedName names a declaration, never a pass-through
                };
                let hops = match computed[index].take() {
                    Some(hops) => hops,
                    // `depth < cap` here, so this never underflows.
                    None => self.binding_hops(scope, name, requester, cap - depth - 1)?,
                };
                for hop in hops {
                    if visited.insert(hop.to.clone()) {
                        next.push(hop.to);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        Ok((Vec::new(), 0))
    }

    /// The re-export hops a walk follows from `scope` for `name`, with `cap`
    /// hops left after them: the scope's rows this requester may follow, a
    /// named row shadowing the `*` rows where the language says so, and of
    /// rows all of a [`LinkRules::later_import_binds`] language only the one
    /// that binds the name ([`Resolver::later_binding`]).
    fn binding_hops(
        &mut self,
        scope: &Scope,
        name: &str,
        requester: &Requester,
        cap: usize,
    ) -> Result<Vec<Hop>> {
        let mut hops = self.hops(scope, name)?;
        // Where the language says so, a named row shadows the scope's `*`
        // rows, even when it leads nowhere (an external crate's item) and
        // even when this requester may not follow it: in rustc the shadowed
        // glob item is not in the scope at all, so a missing edge beats the
        // wrong one a glob would give. A language declaring neither rule
        // keeps both kinds at this depth, with no winner.
        if hops.iter().any(|hop| hop.named && hop.named_shadows_glob) {
            hops.retain(|hop| hop.named || !hop.named_shadows_glob);
        }
        let mut followed = Vec::new();
        for hop in hops {
            // Checked before the walk's `visited`: a row this requester may
            // not follow must not hide another row reaching the same step.
            // Also before the later-binding rule: a row nobody here may
            // follow neither wins nor hides.
            if let Some((language, container)) = &hop.restricted_to {
                if !self.sees(requester, language, container.as_deref())? {
                    continue;
                }
            }
            followed.push(hop);
        }
        if !followed.is_empty() && followed.iter().all(|hop| hop.later_import_binds) {
            followed = self.later_binding(followed, requester, cap)?;
        }
        Ok(followed)
    }

    /// Whether a step's binding `hops` rebind the name its visible
    /// `candidates` declare: exactly one hop (an ordered winner of
    /// [`Resolver::later_binding`]), written after the latest candidate of
    /// its own file. A candidate of another file has no statement order
    /// with the row, so with none in the row's file the declaration wins.
    fn rebinds(&mut self, candidates: &[Candidate], hops: &[Hop]) -> Result<bool> {
        let [hop] = hops else {
            return Ok(false);
        };
        let (file, line, col) = &hop.position;
        let mut latest: Option<(i64, i64)> = None;
        for candidate in candidates.iter().filter(|c| &c.file_path == file) {
            let position = self.position_of(&candidate.id)?;
            latest = latest.max(Some(position));
        }
        Ok(latest.is_some_and(|start| (*line, *col) > start))
    }

    /// Of one scope's followable rows for a name, all of a
    /// [`LinkRules::later_import_binds`] language, the one that binds it:
    /// the latest named row, or a later `*` row that provides the name
    /// ([`Resolver::provides`], within `cap` more hops). A `*` row that
    /// provides nothing is skipped, so it hides nothing. Rows of different
    /// files, or two at one position, have no statement order between them
    /// and are all kept, as for a language without the rule.
    fn later_binding(&mut self, mut hops: Vec<Hop>, requester: &Requester, cap: usize) -> Result<Vec<Hop>> {
        let file = &hops[0].position.0;
        let mut positions: HashSet<(i64, i64)> = HashSet::new();
        let ordered = hops
            .iter()
            .all(|hop| &hop.position.0 == file && positions.insert((hop.position.1, hop.position.2)));
        if !ordered {
            return Ok(hops);
        }
        // Latest statement first.
        hops.sort_by_key(|hop| std::cmp::Reverse((hop.position.1, hop.position.2)));
        for hop in hops {
            if hop.named || self.provides(&hop.to, requester, cap)? {
                return Ok(vec![hop]);
            }
        }
        Ok(Vec::new())
    }

    /// Whether a walk from `step`, following at most `cap` re-export hops,
    /// finds a declaration `requester` may see. Several count: the outer walk
    /// reports that ambiguity itself.
    fn provides(&mut self, step: &Step, requester: &Requester, cap: usize) -> Result<bool> {
        let cache_key = (step.clone(), cap, requester.clone());
        if let Some(known) = self.provides.get(&cache_key) {
            return Ok(*known);
        }
        let (candidates, _) = self.walk_capped(&step.0, &step.1, requester, cap)?;
        let found = !candidates.is_empty();
        self.provides.insert(cache_key, found);
        Ok(found)
    }

    /// A node's `(startLine, startCol)`: its statement's order within its
    /// file.
    fn position_of(&self, id: &str) -> Result<(i64, i64)> {
        let mut statement =
            self.conn.prepare_cached("SELECT startLine, startCol FROM nodes WHERE id = ?1")?;
        Ok(statement.query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))?)
    }

    /// A `qualifiedName` key's member, reached through its head: the head's
    /// last name walked by name from the placeholder's scope, then the member
    /// looked up once by exact `qualifiedName` beside the head. Empty unless
    /// exactly one visible head turns up at depth 1 or deeper - at depth 0 the
    /// scope declares the head itself, so the member is simply not there.
    fn through_head(
        &mut self,
        placeholder: &Placeholder,
        key_path: &QualifiedPath,
    ) -> Result<Vec<Candidate>> {
        let (Some(head), Some(member)) = (head_name(key_path), key_path.last()) else {
            return Ok(Vec::new());
        };
        let (heads, depth) =
            self.walk(&placeholder.scope, &Key::Name(head.to_string()), &placeholder.requester)?;
        let [head] = heads.as_slice() else {
            return Ok(Vec::new()); // none, or ambiguous: a missing edge beats a wrong one
        };
        if depth == 0 {
            return Ok(Vec::new());
        }

        let scope = Scope::container_of(&head.language, head.container.as_deref())
            .unwrap_or_else(|| Scope::File(head.file_path.clone()));
        let member_key = Key::QualifiedName(format!(
            "{}{}{}",
            head.qualified_name,
            member.sep.as_deref().unwrap_or(""),
            member.name
        ));
        let mut members = Vec::new();
        for candidate in self.declared(&scope, &member_key)? {
            if self.visible(&candidate, &scope, &placeholder.requester)? {
                members.push(candidate);
            }
        }
        Ok(members)
    }

    /// The one candidate of `several` that is not a type member, when a
    /// `name` key found exactly one such candidate among type members.
    ///
    /// A name looked up in a module scope denotes a module-level declaration,
    /// never an associated item: Rust's `use m::y` / `m::y()`, Go's `m.Y()`
    /// and Python's `from a import y` cannot reach a field or method of a type
    /// declared in `m`. Plugins store those members in the module's container
    /// beside the free declarations, so they share the name lookup, and are
    /// discarded here. Two or more non-members are still ambiguous, and so
    /// are members alone (a field and a getter of one name). A `qualifiedName`
    /// key names one declaration and gets no tie-break. Design:
    /// docs/architecture/gm-470-member-free-fn-collision.md.
    fn sole_non_member<'a>(&mut self, key: &Key, several: &[&'a Candidate]) -> Result<Option<&'a Candidate>> {
        if !matches!(key, Key::Name(_)) {
            return Ok(None);
        }
        let mut non_member = None;
        for candidate in several {
            if self.is_type_member(candidate)? {
                continue;
            }
            if non_member.replace(*candidate).is_some() {
                return Ok(None);
            }
        }
        Ok(non_member)
    }

    /// Whether `candidate` is a member of a type: its `qualifiedPath`, or one
    /// of its alias paths, minus the last segment is the qualifiedName of a
    /// `Type` declared in the same container (the same language and container
    /// key), or in the same file when it has no container. A candidate
    /// without a path of at least two segments is not one.
    ///
    /// The container, not the file, is what a Go method shares with its
    /// receiver type, which may be declared in another file of the package.
    /// An alias is what names a member whose own path's parent is no type:
    /// a Rust trait-impl method is `m::<S as Tr>::y`, with the alias
    /// `m::S::y`. Aliases are stored only as `qualified_suffixes` text, and
    /// an alias suffix that starts at segment 0 is the whole alias, so its
    /// parent is that text minus the candidate's own last separator and
    /// name - a concatenation undone, never a split.
    fn is_type_member(&mut self, candidate: &Candidate) -> Result<bool> {
        if let Some(known) = self.type_members.get(&candidate.id) {
            return Ok(*known);
        }
        let Some(path) = candidate.qualified_path.as_deref().and_then(qualified_path::decode) else {
            self.type_members.insert(candidate.id.clone(), false);
            return Ok(false);
        };
        let (Some(head), Some(last)) = (path.head(), path.last()) else {
            self.type_members.insert(candidate.id.clone(), false);
            return Ok(false);
        };
        let tail = format!("{}{}", last.sep.as_deref().unwrap_or(""), last.name);
        let own_suffixes: HashSet<String> = (1..path.len()).map(|start| path.suffix_from(start)).collect();
        let mut parents = vec![head.display()];
        let suffixes: Vec<String> = self
            .suffixes_of
            .query_map(params![candidate.id], |row| row.get(0))
            .context("failed to read a candidate's alias suffixes")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to collect a candidate's alias suffixes")?;
        for suffix in suffixes {
            if own_suffixes.contains(&suffix) {
                continue; // a suffix of the candidate's own path, not an alias
            }
            if let Some(parent) = suffix.strip_suffix(&tail).filter(|parent| !parent.is_empty()) {
                parents.push(parent.to_string());
            }
        }

        let mut member = false;
        for parent in parents {
            let found = match candidate.container.as_deref() {
                Some(container) => self
                    .type_in_container
                    .query_row(params![parent, candidate.language, container], |_| Ok(()))
                    .optional(),
                None => {
                    self.type_in_file.query_row(params![parent, candidate.file_path], |_| Ok(())).optional()
                }
            }
            .context("failed to look up a candidate's enclosing type")?;
            if found.is_some() {
                member = true;
                break;
            }
        }
        self.type_members.insert(candidate.id.clone(), member);
        Ok(member)
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
                qualified_name: row.get(6)?,
                container: row.get(7)?,
                qualified_path: row.get(8)?,
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

        type ReexportRow = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
            i64,
            i64,
        );
        let map = |row: &Row| -> rusqlite::Result<ReexportRow> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
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
        for (
            id,
            published,
            language,
            scope_kind,
            target_scope,
            key_kind,
            key,
            visibility,
            visible_in,
            file_path,
            start_line,
            start_col,
        ) in rows
        {
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
            let named_shadows_glob = self.rules.named_shadows_glob(&language);
            let later_import_binds = self.rules.later_import_binds(&language);
            let restricted_to = (visibility == VISIBILITY_CONTAINER).then_some((language, visible_in));
            hops.push(Hop {
                to: (target_scope, hop_key),
                named: !whole_module,
                named_shadows_glob,
                later_import_binds,
                position: (file_path, start_line, start_col),
                restricted_to,
            });
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
                let (language, visible_in) =
                    (candidate.language.clone(), candidate.visibility_container.clone());
                self.sees(requester, &language, visible_in.as_deref())
            }
            _ => Ok(false),
        }
    }

    /// Whether `requester` sees a row of `language` that is visible in
    /// `container` only: the requester's own container is `container` or has
    /// it on its parent chain. A row of another language is never seen.
    fn sees(&mut self, requester: &Requester, language: &str, container: Option<&str>) -> Result<bool> {
        // A container-private row with no container named is malformed;
        // refusing it is the missing-edge side.
        let (Some(visible_in), Some(from)) = (container, &requester.container) else {
            return Ok(false);
        };
        if language != requester.language {
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
}

#[cfg(test)]
mod tests;
