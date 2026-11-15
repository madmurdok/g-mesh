//! What a bulk walk did for one language, as the index stores it, and the
//! stored form of a failed language's error.

/// What a bulk walk did for one language (ADR 0021). The install command is
/// not stored: [`crate::languages::CatalogueEntry::install_command`] derives
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageOutcome {
    /// A discovered plugin walked it to the end. `files` is its `File`-node
    /// count; a discovered language with no files is `Indexed { files: 0 }`.
    Indexed { files: usize },
    /// A catalogue language with no discovered plugin and at least one file
    /// in the project. Not an error. `files` is `None` when it was not counted.
    PluginAbsent { files: Option<usize> },
    /// A discovered plugin that could not be used; nothing of the language is
    /// in the index. `error` is the full error chain, one cause per line,
    /// outermost first ([`failed_error`]); [`error_on_one_line`] renders it
    /// for a one-line message.
    Failed { error: String },
}

/// The stored form of a failed language's error: every cause of `err`'s
/// chain on its own line, outermost first. A newline inside one cause's
/// message becomes a space, so one line is always one cause and the innermost
/// cause is the last non-empty line - no guessing at a separator that a
/// cause's own text may contain.
pub fn failed_error(err: &anyhow::Error) -> String {
    err.chain()
        .map(|cause| {
            cause
                .to_string()
                .split(['\r', '\n'])
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A stored error ([`failed_error`]) on one line: its causes joined by ": ",
/// as anyhow's `{:#}` renders a chain.
pub fn error_on_one_line(error: &str) -> String {
    error.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(": ")
}
