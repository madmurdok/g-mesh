//! One pass: the request loop and recording each answer.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use g_mesh_wire::{EdgeKind, FileChangeDiff, NodeKind, TargetKey, WireNode};
use serde_json::{json, Value};

use super::super::client::{LspClient, Poll, CONTENT_MODIFIED};
use super::super::config::OverloadDisambiguation;
use super::super::position::{file_uri, line_text, PositionEncoding};
use super::{
    answers::{address_of, answer_ids, site_range, Answers, Landing},
    hover::hover_text,
    landing::{
        file_at, lands_on_name, locations, named_landing, node_at, overload_landing, refines, wire_position,
        ServerLocation,
    },
    questions::{questions, Ask, Question},
};
use super::{exited, Budgets, LspBridge, EXIT_GRACE};
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_sdk::{node_contains, SdkIndex};
use g_mesh_plugin_sdk::{OpenSite, OpenSiteKind};
use g_mesh_plugin_sdk::{SemanticAnswer, SemanticEngine};

/// Runs the question list against a live server.
///
/// Written as a free function rather than a method so that the client can be
/// borrowed mutably for the whole pass while the index, the budgets and the
/// accumulator are borrowed beside it - the alternative is a `&mut self`
/// method that re-borrows `self.client` on every line.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_pass(
    client: &mut LspClient,
    language: &str,
    engine: &str,
    roots: [&Path; 2],
    index: &SdkIndex,
    asking: Vec<Question>,
    budgets: &Budgets,
    deadline: Instant,
    disambiguation: OverloadDisambiguation,
) -> (Answers, BTreeSet<RelPath>, Option<String>) {
    let mut answers = Answers::new(language, engine);
    answers.disambiguation = disambiguation;
    let mut queue: Vec<Question> = asking.into_iter().rev().collect();
    // Questions whose empty answer arrived while the server was indexing. They
    // are *not* put straight back on the queue: re-asking a busy server
    // immediately gets the same empty answer, and doing that in a loop is a
    // spin rather than a retry. They wait until the server says it has
    // finished and are asked once more then - see `LspBridge`'s readiness
    // rules. `re_asked` is what keeps "once more" from being "forever".
    let mut deferred: Vec<Question> = Vec::new();
    let mut re_asked: HashSet<(RelPath, u32, u32, &'static str)> = HashSet::new();
    let mut in_flight: BTreeMap<i64, (Question, Instant)> = BTreeMap::new();
    let mut failed_files: BTreeSet<RelPath> = BTreeSet::new();
    let mut touched_files: BTreeSet<RelPath> = BTreeSet::new();
    // The first reason the pass fell short, if it did: `None` is a complete
    // pass.
    let mut failure: Option<String> = None;
    let mut fail = |reason: String| {
        failure.get_or_insert(reason);
    };
    // Never shorter than `request`: a warm-up is a longer budget or none.
    let warm_up = budgets.warm_up.map(|warm_up| warm_up.max(budgets.request));
    let mut warm_up_logged = false;
    // GM-435's `lsp_answered` line: measured only when it is written.
    let mut timing = g_mesh_plugin_sdk::log::timing_enabled().then(PassTiming::default);

    loop {
        // A deferred question goes back on the queue only once the server has
        // been quiet for a whole settle, not the instant its progress set
        // empties - the same rule, and for the same measured reason, as
        // readiness itself (see `LspBridge`'s doc). A re-ask sent into the gap
        // between two of rust-analyzer's startup phases is answered `null`
        // again, and that second empty answer is the one this bridge believes.
        if !deferred.is_empty() && client.quiet_for(budgets.settle) {
            queue.append(&mut deferred);
        }
        // Fill the pipeline - to one question while the server's warm-up is
        // owed (see `Budgets::warm_up`), to `concurrency` after.
        let warming = warm_up.is_some() && !client.warmed_up();
        let width = if warming { 1 } else { budgets.concurrency.max(1) };
        let request_budget = match warm_up {
            Some(warm_up) if warming => warm_up,
            _ => budgets.request,
        };
        if warming && !warm_up_logged && !queue.is_empty() {
            warm_up_logged = true;
            g_mesh_plugin_sdk::log_line!(
                "[{language}] the server has not answered yet - its first question may take up to {request_budget:?} \
                 (warm-up), the rest {:?}",
                budgets.request
            );
        }
        while in_flight.len() < width {
            let Some(question) = queue.pop() else { break };
            touched_files.insert(question.accounted_to().clone());
            let position = {
                let source = index.source(&question.file).unwrap_or_default();
                let line = line_text(source, question.position.line);
                client.encoding().from_wire_column(line, question.position.col)
            };
            let params = json!({
                "textDocument": { "uri": file_uri(&question.file.to_absolute(roots[0])) },
                "position": { "line": question.position.line, "character": position },
            });
            match client.request(question.method(), params) {
                Ok(id) => {
                    if let Some(timing) = timing.as_mut() {
                        timing.questions += 1;
                    }
                    in_flight.insert(id, (question, Instant::now()));
                }
                Err(err) => {
                    failed_files.insert(question.accounted_to().clone());
                    // A pipe broken by a server that has died is its death,
                    // not a failure to write.
                    match client.exit_status(EXIT_GRACE) {
                        Some(status) => {
                            g_mesh_plugin_sdk::log_line!(
                                "[{language}] the language server exited during the pass ({status})"
                            );
                            fail(exited(Some(status)));
                        }
                        None => {
                            g_mesh_plugin_sdk::log_line!(
                                "[{language}] could not ask the language server ({err:#})"
                            );
                            fail(format!("could not ask the language server: {err:#}"));
                        }
                    }
                    queue.clear();
                    break;
                }
            }
        }
        if in_flight.is_empty() && deferred.is_empty() {
            break;
        }

        let now = Instant::now();
        if now >= deadline {
            g_mesh_plugin_sdk::log_line!(
                "[{language}] the semantic pass ran out of its budget with {} question(s) \
                 outstanding and {} unasked",
                in_flight.len(),
                queue.len() + deferred.len()
            );
            fail(format!(
                "the pass ran out of its budget with {} question(s) outstanding and {} unasked",
                in_flight.len(),
                queue.len() + deferred.len()
            ));
            for (id, (question, _)) in std::mem::take(&mut in_flight) {
                client.cancel(id);
                failed_files.insert(question.accounted_to().clone());
            }
            for question in queue.drain(..).chain(deferred.drain(..)) {
                failed_files.insert(question.accounted_to().clone());
            }
            break;
        }

        if in_flight.is_empty() {
            // Only deferred questions left: nothing to time out, and nothing
            // to do but wait for the server to stop indexing.
            if matches!(client.poll(Duration::from_millis(25)), Poll::Closed) {
                for question in deferred.drain(..) {
                    failed_files.insert(question.accounted_to().clone());
                }
                fail(exited(client.exit_status(EXIT_GRACE)));
                break;
            }
            continue;
        }

        // Wake up for whichever comes first: an answer, the oldest request's
        // own timeout, or the end of the pass.
        let oldest = in_flight.values().map(|(_, sent)| *sent).min().unwrap_or(now);
        let wake = (oldest + request_budget).min(deadline);
        let wait = wake.saturating_duration_since(now).max(Duration::from_millis(1));

        match client.poll(wait) {
            Poll::Answered { id, result } => {
                let Some((question, sent)) = in_flight.remove(&id) else { continue };
                if let Some(timing) = timing.as_mut() {
                    timing.answers += 1;
                    timing.latencies.push(sent.elapsed());
                }
                // Any answer, empty or not, is a server that is answering.
                client.mark_warmed_up();
                // An empty answer while the server is indexing is not an
                // answer: re-ask it once, after the indexing ends.
                let empty = question.is_empty_answer(&result);
                let key =
                    (question.file.clone(), question.position.line, question.position.col, question.method());
                if empty && !client.quiet_for(budgets.settle) && re_asked.insert(key) {
                    deferred.push(question);
                    continue;
                }
                let again = record_answer(&mut answers, index, roots, client.encoding(), &question, &result);
                // The second hop of an implementation answer. Pushed onto the
                // front of the queue rather than the back so that a sweep's
                // follow-ups are asked while the questions that produced them
                // are still the server's warm working set.
                queue.extend(again);
            }
            Poll::Failed { id, code, message } => {
                let Some((question, sent)) = in_flight.remove(&id) else { continue };
                if let Some(timing) = timing.as_mut() {
                    timing.failed += 1;
                    timing.content_modified += usize::from(code == Some(CONTENT_MODIFIED));
                    timing.latencies.push(sent.elapsed());
                }
                client.mark_warmed_up();
                // `ContentModified` is not a refusal: the server's state moved
                // under the question (rust-analyzer switching crate graphs,
                // GM-433), so it is asked again once the server is quiet -
                // the same deferral, under the same once-only `re_asked`
                // rule, as an empty answer from a busy server. A second
                // `ContentModified` for the same site falls through to the
                // refusal below, so "again" cannot become "forever".
                if code == Some(CONTENT_MODIFIED) {
                    let key = (
                        question.file.clone(),
                        question.position.line,
                        question.position.col,
                        question.method(),
                    );
                    if re_asked.insert(key) {
                        deferred.push(question);
                        continue;
                    }
                }
                // A server that refuses one question has not answered it, so
                // the file it was in is not covered - but the pass goes on:
                // one bad position is not a reason to drop the other nine
                // thousand answers.
                g_mesh_plugin_sdk::log_line!(
                    "[{language}] the server refused a question about {} ({message})",
                    question.file
                );
                fail(format!("the language server refused a question about {} ({message})", question.file));
                failed_files.insert(question.accounted_to().clone());
            }
            Poll::Closed => {
                g_mesh_plugin_sdk::log_line!(
                    "[{language}] the language server exited during the pass - keeping the {} \
                     answer(s) it did give and reporting the pass incomplete",
                    answers.edges.len()
                );
                for (question, _) in std::mem::take(&mut in_flight).into_values() {
                    failed_files.insert(question.accounted_to().clone());
                }
                for question in queue.drain(..) {
                    failed_files.insert(question.accounted_to().clone());
                }
                fail(exited(client.exit_status(EXIT_GRACE)));
                break;
            }
            Poll::Noise => continue,
            Poll::Idle => {
                // Whatever woke us: retire every request that is past its own
                // timeout. A question with no answer is not a question
                // answered "nothing".
                let now = Instant::now();
                let expired: Vec<i64> = in_flight
                    .iter()
                    .filter(|(_, (_, sent))| now.duration_since(*sent) >= request_budget)
                    .map(|(id, _)| *id)
                    .collect();
                if !expired.is_empty() {
                    // A timed-out warm-up is spent too: it is not re-armed,
                    // so a server that never answers costs one long wait.
                    client.mark_warmed_up();
                }
                for id in expired {
                    let Some((question, _)) = in_flight.remove(&id) else { continue };
                    client.cancel(id);
                    g_mesh_plugin_sdk::log_line!(
                        "[{language}] the server did not answer a question about {} within {:?}{}",
                        question.file,
                        request_budget,
                        if warming { " (its warm-up budget)" } else { "" }
                    );
                    fail(format!(
                        "the language server did not answer a question about {} within {:?}",
                        question.file, request_budget
                    ));
                    failed_files.insert(question.accounted_to().clone());
                }
            }
        }
    }

    if let Some(mut timing) = timing {
        timing.latencies.sort_unstable();
        let ms = |at: Option<&Duration>| at.map_or(0, Duration::as_millis);
        g_mesh_plugin_sdk::log_timing!(
            "lsp_answered language={language} questions={} answers={} failed={} content_modified={} \
             max_request_ms={} p50_request_ms={}",
            timing.questions,
            timing.answers,
            timing.failed,
            timing.content_modified,
            ms(timing.latencies.last()),
            ms(timing.latencies.get(timing.latencies.len() / 2))
        );
    }
    let covered = touched_files.difference(&failed_files).cloned().collect();
    (answers, covered, failure)
}

/// What [`run_pass`] counts for its GM-435 timing line, only while
/// `G_MESH_EDIT_TIMING` is set.
#[derive(Default)]
pub(super) struct PassTiming {
    /// Requests sent, re-asks included.
    pub(super) questions: usize,
    /// Requests answered with a result, empty ones included.
    pub(super) answers: usize,
    /// Requests answered with an error, `ContentModified` included.
    pub(super) failed: usize,
    /// Errors that were `ContentModified`.
    pub(super) content_modified: usize,
    /// Send-to-reply time of every answered or failed request.
    pub(super) latencies: Vec<Duration>,
}

/// Turns one server answer into whatever it is evidence for, and into
/// whatever it still has to be asked.
///
/// The returned questions are the second hop of an implementation answer, or
/// the `hover` hop of an overload answer, and nothing else - see
/// [`LspBridge`]'s doc. Neither second hop returns any, which is what makes
/// the recursion one hop deep by construction rather than by a counter.
#[must_use]
pub(super) fn record_answer(
    answers: &mut Answers,
    index: &SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    question: &Question,
    result: &Value,
) -> Vec<Question> {
    let found = locations(result);
    match &question.ask {
        Ask::Definition(site) => {
            // An untyped receiver call onto an overload set takes the
            // overload path: the same edge, plus the ordinal when one can be
            // bound. A call that names a structural edge keeps the rules
            // below, which an ordinal would not survive (R2 records nothing).
            if site.kind == OpenSiteKind::ReceiverCall && site.replaces.is_none() {
                if let Some((path, node, ordinals)) = overload_landing(index, roots, encoding, &found) {
                    if node.declarations.is_some() {
                        *answers
                            .untyped_answered
                            .entry((site.from_id.clone(), site.name.clone()))
                            .or_default() += 1;
                        let landing = Landing {
                            file: question.file.clone(),
                            site: site.clone(),
                            node_id: node.id.clone(),
                            node_name: node.name.clone(),
                            address: address_of(node, site.from_container.clone()),
                            range: site_range(index.source(&question.file), site.position, &site.name),
                        };
                        return answers.bind_overload(index, landing, node, &path, &ordinals);
                    }
                }
            }
            // Several locations are the normal answer for a `cfg`-gated or
            // overloaded declaration. They are only usable when they agree:
            // core's linker refuses an ambiguous address by design, and
            // picking one here would be this bridge making the guess the
            // linker declines to make.
            let mut target: Option<(RelPath, &WireNode, &ServerLocation)> = None;
            let mut agree = true;
            for location in &found {
                let Some((path, node)) = node_at(index, roots, encoding, location) else { continue };
                match &target {
                    Some((_, chosen, _)) if chosen.id != node.id => {
                        agree = false;
                        break;
                    }
                    Some(_) => {}
                    None => target = Some((path, node, location)),
                }
            }
            // A call onto a const-bound function produced elsewhere (a
            // factory's result, `const f = throttle(g)`) is answered with two
            // locations: the const's name, and the call signature's own
            // declaration inside the factory, which TypeScript adds when the
            // signature is not declared by the symbol itself. Only the first
            // names a declaration, so a disagreeing `CALLS` answer is read
            // again keeping only the locations at their node's own name; one
            // node left is the target. `cfg` and overload twins all sit at
            // names and still disagree, which records nothing.
            if !agree && site.edge_kind == EdgeKind::Calls {
                (target, agree) = (named_landing(index, roots, encoding, &found), true);
            }
            if untyped_call_answered(index, roots, site, &found, agree && target.is_some()) {
                *answers.untyped_answered.entry((site.from_id.clone(), site.name.clone())).or_default() += 1;
            }
            // A call landing on anything but a function is kept only when
            // the server pointed at that node's own name: a const-bound
            // function or a tuple-struct constructor. Pointing inside it at
            // something without a node (a field, an object-literal method)
            // names the encloser, which is not what is called. Any edge
            // kind: a landing on the referrer or on anything containing the
            // site is the encloser fallback for a declaration with no node
            // (a nested fn or enum, a local, a generic parameter), unless it
            // points at that node's own name (recursion, `Self`).
            let encloses = |path: &RelPath, node: &WireNode| {
                node.id == site.from_id || (*path == question.file && node_contains(node, site.position))
            };
            let landed = target.filter(|(path, node, location)| {
                agree
                    && ((!encloses(path, node)
                        && (site.edge_kind != EdgeKind::Calls || node.kind == NodeKind::Function))
                        || lands_on_name(index, path, wire_position(index, path, encoding, location), node))
            });
            let Some((_, node, _)) = landed else {
                // Empty, ambiguous, or nothing this index holds: no evidence
                // against the structural edge, which therefore stands (R1).
                if let Some(replaced) = &site.replaces {
                    answers.upheld.insert(replaced.clone());
                }
                return Vec::new();
            };
            let address = address_of(node, site.from_container.clone());
            if let Some(replaced) = &site.replaces {
                // R2, agreement adds nothing: the answer lands on the
                // structural edge's own target (`Bound::Here`), or would get
                // the structural edge's own id (`Bound::There`, one address
                // and one key kind: a name-keyed placeholder and the
                // answer's qualified-name address never share an id, so a
                // name-keyed structural edge the linker left unlinked is
                // contradicted, not upheld),
                // or core linked the structural edge onto it (through a
                // re-export the edge's placeholder address names, which the
                // answered declaration's own address never matches).
                // Recording it anyway leaves two rows for one call once an
                // edit re-sends the structural edge - see [`LspBridge`]'s doc.
                let (_, prospective) =
                    answer_ids(&question.file, &site.from_id, site.edge_kind, &address, None);
                let lands_on_it = index
                    .graph(&question.file)
                    .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == replaced))
                    .is_some_and(|edge| edge.to_id == node.id);
                let linked_on_it = index.linked_target(replaced) == Some(node.id.as_str());
                if lands_on_it || linked_on_it || &prospective == replaced {
                    answers.upheld.insert(replaced.clone());
                    answers.confirmed.entry(replaced.clone()).or_default().push((
                        site.from_id.clone(),
                        site.edge_kind,
                        node.id.clone(),
                    ));
                    return Vec::new();
                }
            }
            // The placeholder is named after the declaration it waits on, not
            // after the text at this site: `SearchMode::Count` and
            // `SearchMode::CountMatches` are two spellings of one address, and
            // a node that has to speak for both can only honestly carry the
            // name they have in common. The site still supplies the *range* -
            // as wide as what is written there - and which site that is comes
            // from `Placeholder`, not from which answer arrived first.
            answers.record(
                &question.file,
                &question.file,
                &site.from_id,
                site.edge_kind,
                &node.name,
                site_range(index.source(&question.file), site.position, &site.name),
                address,
                &node.id,
                None,
            );
            // The contradiction rule: an answer that lands somewhere else
            // retracts the structural edge the site said it replaces - see
            // [`LspBridge`].
            if let Some(replaced) = &site.replaces {
                answers.contradicted.insert(replaced.clone());
                answers.retract.insert(replaced.clone());
            }
            Vec::new()
        }
        Ask::Implementation { anchor } => {
            let mut again = Vec::new();
            for location in &found {
                if record_implementor(answers, index, roots, encoding, anchor, &question.file, location) {
                    continue;
                }
                // The location is in a file this index holds, at a position no
                // declaration of it covers: an `impl` header, or whatever else
                // the language spells an implementation with. Ask the server
                // what is written there - see [`LspBridge`]'s doc.
                let Some(path) = file_at(index, roots, location) else { continue };
                again.push(Question {
                    position: wire_position(index, &path, encoding, location),
                    file: path,
                    ask: Ask::Implementor { anchor: anchor.clone(), for_file: question.file.clone() },
                });
            }
            again
        }
        Ask::Implementor { anchor, for_file } => {
            for location in &found {
                record_implementor(answers, index, roots, encoding, anchor, for_file, location);
            }
            Vec::new()
        }
        Ask::Overload(site) => {
            let Some(replaced) = &site.replaces else { return Vec::new() };
            // Nothing landed, the locations disagree, or they land somewhere
            // the structural edge does not: no binding, and the structural
            // edge stands. Refining never moves a call.
            let Some((path, node, ordinals)) = overload_landing(index, roots, encoding, &found) else {
                return Vec::new();
            };
            if !refines(index, &question.file, site, replaced, node) {
                return Vec::new();
            }
            let landing = Landing {
                file: question.file.clone(),
                site: site.clone(),
                node_id: node.id.clone(),
                node_name: node.name.clone(),
                address: address_of(node, site.from_container.clone()),
                range: site_range(index.source(&question.file), site.position, &site.name),
            };
            answers.bind_overload(index, landing, node, &path, &ordinals)
        }
        Ask::OverloadHover { pending, at, .. } => {
            answers.hovered(*pending, at, hover_text(result));
            Vec::new()
        }
    }
}

