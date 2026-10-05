//! Helpers shared by the body and open-site tests: a project tree in a
//! temporary directory, extraction through the full [`Extractor`], and the
//! questions the tests ask of one file's graph.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, Position, WireNode};
use g_mesh_plugin_sdk::{Extractor, FileGraph, OpenSite, OpenSiteKind, RelPath};
use g_mesh_plugin_typescript::extractor::keys::is_placeholder_kind;
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::TsProject;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

/// A project tree in its own temporary directory, removed on drop.
pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("g-mesh-ts-bodies-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        for (path, contents) in files {
            let full = fixture.root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }
        fixture
    }

    pub fn load(&self) -> TsProject {
        TypeScriptExtractor.load_project(&self.root).expect("load never fails on a project tree")
    }

    /// Extracts `path` as written to disk, through `project`.
    pub fn extract(&self, project: &TsProject, path: &str) -> Graph {
        let source = fs::read_to_string(self.root.join(path)).unwrap();
        Graph::new(TypeScriptExtractor.extract(project, &RelPath::new(path), &source))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Extracts `source` as `path` with no project around it: no import resolves.
pub fn extract(path: &str, source: &str) -> Graph {
    Graph::new(TypeScriptExtractor.extract(&TsProject::default(), &RelPath::new(path), source))
}

/// Writes `files` to a fresh project, loads it and extracts `path`.
pub fn extract_in(files: &[(&str, &str)], path: &str) -> Graph {
    let fixture = Fixture::new(files);
    let project = fixture.load();
    fixture.extract(&project, path)
}

/// Where the `nth` (zero-based) occurrence of `needle` starts in `source`,
/// in wire columns (characters; every test source is ASCII before it).
pub fn position(source: &str, needle: &str, nth: usize) -> Position {
    let (offset, _) = source.match_indices(needle).nth(nth).unwrap_or_else(|| panic!("{needle:?} #{nth}"));
    let before = &source[..offset];
    let line = before.matches('\n').count() as u32;
    let col = before.rsplit('\n').next().unwrap().chars().count() as u32;
    Position { line, col }
}

/// One file's graph, with every node named by a readable label: a declared
/// symbol by its qualified name, the `File` node as `<file>`, a placeholder
/// as `<nativeKind>:<qualifiedName>`.
pub struct Graph {
    pub graph: FileGraph,
}

impl Graph {
    pub fn new(graph: FileGraph) -> Self {
        Self { graph }
    }

    pub fn file_id(&self) -> &str {
        let file = &self.graph.nodes[0];
        assert_eq!(file.kind, NodeKind::File);
        &file.id
    }

    fn label_of(&self, node: &WireNode) -> String {
        if node.kind == NodeKind::File {
            return "<file>".to_string();
        }
        match node.native_kind.as_deref() {
            Some(kind) if is_placeholder_kind(Some(kind)) => format!("{kind}:{}", node.qualified_name),
            _ => node.qualified_name.clone(),
        }
    }

    pub fn label(&self, id: &str) -> String {
        let node = self.graph.nodes.iter().find(|node| node.id == id);
        node.map_or_else(|| format!("<missing {id}>"), |node| self.label_of(node))
    }

    /// The id of the one node labelled `label`.
    pub fn id(&self, label: &str) -> String {
        let found: Vec<_> = self.graph.nodes.iter().filter(|node| self.label_of(node) == label).collect();
        match found.as_slice() {
            [node] => node.id.clone(),
            other => panic!("{} nodes labelled {label:?} in {:?}", other.len(), self.labels()),
        }
    }

    pub fn labels(&self) -> Vec<String> {
        self.graph.nodes.iter().map(|node| self.label_of(node)).collect()
    }

    /// Every edge of `kind`, as `(from, to)` labels, in emission order.
    pub fn pairs(&self, kind: EdgeKind) -> Vec<(String, String)> {
        self.graph
            .edges
            .iter()
            .filter(|edge| edge.kind == kind)
            .map(|edge| (self.label(&edge.from_id), self.label(&edge.to_id)))
            .collect()
    }

    pub fn has(&self, kind: EdgeKind, from: &str, to: &str) -> bool {
        self.pairs(kind).iter().any(|(f, t)| f == from && t == to)
    }

    /// Every edge kind, in emission order.
    pub fn kinds(&self) -> Vec<EdgeKind> {
        self.graph.edges.iter().map(|edge| edge.kind).collect()
    }

    pub fn sites(&self, kind: OpenSiteKind) -> Vec<&OpenSite> {
        self.graph.open_sites.iter().filter(|site| site.kind == kind).collect()
    }

    /// Every site of `kind` as `(from label, name, edge kind)`.
    pub fn site_summary(&self, kind: OpenSiteKind) -> Vec<(String, String, EdgeKind)> {
        self.sites(kind)
            .into_iter()
            .map(|site| (self.label(&site.from_id), site.name.clone(), site.edge_kind))
            .collect()
    }
}

/// Asserts that `graph` has the edge `from -[kind]-> to`, listing the edges of
/// that kind when it does not.
#[macro_export]
macro_rules! assert_edge {
    ($graph:expr, $kind:expr, $from:expr, $to:expr) => {{
        let graph = &$graph;
        assert!(
            graph.has($kind, $from, $to),
            "no {:?} {} -> {} in {:?}",
            $kind,
            $from,
            $to,
            graph.pairs($kind)
        );
    }};
}

/// Asserts that `graph` has no edge `from -[kind]-> to`.
#[macro_export]
macro_rules! assert_no_edge {
    ($graph:expr, $kind:expr, $from:expr, $to:expr) => {{
        let graph = &$graph;
        assert!(
            !graph.has($kind, $from, $to),
            "unexpected {:?} {} -> {} in {:?}",
            $kind,
            $from,
            $to,
            graph.pairs($kind)
        );
    }};
}
