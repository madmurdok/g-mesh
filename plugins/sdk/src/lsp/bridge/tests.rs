use super::super::position::PositionEncoding;
use super::*;
use super::{answers::*, landing::*, pass::*, questions::*};
use crate::graph::NodeSpec;
use crate::graph::{FileGraphBuilder, OpenSite, OpenSiteKind, PlaceholderKind};
use crate::semantic::SemanticEngine;
use g_mesh_wire::Visibility;
use g_mesh_wire::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, Range, TargetKey, TargetScope, WireDeclaration,
    WireEdge, WireNode,
};
use std::collections::HashMap;

fn node(name: &str, start: (u32, u32), end: (u32, u32)) -> WireNode {
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
    let range =
        Range { start: Position { line: start.0, col: start.1 }, end: Position { line: end.0, col: end.1 } };
    builder.add_node(NodeSpec::new(NodeKind::Type, name, name, range).native_kind("trait"));
    builder.finish().nodes.remove(0)
}

/// **GM-319.** The ceiling has to clear the question list every corpus
/// anyone has actually counted, because a corpus over it is a project
/// whose whole-project pass can never be recorded as finished - the diff
/// is committed, `core::watcher::apply::apply_semantic_pass` fails the
/// pass, `language_state.semanticPassAt` stays unset, the language's
/// receiver-call gap is listed in every session's MCP instructions, and
/// the whole pass is repeated on every daemon start.
///
/// The numbers are GM-314's census, re-counted for g-mesh by GM-319. They
/// are written down here rather than left in a document because this is
/// the assertion that would have caught the defect: at the previous
/// 20,000 it fails on three of the four.
#[test]
fn the_site_ceiling_clears_every_corpus_that_has_been_counted() {
    // corpus, questions the plugins build for it (GM-314 / GM-319).
    let measured = [
        ("django/django", 87_832usize),
        ("tokio-rs/tokio", 27_750),
        ("g-mesh itself", 21_995),
        ("pallets/flask", 1_825),
    ];
    let ceiling = Budgets::default().max_sites;
    for (corpus, questions) in measured {
        assert!(
            ceiling >= questions,
            "{corpus} builds {questions} questions and the ceiling is {ceiling}: its pass \
             would be cut short and never recorded as complete"
        );
    }
    let largest = measured.iter().map(|(_, n)| *n).max().unwrap();
    assert!(
        ceiling >= largest * 4,
        "and with room to spare - the largest corpus measured is {largest}, and a ceiling \
         sized to exactly what has been seen is one the next repository walks through"
    );
}

/// What the ceiling costs if a project ever reaches it, so the figure this
/// type's doc quotes is checked rather than asserted.
///
/// The list is a second copy of open sites the index already holds, so
/// the number that matters is the marginal one; it is bounded here at the
/// order of magnitude, not to the byte, because `Question`'s layout is
/// the compiler's business and a test that pinned it exactly would fail
/// on a field reordering that costs nothing.
#[test]
fn the_site_ceiling_bounds_what_one_question_list_can_cost() {
    let per_question = std::mem::size_of::<Question>();
    let worst = Budgets::default().max_sites.saturating_mul(per_question);
    assert!(
        worst <= 512 * 1024 * 1024,
        "a full question list is {} bytes of `Question` ({per_question} each) - past the \
         point where the ceiling is protecting anything",
        worst
    );
}

/// A request has to land on the identifier, not on the keyword the
/// declaration's range starts at - see [`name_position`].
#[test]
fn a_declarations_question_is_aimed_at_its_name() {
    let source = "pub trait Greeter {\n    fn hello(&self);\n}\n";
    let at = name_position(source, &node("Greeter", (0, 0), (2, 1)));
    assert_eq!(at, Position { line: 0, col: 10 });
}

/// And on a line where a character is not a byte, the column is counted in
/// characters, like every other column on this wire.
#[test]
fn a_name_position_counts_characters_not_bytes() {
    let source = "// héllo 🦀\npub trait Grüßer {}\n";
    let at = name_position(source, &node("Grüßer", (1, 0), (1, 19)));
    assert_eq!(at, Position { line: 1, col: 10 }, "`pub trait ` is ten characters");
}

#[test]
fn a_name_that_is_not_in_its_own_range_falls_back_to_the_range_start() {
    let source = "pub trait Greeter {}\n";
    let at = name_position(source, &node("Missing", (0, 0), (0, 20)));
    assert_eq!(at, Position { line: 0, col: 0 });
}

#[test]
fn all_three_location_shapes_are_read() {
    let single = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 3, "character": 7 } } });
    let array = json!([single.clone(), single.clone()]);
    let links = json!([{
        "targetUri": "file:///p/a.toy",
        "targetRange": { "start": { "line": 1, "character": 0 } },
        "targetSelectionRange": { "start": { "line": 3, "character": 7 } },
    }]);
    for (label, value) in [("single", &single), ("array", &array), ("links", &links)] {
        let found = locations(value);
        assert!(!found.is_empty(), "{label}");
        assert_eq!(found[0].uri, "file:///p/a.toy", "{label}");
        assert_eq!((found[0].line, found[0].column), (3, 7), "{label}");
    }
    assert!(locations(&Value::Null).is_empty(), "no definition is an empty list, not a panic");
    assert!(locations(&json!([])).is_empty());
}

/// A `LocationLink`'s selection range is the one that points at the name;
/// `targetRange` covers the whole declaration and is only the fallback.
#[test]
fn a_location_link_prefers_the_selection_range() {
    let links = json!([{
        "targetUri": "file:///p/a.toy",
        "targetRange": { "start": { "line": 1, "character": 0 } },
    }]);
    let found = locations(&links);
    assert_eq!((found[0].line, found[0].column), (1, 0));
}

#[test]
fn a_placeholder_or_a_file_node_is_never_an_answers_target() {
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
    let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } };
    builder.file_node(range);
    builder.add_placeholder(
        PlaceholderKind::PendingSymbol,
        "x",
        PlaceholderTarget {
            scope: TargetScope::File("b.toy".into()),
            key: TargetKey::Name("x".into()),
            from_container: None,
            key_path: None,
            also_scopes: Vec::new(),
        },
        range,
    );
    builder.add_external_module("some-package", range);
    builder.add_node(NodeSpec::new(NodeKind::Function, "f", "f", range).public());
    let graph = builder.finish();
    assert!(!is_addressable(&graph.nodes[0]), "the File node");
    assert!(!is_addressable(&graph.nodes[1]), "a pending symbol");
    assert!(!is_addressable(&graph.nodes[2]), "an external module");
    assert!(is_addressable(&graph.nodes[3]), "a real declaration");
}

/// The address a semantic answer carries: exact by `qualifiedName`, scoped
/// to the declaration's container when it has one.
#[test]
fn an_address_is_qualified_and_container_scoped_when_it_can_be() {
    let mut node = node("Greeter", (0, 0), (0, 1));
    node.container = Some("krate::greet".to_string());
    node.qualified_name = "greet::Greeter".to_string();
    node.file_path = "src/greet.rs".to_string();
    node.visibility = Visibility::Public;

    let target = address_of(&node, Some("krate::app".to_string()));
    assert_eq!(target.scope, TargetScope::Container("krate::greet".to_string()));
    assert_eq!(target.key, TargetKey::QualifiedName("greet::Greeter".to_string()));
    assert_eq!(target.from_container.as_deref(), Some("krate::app"));

    node.container = None;
    let target = address_of(&node, None);
    assert_eq!(target.scope, TargetScope::File("src/greet.rs".to_string()));
    assert_eq!(target.key_path, None, "no path on the declaration, none on the key");

    let path = g_mesh_wire::QualifiedPath::root("greet").child("::", "Greeter");
    node.qualified_path = Some(path.clone());
    let target = address_of(&node, None);
    assert_eq!(target.key_path, Some(path));
    assert_eq!(target.check_key_path(), Ok(()));
}

/// **GM-378.** Two sites of one file addressing one declaration are one
/// placeholder, and the row it carries must be the same whichever of them
/// the server answered first.
///
/// The instance is ripgrep's, reduced: `SearchMode::Count` at line 140 and
/// `SearchMode::CountMatches` at line 144 of `crates/core/flags/hiargs.rs`
/// both resolve to the enum `SearchMode`, so both land on node id
/// `69f3fc74…`. Recording whichever arrived first put a different `name`
/// and a different `range` on that one row from run to run, over an
/// unchanged checkout indexed by an unchanged binary.
///
/// The two arms here are the same two answers in the two arrival orders,
/// which is exactly the difference between two such runs: with the witness
/// rule removed, the two arms produce the two rows the two indexings did,
/// and this test fails on them. Which *name* the surviving row carries is
/// the sibling test below.
#[test]
fn one_address_answered_from_two_sites_carries_the_same_row_in_either_order() {
    let file = RelPath::new("crates/core/flags/hiargs.rs");
    let mut declaration = node("SearchMode", (30, 9), (40, 1));
    declaration.container = Some("rg::flags::lowargs".to_string());
    declaration.qualified_name = "flags::lowargs::SearchMode".to_string();
    declaration.file_path = "crates/core/flags/lowargs.rs".to_string();

    // The two sites, in source order.
    let sites = [
        (Position { line: 140, col: 28 }, "SearchMode::Count"),
        (Position { line: 144, col: 28 }, "SearchMode::CountMatches"),
    ];
    let mut rows = Vec::new();
    for order in [[0usize, 1], [1, 0]] {
        let mut answers = Answers::new("rust", "rust-analyzer");
        for i in order {
            let (at, written) = sites[i];
            answers.record(
                &file,
                &file,
                "from",
                EdgeKind::References,
                &declaration.name,
                site_range(None, at, written),
                address_of(&declaration, Some("rg::flags::hiargs".to_string())),
                &declaration.id,
                None,
            );
        }
        let (diff, _) = answers.finish();
        assert_eq!(diff.upsert_nodes.len(), 1, "two sites, one address, one node");
        rows.push(diff.upsert_nodes.into_iter().next().unwrap());
    }

    assert_eq!(
        rows[0].id, "1e9a642bc7367b8551f39d6d9c9d50da",
        "the reduction has to be the real instance, not something like it: this is the id \
         the two rows collided on in an index of ripgrep (69f3fc74... before GM-596 put the \
         key kind into the label)"
    );

    assert_eq!(rows[0], rows[1], "the row a shared placeholder carries depends on answer order");
    assert_eq!(
        rows[0].name, "SearchMode",
        "the node speaks for both sites, so it carries the name they have in common - not \
         whichever spelling was answered first"
    );
    assert_eq!(
        rows[0].range.start,
        Position { line: 140, col: 28 },
        "and the earliest of its sites, the way a source-order walk would have picked"
    );
}

