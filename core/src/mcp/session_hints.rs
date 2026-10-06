//! One-sentence `hint`s that say how to read a response field, attached only
//! when the field's triggering value is in the response. Rare triggers carry
//! their sentence every time; frequent ones carry it once per MCP session,
//! on the first response that has the trigger, tracked by [`SessionHints`].
//! Placement and frequency per rule: `docs/architecture/gm-389-guidance-prefix.md`
//! section 3.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

/// A sentence delivered at most once per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HintKey {
    FileRow,
    FilesTally,
    WalkComplete,
    SearchHits,
    UnresolvedRow,
    SemanticTier,
}

/// The once-per-session sentences already sent on one connection. Clones
/// share one set; a fresh value (one per `GMeshMcpServer::new`, so one per
/// connection) starts empty.
#[derive(Clone, Default)]
pub(crate) struct SessionHints(Arc<Mutex<HashSet<HintKey>>>);

impl SessionHints {
    /// `sentence` when `trigger` holds and `key` has not been sent on this
    /// session yet, recording it as sent. A false `trigger` records nothing.
    pub(crate) fn once(&self, trigger: bool, key: HintKey, sentence: &'static str) -> Option<&'static str> {
        (trigger && self.0.lock().unwrap_or_else(PoisonError::into_inner).insert(key)).then_some(sentence)
    }

    /// What [`Self::once`] would return now, without recording anything: for
    /// measuring a candidate response before the one that is sent.
    pub(crate) fn peek(&self, trigger: bool, key: HintKey, sentence: &'static str) -> Option<&'static str> {
        (trigger && !self.0.lock().unwrap_or_else(PoisonError::into_inner).contains(&key)).then_some(sentence)
    }

    /// [`Self::once`] when `send`, otherwise [`Self::peek`].
    pub(crate) fn offer(
        &self,
        send: bool,
        trigger: bool,
        key: HintKey,
        sentence: &'static str,
    ) -> Option<&'static str> {
        if send {
            self.once(trigger, key, sentence)
        } else {
            self.peek(trigger, key, sentence)
        }
    }
}

/// The sentences present, in order, as one `hint` value; `None` when none is.
pub(crate) fn join(sentences: impl IntoIterator<Item = Option<&'static str>>) -> Option<String> {
    let present: Vec<&str> = sentences.into_iter().flatten().collect();
    (!present.is_empty()).then(|| present.join(" "))
}

/// `hint` with `sentence` appended, for a page whose `provenance` is only
/// known after its `hint` was first assembled.
pub(crate) fn append(hint: Option<String>, sentence: Option<&'static str>) -> Option<String> {
    match (hint, sentence) {
        (Some(hint), Some(sentence)) => Some(format!("{hint} {sentence}")),
        (hint, sentence) => hint.or_else(|| sentence.map(str::to_string)),
    }
}

pub(crate) const ALL_UNRESOLVED: &str =
    "allUnresolved: the linker confirmed none of these rows, so check each in its own file before \
     relying on it; the rest of the project needs no search.";

/// Moved out of the instructions (ADR 0022): it is only needed once a row
/// says `resolved: false`.
pub(crate) const UNRESOLVED_ROW: &str =
    "`resolved: false`: the linker could not confirm this cross-file edge (whether that file exports \
     the name); every same-file edge is `resolved: true`, never a reason to grep.";

/// Explains `mcp::provenance`'s field, which the instructions no longer
/// describe per language (ADR 0022, section 2).
pub(crate) const PROVENANCE: &str =
    "`provenance`: this language's semantic pass has not finished, so method calls through a variable \
     receiver may be missing here; ask again later or grep for them.";

pub(crate) const AMBIGUOUS: &str =
    "Several declarations have this name: re-query with the right candidate's `id` as `symbol_id`, \
     not its qualifiedName, and treat that answer as final without grepping to reconfirm it.";

/// `AMBIGUOUS` for a page whose candidates carry their source: every reading
/// is already answered, so the follow-up is needed only for a cut body.
pub(crate) const AMBIGUOUS_SOURCED: &str =
    "Several declarations have this name, each with its source; none is preferred. Pick by reading; \
     re-query an `id` as `symbol_id` only for a source with `omittedLines`.";

/// `AMBIGUOUS_SOURCED` for a page where some candidates could not be given
/// their source: those, like a cut body, need the follow-up.
pub(crate) const AMBIGUOUS_PARTLY_SOURCED: &str =
    "Several declarations have this name, some with their source; none is preferred. Re-query an `id` \
     as `symbol_id` for one without `source` or with `omittedLines`.";

pub(crate) const FILE_ROW: &str =
    "A `kind: File` row is a usage outside any tracked symbol, so the file itself is the answer; \
     don't grep it for the line.";

pub(crate) const FILES_TALLY: &str =
    "`files` covers the whole edge set, not just this page, so answer \"which files\" from it \
     instead of paging or deduplicating rows.";

pub(crate) const WALK_COMPLETE: &str =
    "`truncated: false`: this walk is complete for the depth asked (at `max_depth: 1`, every direct \
     importer or import), so don't re-derive it with grep.";

pub(crate) const WALK_COMPLETE_IMPORT_TYPE: &str =
    "`truncated: false`: this walk is complete for the depth asked (at `max_depth: 1`, every direct \
     importer or import, `import type` included), so don't re-derive it with a `from '...'` grep.";

/// The language whose import edges are file-to-file and include type-only
/// imports - JavaScript files are indexed under it too.
pub(crate) const IMPORT_TYPE_LANGUAGE: &str = "typescript";

/// The follow-up a truncated walk needs, keyed by its `truncatedBy` wire name.
pub(crate) fn truncated_by(cause: &str) -> Option<&'static str> {
    match cause {
        "maxDepth" => {
            Some("`truncatedBy: maxDepth`: to go deeper, call again anchored on each of `frontierNodes`.")
        }
        "maxFanout" => Some(
            "`truncatedBy: maxFanout`: a node had more edges than the fan-out cap, so re-query that \
             node with the single-hop tools and page their results.",
        ),
        "explorationBudget" => Some(
            "`truncatedBy: explorationBudget`: call again with only `resume_token` set to the \
             returned `resumeToken` for the rest.",
        ),
        "responseSize" => Some(
            "`truncatedBy: responseSize`: call again with only `resume_token` set to the returned \
             `resumeToken` for the rest.",
        ),
        _ => None,
    }
}

