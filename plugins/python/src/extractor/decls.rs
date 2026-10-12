//! The declaration pass: every node this file contributes to the graph, and
//! everything an `import` statement says.
//!
//! # What is a node, and what it is called
//!
//! | Python | `kind` | `nativeKind` | name within its module |
//! |---|---|---|---|
//! | `def f` at module level | `Function` | `function` | `f` |
//! | `def inner` inside a `def` | `Function` | `function` | `outer.inner` |
//! | `def m` inside a `class` | `Function` | `method` | `C.m` |
//! | `class C` | `Type` | `class` | `C` |
//! | `class Inner` inside a `class` | `Type` | `class` | `Outer.Inner` |
//! | a module-level assignment | `Variable` | `variable` | `NAME` |
//! | the module or package itself | `Module` | `module` / `package` | its own dotted key |
//!
//! `qualifiedName` is the declaration's lexical path within its module
//! ([`keys`](super::keys), Decision 2), so a method of a nested class is
//! `Outer.Inner.m` - that module's doc has the whole argument.
//!
//! ## `async def` is not its own `nativeKind`
//!
//! An `async def` is a `function`/`method` like any other here, with `async`
//! recorded where it belongs: in the rendered signature. `nativeKind` is part
//! of a node's id, so making `async` part of it would mean that adding or
//! removing the keyword *deletes* the symbol and adds a stranger under a new
//! id - taking every inbound edge with it - on an edit that renamed nothing.
//! And it would buy nothing in exchange: one scope cannot hold both a `def f`
//! and an `async def f`, so `async` never has two declarations to tell apart.
//!
//! ## What is deliberately not a node
//!
//! - **Class-body assignments.** `class C: attr = 1` declares an attribute,
//!   and the design doc's "member-level privacy is not modelled" is the same
//!   boundary: nothing in the tool surface addresses a class attribute, and
//!   the linker only links top-level and type-member *names*. Module-level
//!   assignments are nodes because they are exactly the top-level names a
//!   `from mod import CONSTANT` addresses.
//! - **`for`/`with`/`except` targets at module level.** They bind names, and
//!   [`super::scope`] treats them as bindings, but the task's own word is
//!   "assignments" and a loop variable is not one a `from mod import i` would
//!   ever name.
//! - **`self.x = 1`.** An attribute assignment mutates an object; it binds no
//!   name in any scope this plugin models.
//!
//! # The self-announcement node
//!
//! `crate::project`'s Decision 1 records an obligation on this task, and this
//! is where it is met: a module is a container *and* a member of its package,
//! because `from pkg.sub.mod import f` addresses the first and `from pkg.sub
//! import mod` addresses the second. Python has no `mod child;` statement to
//! hang the membership on, so the extractor states it by construction -
//! [`Declarer::announce`] emits one extra node per file whose
//! `qualifiedName` is the module's own dotted key, whose `name` is its bare
//! name, and whose `container` is its **parent** package.
//!
//! Two files never emit it: a `.pyi` stub (`crate::project`, Decision 6 - two
//! files announcing one name would make `from pkg import mod` ambiguous and
//! core would rightly refuse both) and a module or package with no parent at
//! all (see [`ModuleCtx::announcement`], which argues why nothing is lost).
//!
//! # `import`: five shapes, five different things
//!
//! ```python
//! import a.b                  # a container import; binds `a`
//! import a.b as c             # the same import; binds `c` to container a.b
//! from a.b import C           # a `pending_symbol` placeholder in container a.b
//! from a.b import C as D      # the same placeholder; `D` is what this module calls it
//! from a.b import *           # a container import, and a `*` re-export
//! ```
//!
//! plus the relative forms (`from . import x`, `from ..pkg import y`), which
//! differ only in how the container is computed - [`ModuleCtx::relative`].
//!
//! **Every one of them emits an `IMPORTS` edge from the file onto the
//! container it reads from**, not only the star form. `get_dependencies` is
//! answered from `IMPORTS` edges alone, so listing only star imports would
//! make it answer "this file depends on nothing" for essentially every Python
//! file - the same correction `plugins/rust`'s GM-286 notes record for
//! `use`. The placeholder and the import are independent addresses onto the
//! same module, exactly as the TS plugin emits both a `resolved_module` for
//! the specifier and a `pending_symbol` for each imported name.
//!
//! An import whose dotted name this project does not contain becomes an
//! `external_module` node instead - see `crate::project`'s Decision 8 for how
//! that is decided, and why a relative import never needs deciding.
//!
//! ## A named import whose name is itself a submodule (GM-358)
//!
//! `from a.b import C` draws its `IMPORTS` edge onto `a.b`, the container
//! being read from - but when `C` is not a symbol declared in `a.b`, rather
//! a submodule (`from . import packages`, `from requests import compat`),
//! Python's own import semantics also load `a.b.C` as a side effect, the
//! same as writing `import a.b.C` directly. [`Declarer::imported_name`]
//! draws a second `IMPORTS` edge onto `a.b.C` for exactly this case,
//! guarded by [`ProjectContext::has_container`] so it never fires for an
//! ordinary symbol import. The `pending_symbol`/`REFERENCES` placeholder for
//! `C` itself is unaffected either way - that address was never the bug,
//! only `get_dependencies`'s answer was.
//!
//! # `__all__`
//!
//! `__all__ = ["Greeter", "greet"]` in `pkg/__init__.py` is how a package
//! republishes its submodules' declarations, and `from pkg import Greeter`
//! then has to reach a `class Greeter` declared in `pkg/mod.py`. Core's
//! re-export walk is exactly the mechanism for that, and it needs a `reexport`
//! node **in container `pkg`** carrying what is published and what it really
//! is. [`Declarer::reexport_dunder_all`] emits one per entry that this file
//! imported from somewhere - an entry naming a declaration of this same file
//! needs none, because it is already a member of the container being looked
//! in.
//!
//! Each such node carries the range of the **import statement that bound the
//! name**, not the `__all__` assignment's: core orders one module's
//! re-export rows by their start position, because in Python the later of a
//! named import and a star import binds the name
//! (`docs/architecture/gm-496-python-later-import-binds.md`). A repeated
//! import of the same binding moves that range to the later statement
//! ([`FileModel::import`](super::model::FileModel::import)).
//!
//! The later binding also decides between a declaration and an import of one
//! name ([`FileModel::module_binding`](super::model::FileModel::module_binding)).
//! An entry naming a declaration of this file gets no node only while the
//! declaration is the binding; when an unconditional named import written
//! after it displaced it, the entry gets the import's node like any other.
//! A displaced name gets that one node even when it is not listed in
//! `__all__`, or when there is no `__all__` at all: without it, `from pkg
//! import f` elsewhere would land on the displaced declaration through the
//! container's own members, while the node, placed after the declaration,
//! lets core's linker follow the import instead.
//!
//! It is applied to any module, not only to an `__init__`, because the shape
//! is the same wherever it is written; a package's `__init__` is simply the
//! case it exists for. What it deliberately does **not** do is read `__all__`
//! as visibility - see [`keys`](super::keys), Decision 3.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, Range, TargetKey};
use g_mesh_plugin_sdk::{NodeSpec, PlaceholderKind};
use tree_sitter::Node;

