//! The contract every plugin is held to, as pure functions over what
//! `session` recorded.
//!
//! Nothing in here spawns, reads a pipe or writes the index: each check reads
//! the bulk streams and the control-plane transcript and returns a verdict.
//! That split is what makes each rule unit-testable against a hand-built
//! transcript (this module's tests) *and* provable end to end by a fake plugin
//! that breaks only that rule (`core/tests/plugin_check.rs`).
//!
//! # The checks, and the evidence each one reads
//!
//! Source of truth for the rules: `docs/architecture/multi-language-plugins.md`,
//! "Constraints" and "Interfaces > Conformance kit".
//!
//! - **`session`** - the plugin can be run at all: spawns, handshakes (protocol
//!   version and language, as the daemon verifies them), finishes both bulk
//!   walks with exit 0, and answers every request within
//!   `RoundTripTimeouts`. Everything else depends on it; a check whose input a
//!   failed session never produced is skipped, not passed.
//! - **`shape`** - every bulk line and every response parses as the protocol
//!   v2 shape *after* core's own normalization (`WireNode`/`WireEdge`
//!   deserialize), and a placeholder `nativeKind` carries a `target`. Legacy
//!   v1 fields are a warning on this check, not a failure - see
//!   [`legacy_v1_warning`].
//! - **`stream-order`** - in the structural stream (bulk, `fileChanged`
//!   diffs), an edge's `fromId` and `toId` are nodes of the edge's own file,
//!   emitted before the edge. This is "edges never leave their file": a bulk
//!   batch can be cut anywhere, so an edge may only lean on what an earlier
//!   line already delivered.
//! - **`same-file-rule`** - a structural edge between two nodes of one file is
//!   `resolved: true` when it lands on a declaration and `resolved: false`
//!   when it lands on a placeholder: within its own file a plugin has nothing
//!   left to confirm, and across it core has everything left to confirm.
//! - **`id-stability.bulk-repeat`** - two bulk runs over the same tree emit the
//!   same node and edge id sets.
//! - **`id-stability.whitespace-edit`** - the `fileChanged` answering a
//!   whitespace-only edit (`session::whitespace_edit` has why that edit) is
//!   empty in all four lists.
//! - **`id-stability.deletes-known`** - every `deleteNodeIds` entry names an id
//!   the plugin had emitted before that diff (either bulk run, or an earlier
//!   diff's `upsertNodes`). Judged on every answered diff, but only reported
//!   once the emptied-file step - the one that exercises deletes - has run.
//! - **`id-stability.incremental-matches-bulk`** - after `fileChanged` has
//!   seen the file unmodified, then whitespace-edited, emptied and restored,
//!   the file's node ids in the linked index are exactly what they were right
//!   after the bulk walk was committed and linked (index against index, since
//!   linking legitimately drops a linked `resolved_module` placeholder - see
//!   `session::file_node_ids`). Not in the design doc's list, added here
//!   because it is the failure that costs the most and is otherwise
//!   invisible: a plugin whose incremental path derives ids differently from
//!   its bulk path never deletes the bulk rows, so every edited file silently
//!   accumulates duplicates.
//! - **`ownership.defines-exports-from-file`** - `DEFINES`/`EXPORTS` edges run
//!   from the file's `File` node. Core writes container `DEFINES` edges
//!   itself; a plugin's own always start at a file.
//! - **`ownership.language`** - every node's `language` is the manifest's.
//!   Core keys per-language state (`language_state`, semantic scheduling,
//!   file counts) on it.
//! - **`ownership.no-container`** - no plugin emits `nativeKind: "container"`;
//!   core alone materializes containers.
//! - **`ownership.diff-stays-in-file`** - a `fileChanged` diff for file F
//!   upserts only F's nodes and deletes none of another file's: "a plugin's
//!   diff upserts and deletes nodes by id for one file at a time".
//! - **`capabilities.semantic-pass-undeclared`** (only for `semantic_pass =
//!   false`) - no `semanticPass` frame is ever written to the plugin, and the
//!   plugin never signals starting a semantic engine core will never ask for.
//! - **`capabilities.semantic-engine-lazy`** (only for `semantic_pass =
//!   true`) - the semantic-engine marker (see `session::MARKER_DIR_ENV`) did
//!   not exist yet when the first `semanticPass` frame was written.
//!
//! # Why `semanticPass` diffs are held to fewer rules
//!
//! `stream-order`, `same-file-rule` and `ownership.diff-stays-in-file` read
//! the *structural* stream only. A semantic answer is allowed to cross files:
//! the TS plugin's re-export upgrade re-sends an existing edge with a real
//! `toId` in another file and sends that target node along
//! (`plugins/typescript/src/semanticPass.ts`, "Why the first emits a
//! placeholder and the second a real node id"), and `apply_diff` commits it by
//! id. The constraint the design states is "edges never leave their file *in
//! the structural stream*". Shape, ownership of `language`/containers/
//! `DEFINES`, and `deleteNodeIds` still apply to every diff.
//!
//! # "Not instrumented" is not a pass
//!
//! The lazy-engine check can only fail on evidence: a marker file that was
//! already there. A plugin that never writes the marker gives none either way,
//! so the check is skipped with that reason rather than passed. A plugin whose
//! marker appears only after the first `semanticPass` passes.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::cli::plugin_check::report::{CheckResult, Outcome};
use crate::cli::plugin_check::session::{BulkLine, BulkRun, EditTarget, Exchange, Method, Session};
use crate::daemon::manifest::PluginManifest;
use crate::protocol::conformance::{
    placeholder_target_violation, plugin_emitted_container_violation, PLACEHOLDER_NATIVE_KINDS,
};
use crate::protocol::ndjson::BulkItem;
use crate::protocol::types::{EdgeKind, FileChangeDiff, NodeKind, WireEdge, WireNode};

