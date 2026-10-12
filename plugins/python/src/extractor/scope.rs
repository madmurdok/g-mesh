//! Decision 1: the lexical scope stack, and why a structural extractor cannot
//! do without one.
//!
//! # The trap
//!
//! Every edge this plugin emits for a bare name starts by asking "is this
//! name a declaration somewhere else". If nothing tracks *local* bindings,
//! the answer for
//!
//! ```python
//! parse = 1                 # module level
//!
//! def run(config):
//!     parse = config.split()
//!     return helper(parse)
//! ```
//!
//! is that `parse` is the module-level declaration, and the extractor emits a
//! `REFERENCES` edge onto it for a local list. `find_references` then reports
//! a use that does not exist. This is the shape every structural extractor
//! gets wrong first, and the reason it is worth its own module is that the
//! fix has to be *complete*: one unhandled binding form is one silent class
//! of wrong edges.
//!
//! # Python's own rule, which is not the one Rust has
//!
//! Rust binds a `let` at the point it is written. Python binds a name for the
//! **whole** of the function that assigns it anywhere - that is what makes
//!
//! ```python
//! count = 0
//!
//! def f():
//!     print(count)   # UnboundLocalError, not the module-level `count`
//!     count = 1
//! ```
//!
//! an error rather than a read of the global. So [`Scopes::bind_function_locals`]
//! is a **pre-scan** of the function's body, run before any of it is walked,
//! rather than a bind-as-you-go the way the Rust plugin's `let_declaration`
//! handler works. Binding as we went would leave the `print(count)` line
//! resolving to the module-level `count` - a wrong edge, and one that only
//! appears in code that is already broken, which is exactly the kind of bug
//! that survives a test suite.
//!
//! The pre-scan deliberately does **not** descend into a nested `def`, `class`
//! or `lambda` body: those are their own scopes, and a name assigned inside
//! one is not local to the enclosing function.
//!
//! # Frames, and why a class frame is skipped
//!
//! A frame is a lexical scope: the module, a class body, a function body, a
//! lambda. Each carries the dotted path of declarations made in it (`""`,
//! `"Greeter"`, `"outer"`) and the set of names bound in it that are *not*
//! declarations.
//!
//! Resolving a bare name walks the frames from the inside out, and skips
//! every **class** frame that is not the innermost one. That is Python's own
//! rule, not an approximation: a class body is not a closure, so
//!
//! ```python
//! class C:
//!     def helper(self): ...
//!     def run(self):
//!         return helper()   # NameError - `helper` is not in scope here
//! ```
//!
//! does not see `C.helper`, and an extractor that "helpfully" linked it would
//! be reporting a call Python raises on. A statement written *directly* in
//! the class body does see the class's own names, which is why the innermost
//! frame is never skipped.
//!
//! # What it does when it is unsure
//!
//! [`Frame::binds`] answering `true` means "do not emit an edge for this
//! name". Every binding form below adds names; nothing ever removes one
//! except leaving its frame. So the failure direction is *over*-binding - a
//! name shadowed more widely than Python would shadow it - which costs a
//! missing edge. That is the standing rule (`graph::symbol_links`: "a missing
//! edge beats a wrong one").

use std::collections::{HashMap, HashSet};

use g_mesh_plugin_sdk::wire::QualifiedPath;
use tree_sitter::Node;

use crate::extractor::decls::importlib_name;
use crate::extractor::model::ImportlibName;
use crate::extractor::syntax::text;

/// What kind of lexical scope a frame is - see this module's doc for why the
/// distinction matters exactly once, when a class frame is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    /// The file's own top level.
    Module,
    /// A `class C:` body.
    Class,
    /// A `def`/`async def` body, or a `lambda`.
    Function,
}

/// One lexical scope: what declarations made in it are named, and what names
/// it binds that are not declarations.
#[derive(Debug)]
pub(crate) struct Frame {
    kind: FrameKind,
    /// The dotted path a declaration made in this frame is prefixed with -
    /// `""` at module level, `"Greeter"` inside `class Greeter`,
    /// `"Greeter.render"` inside its method. See `super::keys`, Decision 2.
    path: String,
    locals: HashSet<String>,
    /// The locals of a function frame that are, at every binding of them in
    /// that function, one consistent `importlib` spelling (GM-510, D7b).
    /// Filled by [`Scopes::bind_function_locals`]; a subset of `locals`.
    importlib: HashMap<String, ImportlibName>,
    /// Every name a class body binds, in any form and with no source order
    /// (GM-510): filled by [`Scopes::note_class_names`] for a class frame
    /// only, and kept apart from `locals` so that general name resolution in
    /// a class body is unchanged. What keeps a class-level `NAME` from
    /// folding to the module constant of the same name.
    class_names: HashSet<String>,
}