use crate::extractor::emit::{container_target, Definition, Emitter};
use crate::extractor::keys::{is_public, visibility, FileRole, ModuleCtx};
use crate::extractor::model::{ConstValue, DeclRef, FileModel, Import, ImportlibName, ModuleBinding};
use crate::extractor::scope::{dotted_path, FrameKind, Scopes};
use crate::extractor::syntax::{
    assignment_signature, definition_name, docstring, dotted_segments, has_decorator, inner_definition,
    plain_string, property_accessor, signature, string_literal, text, Accessor,
};
use crate::project::ProjectContext;

/// The name whose module-level assignment states what a module republishes.
const DUNDER_ALL: &str = "__all__";

/// The decorator that makes a `def` one stub of an overload set, matched on
/// its last dotted segment: `@overload`, `@typing.overload`,
/// `@typing_extensions.overload`.
const OVERLOAD: &str = "overload";

/// Walks a file's statements, emitting every declaration and every import.
pub(crate) struct Declarer<'a, 's> {
    pub(crate) project: &'a ProjectContext,
    pub(crate) module: &'a ModuleCtx,
    pub(crate) source: &'s str,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) model: &'a mut FileModel,
    pub(crate) scopes: Scopes,
    /// How many compound statements (`if`/`try`/`with`/`for`/...) enclose the
    /// statement being walked. A module-level import inside one is
    /// conditional and never displaces an earlier binding
    /// ([`FileModel::import`]).
    pub(crate) conditional: usize,
}

impl Declarer<'_, '_> {
    /// The module's own membership in its package - `crate::project`'s
    /// Decision 1, see this module's doc.
    ///
    /// It carries the module docstring as well as the `File` node does. That
    /// is not a slip: the two nodes answer different questions (`what is in
    /// this file` and `what is this module`), a reader who finds either
    /// should see the documentation, and the text is a fact about the module
    /// either way.
    pub(crate) fn announce(&mut self, root: Node) {
        let Some((container, parent, name)) = self.module.announcement() else { return };
        let native_kind = match self.module.role {
            FileRole::Package => "package",
            _ => "module",
        };
        let own = visibility();
        let mut spec = NodeSpec::new(
            NodeKind::Module,
            name,
            self.module.key.clone(),
            self.emitter.positions().file_range(),
        )
        .native_kind(native_kind)
        .visibility(own.clone())
        .in_container(container.clone(), parent);
        spec.qualified_path = dotted_path(&container).map(|path| path.child(".", spec.name.clone()));
        spec.doc_comment = docstring(root, self.source);
        self.emitter.declare(spec, is_public(&own));
    }

