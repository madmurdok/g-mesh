//! A plugin for a language that does not exist, so the SDK's own contract
//! can be checked by the real conformance kit.
//!
//! # Why a toy and not a real language
//!
//! What has to be proved about this crate is that a plugin built on it keeps
//! core's contract: ids stable across a reparse, a whitespace-only edit
//! answered with an empty diff, a declaration edit that actually lands, edges
//! that never leave their file, an engine that does not start until asked.
//! None of that is about parsing. A real grammar would add a tree-sitter
//! dependency, a much larger surface for a failure to hide in, and an
//! interesting question ("is this the right node for a Rust `impl` block?")
//! that has nothing to do with the thing under test. A language whose whole
//! grammar is five line shapes leaves the SDK as the only thing that can be
//! wrong.
//!
//! # The language
//!
//! One declaration per line, whitespace-insensitive at the ends:
//!
//! | Line | Means |
//! |---|---|
//! | `fn NAME` | declares a public function |
//! | `call NAME` | the enclosing function calls `NAME`, which must be declared in this file |
//! | `use FILE NAME` | the enclosing function references `NAME` from another file |
//! | `?NAME` | the enclosing function calls `NAME` through something the structural pass cannot see - an open site |
//! | `# …`, blank | nothing |
//! | anything else | a syntax error: the file keeps every node it did produce, and every one of them is marked |
//!
//! `?NAME` is the toy's receiver call: `x.foo()` in Go or Rust, a name the
//! structural tier can see written down and cannot bind to anything. It is
//! what gives the semantic tier something to answer, and therefore what makes
//! `capabilities.semantic-engine-lazy` a check with evidence rather than a
//! skip.
//!
//! # Not shipped
//!
//! This binary is built by `cargo build` like any other target of the SDK
//! crate and installed by nothing. A plugin is shipped by putting a
//! `plugin.toml` in a discovery root (`daemon::manifest::default_roots`), and
//! the only manifest that ever names this binary is the one
//! `testing::PluginCheck` writes into a scratch directory for the length of
//! one check.

use std::collections::{BTreeSet, HashSet};

use g_mesh_plugin_sdk::ids::node_id;
use g_mesh_plugin_sdk::wire::{
    EdgeKind, FileChangeDiff, NodeKind, PlaceholderTarget, Position, Range, SourceTier, TargetKey,
    TargetScope,
};
use g_mesh_plugin_sdk::{
    run, Extractor, FileGraph, FileGraphBuilder, NodeSpec, OpenSite, OpenSiteKind, PlaceholderKind,
    PluginSpec, RelPath, SdkIndex, SemanticEngine,
};

const LANGUAGE: &str = "toy";
/// The structural engine's label, on every edge the extractor emits. A free
/// diagnostic string on the wire - core branches on the *tier*, never on this.
const ENGINE: &str = "toy-lines";
/// The semantic engine's label, on every edge the semantic tier emits.
const SEMANTIC_ENGINE: &str = "toy-resolver";

fn main() -> ! {
    run(
        ToyExtractor,
        PluginSpec::new(LANGUAGE, env!("CARGO_PKG_VERSION"), &[".toy"]).exclude_dirs(&["vendor"]),
        // A factory, not an engine: the SDK calls it on the first
        // `semanticPass` and never before, which is what
        // `capabilities.semantic-engine-lazy` checks. Nothing is constructed
        // here - the closure is.
        Some(Box::new(|| Ok(Box::new(ToyEngine) as Box<dyn SemanticEngine>))),
    )
}

struct ToyExtractor;

impl Extractor for ToyExtractor {
    const LANGUAGE: &'static str = LANGUAGE;
    /// The toy has no workspace model: no manifest file, no module map,
    /// nothing outside the file being read. Which is the case a real
    /// plugin's `Project` should be *compared* against - if a language needs
    /// nothing from the project to extract a file, it should say so here.
    type Project = ();

