//! [`ServerProfile`]: what a plugin's code declares about how the bridge
//! talks to its particular language server.

use std::time::Duration;

/// How the bridge talks to one particular server, as its plugin's code
/// declares it. Handed to [`LspBridge::with_profile`](crate::LspBridge::with_profile);
/// a bridge given none uses [`ServerProfile::DEFAULT`].
///
/// A per-server knob is a field here when plugin code depends on it, and a
/// `[plugin.semantic]` manifest key ([`SemanticConfig`](crate::SemanticConfig))
/// when the bridge alone acts on it and a packager may change it without a
/// rebuild. `client_watches_files` is safe only when the plugin vets
/// changes before the server sees them; `language_ids` mirrors the plugin's
/// extension table; `quiescent_status` is a protocol extension of one server.
///
/// Build one as a struct literal ending in `..ServerProfile::DEFAULT`, so a
/// field added later leaves every plugin's profile compiling and unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerProfile {
    /// `(extension, languageId)` pairs for `didOpen`: a file whose name ends
    /// with one of the extensions is opened under that `languageId`, and a
    /// file no pair matches is opened under the bridge's language. A server
    /// may parse a document by its `languageId` rather than by its file name:
    /// tsserver reads `typescriptreact` as TSX, and a `.tsx` file opened as
    /// `typescript` is parsed without JSX.
    pub language_ids: &'static [(&'static str, &'static str)],
    /// The budget of each server's first question, instead of
    /// [`Budgets::request`](crate::Budgets::request) - see
    /// [`Budgets`](crate::Budgets)'s `warm_up`. For a server that answers
    /// nothing while it loads a project and reports no progress for the load.
    /// `None` leaves the bridge's `Budgets::warm_up` as it is.
    pub warm_up: Option<Duration>,
    /// Asks the server for rust-analyzer's `experimental/serverStatus`
    /// notification and believes it ready when it reports `quiescent: true`
    /// with nothing in flight, rather than after
    /// [`Budgets::settle`](crate::Budgets::settle) of silence - see
    /// [`LspBridge`](crate::LspBridge)'s doc on readiness. For a server whose
    /// start-up phases can be separated by gaps longer than the settle. A
    /// server that never sends a status keeps the quiet-period rule.
    pub quiescent_status: bool,
    /// Tells the server this client watches files for it
    /// (`workspace.didChangeWatchedFiles.dynamicRegistration`), so a server
    /// that would otherwise run its own file watcher registers its globs
    /// with the client instead. The bridge then forwards, as
    /// `workspace/didChangeWatchedFiles`, only the files core sends a pass
    /// (the plugin's own source files, which changed), and only inside
    /// `SemanticEngine::answer`, just before that pass's documents are
    /// synced; an already open one is re-synced with `didChange`, and one the
    /// index holds is opened with `didOpen`, instead - so in practice only
    /// deleted files and files outside the index are announced. Any other
    /// change on disk (a manifest, a config) reaches the server only when the
    /// plugin asks it to (for rust-analyzer, `rust-analyzer/reloadWorkspace`
    /// through [`LspBridge::request`](crate::LspBridge::request)): for a
    /// plugin that must vet a change before the server may react to it, and
    /// that wraps `answer` in that vetting.
    pub client_watches_files: bool,
}

impl ServerProfile {
    /// No language ids, no warm-up, the quiet-period readiness rule, and the
    /// server watches its own files.
    pub const DEFAULT: Self =
        Self { language_ids: &[], warm_up: None, quiescent_status: false, client_watches_files: false };
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self::DEFAULT
    }
}
