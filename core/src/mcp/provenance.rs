//! Per-response tier provenance (GM-382): which language plugin answered a
//! query, and - when that plugin's semantic tier did not contribute - the
//! fact that it did not.
//!
//! # The defect this closes
//!
//! The four bundled plugins have honestly different capabilities.
//! TypeScript carries a real type checker. Go uses `go/types`. Rust reaches
//! `rust-analyzer` *if it is installed*; Python reaches `pyright` *if it is
//! installed*. A plugin whose semantic tier is absent still answers -
//! structurally - and until this module existed the response was shaped
//! identically either way: `find_callers` on a Rust receiver call with
//! `rust-analyzer` on `PATH`, and the same call without it, returned the
//! same field set, the same `resolved: true` rows, and meant different
//! things. Nothing in the response said which tier had produced it, and the
//! shipped guidance (`mcp::instructions`' paragraph 2, and the `AGENTS.md`
//! snippet `cli::agent_instructions` installs) tells a caller not to
//! re-verify a page that looks complete.
//!
//! That is the root under a family of defects, not one defect: GM-356,
//! GM-358, GM-360, GM-361 and GM-362 were each the same sentence - a
//! guarantee true in TypeScript and false somewhere else, asserted by a
//! response that never said which language or tier it was measured on. Both
//! benchmark corpora were TypeScript until 2026-09-16, which is why the
//! class went unnoticed rather than why it was rare. Fixing instances does
//! not stop the next instance; saying which tier answered does, because the
//! guarantee stops being stated unconditionally.
//!
//! # What this says, and the much larger thing it refuses to say
//!
//! [`Provenance`] carries two facts and no third: the language that
//! answered, and that its semantic tier is absent from this answer. It does
//! **not** estimate what the missing tier would have found.
//!
//! That refusal is the whole design, so it is worth stating why rather than
//! leaving it as an omission somebody helpfully corrects later. "The
//! semantic tier was unavailable" is knowable and cheap. "...and it would
//! have found three more callers" is not knowable: a plugin with no semantic
//! engine has, by construction, not run the analysis that would have found
//! them. The tempting near-miss is that a count *is* available - a
//! structural tier records open receiver-call sites it could not resolve
//! (Go's `go/parser` tier does exactly this, GM-281) - so a response could
//! honestly report "41 unresolved call sites in this project". It must not,
//! because that number answers a different question than the one it will be
//! read as answering. Those 41 sites are calls whose *target* is unknown;
//! they are an upper bound on the whole project, not on this anchor. A
//! caller who reads `find_callers(Square::perimeter)` and sees `41` will act
//! on "41 more callers of `perimeter`", which is a number nothing computed.
//! A manufactured number a caller acts on is strictly worse than silence,
//! because silence is visibly silence. So: the tier is named absent, and
//! what it would have found is left unsaid.
//!
//! `untypedReceiverCalls` (`super::untyped`) does not break this refusal: it
//! is not a project-wide count. It counts only functions that call a method
//! of the anchor's bare name through an untyped receiver and have no edge to
//! the anchor yet, and it says those calls *may* reach it. This block stays
//! as it is.
//!
//! # Why the disclosure is conditional, and scoped to four tools
//!
//! A disclosure that fires everywhere is noise, and noise is how a real
//! signal stops being read. Two rules keep this one rare enough to mean
//! something:
//!
//! 1. **Only the four edge-walking tools carry it.** The semantic tier's
//!    contribution to this index is *edges* - receiver calls resolved to a
//!    callee, `SUPERTYPE_OF` edges for trait impls, cross-crate targets. It
//!    is not declarations and not import specifiers. So `get_file_outline`,
//!    `find_definition`, `get_dependencies` and `search_code` answer exactly
//!    as well without it, and must stay silent: `get_file_outline` on a Go
//!    file does not become less trustworthy because `pyright` is missing.
//!    `find_references`, `find_callers`, `find_callees` and
//!    `find_implementations` are the four whose completeness the missing
//!    tier actually changes, and the four the defect family above landed on.
//! 2. **Only when a tier that would have mattered is actually absent.** A
//!    language whose semantic pass has completed says nothing - that is the
//!    healthy case and it is nearly every case, so paying bytes for it on
//!    every response would spend the budget where there is nothing to
//!    report. A language whose plugin declares no semantic tier at all
//!    ([`Capabilities::semantic_pass`] `= false`) also says nothing: that is
//!    a permanent property of the plugin, `mcp::instructions` already states
//!    it once per session, and repeating a session-level constant on every
//!    response is the definition of noise.
//!
//! [`resolve`] is where both rules live, and the second one is why it
//! returns an `Option`.
//!
//! # Anchor language, not row languages
//!
//! The language named is the **anchor's** (`nodes.language` on the node the
//! query resolved to), not a union computed over the rows. That is not the
//! cheaper choice standing in for the better one; it is the correct one, and
//! for a reason worth keeping written down:
//!
//! - A union over rows is computed from the rows that *exist*. The rows this
//!   module exists to warn about are the ones that do not - the receiver
//!   calls the absent tier never resolved. A Rust page that lost every one
//!   of its rows to a missing `rust-analyzer` would, under row-union
//!   scoping, name no language at all and disclose nothing, which is exactly
//!   backwards.
//! - The anchor is always present (a page with no anchor is not a page this
//!   module annotates), so the disclosure cannot go missing with the rows.
//! - It costs nothing: `graph::queries::map_node_row` already reads
//!   `nodes.language` into `NodeRecord.language` on the `SELECT *` every one
//!   of these handlers already runs to resolve its anchor.
//!
//! For the four tools here the distinction is almost always academic anyway,
//! since a `CALLS` or `SUPERTYPE_OF` edge joins two declarations in the same
//! language. But "almost always" is the kind of premise this codebase has
//! been bitten by, so the rule is stated by what it guarantees rather than
//! by what usually happens.
//!
//! # Why not `edges.source`/`edges.engine`, which look like the obvious answer
//!
//! Every edge in the index already carries real tier provenance:
//! `edges.source` is `'syntactic' | 'semantic'` and `edges.engine` is the
//! engine's own label (`"tree-sitter"`, `"rust-analyzer"`, `"pyright"`,
//! `"go-types"`). Both are loaded into `storage::write::EdgeRecord` by
//! `graph::queries::map_edge_row` and carried through
//! `graph::pagination::ScoredEdge` to every row-building site in these four
//! tools - they are in scope, for free, one field away from the wire.
//!
//! They are still the wrong instrument, and it is worth being explicit
//! because they are what the next person will reach for. A per-row
//! `source: "syntactic"` describes the edge that *is* there. The question
//! this module answers is about the edges that are *not* there, and no
//! property of a returned row can answer it. Annotating rows with their tier
//! would also repeat a property of the whole call once per row: measured on
//! excalidraw's `pointFrom` at `limit: 200` - 51 rows, the established worst
//! case - a per-row tier annotation costs 51 copies of a fact that is true
//! of the call, against one copy for the block below. The per-row fields
//! remain genuinely useful for a *different* future question ("which engine
//! resolved this particular edge"), and this module deliberately does not
//! spend them on this one.
//!
//! # Pending: a semantic pass catching up after a workspace reindex
//!
//! A workspace reindex (ADR 0008) swaps a language in and then runs its
//! whole-project semantic pass against live. Until that pass completes, the
//! files the swap changed have structural edges only, and unchanged call
//! sites may still point where the old dependency graph resolved them. That
//! is not `absent` - the tier is running, and every unchanged node's
//! semantic edges are still served - so it gets its own value,
//! [`SemanticTier::Pending`], with two facts beside it: `since` (when the
//! swap made the pass owed) and `pendingFiles` (which of the files this
//! response names the pass has not reached). Design:
//! [ADR 0009](../../../docs/adr/0009-semantic-pending.md).
//!
//! The pending state lives in `semantic_pending`/`semantic_pending_files`,
//! written by the swap and cleared by any recorded outcome of the pass. A
//! failed, incomplete or not-run pass clears them, and the language falls
//! back to `absent`: no pass is working on those files any more. The list is
//! bounded by the response's own files and by [`MAX_PENDING_FILES`];
//! `pendingFilesOmitted` is an exact count of the response's pending files
//! the cap left out, never an estimate of what the pass would change.
//!
//! # Cost
//!
//! One object, on four tools, only in the degraded case:
//! `,"provenance":{"language":"rust","semanticTier":"absent"}` - 56 bytes at
//! its widest for a bundled language. It is bounded (a language id plus a
//! closed enum), unlike a tally, which is why it needs no
//! `graph::pagination` byte reserve the way `files` and `excludedReferences`
//! do: `hint` is the precedent - an optional response-level field, larger
//! than this one, carried without a reserve of its own. `pending` carries a
//! file list, so a pending response holds back [`PENDING_FILES_RESERVE`]
//! bytes from its page budget ([`Resolved::page_reserve`]); a response that
//! is not pending reserves nothing and pages exactly as before.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