/// The TS plugin's `nativeKind` for an import it could not resolve to a file
/// of the project (a package, a node builtin). Not one of core's placeholder
/// kinds - core stores it as an ordinary `Module` row and never links it - but
/// that is exactly why an edge onto it is a placeholder edge for the
/// same-file rule: nothing will ever confirm it, so `resolved: true` would be
/// a false claim.
const EXTERNAL_MODULE_NATIVE_KIND: &str = "external_module";

fn is_placeholder(native_kind: Option<&str>) -> bool {
    native_kind
        .is_some_and(|kind| PLACEHOLDER_NATIVE_KINDS.contains(&kind) || kind == EXTERNAL_MODULE_NATIVE_KIND)
}

/// Everything a run produced, as `checks` reads it.
pub(crate) struct RunData<'a> {
    pub manifest: &'a PluginManifest,
    pub bulk: [&'a BulkRun; 2],
    pub target: Option<&'a EditTarget>,
    /// The edited file's node ids in the index right after the bulk walk was
    /// committed and linked (`session::file_node_ids`).
    pub bulk_file_ids: Option<&'a BTreeSet<String>>,
    pub session: Option<&'a Session>,
    /// Setup and session failures, in the order they happened.
    pub failures: Vec<String>,
    pub marker_exists_at_end: bool,
}

/// What a node needs to be to be judged: which file it belongs to, and what
/// it is.
#[derive(Clone)]
struct NodeInfo {
    file: String,
    kind: NodeKind,
    native_kind: Option<String>,
}

impl NodeInfo {
    fn of(node: &WireNode) -> Self {
        Self { file: node.file_path.clone(), kind: node.kind, native_kind: node.native_kind.clone() }
    }
}

fn edge_kind(kind: EdgeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn describe_edge(edge: &WireEdge) -> String {
    format!("edge {:?} ({} {:?} -> {:?})", edge.id, edge_kind(edge.kind), edge.from_id, edge.to_id)
}

fn verdict(findings: Vec<String>) -> Outcome {
    if findings.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(findings)
    }
}

fn result(id: &'static str, outcome: Outcome) -> CheckResult {
    CheckResult { id, outcome, warnings: Vec::new() }
}

const BULK_INCOMPLETE: &str = "not reached: bulk run 1 did not complete (see `session`)";

pub(crate) fn evaluate(run: &RunData) -> Vec<CheckResult> {
    let bulk1 = run.bulk[0];
    let answered = answered_diffs(run.session);
    let walk = walk_diffs(run, &answered);

    let mut results = vec![
        result("session", verdict(run.failures.clone())),
        shape(run, &answered),
        structural(run, "stream-order", |findings| {
            bulk_stream_order(&bulk1.lines, findings);
            findings.extend(walk.stream_order.iter().cloned());
        }),
        structural(run, "same-file-rule", |findings| {
            bulk_same_file_rule(&bulk1.lines, findings);
            findings.extend(walk.same_file.iter().cloned());
        }),
        bulk_repeat(run),
        whitespace_edit(run),
        deletes_known(run, &walk),
        incremental_matches_bulk(run),
        structural(run, "ownership.defines-exports-from-file", |findings| {
            bulk_defines_from_file(&bulk1.lines, findings);
            findings.extend(walk.defines.iter().cloned());
        }),
        structural(run, "ownership.language", |findings| {
            for (context, node) in bulk_nodes(&bulk1.lines) {
                language_finding(run.manifest, &context, node, findings);
            }
            for (step, diff) in &answered {
                for node in &diff.upsert_nodes {
                    language_finding(run.manifest, step, node, findings);
                }
            }
        }),
        structural(run, "ownership.no-container", |findings| {
            let nodes = bulk_nodes(&bulk1.lines).into_iter().chain(
                answered
                    .iter()
                    .flat_map(|(step, diff)| diff.upsert_nodes.iter().map(|n| (step.to_string(), n))),
            );
            for (context, node) in nodes {
                if let Some(message) = plugin_emitted_container_violation(node) {
                    findings.push(format!("{context}: {message}"));
                }
            }
        }),
        diff_stays_in_file(run, &walk),
    ];
    results.extend(capabilities(run));
    results
}