pub(crate) const SEARCH_HITS: &str =
    "These hits are ranked by relevance, not resolved: once one plausibly matches, do one \
     confirming read and stop, without rewording the query or grepping the repo.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_once_sentence_is_sent_once_per_session_and_again_on_a_new_one() {
        let session = SessionHints::default();
        assert_eq!(session.once(true, HintKey::FileRow, FILE_ROW), Some(FILE_ROW));
        assert_eq!(session.once(true, HintKey::FileRow, FILE_ROW), None);
        assert_eq!(session.clone().once(true, HintKey::FileRow, FILE_ROW), None, "a clone shares the set");
        assert_eq!(
            session.once(true, HintKey::FilesTally, FILES_TALLY),
            Some(FILES_TALLY),
            "keys are separate"
        );
        assert_eq!(SessionHints::default().once(true, HintKey::FileRow, FILE_ROW), Some(FILE_ROW));
    }

    /// `serve_connection` builds one server per connection, and rmcp may
    /// clone it per request: clones must share one set, new servers must not.
    #[test]
    fn each_server_starts_its_own_session_and_its_clones_share_it() {
        use crate::daemon::indexing_status::IndexingStatus;
        use crate::daemon::lifecycle::CoreActivity;
        use crate::daemon::manifest::DiscoveredPlugins;
        use crate::daemon::registry::PluginRegistry;
        use crate::embedding::EmbeddingPipeline;
        use crate::storage::index_store::IndexStore;

        let dir = tempfile::tempdir().unwrap();
        let server = || {
            let embedding = Arc::new(EmbeddingPipeline::disabled());
            let registry = PluginRegistry::new(
                dir.path(),
                dir.path().to_path_buf(),
                DiscoveredPlugins::default(),
                None,
                None,
                Arc::clone(&embedding),
            );
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            super::super::GMeshMcpServer::new(
                Arc::new(IndexStore::new(conn)),
                Arc::new(registry),
                CoreActivity::new(),
                IndexingStatus::unindexed(),
                embedding,
            )
        };
        let first = server();
        let second = server();

        assert_eq!(first.hints.once(true, HintKey::FileRow, FILE_ROW), Some(FILE_ROW));
        assert_eq!(first.clone().hints.once(true, HintKey::FileRow, FILE_ROW), None);
        assert_eq!(second.hints.once(true, HintKey::FileRow, FILE_ROW), Some(FILE_ROW));
    }

    #[test]
    fn an_absent_trigger_neither_sends_nor_spends_the_sentence() {
        let session = SessionHints::default();
        assert_eq!(session.once(false, HintKey::SearchHits, SEARCH_HITS), None);
        assert_eq!(session.once(true, HintKey::SearchHits, SEARCH_HITS), Some(SEARCH_HITS));
    }

    #[test]
    fn join_keeps_order_and_is_absent_when_empty() {
        assert_eq!(join([None, None]), None);
        assert_eq!(join([Some("a."), None, Some("b.")]).as_deref(), Some("a. b."));
    }

    /// Each sentence stays one short sentence: it is paid for on every later
    /// turn of the session that receives it.
    #[test]
    fn every_sentence_stays_under_two_hundred_bytes() {
        let causes = ["maxDepth", "maxFanout", "explorationBudget", "responseSize"];
        let mut all = vec![
            ALL_UNRESOLVED,
            UNRESOLVED_ROW,
            PROVENANCE,
            AMBIGUOUS,
            AMBIGUOUS_SOURCED,
            AMBIGUOUS_PARTLY_SOURCED,
            FILE_ROW,
            FILES_TALLY,
            WALK_COMPLETE,
            WALK_COMPLETE_IMPORT_TYPE,
            SEARCH_HITS,
        ];
        all.extend(causes.iter().map(|cause| truncated_by(cause).unwrap()));
        for sentence in all {
            assert!(sentence.len() <= 200, "{} B: {sentence}", sentence.len());
        }
        assert_eq!(truncated_by("somethingElse"), None);
    }
}