/// Whether one answer to `site` counts towards dropping its name from the
/// caller's `untypedCalls` (GM-486, D3 as extended after the noise
/// measurement).
///
/// Only an untyped receiver call is counted: a `ReceiverCall` site that
/// replaces no structural edge, which is exactly what
/// `FileGraphBuilder::finish` folds into `untypedCalls`. Such a call counts
/// as answered when the server found something and every place it found is
/// accounted for: either this pass recorded an edge for it (`recorded`), or
/// every location lies outside the files this index holds - a std or
/// dependency method, which can never become an edge and is the noise the
/// marker was measured to carry. An empty answer is not an answer. Neither is
/// one that lands in an indexed file without becoming an edge (targets that
/// disagree, or a position no addressable declaration covers): the call may
/// well be to a project method, and the marker is what says its edge is
/// missing.
pub(super) fn untyped_call_answered(
    index: &SdkIndex,
    roots: [&Path; 2],
    site: &OpenSite,
    found: &[ServerLocation],
    recorded: bool,
) -> bool {
    if site.kind != OpenSiteKind::ReceiverCall || site.replaces.is_some() || found.is_empty() {
        return false;
    }
    recorded || found.iter().all(|location| file_at(index, roots, location).is_none())
}

/// The caller nodes whose `untypedCalls` this pass shortens or restores, ready
/// to upsert (GM-486).
///
/// A name leaves a node's list once every untyped receiver-call site of that
/// name in that node was answered ([`untyped_call_answered`]); a name with one
/// site left unanswered stays. The list each node starts from is the
/// structural one the SDK index holds, so a structural reparse that re-sends
/// the full list is undone by the next pass's answers, the same way edges are.
///
/// Only `files` are considered: the files this pass both asked about and
/// finished. A file the pass did not finish says nothing about its sites, so
/// its nodes are left as core holds them, as an earlier pass's edges are. In a
/// finished file a node is re-sent when its list got shorter, or when an
/// earlier pass shortened it (`trimmed`) and this one no longer does - so a
/// name whose answers stopped arriving comes back.
///
/// The node goes out whole, as the index holds it, with only `untypedCalls`
/// changed: core's upsert replaces every column and child row from the
/// record, so anything less would erase what the structural tier sent.
pub(super) fn trim_untyped_calls(
    index: &SdkIndex,
    files: &BTreeSet<RelPath>,
    answered: &HashMap<(String, String), usize>,
    trimmed: &mut BTreeMap<RelPath, BTreeSet<String>>,
) -> Vec<WireNode> {
    let mut upserts = Vec::new();
    for file in files {
        let previously = trimmed.remove(file).unwrap_or_default();
        let Some(entry) = index.entry(file) else { continue };
        let mut sites: HashMap<(&str, &str), usize> = HashMap::new();
        for site in &entry.graph.open_sites {
            if site.kind == OpenSiteKind::ReceiverCall && site.replaces.is_none() {
                *sites.entry((site.from_id.as_str(), site.name.as_str())).or_default() += 1;
            }
        }
        let mut now = BTreeSet::new();
        for node in entry.graph.nodes.iter().filter(|node| !node.untyped_calls.is_empty()) {
            let kept: Vec<String> = node
                .untyped_calls
                .iter()
                .filter(|name| {
                    let total = sites.get(&(node.id.as_str(), name.as_str())).copied().unwrap_or(0);
                    let done = answered.get(&(node.id.clone(), (*name).clone())).copied().unwrap_or(0);
                    total == 0 || done < total
                })
                .cloned()
                .collect();
            if kept.len() < node.untyped_calls.len() {
                now.insert(node.id.clone());
                upserts.push(WireNode { untyped_calls: kept, ..node.clone() });
            } else if previously.contains(&node.id) {
                upserts.push(node.clone());
            }
        }
        if !now.is_empty() {
            trimmed.insert(file.clone(), now);
        }
    }
    upserts
}