/// **GM-378.** And the name that one row carries is the declaration's,
/// not the text at the site that happened to reach it.
///
/// The other half of the same instance, taken through `record_answer` so
/// that what is under test is the argument the caller chooses.
/// `OpenSite::name` is documented as something "an LSP bridge uses only
/// for diagnostics"; passing it here made the node claim to be
/// `SearchMode::Count` while its `qualifiedName` said `…::SearchMode`.
#[test]
fn a_placeholder_is_named_after_the_declaration_it_waits_on() {
    let root = Path::new("/p");
    let declared = RelPath::new("lowargs.rs");
    let mut builder = FileGraphBuilder::new("rust", "tree-sitter", &declared);
    let range = Range { start: Position { line: 30, col: 9 }, end: Position { line: 40, col: 1 } };
    let mut spec = NodeSpec::new(NodeKind::Type, "SearchMode", "flags::lowargs::SearchMode", range)
        .native_kind("enum")
        .public();
    spec.container = Some("rg::flags::lowargs".to_string());
    builder.add_node(spec);
    let mut index = SdkIndex::new();
    index.insert(declared.clone(), "pub enum SearchMode {}\n".repeat(41), builder.finish());

    let asking = RelPath::new("hiargs.rs");
    let question = Question {
        file: asking.clone(),
        position: Position { line: 140, col: 28 },
        ask: Ask::Definition(OpenSite {
            from_id: "caller".to_string(),
            position: Position { line: 140, col: 28 },
            name: "SearchMode::Count".to_string(),
            kind: OpenSiteKind::Reference,
            edge_kind: EdgeKind::References,
            from_container: Some("rg::flags::hiargs".to_string()),
            replaces: None,
        }),
    };
    let found = json!({
        "uri": "file:///p/lowargs.rs",
        "range": { "start": { "line": 30, "character": 9 } },
    });

    let mut answers = Answers::new("rust", "rust-analyzer");
    let again = record_answer(&mut answers, &index, [root, root], PositionEncoding::Utf16, &question, &found);
    assert!(again.is_empty(), "a definition answer asks nothing further");
    let (diff, _) = answers.finish();
    assert_eq!(diff.upsert_nodes.len(), 1);
    assert_eq!(
        diff.upsert_nodes[0].name, "SearchMode",
        "the placeholder waits on the enum, whatever the site that reached it was spelled"
    );
    assert_eq!(
        diff.upsert_nodes[0].qualified_name, "rg::flags::lowargs::=flags::lowargs::SearchMode",
        "and its name agrees with the address its id is derived from"
    );
    assert_eq!(
        diff.upsert_nodes[0].range,
        site_range(None, Position { line: 140, col: 28 }, "SearchMode::Count"),
        "the range is still the site's own extent, as wide as what is written there"
    );
}

#[test]
fn a_whole_project_budget_scales_with_the_files_in_scope_and_a_per_file_one_does_not() {
    let bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
    assert_eq!(bridge.pass_budget(true, 10), Duration::from_secs(15 * 60), "the floor");
    assert_eq!(bridge.pass_budget(true, 1_000), Duration::from_secs(8_000), "8s a file");
    assert_eq!(bridge.pass_budget(false, 1_000), Duration::from_secs(90));
}

/// The bridge must stay inside core's own timer - see [`Budgets`].
#[test]
fn every_budget_is_inside_the_one_core_allows() {
    let budgets = Budgets::default();
    let bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
    // core: max(20min, 10s * files) for a whole project, flat 120s per file.
    for files in [0usize, 1, 100, 10_000] {
        let core = Duration::from_secs(20 * 60).max(Duration::from_secs(10) * files as u32);
        assert!(bridge.pass_budget(true, files) < core, "{files} files");
    }
    assert!(bridge.pass_budget(false, 1) < Duration::from_secs(120));
    assert!(budgets.readiness <= budgets.project_floor);
}

/// With core's deadline, a pass plans to three quarters of the time left
/// until it, whatever its shape: a many-file per-file-shaped pass under
/// 120s plans 90s, not 8s a file. A deadline already past plans nothing.
///
/// Control: ignore `core_deadline` in `pass_deadline`.
#[test]
fn with_cores_deadline_a_pass_plans_three_quarters_of_the_time_left() {
    let mut bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
    let started = Instant::now();

    bridge.set_pass_deadline(Some(started + Duration::from_secs(120)));
    assert_eq!(bridge.pass_deadline(started, false, 1_000), started + Duration::from_secs(90));
    assert_eq!(bridge.pass_deadline(started, true, 1_000), started + Duration::from_secs(90));

    bridge.set_pass_deadline(Some(started + Duration::from_secs(20 * 60)));
    assert_eq!(bridge.pass_deadline(started, true, 10), started + Duration::from_secs(15 * 60));

    let later = started + Duration::from_secs(10);
    bridge.set_pass_deadline(Some(started));
    assert_eq!(bridge.pass_deadline(later, true, 10), later, "a past deadline leaves no time to plan");
}

/// Without core's deadline - never sent, or cleared by a pass without
/// one - the bridge's own budgets apply.
#[test]
fn without_cores_deadline_a_pass_plans_its_own_budget() {
    let mut bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
    let started = Instant::now();
    for (whole_project, files) in [(false, 1_000), (true, 10), (true, 1_000)] {
        assert_eq!(
            bridge.pass_deadline(started, whole_project, files),
            started + bridge.pass_budget(whole_project, files)
        );
    }

    bridge.set_pass_deadline(Some(started + Duration::from_secs(120)));
    bridge.set_pass_deadline(None);
    assert_eq!(bridge.pass_deadline(started, true, 1_000), started + Duration::from_secs(8_000));
}

// --- GM-486: untyped receiver calls the semantic tier answered ---------

