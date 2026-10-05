//! Imports, re-exports and computed specifiers: the part of the walk that
//! names other files.
//!
//! - Every module specifier becomes a `Module` placeholder with an `IMPORTS`
//!   edge from the file. The [`SpecifierResolver`] decides which: a specifier
//!   it resolves to a project file is a `resolved_module` whose
//!   `qualifiedName` is that file's path and whose target is
//!   `{file: path, name: "*"}`; any other is an `external_module` named by the
//!   raw specifier, with no target.
//! - The names an import binds are kept only when its specifier resolved:
//!   named and default imports in [`ImportBinding`]s (a use of one becomes a
//!   `pending_symbol` placeholder, [`Declarer::imported_symbol`]), a
//!   namespace import apart from them, since it stands for a whole module.
//! - `export { a as b } from "./y"` and `export * from "./y"` import `./y`
//!   and, when it resolved, record a `reexport` placeholder per published
//!   name with no edge; `export * as NS from "./y"` records none.
//! - `require(...)` and `import(...)` are imports when their argument folds
//!   to strings from this file alone: a literal, a template over string
//!   `const`s and string enum members, a conditional of such branches (one
//!   edge per branch), or `path.join`/`path.resolve(__dirname, ...)` over
//!   static segments. Folding is all or nothing; what does not fold imports
//!   nothing. The full scope is `docs/architecture/g-mesh-v1.md`
//!   ("Computed import specifiers").

use std::collections::{HashMap, HashSet};

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};
use g_mesh_plugin_sdk::RelPath;
use tree_sitter::Node;

use crate::extractor::decls::Declarer;
use crate::extractor::keys::{
    file_target, pending_symbol_qualified_name, EXTERNAL_MODULE_NATIVE_KIND, PENDING_SYMBOL_NATIVE_KIND,
    REEXPORT_ALL_NAME, REEXPORT_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND,
};
use crate::extractor::model::{ConstantInitializer, ImportBinding, NodeParams};
use crate::extractor::scope::{is_locally_bound, Scope};
use crate::extractor::syntax::{child_of_kind, children, named_children, string_literal_value};

/// Turns a module specifier written in the file `from` into the project file
/// it names, or `None` when it names nothing in this project (a package, a
/// builtin, a dangling relative import, a file this plugin does not parse).
/// The walk stays a function of (path, text); the filesystem-dependent policy
/// lives behind this.
pub type SpecifierResolver<'r> = dyn Fn(&str, &RelPath) -> Option<RelPath> + 'r;

/// The resolver that resolves nothing: every import is an `external_module`.
pub fn no_resolution(_specifier: &str, _from: &RelPath) -> Option<RelPath> {
    None
}

/// How Node's path module is spelled as a specifier.
const PATH_MODULE_SPECIFIERS: [&str; 2] = ["path", "node:path"];

/// The `path` members that are pure arithmetic on their arguments, so a call
/// of one made of literals is a literal.
const PATH_JOIN_MEMBERS: [&str; 2] = ["join", "resolve"];

/// A `require(...)`/`import(...)` whose argument may fold, kept until the
/// walk is over because the constants it reads may be declared below it.
#[derive(Debug, Clone)]
pub struct PendingCallImport<'t> {
    /// The call's first argument: the specifier expression.
    specifier: Node<'t>,
    scope: Scope,
    /// For `require(...)` only, the callee: when the argument does not fold,
    /// the site is a call of the name `require` instead.
    fallback_call: Option<Node<'t>>,
}

/// What the walk learns from imports and constants, read back when computed
/// specifiers fold and when a use resolves to an imported name.
#[derive(Debug, Default)]
pub struct ImportState<'t> {
    /// Local name -> the symbol of another project file it stands for. First
    /// binding wins.
    import_bindings: HashMap<String, ImportBinding>,
    /// Local name -> the project module `import * as <name>` bound it to.
    namespace_bindings: HashMap<String, ImportBinding>,
    /// Local names bound to Node's `path` module.
    path_module_bindings: HashSet<String>,
    /// Node id of a file-level `const` -> its initializer. First declaration
    /// wins.
    constant_initializers: HashMap<String, ConstantInitializer<'t>>,
    /// `<enum qualifiedName>.<member>` -> the string literal the member is
    /// initialized to. First declaration wins.
    enum_member_values: HashMap<String, String>,
    pending_call_imports: Vec<PendingCallImport<'t>>,
}

impl<'a, 's, 't> Declarer<'a, 's, 't> {
    // --- import statements ---------------------------------------------