/// Records one implementor of `anchor`, if `location` is a declaration this
/// index holds. `false` means it is not one - which is a question for the
/// caller, not an error.
///
/// `for_file` is the file whose question produced this, and it is the
/// anchor's, never the implementor's: see [`Ask::Implementor`].
///
/// # Only a `Type` implements anything (GM-361)
///
/// [`SdkIndex::node_at`] answers with the **smallest node containing** the
/// position, which is not the same as "the declaration written there". An
/// `impl` header inside an inline module is contained by that module's own
/// node and by nothing smaller, so the sweep recorded the *module* as an
/// implementor: measured on ripgrep,
/// `find_implementations("Sink")` carried a row
/// `sink::sinks @ crates/searcher/src/sink.rs:516`, which is
/// `pub mod sinks { … }`, alongside the three types declared inside it.
///
/// A module implements nothing, in any language this bridge serves, so a
/// node that is not a [`NodeKind::Type`] is refused here. Refused, not
/// dropped: `false` is exactly the signal that sends an
/// [`Ask::Implementation`] answer to its second hop, which asks the server
/// what is written at that position and gets the implementing declaration -
/// the same path an `impl` header at a file's top level already took, where
/// `node_at` found only the `File` node.
///
/// # A structural edge that already says it wins
///
/// An implementor whose file holds a structural `SUPERTYPE_OF` edge from the
/// same node that reaches the anchor ([`supertype_reaches`]) is accounted
/// for and records nothing. The answer's qualified-name address never shares
/// an id with a name-keyed placeholder, so recording it would leave two rows
/// for one `impl` - see ADR 0035.
pub(super) fn record_implementor(
    answers: &mut Answers,
    index: &SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    anchor: &str,
    for_file: &RelPath,
    location: &ServerLocation,
) -> bool {
    let Some((_, anchor_node)) = index.node(anchor) else { return false };
    let anchor_node = anchor_node.clone();
    let Some((path, node)) = node_at(index, roots, encoding, location) else { return false };
    if node.id == anchor_node.id {
        // A server that lists the trait's own declaration among its
        // implementations, which some do - and, for the second hop, the
        // `Trait` half of `impl Trait for T` if a server ever points there.
        return true;
    }
    if node.kind != NodeKind::Type {
        // A container that merely encloses the position, not the declaration
        // written at it - see this function's own doc.
        return false;
    }
    if supertype_reaches(index, &path, &node.id, &anchor_node) {
        // The structural edge already says this; see this function's doc.
        return true;
    }
    let (from_id, at, container) = (node.id.clone(), node.range.start, node.container.clone());
    answers.record(
        for_file,
        &path,
        &from_id,
        // The direction `find_implementations` walks: subtype -> supertype.
        // The question was "who implements this", so the edge starts at each
        // answer and points at the anchor.
        EdgeKind::SupertypeOf,
        &anchor_node.name,
        site_range(None, at, &anchor_node.name),
        address_of(&anchor_node, container),
        &anchor_node.id,
        None,
    );
    true
}

