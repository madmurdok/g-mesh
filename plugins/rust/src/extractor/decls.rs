//! The declaration pass: every node this file contributes to the graph, and
//! everything a `use` item says.
//!
//! # What is a node, and what it is called
//!
//! | Rust | `kind` | `nativeKind` | name within its module |
//! |---|---|---|---|
//! | `fn f` | `Function` | `function` | `f` |
//! | `struct`/`enum`/`union` `T` | `Type` | `struct`/`enum`/`union` | `T` |
//! | `type A = …` | `Type` | `type_alias` | `A` |
//! | `trait Tr` | `Type` | `trait` | `Tr` |
//! | `const C` / `static S` | `Variable` | `const` / `static` | `C` / `S` |
//! | `macro_rules! m` | `Function` | `macro` | `m` |
//! | `mod m` | `Module` | `module` | `m` |
//! | `impl T { fn m }` | `Function` | `method` | `T::m` |
//! | `impl Tr for T { fn m }` | `Function` | `trait_impl_method` | `<T as Tr>::m` |
//! | `trait Tr { fn m }` | `Function` | `trait_method` | `Tr::m` |
//! | associated `const`/`type` | `Variable`/`Type` | `assoc_*` / `trait_impl_*` / `trait_const` | as above |
//!
//! A `macro_rules!` is a `Function` because that is the kind core's linker
//! demands of a `CALLS` target, and `m!()` is a call in every sense a caller
//! cares about. Struct fields and enum variants are *not* nodes: the design
//! doc's "Member-level privacy is not modelled" applies to the members
//! themselves, and nothing in the tool surface addresses one.
//!
//! `qualifiedName` prefixes each of those with the module path
//! ([`keys`](super::keys), Decision 2), and the id is derived from it, so two
//! inline modules in one file cannot collide.
//!
//! # The `mod` item is a member of the module that declares it
//!
//! This is the one emission rule that exists for core's sake rather than for
//! Rust's. `graph::containers::parent_chain` walks `containers.parentKey`,
//! and a container only has a row once it has a member - so a module holding
//! nothing but submodules would be a *gap* in the chain, and the walk stops
//! at a gap. `pub(crate)` is a link across that whole chain, so one gap
//! anywhere between a caller and the crate root silently unlinks it.
//!
//! Emitting `mod child;` as a member of its declaring module closes that by
//! construction: a module with submodules always has at least one member,
//! and `containerParent` is sent on every member, so every container in the
//! chain has a row and a parent. That requirement is written down in
//! `parent_chain`'s own doc as a requirement *on this plugin*; this is where
//! it is met. `crate::project` (GM-285) already meets the other half by
//! registering a crate root as a container with its root file as a member.
//!
//! # `use`: four shapes, four different things
//!
//! ```rust,ignore
//! use a::b::C;          // a `pending_symbol` placeholder in container a::b
//! use a::b::C as D;     // the same placeholder; `D` is what this module calls it
//! use a::b::*;          // a container import, and nothing to name
//! pub use a::b::C;      // a `reexport`: this module publishes `C`
//! ```
//!
//! Every one of them also emits an `IMPORTS` edge from the file onto the
//! container `a::b` - not only the glob, which is all the design doc's
//! sketch listed. A `use` *is* an import in Rust, and `get_dependencies` is
//! answered from `IMPORTS` edges alone: listing only globs would make it
//! answer "this file depends on nothing" for essentially every Rust file.
//! The placeholder and the import are independent addresses onto the same
//! module, exactly as the TS plugin emits both a `resolved_module` for the
//! specifier and a `pending_symbol` for each imported name.
//!
//! A `use` rooted at a crate this project does not model becomes an
//! `external_module` node, the same shape the TS plugin gives `import
//! "react"`, and the name it binds is remembered as external so that a later
//! `Serialize::…` is not addressed at this project's own modules.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, TargetKey, Visibility};
use g_mesh_plugin_sdk::{NodeSpec, PlaceholderKind};
use tree_sitter::Node;