/// A check over bulk run 1 plus whatever structural diffs were answered -
/// skipped when bulk run 1 did not complete, since a stream cut short by a
/// kill can end before the nodes its edges were promised.
fn structural(run: &RunData, id: &'static str, collect: impl FnOnce(&mut Vec<String>)) -> CheckResult {
    if !run.bulk[0].complete() {
        return result(id, Outcome::Skip(BULK_INCOMPLETE.to_string()));
    }
    let mut findings = Vec::new();
    collect(&mut findings);
    result(id, verdict(findings))
}

/// Every answered exchange whose response parsed, as (step label, diff).
fn answered_diffs(session: Option<&Session>) -> Vec<(String, &FileChangeDiff)> {
    let Some(session) = session else { return Vec::new() };
    session
        .exchanges
        .iter()
        .filter_map(|exchange| {
            let diff = exchange.response.as_ref()?.diff.as_ref().ok()?;
            Some((format!("{} {} response", exchange.step, exchange.method.name()), diff))
        })
        .collect()
}

fn bulk_nodes(lines: &[BulkLine]) -> Vec<(String, &WireNode)> {
    lines
        .iter()
        .filter_map(|line| match &line.item {
            Ok(BulkItem::Node(node)) => {
                Some((format!("bulk run 1, NDJSON line {}", line.line_no), node.as_ref()))
            }
            _ => None,
        })
        .collect()
}

// --- shape ------------------------------------------------------------------

fn shape(run: &RunData, answered: &[(String, &FileChangeDiff)]) -> CheckResult {
    let bulk1 = run.bulk[0];
    let session_has_responses = run.session.is_some_and(|s| s.exchanges.iter().any(|e| e.response.is_some()));
    if !bulk1.complete() && !session_has_responses {
        return result("shape", Outcome::Skip(BULK_INCOMPLETE.to_string()));
    }

    let mut findings = Vec::new();
    if bulk1.complete() {
        for line in &bulk1.lines {
            match &line.item {
                Err(err) => findings.push(format!("bulk run 1, NDJSON line {}: {err}", line.line_no)),
                Ok(BulkItem::Node(node)) => {
                    if let Some(message) = placeholder_target_violation(node) {
                        findings.push(format!("bulk run 1, NDJSON line {}: {message}", line.line_no));
                    }
                }
                Ok(BulkItem::Edge(_)) => {}
            }
        }
    }
    if let Some(session) = run.session {
        findings.extend(session.malformed_frames.iter().cloned());
        for exchange in &session.exchanges {
            if let Some(Err(err)) = exchange.response.as_ref().map(|r| &r.diff) {
                findings.push(format!(
                    "{} {} response is not a valid FileChangeResponse: {err}",
                    exchange.step,
                    exchange.method.name()
                ));
            }
        }
    }
    for (step, diff) in answered {
        for node in &diff.upsert_nodes {
            if let Some(message) = placeholder_target_violation(node) {
                findings.push(format!("{step}: {message}"));
            }
        }
    }

    let mut check = result("shape", verdict(findings));
    check.warnings.extend(legacy_v1_warning(run));
    check
}

/// LEGACY-V1: remove in GM-275, together with its call site in [`shape`].
///
/// `WireNode`/`WireEdge` still accept the v1 wire shape and normalize it
/// (`protocol::types`' own `LEGACY-V1` sites), so a v1 plugin passes `shape`
/// exactly as core would load it - which is what lets the TS plugin pass this
/// kit before GM-275 migrates it. But a conformance kit that silently accepts
/// the old shape would give GM-275 nothing to flip, so the use is counted
/// from the *raw* JSON (which the normalizing deserializer erases) and
/// reported as a warning. GM-275 turns this into a `shape` failure by moving
/// these counts into findings, then deletes the normalization.
fn legacy_v1_warning(run: &RunData) -> Option<String> {
    let mut exported = 0usize;
    let mut source_without_engine = 0usize;
    let mut derived_targets = 0usize;

    let mut inspect_node = |raw: &serde_json::Value| {
        let has = |key: &str| raw.get(key).is_some_and(|v| !v.is_null());
        if has("exported") && !has("visibility") {
            exported += 1;
        }
        let native_kind = raw.get("nativeKind").and_then(|v| v.as_str());
        if native_kind.is_some_and(|kind| PLACEHOLDER_NATIVE_KINDS.contains(&kind)) && !has("target") {
            derived_targets += 1;
        }
    };
    let mut edges = Vec::new();

    if run.bulk[0].complete() {
        for line in &run.bulk[0].lines {
            let Some(raw) = &line.raw else { continue };
            match &line.item {
                Ok(BulkItem::Node(_)) => inspect_node(raw),
                Ok(BulkItem::Edge(_)) => edges.push(raw.clone()),
                Err(_) => {}
            }
        }
    }
    if let Some(session) = run.session {
        for response in session.exchanges.iter().filter_map(|e| e.response.as_ref()) {
            let result = response.raw.get("result");
            for node in
                result.and_then(|r| r.get("upsertNodes")).and_then(|v| v.as_array()).into_iter().flatten()
            {
                inspect_node(node);
            }
            for edge in
                result.and_then(|r| r.get("upsertEdges")).and_then(|v| v.as_array()).into_iter().flatten()
            {
                edges.push(edge.clone());
            }
        }
    }
    for edge in &edges {
        if edge.get("source").is_some() && edge.get("engine").is_none_or(|v| v.is_null()) {
            source_without_engine += 1;
        }
    }

    let uses: Vec<String> = [
        (exported, "`exported` without `visibility` on {} node(s)"),
        (source_without_engine, "`source` without `engine` on {} edge(s)"),
        (
            derived_targets,
            "a placeholder `target` derived from the `<file>#<name>` qualifiedName convention on {} node(s)",
        ),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, text)| text.replace("{}", &count.to_string()))
    .collect();
    if uses.is_empty() {
        return None;
    }
    Some(format!(
        "legacy protocol v1 wire fields, accepted as core normalizes them until GM-275 makes v2 mandatory: {}",
        uses.join(", ")
    ))
}

