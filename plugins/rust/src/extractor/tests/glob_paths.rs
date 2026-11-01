//! In a module with a glob `use`, a path whose first segment names
//! no workspace crate, declared `mod` or named import of the module
//! (`use super::*;` then `schema::f()`, the parent having imported `schema`)
//! is an open site for the semantic tier, unless that segment is a sysroot
//! crate, a primitive, a name a `use` of the module binds, or a Cargo
//! dependency of the crate's package. Without a glob it stays another
//! crate's path.
//!
//! Its own module, so these tests stay apart from the parent file's; the
//! helpers are the parent's.

use g_mesh_plugin_sdk::wire::EdgeKind;
use g_mesh_plugin_sdk::OpenSiteKind;

use super::{open_sites_from, Crate, Graph};

/// `src/storage/schema.rs`: a free fn and a type with an associated fn.
const SCHEMA: &str = "pub fn set_facts(x: u32) -> u32 { x }\n\
                      pub struct Row;\nimpl Row { pub fn new() -> Self { Row } }\n";

/// A crate whose `daemon` module imports `schema` by name, with `daemon`'s
/// own source `daemon` and, when given, its file-backed child `tests`.
fn glob_crate(manifest: Option<&str>, daemon: &str, tests: Option<&str>) -> Crate {
    let mut files = vec![
        ("src/lib.rs", "pub mod daemon;\npub mod storage;\n"),
        ("src/storage.rs", "pub mod schema;\n"),
        ("src/storage/schema.rs", SCHEMA),
        ("src/daemon.rs", daemon),
    ];
    if let Some(manifest) = manifest {
        files.push(("Cargo.toml", manifest));
    }
    if let Some(tests) = tests {
        files.push(("src/daemon/tests.rs", tests));
    }
    Crate::new(&files)
}

/// Asserts `from`'s call written `name` (the whole path) is an open path
/// question replacing nothing, and that no `CALLS` edge leaves `from`.
fn assert_open_glob_call(graph: &Graph, from: &str, name: &str) {
    let sites = open_sites_from(graph, from, name);
    assert_eq!(sites.len(), 1, "{from} -> {name}: {:#?}", graph.0.open_sites);
    assert_eq!(sites[0].kind, OpenSiteKind::Reference, "{:#?}", sites[0]);
    assert_eq!(sites[0].edge_kind, EdgeKind::Calls, "{:#?}", sites[0]);
    assert_eq!(sites[0].replaces, None, "no structural edge to replace: {:#?}", sites[0]);
    assert_eq!(graph.targets(EdgeKind::Calls, from), Vec::<String>::new(), "no edge out of {from}");
}

/// Asserts `from` makes no `CALLS` edge and opens no path question (its
/// `.ok()` receiver calls aside).
fn assert_nothing_from(graph: &Graph, from: &str) {
    let id = graph.node(from).id.clone();
    let sites: Vec<_> = graph
        .0
        .open_sites
        .iter()
        .filter(|site| site.from_id == id && site.kind == OpenSiteKind::Reference)
        .collect();
    assert!(sites.is_empty(), "{from} must open no path site: {sites:#?}");
    assert_eq!(graph.targets(EdgeKind::Calls, from), Vec::<String>::new(), "no edge out of {from}");
}

