//! What an answer becomes: the accumulated diff and overload settling.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use g_mesh_wire::{
    EdgeKind, FileChangeDiff, NodeKind, PlaceholderTarget, Position, Range, SourceTier, TargetKey,
    TargetScope, WireEdge, WireNode,
};

use super::super::config::OverloadDisambiguation;
use super::{
    hover::hover_matches,
    landing::{choose_overload, Choice},
    questions::{declaration_name_position, Ask, Hovered, Question},
};
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_sdk::SdkIndex;
use g_mesh_plugin_sdk::{EdgeSpec, FileGraphBuilder, OpenSite, OpenSiteKind, PlaceholderKind};

/// Accumulates one pass's answer.
pub(super) struct Answers {
    pub(super) language: String,
    pub(super) engine: String,
    pub(super) builders: BTreeMap<RelPath, FileGraphBuilder>,
    /// Placeholders this pass will emit, by the address they wait on - so that
    /// two sites resolving to one declaration produce one node rather than one
    /// id written twice. The same rule `plugins/rust`'s emitter applies to its
    /// own output, and for the same reason: `apply_diff` upserts by id either
    /// way, but a stream that says one thing twice has stopped describing the
    /// file.
    ///
    /// Collected here rather than added to a builder on the spot, because the
    /// row a shared placeholder carries must not depend on which of its sites
    /// was answered first - see [`Placeholder`].
    pub(super) placeholders: BTreeMap<Address, Placeholder>,
    /// Every edge id this pass recorded, kept or not - what makes a second
    /// site with the same edge a no-op.
    pub(super) edges: HashSet<String>,
    /// The recorded edges, in the order they were recorded. Held rather than
    /// added to a builder on the spot, so that [`Answers::settle`] can drop
    /// the ones a re-sent structural edge already covers (R3) before
    /// they reach the diff or `by_file`.
    pub(super) recorded: Vec<Recorded>,
    pub(super) by_file: BTreeMap<RelPath, Vec<String>>,
    pub(super) retract: BTreeSet<String>,
    /// Structural edges (`OpenSite::replaces`) an answer agreed with, and the
    /// `(from, kind, declaration)` each agreeing answer landed on (R2).
    /// An agreeing answer records nothing; this is what R3 uses to know
    /// which declaration a structural edge onto a placeholder stands for.
    pub(super) confirmed: BTreeMap<String, Vec<(String, EdgeKind, String)>>,
    /// Structural edges at least one answer contradicted - it landed on
    /// another declaration.
    pub(super) contradicted: BTreeSet<String>,
    /// Structural edges at least one answer did *not* contradict: it agreed,
    /// or it was empty, ambiguous or outside the index. One such answer is
    /// enough to keep the edge (R1).
    pub(super) upheld: BTreeSet<String>,
    /// The structural edges this pass re-sends unchanged (R1), set by
    /// [`Answers::settle`].
    pub(super) resend: Vec<WireEdge>,
    /// How many untyped receiver-call sites (`ReceiverCall`, no `replaces`)
    /// this pass got an answer for, by `(from_id, name)` - see
    /// [`pass::untyped_call_answered`] and [`pass::trim_untyped_calls`] (GM-486).
    pub(super) untyped_answered: HashMap<(String, String), usize>,
    /// How this server's overload answers are narrowed - the manifest's
    /// `overload_disambiguation`.
    pub(super) disambiguation: OverloadDisambiguation,
    /// Bound [`OpenSiteKind::OverloadCall`] sites, by the structural edge
    /// they refine: the site's position, and the bound edge it recorded. A
    /// site of that edge that is missing here was left unbound, or never
    /// answered - and either keeps the structural edge (all or nothing, see
    /// [`Answers::settle`]).
    pub(super) overload_bound: BTreeMap<String, BTreeMap<(u32, u32), String>>,
    /// Overload answers waiting on their `hover` hop, by
    /// [`Ask::OverloadHover::pending`]. `None` once concluded.
    pub(super) pending: Vec<Option<PendingOverload>>,
    /// Each candidate declaration's normalised hover, by `(node, ordinal)`,
    /// for the pass: a set called from fifty places is hovered once. An empty
    /// string is a hover that said nothing.
    pub(super) hovers: HashMap<(String, u32), String>,
}