impl Frame {
    fn new(kind: FrameKind, path: String) -> Self {
        Self { kind, path, locals: HashSet::new(), importlib: HashMap::new(), class_names: HashSet::new() }
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The `importlib` spelling this frame's local `name` stands for, when
    /// every binding of it in the function is that one import.
    pub(crate) fn importlib(&self, name: &str) -> Option<ImportlibName> {
        self.importlib.get(name).copied()
    }

    /// Whether this frame binds `name` as something other than a declaration
    /// - a parameter, an assignment target, a `with … as`, a local import.
    pub(crate) fn binds(&self, name: &str) -> bool {
        self.locals.contains(name)
    }

    /// Whether this is a class frame whose body binds `name` anywhere -
    /// an assignment, a loop or `with` target, an import. A class body reads
    /// its own `NAME` before the module's, so such a name is never the
    /// module-level one; a method does not see it, and
    /// [`Scopes::chain`] already skips the class frame there.
    pub(crate) fn class_binds(&self, name: &str) -> bool {
        self.class_names.contains(name)
    }
}

/// A `.`-joined path of identifiers (a frame path, a dotted module key, a
/// member key built from them) as segments. `None` when a segment would be
/// empty, which no path this plugin builds has.
pub(crate) fn dotted_path(text: &str) -> Option<QualifiedPath> {
    let mut names = text.split('.');
    let mut path = QualifiedPath::root(names.next()?);
    for name in names {
        path = path.child(".", name);
    }
    path.check().is_ok().then_some(path)
}

/// The frames in scope at one point of a walk, innermost last.
#[derive(Debug)]
pub(crate) struct Scopes {
    frames: Vec<Frame>,
    /// While [`Scopes::bind_function_locals`] runs: every name it binds, and
    /// the `importlib` spelling of it when every binding so far was that one
    /// import (`None` once anything else bound it). `None` outside the
    /// pre-scan, so a binding made by the body walk records nothing.
    prescan: Option<HashMap<String, Option<ImportlibName>>>,
}

impl Scopes {
    /// A stack holding only the module frame, which is where every file's
    /// walk starts and which is never popped.
    pub(crate) fn new() -> Self {
        Self { frames: vec![Frame::new(FrameKind::Module, String::new())], prescan: None }
    }

    /// Enters a scope whose declarations are prefixed with `path`. Every
    /// caller pairs this with [`Scopes::pop`]; the two are not a guard type
    /// because the walk is recursive and a guard would have to borrow the
    /// stack for the whole of each recursive call.
    pub(crate) fn push(&mut self, kind: FrameKind, path: String) {
        self.frames.push(Frame::new(kind, path));
    }

    /// Leaves the innermost scope. The module frame is never popped: a walk
    /// that tried would be a caller bug, and answering with an empty stack
    /// would turn it into wrong edges rather than a visible failure.
    pub(crate) fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// The dotted path of the innermost frame.
    pub(crate) fn path(&self) -> &str {
        self.frames.last().map_or("", |frame| frame.path.as_str())
    }

    /// The dotted path a declaration named `name` in the innermost frame gets
    /// - `f` at module level, `Greeter.render` inside `class Greeter`.
    pub(crate) fn child_path(&self, name: &str) -> String {
        let path = self.path();
        if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        }
    }

    /// [`Scopes::child_path`] as segments, one per enclosing definition and
    /// one for `name`, each written after `.`.
    pub(crate) fn child_qualified_path(&self, name: &str) -> Option<QualifiedPath> {
        dotted_path(&self.child_path(name))
    }

    /// The innermost frame's kind.
    pub(crate) fn kind(&self) -> FrameKind {
        self.frames.last().map_or(FrameKind::Module, |frame| frame.kind)
    }

    /// The frames a bare name is resolved against, innermost first, with
    /// every non-innermost class frame skipped - see this module's doc.
    pub(crate) fn chain(&self) -> impl Iterator<Item = &Frame> {
        let last = self.frames.len().saturating_sub(1);
        self.frames
            .iter()
            .enumerate()
            .rev()
            .filter(move |(index, frame)| *index == last || frame.kind != FrameKind::Class)
            .map(|(_, frame)| frame)
    }