/// `a.toy` declares `add` and `sub`; `b.toy`'s `caller` calls `len`
/// twice and `add` once through untyped receivers, `typed` through a
/// receiver with a structural edge (`replaces`), and holds a plain
/// reference `bare`. `caller` also carries every column a structural
/// node can, so a re-sent copy that lost one is visible.
fn untyped_index() -> (SdkIndex, String) {
    let range = |start: (u32, u32), end: (u32, u32)| Range {
        start: Position { line: start.0, col: start.1 },
        end: Position { line: end.0, col: end.1 },
    };

    let a = RelPath::new("a.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &a);
    builder.file_node(range((0, 0), (2, 0)));
    for (line, name) in [(0, "add"), (1, "sub")] {
        builder.add_node(
            NodeSpec::new(NodeKind::Function, name, name, range((line, 3), (line, 6)))
                .native_kind("method")
                .public(),
        );
    }
    let mut index = SdkIndex::new();
    index.insert(a, "fn add\nfn sub\n".to_string(), builder.finish());

    let b = RelPath::new("b.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &b);
    builder.record_untyped_receiver_calls();
    builder.file_node(range((0, 0), (6, 0)));
    let caller = builder.add_node(
        NodeSpec::new(NodeKind::Function, "caller", "m::caller", range((0, 0), (5, 1)))
            .native_kind("function")
            .in_container("m", None)
            .public(),
    );
    for (line, name, kind, replaces) in [
        (1, "len", OpenSiteKind::ReceiverCall, None),
        (2, "len", OpenSiteKind::ReceiverCall, None),
        (3, "add", OpenSiteKind::ReceiverCall, None),
        (4, "typed", OpenSiteKind::ReceiverCall, Some("e-typed".to_string())),
        (4, "bare", OpenSiteKind::Reference, None),
        // GM-497: an untyped field read is not a receiver call.
        (5, "fld", OpenSiteKind::ReceiverField, None),
    ] {
        builder.open_site(OpenSite {
            from_id: caller.clone(),
            position: Position { line, col: 4 },
            name: name.to_string(),
            kind,
            edge_kind: if kind == OpenSiteKind::ReceiverField {
                EdgeKind::References
            } else {
                EdgeKind::Calls
            },
            from_container: Some("m".to_string()),
            replaces,
        });
    }
    let mut graph = builder.finish();
    let node = graph.nodes.iter_mut().find(|node| node.id == caller).unwrap();
    // `fld` (a `ReceiverField`) is not folded in: control, count
    // `ReceiverField` in `fold_untyped_calls`.
    assert_eq!(node.untyped_calls, ["add", "len"], "the structural list the fixture starts from");
    node.signature = Some("fn caller()".to_string());
    node.doc_comment = Some("Calls things.".to_string());
    node.declarations = Some(vec![g_mesh_wire::WireDeclaration {
        ordinal: 0,
        start_line: 0,
        start_col: 0,
        end_line: 5,
        end_col: 1,
        signature: Some("fn caller()".to_string()),
        has_body: true,
    }]);
    let path = |first: &str| {
        g_mesh_wire::QualifiedPath(vec![
            g_mesh_wire::PathSegment { sep: None, name: first.to_string() },
            g_mesh_wire::PathSegment { sep: Some("::".to_string()), name: "caller".to_string() },
        ])
    };
    node.qualified_path = Some(path("m"));
    node.alias_paths = vec![path("alias")];
    node.target = Some(PlaceholderTarget {
        scope: TargetScope::Container("m".to_string()),
        key: TargetKey::QualifiedName("m::caller".to_string()),
        from_container: Some("m".to_string()),
        key_path: None,
        also_scopes: Vec::new(),
    });
    index.insert(
        b,
        "fn caller() {\n  x.len();\n  y.len();\n  z.add();\n  t.typed();\n}\n".to_string(),
        graph,
    );
    (index, caller)
}

/// The open site of `caller` called `name` at `line`.
fn site_of(index: &SdkIndex, name: &str, line: u32) -> OpenSite {
    let entry = index.entry(&RelPath::new("b.toy")).unwrap();
    entry
        .graph
        .open_sites
        .iter()
        .find(|site| site.name == name && site.position.line == line)
        .unwrap()
        .clone()
}

/// How many sites of `(caller, name)` one answer `found` counted as
/// answered, and how many edges it recorded.
fn answered_by(index: &SdkIndex, caller: &str, site: OpenSite, found: Value) -> (usize, usize) {
    answered_in(index, "b.toy", caller, site, found)
}

/// [`answered_by`] for a question asked in `file`, which is where a landing
/// can enclose the site.
fn answered_in(index: &SdkIndex, file: &str, caller: &str, site: OpenSite, found: Value) -> (usize, usize) {
    let question =
        Question { file: RelPath::new(file), position: site.position, ask: Ask::Definition(site.clone()) };
    let mut answers = Answers::new("toy", "toy-lsp");
    let root = Path::new("/p");
    let _ = record_answer(&mut answers, index, [root, root], PositionEncoding::Utf16, &question, &found);
    let counted =
        answers.untyped_answered.get(&(caller.to_string(), site.name.clone())).copied().unwrap_or(0);
    let (diff, _) = answers.finish();
    (counted, diff.upsert_edges.len())
}

/// **GM-486, the counting rule.** An untyped site counts as answered when
/// the answer is non-empty and either recorded an edge or lies wholly
/// outside the index. An empty or null answer does not count, and neither
/// does one that lands in an indexed file without an edge.
///
/// Controls, each in `untyped_call_answered` / `record_answer`: drop the
/// `found.is_empty()` check (the empty and null cases count); drop the
/// `|| found.iter().all(..)` arm (the std case does not count); pass
/// `false` for `recorded` (the edge case does not count); pass
/// `target.is_some()` instead of `agree && target.is_some()` (the
/// disagreeing case counts); test `node_at(..).is_none()` instead of
/// `file_at(..).is_none()` (the no-node case counts); drop the
/// `site.replaces.is_some()` or `site.kind != ReceiverCall` check (a
/// typed call, a reference or a field read counts).
#[test]
fn an_untyped_call_is_answered_only_by_a_non_empty_answer_that_is_an_edge_or_outside_the_index() {
    let (index, caller) = untyped_index();
    let len = || site_of(&index, "len", 1);
    let outside = json!({ "uri": "file:///rustlib/core/src/slice.rs", "range": { "start": { "line": 9, "character": 4 } } });
    let at_add = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 0, "character": 3 } } });
    let at_sub = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 1, "character": 3 } } });
    // Inside `a.toy`, before `add`: only the file node covers it.
    let at_no_node = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 0, "character": 0 } } });

    assert_eq!(answered_by(&index, &caller, len(), Value::Null), (0, 0), "a null answer is no answer");
    assert_eq!(answered_by(&index, &caller, len(), json!([])), (0, 0), "an empty answer is no answer");
    assert_eq!(answered_by(&index, &caller, len(), outside.clone()), (1, 0), "std, outside the index");
    assert_eq!(
        answered_by(&index, &caller, len(), json!([outside.clone(), outside.clone()])),
        (1, 0),
        "every location outside the index"
    );
    assert_eq!(answered_by(&index, &caller, len(), at_add.clone()), (1, 1), "an answer that became an edge");
    assert_eq!(
        answered_by(&index, &caller, len(), at_no_node.clone()),
        (0, 0),
        "an indexed file with no node there may be a missed project call"
    );
    assert_eq!(
        answered_by(&index, &caller, len(), json!([at_add.clone(), at_sub])),
        (0, 0),
        "targets that disagree record no edge, so the name stays"
    );
    assert_eq!(
        answered_by(&index, &caller, len(), json!([outside.clone(), at_no_node])),
        (0, 0),
        "one location in the index without an edge is enough to keep the name"
    );
    assert_eq!(
        answered_by(&index, &caller, site_of(&index, "typed", 4), outside.clone()),
        (0, 0),
        "a call with a structural edge is not an untyped call"
    );
    assert_eq!(
        answered_by(&index, &caller, site_of(&index, "bare", 4), outside.clone()),
        (0, 0),
        "a reference is not a receiver call"
    );
    assert_eq!(
        answered_by(&index, &caller, site_of(&index, "fld", 5), outside),
        (0, 0),
        "a field read is not a receiver call (GM-497)"
    );
}

/// [`untyped_index`] with `a.toy` replaced by declarations a definition
/// can land inside without a node of its own: a class field `cb`, an
/// interface property `go`, an object-literal method that repeats its
/// const's name, a const-bound function `tick`, a tuple struct `Pair`,
/// a local `inner` in a function body, and an anonymous `export default`
/// function (node `default`, its range starting at `(`, GM-593), and an
/// impl `impl<S> Sink` (Type node `<S as Sink>`) holding a method `m`,
/// whose return type `S::E` is a site inside both (GM-614).
fn landing_index() -> (SdkIndex, String) {
    let (mut index, caller) = untyped_index();
    let source = "class Box {\n  cb = wrap();\n}\ninterface Opts {\n  go: () => void;\n}\n\
                  const handler = { handler() {} };\nconst tick = () => 1;\nstruct Pair(u8);\n\
                  fn outer() { let inner = 1; }\nexport default () => { go(); };\n\
                  impl<S> Sink {\n  fn m() -> S::E {}\n}\n";
    let a = RelPath::new("a.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &a);
    builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 14, col: 0 } });
    for (kind, name, start, end) in [
        (NodeKind::Type, "Box", (0, 0), (2, 1)),
        (NodeKind::Type, "Opts", (3, 0), (5, 1)),
        (NodeKind::Variable, "handler", (6, 0), (6, 33)),
        (NodeKind::Variable, "tick", (7, 0), (7, 21)),
        (NodeKind::Type, "Pair", (8, 0), (8, 16)),
        (NodeKind::Function, "outer", (9, 0), (9, 29)),
        (NodeKind::Function, "default", (10, 15), (10, 30)),
        (NodeKind::Type, "<S as Sink>", (11, 0), (13, 1)),
        (NodeKind::Function, "m", (12, 2), (12, 20)),
    ] {
        let range = Range {
            start: Position { line: start.0, col: start.1 },
            end: Position { line: end.0, col: end.1 },
        };
        builder.add_node(NodeSpec::new(kind, name, name, range).native_kind("decl").public());
    }
    index.insert(a, source.to_string(), builder.finish());
    (index, caller)
}

/// A definition answer in `a.toy` at `(line, character)`.
fn in_a(line: u32, character: u32) -> Value {
    json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": line, "character": character } } })
}

/// A call answered inside a node with no node of its own there - a class
/// field, an interface property - names the encloser, which is not what
/// is called: no edge. At the encloser's own name it is recorded, and a
/// reference may still land on the encloser.
///
/// Control: `lands_on_name` returns `true` - the field and the property
/// calls are recorded on their owners.
#[test]
fn a_call_answered_at_a_member_without_a_node_never_lands_on_its_owner() {
    let (index, caller) = landing_index();
    let call = || site_of(&index, "len", 1);
    assert_eq!(answered_by(&index, &caller, call(), in_a(1, 2)).1, 0, "a class field");
    assert_eq!(answered_by(&index, &caller, call(), in_a(4, 2)).1, 0, "an interface property");
    assert_eq!(answered_by(&index, &caller, call(), in_a(0, 6)).1, 1, "the class's own name");
    assert_eq!(
        answered_by(&index, &caller, site_of(&index, "fld", 5), in_a(1, 2)).1,
        1,
        "a field read, a reference, is not held to the name"
    );
}

/// A const-bound function and a tuple struct are called at their own
/// names, so those answers are recorded.
#[test]
fn a_call_answered_at_a_variable_or_type_name_is_recorded() {
    let (index, caller) = landing_index();
    let call = || site_of(&index, "len", 1);
    assert_eq!(answered_by(&index, &caller, call(), in_a(7, 6)).1, 1, "a const-bound function");
    assert_eq!(answered_by(&index, &caller, call(), in_a(8, 7)).1, 1, "a tuple struct");
}

/// `const handler = { handler() {} }` answered at the inner `handler`:
/// the word under the answer is the const's name, but the const names
/// itself first, so the method's answer is not the const's.
///
/// Control: `lands_on_name` accepts any whole-word occurrence of the name,
/// not only the first - the inner one is recorded.
#[test]
fn a_call_answered_at_a_member_sharing_its_owners_name_is_not_recorded() {
    let (index, caller) = landing_index();
    let call = || site_of(&index, "len", 1);
    assert_eq!(answered_by(&index, &caller, call(), in_a(6, 18)).1, 0, "the object-literal method");
    assert_eq!(answered_by(&index, &caller, call(), in_a(6, 6)).1, 1, "the const's own name");
}

/// `u.toy`'s `run` calling `tick()` through a structural `CALLS` onto a
/// placeholder keyed by *name* `tick` in `a.toy` - the GM-596 shape -
/// answered with `found` (at `tick`'s own name: [`TICK`]). Returns
/// `(answers, structural edge id, placeholder id, run's id)`. `link` is
/// what core's linker did with the structural edge, by name of the node it
/// moved it onto.
fn const_call_answered(link: Option<&str>, found: Value) -> (Answers, String, String, String) {
    hop_answered(EdgeKind::Calls, link, found)
}