use crate::extractor::emit::{container_target, Emitter};
use crate::extractor::keys::{
    is_public, resolve_module_path, visibility, visibility_modifier, ModuleCtx, PathTarget,
};
use crate::extractor::model::{DeclRef, FileModel, Import};
use crate::extractor::syntax::{
    collapse_whitespace, flatten_path, inner_doc_comment, item_name, outer_doc_comment, path_tail, signature,
    text, Seg,
};
use crate::project::ProjectContext;

/// Which block a declaration sits in, which is what decides its `nativeKind`
/// and how its name is prefixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// Directly in a module.
    Module,
    /// In `impl T { … }`.
    Inherent,
    /// In `impl Tr for T { … }`.
    TraitImpl,
    /// In `trait Tr { … }`.
    TraitDecl,
}

/// The block a walk is currently inside, for the members that hang off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockCtx {
    /// What a member's name is prefixed with: `T`, `<T as Tr>`, or `Tr`.
    pub(crate) prefix: String,
    /// The self type's base name, for `Self::m()` and `self.m()`.
    pub(crate) self_type: String,
    /// The trait impl's own prefix, so `self.m()` inside `impl Tr for T` can
    /// look for `<T as Tr>::m` before falling back to the inherent `T::m`.
    pub(crate) trait_prefix: Option<String>,
    pub(crate) family: Family,
    /// The visibility members take when they cannot state their own: a
    /// trait's items are as visible as the trait, and a trait impl's are
    /// always public (Rust forbids a modifier on either).
    pub(crate) inherited: Option<Visibility>,
}

impl BlockCtx {
    /// A member of this block's full name within its module.
    pub(crate) fn tail(&self, name: &str) -> String {
        format!("{}::{}", self.prefix, name)
    }
}

/// A member's full name within its module, whether or not it is in a block.
pub(crate) fn member_tail(block: Option<&BlockCtx>, name: &str) -> String {
    match block {
        Some(block) => block.tail(name),
        None => name.to_string(),
    }
}

/// The `kind`/`nativeKind` pair an item takes in a given block, or `None` for
/// a node kind this plugin does not declare (a `use`, an attribute, a
/// comment, an `enum_variant`).
fn node_kinds(family: Family, item: &str) -> Option<(NodeKind, &'static str)> {
    let function = |native| Some((NodeKind::Function, native));
    let value = |native| Some((NodeKind::Variable, native));
    let ty = |native| Some((NodeKind::Type, native));
    match item {
        "function_item" | "function_signature_item" => match family {
            Family::Module => function("function"),
            Family::Inherent => function("method"),
            Family::TraitImpl => function("trait_impl_method"),
            Family::TraitDecl => function("trait_method"),
        },
        "const_item" => match family {
            Family::Module => value("const"),
            Family::Inherent => value("assoc_const"),
            Family::TraitImpl => value("trait_impl_const"),
            Family::TraitDecl => value("trait_const"),
        },
        "static_item" => value("static"),
        "type_item" | "associated_type" => match family {
            Family::Module => ty("type_alias"),
            Family::Inherent | Family::TraitDecl => ty("assoc_type"),
            Family::TraitImpl => ty("trait_impl_type"),
        },
        "struct_item" => ty("struct"),
        "enum_item" => ty("enum"),
        "union_item" => ty("union"),
        "trait_item" => ty("trait"),
        "macro_definition" => function("macro"),
        "mod_item" => Some((NodeKind::Module, "module")),
        _ => None,
    }
}