    /// `import ... from "x"` / `import "x"`: the placeholder and edge, the
    /// names bound when `x` resolved, and a binding of Node's path module.
    pub(super) fn handle_import(&mut self, node: Node<'t>) {
        let Some(source) = node.child_by_field_name("source") else { return };
        if let Some(target_path) = self.record_import(source) {
            self.record_import_bindings(node, &target_path);
        }
        let specifier = string_literal_value(source, self.source);
        self.record_path_module_binding(node, specifier.as_deref());
    }

    /// `import * as path from "node:path"` / `import path from "path"`: the
    /// local name, so `path.join(__dirname, ...)` against it can fold. Named
    /// imports (`import { join }`) bind no receiver and are not recorded.
    fn record_path_module_binding(&mut self, statement: Node<'t>, specifier: Option<&str>) {
        if !specifier.is_some_and(|specifier| PATH_MODULE_SPECIFIERS.contains(&specifier)) {
            return;
        }
        let Some(clause) = child_of_kind(statement, "import_clause") else { return };
        for child in named_children(clause) {
            match child.kind() {
                "identifier" => {
                    self.imports.path_module_bindings.insert(self.text(child).to_string());
                }
                "namespace_import" => {
                    if let Some(local) = first_identifier(child) {
                        self.imports.path_module_bindings.insert(self.text(local).to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// The local names an import of the project file `target_path` binds,
    /// each with the name it stands for there. A namespace import binds the
    /// module, not a symbol, and is kept apart.
    fn record_import_bindings(&mut self, statement: Node<'t>, target_path: &RelPath) {
        // `import "./side-effect"` has no clause and binds nothing.
        let Some(clause) = child_of_kind(statement, "import_clause") else { return };
        for child in named_children(clause) {
            match child.kind() {
                "identifier" => self.bind_import(child, target_path, "default"),
                "namespace_import" => {
                    let Some(local) = first_identifier(child) else { continue };
                    let local_name = self.text(local).to_string();
                    if self.imports.namespace_bindings.contains_key(&local_name) {
                        continue;
                    }
                    let binding = ImportBinding {
                        target_path: target_path.clone(),
                        imported_name: local_name.clone(),
                        at: self.range(local),
                    };
                    self.imports.namespace_bindings.insert(local_name, binding);
                }
                "named_imports" => {
                    for specifier in named_children(child) {
                        if specifier.kind() != "import_specifier" {
                            continue;
                        }
                        let Some(name) = specifier.child_by_field_name("name") else { continue };
                        let local = specifier.child_by_field_name("alias").unwrap_or(name);
                        let imported_name = self.text(name);
                        self.bind_import(local, target_path, imported_name);
                    }
                }
                _ => {}
            }
        }
    }

    /// Binds `local` to `imported_name` in `target_path`. First binding wins.
    fn bind_import(&mut self, local: Node<'t>, target_path: &RelPath, imported_name: &str) {
        let local_name = self.text(local).to_string();
        if self.imports.import_bindings.contains_key(&local_name) {
            return;
        }
        let binding = ImportBinding {
            target_path: target_path.clone(),
            imported_name: imported_name.to_string(),
            at: self.range(local),
        };
        self.imports.import_bindings.insert(local_name, binding);
    }

    /// The placeholder and `IMPORTS` edge for a literal specifier. Returns the
    /// project file it resolved to; an unreadable literal (an escape, an
    /// interpolation) imports nothing.
    pub(super) fn record_import(&mut self, source: Node<'t>) -> Option<RelPath> {
        let specifier = string_literal_value(source, self.source)?;
        self.record_specifier(&specifier, source)
    }

    /// The placeholder and `IMPORTS` edge for `specifier`, spanning `at`. A
    /// placeholder's identity is its `qualifiedName`, so specifiers resolving
    /// to one file share one node, and several specifiers folded from one
    /// site are several nodes at the same span.
    ///
    /// The edge targets the placeholder, never the target file's `File` node:
    /// only core knows whether that node is indexed, and repoints the edge.
    fn record_specifier(&mut self, specifier: &str, at: Node<'t>) -> Option<RelPath> {
        let resolved = (self.resolver)(specifier, self.path);
        let qualified_name = resolved.as_ref().map_or(specifier, RelPath::as_str);
        let mut params = NodeParams::new(NodeKind::Module, specifier, qualified_name, self.range(at));
        params.native_kind = Some(
            if resolved.is_some() { RESOLVED_MODULE_NATIVE_KIND } else { EXTERNAL_MODULE_NATIVE_KIND }
                .to_string(),
        );
        params.target = resolved.as_ref().map(|path| file_target(path.as_str(), REEXPORT_ALL_NAME));
        let index = self.model.add_node(params);
        let file_id = self.model.file_id().to_string();
        let target_id = self.model.node(index).id.clone();
        self.model.add_edge(&file_id, EdgeKind::Imports, &target_id);
        resolved
    }

    // --- re-exports -------------------------------------------------------

    /// Records that this file publishes `published_name` as `exported_name`
    /// of `target_path`, without declaring it: a `reexport` placeholder
    /// addressed at the name over there, with no edge and no entry in the
    /// file's name lookup. Two aliases of one target name share one node,
    /// the first one's.
    pub(super) fn record_reexport(
        &mut self,
        at: Node<'t>,
        published_name: &str,
        target_path: &RelPath,
        exported_name: &str,
    ) {
        let mut params = NodeParams::new(
            NodeKind::Module,
            published_name,
            pending_symbol_qualified_name(target_path.as_str(), exported_name),
            self.range(at),
        );
        params.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
        params.target = Some(file_target(target_path.as_str(), exported_name));
        self.model.add_node(params);
    }

    // --- uses of imported names ---------------------------------------------

    /// The `pending_symbol` placeholder for an imported local `name`, created
    /// on first use, or `None` when no import binds it.
    pub fn imported_symbol(&mut self, name: &str) -> Option<usize> {
        let binding = self.imports.import_bindings.get(name)?;
        let mut params = NodeParams::new(
            NodeKind::Module,
            binding.imported_name.clone(),
            pending_symbol_qualified_name(binding.target_path.as_str(), &binding.imported_name),
            binding.at,
        );
        params.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        params.target = Some(file_target(binding.target_path.as_str(), &binding.imported_name));
        Some(self.model.add_node(params))
    }

    /// The project module `import * as <name>` bound `name` to.
    pub fn namespace_binding(&self, name: &str) -> Option<&ImportBinding> {
        self.imports.namespace_bindings.get(name)
    }

    // --- facts kept for folding -----------------------------------------------

    /// A file-level `const` `name` (node `index`) initialized to `value`.
    pub(super) fn record_constant_initializer(
        &mut self,
        index: usize,
        name: &str,
        value: Node<'t>,
        scope: &Scope,
    ) {
        let id = self.model.node(index).id.clone();
        if self.imports.constant_initializers.contains_key(&id) {
            return;
        }
        self.imports.constant_initializers.insert(id, ConstantInitializer { value, scope: scope.clone() });
        if is_path_module_require(value, self.source) {
            self.imports.path_module_bindings.insert(name.to_string());
        }
    }

    /// The enum's members initialized to a plain string literal, under
    /// `<qualified_name>.<member>`.
    pub(super) fn record_enum_member_values(&mut self, node: Node<'t>, qualified_name: &str) {
        let Some(body) = node.child_by_field_name("body") else { return };
        for member in named_children(body) {
            if member.kind() != "enum_assignment" {
                continue;
            }
            let (Some(name), Some(value)) =
                (member.child_by_field_name("name"), member.child_by_field_name("value"))
            else {
                continue;
            };
            let Some(literal) = string_literal_value(value, self.source) else { continue };
            let key = format!("{qualified_name}.{}", self.text(name));
            self.imports.enum_member_values.entry(key).or_insert(literal);
        }
    }

    // --- computed specifiers ------------------------------------------------

    /// Defers the call's first argument for folding when its shape could
    /// fold. Returns whether the call was taken as an import at all; `false`
    /// leaves a `require(...)` an ordinary call of `require`.
    pub(super) fn record_call_import(
        &mut self,
        node: Node<'t>,
        scope: &Scope,
        fallback_call: Option<Node<'t>>,
    ) -> bool {
        let first =
            node.child_by_field_name("arguments").and_then(|args| named_children(args).first().copied());
        let Some(first) = first.filter(|first| is_foldable_specifier_shape(*first, self.source)) else {
            return false;
        };
        self.imports.pending_call_imports.push(PendingCallImport {
            specifier: first,
            scope: scope.clone(),
            fallback_call,
        });
        true
    }

    /// Folds every deferred call import. Returns the `require` callees whose
    /// argument did not fold, with their scopes: each is a call of the name
    /// `require`.
    pub(super) fn resolve_call_imports(&mut self) -> Vec<(Node<'t>, Scope)> {
        let pending = std::mem::take(&mut self.imports.pending_call_imports);
        let mut fallback_calls = Vec::new();
        for call_import in pending {
            if !self.resolve_call_import(&call_import) {
                if let Some(callee) = call_import.fallback_call {
                    fallback_calls.push((callee, call_import.scope));
                }
            }
        }
        fallback_calls
    }

    /// One `IMPORTS` edge per specifier the argument folds to. Returns
    /// whether it folded; an empty specifier names nothing and does not.
    fn resolve_call_import(&mut self, pending: &PendingCallImport<'t>) -> bool {
        let Some(specifiers) = self.fold_specifiers(pending.specifier, &pending.scope) else {
            return false;
        };
        if specifiers.iter().any(String::is_empty) {
            return false;
        }
        for specifier in specifiers {
            self.record_specifier(&specifier, pending.specifier);
        }
        true
    }

    /// Every specifier one argument can name, or `None` if any part of it is
    /// not statically known. Only a conditional yields more than one, one per
    /// branch, in source order.
    fn fold_specifiers(&self, node: Node<'t>, scope: &Scope) -> Option<Vec<String>> {
        if node.kind() == "ternary_expression" {
            let mut branches = Vec::new();
            for field in ["consequence", "alternative"] {
                let branch = node.child_by_field_name(field)?;
                branches.extend(self.fold_specifiers(branch, scope)?);
            }
            return Some(branches);
        }
        self.fold_static(node, scope, &mut HashSet::new()).map(|single| vec![single])
    }

    /// The one string an expression evaluates to, over a closed set of
    /// shapes: literals and templates, string `const`s, string enum members,
    /// and path arithmetic. `folding` holds the constants being folded, so a
    /// self-referencing `const` terminates.
    fn fold_static(&self, node: Node<'t>, scope: &Scope, folding: &mut HashSet<String>) -> Option<String> {
        match node.kind() {
            "string" | "template_string" => self.fold_quoted(node, scope, folding),
            "identifier" => self.fold_constant(self.text(node), scope, folding),
            "member_expression" => self.fold_enum_member(node, scope),
            "call_expression" => self.fold_path_call(node, scope, folding),
            _ => None,
        }
    }

    /// A quoted literal's text with every `${...}` folded. Anything but plain
    /// fragments and substitutions (an escape sequence) does not fold.
    fn fold_quoted(&self, node: Node<'t>, scope: &Scope, folding: &mut HashSet<String>) -> Option<String> {
        let mut folded = String::new();
        for part in named_children(node) {
            match part.kind() {
                "string_fragment" => folded.push_str(self.text(part)),
                "template_substitution" => {
                    let expression = named_children(part).into_iter().next()?;
                    folded.push_str(&self.fold_static(expression, scope, folding)?);
                }
                _ => return None,
            }
        }
        Some(folded)
    }

    /// What `name` is bound to, when this file binds it to a `const` that
    /// folds. A local of that name shadows the declaration; a name another
    /// file declares does not fold. The initializer folds in the scope it
    /// was written in.
    fn fold_constant(&self, name: &str, scope: &Scope, folding: &mut HashSet<String>) -> Option<String> {
        if is_locally_bound(name, scope.locals.as_ref()) {
            return None;
        }
        let index = self.model.lookup_by_name(name, &scope.namespace_prefix, Some(NodeKind::Variable))?;
        let id = self.model.node(index).id.clone();
        let initializer = self.imports.constant_initializers.get(&id)?.clone();
        if folding.contains(&id) {
            return None;
        }
        folding.insert(id.clone());
        let value = self.fold_static(initializer.value, &initializer.scope, folding);
        folding.remove(&id);
        value
    }

    /// `Enum.Member`, where `Enum` is an enum this file declares and `Member`
    /// one of its string members.
    fn fold_enum_member(&self, node: Node<'t>, scope: &Scope) -> Option<String> {
        let object = node.child_by_field_name("object").filter(|object| object.kind() == "identifier")?;
        let property = node.child_by_field_name("property")?;
        let object_name = self.text(object);
        if is_locally_bound(object_name, scope.locals.as_ref()) {
            return None;
        }
        let owner = self.model.lookup_by_name(object_name, &scope.namespace_prefix, Some(NodeKind::Type))?;
        let owner = self.model.node(owner);
        if owner.native_kind.as_deref() != Some("enum") {
            return None;
        }
        let key = format!("{}.{}", owner.qualified_name, self.text(property));
        self.imports.enum_member_values.get(&key).cloned()
    }

    /// `path.join(__dirname, ...)` / `path.resolve(__dirname, ...)` over
    /// segments that fold, as the relative specifier it spells
    /// (`./plugins/index.js`). Refused: a receiver not bound to Node's path
    /// module, an anchor other than `__dirname`, a segment that does not fold
    /// or is absolute, no segment, and a join that is the directory itself.
    fn fold_path_call(&self, node: Node<'t>, scope: &Scope, folding: &mut HashSet<String>) -> Option<String> {
        if !is_path_arithmetic_shape(node, self.source) {
            return None;
        }
        let receiver = node.child_by_field_name("function")?.child_by_field_name("object")?;
        let receiver = self.text(receiver);
        if is_locally_bound(receiver, scope.locals.as_ref())
            || !self.imports.path_module_bindings.contains(receiver)
        {
            return None;
        }
        let arguments = named_children(node.child_by_field_name("arguments")?);
        let (anchor, rest) = arguments.split_first()?;
        if anchor.kind() != "identifier" || self.text(*anchor) != "__dirname" {
            return None;
        }
        if is_locally_bound("__dirname", scope.locals.as_ref()) {
            return None;
        }
        let mut segments = Vec::with_capacity(rest.len());
        for argument in rest {
            let segment = self.fold_static(*argument, scope, folding)?;
            if segment.starts_with('/') {
                return None;
            }
            segments.push(segment);
        }
        if segments.is_empty() {
            return None;
        }
        let joined = posix_join(&segments);
        if joined == "." || joined.is_empty() {
            return None;
        }
        // Without a leading `.` a specifier names a package.
        Some(if joined.starts_with('.') { joined } else { format!("./{joined}") })
    }
}

/// The first identifier child of a `namespace_import`.
fn first_identifier(node: Node) -> Option<Node> {
    named_children(node).into_iter().find(|child| child.kind() == "identifier")
}

/// `export * from "./y"`, told apart from `export * as NS from "./y"` by the
/// `*` being a direct child: the namespace form nests it in a
/// `namespace_export`.
pub fn is_whole_module_reexport(statement: Node) -> bool {
    children(statement).iter().any(|child| child.kind() == "*")
}

/// Whether a `require()`/`import()` argument has a shape a fold could read:
/// a literal or template, a conditional, a name or member access, or
/// `<identifier>.join/resolve(...)`. Promises nothing about the fold.
pub fn is_foldable_specifier_shape(node: Node, source: &str) -> bool {
    match node.kind() {
        "string" | "template_string" | "ternary_expression" | "identifier" | "member_expression" => true,
        "call_expression" => is_path_arithmetic_shape(node, source),
        _ => false,
    }
}

/// `<identifier>.join(...)` / `<identifier>.resolve(...)`, whatever the
/// receiver is bound to.
pub fn is_path_arithmetic_shape(node: Node, source: &str) -> bool {
    let Some(callee) =
        node.child_by_field_name("function").filter(|callee| callee.kind() == "member_expression")
    else {
        return false;
    };
    let object = callee.child_by_field_name("object");
    let property = callee.child_by_field_name("property");
    object.is_some_and(|object| object.kind() == "identifier")
        && property.is_some_and(|property| {
            PATH_JOIN_MEMBERS.contains(&crate::extractor::syntax::text(property, source))
        })
}

/// `require("path")` / `require("node:path")` as an initializer.
pub fn is_path_module_require(value: Node, source: &str) -> bool {
    if value.kind() != "call_expression" {
        return false;
    }
    let callee = value.child_by_field_name("function");
    if !callee.is_some_and(|callee| {
        callee.kind() == "identifier" && crate::extractor::syntax::text(callee, source) == "require"
    }) {
        return false;
    }
    let argument =
        value.child_by_field_name("arguments").and_then(|args| named_children(args).first().copied());
    argument
        .and_then(|argument| string_literal_value(argument, source))
        .is_some_and(|specifier| PATH_MODULE_SPECIFIERS.contains(&specifier.as_str()))
}

/// POSIX `path.join` over relative segments: empty segments dropped, the
/// rest joined with `/` and normalized (`.` dropped, `..` folded into its
/// parent where there is one, a trailing `/` kept). Joins to `.` when
/// nothing is left.
fn posix_join(segments: &[String]) -> String {
    let joined = segments.iter().filter(|segment| !segment.is_empty()).cloned().collect::<Vec<_>>().join("/");
    if joined.is_empty() {
        return ".".to_string();
    }
    let trailing_separator = joined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let mut normalized = parts.join("/");
    if normalized.is_empty() {
        normalized.push('.');
    }
    if trailing_separator {
        normalized.push('/');
    }
    normalized
}

#[cfg(test)]
mod tests;