/// An overload answer whose candidates only `hover` can tell apart - the
/// join of its [`Ask::OverloadHover`] questions.
pub(super) struct PendingOverload {
    pub(super) landing: Landing,
    pub(super) candidates: Vec<u32>,
    /// The call's normalised hover, once answered.
    pub(super) call: Option<String>,
    /// Hover questions still unanswered.
    pub(super) waiting: usize,
}

/// Where an overload answer landed, and the site it answers: everything
/// [`Answers::conclude`] needs to record the outcome.
pub(super) struct Landing {
    /// The call's file.
    pub(super) file: RelPath,
    pub(super) site: OpenSite,
    pub(super) node_id: String,
    pub(super) node_name: String,
    pub(super) address: PlaceholderTarget,
    /// What the site covers - see [`site_range`].
    pub(super) range: Range,
}

/// One semantic edge this pass recorded, waiting for [`Answers::finish`].
pub(super) struct Recorded {
    pub(super) id: String,
    /// The file the edge starts in, and so the builder it goes into.
    pub(super) in_file: RelPath,
    pub(super) from_id: String,
    pub(super) kind: EdgeKind,
    /// The placeholder the edge lands on.
    pub(super) to_id: String,
    /// The declaration the placeholder waits on - what R3 compares a re-sent
    /// structural edge's target with.
    pub(super) declaration: String,
    /// The ordinal of the declaration the call binds, for a bound overload
    /// call - written to `toDeclaration`, and part of the edge's id.
    pub(super) to_declaration: Option<u32>,
}

/// The placeholder id and the edge id an answer from `from_id` onto `target`
/// gets in `in_file`. One formula for [`Answers::record`] and for the
/// agreement test in [`record_answer`], so the two cannot drift apart.
///
/// `to_declaration` is part of the edge id and not of the placeholder's: two
/// calls of two overloads from one caller are two edges onto one placeholder.
pub(super) fn answer_ids(
    in_file: &RelPath,
    from_id: &str,
    kind: EdgeKind,
    target: &PlaceholderTarget,
    to_declaration: Option<u32>,
) -> (String, String) {
    let placeholder = g_mesh_plugin_sdk::placeholder_id(in_file, PlaceholderKind::PendingSymbol, target);
    let edge = g_mesh_plugin_sdk::ids::edge_id(from_id, kind, &placeholder, to_declaration);
    (placeholder, edge)
}

/// The node one address gets, and the use site whose `name` and `range` it
/// shows.
///
/// # Why the site is chosen rather than taken
///
/// A placeholder's identity is the address it waits on: its id is derived from
/// the file and the rendered target, and nothing else (`ids::node_id`). Its
/// `name` and `range`, though, describe a *use site*, and an address reached
/// from several sites has several of those - `SearchMode::Count` and
/// `SearchMode::CountMatches` in ripgrep's `crates/core/flags/hiargs.rs` both
/// address the enum `SearchMode`, so one node has to speak for both.
///
/// Recording whichever arrived first made that choice out of LSP answer
/// arrival order, which is not a property of the file. Three indexings of one
/// unchanged ripgrep checkout by one binary agreed on every node id and every
/// edge, and disagreed on 50, 54 and 63 placeholder *rows* between the pairs -
/// every one of them in `range`, six of them in `name` as well. Nothing
/// downstream was wrong, since the linker reads the target and never the row,
/// but the index had stopped being a function of the source: any node-level
/// comparison over a Rust corpus was reading that noise as a difference.
///
/// So the row is made a function of the address instead. `name` is the
/// resolved *declaration's* own name rather than the text at a use site -
/// `SearchMode`, which every site addressing it is a use of, and which
/// `qualifiedName` already says. `OpenSite::name` was never meant for this: it
/// is documented as "an LSP bridge uses it only for diagnostics", and
/// [`record_implementor`] already passes the declaration's name. The `range`
/// is the *earliest* of the sites, which is the convention the structural tier
/// arrives at by walking a file in source order.
pub(super) struct Placeholder {
    pub(super) id: String,
    pub(super) target: PlaceholderTarget,
    pub(super) name: String,
    pub(super) range: Range,
}