    /// Walks one statement list - the file's top level, a class body, or a
    /// function body - in the frame [`Declarer::scopes`] is currently in.
    pub(crate) fn collect(&mut self, list: Node) {
        let mut cursor = list.walk();
        for statement in list.named_children(&mut cursor) {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: Node) {
        match statement.kind() {
            "decorated_definition" => {
                let inner = inner_definition(statement);
                match inner.kind() {
                    "function_definition" => self.function(statement, inner),
                    "class_definition" => self.class(statement, inner),
                    _ => {}
                }
            }
            "function_definition" => self.function(statement, statement),
            "class_definition" => self.class(statement, statement),
            "expression_statement" => {
                if self.scopes.kind() != FrameKind::Module {
                    return;
                }
                if let Some(assignment) = statement.named_child(0) {
                    if assignment.kind() == "assignment" {
                        self.assignment(assignment);
                    }
                }
            }
            "import_statement" if self.scopes.kind() == FrameKind::Module => self.import_statement(statement),
            "import_from_statement" if self.scopes.kind() == FrameKind::Module => {
                self.import_from_statement(statement)
            }
            // A compound statement is not itself a declaration, and every
            // branch of one is indexed with no predicate evaluated - the same
            // rule `plugins/rust` applies to `cfg`. That is what makes
            // `if TYPE_CHECKING: from x import Y` and
            // `try: import fast / except ImportError: import slow` visible at
            // all, and it is why a conditional import is a *documented gap*
            // (both branches are indexed, neither is chosen) rather than a
            // silent omission.
            "if_statement"
            | "elif_clause"
            | "else_clause"
            | "try_statement"
            | "except_clause"
            | "except_group_clause"
            | "finally_clause"
            | "with_statement"
            | "with_clause"
            | "with_item"
            | "for_statement"
            | "while_statement"
            | "match_statement"
            | "case_clause"
            | "block" => {
                self.conditional += 1;
                self.collect(statement);
                self.conditional -= 1;
            }
            _ => {}
        }
    }

    /// `def f` / `async def f`, at any nesting depth.
    fn function(&mut self, outer: Node, inner: Node) {
        let accessor = self.accessor(outer, inner);
        let native_kind = match (self.scopes.kind(), accessor) {
            (FrameKind::Class, Some(accessor)) => accessor.native_kind(),
            (FrameKind::Class, None) => "method",
            _ => "function",
        };
        let Some(id) = self.declare(outer, inner, NodeKind::Function, native_kind) else { return };
        // Every `def` of this id, not only the one the node row came from:
        // an `@overload` set is one node whose declaration list is all of
        // them (see `super::emit`'s "Overload sets").
        self.emitter.definition(
            &id,
            Definition {
                range: self.emitter.positions().range(outer),
                signature: signature(outer, self.source),
                overload: has_decorator(outer, self.source, OVERLOAD),
            },
        );
        // A nested `def` or `class` is a declaration of this file too, so the
        // body is walked - but as a *function* frame, which is what makes
        // `super::scope`'s class-skipping rule apply to everything inside it.
        let Some(name) = definition_name(inner, self.source) else { return };
        let path = self.scopes.child_path(name);
        self.scopes.push(FrameKind::Function, path);
        if let Some(body) = inner.child_by_field_name("body") {
            self.collect(body);
        }
        self.scopes.pop();
    }

    /// `class C`, at any nesting depth.
    fn class(&mut self, outer: Node, inner: Node) {
        let Some(id) = self.declare(outer, inner, NodeKind::Type, "class") else { return };
        let _ = id;
        let Some(name) = definition_name(inner, self.source) else { return };
        let path = self.scopes.child_path(name);
        self.scopes.push(FrameKind::Class, path);
        if let Some(body) = inner.child_by_field_name("body") {
            self.collect(body);
        }
        self.scopes.pop();
    }

    /// Emits one definition node and records it in the model.
    ///
    /// `outer` is the `decorated_definition` when there is one, because the
    /// node's *range* and *signature* must include the decorators; `inner` is
    /// the `def`/`class` itself, which is where the name, the parameters and
    /// the body are.
    fn declare(&mut self, outer: Node, inner: Node, kind: NodeKind, native_kind: &str) -> Option<String> {
        let name = definition_name(inner, self.source)?.to_string();
        let qualified = self.scopes.child_path(&name);
        let own = visibility();
        let mut spec =
            NodeSpec::new(kind, name.clone(), qualified.clone(), self.emitter.positions().range(outer))
                .native_kind(native_kind)
                .visibility(own.clone())
                .in_container(self.module.key.clone(), self.module.parent.clone());
        spec.qualified_path = self.scopes.child_qualified_path(&name);
        spec.signature = signature(outer, self.source);
        spec.doc_comment = inner.child_by_field_name("body").and_then(|body| docstring(body, self.source));
        let id = self.emitter.declare(spec, is_public(&own));
        let decl = DeclRef { id: id.clone(), kind };
        // A setter/deleter shares the getter's name and qualified name; it
        // goes in the accessor table only, so name lookups keep finding the
        // getter alone (see `FileModel::declare_accessor`).
        match self.accessor(outer, inner) {
            Some(accessor) if kind == NodeKind::Function => {
                self.model.declare_accessor(&qualified, accessor, decl)
            }
            _ => {
                let scope = self.scopes.path().to_string();
                let start = self.emitter.positions().range(outer).start;
                self.model.declare(&scope, &name, &qualified, decl, start);
            }
        }
        Some(id)
    }

    /// The property accessor the `def` `inner` (decorated as `outer`)
    /// declares, when it is one: only in a class body.
    fn accessor(&self, outer: Node, inner: Node) -> Option<Accessor> {
        if self.scopes.kind() != FrameKind::Class {
            return None;
        }
        let name = definition_name(inner, self.source)?;
        property_accessor(outer, name, self.source)
    }

    /// A module-level assignment: one `Variable` per name it binds, plus, for
    /// `__all__`, the list of names this module republishes.
    fn assignment(&mut self, assignment: Node) {
        let Some(left) = assignment.child_by_field_name("left") else { return };
        let mut names = Vec::new();
        collect_assigned_names(left, self.source, &mut names);
        // What a dynamic import's argument may fold through (GM-510, D3):
        // only `NAME = "lit"` / `NAME: T = "lit"` and `NAME = OTHER`, a single
        // identifier target. An annotation with no value binds nothing.
        let right = assignment.child_by_field_name("right");
        let value = match right {
            Some(right) if left.kind() == "identifier" => match right.kind() {
                "string" => plain_string(right, self.source).map(|lit| ConstValue::Literal(lit.to_string())),
                "identifier" => Some(ConstValue::Name(text(right, self.source).to_string())),
                _ => None,
            },
            _ => None,
        };
        for name in names {
            if name == DUNDER_ALL {
                self.collect_dunder_all(assignment);
            }
            let own = visibility();
            let mut spec = NodeSpec::new(
                NodeKind::Variable,
                name,
                self.scopes.child_path(name),
                self.emitter.positions().range(assignment),
            )
            .native_kind("variable")
            .visibility(own.clone())
            .in_container(self.module.key.clone(), self.module.parent.clone());
            spec.qualified_path = self.scopes.child_qualified_path(name);
            spec.signature = Some(assignment_signature(name, assignment, self.source));
            let id = self.emitter.declare(spec, is_public(&own));
            if right.is_some() {
                self.model.note_constant(&id, value.clone());
            }
            let scope = self.scopes.path().to_string();
            let start = self.emitter.positions().range(assignment).start;
            self.model.declare(&scope, name, name, DeclRef { id, kind: NodeKind::Variable }, start);
        }
    }

    /// The names `__all__ = [...]` lists, when they are plain string
    /// literals.
    ///
    /// Anything else - `__all__ = mod.__all__ + ["x"]`, a list comprehension,
    /// a name computed at import time - is read as **no** names rather than
    /// as a guess. That is the "star-import name sets that depend on runtime"
    /// gap this plugin documents, and the cost of it is that a package
    /// building its `__all__` dynamically re-exports nothing here: a missing
    /// edge, never a wrong one.
    fn collect_dunder_all(&mut self, assignment: Node) {
        let Some(right) = assignment.child_by_field_name("right") else { return };
        if !matches!(right.kind(), "list" | "tuple") {
            return;
        }
        let mut cursor = right.walk();
        let names: Vec<String> = right
            .named_children(&mut cursor)
            .filter_map(|entry| string_literal(entry, self.source))
            .map(str::to_string)
            .collect();
        self.model.set_dunder_all(names, self.emitter.positions().range(assignment));
    }

    /// `import a.b`, `import a.b as c` - one leaf per name bound. The edges
    /// come from [`import_edges`], which `super::bodies` shares for a
    /// function-local import; what is left here is the bindings.
    fn import_statement(&mut self, item: Node) {
        let range = self.emitter.positions().range(item);
        for edge in import_edges(item, self.source, self.project, self.module) {
            self.import_edge(&edge.container, &edge.specifier, range, edge.relative);
        }
        let unconditional = self.conditional == 0;
        let mut cursor = item.walk();
        for leaf in item.named_children(&mut cursor) {
            let importlib = importlib_name(item, leaf, self.source);
            match leaf.kind() {
                // `import a.b` binds the *top* package, `a`, and makes `a.b`
                // reachable through it - which is why the local name and the
                // imported container are different here and nowhere else.
                "dotted_name" => {
                    let Some(segments) = dotted_segments(leaf, self.source) else { continue };
                    // The *bound* name is the top package, so whether the
                    // binding is one of ours is a question about `a`, not
                    // about the `a.b` the edge was drawn onto.
                    let top = segments[0].to_string();
                    let binding = if self.project.has_container(&top) {
                        Import::Module { container: top.clone() }
                    } else {
                        Import::External
                    };
                    self.model.import(&top, binding, range, unconditional);
                    self.model.note_importlib(&top, importlib, unconditional);
                }
                "aliased_import" => {
                    let Some(full) = imported_module(leaf, self.source) else { continue };
                    let external = !self.project.has_container(&full);
                    if let Some(alias) = leaf.child_by_field_name("alias") {
                        let binding = if external {
                            Import::External
                        } else {
                            Import::Module { container: full.clone() }
                        };
                        let local = text(alias, self.source);
                        self.model.import(local, binding, range, unconditional);
                        self.model.note_importlib(local, importlib, unconditional);
                    }
                }
                _ => {}
            }
        }
    }

    /// `from a.b import …`, absolute or relative, named or star. The edges -
    /// the container's, and a GM-358 submodule's - come from
    /// [`import_edges`]; what is left here is the bindings.
    fn import_from_statement(&mut self, item: Node) {
        let range = self.emitter.positions().range(item);
        // More dots than there are packages above this file is Python's own
        // `ImportError`: nothing is emitted, which is the honest answer to a
        // statement that cannot run.
        let Some(from) = from_module(item, self.source, self.module) else { return };
        for edge in import_edges(item, self.source, self.project, self.module) {
            self.import_edge(&edge.container, &edge.specifier, range, edge.relative);
        }
        let external = !(from.relative || self.project.has_container(&from.container));
        let container = from.container;

        let mut cursor = item.walk();
        let module_name = item.child_by_field_name("module_name");
        for leaf in item.named_children(&mut cursor) {
            if Some(leaf) == module_name {
                continue;
            }
            match leaf.kind() {
                // `from a.b import *`: the container import above, plus the
                // re-export shape - this module republishes whatever that one
                // exports, which is exactly `*` at both ends.
                "wildcard_import" => {
                    #[cfg(test)]
                    crate::census::note_glob(!external);
                    if !external {
                        self.model.star(range.start);
                        self.emitter.reexport(
                            "*",
                            container_target(&container, TargetKey::Name("*".to_string()), &self.module.key),
                            &self.module.key,
                            range,
                        );
                    }
                }
                "dotted_name" | "aliased_import" => {
                    let Some((name, local)) = from_leaf(leaf, self.source) else { continue };
                    let importlib = importlib_name(item, leaf, self.source);
                    self.imported_name(&container, name, local, range, external, importlib);
                }
                _ => {}
            }
        }
    }

    /// One name a `from … import …` binds: a `pending_symbol` placeholder at
    /// its real address, a `REFERENCES` edge from the file onto it, and the
    /// binding recorded for the body pass.
    ///
    /// The edge starts at the **file**, not at a symbol, because an import
    /// line belongs to the file rather than to anything in it - so it shows up
    /// in `find_references` as a whole-file row, which is the granularity an
    /// import statement genuinely has.
    ///
    /// `importlib` is what [`importlib_name`] made of this leaf, recorded
    /// beside the binding (GM-510, D1a).
    fn imported_name(
        &mut self,
        container: &str,
        name: &str,
        local: &str,
        range: Range,
        external: bool,
        importlib: Option<ImportlibName>,
    ) {
        let unconditional = self.conditional == 0;
        self.model.note_importlib(local, importlib, unconditional);
        if external {
            self.model.import(local, Import::External, range, unconditional);
            return;
        }
        let placeholder = self.emitter.placeholder(
            PlaceholderKind::PendingSymbol,
            name,
            container_target(container, TargetKey::Name(name.to_string()), &self.module.key),
            range,
        );
        let file = self.emitter.file_id().to_string();
        self.emitter.placeholder_edge(EdgeKind::References, &file, &placeholder);
        self.model.import(
            local,
            Import::Item { container: container.to_string(), name: name.to_string() },
            range,
            unconditional,
        );
    }

    /// [`emit_import_edge`] for this file.
    fn import_edge(&mut self, container: &str, specifier: &str, range: Range, relative: bool) -> bool {
        emit_import_edge(self.project, self.emitter, container, specifier, range, relative)
    }

    /// Marks every module-level name bound by something other than a plain
    /// assignment as never a constant (GM-510, D3 and M3), before the walk
    /// that records the assignments. See [`module_rebindings`].
    pub(crate) fn poison_rebindings(&mut self, root: Node) {
        let mut names = Vec::new();
        module_rebindings(root, self.source, true, &mut names);
        for name in names {
            self.model.poison_name(name);
        }
    }

    /// Emits a `reexport` node for every `__all__` entry this file imported
    /// from somewhere else, and for every name a later import displaced from
    /// one of this file's declarations - see this module's doc.
    ///
    /// Run after the whole file has been walked, because `__all__` may be
    /// written above the imports it names (and in a package's `__init__` it
    /// routinely is).
    pub(crate) fn reexport_dunder_all(&mut self) {
        let all = self.model.dunder_all().clone();
        let mut published: Vec<String> = if all.range.is_some() { all.names } else { Vec::new() };
        for name in self.model.displaced() {
            if !published.iter().any(|listed| listed == name) {
                published.push(name.to_string());
            }
        }
        for published in &published {
            // A name whose binding is this file's own declaration is already
            // a member of the container a lookup searches, so a re-export
            // node for it would be a second, weaker answer to a question
            // already answered. That holds with a `*` import after the
            // declaration too: the import a node would carry was either
            // written before the declaration, which wins over it, or is
            // conditional, which never displaces it; the star's own node is
            // what may rebind the name.
            if matches!(
                self.model.module_binding(published, None),
                Some(ModuleBinding::Decl(_) | ModuleBinding::DeclBeforeStar(_))
            ) {
                continue;
            }
            let Some(Import::Item { container, name }) = self.model.lookup_import(published) else {
                continue;
            };
            let (container, name) = (container.clone(), name.clone());
            // The import statement's range, not `__all__`'s: core orders one
            // module's re-export rows by position, and the statement that
            // bound the name is what Python's "the later import binds" reads.
            let Some(range) = self.model.import_range(published) else { continue };
            self.emitter.reexport(
                published,
                container_target(&container, TargetKey::Name(name), &self.module.key),
                &self.module.key,
                range,
            );
        }
    }
}

/// One `IMPORTS` edge an import statement draws, before it is emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportEdge {
    /// The dotted container key the edge goes onto.
    pub(crate) container: String,
    /// The import as written (`a.b`, `..pkg`, `..pkg.sub`) - GM-544.
    pub(crate) specifier: String,
    /// A relative import: always one of ours, so `has_container` is not
    /// consulted.
    pub(crate) relative: bool,
}