/// Whether `path`'s structural graph has a `SUPERTYPE_OF` edge from `from_id`
/// that reaches `anchor`: it targets the anchor's node, core linked it onto
/// that node, or its placeholder target is the anchor's own scope with a
/// `name` key equal to the anchor's `qualifiedName`. The last holds without
/// the linked map, which a pass that did not link the file lacks, but only
/// while core has not linked the edge onto a different node: a placeholder
/// core resolved elsewhere names another type that shares the anchor's
/// name, and the semantic answer is the one to keep.
pub(super) fn supertype_reaches(index: &SdkIndex, path: &RelPath, from_id: &str, anchor: &WireNode) -> bool {
    let Some(graph) = index.graph(path) else { return false };
    let scope = address_of(anchor, None).scope;
    let name_keyed_on_anchor = |to_id: &str| {
        graph.nodes.iter().any(|node| {
            node.id == to_id
                && node.native_kind.as_deref() == Some("pending_symbol")
                && node.target.as_ref().is_some_and(|target| {
                    target.scope == scope
                        && matches!(&target.key, TargetKey::Name(name) if *name == anchor.qualified_name)
                })
        })
    };
    graph.edges.iter().any(|edge| {
        edge.kind == EdgeKind::SupertypeOf
            && edge.from_id == from_id
            && (edge.to_id == anchor.id
                || index.linked_target(&edge.id) == Some(anchor.id.as_str())
                || (index.linked_target(&edge.id).is_none() && name_keyed_on_anchor(&edge.to_id)))
    })
}

