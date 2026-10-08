//! Assembles `get_info`'s `with_instructions` string from this project's
//! language coverage (design and measurements:
//! `docs/adr/0003-mcp-instructions-rendering.md`, amended by
//! `docs/adr/0022-instructions-coverage-states.md`). Invariants:
//! - Every rendering fits [`INSTRUCTIONS_BYTE_CEILING`], a margin under
//!   Claude Code's 2KB truncation of `with_instructions`.
//! - Which languages have answers, and which do not and why (no plugin, a
//!   failed plugin, or a language g-mesh does not know), is always said. The
//!   trim ladder in [`build_within`] never drops an absent or failed
//!   language's name or its install command.
//! - The receiver-call gap (`x.foo()`) is rendered from manifest capabilities
//!   only, never from `language_state.semanticPassAt`: the text is read once
//!   per session, so it says only what is true at every moment of it. Pass
//!   state reaches the caller live, per answer, through `mcp::provenance`.
//! - A language is rendered by its manifest `language` id; core holds no
//!   display-name table and no per-language syntax.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::daemon::candidates::Detection;
use crate::daemon::manifest::{Capabilities, MemberOverrides, ReceiverCallResolution};
use crate::languages::LanguageOutcome;

/// Working byte ceiling; one constant because [`build`]'s trim ladder and the
/// worst-case tests must agree on the same figure.
pub const INSTRUCTIONS_BYTE_CEILING: usize = 1900;

/// Said after the cold-start line: the wait is the walk, not a failure.
const WAIT_IS_NOT_WRONG: &str = " - slow, not wrong; do not abandon it for grep.";

/// Prefixed while the project owes its cold start (`Phase::Unindexed` or
/// `Phase::Walking`): the walk is running now or starts on the first tool
/// call, and the root tells a caller's g-mesh sessions apart. A root too long
/// for [`INSTRUCTIONS_BYTE_CEILING`] falls back to [`cold_start_line_fallback`],
/// so this line is never what breaks the ceiling. A warm rendering never says
/// the wait: an upgrade wipes the index, so a walk owed after one is a cold
/// start too (ADR 0022, section 1, row 10).
fn cold_start_line(root: &Path, walking: bool) -> String {
    format!("Index root: {}. {}", root.display(), cold_start_line_fallback(walking))
}

/// [`cold_start_line`] without the root (D12 in
/// `docs/architecture/lazy-indexing.md`).
fn cold_start_line_fallback(walking: bool) -> String {
    let state = if walking {
        "Being built now - the first tool call waits for it to finish before answering"
    } else {
        "Not indexed yet - the first tool call builds it (structural first; semantic search after) and \
         waits for it"
    };
    format!("{state}{WAIT_IS_NOT_WRONG}")
}

/// [`cold_start_line`] (or its no-path fallback) on top of the [`build`]
/// rendering for `coverage`, which must need no I/O: during the cold start
/// `mcp::mod::GMeshMcpServer::instructions` never takes the connection lock,
/// because the walk or its batch commit may hold it through embedding
/// inference, and a caller mid-handshake must not wait on it.
///
/// The body is built against the ceiling less the no-path line, so that line
/// always fits on top of it.
pub fn cold_start(root: &Path, walking: bool, coverage: &Coverage) -> String {
    let fallback = cold_start_line_fallback(walking);
    let built = build_within(coverage, INSTRUCTIONS_BYTE_CEILING - fallback.len() - 2);
    let with_path = format!("{}\n\n{built}", cold_start_line(root, walking));
    if with_path.len() <= INSTRUCTIONS_BYTE_CEILING {
        with_path
    } else {
        format!("{fallback}\n\n{built}")
    }
}

/// One language with answers (or, at a cold start, with a plugin that will
/// give them), with its manifest capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentLanguage {
    /// The manifest's own `language` id, e.g. `"typescript"`, rendered as-is.
    pub language: String,
    /// This language's `[plugin.capabilities]`, or the conservative
    /// [`Capabilities::default`] when its plugin was since removed.
    pub capabilities: Capabilities,
}