/// [`const_call_answered`] for a structural edge and open site of
/// `edge_kind`.
fn hop_answered(edge_kind: EdgeKind, link: Option<&str>, found: Value) -> (Answers, String, String, String) {
    let (mut index, _) = landing_index();
    let u = RelPath::new("u.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &u);
    builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 2, col: 0 } });
    let run = builder
        .add_node(NodeSpec::new(NodeKind::Function, "run", "run", line_range(1)).native_kind("function"));
    let target = file_name("a.toy", "tick");
    let placeholder = builder.add_placeholder(PlaceholderKind::PendingSymbol, "tick", target, line_range(1));
    let structural = builder.placeholder_edge(edge_kind, &run, &placeholder);
    let site = OpenSite {
        from_id: run.clone(),
        position: Position { line: 1, col: 2 },
        name: "tick".to_string(),
        kind: OpenSiteKind::Reference,
        edge_kind,
        from_container: None,
        replaces: Some(structural.clone()),
    };
    builder.open_site(site.clone());
    index.insert(u.clone(), "fn run\n  tick()\n".to_string(), builder.finish());

    if let Some(name) = link {
        let linked = a_node(&index, name);
        index.set_linked([(structural.clone(), linked)]);
    }
    let question = Question { file: u, position: site.position, ask: Ask::Definition(site) };
    let mut answers = Answers::new("toy", "toy-lsp");
    let root = Path::new("/p");
    let again = record_answer(&mut answers, &index, [root, root], PositionEncoding::Utf16, &question, &found);
    assert!(again.is_empty());
    (answers, structural, placeholder, run)
}

/// `tick`'s own name in [`landing_index`]'s `a.toy`.
const TICK: (u32, u32) = (7, 6);

/// **GM-596.** `import { tick } from "./a"; tick()` with `const tick =
/// () => 1`: the structural `CALLS` waits on a placeholder keyed by
/// *name* `tick` in `a.toy`, which the linker never lands on a Variable.
/// The answer at the const's own name addresses *qualified name* `tick`
/// in `a.toy` - the same string, a different key. It is recorded under
/// its own id, not taken for agreement, and the structural edge it
/// contradicts is retracted.
///
/// Controls: drop the `=` sigil for a `File` scope in `render_target`
/// (the answer gets the structural edge's id and records nothing); drop
/// the `contradicted`/`retract` inserts after `record` (the structural
/// edge is not retracted).
#[test]
fn a_call_answered_at_a_const_name_keyed_structural_target_is_recorded_under_its_own_id() {
    let (answers, structural, placeholder, run) = const_call_answered(None, in_a(TICK.0, TICK.1));
    let (diff, _) = answers.finish();

    assert_eq!(diff.upsert_edges.len(), 1, "the answer is recorded, not taken for agreement");
    let answer = &diff.upsert_edges[0];
    assert_ne!(answer.id, structural, "a name key and a qualified-name key never share an edge id");
    assert_ne!(answer.to_id, placeholder, "nor a placeholder id");
    assert_eq!(answer.from_id, run);
    assert_eq!(answer.kind, EdgeKind::Calls);
    assert!(
        diff.delete_edge_ids.contains(&structural),
        "the structural edge the answer contradicts is retracted: {:?}",
        diff.delete_edge_ids
    );
}

/// **GM-596, K5.** The same name-keyed structural `CALLS`, but core's
/// linker moved it onto `tick` itself (`linked_target`). The answer at
/// `tick`'s name addresses qualified name `tick`, so neither its id nor
/// the structural edge's own target matches: only the linked target says
/// they agree. Agreement records nothing and retracts nothing (R2).
///
/// Control: drop `linked_on_it` from R2 - the answer is recorded and the
/// structural edge retracted.
#[test]
fn a_call_agreeing_with_a_name_keyed_edge_core_linked_onto_the_same_node_records_nothing() {
    let (answers, structural, _, _) = const_call_answered(Some("tick"), in_a(TICK.0, TICK.1));
    let (diff, _) = answers.finish();
    assert!(diff.upsert_edges.is_empty(), "agreement adds nothing: {:?}", diff.upsert_edges);
    assert!(
        !diff.delete_edge_ids.contains(&structural),
        "the structural edge stands: {:?}",
        diff.delete_edge_ids
    );
}

/// **GM-596, gap 1.** TypeScript answers a call onto a factory-made const
/// (`const tick = throttle(g)`) with two locations: the const's name, and
/// the call signature's declaration inside the factory's body. The second
/// sits at no node's name, so it does not outvote the first: one edge onto
/// the const, and the structural edge it contradicts is retracted.
///
/// Control: drop the `!agree && Calls` re-filter in `record_answer` - the
/// locations disagree and nothing is recorded.
#[test]
fn a_call_answered_at_a_const_name_and_inside_another_functions_body_lands_on_the_const() {
    let found = json!([in_a(TICK.0, TICK.1), in_a(9, 17)]);
    let (answers, structural, _, run) = const_call_answered(None, found);
    let (diff, _) = answers.finish();
    assert_eq!(diff.upsert_edges.len(), 1, "one edge: {:?}", diff.upsert_edges);
    let answer = &diff.upsert_edges[0];
    assert_eq!((answer.from_id.as_str(), answer.kind), (run.as_str(), EdgeKind::Calls));
    let (alone, _, _, _) = const_call_answered(None, in_a(TICK.0, TICK.1));
    let alone = alone.finish().0.upsert_edges;
    assert_eq!(answer.to_id, alone[0].to_id, "the edge the const's name alone records, not `outer`");
    assert!(
        diff.delete_edge_ids.contains(&structural),
        "the structural edge is retracted: {:?}",
        diff.delete_edge_ids
    );
}

/// **GM-596, gap 1.** Two locations at the names of two different nodes
/// (`cfg` or overload twins) still disagree: nothing is recorded and the
/// structural edge stands.
///
/// Control: `named_landing` returns its first named location without the
/// `all(..)` check - the edge onto `tick` is recorded.
#[test]
fn a_call_answered_at_the_names_of_two_different_nodes_records_nothing() {
    let found = json!([in_a(TICK.0, TICK.1), in_a(9, 3)]);
    let (answers, structural, _, _) = const_call_answered(None, found);
    let (diff, _) = answers.finish();
    assert!(diff.upsert_edges.is_empty(), "no edge: {:?}", diff.upsert_edges);
    assert!(
        !diff.delete_edge_ids.contains(&structural),
        "the structural edge stands: {:?}",
        diff.delete_edge_ids
    );
}

/// **GM-596, gap 1.** Re-reading a disagreeing answer for the location at
/// a node's own name is for `CALLS` only: a `REFERENCES` hop answered at
/// `tick`'s name and inside `outer`'s body still disagrees, records
/// nothing, and leaves the structural edge standing.
///
/// Control: drop the `site.edge_kind == EdgeKind::Calls` guard in
/// `record_answer` - the edge onto `tick` is recorded.
#[test]
fn a_non_call_answered_at_a_const_name_and_inside_another_functions_body_records_nothing() {
    let found = json!([in_a(TICK.0, TICK.1), in_a(9, 17)]);
    let (answers, structural, _, _) = hop_answered(EdgeKind::References, None, found);
    let (diff, _) = answers.finish();
    assert!(diff.upsert_edges.is_empty(), "no edge: {:?}", diff.upsert_edges);
    assert!(
        !diff.delete_edge_ids.contains(&structural),
        "the structural edge stands: {:?}",
        diff.delete_edge_ids
    );
}

/// `impl Matcher for RegexMatcher` across two files: `m.toy` declares the
/// trait `Matcher` (the anchor), `i.toy` declares `RegexMatcher` with a
/// structural `SUPERTYPE_OF` onto a placeholder addressed by `target`,
/// linked by core as `linked` says. The server lists `RegexMatcher`
/// among `Matcher`'s implementations; returns the edges that answer upserts.
fn implementor_answered(target: PlaceholderTarget, linked: LinkedOnto) -> Vec<WireEdge> {
    let mut index = SdkIndex::new();
    let m = RelPath::new("m.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &m);
    builder.file_node(line_range(0));
    let anchor = builder.add_node(
        NodeSpec::new(NodeKind::Type, "Matcher", "Matcher", line_range(0)).native_kind("trait").public(),
    );
    index.insert(m.clone(), "trait Matcher\n".to_string(), builder.finish());

    let i = RelPath::new("i.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &i);
    builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 2, col: 0 } });
    let implementor = builder.add_node(
        NodeSpec::new(NodeKind::Type, "RegexMatcher", "RegexMatcher", line_range(0)).native_kind("struct"),
    );
    let placeholder =
        builder.add_placeholder(PlaceholderKind::PendingSymbol, "Matcher", target, line_range(1));
    let structural = builder.placeholder_edge(EdgeKind::SupertypeOf, &implementor, &placeholder);
    index.insert(i, "struct RegexMatcher\n".to_string(), builder.finish());
    match linked {
        LinkedOnto::Nothing => {}
        LinkedOnto::Anchor => index.set_linked([(structural, anchor.clone())]),
        LinkedOnto::Elsewhere => {
            // Another `Matcher`, in another file: what core resolved the
            // name-keyed placeholder to instead of the anchor.
            let n = RelPath::new("n.toy");
            let mut builder = FileGraphBuilder::new("toy", "toy-parser", &n);
            builder.file_node(line_range(0));
            let other = builder.add_node(
                NodeSpec::new(NodeKind::Type, "Matcher", "Matcher", line_range(0))
                    .native_kind("trait")
                    .public(),
            );
            index.insert(n, "trait Matcher\n".to_string(), builder.finish());
            index.set_linked([(structural, other)]);
        }
    }

    let question =
        Question { file: m, position: Position { line: 0, col: 6 }, ask: Ask::Implementation { anchor } };
    let mut answers = Answers::new("toy", "toy-lsp");
    let root = Path::new("/p");
    let found = json!({ "uri": "file:///p/i.toy", "range": { "start": { "line": 0, "character": 7 } } });
    let again = record_answer(&mut answers, &index, [root, root], PositionEncoding::Utf16, &question, &found);
    assert!(again.is_empty(), "the location is a declaration: no second hop");
    answers.finish().0.upsert_edges
}