/// The name an `impl`'s self type or trait clause goes by.
///
/// A plain path gives its last segment (`fmt::Display` is `Display`,
/// `Point<T>` is `Point`). Anything else - `&T`, `(A, B)`, `[u8; 4]`, `dyn
/// Trait` - has no such segment, and rather than dropping the block's
/// members the rendered type text stands in: `impl Trait for &u8`'s methods
/// are `<&u8 as Trait>::m`. Unlovely, unique, and stable under
/// reformatting, which is all a `qualifiedName` has to be.
pub(crate) fn type_name(node: Node, source: &str) -> String {
    flatten_path(node, source)
        .as_deref()
        .and_then(path_tail)
        .map(str::to_string)
        .unwrap_or_else(|| collapse_whitespace(text(node, source)))
}

/// The block an `impl_item` opens.
pub(crate) fn impl_block(item: Node, source: &str) -> BlockCtx {
    let self_type = item.child_by_field_name("type").map(|node| type_name(node, source)).unwrap_or_default();
    match item.child_by_field_name("trait") {
        Some(clause) => {
            let trait_name = type_name(clause, source);
            let prefix = format!("<{self_type} as {trait_name}>");
            BlockCtx {
                trait_prefix: Some(prefix.clone()),
                prefix,
                self_type,
                family: Family::TraitImpl,
                // Rust forbids a visibility modifier on a trait impl's
                // items: they are reachable wherever the trait and the type
                // both are, which is what `public` means here.
                inherited: Some(Visibility::Public),
            }
        }
        None => BlockCtx {
            prefix: self_type.clone(),
            self_type,
            trait_prefix: None,
            family: Family::Inherent,
            inherited: None,
        },
    }
}

/// The block a `trait_item` opens. `Self` inside a default method is the
/// trait itself, so the self type and the prefix are the same name.
pub(crate) fn trait_block(item: Node, source: &str, own: &Visibility) -> BlockCtx {
    let name = item_name(item, source).unwrap_or_default().to_string();
    BlockCtx {
        prefix: name.clone(),
        self_type: name,
        trait_prefix: None,
        family: Family::TraitDecl,
        inherited: Some(own.clone()),
    }
}

/// Walks a file's items, emitting every declaration and every `use`.
pub(crate) struct Declarer<'a, 's> {
    pub(crate) project: &'a ProjectContext,
    pub(crate) source: &'s str,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) model: &'a mut FileModel,
}

impl Declarer<'_, '_> {
    /// Records every `mod` item in the file, at every nesting depth, before
    /// anything else runs.
    ///
    /// `use self::helpers::run;` may sit above `mod helpers;`, and Rust does
    /// not care - items in a module are mutually visible whatever their
    /// order. Resolving that `use` needs to know `helpers` is a child module,
    /// so the child modules are learned first and everything else second.
    pub(crate) fn collect_modules(&mut self, list: Node, module: &ModuleCtx) {
        let mut cursor = list.walk();
        for item in list.named_children(&mut cursor) {
            if item.kind() != "mod_item" {
                continue;
            }
            let Some(name) = item_name(item, self.source) else { continue };
            self.model.child_module(&module.key, name);
            if let Some(body) = item.child_by_field_name("body") {
                self.collect_modules(body, &module.child(name));
            }
        }
    }

    /// Walks one item list - a file's top level, a module body, or an
    /// `impl`/`trait` body.
    pub(crate) fn collect(&mut self, list: Node, module: &ModuleCtx, block: Option<&BlockCtx>) {
        let mut cursor = list.walk();
        for item in list.named_children(&mut cursor) {
            self.item(item, module, block);
        }
    }

