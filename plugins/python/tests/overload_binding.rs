//! GM-348 B1: a real pyright, behind the SDK's `LspBridge` with the manifest
//! this plugin ships, binds each overloaded call to the `@overload` stub it
//! calls - read straight off the bridge's diff, with no core in between.
//!
//! `plugins/sdk/tests/lsp_bridge.rs` pins every rule of the binding against a
//! scripted server; this file is the one place that proves pyright still
//! answers the way those scripts assume (`definition` names the whole set,
//! `hover` at the call renders the bound stub, `hover` at a stub's own name
//! renders it through the same printer). A pyright upgrade that changes its
//! hover printer fails here, not silently in a user's index.
//!
//! The calls are made from the declaring module *and* from another one: a
//! cross-file call's candidate hovers are asked in a file the pass may never
//! have opened, which the bridge relies on pyright serving from disk.
//!
//! pyright is a test dependency of this crate, as `tests/conformance.rs`'s
//! module doc argues: missing, this test fails naming the remedy rather than
//! skipping.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use g_mesh_plugin_python::extractor::PythonExtractor;
use g_mesh_plugin_python::project::ProjectContext;
use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::wire::WireEdge;
use g_mesh_plugin_sdk::{Extractor, RelPath, SdkIndex, SemanticAnswer, SemanticEngine};

/// The design note's fixture (`docs/architecture/gm-348-bridge-overload-binding.md`
/// section 1), plus a caller in the same module.
const M_PY: &str = r#"from typing import Union, overload


@overload
def f(x: int) -> int: ...
@overload
def f(x: str) -> str: ...
def f(x: Union[int, str]) -> Union[int, str]:
    return x


class C:
    @overload
    def m(self, x: int) -> int: ...
    @overload
    def m(self, x: str) -> str: ...
    def m(self, x):
        return x


def here():
    a = f(1)
    b = f("s")
    c = C().m("s")
    return a, b, c
"#;

/// The same three calls from across a file boundary, plus a receiver typed
/// by a parameter annotation - the shape of `conformance/project`'s
/// `codec.encode("s")`, which no `expect.toml` entry can tell bound from
/// unbound (one call is one edge either way).
const USE_PY: &str = r#"from pkg.m import C, f


def there():
    a = f(1)
    b = f("s")
    c = C().m("s")
    return a, b, c


def through(codec: C):
    return codec.m(1)
"#;

/// A scratch project that removes itself.
struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-py-overloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, contents) in [("pkg/__init__.py", ""), ("pkg/m.py", M_PY), ("pkg/use.py", USE_PY)] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        Project(root.canonicalize().unwrap())
    }

    /// Every file through the plugin's own extractor, as `run` would index it.
    fn index(&self) -> SdkIndex {
        let project = ProjectContext::load(&self.0).expect("the project model loads");
        let mut index = SdkIndex::new();
        for path in ["pkg/__init__.py", "pkg/m.py", "pkg/use.py"] {
            let source = std::fs::read_to_string(self.0.join(path)).unwrap();
            let path = RelPath::new(path);
            let graph = PythonExtractor.extract(&project, &path, &source);
            index.insert(path, source, graph);
        }
        index
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `plugin.toml`'s `[plugin.semantic]`, with the command pointed at a real
/// pyright-langserver: the shipped `overload_disambiguation = "hover"` is
/// what this test exercises.
fn shipped_config() -> SemanticConfig {
    let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    let mut config = SemanticConfig::from_manifest_at(manifest)
        .expect("plugin.toml parses")
        .expect("plugin.toml has [plugin.semantic]");
    config.command = pyright_langserver();
    config
}

/// The pyright-langserver to run: this crate's `node_modules` first, `PATH`
/// otherwise, each bare and (on Windows) as the `.cmd` npm writes, proved by
/// its `pyright --version` twin - the same lookup `tests/conformance.rs`
/// makes, for the reasons its doc gives.
fn pyright_langserver() -> PathBuf {
    let local = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    let extensions: &[&str] = if cfg!(windows) { &["", ".cmd"] } else { &[""] };
    let mut tried = Vec::new();
    for dir in [Some(local), None] {
        for extension in extensions {
            let at = |bin: &str| match dir {
                Some(dir) => dir.join(format!("{bin}{extension}")),
                None => PathBuf::from(format!("{bin}{extension}")),
            };
            let server = at("pyright-langserver");
            tried.push(server.clone());
            let usable = std::process::Command::new(at("pyright"))
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success());
            if usable {
                return server;
            }
        }
    }
    panic!(
        "this test drives a real pyright and there is none that works (tried {tried:?}). \
         Install it with `scripts/test-deps.sh pyright` from the repository root."
    )
}