/// What a `from … import …` statement's module part names.
pub(crate) struct FromModule {
    /// The container key, a relative one already resolved.
    pub(crate) container: String,
    /// The module as written: the dotted name, or a relative one's dots and
    /// tail (`..pkg`), whitespace dropped.
    pub(crate) written: String,
    pub(crate) relative: bool,
}

/// Every `IMPORTS` edge the `import`/`from … import` statement `item` draws,
/// wherever it sits (GM-510, D7a). One place for the rules, shared by
/// [`Declarer`] at module level and `super::bodies` for a function-local or
/// class-body import, so the two cannot drift.
///
/// - `import a.b`, `import a.b as c`: one edge onto `a.b` per leaf.
/// - `from a.b import x`: an edge onto `a.b`, plus one onto `a.b.x` when that
///   is a real submodule (GM-358, below). Relative forms resolve against
///   this file's package; dots above the top-level package give nothing.
///
/// `from a.b import c` where `c` is itself a submodule of `a.b` rather than a
/// symbol declared inside it also loads that submodule as a side effect -
/// Python's own import semantics, the same thing `import a.b.c` would
/// address directly. What was missing (GM-358) is the `IMPORTS` edge
/// `get_dependencies` is answered from. `has_container` is exactly the
/// self-announcement registry `crate::project`'s Decision 8 builds for this:
/// a real submodule's dotted key is in it, an ordinary symbol's never is, so
/// this never fires for the common `from a.b import SomeFunction` case. An
/// external container is never asked: its submodules are not ours.
pub(crate) fn import_edges(
    item: Node,
    source: &str,
    project: &ProjectContext,
    module: &ModuleCtx,
) -> Vec<ImportEdge> {
    let mut edges = Vec::new();
    match item.kind() {
        "import_statement" => {
            let mut cursor = item.walk();
            for leaf in item.named_children(&mut cursor) {
                let Some(full) = imported_module(leaf, source) else { continue };
                edges.push(ImportEdge { container: full.clone(), specifier: full, relative: false });
            }
        }
        "import_from_statement" => {
            let Some(from) = from_module(item, source, module) else { return edges };
            let internal = from.relative || project.has_container(&from.container);
            edges.push(ImportEdge {
                container: from.container.clone(),
                specifier: from.written.clone(),
                relative: from.relative,
            });
            if !internal {
                return edges;
            }
            let module_name = item.child_by_field_name("module_name");
            let mut cursor = item.walk();
            for leaf in item.named_children(&mut cursor) {
                if Some(leaf) == module_name {
                    continue;
                }
                let Some((name, _)) = from_leaf(leaf, source) else { continue };
                let submodule = format!("{}.{name}", from.container);
                if project.has_container(&submodule) {
                    let written = &from.written;
                    let specifier = if written.ends_with('.') {
                        format!("{written}{name}")
                    } else {
                        format!("{written}.{name}")
                    };
                    edges.push(ImportEdge { container: submodule, specifier, relative: false });
                }
            }
        }
        _ => {}
    }
    edges
}