    /// Binds one name in the innermost scope.
    pub(crate) fn bind(&mut self, name: &str) {
        if let Some(frame) = self.frames.last_mut() {
            frame.locals.insert(name.to_string());
        }
    }

    /// Binds every name a parameter list introduces - including `self`, and
    /// including the names inside a destructuring parameter.
    ///
    /// A parameter's **annotation and default value are skipped**, because
    /// they are not bindings but ordinary expressions evaluated in the
    /// enclosing scope: `def f(x: Config = DEFAULT)` uses `Config` and
    /// `DEFAULT`, and binding them here would suppress two real references.
    pub(crate) fn bind_parameters(&mut self, parameters: Node, source: &str) {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            self.bind_parameter(parameter, source);
        }
    }

    fn bind_parameter(&mut self, parameter: Node, source: &str) {
        if parameter.kind() == "identifier" {
            self.bind(text(parameter, source));
            return;
        }
        let skip = [parameter.child_by_field_name("type"), parameter.child_by_field_name("value")];
        let mut cursor = parameter.walk();
        for child in parameter.named_children(&mut cursor) {
            if skip.contains(&Some(child)) {
                continue;
            }
            self.bind_parameter(child, source);
        }
    }

    /// Binds every name the *whole* of a function body assigns - Python's own
    /// scoping rule, run as a pre-scan before the body is walked. See this
    /// module's doc for why it cannot be done as the walk goes.
    ///
    /// Nested `def`/`class`/`lambda` bodies are not descended into: they are
    /// their own scopes. Their *names*, though, are declarations this plugin
    /// emits nodes for, so they are deliberately not bound here either - a
    /// bare `inner()` inside `outer` must resolve to the node for
    /// `outer.inner`, and binding the name would suppress that edge.
    /// `global x` / `nonlocal x` say the opposite of "local": the name
    /// resolves *outside* this function, and the `x = 1` that inevitably
    /// follows must therefore not bind it. That is why the declared names are
    /// collected in their own sweep before anything is bound rather than
    /// handled where they are met - a `global` may be written below the
    /// assignment it governs, and Python still applies it to the whole
    /// function.
    ///
    /// The same sweep decides which locals are `importlib` bindings (GM-510,
    /// D7b): a name is one only when every binding of it in the function is
    /// an import of one consistent [`ImportlibName`]. A parameter (already
    /// bound when this runs), an assignment, a `for`/`with`/`except` target,
    /// a walrus, or an import of anything else poisons it. The sweep has no
    /// source order, so `try: from importlib import import_module / except
    /// ImportError: from compat import import_module` is poisoned too: a
    /// missing edge, never a wrong one.
    pub(crate) fn bind_function_locals(&mut self, body: Node, source: &str) {
        let mut declared_elsewhere = HashSet::new();
        collect_outer_declarations(body, source, &mut declared_elsewhere);
        let parameters = self.frames.last().map(|frame| frame.locals.clone()).unwrap_or_default();
        self.prescan = Some(parameters.into_iter().map(|name| (name, None)).collect());
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.bind_locals_in(child, source, &declared_elsewhere);
        }
        let prescan = self.prescan.take().unwrap_or_default();
        if let Some(frame) = self.frames.last_mut() {
            frame.importlib =
                prescan.into_iter().filter_map(|(name, importlib)| Some((name, importlib?))).collect();
        }
    }

    /// Records in the innermost (class) frame every name its `body` binds
    /// ([`Frame::class_binds`]), by the same sweep as
    /// [`Scopes::bind_function_locals`] but leaving `locals` as it was. The
    /// sweep has no source order, so a use above the assignment is treated
    /// as the class's own name too: a missing edge, never a wrong one.
    pub(crate) fn note_class_names(&mut self, body: Node, source: &str) {
        let mut declared_elsewhere = HashSet::new();
        collect_outer_declarations(body, source, &mut declared_elsewhere);
        let locals = self.frames.last().map(|frame| frame.locals.clone()).unwrap_or_default();
        self.prescan = Some(HashMap::new());
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.bind_locals_in(child, source, &declared_elsewhere);
        }
        let prescan = self.prescan.take().unwrap_or_default();
        if let Some(frame) = self.frames.last_mut() {
            frame.locals = locals;
            frame.class_names = prescan.into_keys().collect();
        }
    }

    /// Records one pre-scan binding of `name`: `importlib` is the spelling an
    /// import bound it to, `None` for any other binding.
    fn note_prescan(&mut self, name: &str, importlib: Option<ImportlibName>) {
        let Some(prescan) = self.prescan.as_mut() else { return };
        prescan
            .entry(name.to_string())
            .and_modify(|seen| {
                if *seen != importlib {
                    *seen = None;
                }
            })
            .or_insert(importlib);
    }

    fn bind_locals_in(&mut self, node: Node, source: &str, skip: &HashSet<String>) {
        match node.kind() {
            // Their own scopes; their names are declarations, not locals.
            "function_definition" | "class_definition" | "decorated_definition" | "lambda" => return,
            "global_statement" | "nonlocal_statement" => return,
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.bind_unless(left, source, skip);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind_unless(name, source, skip);
                }
            }
            // `with … as x`, `except … as e`: the grammar spells both with an
            // `as_pattern` whose `alias` is the binding.
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.bind_unless(alias, source, skip);
                }
            }
            "import_statement" | "import_from_statement" => {
                self.bind_import(node, source, skip);
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.bind_locals_in(child, source, skip);
        }
    }

    /// The names an `import` inside a function body binds. They are locals
    /// here, not module members: a function-local import is visible only in
    /// that function. Its `IMPORTS` edges are drawn by `super::bodies`
    /// (GM-510, D7a), but what it binds stays a plain local - only whether
    /// that local is `importlib` is recorded ([`Frame::importlib`]).
    fn bind_import(&mut self, node: Node, source: &str, skip: &HashSet<String>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let local = match child.kind() {
                // `import a.b` binds `a`; `from a import b` binds `b`.
                "dotted_name" => {
                    if Some(child) == node.child_by_field_name("module_name") {
                        continue;
                    }
                    child.named_child(0)
                }
                "aliased_import" => child.child_by_field_name("alias"),
                _ => None,
            };
            let Some(local) = local else { continue };
            let name = text(local, source);
            if local.kind() != "identifier" || skip.contains(name) {
                continue;
            }
            self.bind(name);
            self.note_prescan(name, importlib_name(node, child, source));
        }
    }

    /// Binds every name an assignment target introduces.
    ///
    /// Walks the target rather than matching each pattern node kind, because
    /// the shapes that *do not* bind are the short list: an attribute
    /// (`self.x = 1` mutates an object, it binds no name here) and a
    /// subscript (`table[k] = v`), both of which are also real *uses* of
    /// their own object and are walked as such by [`super::bodies`].
    pub(crate) fn bind_target(&mut self, target: Node, source: &str) {
        self.bind_unless(target, source, &HashSet::new());
    }

    /// [`Scopes::bind_target`], minus the names a `global`/`nonlocal`
    /// statement has claimed for an outer scope.
    fn bind_unless(&mut self, target: Node, source: &str, skip: &HashSet<String>) {
        match target.kind() {
            "identifier" => {
                let name = text(target, source);
                if !skip.contains(name) {
                    self.bind(name);
                    self.note_prescan(name, None);
                }
            }
            "attribute" | "subscript" => {}
            _ => {
                let mut cursor = target.walk();
                for child in target.named_children(&mut cursor) {
                    self.bind_unless(child, source, skip);
                }
            }
        }
    }
}

