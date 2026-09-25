//! Assembles `get_info`'s `with_instructions` string from the languages
//! present in this project's index (design and measurements:
//! `docs/adr/0003-mcp-instructions-rendering.md`). Invariants:
//! - Every rendering fits [`INSTRUCTIONS_BYTE_CEILING`], a margin under
//!   Claude Code's 2KB truncation of `with_instructions`.
//! - The receiver-call gap (`x.foo()`) is rendered from data, never stated as
//!   a constant: Go and Rust resolve it only once their semantic pass has
//!   completed (`language_state.semanticPassAt`), TypeScript never does.
//! - A language is rendered by its manifest `language` id; core holds no
//!   display-name table and no per-language syntax.
//! - `get_info` is read once per session and `semanticPassAt` only moves from
//!   unset to set, so a session can over-report a gap, never under-report one.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::daemon::candidates::Detection;
use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};

/// Working byte ceiling; one constant because [`build`]'s fallback decision
/// and the worst-case test must agree on the same figure.
pub const INSTRUCTIONS_BYTE_CEILING: usize = 1900;

/// Prefixed while the project owes its cold start (`Phase::Unindexed` or
/// `Phase::Walking`): the walk is running now or starts on the first tool
/// call, and the root tells a caller's g-mesh sessions apart. A root too long
/// for [`INSTRUCTIONS_BYTE_CEILING`] falls back to [`cold_start_line_fallback`],
/// so this line is never what breaks the ceiling.
fn cold_start_line(root: &Path, walking: bool) -> String {
    if walking {
        format!(
            "Index root: {}. Being built now - the first tool call waits for it to finish before answering.",
            root.display()
        )
    } else {
        format!(
            "Index root: {}. Not indexed yet - the first tool call builds it (structural first; semantic \
             search after) and waits for it.",
            root.display()
        )
    }
}

/// [`cold_start_line`] without the root (D12 in
/// `docs/architecture/lazy-indexing.md`).
fn cold_start_line_fallback(walking: bool) -> &'static str {
    if walking {
        "Being built now - the first tool call waits for it to finish before answering."
    } else {
        "Not indexed yet - the first tool call builds it (structural first; semantic search after) and \
         waits for it."
    }
}

/// [`cold_start_line`] (or its no-path fallback) on top of the [`build`]
/// rendering for `present`, which is capabilities-only: during the cold start
/// `mcp::mod::GMeshMcpServer::instructions` never takes the connection lock,
/// because the walk or its batch commit may hold it through embedding
/// inference, and a caller mid-handshake must not wait on it.
pub fn cold_start(root: &Path, walking: bool, present: &[PresentLanguage]) -> String {
    let built = build(present);
    let with_path = format!("{}\n\n{built}", cold_start_line(root, walking));
    if with_path.len() <= INSTRUCTIONS_BYTE_CEILING {
        with_path
    } else {
        format!("{}\n\n{built}", cold_start_line_fallback(walking))
    }
}

/// One language present in the index, with its manifest capabilities and
/// whether its semantic pass has completed at least once project-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentLanguage {
    /// The manifest's own `language` id, e.g. `"typescript"`, rendered as-is.
    pub language: String,
    /// This language's `[plugin.capabilities]`, or the conservative
    /// [`Capabilities::default`] when its plugin was since removed.
    pub capabilities: Capabilities,
    /// Whether `language_state.semanticPassAt` is set for this language (see
    /// [`has_open_receiver_gap`] for when it matters).
    pub semantic_pass_done: bool,
}

/// Whether `language`'s receiver-call gap is still open. Mirrors
/// `daemon::manifest::Capabilities::receiver_calls`: a structural tier that
/// resolves receiver calls closes it; otherwise it is open until a semantic
/// tier that can resolve it (`receiver_calls = Resolved`) has run. Until then
/// the edges are not in the index, and the text must not promise them.
fn has_open_receiver_gap(present: &PresentLanguage) -> bool {
    if present.capabilities.receiver_calls_structural == ReceiverCallResolution::Resolved {
        return false;
    }
    !(present.capabilities.receiver_calls == ReceiverCallResolution::Resolved && present.semantic_pass_done)
}

/// Every language in `present` with an open receiver-call gap, sorted by id
/// so the rendered list does not depend on `HashMap` iteration order.
fn languages_with_open_receiver_gap(present: &[PresentLanguage]) -> Vec<String> {
    let mut gapped: Vec<String> =
        present.iter().filter(|p| has_open_receiver_gap(p)).map(|p| p.language.clone()).collect();
    gapped.sort();
    gapped
}

/// Renders `names` as an English list: `"go"`, `"go and rust"`, `"go, rust
/// and typescript"` (no Oxford comma: a byte saved per rendering).
fn format_language_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = names.split_last().expect("non-empty per the two arms above");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

const P1: &str = "Structural code-graph queries over this project's index. Prefer these over \
     grepping when you need definitions, references, call edges or imports.";