/// The `IMPORTS` edge from this file onto whatever a dotted name addressed,
/// and whether that turned out to be outside this project.
///
/// A **relative** import is never outside it: that is what the leading dots
/// mean, so there is nothing to decide and `ProjectContext::has_container` is
/// not consulted. An absolute one is decided by that query - see
/// `crate::project`'s Decision 8.
///
/// `specifier` is the import as written (`a.b`, `..pkg`), the edge's
/// `specifier` (GM-544). A relative import is matched by a resolution delta
/// through its stored target, never through this text.
///
/// A free function so that `super::bodies` (function-local and dynamic
/// imports, GM-510) emits exactly what [`Declarer`] does.
pub(crate) fn emit_import_edge(
    project: &ProjectContext,
    emitter: &mut Emitter,
    container: &str,
    specifier: &str,
    range: Range,
    relative: bool,
) -> bool {
    let internal = relative || project.has_container(container);
    let to = if internal {
        emitter.placeholder(
            PlaceholderKind::ResolvedModule,
            container.rsplit('.').next().unwrap_or(container),
            container_target(container, TargetKey::Name("*".to_string()), container),
            range,
        )
    } else {
        emitter.external_module(container, range)
    };
    let file = emitter.file_id().to_string();
    emitter.import_edge(&file, &to, specifier);
    !internal
}