/// What the coverage paragraph says about the languages that have answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Covered {
    /// What the index holds (a warm session).
    Indexed(Vec<PresentLanguage>),
    /// What plugins are installed, when the index cannot be read yet (a cold
    /// start) or its read failed.
    Installed(Vec<PresentLanguage>),
}

/// What the coverage paragraph says about the languages with no answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Uncovered {
    /// The last walk's recorded outcomes (ADR 0021, section 5).
    Recorded {
        /// `PluginAbsent` languages, with their file count when it was taken.
        absent: Vec<(String, Option<usize>)>,
        /// `Failed` languages, with their full error chain.
        failed: Vec<(String, String)>,
    },
    /// Catalogue languages with no plugin, said conditionally ("if this
    /// project has ... files"): no outcome says which have files, and reading
    /// the files would be I/O at `initialize`.
    Missing(Vec<String>),
    /// Nothing to say: no recorded outcome is absent or failed, and no
    /// catalogue language is missing.
    #[default]
    Nothing,
}

/// Everything the instructions say about which languages g-mesh answers for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub covered: Covered,
    pub uncovered: Uncovered,
}

impl Coverage {
    /// A warm session's coverage: `indexed` from the index, absent and failed
    /// languages from the recorded `outcomes`, never re-derived. With no
    /// recorded outcome at all (an index from before outcomes were recorded),
    /// falls back to the conditional `missing` wording.
    pub fn from_outcomes(
        indexed: Vec<PresentLanguage>,
        outcomes: Vec<(String, LanguageOutcome)>,
        missing: Vec<String>,
    ) -> Self {
        if outcomes.is_empty() {
            return Self { covered: Covered::Indexed(indexed), uncovered: Uncovered::missing(missing) };
        }
        let mut absent = Vec::new();
        let mut failed = Vec::new();
        for (language, outcome) in outcomes {
            match outcome {
                LanguageOutcome::Indexed { .. } => {}
                LanguageOutcome::PluginAbsent { files } => absent.push((language, files)),
                LanguageOutcome::Failed { error } => failed.push((language, error)),
            }
        }
        let uncovered = if absent.is_empty() && failed.is_empty() {
            Uncovered::Nothing
        } else {
            Uncovered::Recorded { absent, failed }
        };
        Self { covered: Covered::Indexed(indexed), uncovered }
    }
}

impl Uncovered {
    /// [`Uncovered::Missing`], or [`Uncovered::Nothing`] when no language is.
    pub fn missing(missing: Vec<String>) -> Self {
        if missing.is_empty() {
            Self::Nothing
        } else {
            Self::Missing(missing)
        }
    }
}

/// How a language's receiver calls (`x.foo()`) reach the index, from its
/// manifest capabilities alone (ADR 0022, section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverClass {
    /// The structural tier resolves them: no gap.
    Static,
    /// Only a semantic pass resolves them: an edge exists once it has run.
    PassDependent,
    /// No tier resolves them, or none that can deliver them.
    Never,
}

fn receiver_class(capabilities: &Capabilities) -> ReceiverClass {
    if capabilities.receiver_calls_structural == ReceiverCallResolution::Resolved {
        ReceiverClass::Static
    } else if capabilities.receiver_calls == ReceiverCallResolution::Resolved && capabilities.semantic_pass {
        ReceiverClass::PassDependent
    } else {
        ReceiverClass::Never
    }
}

/// Renders `names` as an English list: `"go"`, `"go and rust"`, `"go, rust
/// and typescript"` (no Oxford comma: a byte saved per rendering).
fn format_language_list(names: &[String]) -> String {
    format_list(names, "and")
}

/// [`format_language_list`] with `conjunction` in place of "and".
fn format_list(names: &[String], conjunction: &str) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} {conjunction} {second}"),
        _ => {
            let (last, rest) = names.split_last().expect("non-empty per the two arms above");
            format!("{} {conjunction} {last}", rest.join(", "))
        }
    }
}

const P1: &str = "Structural code-graph queries over this project's index. Prefer these over \
     grepping when you need definitions, references, call edges or imports.";

const P2: &str = "A result anchored by `symbol_id`, or by an unambiguous `symbol_name` \
     (excludes other same-named declarations' call sites, same guarantee either \
     way), is already resolved per call site to that exact declaration - do not \
     re-check it with grep as a routine habit.";