// --- bulk-stream rules ------------------------------------------------------

/// First occurrence of every node id in one bulk stream: (line, info).
fn bulk_node_index(lines: &[BulkLine]) -> HashMap<&str, (usize, NodeInfo)> {
    let mut index = HashMap::new();
    for line in lines {
        if let Ok(BulkItem::Node(node)) = &line.item {
            index.entry(node.id.as_str()).or_insert_with(|| (line.line_no, NodeInfo::of(node)));
        }
    }
    index
}

fn bulk_edges(lines: &[BulkLine]) -> impl Iterator<Item = (usize, &WireEdge)> {
    lines.iter().filter_map(|line| match &line.item {
        Ok(BulkItem::Edge(edge)) => Some((line.line_no, edge)),
        _ => None,
    })
}

fn bulk_stream_order(lines: &[BulkLine], findings: &mut Vec<String>) {
    let index = bulk_node_index(lines);
    for (line_no, edge) in bulk_edges(lines) {
        let at = format!("bulk run 1, NDJSON line {line_no}");
        let from = index.get(edge.from_id.as_str());
        match from {
            None => {
                findings.push(format!("{at}: {} - fromId is never emitted as a node", describe_edge(edge)))
            }
            Some((from_line, _)) if *from_line > line_no => findings.push(format!(
                "{at}: {} - fromId is only emitted later, at line {from_line}",
                describe_edge(edge)
            )),
            Some(_) => {}
        }
        let edge_file = from.map(|(_, info)| info.file.as_str());
        match index.get(edge.to_id.as_str()) {
            None => findings.push(format!(
                "{at}: {} - toId is never emitted as a node; an edge may only target a node of its own file",
                describe_edge(edge)
            )),
            Some((_, to)) if edge_file.is_some_and(|file| file != to.file) => findings.push(format!(
                "{at}: {} - toId is a node of {:?}, not of the edge's own file {:?}; edges never leave their file",
                describe_edge(edge),
                to.file,
                edge_file.unwrap_or_default()
            )),
            Some((to_line, _)) if *to_line > line_no => findings.push(format!(
                "{at}: {} - toId (same file) is only emitted later, at line {to_line}",
                describe_edge(edge)
            )),
            Some(_) => {}
        }
    }
}

/// The same-file rule for one edge whose endpoints are both known; `None`
/// when it holds or does not apply.
fn same_file_violation(edge: &WireEdge, from: &NodeInfo, to: &NodeInfo) -> Option<String> {
    if from.file != to.file {
        return None;
    }
    let onto_placeholder = is_placeholder(to.native_kind.as_deref());
    match (onto_placeholder, edge.resolved) {
        (true, true) => Some(format!(
            "{} is `resolved: true` but lands on a placeholder (nativeKind {:?}) - only core can confirm it",
            describe_edge(edge),
            to.native_kind.as_deref().unwrap_or_default()
        )),
        (false, false) => Some(format!(
            "{} is `resolved: false` but lands on a declaration of its own file {:?} - nothing is left to confirm",
            describe_edge(edge),
            to.file
        )),
        _ => None,
    }
}

fn bulk_same_file_rule(lines: &[BulkLine], findings: &mut Vec<String>) {
    let index = bulk_node_index(lines);
    for (line_no, edge) in bulk_edges(lines) {
        if let (Some((_, from)), Some((_, to))) =
            (index.get(edge.from_id.as_str()), index.get(edge.to_id.as_str()))
        {
            if let Some(message) = same_file_violation(edge, from, to) {
                findings.push(format!("bulk run 1, NDJSON line {line_no}: {message}"));
            }
        }
    }
}

fn defines_violation(edge: &WireEdge, from: &NodeInfo) -> Option<String> {
    (matches!(edge.kind, EdgeKind::Defines | EdgeKind::Exports) && from.kind != NodeKind::File).then(|| {
        format!(
            "{} starts at a {:?} node - DEFINES/EXPORTS always run from the file's File node",
            describe_edge(edge),
            from.kind
        )
    })
}

fn bulk_defines_from_file(lines: &[BulkLine], findings: &mut Vec<String>) {
    let index = bulk_node_index(lines);
    for (line_no, edge) in bulk_edges(lines) {
        if let Some((_, from)) = index.get(edge.from_id.as_str()) {
            if let Some(message) = defines_violation(edge, from) {
                findings.push(format!("bulk run 1, NDJSON line {line_no}: {message}"));
            }
        }
    }
}

