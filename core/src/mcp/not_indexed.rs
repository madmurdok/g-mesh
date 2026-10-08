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

/// Fixtures the handler tests share: a coverage for each state, a recorded
/// failed outcome, and the parsed body of a refusal.
#[cfg(test)]
pub(super) mod test_support {
    use std::collections::BTreeMap;

    use rmcp::model::{CallToolResult, ContentBlock};
    use rusqlite::Connection;

    use crate::daemon::registry::PathCoverage;
    use crate::languages::{self, LanguageOutcome};
    use crate::storage::schema;

    /// The innermost cause of [`FAILED_CHAIN`], as the field carries it.
    pub(in crate::mcp) const INNERMOST: &str = "No such file or directory (os error 2)";

    /// A stored failed-outcome chain (`languages::failed_error`'s shape):
    /// outermost first, one cause per line.
    pub(in crate::mcp) const FAILED_CHAIN: &str =
        "python plugin failed its bulk walk\nfailed to spawn g-mesh-plugin-python\nNo such file or directory (os error 2)";

    pub(in crate::mcp) fn python_absent() -> PathCoverage {
        PathCoverage::Absent(languages::entry("python").expect("python is catalogued"))
    }

    pub(in crate::mcp) fn python_failed() -> PathCoverage {
        PathCoverage::Failed("python".to_string())
    }

    /// Records `language` as failed with [`FAILED_CHAIN`], the way a walk does.
    pub(in crate::mcp) fn record_failed(conn: &Connection, language: &str) {
        let outcomes = BTreeMap::from([(
            language.to_string(),
            LanguageOutcome::Failed { error: FAILED_CHAIN.to_string() },
        )]);
        schema::record_language_outcomes(conn, &outcomes).expect("failed to record the outcome");
    }