    fn load_project(&self, _root: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn extract(&self, _project: &(), path: &RelPath, source: &str) -> FileGraph {
        let mut graph = FileGraphBuilder::new(LANGUAGE, ENGINE, path);
        let lines: Vec<&str> = source.split('\n').collect();

        // The file's range ends at (number of newlines, length of the final
        // unterminated line). Getting this wrong is the single easiest way to
        // fail `id-stability.whitespace-edit`: an end of `(lines, 0)` moves
        // when a space is inserted before the last newline, and this does not.
        let file_end = Position {
            line: (lines.len() - 1) as u32,
            col: lines.last().map_or(0, |line| line.chars().count()) as u32,
        };
        let file = graph.file_node(Range { start: Position { line: 0, col: 0 }, end: file_end });

        // Collected over the whole file first: a call may name a function
        // declared further down, and a plugin that only knew what it had
        // already seen would emit an unresolved edge for it - which within
        // one file would be a false claim, since nothing outside the file can
        // ever confirm it.
        let declared: HashSet<&str> = lines.iter().filter_map(|line| word_after(line, "fn")).collect();

        let mut enclosing = file.clone();
        let mut has_syntax_errors = false;

        for (row, line) in lines.iter().enumerate() {
            let row = row as u32;
            let Some(range) = line_range(line, row) else { continue };

            if let Some(name) = word_after(line, "fn") {
                let id = graph.add_node(
                    NodeSpec::new(NodeKind::Function, name, name, range).public().native_kind("fn"),
                );
                graph.defines(&file, &id, true);
                enclosing = id;
            } else if let Some(name) = word_after(line, "call") {
                // Only onto a declaration of this same file. A call to a name
                // nothing in the file declares is not an edge onto nothing -
                // it is no edge, because the toy has no way to say which file
                // it meant.
                if declared.contains(name) {
                    let target = node_id(path.as_str(), NodeKind::Function, name, Some("fn"));
                    graph.resolved_edge(EdgeKind::Calls, &enclosing, &target);
                }
            } else if let Some((file_name, name)) = use_target(line) {
                let placeholder = graph.add_placeholder(
                    PlaceholderKind::PendingSymbol,
                    name,
                    PlaceholderTarget {
                        scope: TargetScope::File(file_name.to_string()),
                        key: TargetKey::Name(name.to_string()),
                        from_container: None,
                    },
                    range,
                );
                graph.placeholder_edge(EdgeKind::References, &enclosing, &placeholder);
            } else if let Some(name) = open_site(line) {
                // No edge and no placeholder: the structural tier cannot
                // honestly address one, because it does not know which file
                // would answer. It records where the question is and moves on.
                graph.open_site(OpenSite {
                    from_id: enclosing.clone(),
                    position: range.start,
                    name: name.to_string(),
                    kind: OpenSiteKind::ReceiverCall,
                    edge_kind: EdgeKind::Calls,
                    from_container: None,
                });
            } else {
                has_syntax_errors = true;
            }
        }

        if has_syntax_errors {
            // Not a failure: everything above is still the file's real graph,
            // and this is the flag that says so honestly.
            graph.mark_syntax_errors();
        }
        graph.finish()
    }
}

/// The toy's semantic tier: it answers an open site by looking for the
/// function in every *other* file the SDK has extracted.
///
/// A real engine asks a compiler or a language server. This one asks
/// [`SdkIndex`], which is the point: the index is the SDK's half of the
/// contract with a semantic tier, and an engine that can be written against
/// it alone - no filesystem, no parse of its own - is evidence that it
/// carries what such an engine needs.
struct ToyEngine;

impl SemanticEngine for ToyEngine {
    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> anyhow::Result<FileChangeDiff> {
        // Empty means the whole project - the wire's own convention for the
        // pass that follows the cold-start walk.
        let scope: Vec<RelPath> = if files.is_empty() { index.paths() } else { files.to_vec() };
        let mut diff = FileChangeDiff::default();

        for path in &scope {
            let Some(entry) = index.entry(path) else { continue };
            for site in &entry.graph.open_sites {
                let Some(declaring) = declaring_file(index, path, &site.name) else { continue };
                // The upgrade is an ordinary placeholder plus an ordinary
                // edge, both in the *asking* file - the same shape the
                // structural tier would have emitted had it known where to
                // look. What makes it a semantic answer is `source`.
                let mut answer = FileGraphBuilder::new(LANGUAGE, SEMANTIC_ENGINE, path);
                let placeholder = answer.add_placeholder(
                    PlaceholderKind::PendingSymbol,
                    site.name.clone(),
                    PlaceholderTarget {
                        scope: TargetScope::File(declaring.as_str().to_string()),
                        key: TargetKey::Name(site.name.clone()),
                        from_container: site.from_container.clone(),
                    },
                    Range { start: site.position, end: site.position },
                );
                answer.add_edge(g_mesh_plugin_sdk::EdgeSpec {
                    from_id: site.from_id.clone(),
                    to_id: placeholder,
                    kind: site.edge_kind,
                    resolved: false,
                    to_declaration: None,
                    source: SourceTier::Semantic,
                    engine: SEMANTIC_ENGINE.to_string(),
                });
                let answer = answer.finish();
                diff.upsert_nodes.extend(answer.nodes);
                diff.upsert_edges.extend(answer.edges);
            }
        }
        Ok(diff)
    }
}

/// The file that declares `name`, other than `asking`. Ties are broken by
/// path so the answer does not depend on iteration order; ambiguity is
/// refused rather than guessed, because a wrong semantic answer is worse than
/// none - it claims a precision the structural tier at least did not pretend
/// to.
fn declaring_file(index: &SdkIndex, asking: &RelPath, name: &str) -> Option<RelPath> {
    let candidates: BTreeSet<RelPath> = index
        .files()
        .filter(|(path, _)| *path != asking)
        .filter(|(_, entry)| {
            entry.graph.nodes.iter().any(|node| node.kind == NodeKind::Function && node.name == name)
        })
        .map(|(path, _)| path.clone())
        .collect();
    (candidates.len() == 1).then(|| candidates.into_iter().next().expect("checked"))
}

/// `"helper"` for `  call helper  ` asked about `"call"`; `None` for
/// any other shape, including `call` with no argument or with two.
fn word_after<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let mut words = line.split_whitespace();
    (words.next()? == keyword).then_some(())?;
    let name = words.next()?;
    words.next().is_none().then_some(name)
}