/// The edges `answer` sends from the function named `caller`, as
/// `(target name, ordinal)`.
fn semantic_from(answer: &SemanticAnswer, index: &SdkIndex, caller: &str) -> BTreeSet<(String, Option<u32>)> {
    let from = index
        .files()
        .flat_map(|(_, entry)| entry.graph.nodes.iter())
        .find(|node| node.qualified_name == caller)
        .unwrap_or_else(|| panic!("no node {caller}"))
        .id
        .clone();
    let name_of = |edge: &WireEdge| {
        answer
            .diff
            .upsert_nodes
            .iter()
            .find(|node| node.id == edge.to_id)
            .map(|node| node.name.clone())
            .unwrap_or_else(|| panic!("edge {edge:?} lands on a placeholder the answer emits"))
    };
    answer
        .diff
        .upsert_edges
        .iter()
        .filter(|edge| edge.from_id == from && edge.source == g_mesh_plugin_sdk::wire::SourceTier::Semantic)
        .map(|edge| (name_of(edge), edge.to_declaration))
        .collect()
}

/// The structural `CALLS` edge from `caller` the `OverloadCall` sites onto
/// `f` name in `replaces`.
fn structural_onto_f(index: &SdkIndex, file: &str, caller: &str) -> String {
    let graph = index.graph(&RelPath::new(file)).expect("the file is indexed");
    let from = graph.nodes.iter().find(|node| node.qualified_name == caller).expect("the caller").id.clone();
    graph
        .open_sites
        .iter()
        .find(|site| site.from_id == from && site.name == "f")
        .and_then(|site| site.replaces.clone())
        .unwrap_or_else(|| panic!("an OverloadCall site onto f from {caller}: {:#?}", graph.open_sites))
}

/// **GM-348 B1.** `f(1)`, `f("s")` and `C().m("s")` bind ordinals 0, 1 and
/// 1 - same-file and cross-file alike - and a receiver typed by a parameter
/// annotation binds too. Both calls of `f` from one caller bound, so each
/// caller's structural edge onto `f` is retracted.
///
/// Controls (see the GM-348 S8 controls file): make `choose_overload` return
/// `Choice::Unbound` for several ordinals (every `f` and `m` call is unbound:
/// `f` keeps its structural edges, `m` gets plain edges with no ordinal);
/// remove `overload_disambiguation` from `plugin.toml` (the same, through the
/// shipped config); make `hover_matches` exact-only (the method calls go
/// unbound, the function calls still bind).
#[test]
fn pyright_binds_each_overloaded_call_to_its_stub() {
    let project = Project::new();
    let index = project.index();
    let mut bridge = LspBridge::new("python", &project.0, shipped_config());

    let answer = bridge.answer(&[], &index).expect("the bridge answers");
    assert!(answer.complete, "{:?}", answer.reason);

    let bound = |caller: &str| semantic_from(&answer, &index, caller);
    let expected: BTreeSet<(String, Option<u32>)> =
        [("f", Some(0)), ("f", Some(1)), ("m", Some(1))].map(|(name, at)| (name.to_string(), at)).into();
    assert_eq!(bound("here"), expected, "same file: {:#?}", answer.diff);
    assert_eq!(bound("there"), expected, "cross file: {:#?}", answer.diff);
    assert_eq!(
        bound("through"),
        BTreeSet::from([("m".to_string(), Some(0))]),
        "a receiver typed by an annotation: {:#?}",
        answer.diff
    );

    for (file, caller) in [("pkg/m.py", "here"), ("pkg/use.py", "there")] {
        let structural = structural_onto_f(&index, file, caller);
        assert!(
            answer.diff.delete_edge_ids.contains(&structural),
            "{caller}: both calls of f bound, so its structural edge gives way: {:#?}",
            answer.diff
        );
    }
}