    fn item(&mut self, item: Node, module: &ModuleCtx, block: Option<&BlockCtx>) {
        match item.kind() {
            "use_declaration" => self.use_declaration(item, module),
            "extern_crate_declaration" => self.extern_crate(item, module),
            "mod_item" => self.mod_item(item, module),
            "impl_item" => {
                let block = impl_block(item, self.source);
                if let Some(body) = item.child_by_field_name("body") {
                    self.collect(body, module, Some(&block));
                }
            }
            "trait_item" => {
                let own = visibility(item, module, self.source);
                let Some(id) = self.declare(item, module, None, NodeKind::Type, "trait", own.clone()) else {
                    return;
                };
                let _ = id;
                let block = trait_block(item, self.source, &own);
                if let Some(body) = item.child_by_field_name("body") {
                    self.collect(body, module, Some(&block));
                }
            }
            // `extern "C" { … }`: its items are ordinary module-level
            // declarations that happen to have no body.
            "foreign_mod_item" => {
                if let Some(body) = item.child_by_field_name("body") {
                    self.collect(body, module, block);
                }
            }
            kind => {
                let family = block.map_or(Family::Module, |block| block.family);
                let Some((node_kind, native_kind)) = node_kinds(family, kind) else { return };
                let own = block
                    .and_then(|block| block.inherited.clone())
                    .unwrap_or_else(|| self.item_visibility(item, module));
                self.declare(item, module, block, node_kind, native_kind, own);
            }
        }
    }

    /// An item's own visibility, with the one exception Rust spells as an
    /// attribute rather than a modifier: `#[macro_export]` publishes a
    /// `macro_rules!` at the crate root, which is as close to `pub` as a
    /// macro gets.
    fn item_visibility(&self, item: Node, module: &ModuleCtx) -> Visibility {
        if item.kind() == "macro_definition" && self.has_attribute(item, "macro_export") {
            return Visibility::Public;
        }
        visibility(item, module, self.source)
    }