/// A witness's rank: earliest position wins, and the name settles the tie that
/// two sites at one position would otherwise leave to arrival order again.
pub(super) fn witness_rank<'a>(name: &'a str, range: &Range) -> (u32, u32, u32, u32, &'a str) {
    (range.start.line, range.start.col, range.end.line, range.end.col, name)
}

/// What a use site covers: the text written at it. `written` is the source
/// text, which is not always the name the placeholder ends up carrying - see
/// [`Placeholder`].
///
/// The site's position is where the server is asked, which is not always
/// where `written` starts: a path call is asked at its last segment (`new` in
/// `super::Q::<u8>::new()`, GM-618), because asked at the path's start a
/// server answers the qualifier. The range still covers the whole path, as
/// the structural tier's range for the same call does, so it starts at the
/// occurrence of `written` in `source`'s line that covers the position,
/// nearest first. With no source, or no such occurrence on that line (a path
/// written across lines), it starts at the position.
pub(super) fn site_range(source: Option<&str>, at: Position, written: &str) -> Range {
    let width = written.chars().count() as u32;
    let start = source
        .and_then(|source| source.split('\n').nth(at.line as usize))
        .and_then(|line| written_start(line, at.col, written, width))
        .unwrap_or(at.col);
    Range {
        start: Position { line: at.line, col: start },
        end: Position { line: at.line, col: start + width },
    }
}

/// The column, in characters, where an occurrence of `written` (`width`
/// characters) in `line` starts and covers `col`, the nearest one to `col`.
pub(super) fn written_start(line: &str, col: u32, written: &str, width: u32) -> Option<u32> {
    let lowest = (col + 1).saturating_sub(width);
    (lowest..=col)
        .rev()
        .find(|&start| line.chars().skip(start as usize).take(width as usize).eq(written.chars()))
}

/// Everything a placeholder's identity is derived from, in a form that can key
/// a map: its file, and the target it waits on (scope and key, each with the
/// discriminant that keeps a file scope from colliding with a container of the
/// same spelling).
pub(super) type Address = (RelPath, bool, String, bool, String);

pub(super) fn address_key(file: &RelPath, target: &PlaceholderTarget) -> Address {
    let (container, scope) = match &target.scope {
        TargetScope::File(path) => (false, path.clone()),
        TargetScope::Container(container) => (true, container.clone()),
    };
    let (qualified, key) = match &target.key {
        TargetKey::Name(name) => (false, name.clone()),
        TargetKey::QualifiedName(qualified) => (true, qualified.clone()),
    };
    (file.clone(), container, scope, qualified, key)
}

impl Answers {
    pub(super) fn new(language: &str, engine: &str) -> Self {
        Self {
            language: language.to_string(),
            engine: engine.to_string(),
            builders: BTreeMap::new(),
            placeholders: BTreeMap::new(),
            edges: HashSet::new(),
            recorded: Vec::new(),
            by_file: BTreeMap::new(),
            retract: BTreeSet::new(),
            confirmed: BTreeMap::new(),
            contradicted: BTreeSet::new(),
            upheld: BTreeSet::new(),
            resend: Vec::new(),
            untyped_answered: HashMap::new(),
            disambiguation: OverloadDisambiguation::default(),
            overload_bound: BTreeMap::new(),
            pending: Vec::new(),
            hovers: HashMap::new(),
        }
    }

    pub(super) fn builder(&mut self, file: &RelPath) -> &mut FileGraphBuilder {
        let (language, engine) = (self.language.clone(), self.engine.clone());
        self.builders.entry(file.clone()).or_insert_with(|| FileGraphBuilder::new(&language, &engine, file))
    }