/// `("b.toy", "shared")` for `use b.toy shared`.
fn use_target(line: &str) -> Option<(&str, &str)> {
    let mut words = line.split_whitespace();
    (words.next()? == "use").then_some(())?;
    let file = words.next()?;
    let name = words.next()?;
    words.next().is_none().then_some((file, name))
}

/// `"compute"` for `?compute` - the toy's receiver call.
fn open_site(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let name = words.next()?.strip_prefix('?').filter(|name| !name.is_empty())?;
    words.next().is_none().then_some(name)
}

/// One line's range: from its first non-space character to its last, so that
/// trailing whitespace is outside every range this plugin reports. `None` for
/// a line with nothing on it, and for a comment.
///
/// That the range stops at the last non-space character is not cosmetic. It
/// is what makes a trailing space an edit no range can legitimately move,
/// which is what `id-stability.whitespace-edit` asks of every plugin.
fn line_range(line: &str, row: u32) -> Option<Range> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let start = (line.chars().count() - trimmed.chars().count()) as u32;
    let end = line.trim_end().chars().count() as u32;
    Some(Range { start: Position { line: row, col: start }, end: Position { line: row, col: end } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> FileGraph {
        ToyExtractor.extract(&(), &RelPath::new("a.toy"), source)
    }

    #[test]
    fn a_files_range_ends_where_a_trailing_space_cannot_move_it() {
        let plain = extract("fn a\n");
        let spaced = extract("fn a \n");
        assert_eq!(plain, spaced, "a space before the last newline must change nothing");
    }

    #[test]
    fn a_call_onto_a_declaration_of_the_same_file_is_resolved() {
        let graph = extract("fn a\nfn b\ncall a\n");
        let call = graph.edges.iter().find(|edge| edge.kind == EdgeKind::Calls).expect("a CALLS edge");
        assert!(call.resolved, "nothing is left for core to confirm inside one file");
    }

    #[test]
    fn a_call_onto_a_name_the_file_does_not_declare_is_no_edge_at_all() {
        let graph = extract("fn a\ncall nowhere\n");
        assert!(graph.edges.iter().all(|edge| edge.kind != EdgeKind::Calls), "{:?}", graph.edges);
    }

    #[test]
    fn a_cross_file_use_is_a_placeholder_and_its_edge_is_unresolved() {
        let graph = extract("fn a\nuse b.toy shared\n");
        let placeholder = graph
            .nodes
            .iter()
            .find(|node| node.native_kind.as_deref() == Some("pending_symbol"))
            .expect("a placeholder");
        assert!(placeholder.target.is_some());
        let reference =
            graph.edges.iter().find(|edge| edge.kind == EdgeKind::References).expect("a REFERENCES edge");
        assert!(!reference.resolved, "only core can confirm a cross-file target");
        assert_eq!(reference.to_id, placeholder.id);
    }

    #[test]
    fn an_open_site_produces_no_node_and_no_edge() {
        let graph = extract("fn a\n?compute\n");
        assert_eq!(graph.open_sites.len(), 1);
        assert_eq!(graph.open_sites[0].name, "compute");
        assert!(
            graph.nodes.iter().all(|node| node.native_kind.as_deref() != Some("pending_symbol")),
            "an open site is not a placeholder - the structural tier does not know what to address"
        );
        assert!(graph.edges.iter().all(|edge| edge.kind != EdgeKind::Calls));
    }

    #[test]
    fn a_syntax_error_keeps_the_files_graph_and_marks_it() {
        let graph = extract("fn a\nthis is not the toy language\nfn b\n");
        assert_eq!(
            graph.nodes.iter().filter(|node| node.kind == NodeKind::Function).count(),
            2,
            "a broken line costs that line, not the file"
        );
        assert!(graph.nodes.iter().all(|node| node.has_syntax_errors));
    }

    #[test]
    fn comments_and_blank_lines_produce_nothing_and_are_not_errors() {
        let graph = extract("# a comment\n\n   \nfn a\n");
        assert_eq!(graph.nodes.len(), 2, "the File node and `a`");
        assert!(graph.nodes.iter().all(|node| !node.has_syntax_errors));
    }

    #[test]
    fn extraction_is_a_pure_function_of_the_source() {
        let source = "fn a\ncall a\nuse b.toy shared\n?compute\n";
        assert_eq!(extract(source), extract(source));
    }

    /// The engine only answers when exactly one other file declares the name:
    /// a guess that happens to be right is indistinguishable from one that is
    /// not, and the whole value of a semantic answer is that it is exact.
    #[test]
    fn the_engine_refuses_an_ambiguous_name_and_answers_an_unambiguous_one() {
        let mut index = SdkIndex::new();
        let asking = RelPath::new("a.toy");
        index.insert(asking.clone(), String::new(), extract("fn caller\n?compute\n"));
        index.insert(
            RelPath::new("b.toy"),
            String::new(),
            ToyExtractor.extract(&(), &RelPath::new("b.toy"), "fn compute\n"),
        );

        let answered = ToyEngine.answer(&[], &index).unwrap();
        assert_eq!(answered.upsert_edges.len(), 1);
        assert_eq!(answered.upsert_edges[0].source, SourceTier::Semantic);
        assert_eq!(answered.upsert_nodes.len(), 1);
        assert_eq!(answered.upsert_nodes[0].file_path, "a.toy", "the placeholder lives in the asking file");

        index.insert(
            RelPath::new("c.toy"),
            String::new(),
            ToyExtractor.extract(&(), &RelPath::new("c.toy"), "fn compute\n"),
        );
        let ambiguous = ToyEngine.answer(&[], &index).unwrap();
        assert_eq!(ambiguous, FileChangeDiff::default(), "two candidates is no answer, not either answer");
    }
}