    fn has_attribute(&self, item: Node, name: &str) -> bool {
        let mut sibling = item.prev_sibling();
        while let Some(node) = sibling {
            match node.kind() {
                "attribute_item" => {
                    if text(node, self.source).contains(name) {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => return false,
            }
            sibling = node.prev_sibling();
        }
        false
    }

    /// Emits one declaration node and records it in the model.
    fn declare(
        &mut self,
        item: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        kind: NodeKind,
        native_kind: &str,
        own: Visibility,
    ) -> Option<String> {
        let name = item_name(item, self.source)?.to_string();
        let tail = member_tail(block, &name);
        let mut spec =
            NodeSpec::new(kind, name.clone(), module.qualified(&tail), self.emitter.positions().range(item))
                .native_kind(native_kind)
                .visibility(own.clone())
                .in_container(module.key.clone(), module.parent.clone());
        spec.signature = signature(item, self.source);
        spec.doc_comment = outer_doc_comment(item, self.source);
        let id = self.emitter.declare(spec, is_public(&own));
        self.model.declare(&module.key, &name, &tail, DeclRef { id: id.clone(), kind });
        Some(id)
    }

    /// `mod child;` and `mod child { … }`: a member of the module that
    /// declares it (see this module's doc), plus, for an inline one, the
    /// whole child module walked under its own container key.
    fn mod_item(&mut self, item: Node, module: &ModuleCtx) {
        let Some(name) = item_name(item, self.source).map(str::to_string) else { return };
        let own = visibility(item, module, self.source);
        let child = module.child(&name);
        let mut spec = NodeSpec::new(
            NodeKind::Module,
            name.clone(),
            module.qualified(&name),
            self.emitter.positions().range(item),
        )
        .native_kind("module")
        .visibility(own.clone())
        .in_container(module.key.clone(), module.parent.clone());
        spec.signature = signature(item, self.source);
        // An inline module's own `//!` header documents the module; a
        // file-backed one's lives in the file it names, and is that file's
        // `File` node's doc.
        spec.doc_comment = outer_doc_comment(item, self.source).or_else(|| {
            item.child_by_field_name("body").and_then(|body| inner_doc_comment(body, self.source))
        });
        let id = self.emitter.declare(spec, is_public(&own));
        self.model.declare(&module.key, &name, &name, DeclRef { id, kind: NodeKind::Module });
        if let Some(body) = item.child_by_field_name("body") {
            self.collect(body, &child, None);
        }
    }

    /// `extern crate foo;` - the 2015-edition way of naming a dependency,
    /// still written for `alloc`/`test` and in macro-exporting crates.
    fn extern_crate(&mut self, item: Node, module: &ModuleCtx) {
        let Some(name) = item_name(item, self.source) else { return };
        let range = self.emitter.positions().range(item);
        let target = resolve_module_path(&[Seg::Name(name)], module, self.model, self.project);
        self.import_edge(target, name, range);
    }

    fn use_declaration(&mut self, item: Node, module: &ModuleCtx) {
        let Some(argument) = item.child_by_field_name("argument") else { return };
        // Any restriction still re-exports: `graph::symbol_links` documents
        // that a re-export's own visibility is not checked, and the
        // declaration at the end of the chain is checked against the original
        // requester anyway.
        let republishes = visibility_modifier(item).is_some();
        let mut leaves = Vec::new();
        collect_use_leaves(argument, &[], &mut leaves, self.source);
        for leaf in leaves {
            self.use_leaf(&leaf, module, republishes);
        }
    }

    fn use_leaf(&mut self, leaf: &UseLeaf<'_>, module: &ModuleCtx, republishes: bool) {
        let range = self.emitter.positions().range(leaf.node);
        // A leaf with no prefix is a bare `use some_crate;` - the path is the
        // name itself, and there is nothing inside it to place.
        if leaf.prefix.is_empty() {
            if let LeafKind::Named { name, .. } = leaf.kind {
                let target = resolve_module_path(&[Seg::Name(name)], module, self.model, self.project);
                self.import_edge(target, name, range);
            }
            return;
        }

        let target = resolve_module_path(&leaf.prefix, module, self.model, self.project);
        let container = match &target {
            PathTarget::Container(container) => container.clone(),
            PathTarget::ExternalCrate(krate) => {
                let krate = krate.clone();
                self.import_edge(target, &krate, range);
                if let LeafKind::Named { name, alias } = leaf.kind {
                    self.model.import(&module.key, alias.unwrap_or(name), Import::External);
                }
                return;
            }
            PathTarget::Unresolved => return,
        };
        self.import_edge(PathTarget::Container(container.clone()), "", range);

        match leaf.kind {
            LeafKind::Named { name, alias } => {
                let placeholder = self.emitter.placeholder(
                    PlaceholderKind::PendingSymbol,
                    name,
                    container_target(&container, TargetKey::Name(name.to_string()), &module.key),
                    range,
                );
                // The file, not a symbol, is what imports a name - so this
                // shows up in `find_references` as a whole-file row, which is
                // the granularity a `use` line genuinely has.
                let file = self.emitter.file_id().to_string();
                self.emitter.placeholder_edge(EdgeKind::References, &file, &placeholder);
                self.model.import(
                    &module.key,
                    alias.unwrap_or(name),
                    Import::Item { container: container.clone(), name: name.to_string() },
                );
                if republishes {
                    self.reexport(alias.unwrap_or(name), &container, name, module, range);
                }
            }
            LeafKind::Glob => {
                #[cfg(test)]
                crate::census::note_glob(&module.key);
                if republishes {
                    // `*` at both ends: core's own spelling for "this scope
                    // republishes everything that one does".
                    self.reexport("*", &container, "*", module, range);
                }
            }
        }
    }

    /// A `pub use`: a `reexport` node carrying what this module *publishes*
    /// (its `name`) and what that really is (its target).
    fn reexport(
        &mut self,
        published: &str,
        container: &str,
        name: &str,
        module: &ModuleCtx,
        range: g_mesh_plugin_sdk::wire::Range,
    ) {
        self.emitter.reexport(
            published,
            container_target(container, TargetKey::Name(name.to_string()), &module.key),
            &module.key,
            range,
        );
    }

    /// The `IMPORTS` edge from this file onto whatever a `use` prefix named.
    fn import_edge(&mut self, target: PathTarget, name: &str, range: g_mesh_plugin_sdk::wire::Range) {
        let file = self.emitter.file_id().to_string();
        let to = match target {
            PathTarget::Container(container) => self.emitter.placeholder(
                PlaceholderKind::ResolvedModule,
                container.rsplit("::").next().unwrap_or(&container),
                container_target(&container, TargetKey::Name("*".to_string()), &container),
                range,
            ),
            PathTarget::ExternalCrate(krate) => self.emitter.external_module(&krate, range),
            PathTarget::Unresolved => {
                let _ = name;
                return;
            }
        };
        self.emitter.placeholder_edge(EdgeKind::Imports, &file, &to);
    }
}

/// One leaf of a `use` tree, flattened out of however many nested groups
/// produced it.
#[derive(Debug)]
struct UseLeaf<'s> {
    /// The module path in front of the leaf.
    prefix: Vec<Seg<'s>>,
    kind: LeafKind<'s>,
    /// The syntax the leaf was written at, for the placeholder's range - a
    /// placeholder belongs to the file that is *waiting*, so its range is the
    /// use site.
    node: Node<'s>,
}

#[derive(Debug, Clone, Copy)]
enum LeafKind<'s> {
    Named { name: &'s str, alias: Option<&'s str> },
    Glob,
}