/// [`P2`]'s last sentence, rendered only with a receiver paragraph: it points
/// at that paragraph.
const P2_GAP: &str =
    "Only fall back to grep for the one specific gap below, never as a general double-check.";

/// The "unsupported" state is this sentence's second half, said once and
/// generically: it names no catalogue.
fn head_indexed(list: &str) -> String {
    format!("Indexed here: {list}. g-mesh has no answers about files in any other language.")
}

const HEAD_NONE: &str = "Nothing is indexed here: g-mesh has no answers about this project's files.";

/// Ladder step 4's replacement for the indexed or installed list.
const HEAD_NO_LIST: &str = "g-mesh has no answers about files in a language it has not indexed.";

fn head_installed(list: &str) -> String {
    format!("Plugins installed: {list}. g-mesh has no answers about files in any other language.")
}

const HEAD_INSTALLED_NONE: &str =
    "No language plugin is installed: g-mesh has no answers about this project's files.";

const TRAILER: &str = "Until then an empty answer about those files is not evidence of absence.";

/// The longest a failed language's error may be, cut on a char boundary. It
/// is the first thing the ceiling cuts.
const ERROR_BYTES: usize = 100;

/// What a failed language's item shows of its stored error: the innermost
/// cause, with filesystem paths shortened to their file name, at most
/// [`ERROR_BYTES`] bytes. The store keeps the whole anyhow chain one cause
/// per line, outermost first (`languages::failed_error`), so the innermost
/// cause is the last non-empty line, whole even when its own text contains
/// ": ". The outer contexts name the step and the inner cause names what went
/// wrong; with 100 bytes, the cause is the part worth keeping.
pub(super) fn error_cause(error: &str) -> String {
    let cause = error.lines().rev().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    let cause = shorten_paths(cause);
    if cause.len() <= ERROR_BYTES {
        return cause;
    }
    let mut end = ERROR_BYTES - "...".len();
    while !cause.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &cause[..end])
}