use crate::daemon::manifest::Capabilities;
use crate::storage::schema;

/// At most this many files in `pendingFiles`; the rest are counted in
/// `pendingFilesOmitted`.
pub(super) const MAX_PENDING_FILES: usize = 25;

/// Bytes a pending response holds back from its page budget for the
/// `provenance` block: [`MAX_PENDING_FILES`] paths at ~60 bytes each.
pub(super) const PENDING_FILES_RESERVE: usize = 1_500;

/// Which language plugin answered, and what its semantic tier contributed.
///
/// Serialized as the response-level `provenance` object on the four
/// edge-walking tools, and **only** when there is something to disclose -
/// see [`resolve`]. Response-level rather than per-row for the reason
/// `find_callers_callees::ExcludedReferences` and `all_unresolved` are:
/// it is a property of the call, and a property of the call repeated once
/// per row is both wrong in shape and paid for per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Provenance {
    /// The manifest's own `language` id (`"rust"`, `"python"`, ...), spelled
    /// exactly as `nodes.language` and `daemon::manifest` spell it - the
    /// same rule `mcp::instructions` follows and for the same reason: an
    /// agent cross-referencing the two must never meet two spellings of one
    /// language.
    pub(super) language: String,
    /// What this language's semantic tier contributed to this answer.
    pub(super) semantic_tier: SemanticTier,
    /// With `pending` only: when the reindex swap made the pass owed, RFC
    /// 3339 UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) since: Option<String>,
    /// With `pending` only: the files this response names whose edges the
    /// pass has not refreshed yet - the anchor's file first when it is one,
    /// the rest sorted. Absent, never `[]`, when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) pending_files: Vec<String>,
    /// With `pending` only, and only when [`MAX_PENDING_FILES`] cut the list:
    /// how many more of this response's files are pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pending_files_omitted: Option<usize>,
}

