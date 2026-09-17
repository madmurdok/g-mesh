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
//! It is applied to any module, not only to an `__init__`, because the shape
//! is the same wherever it is written; a package's `__init__` is simply the
//! case it exists for. What it deliberately does **not** do is read `__all__`
//! as visibility - see [`keys`](super::keys), Decision 3.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, Range, TargetKey};
use g_mesh_plugin_sdk::{NodeSpec, PlaceholderKind};
use tree_sitter::Node;

use crate::extractor::emit::{container_target, Emitter};
use crate::extractor::keys::{is_public, visibility, FileRole, ModuleCtx};
use crate::extractor::model::{DeclRef, FileModel, Import};
use crate::extractor::scope::{FrameKind, Scopes};
use crate::extractor::syntax::{
    assignment_signature, definition_name, docstring, dotted_segments, inner_definition, signature,
    string_literal, text,
};
use crate::project::ProjectContext;

/// The name whose module-level assignment states what a module republishes.
const DUNDER_ALL: &str = "__all__";

/// Walks a file's statements, emitting every declaration and every import.
pub(crate) struct Declarer<'a, 's> {
    pub(crate) project: &'a ProjectContext,
    pub(crate) module: &'a ModuleCtx,
    pub(crate) source: &'s str,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) model: &'a mut FileModel,
    pub(crate) scopes: Scopes,
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
        .in_container(container, parent);
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
            | "block" => self.collect(statement),
            _ => {}
        }
    }

    /// `def f` / `async def f`, at any nesting depth.
    fn function(&mut self, outer: Node, inner: Node) {
        let native_kind = match self.scopes.kind() {
            FrameKind::Class => "method",
            _ => "function",
        };
        let Some(id) = self.declare(outer, inner, NodeKind::Function, native_kind) else { return };
        let _ = id;
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
        spec.signature = signature(outer, self.source);
        spec.doc_comment = inner.child_by_field_name("body").and_then(|body| docstring(body, self.source));
        let id = self.emitter.declare(spec, is_public(&own));
        let scope = self.scopes.path().to_string();
        self.model.declare(&scope, &name, &qualified, DeclRef { id: id.clone(), kind });
        Some(id)
    }

    /// A module-level assignment: one `Variable` per name it binds, plus, for
    /// `__all__`, the list of names this module republishes.
    fn assignment(&mut self, assignment: Node) {
        let Some(left) = assignment.child_by_field_name("left") else { return };
        let mut names = Vec::new();
        collect_assigned_names(left, self.source, &mut names);
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
            spec.signature = Some(assignment_signature(name, assignment, self.source));
            let id = self.emitter.declare(spec, is_public(&own));
            let scope = self.scopes.path().to_string();
            self.model.declare(&scope, name, name, DeclRef { id, kind: NodeKind::Variable });
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

    /// `import a.b`, `import a.b as c` - one leaf per name bound.
    fn import_statement(&mut self, item: Node) {
        let range = self.emitter.positions().range(item);
        let mut cursor = item.walk();
        for leaf in item.named_children(&mut cursor) {
            match leaf.kind() {
                // `import a.b` binds the *top* package, `a`, and makes `a.b`
                // reachable through it - which is why the local name and the
                // imported container are different here and nowhere else.
                "dotted_name" => {
                    let Some(segments) = dotted_segments(leaf, self.source) else { continue };
                    let full = segments.join(".");
                    self.import_edge(&full, range, false);
                    // The *bound* name is the top package, so whether the
                    // binding is one of ours is a question about `a`, not
                    // about the `a.b` the edge was drawn onto.
                    let top = segments[0].to_string();
                    let binding = if self.project.has_container(&top) {
                        Import::Module { container: top.clone() }
                    } else {
                        Import::External
                    };
                    self.model.import(&top, binding);
                }
                "aliased_import" => {
                    let Some(name) = leaf.child_by_field_name("name") else { continue };
                    let Some(segments) = dotted_segments(name, self.source) else { continue };
                    let full = segments.join(".");
                    let external = self.import_edge(&full, range, false);
                    if let Some(alias) = leaf.child_by_field_name("alias") {
                        let binding = if external {
                            Import::External
                        } else {
                            Import::Module { container: full.clone() }
                        };
                        self.model.import(text(alias, self.source), binding);
                    }
                }
                _ => {}
            }
        }
    }

    /// `from a.b import …`, absolute or relative, named or star.
    fn import_from_statement(&mut self, item: Node) {
        let range = self.emitter.positions().range(item);
        let Some(module_name) = item.child_by_field_name("module_name") else { return };
        let (container, relative) = match module_name.kind() {
            "relative_import" => {
                let Some(container) = self.relative_container(module_name) else {
                    // More dots than there are packages above this file -
                    // Python's own `ImportError`. Nothing is emitted, which
                    // is the honest answer to a statement that cannot run.
                    return;
                };
                (container, true)
            }
            _ => {
                let Some(segments) = dotted_segments(module_name, self.source) else { return };
                (segments.join("."), false)
            }
        };

        let external = self.import_edge(&container, range, relative);

        let mut cursor = item.walk();
        for leaf in item.named_children(&mut cursor) {
            if leaf == module_name {
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
                        self.emitter.reexport(
                            "*",
                            container_target(&container, TargetKey::Name("*".to_string()), &self.module.key),
                            &self.module.key,
                            range,
                        );
                    }
                }
                "dotted_name" => {
                    let Some(segments) = dotted_segments(leaf, self.source) else { continue };
                    let Some(name) = segments.last() else { continue };
                    self.imported_name(&container, name, name, range, external);
                }
                "aliased_import" => {
                    let Some(name_node) = leaf.child_by_field_name("name") else { continue };
                    let Some(segments) = dotted_segments(name_node, self.source) else { continue };
                    let Some(name) = segments.last() else { continue };
                    let local = leaf
                        .child_by_field_name("alias")
                        .map(|alias| text(alias, self.source))
                        .unwrap_or(name);
                    self.imported_name(&container, name, local, range, external);
                }
                _ => {}
            }
        }
    }

    /// The container a `relative_import` names - see
    /// [`ModuleCtx::relative`].
    ///
    /// The dot count is taken from the `import_prefix`'s own text rather than
    /// from its child count, because the tokenizer splits a run of dots into
    /// `.` and `...` tokens in ways that depend on how many there are, and
    /// counting characters cannot get that wrong.
    fn relative_container(&self, module_name: Node) -> Option<String> {
        let mut cursor = module_name.walk();
        let children: Vec<Node> = module_name.children(&mut cursor).collect();
        let prefix = children.iter().find(|child| child.kind() == "import_prefix")?;
        let level = text(*prefix, self.source).chars().filter(|ch| *ch == '.').count();
        let tail = children
            .iter()
            .find(|child| child.kind() == "dotted_name")
            .and_then(|node| dotted_segments(*node, self.source))
            .unwrap_or_default();
        self.module.relative(level, &tail)
    }

    /// One name a `from … import …` binds: a `pending_symbol` placeholder at
    /// its real address, a `REFERENCES` edge from the file onto it, and the
    /// binding recorded for the body pass.
    ///
    /// The edge starts at the **file**, not at a symbol, because an import
    /// line belongs to the file rather than to anything in it - so it shows up
    /// in `find_references` as a whole-file row, which is the granularity an
    /// import statement genuinely has.
    fn imported_name(&mut self, container: &str, name: &str, local: &str, range: Range, external: bool) {
        if external {
            self.model.import(local, Import::External);
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
        self.model.import(local, Import::Item { container: container.to_string(), name: name.to_string() });
    }

    /// The `IMPORTS` edge from this file onto whatever a dotted name
    /// addressed, and whether that turned out to be outside this project.
    ///
    /// A **relative** import is never outside it: that is what the leading
    /// dots mean, so there is nothing to decide and
    /// `ProjectContext::has_container` is not consulted. An absolute one is
    /// decided by that query - see `crate::project`'s Decision 8.
    fn import_edge(&mut self, container: &str, range: Range, relative: bool) -> bool {
        let internal = relative || self.project.has_container(container);
        let to = if internal {
            self.emitter.placeholder(
                PlaceholderKind::ResolvedModule,
                container.rsplit('.').next().unwrap_or(container),
                container_target(container, TargetKey::Name("*".to_string()), container),
                range,
            )
        } else {
            self.emitter.external_module(container, range)
        };
        let file = self.emitter.file_id().to_string();
        self.emitter.placeholder_edge(EdgeKind::Imports, &file, &to);
        !internal
    }

    /// Emits a `reexport` node for every `__all__` entry this file imported
    /// from somewhere else - see this module's doc.
    ///
    /// Run after the whole file has been walked, because `__all__` may be
    /// written above the imports it names (and in a package's `__init__` it
    /// routinely is).
    pub(crate) fn reexport_dunder_all(&mut self) {
        let all = self.model.dunder_all().clone();
        let Some(range) = all.range else { return };
        for published in &all.names {
            // A name this file declares itself is already a member of the
            // container a lookup searches, so a re-export node for it would
            // be a second, weaker answer to a question already answered.
            if self.model.lookup("", published, None).is_some() {
                continue;
            }
            let Some(Import::Item { container, name }) = self.model.lookup_import(published) else {
                continue;
            };
            let (container, name) = (container.clone(), name.clone());
            self.emitter.reexport(
                published,
                container_target(&container, TargetKey::Name(name), &self.module.key),
                &self.module.key,
                range,
            );
        }
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