/// **GM-596, gap 2 (F2a).** The implementor's structural `SUPERTYPE_OF`
/// waits on a placeholder of another name, but core linked it onto the
/// anchor: the answer says what the structural edge already says and
/// records nothing.
///
/// Control: drop the `supertype_reaches` check in `record_implementor` -
/// a second, semantic row for the same `impl` is upserted.
#[test]
fn an_implementor_whose_structural_supertype_edge_is_linked_onto_the_anchor_records_nothing() {
    let upserts = implementor_answered(file_name("x.toy", "Alias"), LinkedOnto::Anchor);
    assert!(upserts.is_empty(), "the structural edge already says it: {upserts:?}");
}

/// **GM-596, gap 2.** The same structural edge, unlinked and waiting on a
/// placeholder of another name: it does not reach the anchor, so the
/// answer is recorded.
///
/// Control: `supertype_reaches` always returns `true` - nothing is
/// recorded.
#[test]
fn an_implementor_whose_structural_supertype_edge_reaches_elsewhere_is_recorded() {
    let upserts = implementor_answered(file_name("m.toy", "Other"), LinkedOnto::Nothing);
    assert_eq!(upserts.len(), 1, "{upserts:?}");
    assert_eq!(upserts[0].kind, EdgeKind::SupertypeOf);
}

/// **GM-596, gap 2.** Unlinked, but the placeholder is the anchor's own
/// scope keyed by *name* equal to its qualified name - the address that
/// shared the answer's id before the `=` sigil. Without the linked map (a
/// pass that did not link the file) it still reaches the anchor: nothing
/// is recorded.
///
/// Control: drop the `name_keyed_on_anchor` arm of `supertype_reaches` -
/// the answer is recorded as a duplicate row.
#[test]
fn an_implementor_whose_unlinked_placeholder_is_keyed_by_the_anchors_name_records_nothing() {
    let upserts = implementor_answered(file_name("m.toy", "Matcher"), LinkedOnto::Nothing);
    assert!(upserts.is_empty(), "the name-keyed placeholder addresses the anchor: {upserts:?}");
}

/// What core's linker did with [`implementor_answered`]'s structural edge.
#[derive(Clone, Copy, Debug)]
enum LinkedOnto {
    /// Nothing: a pass that did not link the file.
    Nothing,
    /// The anchor itself.
    Anchor,
    /// Another node of the anchor's name, in another file.
    Elsewhere,
}

/// **GM-596, gap 2.** Unlinked, keyed by *name* equal to the anchor's
/// qualified name, but in another file's scope: a different `Matcher`'s
/// address, so it does not reach the anchor and the answer is recorded.
///
/// Control: drop `target.scope == scope` from `supertype_reaches`'
/// `name_keyed_on_anchor` - nothing is recorded.
#[test]
fn an_implementor_whose_name_keyed_placeholder_is_in_another_scope_is_recorded() {
    let upserts = implementor_answered(file_name("x.toy", "Matcher"), LinkedOnto::Nothing);
    assert_eq!(upserts.len(), 1, "{upserts:?}");
    assert_eq!(upserts[0].kind, EdgeKind::SupertypeOf);
}

/// **GM-596, gap 2.** The placeholder is keyed by the anchor's own scope
/// and name, but core linked the edge onto a different `Matcher`: the
/// structural edge states another supertype, so the answer is recorded.
///
/// Control: drop `linked_target(..).is_none()` from `supertype_reaches`'
/// name-keyed arm - nothing is recorded.
#[test]
fn an_implementor_whose_name_keyed_placeholder_core_linked_elsewhere_is_recorded() {
    let upserts = implementor_answered(file_name("m.toy", "Matcher"), LinkedOnto::Elsewhere);
    assert_eq!(upserts.len(), 1, "{upserts:?}");
    assert_eq!(upserts[0].kind, EdgeKind::SupertypeOf);
}

/// **GM-596, gap 2.** `impl Matcher for RegexMatcher` in the trait's own
/// file: the structural `SUPERTYPE_OF` is resolved, straight onto the
/// anchor, with nothing linked. It already says what the answer would, so
/// nothing is recorded.
///
/// Control: drop the `edge.to_id == anchor.id` arm of `supertype_reaches`
/// - a second, semantic row for the same `impl` is upserted.
#[test]
fn an_implementor_whose_resolved_structural_supertype_edge_targets_the_anchor_records_nothing() {
    let mut index = SdkIndex::new();
    let m = RelPath::new("m.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &m);
    builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 2, col: 0 } });
    let anchor = builder.add_node(
        NodeSpec::new(NodeKind::Type, "Matcher", "Matcher", line_range(0)).native_kind("trait").public(),
    );
    let implementor = builder.add_node(
        NodeSpec::new(NodeKind::Type, "RegexMatcher", "RegexMatcher", line_range(1)).native_kind("struct"),
    );
    builder.resolved_edge(EdgeKind::SupertypeOf, &implementor, &anchor);
    index.insert(m.clone(), "trait Matcher\nstruct RegexMatcher\n".to_string(), builder.finish());

    let question =
        Question { file: m, position: Position { line: 0, col: 6 }, ask: Ask::Implementation { anchor } };
    let mut answers = Answers::new("toy", "toy-lsp");
    let root = Path::new("/p");
    let found = json!({ "uri": "file:///p/m.toy", "range": { "start": { "line": 1, "character": 7 } } });
    let again = record_answer(&mut answers, &index, [root, root], PositionEncoding::Utf16, &question, &found);
    assert!(again.is_empty(), "the location is a declaration: no second hop");
    let upserts = answers.finish().0.upsert_edges;
    assert!(upserts.is_empty(), "the structural edge already says it: {upserts:?}");
}

/// A call landing on a function is recorded wherever in it the answer
/// points: only a non-function target is held to its name.
///
/// Control: drop the `node.kind == NodeKind::Function` arm - the answer
/// inside `outer`'s body records nothing.
#[test]
fn a_call_answered_inside_a_function_still_lands_on_it() {
    let (index, caller) = landing_index();
    assert_eq!(answered_by(&index, &caller, site_of(&index, "len", 1), in_a(9, 17)).1, 1);
}

/// The id of `a.toy`'s node called `name` in [`landing_index`].
fn a_node(index: &SdkIndex, name: &str) -> String {
    let entry = index.entry(&RelPath::new("a.toy")).unwrap();
    entry.graph.nodes.iter().find(|node| node.name == name).unwrap().id.clone()
}

/// The `len` call site of line 1 (an untyped receiver call), moved into
/// `a.toy`'s `from`: an answer landing on `from` is a self-landing.
fn self_call(index: &SdkIndex, from: &str) -> (String, OpenSite) {
    let id = a_node(index, from);
    let mut site = site_of(index, "len", 1);
    site.from_id = id.clone();
    (id, site)
}

/// **GM-593.** A call answered inside its own caller, away from the
/// caller's name, is the enclosing-node fallback for a declaration with
/// no node of its own (a nested fn, a local): no self-loop. The site
/// still counts as answered, because the server did answer it.
///
/// Control: drop `&& node.id != site.from_id` in `record_answer` - one
/// edge `outer -> outer`.
#[test]
fn a_call_answered_inside_its_own_caller_records_no_self_loop() {
    let (index, _) = landing_index();
    let (outer, site) = self_call(&index, "outer");
    assert_eq!(answered_by(&index, &outer, site, in_a(9, 17)), (1, 0), "`inner` inside `outer`");
}

/// **GM-593.** A dropped self-loop still counts the site as answered:
/// the count is decided before the landing filter, from the
/// unfiltered target.
///
/// Control: in `record_answer`, call `untyped_call_answered` after the
/// filter with `landed.is_some()` - the count drops to 0, the answer
/// lies in an indexed file.
#[test]
fn a_dropped_self_loop_still_counts_as_answered() {
    let (index, _) = landing_index();
    let (outer, site) = self_call(&index, "outer");
    assert_eq!(answered_by(&index, &outer, site, in_a(9, 17)).0, 1);
}

/// **GM-593.** Recursion: a self-landing at the caller's own name
/// records the edge.
///
/// Control: drop every self-landing (option B: `node.id != site.from_id
/// &&` before the whole `CALLS` clause) - 0 edges.
#[test]
fn a_call_answered_at_its_own_callers_name_is_recursion() {
    let (index, _) = landing_index();
    let (outer, site) = self_call(&index, "outer");
    assert_eq!(answered_by(&index, &outer, site, in_a(9, 3)), (1, 1));
}

/// **GM-593.** An anonymous `export default` function (node `default`)
/// cannot call itself by name, so a self-landing in its body is always
/// the fallback: nothing recorded. A call into it from another function
/// is not a self-landing and is recorded.
///
/// Controls: drop `&& node.id != site.from_id` - the self-landing records
/// one edge; drop the `Function` arm (option A) - the call from `caller`
/// records nothing.
#[test]
fn a_self_landing_in_an_anonymous_default_export_records_nothing() {
    let (index, caller) = landing_index();
    let (default, site) = self_call(&index, "default");
    assert_eq!(answered_by(&index, &default, site, in_a(10, 23)).1, 0, "self-landing at `go`");
    assert_eq!(
        answered_by(&index, &caller, site_of(&index, "len", 1), in_a(10, 23)).1,
        1,
        "a call from `caller` into it"
    );
}