/// Every name a `global` or `nonlocal` statement anywhere in this function's
/// own body claims for an outer scope. Nested `def`/`class`/`lambda` bodies
/// are skipped, because their `global` statements govern *them*.
fn collect_outer_declarations(node: Node, source: &str, out: &mut HashSet<String>) {
    match node.kind() {
        "function_definition" | "class_definition" | "decorated_definition" | "lambda" => return,
        "global_statement" | "nonlocal_statement" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    out.insert(text(child, source).to_string());
                }
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_outer_declarations(child, source, out);
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

    fn first<'t>(tree: &'t tree_sitter::Tree, kind: &str) -> Node<'t> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == kind {
                return node;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        panic!("no {kind} in the tree");
    }

    /// Binds the locals of the file's first function and answers what is
    /// bound in that frame.
    fn locals_of(source: &str) -> Scopes {
        let tree = parse(source);
        let definition = first(&tree, "function_definition");
        let mut scopes = Scopes::new();
        scopes.push(FrameKind::Function, "f".to_string());
        if let Some(parameters) = definition.child_by_field_name("parameters") {
            scopes.bind_parameters(parameters, source);
        }
        scopes.bind_function_locals(definition.child_by_field_name("body").unwrap(), source);
        scopes
    }

    fn binds(scopes: &Scopes, name: &str) -> bool {
        scopes.chain().any(|frame| frame.binds(name))
    }

    #[test]
    fn a_binding_is_only_in_scope_inside_its_frame() {
        let mut scopes = Scopes::new();
        assert!(!binds(&scopes, "x"));
        scopes.push(FrameKind::Function, "f".into());
        scopes.bind("x");
        assert!(binds(&scopes, "x"));
        scopes.push(FrameKind::Function, "f.g".into());
        assert!(binds(&scopes, "x"), "an inner function still closes over an outer binding");
        scopes.pop();
        scopes.pop();
        assert!(!binds(&scopes, "x"));
    }

    /// Python's rule, and the reason the pre-scan exists: a name assigned
    /// *anywhere* in a function is local to the whole of it.
    #[test]
    fn a_name_assigned_later_in_the_function_is_already_local_at_the_top() {
        let scopes = locals_of("def f():\n    print(count)\n    count = 1\n");
        assert!(binds(&scopes, "count"));
    }

    #[test]
    fn every_binding_form_python_has_is_covered() {
        let scopes = locals_of(
            "def f(a, b: int = D, *rest, **kw):\n\
             \x20   with open(p) as fh:\n\
             \x20       pass\n\
             \x20   try:\n\
             \x20       pass\n\
             \x20   except ValueError as err:\n\
             \x20       pass\n\
             \x20   for i, j in rows:\n\
             \x20       pass\n\
             \x20   total = [n for n in rows]\n\
             \x20   while (chunk := read()):\n\
             \x20       pass\n\
             \x20   import os.path\n\
             \x20   from pkg import thing as renamed\n\
             \x20   first, (second, third) = pair\n",
        );
        for name in [
            "a", "b", "rest", "kw", "fh", "err", "i", "j", "total", "n", "chunk", "os", "renamed", "first",
            "second", "third",
        ] {
            assert!(binds(&scopes, name), "{name} must be bound");
        }
    }

    /// A parameter's annotation and default are expressions, not bindings -
    /// suppressing them would suppress two real references.
    #[test]
    fn an_annotation_and_a_default_are_not_bindings() {
        let scopes = locals_of("def f(b: Config = DEFAULT):\n    pass\n");
        assert!(binds(&scopes, "b"));
        assert!(!binds(&scopes, "Config"));
        assert!(!binds(&scopes, "DEFAULT"));
    }

    /// A nested definition's name is a declaration this plugin emits a node
    /// for, so binding it would suppress the edge onto that node.
    #[test]
    fn a_nested_definitions_name_is_not_bound_as_a_local() {
        let scopes = locals_of("def f():\n    def inner():\n        shadowed = 1\n    return inner()\n");
        assert!(!binds(&scopes, "inner"));
        assert!(!binds(&scopes, "shadowed"), "a nested function's own locals are not this frame's");
    }

    /// `global` says "this name is not local", so binding it would suppress
    /// exactly the edge the statement announces.
    #[test]
    fn a_global_declaration_does_not_make_the_name_local() {
        let scopes = locals_of("def f():\n    global counter\n    counter = 1\n");
        assert!(!binds(&scopes, "counter"));
    }

    /// `self.x = 1` and `table[k] = v` bind no name.
    #[test]
    fn an_attribute_or_subscript_target_binds_nothing() {
        let scopes = locals_of("def f(self, table, k, v):\n    self.x = 1\n    table[k] = v\n");
        assert!(!binds(&scopes, "x"));
    }

    /// The class-skipping rule: a method body does not see its class's own
    /// names, but a statement directly in the class body does.
    #[test]
    fn a_method_body_skips_its_class_frame_and_the_class_body_does_not() {
        let mut scopes = Scopes::new();
        scopes.push(FrameKind::Class, "C".into());
        assert_eq!(scopes.chain().map(Frame::path).collect::<Vec<_>>(), vec!["C", ""]);
        scopes.push(FrameKind::Function, "C.m".into());
        assert_eq!(
            scopes.chain().map(Frame::path).collect::<Vec<_>>(),
            vec!["C.m", ""],
            "the class frame is skipped from inside a method"
        );
    }

    #[test]
    fn a_child_path_is_the_frames_path_plus_the_name() {
        let mut scopes = Scopes::new();
        assert_eq!(scopes.child_path("f"), "f");
        scopes.push(FrameKind::Class, "Outer".into());
        assert_eq!(scopes.child_path("Inner"), "Outer.Inner");
        scopes.push(FrameKind::Class, "Outer.Inner".into());
        assert_eq!(scopes.child_path("m"), "Outer.Inner.m");
    }
}