fn language_finding(manifest: &PluginManifest, context: &str, node: &WireNode, findings: &mut Vec<String>) {
    if node.language != manifest.language {
        findings.push(format!(
            "{context}: node {:?} ({:?}) declares language {:?}, but the manifest's language is {:?}",
            node.id, node.file_path, node.language, manifest.language
        ));
    }
}

fn bulk_ids(lines: &[BulkLine]) -> (BTreeSet<&str>, BTreeSet<&str>) {
    let mut nodes = BTreeSet::new();
    let mut edges = BTreeSet::new();
    for line in lines {
        match &line.item {
            Ok(BulkItem::Node(node)) => {
                nodes.insert(node.id.as_str());
            }
            Ok(BulkItem::Edge(edge)) => {
                edges.insert(edge.id.as_str());
            }
            Err(_) => {}
        }
    }
    (nodes, edges)
}

fn bulk_repeat(run: &RunData) -> CheckResult {
    const ID: &str = "id-stability.bulk-repeat";
    let [first, second] = run.bulk;
    if !first.complete() || !second.complete() {
        return result(
            ID,
            Outcome::Skip("not reached: a bulk run did not complete (see `session`)".to_string()),
        );
    }
    let (nodes1, edges1) = bulk_ids(&first.lines);
    let (nodes2, edges2) = bulk_ids(&second.lines);
    let mut findings = Vec::new();
    for (what, one, two) in [("node", &nodes1, &nodes2), ("edge", &edges1, &edges2)] {
        for id in one.difference(two) {
            findings.push(format!("{what} id {id:?} is emitted by bulk run 1 but not by bulk run 2"));
        }
        for id in two.difference(one) {
            findings.push(format!("{what} id {id:?} is emitted by bulk run 2 but not by bulk run 1"));
        }
    }
    result(ID, verdict(findings))
}

// --- control-plane rules ------------------------------------------------------

/// What walking every answered diff in order found, for the rules that need
/// to know which nodes existed *before* each diff.
#[derive(Default)]
struct DiffWalk {
    stream_order: Vec<String>,
    same_file: Vec<String>,
    defines: Vec<String>,
    deletes: Vec<String>,
    stays_in_file: Vec<String>,
}

fn walk_diffs(run: &RunData, answered: &[(String, &FileChangeDiff)]) -> DiffWalk {
    let mut walk = DiffWalk::default();
    let Some(session) = run.session else { return walk };

    // Nodes as the index would know them: seeded from the committed walk.
    let mut known: HashMap<String, NodeInfo> = HashMap::new();
    // Every node id the plugin has ever emitted - for `deleteNodeIds`.
    let mut emitted: HashSet<String> = HashSet::new();
    for (index, run) in run.bulk.iter().enumerate() {
        for line in &run.lines {
            if let Ok(BulkItem::Node(node)) = &line.item {
                emitted.insert(node.id.clone());
                if index == 0 {
                    known.entry(node.id.clone()).or_insert_with(|| NodeInfo::of(node));
                }
            }
        }
    }

    let answered_exchanges =
        session.exchanges.iter().filter(|e| matches!(&e.response, Some(r) if r.diff.is_ok()));
    for (exchange, (step, diff)) in answered_exchanges.zip(answered) {
        walk_one_diff(&mut walk, exchange, step, diff, &known, &emitted);
        for node in &diff.upsert_nodes {
            emitted.insert(node.id.clone());
        }
        for id in &diff.delete_node_ids {
            known.remove(id);
        }
        for node in &diff.upsert_nodes {
            known.insert(node.id.clone(), NodeInfo::of(node));
        }
    }
    walk
}

fn walk_one_diff(
    walk: &mut DiffWalk,
    exchange: &Exchange,
    step: &str,
    diff: &FileChangeDiff,
    known: &HashMap<String, NodeInfo>,
    emitted: &HashSet<String>,
) {
    let own: HashMap<&str, NodeInfo> =
        diff.upsert_nodes.iter().map(|n| (n.id.as_str(), NodeInfo::of(n))).collect();
    let lookup = |id: &str| own.get(id).cloned().or_else(|| known.get(id).cloned());

    for id in &diff.delete_node_ids {
        if !emitted.contains(id) {
            walk.deletes.push(format!(
                "{step}: deleteNodeIds names {id:?}, which no bulk run and no earlier diff ever emitted"
            ));
        }
    }

    for edge in &diff.upsert_edges {
        if let Some(from) = lookup(&edge.from_id) {
            if let Some(message) = defines_violation(edge, &from) {
                walk.defines.push(format!("{step}: {message}"));
            }
        }
    }

    if exchange.method != Method::FileChanged {
        return;
    }
    let file = exchange.file_paths.first().map(String::as_str).unwrap_or_default();

    for node in &diff.upsert_nodes {
        if node.file_path != file {
            walk.stays_in_file.push(format!(
                "{step}: upserts node {:?} of {:?} in a diff for {file:?}",
                node.id, node.file_path
            ));
        }
    }
    for id in &diff.delete_node_ids {
        if let Some(info) = known.get(id).filter(|info| info.file != file) {
            walk.stays_in_file
                .push(format!("{step}: deletes node {id:?} of {:?} in a diff for {file:?}", info.file));
        }
    }

    for edge in &diff.upsert_edges {
        let from = lookup(&edge.from_id);
        let to = lookup(&edge.to_id);
        for (end, id, info) in [("fromId", &edge.from_id, &from), ("toId", &edge.to_id, &to)] {
            match info {
                None => walk.stream_order.push(format!(
                    "{step}: {} - {end} {id:?} names no node in this diff or anything emitted before it",
                    describe_edge(edge)
                )),
                Some(info) if info.file != file => walk.stream_order.push(format!(
                    "{step}: {} - {end} is a node of {:?}, not of {file:?}; edges never leave their file",
                    describe_edge(edge),
                    info.file
                )),
                Some(_) => {}
            }
        }
        if let (Some(from), Some(to)) = (&from, &to) {
            if let Some(message) = same_file_violation(edge, from, to) {
                walk.same_file.push(format!("{step}: {message}"));
            }
        }
    }
}

