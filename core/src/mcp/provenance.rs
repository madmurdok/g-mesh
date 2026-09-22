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
//! # Cost
//!
//! One object, on four tools, only in the degraded case:
//! `,"provenance":{"language":"rust","semanticTier":"absent"}` - 56 bytes at
//! its widest for a bundled language. It is bounded (a language id plus a
//! closed enum), unlike a tally, which is why it needs no
//! `graph::pagination` byte reserve the way `files` and `excludedReferences`
//! do: `hint` is the precedent - an optional response-level field, larger
//! than this one, carried without a reserve of its own.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

use crate::daemon::manifest::Capabilities;
use crate::storage::schema;

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
}

/// What a language's semantic tier contributed to one response.
///
/// One variant, deliberately. The daemon can distinguish "absent" from
/// "present" - that is [`resolve`]'s whole job - but it cannot distinguish
/// the three *reasons* a tier is absent, and this type refuses to imply
/// that it can. `language_state.semanticPassAt` is NULL identically for a
/// pass never scheduled, a pass still running, and a pass that answered
/// `incomplete: true` because the engine could not be started
/// (`plugins/sdk`'s `LazyEngine::answer`, and
/// `docs/architecture/multi-language-plugins.md`'s "Semantic engine
/// missing"). The plugin logs the real reason in words, where there is room
/// for it; the wire's `FileChangeResponse.incomplete` carries the bit but is
/// folded into "leave `semanticPassAt` unset" and not persisted.
///
/// So the enum says the one thing that is true of all three, and stays a
/// closed set rather than a bare `bool` so that a later task which *does*
/// persist the distinction (splitting this into `Unavailable` and `Pending`,
/// say, which are differently actionable - install the engine, versus retry
/// in a moment) can add variants without changing the field's type or its
/// meaning where it already appears. A `bool` would have had to be renamed
/// to grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum SemanticTier {
    /// This language's plugin declares a semantic tier, and it has not
    /// completed for this project - so this answer came from the structural
    /// tier alone. Says nothing about what the semantic tier would have
    /// added, on purpose: see this module's doc comment.
    Absent,
}

/// The `provenance` block for an answer anchored in `language`, or `None`
/// when there is nothing to disclose.
///
/// `None` - the common case, and the case that costs zero bytes - covers
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
/// A failed read of `language_state` also yields `None` rather than an
/// error. The caller asked a structural question the index could answer, and
/// refusing it because a *footnote* could not be computed would turn an
/// unreadable row into a dead tool surface - the same rule
/// `find_callers_callees::excluded_references` already applies to its own
/// best-effort tally. The direction of that failure is the safe one in only
/// one sense and it is worth being honest about which: a missing block reads
/// as "nothing to report", so a lost read under-warns rather than
/// over-warns. It is accepted because a `SELECT` against a four-column table
/// on a connection the handler is already holding does not fail for reasons
/// that leave the rest of the response trustworthy.
pub(super) fn resolve(
    conn: &Connection,
    capabilities: &HashMap<String, Capabilities>,
    language: &str,
) -> Option<Provenance> {
    if !capabilities.get(language).copied().unwrap_or_default().semantic_pass {
        return None;
    }
    if schema::language_semantic_pass_done(conn, language).ok()? {
        return None;
    }
    Some(Provenance { language: language.to_string(), semantic_tier: SemanticTier::Absent })
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

    #[test]
    fn a_declared_semantic_tier_that_has_not_run_is_disclosed() {
        let conn = db();

        let provenance = resolve(&conn, &with_semantic_tier("rust"), "rust");

        assert_eq!(
            provenance,
            Some(Provenance { language: "rust".to_string(), semantic_tier: SemanticTier::Absent })
        );
    }

    /// The control for the test above: the *only* thing that changed is that
    /// the pass completed, and the disclosure has to vanish for it. A block
    /// that appeared in both arms would be a permanent footnote, not a
    /// signal - and this is the assertion that tells the two arms apart.
    #[test]
    fn a_completed_semantic_pass_discloses_nothing() {
        let conn = db();
        schema::record_language_semantic_pass(&conn, "rust").unwrap();

        assert_eq!(resolve(&conn, &with_semantic_tier("rust"), "rust"), None);
    }

    /// Rule 2's second half: a plugin with no semantic tier has nothing
    /// absent, so an index in exactly the state that *would* make a
    /// semantic-capable language disclose must still stay silent for this
    /// one. Same connection state as the disclosing test above; only the
    /// declared capability differs.
    #[test]
    fn a_plugin_declaring_no_semantic_tier_discloses_nothing() {
        let conn = db();

        assert_eq!(resolve(&conn, &without_semantic_tier("rust"), "rust"), None);
    }

    /// A language present in the index whose plugin was since removed falls
    /// to `Capabilities::default()` - `semantic_pass: false` - and is
    /// therefore silent rather than disclosing a tier nothing declares.
    #[test]
    fn a_language_with_no_discovered_manifest_discloses_nothing() {
        let conn = db();

        assert_eq!(resolve(&conn, &HashMap::new(), "rust"), None);
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

        assert_eq!(resolve(&conn, &capabilities, "go"), None);
        assert_eq!(
            resolve(&conn, &capabilities, "rust"),
            Some(Provenance { language: "rust".to_string(), semantic_tier: SemanticTier::Absent })
        );
    }

    /// The wire spelling, pinned here rather than only in the conformance
    /// fixtures: `camelCase` key, lower-case enum value, and no third field.
    /// In particular nothing resembling a count of what the absent tier
    /// would have found - this module's doc comment says why that field must
    /// never exist.
    #[test]
    fn the_serialized_shape_is_two_fields_and_no_estimate() {
        let provenance = Provenance { language: "python".to_string(), semantic_tier: SemanticTier::Absent };

        let json = serde_json::to_string(&provenance).unwrap();

        assert_eq!(json, r#"{"language":"python","semanticTier":"absent"}"#);
    }
}