/// **GM-614, T1.** A `REFERENCES` answer inside its own referrer, away
/// from the referrer's name, is the enclosing-node fallback for a
/// declaration with no node of its own (a generic parameter): no edge.
///
/// Control: skip the rule for non-`CALLS` (`site.edge_kind != Calls ||`
/// as the first disjunct of the filter) - 1 edge.
#[test]
fn a_reference_landing_inside_its_own_encloser_is_not_recorded() {
    let (index, _) = landing_index();
    let outer = a_node(&index, "outer");
    let mut site = site_of(&index, "fld", 5);
    site.from_id = outer.clone();
    assert_eq!(answered_by(&index, &outer, site, in_a(9, 17)).1, 0);
}

/// **GM-614, T2.** A `REFERENCES` answer at the referrer's own name
/// (`Self`) is kept.
///
/// Control: drop the `lands_on_name` escape for enclosing landings - 0
/// edges.
#[test]
fn a_reference_landing_at_its_own_encloser_name_is_recorded() {
    let (index, _) = landing_index();
    let outer = a_node(&index, "outer");
    let mut site = site_of(&index, "fld", 5);
    site.from_id = outer.clone();
    assert_eq!(answered_by(&index, &outer, site, in_a(9, 3)).1, 1);
}

/// A `REFERENCES` site at `S::E` in `a.toy`'s `m`, inside `impl<S> Sink`.
fn reference_in_m(index: &SdkIndex) -> (String, OpenSite) {
    let m = a_node(index, "m");
    let mut site = site_of(index, "fld", 5);
    site.from_id = m.clone();
    site.position = Position { line: 12, col: 12 };
    (m, site)
}

/// **GM-614, T3.** A `REFERENCES` answer on a non-self node that
/// contains the site (the impl), away from its name, is recorded
/// nowhere.
///
/// Control: `encloses` is `node.id == site.from_id` only (option A) - 1
/// edge on the impl.
#[test]
fn a_reference_landing_on_a_non_self_encloser_is_not_recorded() {
    let (index, _) = landing_index();
    let (m, site) = reference_in_m(&index);
    assert_eq!(answered_in(&index, "a.toy", &m, site, in_a(11, 5)).1, 0, "`S` of `impl<S>`");
}

/// **GM-614, T4.** A `REFERENCES` answer inside a node that does not
/// enclose the site is recorded as before, away from the name too.
///
/// Control: hold every `REFERENCES` landing to its name (drop the
/// `!encloses` arm) - 0 edges.
#[test]
fn a_reference_landing_inside_a_node_that_does_not_enclose_it_is_recorded() {
    let (index, caller) = landing_index();
    assert_eq!(answered_by(&index, &caller, site_of(&index, "fld", 5), in_a(9, 17)).1, 1);
}

/// **GM-614, T5.** A `CALLS` answer on a non-self `Function` that
/// contains the site, away from its name, is recorded nowhere. The site
/// is placed inside `outer` while its referrer is `m`, as a nested
/// function's call is. GM-593's tests stay green.
///
/// Control: for `CALLS` use `node.id != site.from_id` in place of
/// `!encloses` - 1 edge on `outer`.
#[test]
fn a_call_landing_on_a_non_self_enclosing_function_is_not_recorded() {
    let (index, _) = landing_index();
    let m = a_node(&index, "m");
    let mut site = site_of(&index, "len", 1);
    site.from_id = m.clone();
    site.position = Position { line: 9, col: 14 };
    assert_eq!(answered_in(&index, "a.toy", &m, site, in_a(9, 17)).1, 0);
}

fn answered(counts: &[(&str, &str, usize)]) -> HashMap<(String, String), usize> {
    counts.iter().map(|(from, name, n)| ((from.to_string(), name.to_string()), *n)).collect()
}

fn b_toy() -> BTreeSet<RelPath> {
    BTreeSet::from([RelPath::new("b.toy")])
}

/// **GM-486.** A name leaves the list only when every untyped site of it
/// in that caller was answered: one of two `len` sites is not enough.
/// Only the listed files are considered.
///
/// Control: in `trim_untyped_calls`, keep a name only when `done == 0`
/// instead of `done < total` (`len` drops after one of its two sites).
#[test]
fn a_name_drops_only_when_every_one_of_its_sites_in_the_caller_was_answered() {
    let (index, caller) = untyped_index();
    let trim = |counts: &[(&str, &str, usize)], files: &BTreeSet<RelPath>| {
        trim_untyped_calls(&index, files, &answered(counts), &mut BTreeMap::new())
    };

    let one_len = trim(&[(&caller, "len", 1), (&caller, "add", 1)], &b_toy());
    assert_eq!(one_len.len(), 1, "{one_len:#?}");
    assert_eq!(one_len[0].id, caller);
    assert_eq!(one_len[0].untyped_calls, ["len"], "`add` is answered, one `len` site is still open");

    let both_len = trim(&[(&caller, "len", 2), (&caller, "add", 1)], &b_toy());
    assert_eq!(both_len[0].untyped_calls, Vec::<String>::new(), "every site answered");

    assert!(trim(&[(&caller, "len", 1)], &b_toy()).is_empty(), "nothing shorter, nothing re-sent");
    assert!(
        trim(&[(&caller, "add", 1)], &BTreeSet::from([RelPath::new("a.toy")])).is_empty(),
        "a file not listed is left as core holds it"
    );
}

/// **GM-486.** The re-sent caller is the structural node whole, with only
/// `untypedCalls` shorter: core's upsert replaces every column and child
/// row from the record, so anything less would erase what the structural
/// tier sent (declarations, qualified and alias paths, target, doc,
/// signature, container).
///
/// Control: in `trim_untyped_calls`, build the upsert as
/// `WireNode { untyped_calls: kept, declarations: None, alias_paths:
/// Vec::new(), ..node.clone() }`.
#[test]
fn a_re_sent_caller_keeps_every_other_column() {
    let (index, caller) = untyped_index();
    let original = index
        .entry(&RelPath::new("b.toy"))
        .unwrap()
        .graph
        .nodes
        .iter()
        .find(|n| n.id == caller)
        .unwrap()
        .clone();

    let upserts =
        trim_untyped_calls(&index, &b_toy(), &answered(&[(&caller, "add", 1)]), &mut BTreeMap::new());
    assert_eq!(upserts, vec![WireNode { untyped_calls: vec!["len".to_string()], ..original }]);
}

/// **GM-486.** Every pass starts from the structural list the index
/// holds: the same answers trim the same node again on the next pass
/// (undoing a structural reparse that re-sent the full list in between),
/// a name comes back once its answers stop, and a node that is neither
/// trimmed nor was trimmed is not re-sent.
///
/// Controls, in `trim_untyped_calls`: drop the `else if
/// previously.contains(..)` branch (pass 3 re-sends nothing, so `add`
/// never comes back); re-send a shortened node only when
/// `!previously.contains(&node.id)` (pass 2 re-sends the full list).
#[test]
fn each_pass_trims_from_the_structural_list_and_a_name_comes_back_when_its_answers_stop() {
    let (index, caller) = untyped_index();
    let mut trimmed = BTreeMap::new();
    let mut pass = |counts: &[(&str, &str, usize)]| {
        let upserts = trim_untyped_calls(&index, &b_toy(), &answered(counts), &mut trimmed);
        upserts.into_iter().map(|node| (node.id, node.untyped_calls)).collect::<Vec<_>>()
    };
    let sent =
        |names: &[&str]| vec![(caller.clone(), names.iter().map(|n| n.to_string()).collect::<Vec<_>>())];

    assert_eq!(pass(&[(&caller, "add", 1)]), sent(&["len"]), "pass 1 trims `add`");
    assert_eq!(pass(&[(&caller, "add", 1)]), sent(&["len"]), "pass 2 trims it again, from the full list");
    assert_eq!(pass(&[]), sent(&["add", "len"]), "pass 3: no answer, the full list goes back");
    assert_eq!(pass(&[]), Vec::new(), "pass 4: nothing trimmed now or before, nothing re-sent");
}

// --- which open sites a pass asks about ---------------------------------

fn line_range(line: u32) -> Range {
    Range { start: Position { line, col: 0 }, end: Position { line, col: 9 } }
}

/// What `m.toy`, the file a hop's placeholder names, declares under the
/// name `n`.
#[derive(Clone, Copy, Debug)]
enum Declares {
    Nothing,
    Function,
    /// `export const n = make()`: a Variable, the GM-596 shape.
    Variable,
    Type,
    Placeholder,
}

fn declaring_file(index: &mut SdkIndex, declares: Declares) {
    let m = RelPath::new("m.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &m);
    builder.file_node(line_range(0));
    match declares {
        Declares::Nothing => {}
        Declares::Function => {
            builder.add_node(
                NodeSpec::new(NodeKind::Function, "n", "n", line_range(0)).native_kind("function").public(),
            );
        }
        Declares::Variable => {
            builder.add_node(
                NodeSpec::new(NodeKind::Variable, "n", "n", line_range(0)).native_kind("const").public(),
            );
        }
        Declares::Type => {
            builder.add_node(
                NodeSpec::new(NodeKind::Type, "n", "n", line_range(0)).native_kind("class").public(),
            );
        }
        Declares::Placeholder => {
            builder.add_placeholder(
                PlaceholderKind::PendingSymbol,
                "n",
                file_name("o.toy", "n"),
                line_range(0),
            );
        }
    }
    index.insert(m, "fn n\n".to_string(), builder.finish());
}

/// `o.toy`, declaring an overload set named `n`: a node with
/// `declarations`, which is what makes an `OverloadCall` onto `n` worth
/// asking.
fn overload_file(index: &mut SdkIndex) {
    let o = RelPath::new("o.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &o);
    builder.file_node(line_range(0));
    let declaration = |ordinal| WireDeclaration {
        ordinal,
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 9,
        signature: None,
        has_body: false,
    };
    builder.add_node(
        NodeSpec::new(NodeKind::Function, "n", "n", line_range(0))
            .native_kind("function")
            .public()
            .declarations(vec![declaration(0), declaration(1)]),
    );
    index.insert(o, "fn n\n".to_string(), builder.finish());
}

fn file_name(file: &str, name: &str) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: TargetScope::File(file.to_string()),
        key: TargetKey::Name(name.to_string()),
        from_container: None,
        key_path: None,
        also_scopes: Vec::new(),
    }
}