impl Provenance {
    fn absent(language: &str) -> Self {
        Provenance {
            language: language.to_string(),
            semantic_tier: SemanticTier::Absent,
            since: None,
            pending_files: Vec::new(),
            pending_files_omitted: None,
        }
    }
}

/// What a language's semantic tier contributed to one response.
///
/// A closed set rather than a `bool`, so each state a caller acts on
/// differently has its own value: install the engine (`absent`) versus ask
/// again in a moment (`pending`). `absent` still does not say *why* the tier
/// is missing - never scheduled, not run, or answered `incomplete` - because
/// `language_state` does not persist the difference; the plugin logs the
/// reason and `g-mesh status` shows the recorded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum SemanticTier {
    /// This language's plugin declares a semantic tier, and it has not
    /// completed for this project - so this answer came from the structural
    /// tier alone. Says nothing about what the semantic tier would have
    /// added, on purpose: see this module's doc comment.
    Absent,
    /// This language's whole-project semantic pass is owed after a workspace
    /// reindex and has neither completed nor failed. Unchanged nodes keep
    /// their semantic edges; the files the reindex changed have structural
    /// edges until the pass reaches them.
    Pending,
}

/// What [`resolve`] found for one language, before the response's own files
/// are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolved {
    /// Nothing to disclose: no semantic tier declared, or its pass is done.
    Silent,
    /// The declared semantic tier has not completed and no pass is pending.
    Absent,
    /// A pass is owed after a reindex swapped the language in at `since`.
    Pending { since: String },
}

