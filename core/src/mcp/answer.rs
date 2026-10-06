//! The non-row answers of `find_references`, `find_callers` and
//! `find_callees` (`answer: "files"` and `answer: "count"`): the same
//! whole-set predicate the row walk pages through, summarised instead of
//! listed. "Which files" and "how many / is it called at all" are answered
//! from one `GROUP BY` or one `COUNT`, without the rows nobody asked for.

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction, FileTally};

use super::tool_result::success;
use super::{anchor, Answer};

/// Wire shape of a non-row answer. `D` is the tool's own response-level
/// disclosures (unlinked usages, excluded references, provenance, ...),
/// flattened in so they keep the field names the row answer gives them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary<'a, D: Serialize> {
    anchor: &'a anchor::AnchorInfo,
    /// Exact number of matching rows over the whole set.
    total: usize,
    /// How many of `total` the linker could not confirm. `count` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolved: Option<usize>,
    /// Every file holding a match, with its count. `files` only, and sent
    /// there even when empty: it is the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<&'a [FileTally]>,
    /// Present only when the tally's entry cap or the response's byte
    /// ceiling left files out, which the caller could otherwise see only by
    /// summing `refs` against `total`.
    #[serde(skip_serializing_if = "is_false")]
    files_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    #[serde(flatten)]
    disclosures: D,
}

/// The [`Summary`] for `counted`, with `files` cut to what fits
/// `pagination::MAX_RESPONSE_BYTES` beside everything else the response
/// carries. `tail(files, send)` returns the hint and disclosures for a
/// response naming `files`; `send` is false while candidates are measured and
/// true for the one sent, so a once-per-session hint is spent only there.
pub(super) fn respond<D: Serialize>(
    anchor: &anchor::AnchorInfo,
    counted: &Counted,
    mut tail: impl FnMut(Option<&[FileTally]>, bool) -> (Option<String>, D),
) -> Result<CallToolResult, ErrorData> {
    let mut build = |budget: usize, send: bool| {
        let mut files = counted.files.clone();
        let cut = files.as_mut().is_some_and(|files| pagination::truncate_to_bytes(files, budget));
        let (hint, disclosures) = tail(files.as_deref(), send);
        let bytes = pagination::wire_len(&Summary {
            anchor,
            total: counted.total,
            unresolved: counted.unresolved,
            files: files.as_deref(),
            files_truncated: counted.files_truncated || cut,
            hint: hint.clone(),
            disclosures: &disclosures,
        });
        (files, cut, hint, disclosures, bytes)
    };
    let budget = pagination::fit_budget(|budget| {
        let (files, _, _, _, bytes) = build(budget, false);
        (files.as_deref().map_or(0, pagination::wire_len), bytes)
    });
    let (files, cut, hint, disclosures, _) = build(budget, true);
    success(&Summary {
        anchor,
        total: counted.total,
        unresolved: counted.unresolved,
        files: files.as_deref(),
        files_truncated: counted.files_truncated || cut,
        hint,
        disclosures,
    })
}

/// The counted part of a [`Summary`], before the tool attaches its anchor,
/// hint and disclosures.
pub(super) struct Counted {
    pub total: usize,
    pub unresolved: Option<usize>,
    pub files: Option<Vec<FileTally>>,
    pub files_truncated: bool,
}

/// Counts (and for [`Answer::Files`] tallies) the edges a row walk over the
/// same anchor, direction, kinds and scope would page through. `None` for
/// [`Answer::Rows`], which the caller answers with rows as before.
pub(super) fn count(
    conn: &Connection,
    answer: Answer,
    anchor_id: &str,
    direction: Direction,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> anyhow::Result<Option<Counted>> {
    if answer == Answer::Rows {
        return Ok(None);
    }
    let counted = pagination::count_edges_by_resolution(conn, anchor_id, direction, edge_kinds, file_paths)
        .context("failed to count edges")?;
    if answer == Answer::Count {
        return Ok(Some(Counted {
            total: counted.total,
            unresolved: Some(counted.unresolved),
            files: None,
            files_truncated: false,
        }));
    }
    let files = pagination::tally_edge_files(conn, anchor_id, direction, edge_kinds, file_paths)
        .context("failed to tally edge files")?;
    let tallied: i64 = files.iter().map(|tally| tally.refs).sum();
    Ok(Some(Counted {
        total: counted.total,
        unresolved: None,
        files_truncated: (tallied as usize) < counted.total,
        files: Some(files),
    }))
}

/// `skip_serializing_if` predicate for a `bool` that is only interesting when
/// true - the "absent, not false" rule the `Option` fields get from
/// `Option::is_none`.
pub(super) fn is_false(flag: &bool) -> bool {
    !*flag
}