/// What an open site of `u.toy` names in `replaces`.
#[derive(Clone, Copy, Debug)]
enum Replaces {
    TheEdge,
    AMissingEdge,
    Nothing,
}

/// `u.toy`: `f` with a `CALLS` edge onto a `kind` placeholder addressed by
/// `target`, and one open site per `(kind, replaces, line)`, all at
/// column 2 of `line`.
fn using_file(
    index: &mut SdkIndex,
    kind: PlaceholderKind,
    target: PlaceholderTarget,
    sites: &[(OpenSiteKind, Replaces, u32)],
) {
    using_file_by(index, EdgeKind::Calls, kind, target, sites);
}

/// [`using_file`] with an `edge_kind` edge (and sites) in place of
/// `CALLS`.
fn using_file_by(
    index: &mut SdkIndex,
    edge_kind: EdgeKind,
    kind: PlaceholderKind,
    target: PlaceholderTarget,
    sites: &[(OpenSiteKind, Replaces, u32)],
) {
    let u = RelPath::new("u.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &u);
    builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 4, col: 0 } });
    let f = builder.add_node(
        NodeSpec::new(NodeKind::Function, "f", "f", line_range(0)).native_kind("function").public(),
    );
    let placeholder = builder.add_placeholder(kind, "n", target, line_range(1));
    let edge = builder.placeholder_edge(edge_kind, &f, &placeholder);
    for (site_kind, replaces, line) in sites {
        builder.open_site(OpenSite {
            from_id: f.clone(),
            position: Position { line: *line, col: 2 },
            name: "n".to_string(),
            kind: *site_kind,
            edge_kind,
            from_container: None,
            replaces: match replaces {
                Replaces::TheEdge => Some(edge.clone()),
                Replaces::AMissingEdge => Some("e-missing".to_string()),
                Replaces::Nothing => None,
            },
        });
    }
    index.insert(u, "fn f\n  n()\n  n()\n  n()\n".to_string(), builder.finish());
}

/// What one pass over `u.toy` asks: each question's kind (`OverloadCall`
/// for an overload question) and line, sorted, and the unanswerable count.
fn asked_about_u(index: &SdkIndex) -> (Vec<(OpenSiteKind, u32)>, usize) {
    let questions =
        questions(index, &[RelPath::new("u.toy")], &SemanticConfig::new("toy-server"), &Budgets::default());
    let mut asked: Vec<(OpenSiteKind, u32)> = questions
        .asking
        .iter()
        .map(|question| match &question.ask {
            Ask::Definition(site) => (site.kind, site.position.line),
            Ask::Overload(site) => (OpenSiteKind::OverloadCall, site.position.line),
            other => panic!("only open sites are asked here: {other:?}"),
        })
        .collect();
    asked.sort_by_key(|(kind, line)| (format!("{kind:?}"), *line));
    (asked, questions.unanswerable)
}

/// A hop - a `Reference` site whose `replaces` names an edge onto a
/// `pending_symbol` placeholder addressed by file and bare name - is asked
/// only while that file is indexed and declares nothing addressable of
/// that name. Every other hop is left to its structural edge, and none of
/// them counts as unanswerable.
#[test]
fn a_hop_is_asked_only_while_the_linker_cannot_settle_its_placeholder() {
    use PlaceholderKind::{PendingSymbol, Reexport};
    let qualified = PlaceholderTarget {
        scope: TargetScope::File("m.toy".to_string()),
        key: TargetKey::QualifiedName("n".to_string()),
        from_container: None,
        key_path: None,
        also_scopes: Vec::new(),
    };
    let container = PlaceholderTarget {
        scope: TargetScope::Container("m".to_string()),
        key: TargetKey::Name("n".to_string()),
        from_container: None,
        key_path: None,
        also_scopes: Vec::new(),
    };
    let cases = [
        (
            "m.toy declares no n",
            Declares::Nothing,
            PendingSymbol,
            file_name("m.toy", "n"),
            Replaces::TheEdge,
            1,
        ),
        (
            "m.toy's n is only a placeholder",
            Declares::Placeholder,
            PendingSymbol,
            file_name("m.toy", "n"),
            Replaces::TheEdge,
            1,
        ),
        (
            "m.toy declares n",
            Declares::Function,
            PendingSymbol,
            file_name("m.toy", "n"),
            Replaces::TheEdge,
            0,
        ),
        (
            "the file is not indexed",
            Declares::Nothing,
            PendingSymbol,
            file_name("gone.toy", "n"),
            Replaces::TheEdge,
            0,
        ),
        ("a container scope", Declares::Nothing, PendingSymbol, container, Replaces::TheEdge, 0),
        ("a qualified-name key", Declares::Nothing, PendingSymbol, qualified, Replaces::TheEdge, 0),
        (
            "a reexport placeholder",
            Declares::Nothing,
            Reexport,
            file_name("m.toy", "n"),
            Replaces::TheEdge,
            0,
        ),
        (
            "the edge is missing",
            Declares::Nothing,
            PendingSymbol,
            file_name("m.toy", "n"),
            Replaces::AMissingEdge,
            0,
        ),
    ];
    for (case, declares, kind, target, replaces, expected) in cases {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, declares);
        using_file(&mut index, kind, target, &[(OpenSiteKind::Reference, replaces, 1)]);
        let (asked, unanswerable) = asked_about_u(&index);
        let want: Vec<(OpenSiteKind, u32)> = (0..expected).map(|_| (OpenSiteKind::Reference, 1)).collect();
        assert_eq!(asked, want, "{case}");
        assert_eq!(unanswerable, 0, "a hop not asked is not unanswerable: {case}");
    }
}

/// **GM-596.** Whether a hop is settled depends on the edge's kind as
/// well as on what `m.toy` declares as `n`: the linker lands a syntactic
/// edge on a name key only on a kind core's structural rule accepts -
/// `CALLS` on a `Function`, `SUPERTYPE_OF` on a `Type` or a `Function`,
/// any other kind on anything addressable. A hop the linker would leave
/// unlinked is asked; one it settles is not.
///
/// Controls, each in `unsettled_hop`: `accepts` returns `true` for every
/// kind (the `CALLS`/`SUPERTYPE_OF` onto a Variable rows are not asked);
/// `accepts` applies the `CALLS` rule to every edge kind (the
/// `REFERENCES` onto a Variable row and the `SUPERTYPE_OF` onto a Type
/// row are asked).
#[test]
fn a_hop_onto_a_declaration_the_edge_kind_does_not_accept_is_asked() {
    let cases = [
        (EdgeKind::Calls, Declares::Variable, 1, "a call of an imported const"),
        (EdgeKind::Calls, Declares::Function, 0, "a call of an imported function"),
        (EdgeKind::SupertypeOf, Declares::Variable, 1, "a supertype named after a const"),
        (EdgeKind::SupertypeOf, Declares::Type, 0, "a supertype that is a type"),
        (EdgeKind::SupertypeOf, Declares::Function, 0, "a supertype that is a function"),
        (EdgeKind::References, Declares::Variable, 0, "a reference to a const"),
    ];
    for (edge_kind, declares, expected, case) in cases {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, declares);
        using_file_by(
            &mut index,
            edge_kind,
            PlaceholderKind::PendingSymbol,
            file_name("m.toy", "n"),
            &[(OpenSiteKind::Reference, Replaces::TheEdge, 1)],
        );
        let (asked, unanswerable) = asked_about_u(&index);
        let want: Vec<(OpenSiteKind, u32)> = (0..expected).map(|_| (OpenSiteKind::Reference, 1)).collect();
        assert_eq!(asked, want, "{case} ({edge_kind:?} onto {declares:?})");
        assert_eq!(unanswerable, 0, "{case}");
    }
}

