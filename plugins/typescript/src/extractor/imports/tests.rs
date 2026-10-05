//! The names an import binds. They are read back only by uses of those names,
//! so these tests drive the import statements directly and ask the
//! [`Declarer`] what each local name stands for.

use g_mesh_plugin_sdk::wire::{NodeKind, PlaceholderTarget, Position, Range, TargetKey, TargetScope};
use g_mesh_plugin_sdk::{CharColumns, RelPath};

use super::posix_join;
use crate::extractor::decls::Declarer;
use crate::extractor::grammar;
use crate::extractor::model::{DraftNode, FileModel};
use crate::extractor::syntax::named_children;

const PATH: &str = "src/a.ts";

/// Runs every top-level import statement of `source` through the walk's
/// import handler, resolving `./m` and `./n` to project files, then hands
/// the declarer to `check`.
fn with_imports(source: &str, check: impl FnOnce(&mut Declarer)) {
    let rel = RelPath::new(PATH);
    let resolver = |specifier: &str, _from: &RelPath| match specifier {
        "./m" => Some(RelPath::new("src/m.ts")),
        "./n" => Some(RelPath::new("src/n.ts")),
        _ => None,
    };
    let columns = CharColumns::new(source);
    let tree = grammar::parse(grammar::Grammar::TypeScript, source).expect("a parse");
    let root = tree.root_node();
    let mut model = FileModel::new(PATH, columns.file_range());
    let mut declarer = Declarer::new(source, &columns, &rel, &resolver, &mut model);
    for statement in named_children(root) {
        if statement.kind() == "import_statement" {
            declarer.handle_import(statement);
        }
    }
    check(&mut declarer);
}

/// The `pending_symbol` node a use of `local` would create.
fn symbol<'d>(declarer: &'d mut Declarer, local: &str) -> Option<&'d DraftNode> {
    let index = declarer.imported_symbol(local)?;
    Some(declarer.model.node(index))
}

fn range(line: u32, start_col: u32, end_col: u32) -> Range {
    Range { start: Position { line, col: start_col }, end: Position { line, col: end_col } }
}

fn target(file: &str, name: &str) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: TargetScope::File(file.to_string()),
        key: TargetKey::Name(name.to_string()),
        from_container: None,
        key_path: None,
    }
}

#[test]
fn default_import_binds_the_name_default() {
    with_imports("import D from \"./m\";\n", |declarer| {
        let node = symbol(declarer, "D").expect("D is bound");
        assert_eq!(node.kind, NodeKind::Module);
        assert_eq!(node.native_kind.as_deref(), Some("pending_symbol"));
        assert_eq!(node.name, "default");
        assert_eq!(node.qualified_name, "src/m.ts#default");
        assert_eq!(node.target, Some(target("src/m.ts", "default")));
        assert_eq!(node.range, range(0, 7, 8));
    });
}

#[test]
fn aliased_import_binds_the_alias_to_the_original_name() {
    with_imports("import {\n  x as y,\n  z,\n} from \"./m\";\n", |declarer| {
        let node = symbol(declarer, "y").expect("y is bound").clone();
        assert_eq!(node.name, "x");
        assert_eq!(node.qualified_name, "src/m.ts#x");
        assert_eq!(node.target, Some(target("src/m.ts", "x")));
        // The placeholder spans the local name, the alias.
        assert_eq!(node.range, range(1, 7, 8));
        assert!(symbol(declarer, "x").is_none());
        assert_eq!(symbol(declarer, "z").expect("z is bound").qualified_name, "src/m.ts#z");
    });
}

#[test]
fn first_binding_of_a_local_name_wins() {
    with_imports(
        "import { a } from \"./m\";\nimport { b as a } from \"./n\";\nimport a from \"./n\";\n",
        |declarer| {
            let node = symbol(declarer, "a").expect("a is bound");
            assert_eq!(node.qualified_name, "src/m.ts#a");
            assert_eq!(node.range.start.line, 0);
        },
    );
}

#[test]
fn imports_of_an_unresolved_specifier_bind_nothing() {
    with_imports("import D, { a, b as c } from \"pkg\";\nimport * as NS from \"pkg\";\n", |declarer| {
        for local in ["D", "a", "b", "c", "NS"] {
            assert!(symbol(declarer, local).is_none(), "{local} is bound");
        }
        assert!(declarer.namespace_binding("NS").is_none());
    });
}

#[test]
fn namespace_import_is_a_namespace_binding_not_an_import_binding() {
    with_imports("import * as NS from \"./m\";\nimport * as NS from \"./n\";\n", |declarer| {
        assert!(symbol(declarer, "NS").is_none());
        let binding = declarer.namespace_binding("NS").expect("NS is bound").clone();
        assert_eq!(binding.target_path.as_str(), "src/m.ts");
        assert_eq!(binding.imported_name, "NS");
        assert_eq!(binding.at, range(0, 12, 14));
    });
}

#[test]
fn a_use_of_one_binding_twice_is_one_placeholder() {
    with_imports("import { a } from \"./m\";\n", |declarer| {
        let first = declarer.imported_symbol("a").expect("a is bound");
        let second = declarer.imported_symbol("a").expect("a is bound");
        assert_eq!(first, second);
    });
}

fn join(segments: &[&str]) -> String {
    posix_join(&segments.iter().map(|segment| segment.to_string()).collect::<Vec<_>>())
}

#[test]
fn posix_join_drops_empty_and_dot_segments_and_folds_parents() {
    assert_eq!(join(&["a", "", "./b/../c", "d"]), "a/c/d");
    assert_eq!(join(&["a", "..", "..", "b"]), "../b");
    assert_eq!(join(&["..", "..", "x"]), "../../x");
    assert_eq!(join(&["a//b"]), "a/b");
}

#[test]
fn posix_join_keeps_a_trailing_separator() {
    assert_eq!(join(&["a", "b/"]), "a/b/");
    assert_eq!(join(&["a/b/.."]), "a");
}

#[test]
fn posix_join_of_nothing_is_the_directory_itself() {
    assert_eq!(join(&[]), ".");
    assert_eq!(join(&["", ""]), ".");
    assert_eq!(join(&["a", ".."]), ".");
}