    /// One answer: the placeholder that addresses `target`, and the edge onto
    /// it.
    ///
    /// `in_file` is the file the edge *starts* in, which is where the
    /// placeholder goes too - a placeholder is a node of the file that is
    /// waiting, and `fromFile` is what core's visibility check reads. `for_file`
    /// is the file whose question produced this, which is what the answer is
    /// remembered against for retraction; the two differ only for an
    /// implementation answer, whose edge starts at a declaration in some other
    /// file entirely.
    ///
    /// `name` and `at` describe the use site this answer came from; which of
    /// an address's sites the node ends up showing is [`Placeholder`]'s rule,
    /// not this call's. The node itself is added in [`Answers::finish`], once
    /// every site has had its say - so the id is derived here rather than
    /// returned by the builder. The edge waits there too, so that
    /// [`Answers::settle`] can still drop it (R3).
    ///
    /// `declaration` is the id of the node the answer landed on, which the
    /// placeholder addresses; `to_declaration` the ordinal of the one of its
    /// declarations a bound overload call binds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record(
        &mut self,
        for_file: &RelPath,
        in_file: &RelPath,
        from_id: &str,
        kind: EdgeKind,
        name: &str,
        at: Range,
        target: PlaceholderTarget,
        declaration: &str,
        to_declaration: Option<u32>,
    ) -> String {
        let key = address_key(in_file, &target);
        let placeholder = match self.placeholders.get_mut(&key) {
            Some(held) => {
                if witness_rank(name, &at) < witness_rank(&held.name, &held.range) {
                    held.name = name.to_string();
                    held.range = at;
                }
                held.id.clone()
            }
            None => {
                let (id, _) = answer_ids(in_file, from_id, kind, &target, to_declaration);
                self.placeholders
                    .insert(key, Placeholder { id: id.clone(), target, name: name.to_string(), range: at });
                id
            }
        };

        let id = g_mesh_plugin_sdk::ids::edge_id(from_id, kind, &placeholder, to_declaration);
        if !self.edges.insert(id.clone()) {
            return id;
        }
        self.recorded.push(Recorded {
            id: id.clone(),
            in_file: in_file.clone(),
            from_id: from_id.to_string(),
            kind,
            to_id: placeholder,
            declaration: declaration.to_string(),
            to_declaration,
        });
        self.by_file.entry(for_file.clone()).or_default().push(id.clone());
        id
    }

    /// Settles what this pass says about the structural edges its sites named
    /// in `replaces`, for the files it finished - see [`LspBridge`]'s doc on
    /// retraction. Called once, after the last answer and before
    /// `by_file` is read for retraction.
    ///
    /// - **R1, restore unless contradicted.** Every structural edge a site in
    ///   a finished file names is re-sent unchanged, as the index holds it,
    ///   unless every answer about it contradicted it. It never enters
    ///   `by_file`, so neither `retract_stale` nor core's semantic sweep can
    ///   reach it.
    /// - **R3, coverage.** A recorded semantic edge whose id is a re-sent
    ///   edge's, or that goes from the same node, of the same kind, onto the
    ///   declaration a re-sent edge already lands on, is dropped: from the
    ///   diff and from `by_file`, so an earlier pass's copy of it is
    ///   retracted.
    ///
    /// - **Overload binding, all or nothing per edge.** A structural edge
    ///   that [`OpenSiteKind::OverloadCall`] sites refine is retracted only
    ///   when *every* such site of it in a finished file bound an ordinal;
    ///   its bound edges are kept. Otherwise it is re-sent by R1 and every
    ///   bound edge refining it is dropped, as is every bound edge whose file
    ///   was not finished. Exempt from R3: an unbound edge never covers a
    ///   bound one.
    ///
    /// A file the pass did not finish is not touched: its sites' answers may
    /// be missing, and "no answer" is not "the structural edge stands" until
    /// the question was actually put.
    pub(super) fn settle(&mut self, index: &SdkIndex, finished: &BTreeSet<RelPath>) {
        let bound = self.settle_overloads(index, finished);
        let mut seen: HashSet<String> = HashSet::new();
        for file in finished {
            let Some(graph) = index.graph(file) else { continue };
            for site in &graph.open_sites {
                let Some(replaced) = &site.replaces else { continue };
                if bound.contains(replaced) {
                    continue;
                }
                if self.contradicted.contains(replaced) && !self.upheld.contains(replaced) {
                    continue;
                }
                if !seen.insert(replaced.clone()) {
                    continue;
                }
                if let Some(edge) = graph.edges.iter().find(|edge| &edge.id == replaced) {
                    self.resend.push(edge.clone());
                }
            }
        }
        if self.resend.is_empty() {
            return;
        }

        // What the re-sent edges cover: their own ids, and the declarations
        // they land on - their own target, or whatever an agreeing answer
        // said a placeholder target stands for.
        let ids: HashSet<&str> = self.resend.iter().map(|edge| edge.id.as_str()).collect();
        let mut lands: HashMap<(&str, &str), Vec<EdgeKind>> = HashMap::new();
        for edge in &self.resend {
            lands.entry((edge.from_id.as_str(), edge.to_id.as_str())).or_default().push(edge.kind);
            for (from, kind, declaration) in self.confirmed.get(&edge.id).into_iter().flatten() {
                lands.entry((from.as_str(), declaration.as_str())).or_default().push(*kind);
            }
        }
        // A bound overload edge says more than any structural edge onto the
        // same function: it is never covered.
        let covered = |edge: &Recorded| {
            edge.to_declaration.is_none()
                && (ids.contains(edge.id.as_str())
                    || lands
                        .get(&(edge.from_id.as_str(), edge.declaration.as_str()))
                        .is_some_and(|kinds| kinds.contains(&edge.kind)))
        };
        let dropped: HashSet<String> =
            self.recorded.iter().filter(|edge| covered(edge)).map(|edge| edge.id.clone()).collect();
        if dropped.is_empty() {
            return;
        }
        self.recorded.retain(|edge| !dropped.contains(&edge.id));
        for ids in self.by_file.values_mut() {
            ids.retain(|id| !dropped.contains(id));
        }
    }

    /// The overload half of [`Answers::settle`]: retracts every structural
    /// edge all of whose [`OpenSiteKind::OverloadCall`] sites in `finished`
    /// files bound an ordinal, drops every other bound edge, and returns the
    /// retracted edges - which R1 must then not re-send.
    pub(super) fn settle_overloads(
        &mut self,
        index: &SdkIndex,
        finished: &BTreeSet<RelPath>,
    ) -> BTreeSet<String> {
        let mut sites: BTreeMap<&str, Vec<(u32, u32)>> = BTreeMap::new();
        for file in finished {
            let Some(graph) = index.graph(file) else { continue };
            for site in graph.open_sites.iter().filter(|site| site.kind == OpenSiteKind::OverloadCall) {
                let Some(replaced) = &site.replaces else { continue };
                sites.entry(replaced.as_str()).or_default().push((site.position.line, site.position.col));
            }
        }
        let bound: BTreeSet<String> = sites
            .into_iter()
            .filter(|(replaced, positions)| {
                self.overload_bound
                    .get(*replaced)
                    .is_some_and(|by_site| positions.iter().all(|position| by_site.contains_key(position)))
            })
            .map(|(replaced, _)| replaced.to_string())
            .collect();
        // One edge id can serve two structural edges (two calls of one
        // overload from one caller): kept if either is wholly bound.
        let keep: HashSet<&String> = bound
            .iter()
            .filter_map(|replaced| self.overload_bound.get(replaced))
            .flat_map(|by| by.values())
            .collect();
        let dropped: HashSet<String> = self
            .overload_bound
            .iter()
            .filter(|(replaced, _)| !bound.contains(*replaced))
            .flat_map(|(_, by_site)| by_site.values())
            .filter(|id| !keep.contains(id))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            self.recorded.retain(|edge| !dropped.contains(&edge.id));
            for ids in self.by_file.values_mut() {
                ids.retain(|id| !dropped.contains(id));
            }
        }
        self.retract.extend(bound.iter().cloned());
        bound
    }

    /// Records what one overload answer concluded: a bound edge for a bound
    /// [`OpenSiteKind::OverloadCall`] (booked for [`Answers::settle`]),
    /// nothing for an unbound one (its structural edge stands), and for a
    /// receiver call the edge it would have had anyway, with the ordinal when
    /// there is one.
    pub(super) fn conclude(&mut self, landing: Landing, ordinal: Option<u32>) {
        let site = &landing.site;
        if site.kind == OpenSiteKind::OverloadCall && ordinal.is_none() {
            return;
        }
        let id = self.record(
            &landing.file,
            &landing.file,
            &site.from_id,
            site.edge_kind,
            &landing.node_name,
            landing.range,
            landing.address.clone(),
            &landing.node_id,
            ordinal,
        );
        if site.kind == OpenSiteKind::OverloadCall {
            if let Some(replaced) = &site.replaces {
                self.overload_bound
                    .entry(replaced.clone())
                    .or_default()
                    .insert((site.position.line, site.position.col), id);
            }
        }
    }

    /// Steps 2-4 of [`LspBridge`]'s overload binding for an answer that
    /// landed on `node`'s declarations `ordinals`: binds, leaves unbound, or
    /// returns the `hover` questions that will decide.
    pub(super) fn bind_overload(
        &mut self,
        index: &SdkIndex,
        landing: Landing,
        node: &WireNode,
        node_file: &RelPath,
        ordinals: &BTreeSet<u32>,
    ) -> Vec<Question> {
        let candidates = match choose_overload(node, ordinals, self.disambiguation) {
            Choice::Bound(ordinal) => {
                self.conclude(landing, Some(ordinal));
                return Vec::new();
            }
            Choice::Unbound => {
                self.conclude(landing, None);
                return Vec::new();
            }
            Choice::Hover(candidates) => candidates,
        };
        let pending = self.pending.len();
        let mut again = vec![Question {
            file: landing.file.clone(),
            position: landing.site.position,
            ask: Ask::OverloadHover { pending, at: Hovered::Call, for_file: landing.file.clone() },
        }];
        let source = index.source(node_file).unwrap_or_default();
        for &ordinal in &candidates {
            if self.hovers.contains_key(&(node.id.clone(), ordinal)) {
                continue;
            }
            let declaration =
                node.declarations.iter().flatten().find(|declaration| declaration.ordinal == ordinal);
            let Some(position) = declaration
                .and_then(|declaration| declaration_name_position(source, &node.name, declaration))
            else {
                // Nowhere to hover: the candidate can never match. Settled now
                // so it is not asked again for the next call.
                self.hovers.insert((node.id.clone(), ordinal), String::new());
                continue;
            };
            again.push(Question {
                file: node_file.clone(),
                position,
                ask: Ask::OverloadHover {
                    pending,
                    at: Hovered::Candidate { node: node.id.clone(), ordinal },
                    for_file: landing.file.clone(),
                },
            });
        }
        self.pending.push(Some(PendingOverload { landing, candidates, call: None, waiting: again.len() }));
        again
    }

    /// One `hover` answer of a pending overload; concludes it when it was the
    /// last one outstanding.
    pub(super) fn hovered(&mut self, pending: usize, at: &Hovered, text: Option<String>) {
        let text = text.unwrap_or_default();
        let Some(Some(waiting)) = self.pending.get_mut(pending) else { return };
        match at {
            Hovered::Call => waiting.call = Some(text),
            Hovered::Candidate { node, ordinal } => {
                self.hovers.insert((node.clone(), *ordinal), text);
            }
        }
        waiting.waiting = waiting.waiting.saturating_sub(1);
        if waiting.waiting > 0 {
            return;
        }
        let Some(done) = self.pending[pending].take() else { return };
        let call = done.call.unwrap_or_default();
        let matching: Vec<u32> = done
            .candidates
            .iter()
            .copied()
            .filter(|ordinal| {
                self.hovers
                    .get(&(done.landing.node_id.clone(), *ordinal))
                    .is_some_and(|declared| hover_matches(declared, &call))
            })
            .collect();
        let ordinal = match matching.as_slice() {
            [one] => Some(*one),
            _ => None,
        };
        self.conclude(done.landing, ordinal);
    }

    pub(super) fn finish(mut self) -> (FileChangeDiff, BTreeMap<RelPath, Vec<String>>) {
        let mut diff = FileChangeDiff::default();
        // The edges that survived `settle`, in the order they were recorded,
        // and only the placeholders one of them still lands on.
        let recorded = std::mem::take(&mut self.recorded);
        let used: HashSet<&str> = recorded.iter().map(|edge| edge.to_id.as_str()).collect();
        let kept: HashSet<String> = recorded.iter().map(|edge| edge.id.clone()).collect();
        for edge in &recorded {
            let engine = self.engine.clone();
            self.builder(&edge.in_file).add_edge(EdgeSpec {
                from_id: edge.from_id.clone(),
                to_id: edge.to_id.clone(),
                kind: edge.kind,
                // Every edge here lands on a placeholder, so nothing is
                // confirmed until core links it: `resolved` describes what the
                // edge points at, never who produced it.
                resolved: false,
                to_declaration: edge.to_declaration,
                source: SourceTier::Semantic,
                engine,
                specifier: None,
            });
        }
        // The nodes, now that every site addressing each one has been seen. In
        // address order, which is the file's own order and not the order the
        // server happened to answer in - the same reason the row itself is
        // chosen rather than taken (see `Placeholder`).
        for ((file, ..), held) in std::mem::take(&mut self.placeholders) {
            if !used.contains(held.id.as_str()) {
                continue;
            }
            let id = self.builder(&file).add_placeholder(
                PlaceholderKind::PendingSymbol,
                held.name,
                held.target,
                held.range,
            );
            debug_assert_eq!(id, held.id, "a placeholder's id must not depend on its row");
        }
        for (_, builder) in std::mem::take(&mut self.builders) {
            let graph = builder.finish();
            diff.upsert_nodes.extend(graph.nodes);
            diff.upsert_edges.extend(graph.edges);
        }
        // The structural edges R1 restores, exactly as the index holds them:
        // `source` stays syntactic, so core's semantic sweep never reads them
        // as this tier's.
        let resent: HashSet<String> = self.resend.iter().map(|edge| edge.id.clone()).collect();
        diff.upsert_edges.extend(std::mem::take(&mut self.resend));
        // An edge this pass re-emitted is not stale, whatever an earlier pass
        // recorded about it: retracting and upserting one id in one diff is a
        // delete followed by an insert of the same row, which is at best a
        // waste and at worst a foreign-key fault for anything pointing at it.
        // The same holds for a structural edge one site contradicted and
        // another upheld: it is re-sent, so it is not retracted.
        diff.delete_edge_ids =
            self.retract.into_iter().filter(|id| !kept.contains(id) && !resent.contains(id)).collect();
        (diff, self.by_file)
    }
}