/// **GM-596.** Whether a `CALLS` hop to `handler` is settled follows the
/// linker's `Resolver::choose` order: type members go whenever a
/// non-member shares the name, and only then does the kind rule apply.
/// So `Router#handler` (a Function member) does not settle a call of
/// `export const handler = make()`, but settles it when it is the only
/// `handler`, and a top-level `function handler` settles it beside the
/// member.
///
/// Control in `unsettled_hop`: replace `(keep_members || !member) &&
/// accepts(..)` with `accepts(..)` (the const-plus-member row is then
/// settled, so not asked).
#[test]
fn a_hop_by_name_drops_type_members_before_the_kind_rule() {
    let cases = [
        (Some(NodeKind::Variable), 1, "a const beside a method of the same name"),
        (None, 0, "a method alone"),
        (Some(NodeKind::Function), 0, "a function beside a method of the same name"),
    ];
    for (top_level, expected, case) in cases {
        let mut index = SdkIndex::new();
        let m = RelPath::new("m.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &m);
        builder.file_node(line_range(0));
        builder.add_node(
            NodeSpec::new(NodeKind::Type, "Router", "Router", line_range(0)).native_kind("class").public(),
        );
        let member = g_mesh_wire::QualifiedPath::root("Router").child("#", "handler");
        builder.add_node(
            NodeSpec::with_path(NodeKind::Function, "handler", member, line_range(0))
                .native_kind("method")
                .public(),
        );
        if let Some(kind) = top_level {
            builder.add_node(
                NodeSpec::new(kind, "handler", "handler", line_range(0)).native_kind("const").public(),
            );
        }
        index.insert(m, "class Router {}\n".to_string(), builder.finish());
        using_file_by(
            &mut index,
            EdgeKind::Calls,
            PlaceholderKind::PendingSymbol,
            file_name("m.toy", "handler"),
            &[(OpenSiteKind::Reference, Replaces::TheEdge, 1)],
        );
        let (asked, unanswerable) = asked_about_u(&index);
        let want: Vec<(OpenSiteKind, u32)> = (0..expected).map(|_| (OpenSiteKind::Reference, 1)).collect();
        assert_eq!(asked, want, "{case}");
        assert_eq!(unanswerable, 0, "{case}");
    }
}

/// A `Reference` site with no `replaces`, a `ReceiverCall` and a
/// `ReceiverField` (GM-497) are asked whatever the placeholder looks
/// like: the hop rule is about a `Reference`'s `replaces`. Control for
/// the `ReceiverField` row: route `ReceiverField` to `continue` in
/// `questions`.
#[test]
fn a_reference_without_replaces_and_a_receiver_call_are_asked_as_before() {
    let mut index = SdkIndex::new();
    declaring_file(&mut index, Declares::Function);
    using_file(
        &mut index,
        PlaceholderKind::PendingSymbol,
        file_name("m.toy", "n"),
        &[
            (OpenSiteKind::Reference, Replaces::Nothing, 1),
            (OpenSiteKind::ReceiverCall, Replaces::Nothing, 2),
            (OpenSiteKind::ReceiverCall, Replaces::TheEdge, 3),
            (OpenSiteKind::ReceiverField, Replaces::TheEdge, 4),
        ],
    );
    let (asked, unanswerable) = asked_about_u(&index);
    assert_eq!(
        asked,
        vec![
            (OpenSiteKind::ReceiverCall, 2),
            (OpenSiteKind::ReceiverCall, 3),
            (OpenSiteKind::ReceiverField, 4),
            (OpenSiteKind::Reference, 1)
        ]
    );
    assert_eq!(unanswerable, 0);
}

/// **GM-497, items 10, 11 and 16.** Onto the placeholder a typed Rust field
/// read replaces - a `Container` scope with a `QualifiedName` key - a
/// `ReceiverField` site is asked with or without `replaces`, while a
/// `Reference` hop with the same `replaces` is still left to its
/// structural edge. Controls: route `ReceiverField` to `continue` in
/// `questions` (lines 2 and 3 are not asked); give `ReceiverField` the
/// hop arm (`ReceiverField if site.replaces.is_some()` beside
/// `Reference`: line 2 is not asked); route a `Reference` with `replaces`
/// to `Ask::Definition` unconditionally (line 1 is asked).
#[test]
fn a_receiver_field_is_asked_onto_the_placeholder_a_hop_is_not() {
    let mut index = SdkIndex::new();
    declaring_file(&mut index, Declares::Nothing);
    let field = PlaceholderTarget {
        scope: TargetScope::Container("m".to_string()),
        key: TargetKey::QualifiedName("T.n".to_string()),
        from_container: None,
        key_path: None,
        also_scopes: Vec::new(),
    };
    using_file(
        &mut index,
        PlaceholderKind::PendingSymbol,
        field,
        &[
            (OpenSiteKind::Reference, Replaces::TheEdge, 1),
            (OpenSiteKind::ReceiverField, Replaces::TheEdge, 2),
            (OpenSiteKind::ReceiverField, Replaces::Nothing, 3),
        ],
    );
    let (asked, unanswerable) = asked_about_u(&index);
    assert_eq!(asked, vec![(OpenSiteKind::ReceiverField, 2), (OpenSiteKind::ReceiverField, 3)]);
    assert_eq!(unanswerable, 0);
}

/// A hop at the same call (enclosing node and position) as an overload
/// question that was kept is dropped, whichever of the two sites was
/// recorded first; a hop elsewhere stays.
#[test]
fn a_hop_at_a_kept_overload_call_is_dropped_in_either_order() {
    let hop = (OpenSiteKind::Reference, Replaces::TheEdge, 1);
    let overload = (OpenSiteKind::OverloadCall, Replaces::TheEdge, 1);
    let elsewhere = (OpenSiteKind::OverloadCall, Replaces::TheEdge, 2);
    for sites in [[hop, overload, elsewhere], [overload, hop, elsewhere], [elsewhere, overload, hop]] {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, Declares::Nothing);
        overload_file(&mut index);
        using_file(&mut index, PlaceholderKind::PendingSymbol, file_name("m.toy", "n"), &sites);
        let (asked, unanswerable) = asked_about_u(&index);
        assert_eq!(
            asked,
            vec![(OpenSiteKind::OverloadCall, 1), (OpenSiteKind::OverloadCall, 2)],
            "{sites:?}"
        );
        assert_eq!(unanswerable, 0);
    }

    let mut index = SdkIndex::new();
    declaring_file(&mut index, Declares::Nothing);
    overload_file(&mut index);
    using_file(
        &mut index,
        PlaceholderKind::PendingSymbol,
        file_name("m.toy", "n"),
        &[(OpenSiteKind::OverloadCall, Replaces::TheEdge, 2), hop],
    );
    let (asked, _) = asked_about_u(&index);
    assert_eq!(
        asked,
        vec![(OpenSiteKind::OverloadCall, 2), (OpenSiteKind::Reference, 1)],
        "a hop at another call"
    );
}

/// When no overload set answers to the call's name, its `OverloadCall`
/// site is filtered out and the hop at the same call is asked instead.
#[test]
fn a_hop_at_a_filtered_overload_call_is_asked() {
    for sites in [
        [(OpenSiteKind::Reference, Replaces::TheEdge, 1), (OpenSiteKind::OverloadCall, Replaces::TheEdge, 1)],
        [(OpenSiteKind::OverloadCall, Replaces::TheEdge, 1), (OpenSiteKind::Reference, Replaces::TheEdge, 1)],
    ] {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, Declares::Nothing);
        using_file(&mut index, PlaceholderKind::PendingSymbol, file_name("m.toy", "n"), &sites);
        let (asked, unanswerable) = asked_about_u(&index);
        assert_eq!(asked, vec![(OpenSiteKind::Reference, 1)], "{sites:?}");
        assert_eq!(unanswerable, 0);
    }
}

/// `didOpen`'s `languageId` is the first pair whose extension the path
/// ends with, else the bridge's language.
#[test]
fn a_document_opens_under_the_first_matching_language_id() {
    let ids = [(".tsx", "typescriptreact"), (".x.tsx", "never-reached"), (".js", "javascript")];
    let id = |path: &str| language_id(&RelPath::new(path), "typescript", &ids);
    assert_eq!(id("src/a.tsx"), "typescriptreact");
    assert_eq!(id("src/a.x.tsx"), "typescriptreact", "the first pair wins");
    assert_eq!(id("src/a.js"), "javascript");
    assert_eq!(id("src/a.ts"), "typescript");
    assert_eq!(language_id(&RelPath::new("src/a.tsx"), "typescript", &[]), "typescript");
}

/// GM-618: a path call is asked at its last segment, and so is a
/// `References` path, but what the site covers is the whole
/// written path. The range is found on the source
/// line from the asked column, not started there.
///
/// Control: in `site_range`, make the found start unused
/// (`written_start(..).and(None)` after the `and_then`); the path rows
/// start at the asked column and overshoot the written text.
#[test]
fn a_site_asked_at_the_last_segment_covers_the_whole_written_path() {
    let at = |line: u32, col: u32| Position { line, col };
    let covers = |source: Option<&str>, position: Position, written: &str| {
        let range = site_range(source, position, written);
        ((range.start.line, range.start.col), (range.end.line, range.end.col))
    };
    let path = "    super::Q::<u8>::new();";
    let source = format!("fn run() {{\n{path}\n}}\n");
    let asked = at(1, path.find("new").unwrap() as u32);
    // The written path, not [asked, asked + 19).
    assert_eq!(covers(Some(&source), asked, "super::Q::<u8>::new"), ((1, 4), (1, 23)));
    // A `References` path is asked at its last segment too: the range
    // still starts at the path's start, past generic arguments in the
    // middle and up to a raw identifier at the end.
    let typed = "    let _: super::a::W<u8>::X = x;";
    let asked_x = at(0, typed.rfind('X').unwrap() as u32);
    assert_eq!(covers(Some(typed), asked_x, "super::a::W<u8>::X"), ((0, 11), (0, 29)));
    let raw = "    let _ = super::q::r#type;";
    let asked_raw = at(0, raw.find("r#type").unwrap() as u32);
    assert_eq!(covers(Some(raw), asked_raw, "super::q::r#type"), ((0, 12), (0, 28)));
    // A site asked at its own start is unchanged.
    let own = "    foo();";
    assert_eq!(covers(Some(&format!("{own}\n")), at(0, 4), "foo"), ((0, 4), (0, 7)));
    // The path's own text is counted in characters, after a wide prefix.
    let wide = "    let 🦀 = a::b::f();";
    assert_eq!(covers(Some(&format!("{wide}\n")), at(0, 18), "a::b::f"), ((0, 12), (0, 19)));
    // The occurrence that covers the asked column wins, not the first one on the line.
    let twice = "a::f(); a::f();";
    assert_eq!(covers(Some(twice), at(0, 11), "a::f"), ((0, 8), (0, 12)));
    // No source, or the path not written on the line (split across
    // lines): the asked column starts the range, as before.
    assert_eq!(covers(None, asked, "super::Q::<u8>::new"), ((1, 20), (1, 39)));
    assert_eq!(covers(Some("fn run() {\n    new();\n}\n"), at(1, 4), "super::Q::new"), ((1, 4), (1, 17)));
    assert_eq!(covers(Some("x\n"), at(5, 2), "a::f"), ((5, 2), (5, 6)));
}