/// Flattens a `use` argument - `a::b::{C, d::{E as F, self}, *}` - into one
/// leaf per name it actually binds.
///
/// Recursive rather than iterative because the grammar is: a `use_list` may
/// hold another `scoped_use_list`, to any depth. `prefix` is threaded through
/// by value at each branch so that sibling groups do not inherit each other's
/// path.
fn collect_use_leaves<'s>(node: Node<'s>, prefix: &[Seg<'s>], out: &mut Vec<UseLeaf<'s>>, source: &'s str) {
    match node.kind() {
        "scoped_use_list" => {
            let Some(list) = node.child_by_field_name("list") else { return };
            let mut extended = prefix.to_vec();
            if let Some(path) = node.child_by_field_name("path") {
                let Some(segments) = flatten_path(path, source) else { return };
                extended.extend(segments);
            }
            collect_use_leaves(list, &extended, out, source);
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_use_leaves(child, prefix, out, source);
            }
        }
        "use_as_clause" => {
            let Some(path) = node.child_by_field_name("path") else { return };
            let alias = node.child_by_field_name("alias").map(|node| text(node, source));
            push_path_leaf(path, prefix.to_vec(), alias, node, out, source);
        }
        "use_wildcard" => {
            let mut extended = prefix.to_vec();
            // `use a::b::*` carries its path as an unnamed child; `use *`
            // (inside a group: `a::{*}`) carries none at all.
            let mut cursor = node.walk();
            if let Some(path) = node.named_children(&mut cursor).next() {
                let Some(segments) = flatten_path(path, source) else { return };
                extended.extend(segments);
            }
            out.push(UseLeaf { prefix: extended, kind: LeafKind::Glob, node });
        }
        _ => push_path_leaf(node, prefix.to_vec(), None, node, out, source),
    }
}