/// The dotted module an `import` statement's leaf names: `a.b` for both
/// `import a.b` and `import a.b as c`.
fn imported_module(leaf: Node, source: &str) -> Option<String> {
    let name = match leaf.kind() {
        "dotted_name" => leaf,
        "aliased_import" => leaf.child_by_field_name("name")?,
        _ => return None,
    };
    Some(dotted_segments(name, source)?.join("."))
}

/// A `from … import` leaf's imported name and the local it binds: `(x, x)`
/// for `x`, `(x, y)` for `x as y`.
fn from_leaf<'s>(leaf: Node, source: &'s str) -> Option<(&'s str, &'s str)> {
    match leaf.kind() {
        "dotted_name" => {
            let name = *dotted_segments(leaf, source)?.last()?;
            Some((name, name))
        }
        "aliased_import" => {
            let name = *dotted_segments(leaf.child_by_field_name("name")?, source)?.last()?;
            let local = leaf.child_by_field_name("alias").map(|alias| text(alias, source)).unwrap_or(name);
            Some((name, local))
        }
        _ => None,
    }
}

/// The module a `from … import …` statement names, or `None` when it names
/// nothing this file can resolve (dots above the top-level package).
pub(crate) fn from_module(item: Node, source: &str, module: &ModuleCtx) -> Option<FromModule> {
    let module_name = item.child_by_field_name("module_name")?;
    if module_name.kind() == "relative_import" {
        let container = relative_container(module_name, source, module)?;
        let written = text(module_name, source).chars().filter(|ch| !ch.is_whitespace()).collect();
        Some(FromModule { container, written, relative: true })
    } else {
        let container = dotted_segments(module_name, source)?.join(".");
        Some(FromModule { written: container.clone(), container, relative: false })
    }
}

