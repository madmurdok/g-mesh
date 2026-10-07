//! Why a path-anchored answer is empty when the path's language is not
//! indexed at all. Design: `docs/architecture/gm-503-absent-language-field.md`.
//!
//! The session instructions already say which languages are absent or
//! failed, but they are read once per session; an agent that skipped them
//! would otherwise get `no file 'tools/gen.py' found in the index` for a
//! Python file in a project with no Python plugin - the same words a typo
//! gets. The three path-anchored tools (`get_file_outline`,
//! `find_definition` by `file_path` + `position`, `get_dependencies` by
//! `file_path`) refuse such a path with [`refusal`]: a tool error whose one
//! content block is a JSON object carrying the human message and the
//! structured [`NotIndexed`] value.
//!
//! The four find tools with a `file_paths` filter (`find_references`,
//! `find_callers`, `find_callees`, direct `find_implementations`) answer as
//! usual and add a `notIndexed` list, [`group`]ed per language, naming the
//! filter entries in an uncovered language. `search_code` has no path.
//!
//! The coverage itself comes from the daemon's registry
//! (`PluginRegistry::path_coverage`), computed per call with no I/O; the one
//! store read here is the failed language's recorded error, and only on the
//! refusal path.

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::daemon::registry::PathCoverage;
use crate::languages::{CatalogueEntry, LanguageOutcome};
use crate::storage::schema;

use super::instructions::error_cause;
use super::tool_result::internal_error;

/// Why nothing of a language is in the index.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NotIndexedReason {
    /// No plugin for the language is installed.
    PluginAbsent,
    /// Its plugin is installed but failed the last walk (ADR 0021).
    PluginFailed,
}

/// One uncovered language, as an answer reports it. Serialized camelCase;
/// optional fields are omitted when empty.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct NotIndexed {
    pub language: String,
    pub reason: NotIndexedReason,
    /// The command that fixes it: `g-mesh plugins install <lang>` when
    /// absent, `g-mesh reindex` (after fixing the plugin) when failed.
    pub command: String,
    /// Failed only: the innermost cause of the recorded error, as the
    /// session instructions show it (`instructions::error_cause`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Filter answers only (`file_paths`): which requested paths this
    /// language covers. Empty, and so omitted, on a path-anchored refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_paths: Vec<String>,
}

impl NotIndexed {
    pub(super) fn absent(entry: &CatalogueEntry) -> Self {
        Self {
            language: entry.language.to_string(),
            reason: NotIndexedReason::PluginAbsent,
            command: entry.install_command(),
            error: None,
            file_paths: Vec::new(),
        }
    }

    /// `error` is the stored error chain (`languages::failed_error`), or
    /// `None` when no outcome was recorded for the language.
    pub(super) fn failed(language: &str, error: Option<&str>) -> Self {
        Self {
            language: language.to_string(),
            reason: NotIndexedReason::PluginFailed,
            command: "g-mesh reindex".to_string(),
            error: error.map(error_cause),
            file_paths: Vec::new(),
        }
    }

    /// Builds the value for `coverage`, reading a failed language's recorded
    /// error from the store under the read the caller already holds.
    pub(super) fn from_coverage(conn: &Connection, coverage: &PathCoverage) -> Result<Self, ErrorData> {
        match coverage {
            PathCoverage::Absent(entry) => Ok(Self::absent(entry)),
            PathCoverage::Failed(language) => {
                let outcomes = schema::language_outcomes(conn)
                    .map_err(|e| internal_error("failed to read the recorded language outcomes", e))?;
                let error = outcomes.into_iter().find_map(|(recorded, outcome)| match outcome {
                    LanguageOutcome::Failed { error } if recorded == *language => Some(error),
                    _ => None,
                });
                Ok(Self::failed(language, error.as_deref()))
            }
        }
    }

    /// The human half of a refusal, appended to the tool's own miss message.
    /// Uses the session instructions' wording for the two states, so an
    /// agent that did read them sees the same words.
    pub(super) fn sentence(&self) -> String {
        let language = &self.language;
        match self.reason {
            NotIndexedReason::PluginAbsent => format!(
                " - {language} files are not indexed here: no plugin is installed (`{}`), so this is not \
                 evidence the file is empty or missing.",
                self.command
            ),
            NotIndexedReason::PluginFailed => {
                let cause = self.error.as_deref().map(|error| format!(" ({error})")).unwrap_or_default();
                format!(
                    " - {language} files are not indexed here: its plugin failed{cause}; fix the plugin, then \
                     run `{}`. This is not evidence the file is empty or missing.",
                    self.command
                )
            }
        }
    }
}

/// The `notIndexed` field of a filtered find answer (`find_references`,
/// `find_callers`, `find_callees`, direct `find_implementations`): one entry
/// per uncovered language, in the order its first path was requested, with
/// `filePaths` naming the requested paths it covers (duplicates dropped).
/// `uncovered` pairs each `file_paths` entry the registry reports as not
/// covered with that coverage; empty (no filter, or every path covered)
/// yields an empty list, which the answers omit.
pub(super) fn group(
    conn: &Connection,
    uncovered: &[(String, PathCoverage)],
) -> Result<Vec<NotIndexed>, ErrorData> {
    let mut grouped: Vec<NotIndexed> = Vec::new();
    for (path, coverage) in uncovered {
        let language = match coverage {
            PathCoverage::Absent(entry) => entry.language,
            PathCoverage::Failed(language) => language.as_str(),
        };
        let index = match grouped.iter().position(|entry| entry.language == language) {
            Some(index) => index,
            None => {
                grouped.push(NotIndexed::from_coverage(conn, coverage)?);
                grouped.len() - 1
            }
        };
        let file_paths = &mut grouped[index].file_paths;
        if !file_paths.contains(path) {
            file_paths.push(path.clone());
        }
    }
    Ok(grouped)
}

/// The JSON body of a refusal: `{"error": <message + sentence>, "notIndexed": {...}}`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Refusal<'a> {
    error: String,
    not_indexed: &'a NotIndexed,
}

/// A tool error (`isError: true`) for a path-anchored miss on a path in an
/// uncovered language. `message` is the tool's own miss message (`g-mesh: no
/// file 'x.py' found in the index`); the one content block is the JSON text
/// of [`Refusal`], so a reader of the text sees the reason and a client can
/// parse the `notIndexed` value.
pub(super) fn refusal(message: &str, not_indexed: &NotIndexed) -> Result<CallToolResult, ErrorData> {
    let body = Refusal { error: format!("{message}{}", not_indexed.sentence()), not_indexed };
    Ok(CallToolResult::error(vec![ContentBlock::json(&body)?]))
}

/// [`refusal`] for `coverage`, or the plain text error when the path's
/// language is covered (`None`). The one entry point the three handlers use
/// on their miss path.
pub(super) fn miss(
    conn: &Connection,
    coverage: Option<&PathCoverage>,
    message: String,
) -> Result<CallToolResult, ErrorData> {
    match coverage {
        Some(coverage) => refusal(&message, &NotIndexed::from_coverage(conn, coverage)?),
        None => super::tool_result::error(message),
    }
}