fn not_reached(what: &str) -> Outcome {
    Outcome::Skip(format!("not reached: the session failed before {what} (see `session`)"))
}

fn whitespace_edit(run: &RunData) -> CheckResult {
    const ID: &str = "id-stability.whitespace-edit";
    let exchange = run
        .session
        .and_then(|s| s.whitespace_exchange.map(|index| &s.exchanges[index]))
        .filter(|e| e.response.is_some());
    let (Some(exchange), Some(target)) = (exchange, run.target) else {
        return result(ID, not_reached("the whitespace-edit step was answered"));
    };
    let Some(Ok(diff)) = exchange.response.as_ref().map(|r| &r.diff) else {
        return result(
            ID,
            Outcome::Skip("the whitespace-edit response did not parse (see `shape`)".to_string()),
        );
    };

    let at = format!(
        "{} after a space was inserted at the end of line {} of {:?}",
        exchange.method.name(),
        target.line,
        target.file_path
    );
    let mut findings = Vec::new();
    for node in &diff.upsert_nodes {
        let r = &node.range;
        findings.push(format!(
            "{at}: upserts node {:?} ({:?} {:?}, range {}:{}-{}:{}) - a whitespace-only edit must leave it untouched",
            node.id, node.kind, node.name, r.start.line, r.start.col, r.end.line, r.end.col
        ));
    }
    for id in &diff.delete_node_ids {
        findings.push(format!("{at}: deletes node {id:?}"));
    }
    for edge in &diff.upsert_edges {
        findings.push(format!("{at}: upserts {}", describe_edge(edge)));
    }
    for id in &diff.delete_edge_ids {
        findings.push(format!("{at}: deletes edge {id:?}"));
    }
    result(ID, verdict(findings))
}

fn deletes_known(run: &RunData, walk: &DiffWalk) -> CheckResult {
    const ID: &str = "id-stability.deletes-known";
    let reached = run
        .session
        .and_then(|s| s.emptied_exchange.map(|index| &s.exchanges[index]))
        .is_some_and(|e| e.response.is_some());
    if !reached {
        return result(ID, not_reached("the emptied-file step was answered"));
    }
    result(ID, verdict(walk.deletes.clone()))
}

fn incremental_matches_bulk(run: &RunData) -> CheckResult {
    const ID: &str = "id-stability.incremental-matches-bulk";
    let restored = run.session.and_then(|s| s.restored_file_ids.as_ref());
    let (Some(restored), Some(bulk), Some(target)) = (restored, run.bulk_file_ids, run.target) else {
        return result(ID, not_reached("the restored file was read back from the index"));
    };
    let mut findings = Vec::new();
    for id in restored.difference(bulk) {
        findings.push(format!(
            "node {id:?} of {:?} is in the index after fileChanged restored the file, but was not after the bulk walk \
             - the incremental path derived an id the bulk path does not, and the bulk row was never deleted",
            target.file_path
        ));
    }
    for id in bulk.difference(restored) {
        findings.push(format!(
            "node {id:?} of {:?} was in the index after the bulk walk, but is gone after fileChanged restored the file",
            target.file_path
        ));
    }
    result(ID, verdict(findings))
}

fn diff_stays_in_file(run: &RunData, walk: &DiffWalk) -> CheckResult {
    const ID: &str = "ownership.diff-stays-in-file";
    let answered = run
        .session
        .is_some_and(|s| s.exchanges.iter().any(|e| e.method == Method::FileChanged && e.response.is_some()));
    if !answered {
        return result(ID, not_reached("any fileChanged was answered"));
    }
    result(ID, verdict(walk.stays_in_file.clone()))
}

// --- capabilities -------------------------------------------------------------