/// Behaviours 1 and 3: in an inline module whose only way to `schema` is
/// `use super::*;`, `schema::set_facts(2)` and `schema::Row::new()` are
/// each an open site. The parent's own call through its named `use` keeps
/// its placeholder edge.
///
/// Control: in `Bodies::resolve_path`, drop the `ExternalCrate(first) if
/// glob_may_bind` arm (back to `Bound::Nothing`) - `via_glob` opens nothing;
/// in `Bodies::resolve_type_qualified`, drop its guarded arm - `via_type`
/// opens nothing.
#[test]
fn a_glob_reached_module_path_in_an_inline_module_is_an_open_site() {
    let krate = glob_crate(
        None,
        "use crate::storage::schema;\n\
         pub fn direct() -> u32 { schema::set_facts(1) }\n\
         pub mod inline {\n    use super::*;\n    \
         pub fn via_glob() -> u32 { schema::set_facts(2) }\n    \
         pub fn via_type() { schema::Row::new(); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    assert_open_glob_call(&graph, "daemon::inline::via_glob", "schema::set_facts");
    assert_open_glob_call(&graph, "daemon::inline::via_type", "schema::Row::new");
    assert_eq!(
        graph.targets(EdgeKind::Calls, "daemon::direct"),
        vec!["pending_symbol krate::storage::schema::set_facts"],
        "the named import still resolves structurally"
    );
    assert!(open_sites_from(&graph, "daemon::direct", "schema::set_facts").is_empty());
}

/// Behaviour 2: the same in a file-backed child (`mod tests;` beside the
/// parent's `use crate::storage::schema;`, `tests.rs` opening with `use
/// super::*;`).
///
/// Control: as above, the `resolve_path` arm - `via_file` opens nothing.
#[test]
fn a_glob_reached_module_path_in_a_file_backed_child_is_an_open_site() {
    let krate = glob_crate(
        None,
        "use crate::storage::schema;\n#[cfg(test)]\nmod tests;\n",
        Some("use super::*;\nfn via_file() -> u32 { schema::set_facts(4) }\nfn via_type() { schema::Row::new(); }\n"),
    );
    let graph = krate.extract("src/daemon/tests.rs");
    assert_open_glob_call(&graph, "daemon::tests::via_file", "schema::set_facts");
    assert_open_glob_call(&graph, "daemon::tests::via_type", "schema::Row::new");
}

/// Behaviour 4: a path rooted at a sysroot crate or a primitive type is
/// never a project item, glob or not.
///
/// Control: empty `NEVER_A_PROJECT_ITEM` - each fn opens a site.
#[test]
fn sysroot_and_primitive_roots_in_a_glob_module_open_nothing() {
    let krate = glob_crate(
        None,
        "use crate::storage::schema;\n\
         pub mod inline {\n    use super::*;\n    \
         pub fn on_std() { std::mem::drop(1); }\n    \
         pub fn on_core() { core::mem::drop(1); }\n    \
         pub fn on_alloc() { alloc::vec::Vec::<u8>::new(); }\n    \
         pub fn on_u32() { u32::from_str_radix(\"1\", 2).ok(); }\n    \
         pub fn on_str() { str::from_utf8(b\"x\").ok(); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    for from in ["on_std", "on_core", "on_alloc", "on_u32", "on_str"] {
        assert_nothing_from(&graph, &format!("daemon::inline::{from}"));
    }
}

/// Behaviour 5: a root that is a Cargo dependency of the crate's package -
/// from `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` or
/// a `[target.*]` table, its key read with `-` as `_`, a `package =` rename
/// by its key - opens nothing. The renamed dependency's *package* name is
/// not a name the source can write, so `real_name::f()` is still asked.
///
/// Control: make `Bodies::glob_may_bind` ignore
/// `ProjectContext::is_dependency` - every dependency fn opens a site.
#[test]
fn a_cargo_dependency_root_in_a_glob_module_opens_nothing() {
    let krate = glob_crate(
        Some(
            "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\ntempfile = \"3\"\nalias = { package = \"real-name\", version = \"1\" }\n\n\
             [dev-dependencies]\nmy-dev-dep = \"1\"\n\n\
             [build-dependencies]\nbuild_dep = \"1\"\n\n\
             [target.'cfg(unix)'.dependencies]\nunix_dep = \"1\"\n",
        ),
        "use crate::storage::schema;\n\
         pub mod inline {\n    use super::*;\n    \
         pub fn on_dep() { tempfile::tempdir(); }\n    \
         pub fn on_dev() { my_dev_dep::f(); }\n    \
         pub fn on_build() { build_dep::f(); }\n    \
         pub fn on_target() { unix_dep::f(); }\n    \
         pub fn on_alias() { alias::f(); }\n    \
         pub fn on_package_name() { real_name::f(); }\n    \
         pub fn on_project() { schema::set_facts(2); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    for from in ["on_dep", "on_dev", "on_build", "on_target", "on_alias"] {
        assert_nothing_from(&graph, &format!("daemon::inline::{from}"));
    }
    assert_open_glob_call(&graph, "daemon::inline::on_package_name", "real_name::f");
    assert_open_glob_call(&graph, "daemon::inline::on_project", "schema::set_facts");
}

/// Behaviour 6: a root a `use` of the glob module itself binds to another
/// crate's module (`use regex::bytes;` then `bytes::Regex::new()`) is that
/// crate's, not a glob's.
///
/// Control: drop the `lookup_import(..).is_none()` term from
/// `Bodies::glob_may_bind` - `on_imported_module` opens a site.
#[test]
fn a_root_the_glob_module_imports_by_name_opens_nothing() {
    let krate = glob_crate(
        None,
        "use crate::storage::schema;\n\
         pub mod inline {\n    use super::*;\n    use regex::bytes;\n    \
         pub fn on_imported_module() { bytes::Regex::new(\"x\").ok(); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    assert_nothing_from(&graph, "daemon::inline::on_imported_module");
}

/// Behaviour 7: without a glob `use` in the module, an unknown root is
/// another crate's and nothing is asked.
///
/// Control: drop the `has_glob` term from `Bodies::glob_may_bind` -
/// `no_glob` opens a site.
#[test]
fn an_unknown_root_in_a_module_without_a_glob_opens_nothing() {
    let krate = glob_crate(
        None,
        "use crate::storage::schema;\n\
         pub mod inline {\n    pub fn no_glob() { schema::set_facts(2); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    assert_nothing_from(&graph, "daemon::inline::no_glob");
}

/// Behaviour 8: a dependency table of a shape this plugin does not expect
/// (`dependencies = 5`, `target = "x"`) does not stop the crate from being
/// modelled: the file is still `krate::daemon`'s, not an orphan, and the
/// glob-reached call is still asked.
#[test]
fn a_malformed_dependency_table_still_models_the_crate() {
    let krate = glob_crate(
        Some(
            "dependencies = 5\ndev-dependencies = [1]\ntarget = \"x\"\n\n\
             [package]\nname = \"krate\"\nversion = \"0.1.0\"\n",
        ),
        "use crate::storage::schema;\n\
         pub mod inline {\n    use super::*;\n    pub fn via_glob() { schema::set_facts(2); }\n}\n",
        None,
    );
    let graph = krate.extract("src/daemon.rs");
    assert_eq!(graph.node("daemon::inline::via_glob").container.as_deref(), Some("krate::daemon::inline"));
    assert_open_glob_call(&graph, "daemon::inline::via_glob", "schema::set_facts");
}