const P2: &str = "A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
     (excludes other same-named declarations' call sites, same guarantee either \
     way), is already resolved per call site to that exact declaration - do not \
     re-check it with grep as a routine habit. Only fall back to grep for the one \
     specific gap below, never as a general double-check.";

const P3: &str = "`resolved: false` marks the one thing the indexer could not settle alone: an \
     edge whose target is in *another* file, where whether that file exports the \
     name isn't knowable from the usage alone. Every same-file edge is \
     `resolved: true` - never a reason to grep. find_references/find_callers/\
     find_callees/find_implementations also carry a response-level \
     `allUnresolved: true` when *every* row in a non-empty page is unconfirmed - \
     the page otherwise looks complete (`hasMore: false`, plausible results), so \
     check this field, not just individual rows. Never set on an empty page.";

/// The receiver-call paragraph with no language list: used when nothing is
/// known yet or exactly one language is present (see [`build`]).
const P4_GENERIC: &str = "The one legitimate reason to grep afterward: a method call through a \
     variable receiver (`x.foo()`) produces no edge by design, so caller/reference \
     lists for methods can under-report; bare function calls and this/super/qualified-type \
     calls have no such gap, and a `hasMore: false` page for those is exhaustive. On a \
     project's first index, or a re-index after an upgrade, a tool call waits for the walk \
     to finish before answering - slow, not wrong; do not abandon it for grep.";

/// Every present language *resolves* receiver calls, so clause (1) says they
/// bind to the declared or inferred type: an override's caller page
/// under-reports, and the missing calls sit on the base's page. It must never
/// pass an implementor count off as missing callers (ADR 0003).
const P4_STATIC_RECEIVER: &str =
    "The one legitimate reason to grep afterward: a method call through a variable \
     receiver (`x.foo()`) binds to the receiver's declared or inferred type, not \
     the one it holds at run time, so an override's caller page under-reports - \
     calls reaching it through a base or interface sit on that base's page, and \
     find_implementations is the way across. On a project's first index, or a \
     re-index after an upgrade, a tool call waits for the walk to finish before \
     answering - slow, not wrong; do not abandon it for grep.";

/// [`P4_GENERIC`] with `"by design"` replaced by `"in {list}"` and nothing
/// else, so the bare/this/super/qualified-type reassurance stays universally
/// true without per-language syntax in core.
fn p4_named(list: &str) -> String {
    format!(
        "The one legitimate reason to grep afterward: a method call through a \
         variable receiver (`x.foo()`) produces no edge in {list}, so caller/reference \
         lists for methods can under-report; bare function calls and this/super/qualified-type \
         calls have no such gap, and a `hasMore: false` page for those is exhaustive. On a \
         project's first index, or a re-index after an upgrade, a tool call waits for the walk \
         to finish before answering - slow, not wrong; do not abandon it for grep."
    )
}

/// Used when [`p4_named`] exceeds [`INSTRUCTIONS_BYTE_CEILING`]: a generic
/// sentence plus "check which", never a list truncated mid-name, and no
/// pointer to a response field (none carries this fact yet).
fn p4_fallback() -> String {
    "The one legitimate reason to grep afterward: a method call through a variable \
     receiver (`x.foo()`) produces no edge in some of this project's languages until \
     their semantic layer finishes - check which before trusting a method's page as \
     exhaustive. On a project's first index, or a re-index after an upgrade, a tool \
     call waits for the walk to finish before answering - slow, not wrong; do not \
     abandon it for grep."
        .to_string()
}

const P5: &str = "Efficient usage: pass `symbol_name` directly to the four tools above instead \
     of calling find_definition first, and raise `limit` for symbols with many \
     results instead of paging.";

/// Joins the five paragraphs blank-line separated, nothing trimmed or
/// reflowed; only `p4` varies.
fn assemble(p4: &str) -> String {
    [P1, P2, P3, p4, P5].join("\n\n")
}

/// Builds `get_info`'s `with_instructions` string for this session. Cases, in
/// the order they are checked:
/// 1. `present` is empty (no index yet): [`P4_GENERIC`]. When the DB read
///    fails, `get_info` passes a capabilities-only `present` instead.
/// 2. No present language has an open gap: [`P4_STATIC_RECEIVER`].
/// 3. Exactly one language is present (so it has the gap): [`P4_GENERIC`],
///    unnamed, so a TypeScript-only project renders the same text as an empty
///    index (`ts_only_is_byte_identical_to_the_original_string`).
/// 4. Two or more are present and at least one is gapped: [`p4_named`],
///    naming every gapped language even when that is all of them, falling
///    back to [`p4_fallback`] above [`INSTRUCTIONS_BYTE_CEILING`].
pub fn build(present: &[PresentLanguage]) -> String {
    if present.is_empty() {
        return assemble(P4_GENERIC);
    }

    let gapped = languages_with_open_receiver_gap(present);
    if gapped.is_empty() {
        return assemble(P4_STATIC_RECEIVER);
    }
    if present.len() == 1 {
        return assemble(P4_GENERIC);
    }

    let list = format_language_list(&gapped);
    let named = assemble(&p4_named(&list));
    if named.len() <= INSTRUCTIONS_BYTE_CEILING {
        named
    } else {
        assemble(&p4_fallback())
    }
}

/// Zips present-language/semantic-state pairs with the registry's
/// capabilities; a language with no manifest gets [`Capabilities::default`].
pub fn present_languages(
    present: Vec<(String, bool)>,
    capabilities: &HashMap<String, Capabilities>,
) -> Vec<PresentLanguage> {
    present
        .into_iter()
        .map(|(language, semantic_pass_done)| {
            let capabilities = capabilities.get(&language).copied().unwrap_or_default();
            PresentLanguage { language, capabilities, semantic_pass_done }
        })
        .collect()
}

/// How a front counts its projects: `N`, or `N+` when the walk stopped at a
/// limit and more may exist.
pub(crate) fn project_count(count: usize, truncated: bool) -> String {
    if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    }
}