/// Turns a path leaf into a `(prefix, name)` pair: everything but the last
/// segment is prefix, and the last segment is the name being bound.
///
/// `use a::b::{self}` - a leaf that is the keyword `self` - names the module
/// `b` itself, so the prefix gives up its own last segment to become the
/// name.
fn push_path_leaf<'s>(
    path: Node<'s>,
    mut prefix: Vec<Seg<'s>>,
    alias: Option<&'s str>,
    node: Node<'s>,
    out: &mut Vec<UseLeaf<'s>>,
    source: &'s str,
) {
    let Some(segments) = flatten_path(path, source) else { return };
    let Some((last, rest)) = segments.split_last() else { return };
    prefix.extend_from_slice(rest);
    let name = match last {
        Seg::Name(name) => *name,
        Seg::SelfMod => match prefix.pop() {
            Some(Seg::Name(name)) => name,
            // `use self;`, or `use crate::{self}` - the crate root itself,
            // which no name in this module addresses.
            _ => return,
        },
        Seg::Crate | Seg::Super => return,
    };
    out.push(UseLeaf { prefix, kind: LeafKind::Named { name, alias }, node });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        parser.parse(source, None).unwrap()
    }

    fn leaves(source: &'static str) -> Vec<(Vec<String>, String)> {
        let tree = parse(source);
        let use_item = tree.root_node().named_child(0).expect("a use item");
        let argument = use_item.child_by_field_name("argument").expect("an argument");
        let mut out = Vec::new();
        collect_use_leaves(argument, &[], &mut out, source);
        out.into_iter()
            .map(|leaf| {
                let prefix = leaf
                    .prefix
                    .iter()
                    .map(|segment| match segment {
                        Seg::Crate => "crate".to_string(),
                        Seg::SelfMod => "self".to_string(),
                        Seg::Super => "super".to_string(),
                        Seg::Name(name) => (*name).to_string(),
                    })
                    .collect();
                let leaf = match leaf.kind {
                    LeafKind::Named { name, alias } => match alias {
                        Some(alias) => format!("{name} as {alias}"),
                        None => name.to_string(),
                    },
                    LeafKind::Glob => "*".to_string(),
                };
                (prefix, leaf)
            })
            .collect()
    }

    #[test]
    fn a_nested_use_group_flattens_to_one_leaf_per_bound_name() {
        assert_eq!(
            leaves("use crate::a::{B, c::{D as E, self}, f::*};"),
            vec![
                (vec!["crate".into(), "a".into()], "B".into()),
                (vec!["crate".into(), "a".into(), "c".into()], "D as E".into()),
                (vec!["crate".into(), "a".into()], "c".into()),
                (vec!["crate".into(), "a".into(), "f".into()], "*".into()),
            ]
        );
    }

    #[test]
    fn a_plain_use_and_a_glob_carry_their_whole_prefix() {
        assert_eq!(
            leaves("use std::fmt::Display;"),
            vec![(vec!["std".into(), "fmt".into()], "Display".into())]
        );
        assert_eq!(leaves("use super::g::*;"), vec![(vec!["super".into(), "g".into()], "*".into())]);
        assert_eq!(leaves("use some_crate;"), vec![(Vec::<String>::new(), "some_crate".into())]);
    }

    #[test]
    fn a_trait_impls_members_are_named_by_the_trait_as_well_as_the_type() {
        let source = "impl fmt::Display for Point<u8> { fn fmt() {} }";
        let tree = parse(source);
        let item = tree.root_node().named_child(0).unwrap();
        let block = impl_block(item, source);
        assert_eq!(block.tail("fmt"), "<Point as Display>::fmt");
        assert_eq!(block.self_type, "Point");
        assert_eq!(block.family, Family::TraitImpl);
    }

    #[test]
    fn an_inherent_impls_members_are_named_by_the_type_alone() {
        let source = "impl<T> Point<T> { fn new() {} }";
        let tree = parse(source);
        let item = tree.root_node().named_child(0).unwrap();
        let block = impl_block(item, source);
        assert_eq!(block.tail("new"), "Point::new");
        assert_eq!(block.family, Family::Inherent);
        assert_eq!(block.trait_prefix, None);
    }

    /// An `impl` whose self type is not a path still names its methods -
    /// see [`type_name`].
    #[test]
    fn an_impl_on_a_non_path_type_falls_back_to_the_rendered_type() {
        let source = "impl Trait for &  u8 { fn m() {} }";
        let tree = parse(source);
        let item = tree.root_node().named_child(0).unwrap();
        // Whitespace runs collapse to one space rather than vanishing, which
        // is what makes the name invariant under reformatting: `&  u8` and
        // `& u8` are one name, and so are `&u8` and `&u8`.
        assert_eq!(impl_block(item, source).tail("m"), "<& u8 as Trait>::m");
    }
}