impl SemanticEngine for LspBridge {
    /// A project-model change makes the running server prove its readiness
    /// again: the next pass waits for a full settle (GM-433) - see
    /// [`LspClient::unsettle`] for why, and for why an on-demand server is
    /// left as it is. No server running means nothing to do: the next one
    /// starts unsettled anyway.
    fn workspace_changed(&mut self) {
        if let Some(client) = self.client.as_mut() {
            client.unsettle();
        }
    }

    /// A whole-project pass is owed: start the server now, so it indexes
    /// while core is busy elsewhere. Through [`LspBridge::ensure_client`], so
    /// the server is registered for `kill_live_servers`, counts against
    /// `MAX_SERVER_STARTS` and a missing binary turns the tier off exactly as
    /// a pass's start would. A running server is left as it is, settled or
    /// not. Readiness stays the pass's question: what the server sends
    /// meanwhile queues on the client's channel and is read by `wait_ready`.
    fn prepare(&mut self) {
        let deadline = Instant::now() + self.budgets.single_file;
        self.ensure_client(deadline);
    }

    fn set_pass_deadline(&mut self, deadline: Option<Instant>) {
        self.core_deadline = deadline;
    }

    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<SemanticAnswer> {
        let whole_project = files.is_empty();
        if self.profile.client_watches_files {
            self.unforwarded.extend(files.iter().cloned());
        }
        // What this pass was sent - core adds the files earlier passes did not
        // finish itself. A file no longer in the index was deleted or
        // failed to extract: there is nothing left to ask about it, so it is
        // not unfinished either.
        let scope: Vec<RelPath> = if whole_project {
            index.paths()
        } else {
            let scope: BTreeSet<RelPath> =
                files.iter().filter(|path| index.entry(path).is_some()).cloned().collect();
            scope.into_iter().collect()
        };
        if scope.is_empty() {
            return Ok(SemanticAnswer::complete(FileChangeDiff::default()).with_unfinished(BTreeSet::new()));
        }

        let started = Instant::now();
        let deadline = self.pass_deadline(started, whole_project, scope.len());
        let plan = questions(index, &scope, &self.config, &self.budgets);
        if plan.unanswerable > 0 {
            g_mesh_plugin_sdk::log_line!(
                "[{}] {} open site(s) of a kind this bridge does not answer were skipped",
                self.language,
                plan.unanswerable
            );
        }
        if plan.asking.is_empty() {
            if plan.truncated {
                // An empty list the ceiling produced, not one the scope did:
                // `max_sites` is zero, so not one file was admitted. Nothing
                // was asked, so nothing is covered - and in particular nothing
                // may be retracted, which is what this branch used to do to
                // every file in scope on the strength of a list it had refused
                // to build (GM-319). An unasked site is not a site that went
                // away.
                return Ok(SemanticAnswer::incomplete_because(
                    FileChangeDiff::default(),
                    "the open-site ceiling (max_sites) admitted no site",
                )
                .with_unfinished(scope.into_iter().collect()));
            }
            // Nothing to ask means nothing to start a compiler for. The files
            // in scope are still *covered*, so an earlier pass's answers about
            // sites that have since gone away are retracted.
            let mut answers = Answers::new(&self.language, &self.config.engine);
            let covered: BTreeSet<RelPath> = scope.iter().cloned().collect();
            retract_stale(&mut answers, &self.emitted, &covered, &BTreeMap::new());
            for file in &covered {
                self.emitted.insert(file.clone(), Vec::new());
            }
            let (mut diff, _) = answers.finish();
            // No question means no untyped receiver call in these files, so
            // this only forgets what an earlier pass trimmed in them.
            diff.upsert_nodes.extend(trim_untyped_calls(index, &covered, &HashMap::new(), &mut self.trimmed));
            // Nothing left to ask is nothing unfinished.
            return Ok(SemanticAnswer::complete(diff).with_unfinished(BTreeSet::new()));
        }

        let language = self.language.clone();
        let language_ids = self.profile.language_ids;
        let engine = self.config.engine.clone();
        let budgets = self.budgets;
        let disambiguation = self.config.overload_disambiguation;
        let root = self.root.clone();
        let real_root = self.real_root.clone();
        let mut opened = std::mem::take(&mut self.opened);
        let mut unforwarded = std::mem::take(&mut self.unforwarded);
        let asked_about: BTreeSet<RelPath> =
            plan.asking.iter().map(|question| question.file.clone()).collect();
        let outcome = {
            // The client is borrowed for the whole pass, so everything else
            // this needs was cloned or moved out of `self` above.
            match self.ensure_client(deadline) {
                None => Err(None),
                Some(client) => {
                    client.drain();
                    // An open or indexed file this pass was sent is synced
                    // even when nothing in it is asked: the server then reads
                    // it from the client, not from disk (GM-624).
                    let mut synced =
                        Self::forward_watched(client, &mut unforwarded, &mut opened, &root, index);
                    synced.extend(asked_about.iter().cloned());
                    Self::sync_documents(client, &mut opened, &root, index, &synced, &language, language_ids);
                    g_mesh_plugin_sdk::log_timing!("lsp_synced language={language} files={}", synced.len());
                    let ready_started = Instant::now();
                    let ready = Self::wait_ready(client, &budgets, deadline, &language);
                    g_mesh_plugin_sdk::log_timing!(
                        "lsp_ready language={language} waited_ms={} ready={}",
                        ready_started.elapsed().as_millis(),
                        ready.is_ok()
                    );
                    match ready {
                        Ok(()) => Ok(run_pass(
                            client,
                            &language,
                            &engine,
                            [&root, &real_root],
                            index,
                            plan.asking,
                            &budgets,
                            deadline,
                            disambiguation,
                        )),
                        Err(reason) => Err(Some(reason)),
                    }
                }
            }
        };
        self.opened = opened;
        self.unforwarded = unforwarded;

        let (mut answers, covered, failure) = match outcome {
            Ok(outcome) => outcome,
            // No server, or one that never became ready: no answers, and -
            // crucially - nothing recorded as "no target", which is what the
            // readiness rule exists to prevent.
            Err(reason) => {
                let reason = reason
                    .or_else(|| self.start_failure.clone())
                    .unwrap_or_else(|| "the language server is not running".to_string());
                // Not one file was finished: every one asked about is
                // unfinished. A file in scope with no question has nothing to
                // finish, and the ceiling did not cut any (`plan.truncated`
                // aside, which this answer is incomplete for anyway).
                return Ok(SemanticAnswer::incomplete_because(FileChangeDiff::default(), reason)
                    .with_unfinished(asked_about));
            }
        };

        // The files this pass asked about and finished. A file reached only by
        // an implementation sweep's second hop is covered without its own
        // sites having been asked, so it is not one of them.
        let finished: BTreeSet<RelPath> = covered.intersection(&asked_about).cloned().collect();
        // Restores structural edges and drops the semantic edges they cover,
        // before `by_file` is read: a dropped edge an earlier pass emitted is
        // then retracted below.
        answers.settle(index, &finished);
        let produced = answers.by_file.clone();
        retract_stale(&mut answers, &self.emitted, &covered, &produced);
        let untyped_answered = std::mem::take(&mut answers.untyped_answered);
        let (mut diff, emitted) = answers.finish();
        // Callers whose untyped receiver calls were all answered (GM-486), in
        // the files this pass finished.
        diff.upsert_nodes.extend(trim_untyped_calls(index, &finished, &untyped_answered, &mut self.trimmed));
        // A file this pass finished gets a fresh baseline: what it produced is
        // now the whole truth about that file's semantic edges. A file it did
        // *not* finish keeps its old ids as well as the new ones - the old ones
        // may still describe live code that this pass simply never got to, and
        // forgetting them here would mean nothing ever retracts them.
        for file in &covered {
            self.emitted.insert(file.clone(), emitted.get(file).cloned().unwrap_or_default());
        }
        for (file, ids) in emitted {
            if covered.contains(&file) {
                continue;
            }
            let baseline = self.emitted.entry(file).or_default();
            for id in ids {
                if !baseline.contains(&id) {
                    baseline.push(id);
                }
            }
        }

        g_mesh_plugin_sdk::log_timing!(
            "lsp_done language={} files={} nodes={} edges={} retracted={} incomplete={}",
            self.language,
            scope.len(),
            diff.upsert_nodes.len(),
            diff.upsert_edges.len(),
            diff.delete_edge_ids.len(),
            failure.is_some() || plan.truncated
        );
        g_mesh_plugin_sdk::log_line!(
            "[{}] semantic pass: {} file(s), {} node(s)/{} edge(s) upserted, {} edge(s) retracted, \
             in {:?}{}",
            self.language,
            scope.len(),
            diff.upsert_nodes.len(),
            diff.upsert_edges.len(),
            diff.delete_edge_ids.len(),
            started.elapsed(),
            if failure.is_none() && !plan.truncated { "" } else { " (incomplete)" }
        );
        let failure = failure.or_else(|| {
            plan.truncated.then(|| "the open-site ceiling (max_sites) left sites unasked".to_string())
        });
        // What core is to ask again: every file asked about and not finished,
        // and - when the ceiling cut the list - every file in scope it left
        // unasked, since a file with no question is settled only when the
        // ceiling is not why it had none.
        let mut unfinished: BTreeSet<RelPath> = asked_about.difference(&finished).cloned().collect();
        if plan.truncated {
            unfinished.extend(scope.iter().filter(|file| !asked_about.contains(*file)).cloned());
        }
        Ok(SemanticAnswer {
            diff,
            complete: failure.is_none(),
            reason: failure,
            unfinished: Some(unfinished),
        })
    }
}

/// Retracts what an earlier pass emitted for a file this pass finished and did
/// not emit again - see [`LspBridge`]'s doc on retraction.
pub(super) fn retract_stale(
    answers: &mut Answers,
    remembered: &BTreeMap<RelPath, Vec<String>>,
    covered: &BTreeSet<RelPath>,
    now: &BTreeMap<RelPath, Vec<String>>,
) {
    for file in covered {
        let Some(previous) = remembered.get(file) else { continue };
        let still: HashSet<&String> = now.get(file).map(|ids| ids.iter().collect()).unwrap_or_default();
        for id in previous {
            if !still.contains(id) {
                answers.retract.insert(id.clone());
            }
        }
    }
}