/// The container a `relative_import` names - see [`ModuleCtx::relative`].
///
/// The dot count is taken from the `import_prefix`'s own text rather than
/// from its child count, because the tokenizer splits a run of dots into `.`
/// and `...` tokens in ways that depend on how many there are, and counting
/// characters cannot get that wrong.
fn relative_container(module_name: Node, source: &str, module: &ModuleCtx) -> Option<String> {
    let mut cursor = module_name.walk();
    let children: Vec<Node> = module_name.children(&mut cursor).collect();
    let prefix = children.iter().find(|child| child.kind() == "import_prefix")?;
    let level = text(*prefix, source).chars().filter(|ch| *ch == '.').count();
    let tail = children
        .iter()
        .find(|child| child.kind() == "dotted_name")
        .and_then(|node| dotted_segments(*node, source))
        .unwrap_or_default();
    module.relative(level, &tail)
}

/// Which `importlib` spelling the import `statement`'s `leaf` binds, if any
/// (GM-510, D1). Shared by the module-level table ([`FileModel`]) and the
/// per-frame pre-scan (`super::scope`).
///
/// - `import importlib`, `import importlib.util`: the leaf binds `importlib`
///   itself, so [`ImportlibName::Module`].
/// - `import importlib as il`: `Module` only when the aliased name is
///   exactly `importlib` (`import importlib.util as u` binds `util`).
/// - `from importlib import import_module [as im]`:
///   [`ImportlibName::ImportModule`], for the absolute `importlib` only.
pub(crate) fn importlib_name(statement: Node, leaf: Node, source: &str) -> Option<ImportlibName> {
    match statement.kind() {
        "import_statement" => match leaf.kind() {
            "dotted_name" => (dotted_segments(leaf, source)?.first() == Some(&"importlib"))
                .then_some(ImportlibName::Module),
            "aliased_import" => {
                let name = leaf.child_by_field_name("name")?;
                (dotted_segments(name, source)? == ["importlib"]).then_some(ImportlibName::Module)
            }
            _ => None,
        },
        "import_from_statement" => {
            let module_name = statement.child_by_field_name("module_name")?;
            if leaf == module_name
                || module_name.kind() != "dotted_name"
                || dotted_segments(module_name, source)? != ["importlib"]
            {
                return None;
            }
            let (name, _) = from_leaf(leaf, source)?;
            (name == "import_module").then_some(ImportlibName::ImportModule)
        }
        _ => None,
    }
}