    /// The JSON body of a tool error that must be a refusal.
    pub(in crate::mcp) fn refusal_body(result: &CallToolResult) -> serde_json::Value {
        assert_eq!(result.is_error, Some(true), "expected a tool error: {:?}", result.content);
        assert_eq!(result.content.len(), 1, "a refusal is one content block: {:?}", result.content);
        match &result.content[0] {
            ContentBlock::Text(text) => serde_json::from_str(&text.text)
                .unwrap_or_else(|err| panic!("a refusal's body must be JSON ({err}): {}", text.text)),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// The text of a tool error that must stay the plain message (no JSON).
    pub(in crate::mcp) fn plain_error(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected a tool error: {:?}", result.content);
        let text = match &result.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_err(),
            "a covered-language miss must stay a plain-text error: {text}"
        );
        assert!(!text.contains("not indexed here"), "no not-indexed sentence expected: {text}");
        text
    }

    /// Asserts `body` is the absent refusal for python after `message`.
    pub(in crate::mcp) fn assert_python_absent_refusal(body: &serde_json::Value, message: &str) {
        assert_eq!(
            body["notIndexed"],
            serde_json::json!({
                "language": "python",
                "reason": "pluginAbsent",
                "command": "g-mesh plugins install python",
            }),
            "{body}"
        );
        let error = body["error"].as_str().expect("the refusal keeps a human `error` string");
        assert!(error.starts_with(message), "the tool's own miss message comes first: {error}");
        assert!(error.contains("python files are not indexed here"), "{error}");
        assert!(error.contains("no plugin is installed"), "{error}");
        assert!(error.contains("`g-mesh plugins install python`"), "{error}");
    }

    /// Asserts `body` is the failed refusal for python (recorded with
    /// [`record_failed`]) after `message`.
    pub(in crate::mcp) fn assert_python_failed_refusal(body: &serde_json::Value, message: &str) {
        assert_eq!(
            body["notIndexed"],
            serde_json::json!({
                "language": "python",
                "reason": "pluginFailed",
                "command": "g-mesh reindex",
                "error": INNERMOST,
            }),
            "{body}"
        );
        let error = body["error"].as_str().expect("the refusal keeps a human `error` string");
        assert!(error.starts_with(message), "the tool's own miss message comes first: {error}");
        assert!(error.contains(&format!("its plugin failed ({INNERMOST})")), "{error}");
        assert!(error.contains("`g-mesh reindex`"), "{error}");
        assert!(!error.contains("plugins install"), "a failed language is not fixed by installing: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    /// The absent value serializes to exactly three camelCase
    /// keys; `error` and `filePaths` are omitted, not `null`/`[]`.
    ///
    /// Controls: drop `skip_serializing_if` on `error` (`"error": null`
    /// appears) or on `file_paths` (`"filePaths": []`); drop
    /// `rename_all = "camelCase"` on `NotIndexedReason` (`"PluginAbsent"`).
    #[test]
    fn an_absent_value_serializes_to_exactly_language_reason_and_command() {
        let value = NotIndexed::absent(crate::languages::entry("python").unwrap());
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            serde_json::json!({
                "language": "python",
                "reason": "pluginAbsent",
                "command": "g-mesh plugins install python",
            })
        );
    }

    /// A failed language read from the store carries
    /// the reindex command and only the innermost cause of the stored chain.
    ///
    /// Control: `error: error.map(str::to_string)` in `NotIndexed::failed`
    /// (the whole chain is carried) - the `error` assertion fails.
    #[test]
    fn a_failed_value_carries_the_reindex_command_and_the_innermost_cause() {
        let conn = setup();
        record_failed(&conn, "python");
        let value = NotIndexed::from_coverage(&conn, &python_failed()).unwrap();
        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            serde_json::json!({
                "language": "python",
                "reason": "pluginFailed",
                "command": "g-mesh reindex",
                "error": INNERMOST,
            })
        );
    }

    /// A failed language with no recorded outcome row (another language's
    /// row only): no `error` key, and the sentence names no cause.
    ///
    /// Control: match the recorded row on any language in `from_coverage`
    /// (drop `recorded == *language`) - rust's cause leaks into python's.
    #[test]
    fn a_failed_language_with_no_recorded_row_omits_the_error() {
        let conn = setup();
        record_failed(&conn, "rust");
        let value = NotIndexed::from_coverage(&conn, &python_failed()).unwrap();
        let json = serde_json::to_value(&value).unwrap();
        assert!(json.get("error").is_none(), "{json}");
        assert!(value.sentence().contains("its plugin failed; fix the plugin"), "{}", value.sentence());
    }

    /// The two sentences use the instructions' words and name the command.
    ///
    /// Control: swap the two arms of `sentence` - both assertions fail.
    #[test]
    fn each_sentence_names_its_state_and_its_command() {
        let absent = NotIndexed::absent(crate::languages::entry("go").unwrap()).sentence();
        assert!(absent.contains("go files are not indexed here: no plugin is installed"), "{absent}");
        assert!(absent.contains("`g-mesh plugins install go`"), "{absent}");
        assert!(absent.contains("not evidence the file is empty or missing"), "{absent}");

        let failed = NotIndexed::failed("go", Some("outer\ninner cause")).sentence();
        assert!(
            failed.contains("go files are not indexed here: its plugin failed (inner cause)"),
            "{failed}"
        );
        assert!(failed.contains("run `g-mesh reindex`"), "{failed}");
        assert!(!failed.contains("outer"), "{failed}");
    }

    /// `miss` with a coverage is a one-block JSON tool error; with `None`
    /// (a covered language) it is exactly the tool's plain message.
    ///
    /// Controls: make `miss`'s `None` arm call `refusal` with any value
    /// (the plain assertion fails); make `refusal` return
    /// `tool_result::error` of the sentence (the body is not JSON).
    #[test]
    fn a_miss_is_a_json_refusal_only_for_an_uncovered_language() {
        let conn = setup();
        let message = "g-mesh: no file 'x.py' found in the index";

        let refused = miss(&conn, Some(&python_absent()), message.to_string()).unwrap();
        assert_python_absent_refusal(&refusal_body(&refused), message);

        let plain = miss(&conn, None, message.to_string()).unwrap();
        assert_eq!(plain_error(&plain), message);
    }

    /// One entry per language in first-requested order,
    /// `filePaths` in request order with a repeated path listed once; the
    /// failed language's entry reads the store.
    ///
    /// Controls: drop the `contains` check in `group` (`x.py` twice); push
    /// a new entry per path instead of looking one up (three python entries).
    #[test]
    fn group_makes_one_entry_per_language_in_first_seen_order() {
        let conn = setup();
        record_failed(&conn, "python");
        let typescript = PathCoverage::Absent(crate::languages::entry("typescript").unwrap());
        let uncovered = vec![
            ("x.py".to_string(), python_failed()),
            ("web/a.ts".to_string(), typescript.clone()),
            ("tools/y.py".to_string(), python_failed()),
            ("x.py".to_string(), python_failed()),
        ];

        let grouped = serde_json::to_value(group(&conn, &uncovered).unwrap()).unwrap();
        assert_eq!(
            grouped,
            serde_json::json!([
                {
                    "language": "python",
                    "reason": "pluginFailed",
                    "command": "g-mesh reindex",
                    "error": INNERMOST,
                    "filePaths": ["x.py", "tools/y.py"],
                },
                {
                    "language": "typescript",
                    "reason": "pluginAbsent",
                    "command": "g-mesh plugins install typescript",
                    "filePaths": ["web/a.ts"],
                },
            ])
        );
        assert!(group(&conn, &[]).unwrap().is_empty());
    }
}