/// Replaces each space-separated token that is an absolute (`/...`,
/// Windows `C:\...` or `C:/...`, UNC `\\server\...`) or home-relative
/// (`~/...`) path by its last component, keeping the punctuation around it:
/// `(/private/tmp/x/plugin.js)` becomes `(plugin.js)`. Anything else is left
/// as it is. Windows forms are recognised on every host, so a stored error
/// from either OS shortens the same way and the Windows arm is testable
/// anywhere.
fn shorten_paths(text: &str) -> String {
    const OPEN: &[char] = &['(', '[', '"', '\'', '`'];
    const CLOSE: &[char] = &[')', ']', '"', '\'', '`', ',', ';', ':', '.'];
    text.split(' ')
        .map(|token| {
            let start = token.len() - token.trim_start_matches(OPEN).len();
            let end = token.trim_end_matches(CLOSE).len().max(start);
            let path = &token[start..end];
            let name = match windows_rooted(path) {
                Some(rest) => rest.rsplit(['\\', '/']).find(|part| !part.is_empty()),
                None => path
                    .strip_prefix('/')
                    .or_else(|| path.strip_prefix("~/"))
                    .and_then(|rest| rest.rsplit('/').find(|part| !part.is_empty())),
            };
            match name {
                Some(name) => format!("{}{name}{}", &token[..start], &token[end..]),
                None => token.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The part of `path` after a Windows root - a drive (`C:\`, `C:/`) or a
/// UNC/extended-length prefix (`\\`) - or `None` when `path` has neither.
fn windows_rooted(path: &str) -> Option<&str> {
    if let Some(rest) = path.strip_prefix("\\\\") {
        return Some(rest);
    }
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic() => Some(&path[3..]),
        _ => None,
    }
}

fn install_command(language: &str) -> String {
    format!("`g-mesh plugins install {language}`")
}

/// The coverage paragraph (ADR 0022, section 3). `with_errors`: ladder
/// step 1 names each failed language's error. `with_list`: ladder steps
/// 1-3 name the covered languages.
fn coverage_paragraph(coverage: &Coverage, with_errors: bool, with_list: bool) -> String {
    let (languages, indexed) = match &coverage.covered {
        Covered::Indexed(languages) => (languages, true),
        Covered::Installed(languages) => (languages, false),
    };
    let mut names: Vec<String> = languages.iter().map(|p| p.language.clone()).collect();
    names.sort();
    let mut sentences = vec![match (names.is_empty(), with_list, indexed) {
        (true, _, true) => HEAD_NONE.to_string(),
        (true, _, false) => HEAD_INSTALLED_NONE.to_string(),
        (false, false, _) => HEAD_NO_LIST.to_string(),
        (false, true, true) => head_indexed(&format_language_list(&names)),
        (false, true, false) => head_installed(&format_language_list(&names)),
    }];
    match &coverage.uncovered {
        Uncovered::Nothing => {}
        Uncovered::Missing(missing) => {
            let commands: Vec<String> = missing.iter().map(|language| install_command(language)).collect();
            sentences.push(format!(
                "If this project has {} files, they are not indexed: no plugin installed ({}).",
                format_list(missing, "or"),
                commands.join(", ")
            ));
        }
        Uncovered::Recorded { absent, failed } => {
            if !absent.is_empty() {
                let items: Vec<String> = absent
                    .iter()
                    .map(|(language, files)| match files {
                        Some(n) => format!("{language} ({n} files; {})", install_command(language)),
                        None => format!("{language} (files not counted; {})", install_command(language)),
                    })
                    .collect();
                sentences.push(format!("Not indexed, no plugin installed: {}.", items.join(", ")));
            }
            if !failed.is_empty() {
                let items: Vec<String> = failed
                    .iter()
                    .map(|(language, error)| {
                        if with_errors {
                            format!("{language} ({})", error_cause(error))
                        } else {
                            language.clone()
                        }
                    })
                    .collect();
                sentences.push(format!(
                    "Not indexed, plugin failed: {} - fix the plugin, then run `g-mesh reindex`.",
                    items.join(", ")
                ));
            }
            sentences.push(TRAILER.to_string());
        }
    }
    sentences.join(" ")
}

/// Receiver calls in the named languages may never produce an edge. Only
/// bare/this/super/qualified-type calls are promised exhaustive, which is
/// true in every language without per-language syntax in core.
fn p4_perm(list: &str) -> String {
    format!(
        "The one legitimate reason to grep afterward: in {list}, a method call through a variable \
         receiver (`x.foo()`) may produce no edge, so a method's caller/reference list there can \
         under-report; bare function calls and this/super/qualified-type calls have no such gap, and \
         for those `hasMore: false` without `unlinkedUsages` is exhaustive."
    )
}

/// Ladder step 3's [`p4_perm`]: names no language, because at that step the
/// list of languages is what was cut to fit the budget.
const P4_PERM_FALLBACK: &str = "The one legitimate reason to grep afterward: in some of this project's \
     languages a method call through a variable receiver (`x.foo()`) may produce no edge, so a \
     method's caller/reference list can under-report; bare function calls and this/super/qualified-type \
     calls have no such gap, and for those `hasMore: false` without `unlinkedUsages` is exhaustive.";

/// No covered language is [`ReceiverClass::Never`], but one reports no
/// `overrides` field ([`MemberOverrides::None`]): receiver calls bind to the
/// declared or inferred type, so an override's caller page under-reports,
/// and the missing calls sit on the base's page. It must never pass an
/// implementor count off as missing callers (ADR 0003).
const P4_STATIC: &str = "The one legitimate reason to grep afterward: a method call through a variable \
     receiver (`x.foo()`) binds to the receiver's declared or inferred type, not the one it holds at run \
     time, so an override's caller page under-reports - calls reaching it through a base or interface \
     sit on that base's page, and find_implementations is the way across.";

/// Appended to [`p4_perm`], [`P4_PERM_FALLBACK`] or [`P4_STATIC`] when any
/// covered language is [`ReceiverClass::PassDependent`]; "such a call" is the
/// receiver call that paragraph names. True at every moment of a session, so
/// it cannot go stale; the live state is `provenance` on the answer.
const S_PASS: &str = "Until a language's semantic pass has run, such a call has no edge at all there; \
     a page answered before then carries `provenance`.";

/// [`S_PASS`] standing alone: no language is [`ReceiverClass::Never`] and
/// every one reports `overrides`, so nothing precedes it.
const S_PASS_ALONE: &str = "The one legitimate reason to grep afterward: until a language's semantic \
     pass has run, a method call through a variable receiver (`x.foo()`) has no edge at all there; a \
     page answered before then carries `provenance`.";

const P5: &str = "Efficient usage: pass `symbol_name` directly to \
     find_references/find_callers/find_callees/find_implementations instead of calling find_definition \
     first, and raise `limit` for symbols with many results instead of paging.";

/// The receiver paragraph for `languages`, or `None` when there is no gap to
/// name: no language, or every one [`ReceiverClass::Static`] and reporting
/// `overrides`. `with_list`: name the [`ReceiverClass::Never`] languages.
fn receiver_paragraph(languages: &[PresentLanguage], with_list: bool) -> Option<String> {
    let mut never: Vec<String> = languages
        .iter()
        .filter(|p| receiver_class(&p.capabilities) == ReceiverClass::Never)
        .map(|p| p.language.clone())
        .collect();
    never.sort();
    let pass_dependent =
        languages.iter().any(|p| receiver_class(&p.capabilities) == ReceiverClass::PassDependent);
    let silent_on_overrides =
        languages.iter().any(|p| p.capabilities.member_overrides == MemberOverrides::None);
    let mut paragraph = match (never.is_empty(), with_list) {
        (false, true) => p4_perm(&format_language_list(&never)),
        (false, false) => P4_PERM_FALLBACK.to_string(),
        (true, _) if silent_on_overrides => P4_STATIC.to_string(),
        (true, _) if pass_dependent => return Some(S_PASS_ALONE.to_string()),
        (true, _) => return None,
    };
    if pass_dependent {
        paragraph.push(' ');
        paragraph.push_str(S_PASS);
    }
    Some(paragraph)
}

/// One rung of the trim ladder (ADR 0022, section 4): `step` 1 is the full
/// text, each later step drops one more thing.
fn render(coverage: &Coverage, step: u8) -> String {
    let languages = match &coverage.covered {
        Covered::Indexed(languages) | Covered::Installed(languages) => languages,
    };
    let receiver = receiver_paragraph(languages, step < 3);
    let p2 = match receiver {
        Some(_) => format!("{P2} {P2_GAP}"),
        None => P2.to_string(),
    };
    let mut paragraphs = vec![P1.to_string(), coverage_paragraph(coverage, step < 2, step < 4), p2];
    paragraphs.extend(receiver);
    paragraphs.push(P5.to_string());
    paragraphs.join("\n\n")
}

/// Builds `get_info`'s `with_instructions` string for this session: P1, the
/// coverage paragraph, P2 (with [`P2_GAP`] only before a receiver
/// paragraph), the receiver paragraph (left out when there is no gap to name,
/// [`receiver_paragraph`]), P5.
pub fn build(coverage: &Coverage) -> String {
    build_within(coverage, INSTRUCTIONS_BYTE_CEILING)
}

/// [`build`], taking the first ladder step that fits `budget`:
/// 1. the full text;
/// 2. failed languages' errors dropped, their names kept;
/// 3. the never-resolving list replaced by [`P4_PERM_FALLBACK`];
/// 4. the covered list replaced by [`HEAD_NO_LIST`].
///
/// Absent and failed names and install commands are never dropped, so a
/// rendering that still does not fit at step 4 is returned as is.
fn build_within(coverage: &Coverage, budget: usize) -> String {
    let mut rendered = render(coverage, 1);
    for step in 2..=4 {
        if rendered.len() <= budget {
            break;
        }
        rendered = render(coverage, step);
    }
    rendered
}

/// Pairs each present language with the registry's capabilities; a language
/// with no manifest gets [`Capabilities::default`].
pub fn present_languages(
    present: impl IntoIterator<Item = String>,
    capabilities: &HashMap<String, Capabilities>,
) -> Vec<PresentLanguage> {
    present
        .into_iter()
        .map(|language| {
            let capabilities = capabilities.get(&language).copied().unwrap_or_default();
            PresentLanguage { language, capabilities }
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