/// The address of a declaration, as a placeholder target.
///
/// A `qualifiedName` key rather than a `name` key, always: a semantic tier
/// knows *which* declaration it means, and `name` would re-introduce the
/// ambiguity it just resolved (`Close` exists on a dozen types;
/// `Server.Close` on one). The scope is the declaration's container when it
/// has one and its file otherwise - core's linker looks candidates up by
/// `(language, container)` or by `filePath`, and a language with no containers
/// only ever has the second.
pub(super) fn address_of(node: &WireNode, from_container: Option<String>) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: match &node.container {
            Some(container) => TargetScope::Container(container.clone()),
            None => TargetScope::File(node.file_path.clone()),
        },
        key: TargetKey::QualifiedName(node.qualified_name.clone()),
        from_container,
        key_path: node.qualified_path.clone(),
        also_scopes: Vec::new(),
    }
}

/// Whether a node is one an answer may point at.
///
/// A placeholder is not a declaration - it is another file's unanswered
/// question, and addressing one would chain a guess onto a guess. A `File`
/// node is not one either: a definition that lands only inside it (nothing
/// smaller contains the position) says "somewhere in this file", which is
/// weaker than the `IMPORTS` edge that already exists for it.
pub(super) fn is_addressable(node: &WireNode) -> bool {
    if node.kind == NodeKind::File {
        return false;
    }
    !matches!(
        node.native_kind.as_deref(),
        Some("pending_symbol") | Some("reexport") | Some("resolved_module") | Some("external_module")
    )
}