const INDEXED_MARK: &str = " (indexed)";

/// The front's sentence (D12), with `subject` naming the folder: the root
/// path or the no-path fallback. It stays true after a session switch (D11
/// step 5): it says "before any other tool", never "nothing is selected".
/// `indexed` counts the candidates with a completed index
/// (`candidates::has_completed_index`); when non-zero the sentence says they
/// are listed first and marked with [`INDEXED_MARK`].
fn front_sentence(subject: &str, count: &str, indexed: usize) -> String {
    let state = if indexed == 0 {
        "has indexed none of them".to_string()
    } else {
        format!("has already indexed {indexed} of them, listed first and marked (indexed)")
    };
    format!(
        "{subject} {count} projects; g-mesh serves one at a time and {state}. \
         Before any other g-mesh tool, call select_project with the one you are working on (ask the \
         user if unclear). Its result names the project this session then serves and carries that \
         project's guidance; call it again to switch, or to re-read that guidance. Projects: "
    )
}

/// `head` followed by as many of `names` as fit under
/// [`INSTRUCTIONS_BYTE_CEILING`], then either `.` (all listed) or the `(+K
/// more ...)` pointer, with how many names made it. `None` when not even the
/// head and its ending fit.
fn front_with_names(head: &str, names: &[&str]) -> Option<(String, usize)> {
    let ending = |listed: usize| {
        if listed == names.len() {
            ".".to_string()
        } else {
            format!(
                " (+{} more - call select_project with no argument for the full list).",
                names.len() - listed
            )
        }
    };
    let mut listed = 0;
    let mut body = String::new();
    while listed < names.len() {
        let separator = if listed == 0 { "" } else { ", " };
        let next = format!("{body}{separator}{}", names[listed]);
        if head.len() + next.len() + ending(listed + 1).len() > INSTRUCTIONS_BYTE_CEILING {
            break;
        }
        body = next;
        listed += 1;
    }
    let rendered = format!("{head}{body}{}", ending(listed));
    (rendered.len() <= INSTRUCTIONS_BYTE_CEILING).then_some((rendered, listed))
}

/// `get_info`'s instructions for a front (D12 in
/// `docs/architecture/lazy-indexing.md`): [`P1`], then the folder sentence and
/// as many candidate `rel_path`s as fit under [`INSTRUCTIONS_BYTE_CEILING`].
/// No language paragraphs: the selected project's guidance arrives in the
/// `select_project` result. Indexed candidates come first, marked with
/// [`INDEXED_MARK`], so the ceiling cuts unindexed names first; the rest keep
/// their walk order. When the root path would cost the list its first name
/// (or break the ceiling), the path is dropped, as in [`cold_start`].
pub fn build_front(root: &Path, detection: &Detection, indexed: &HashSet<&str>) -> String {
    let (done, rest): (Vec<&str>, Vec<&str>) =
        detection.candidates.iter().map(|c| c.rel_path.as_str()).partition(|name| indexed.contains(name));
    let marked: Vec<String> = done.iter().map(|name| format!("{name}{INDEXED_MARK}")).collect();
    let names: Vec<&str> = marked.iter().map(String::as_str).chain(rest).collect();
    let count = project_count(names.len(), detection.truncated);
    let with_path = format!(
        "{P1}\n\n{}",
        front_sentence(&format!("{} is a folder of", root.display()), &count, done.len())
    );
    if let Some((rendered, listed)) = front_with_names(&with_path, &names) {
        if listed > 0 || names.is_empty() {
            return rendered;
        }
    }
    let without_path = format!("{P1}\n\n{}", front_sentence("This folder holds", &count, done.len()));
    front_with_names(&without_path, &names).map(|(rendered, _)| rendered).unwrap_or(without_path)
}

#[cfg(test)]
mod tests;