fn capabilities(run: &RunData) -> [CheckResult; 2] {
    const UNDECLARED: &str = "capabilities.semantic-pass-undeclared";
    const LAZY: &str = "capabilities.semantic-engine-lazy";
    let declared = run.manifest.capabilities.semantic_pass;

    let Some(session) = run.session else {
        return [
            result(UNDECLARED, not_reached("the control-plane session started")),
            result(LAZY, not_reached("the control-plane session started")),
        ];
    };

    if !declared {
        let mut findings = Vec::new();
        for exchange in session.exchanges.iter().filter(|e| e.method == Method::SemanticPass) {
            findings.push(format!(
                "{}: a semanticPass request was sent although the manifest declares semantic_pass = false",
                exchange.step
            ));
        }
        if run.marker_exists_at_end {
            findings.push(
                "the plugin wrote the semantic-engine marker although its manifest declares semantic_pass = false \
                 - it started an engine core will never send a semanticPass to"
                    .to_string(),
            );
        }
        return [
            result(UNDECLARED, verdict(findings)),
            result(
                LAZY,
                Outcome::Skip("not applicable: the manifest declares semantic_pass = false".to_string()),
            ),
        ];
    }

    let not_applicable =
        Outcome::Skip("not applicable: the manifest declares semantic_pass = true".to_string());
    let lazy = match session.marker_at_first_semantic_pass {
        Some(true) => Outcome::Fail(vec![
            "the semantic-engine marker already existed when the first semanticPass request was written - \
             the engine was started for structural work (bulk index or fileChanged) alone"
                .to_string(),
        ]),
        Some(false) if run.marker_exists_at_end => Outcome::Pass,
        None if run.marker_exists_at_end => Outcome::Fail(vec![
            "the semantic-engine marker was written although no semanticPass request was ever sent".to_string(),
        ]),
        None => not_reached("a semanticPass request was sent"),
        Some(false) => Outcome::Skip(format!(
            "not instrumented: the plugin never wrote the {:?} marker - see the README's plugin authoring section",
            crate::cli::plugin_check::session::SEMANTIC_ENGINE_MARKER
        )),
    };
    [result(UNDECLARED, not_applicable), result(LAZY, lazy)]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cli::plugin_check::session::{parse_bulk_lines, Response};
    use crate::daemon::manifest::{Capabilities, WorkspaceConfig};

    fn manifest(semantic_pass: bool) -> PluginManifest {
        PluginManifest {
            language: "fake".to_string(),
            protocol_version: crate::protocol::types::CURRENT_PROTOCOL_VERSION,
            plugin_version: "0".to_string(),
            command: PathBuf::from("true"),
            args: Vec::new(),
            extensions: vec![".fk".to_string()],
            fingerprint_ignore: Vec::new(),
            manifest_dir: PathBuf::from("/plugins/fake"),
            capabilities: Capabilities { semantic_pass, ..Capabilities::default() },
            workspace: WorkspaceConfig::default(),
        }
    }

    fn node(id: &str, kind: &str, file: &str, native_kind: Option<&str>) -> String {
        let native_kind = native_kind.map(|k| format!(",\"nativeKind\":\"{k}\"")).unwrap_or_default();
        let target = if native_kind.is_empty() {
            String::new()
        } else {
            ",\"target\":{\"scope\":{\"file\":\"b.fk\"},\"key\":{\"name\":\"x\"}}".to_string()
        };
        format!(
            "{{\"id\":\"{id}\",\"kind\":\"{kind}\",\"name\":\"{id}\",\"qualifiedName\":\"{id}\",\"filePath\":\"{file}\",\
             \"range\":{{\"start\":{{\"line\":0,\"col\":0}},\"end\":{{\"line\":0,\"col\":1}}}},\"visibility\":\"public\",\
             \"language\":\"fake\"{native_kind}{target}}}"
        )
    }

    fn edge(id: &str, from: &str, to: &str, kind: &str, resolved: bool) -> String {
        format!(
            "{{\"id\":\"{id}\",\"fromId\":\"{from}\",\"toId\":\"{to}\",\"kind\":\"{kind}\",\"source\":\"syntactic\",\
             \"engine\":\"fake\",\"resolved\":{resolved}}}"
        )
    }

    fn bulk(lines: &[String]) -> BulkRun {
        let bytes = lines.join("\n").into_bytes();
        BulkRun { lines: parse_bulk_lines(&bytes), bytes, failure: None }
    }

    fn conformant_lines() -> Vec<String> {
        vec![
            node("a", "File", "a.fk", None),
            node("f", "Function", "a.fk", None),
            node("p", "Module", "a.fk", Some("pending_symbol")),
            edge("e1", "a", "f", "DEFINES", true),
            edge("e2", "f", "p", "CALLS", false),
            node("b", "File", "b.fk", None),
        ]
    }

    fn outcome_of(results: &[CheckResult], id: &str) -> Outcome {
        results.iter().find(|r| r.id == id).unwrap_or_else(|| panic!("no check {id}")).outcome.clone()
    }

    fn evaluate_bulk(lines: Vec<String>) -> Vec<CheckResult> {
        let manifest = manifest(false);
        let run1 = bulk(&lines);
        let run2 = bulk(&lines);
        evaluate(&RunData {
            manifest: &manifest,
            bulk: [&run1, &run2],
            target: None,
            bulk_file_ids: None,
            session: None,
            failures: Vec::new(),
            marker_exists_at_end: false,
        })
    }

    #[test]
    fn a_conformant_stream_passes_every_bulk_check() {
        let results = evaluate_bulk(conformant_lines());
        for id in [
            "session",
            "shape",
            "stream-order",
            "same-file-rule",
            "id-stability.bulk-repeat",
            "ownership.defines-exports-from-file",
            "ownership.language",
            "ownership.no-container",
        ] {
            assert_eq!(outcome_of(&results, id), Outcome::Pass, "{id}");
        }
    }

    #[test]
    fn an_edge_before_its_target_node_breaks_stream_order_only() {
        let mut lines = conformant_lines();
        let target = lines.remove(1);
        lines.insert(4, target);
        let results = evaluate_bulk(lines);
        let Outcome::Fail(findings) = outcome_of(&results, "stream-order") else {
            panic!("stream-order must fail")
        };
        assert!(findings[0].contains("only emitted later"), "{findings:?}");
        assert_eq!(outcome_of(&results, "same-file-rule"), Outcome::Pass);
    }

    #[test]
    fn an_edge_onto_another_files_node_breaks_stream_order() {
        let mut lines = conformant_lines();
        lines.push(edge("e3", "f", "b", "REFERENCES", false));
        let Outcome::Fail(findings) = outcome_of(&evaluate_bulk(lines), "stream-order") else { panic!() };
        assert!(findings[0].contains("edges never leave their file"), "{findings:?}");
    }

    #[test]
    fn resolution_flags_are_judged_by_what_the_edge_lands_on() {
        let mut lines = conformant_lines();
        lines[3] = edge("e1", "a", "f", "DEFINES", false);
        lines[4] = edge("e2", "f", "p", "CALLS", true);
        let Outcome::Fail(findings) = outcome_of(&evaluate_bulk(lines), "same-file-rule") else { panic!() };
        assert_eq!(findings.len(), 2, "{findings:?}");
    }

    /// The wire half of `capabilities.semantic-pass-undeclared` - the half no
    /// fake plugin can provoke, since only core decides what is sent. A
    /// transcript holding a `semanticPass` exchange for a plugin that did not
    /// declare the capability must fail the check.
    #[test]
    fn a_semantic_pass_sent_to_an_undeclared_plugin_fails_the_capability_check() {
        let manifest = manifest(false);
        let run = bulk(&conformant_lines());
        let session = Session {
            exchanges: vec![Exchange {
                step: "semanticPass #1".to_string(),
                method: Method::SemanticPass,
                file_paths: Vec::new(),
                response: Some(Response {
                    raw: serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}),
                    diff: Ok(FileChangeDiff::default()),
                }),
            }],
            ..Session::default()
        };
        let data = RunData {
            manifest: &manifest,
            bulk: [&run, &run],
            target: None,
            bulk_file_ids: None,
            session: Some(&session),
            failures: Vec::new(),
            marker_exists_at_end: false,
        };
        let results = evaluate(&data);
        assert!(matches!(outcome_of(&results, "capabilities.semantic-pass-undeclared"), Outcome::Fail(_)));
    }

    #[test]
    fn a_missing_marker_is_not_instrumented_rather_than_passed() {
        let manifest = manifest(true);
        let run = bulk(&conformant_lines());
        let session = Session { marker_at_first_semantic_pass: Some(false), ..Session::default() };
        let data = |marker_exists_at_end| RunData {
            manifest: &manifest,
            bulk: [&run, &run],
            target: None,
            bulk_file_ids: None,
            session: Some(&session),
            failures: Vec::new(),
            marker_exists_at_end,
        };
        let lazy = |data: RunData| outcome_of(&evaluate(&data), "capabilities.semantic-engine-lazy");
        assert!(matches!(lazy(data(false)), Outcome::Skip(reason) if reason.starts_with("not instrumented")));
        assert_eq!(lazy(data(true)), Outcome::Pass);
    }

    #[test]
    fn legacy_v1_fields_warn_without_failing_shape() {
        let lines = vec![
            "{\"id\":\"a\",\"kind\":\"File\",\"name\":\"a\",\"qualifiedName\":\"a.fk\",\"filePath\":\"a.fk\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"exported\":false,\"language\":\"fake\"}".to_string(),
            "{\"id\":\"p\",\"kind\":\"Module\",\"name\":\"x\",\"qualifiedName\":\"b.fk#x\",\"filePath\":\"a.fk\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"exported\":false,\"language\":\"fake\",\"nativeKind\":\"pending_symbol\"}".to_string(),
            "{\"id\":\"e\",\"fromId\":\"a\",\"toId\":\"p\",\"kind\":\"IMPORTS\",\"source\":\"tree-sitter\",\"resolved\":false}".to_string(),
        ];
        let results = evaluate_bulk(lines);
        let shape = results.iter().find(|r| r.id == "shape").unwrap();
        assert_eq!(shape.outcome, Outcome::Pass);
        assert_eq!(shape.warnings.len(), 1);
        assert!(
            shape.warnings[0].contains("on 2 node(s), `source` without `engine` on 1 edge(s)"),
            "{:?}",
            shape.warnings
        );
        assert!(shape.warnings[0].contains("on 1 node(s)"), "{:?}", shape.warnings);
    }
}