/// Every module-level name bound by anything but a plain assignment, and
/// every name a `global` statement anywhere claims (GM-510, D3 and M3).
///
/// `module_scope` is false below a `def`, `class` or `lambda`: their bindings
/// are their own, and only a `global` there reaches the module. A
/// comprehension's `for` target is its own scope too; a walrus inside one
/// binds the enclosing scope, so it counts. A plain assignment is not listed:
/// [`FileModel::note_constant`] sees every one of those itself, and a chained
/// `A = B = …` lists its inner targets here because [`Declarer::assignment`]
/// reads only the outer one.
fn module_rebindings<'s>(node: Node, source: &'s str, module_scope: bool, out: &mut Vec<&'s str>) {
    match node.kind() {
        "global_statement" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    out.push(text(child, source));
                }
            }
            return;
        }
        "function_definition" | "class_definition" => {
            if module_scope {
                if let Some(name) = node.child_by_field_name("name") {
                    out.push(text(name, source));
                }
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // Decorators, defaults and bases are evaluated in this scope;
                // the body is not.
                let nested = Some(child) == node.child_by_field_name("body");
                module_rebindings(child, source, module_scope && !nested, out);
            }
            return;
        }
        "lambda" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                module_rebindings(child, source, false, out);
            }
            return;
        }
        _ => {}
    }
    if module_scope {
        match node.kind() {
            "assignment" => {
                if let Some(right) = node.child_by_field_name("right") {
                    if right.kind() == "assignment" {
                        if let Some(left) = right.child_by_field_name("left") {
                            collect_assigned_names(left, source, out);
                        }
                    }
                }
            }
            "augmented_assignment" | "for_statement" => {
                if let Some(left) = node.child_by_field_name("left") {
                    collect_assigned_names(left, source, out);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    collect_assigned_names(name, source, out);
                }
            }
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    collect_assigned_names(alias, source, out);
                }
            }
            "delete_statement" | "case_pattern" => collect_assigned_names(node, source, out),
            "import_statement" | "import_from_statement" => {
                let module_name = node.child_by_field_name("module_name");
                let mut cursor = node.walk();
                for leaf in node.named_children(&mut cursor) {
                    if Some(leaf) == module_name {
                        continue;
                    }
                    match (node.kind(), leaf.kind()) {
                        ("import_statement", "dotted_name") => {
                            if let Some(first) = leaf.named_child(0) {
                                out.push(text(first, source));
                            }
                        }
                        (_, "aliased_import") => {
                            if let Some(alias) = leaf.child_by_field_name("alias") {
                                out.push(text(alias, source));
                            }
                        }
                        (_, "dotted_name") => {
                            if let Some((_, local)) = from_leaf(leaf, source) {
                                out.push(local);
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            _ => {}
        }
    }
    // A comprehension's `for_in_clause` is deliberately not an arm above:
    // its target is local to the comprehension. Walking on still reaches any
    // walrus inside it.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        module_rebindings(child, source, module_scope, out);
    }
}

/// Every name an assignment's left-hand side binds, in source order.
///
/// An attribute (`self.x = 1`) and a subscript (`table[k] = v`) bind no name,
/// for the reason [`super::scope`]'s own `bind_target` gives: they mutate an
/// object rather than introducing a name in any scope.
fn collect_assigned_names<'s>(target: Node, source: &'s str, out: &mut Vec<&'s str>) {
    match target.kind() {
        "identifier" => out.push(text(target, source)),
        "attribute" | "subscript" => {}
        _ => {
            let mut cursor = target.walk();
            for child in target.named_children(&mut cursor) {
                collect_assigned_names(child, source, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_python::LANGUAGE.into()).unwrap();
        parser.parse(source, None).unwrap()
    }

    fn assigned(source: &str) -> Vec<String> {
        let tree = parse(source);
        let mut stack = vec![tree.root_node()];
        let assignment = loop {
            let node = stack.pop().expect("an assignment");
            if node.kind() == "assignment" {
                break node;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        };
        let mut out = Vec::new();
        collect_assigned_names(assignment.child_by_field_name("left").unwrap(), source, &mut out);
        out.into_iter().map(str::to_string).collect()
    }

    #[test]
    fn a_tuple_assignment_declares_every_name_it_binds() {
        assert_eq!(assigned("FIRST, SECOND = 1, 2\n"), vec!["FIRST", "SECOND"]);
        assert_eq!(assigned("A, (B, C) = x\n"), vec!["A", "B", "C"]);
    }

    #[test]
    fn an_annotated_assignment_declares_its_one_name() {
        assert_eq!(assigned("MAX: int = 3\n"), vec!["MAX"]);
    }

    #[test]
    fn an_attribute_or_subscript_target_declares_nothing() {
        assert_eq!(assigned("self.x = 1\n"), Vec::<String>::new());
        assert_eq!(assigned("table[key] = value\n"), Vec::<String>::new());
    }
}