/// The provenance state of an answer anchored in `language`, in this order:
/// no semantic tier declared -> [`Resolved::Silent`]; pass done ->
/// [`Resolved::Silent`] (so a stale pending row can never speak); a
/// `semantic_pending` row -> [`Resolved::Pending`]; otherwise
/// [`Resolved::Absent`].
///
/// `Silent` - the common case, and the case that costs zero bytes - covers
/// both halves of this module's second rule:
///
/// - the language's semantic pass has completed, so the answer had the
///   plugin's best tier behind it and a disclosure would be false; or
/// - the language's plugin declares no semantic tier at all
///   ([`Capabilities::semantic_pass`] `= false`, which is also the default
///   for a language present in the index whose plugin has since been
///   removed, by the same conservative "says nothing, assumed to do the
///   least" rule `mcp::instructions::present_languages` applies). Nothing is
///   absent that was ever going to be there, and `mcp::instructions` already
///   says so once per session.
///
/// A failed read of `language_state` also yields `Silent` rather than an
/// error, and a failed read of `semantic_pending` yields `Absent`. The caller
/// asked a structural question the index could answer, and refusing it
/// because a *footnote* could not be computed would turn an unreadable row
/// into a dead tool surface - the same rule
/// `find_callers_callees::excluded_references` already applies to its own
/// best-effort tally. The direction of that failure is the safe one in only
/// one sense and it is worth being honest about which: it under-warns rather
/// than over-warns. It is accepted because a `SELECT` against a small table
/// on a connection the handler is already holding does not fail for reasons
/// that leave the rest of the response trustworthy.
pub(super) fn resolve(
    conn: &Connection,
    capabilities: &HashMap<String, Capabilities>,
    language: &str,
) -> Resolved {
    if !capabilities.get(language).copied().unwrap_or_default().semantic_pass {
        return Resolved::Silent;
    }
    match schema::language_semantic_pass_done(conn, language) {
        Ok(false) => {}
        Ok(true) | Err(_) => return Resolved::Silent,
    }
    match schema::semantic_pending_since(conn, language) {
        Ok(Some(since)) => Resolved::Pending { since },
        Ok(None) | Err(_) => Resolved::Absent,
    }
}

impl Resolved {
    /// Bytes the response's page must hold back for this block: only a
    /// pending block carries a variable-length list.
    pub(super) fn page_reserve(&self) -> usize {
        match self {
            Resolved::Pending { .. } => PENDING_FILES_RESERVE,
            Resolved::Silent | Resolved::Absent => 0,
        }
    }

    /// The `provenance` block for a response in `language` naming `touched`
    /// files, `anchor_file` among them first when the response has an anchor.
    /// A failed read of the pending files degrades to `absent`.
    pub(super) fn disclose<'a>(
        self,
        conn: &Connection,
        language: &str,
        anchor_file: Option<&'a str>,
        touched: impl IntoIterator<Item = &'a str>,
    ) -> Option<Provenance> {
        let since = match self {
            Resolved::Silent => return None,
            Resolved::Absent => return Some(Provenance::absent(language)),
            Resolved::Pending { since } => since,
        };
        let mut files: Vec<&str> = anchor_file.into_iter().chain(touched).collect();
        files.sort_unstable();
        files.dedup();
        let Ok(mut pending) = schema::semantic_pending_files_among(conn, language, &files) else {
            return Some(Provenance::absent(language));
        };
        pending.sort_unstable();
        if let Some(anchor_file) = anchor_file {
            if let Some(at) = pending.iter().position(|path| path == anchor_file) {
                let anchor = pending.remove(at);
                pending.insert(0, anchor);
            }
        }
        let omitted = pending.len().saturating_sub(MAX_PENDING_FILES);
        pending.truncate(MAX_PENDING_FILES);
        Some(Provenance {
            language: language.to_string(),
            semantic_tier: SemanticTier::Pending,
            since: Some(since),
            pending_files: pending,
            pending_files_omitted: (omitted > 0).then_some(omitted),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::manifest::ReceiverCallResolution;

    /// A capability map naming one language with a declared semantic tier.
    fn with_semantic_tier(language: &str) -> HashMap<String, Capabilities> {
        HashMap::from([(
            language.to_string(),
            Capabilities {
                semantic_pass: true,
                semantic_sweep: false,
                semantic_prepare: false,
                receiver_calls: ReceiverCallResolution::Resolved,
                receiver_calls_structural: ReceiverCallResolution::Unresolved,
            },
        )])
    }

    /// The same language, by a plugin that declares no semantic tier - the
    /// shape [`resolve`] must stay silent about however the index looks.
    fn without_semantic_tier(language: &str) -> HashMap<String, Capabilities> {
        HashMap::from([(language.to_string(), Capabilities::default())])
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    /// `language` pending since `since`, with `files` not yet refreshed - the
    /// rows a workspace reindex swap writes.
    fn mark_pending(conn: &Connection, language: &str, since: &str, files: &[&str]) {
        conn.execute("INSERT INTO semantic_pending (language, since) VALUES (?1, ?2)", [language, since])
            .unwrap();
        for file in files {
            conn.execute(
                "INSERT INTO semantic_pending_files (language, filePath) VALUES (?1, ?2)",
                [language, file],
            )
            .unwrap();
        }
    }

    /// The whole disclosure for `language` on a response naming `touched`.
    fn disclosed(
        conn: &Connection,
        capabilities: &HashMap<String, Capabilities>,
        language: &str,
        anchor_file: Option<&str>,
        touched: &[&str],
    ) -> Option<Provenance> {
        resolve(conn, capabilities, language).disclose(conn, language, anchor_file, touched.iter().copied())
    }

    #[test]
    fn a_declared_semantic_tier_that_has_not_run_is_disclosed() {
        let conn = db();

        let provenance = disclosed(&conn, &with_semantic_tier("rust"), "rust", Some("a.rs"), &[]);

        assert_eq!(provenance, Some(Provenance::absent("rust")));
    }

    /// The control for the test above: the *only* thing that changed is that
    /// the pass completed, and the disclosure has to vanish for it. A block
    /// that appeared in both arms would be a permanent footnote, not a
    /// signal - and this is the assertion that tells the two arms apart.
    #[test]
    fn a_completed_semantic_pass_discloses_nothing() {
        let conn = db();
        schema::record_language_semantic_pass(&conn, "rust").unwrap();

        assert_eq!(resolve(&conn, &with_semantic_tier("rust"), "rust"), Resolved::Silent);
    }

    /// Rule 2's second half: a plugin with no semantic tier has nothing
    /// absent, so an index in exactly the state that *would* make a
    /// semantic-capable language disclose must still stay silent for this
    /// one. Same connection state as the disclosing test above; only the
    /// declared capability differs.
    #[test]
    fn a_plugin_declaring_no_semantic_tier_discloses_nothing() {
        let conn = db();

        assert_eq!(resolve(&conn, &without_semantic_tier("rust"), "rust"), Resolved::Silent);
    }

    /// A language present in the index whose plugin was since removed falls
    /// to `Capabilities::default()` - `semantic_pass: false` - and is
    /// therefore silent rather than disclosing a tier nothing declares.
    #[test]
    fn a_language_with_no_discovered_manifest_discloses_nothing() {
        let conn = db();

        assert_eq!(resolve(&conn, &HashMap::new(), "rust"), Resolved::Silent);
    }

    /// Each language's pass is its own fact: Go's having completed says
    /// nothing about Rust's, and a Rust-anchored answer must not be
    /// silenced by a *different* language's healthy tier. This is the
    /// polyglot half of the noise rule - the leak that would otherwise let
    /// one language's state speak for another's.
    #[test]
    fn one_languages_completed_pass_does_not_silence_another() {
        let conn = db();
        schema::record_language_semantic_pass(&conn, "go").unwrap();
        let capabilities: HashMap<String, Capabilities> =
            with_semantic_tier("rust").into_iter().chain(with_semantic_tier("go")).collect();

        assert_eq!(resolve(&conn, &capabilities, "go"), Resolved::Silent);
        assert_eq!(resolve(&conn, &capabilities, "rust"), Resolved::Absent);
    }

    /// `resolve`'s order, one arm per step. The stale-row arm is the one the
    /// order exists for: a pass recorded done must silence a pending row
    /// whose clear failed, and a language whose plugin declares no semantic
    /// tier must stay silent even with a row.
    #[test]
    fn resolve_checks_capability_then_completion_then_pending() {
        let conn = db();
        mark_pending(&conn, "rust", "2026-09-26T10:14:03Z", &[]);
        assert_eq!(
            resolve(&conn, &with_semantic_tier("rust"), "rust"),
            Resolved::Pending { since: "2026-09-26T10:14:03Z".to_string() }
        );
        assert_eq!(resolve(&conn, &without_semantic_tier("rust"), "rust"), Resolved::Silent);

        conn.execute(
            "INSERT INTO language_state (language, semanticPassAt) VALUES ('rust', CURRENT_TIMESTAMP)",
            [],
        )
        .unwrap();
        assert_eq!(
            resolve(&conn, &with_semantic_tier("rust"), "rust"),
            Resolved::Silent,
            "a done pass with a stale pending row discloses nothing"
        );

        assert_eq!(resolve(&conn, &with_semantic_tier("go"), "go"), Resolved::Absent);
    }

    /// A recorded failure (incomplete, not run) ends `pending`: the language
    /// reads `absent` again. Control: remove the clear from
    /// `schema::record_language_semantic_pass_failure` -> still `Pending`.
    #[test]
    fn a_recorded_failure_turns_pending_back_into_absent() {
        let conn = db();
        mark_pending(&conn, "rust", "2026-09-26T10:14:03Z", &["a.rs"]);

        schema::record_language_semantic_pass_failure(&conn, "rust", "incomplete").unwrap();

        assert_eq!(resolve(&conn, &with_semantic_tier("rust"), "rust"), Resolved::Absent);
    }

    /// Only the touched files that are pending are named, the anchor's file
    /// first, the rest sorted; a pending file the response does not name is
    /// not listed.
    #[test]
    fn a_pending_block_names_only_the_responses_pending_files_anchor_first() {
        let conn = db();
        mark_pending(&conn, "rust", "2026-09-26T10:14:03Z", &["z.rs", "a.rs", "m.rs", "elsewhere.rs"]);

        let provenance = disclosed(
            &conn,
            &with_semantic_tier("rust"),
            "rust",
            Some("z.rs"),
            &["m.rs", "c.rs", "a.rs", "m.rs"],
        )
        .unwrap();

        assert_eq!(provenance.semantic_tier, SemanticTier::Pending);
        assert_eq!(provenance.since.as_deref(), Some("2026-09-26T10:14:03Z"));
        assert_eq!(provenance.pending_files, vec!["z.rs", "a.rs", "m.rs"]);
        assert_eq!(provenance.pending_files_omitted, None);
    }

    /// More pending files than the cap: [`MAX_PENDING_FILES`] listed, the
    /// rest counted exactly.
    #[test]
    fn the_pending_file_list_is_capped_and_the_rest_counted() {
        let conn = db();
        let files: Vec<String> = (0..30).map(|i| format!("src/f{i:02}.rs")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        mark_pending(&conn, "rust", "2026-09-26T10:14:03Z", &files);

        let provenance = disclosed(&conn, &with_semantic_tier("rust"), "rust", None, &files).unwrap();

        assert_eq!(provenance.pending_files.len(), MAX_PENDING_FILES);
        assert_eq!(provenance.pending_files_omitted, Some(30 - MAX_PENDING_FILES));
    }

    /// The wire spelling, pinned here rather than only in the conformance
    /// fixtures: `camelCase` keys, lower-case enum values, `since` and the
    /// file fields only with `pending`, no empty list, and nothing resembling
    /// a count of what the tier would have found - this module's doc comment
    /// says why that field must never exist.
    #[test]
    fn the_serialized_shapes_are_exact() {
        let absent = Provenance::absent("python");
        assert_eq!(
            serde_json::to_string(&absent).unwrap(),
            r#"{"language":"python","semanticTier":"absent"}"#
        );

        let pending_without_files = Provenance {
            language: "rust".to_string(),
            semantic_tier: SemanticTier::Pending,
            since: Some("2026-09-26T10:14:03Z".to_string()),
            pending_files: Vec::new(),
            pending_files_omitted: None,
        };
        assert_eq!(
            serde_json::to_string(&pending_without_files).unwrap(),
            r#"{"language":"rust","semanticTier":"pending","since":"2026-09-26T10:14:03Z"}"#
        );

        let pending_with_files = Provenance {
            pending_files: vec!["core/src/a.rs".to_string(), "core/src/b.rs".to_string()],
            pending_files_omitted: Some(5),
            ..pending_without_files
        };
        assert_eq!(
            serde_json::to_string(&pending_with_files).unwrap(),
            r#"{"language":"rust","semanticTier":"pending","since":"2026-09-26T10:14:03Z","pendingFiles":["core/src/a.rs","core/src/b.rs"],"pendingFilesOmitted":5}"#
        );
    }
}
