//! Typed answers of the index-backed MCP tools, and the one door to them.
//! The server renders them (`response_bound::render`); `plugin_check` reads
//! them.

use rmcp::ErrorData;
use serde::Serialize;

use super::context::QueryCtx;
use super::find_callers_callees::{self, CalleesAnswer, CallersAnswer};
use super::find_definition::{self, CandidatePage, DefinitionNode, FileNamePage};
use super::find_implementations::{self, ImplementationAnswer};
use super::find_references::{self, ReferencesAnswer};
use super::not_indexed::NotIndexed;
use super::{FindDefinitionParams, FindImplementationsParams, SymbolQueryParams};

/// What one tool call answers, before it becomes bytes.
pub(crate) enum ToolAnswer<P> {
    /// The tool's own answer (`isError` unset).
    Page(P),
    /// `symbol_name` did not pin one declaration: the ranked candidates, or
    /// the file-name / semantic-neighbour suggestions (`isError` unset).
    Candidates(Candidates),
    /// A tool-level refusal (`isError: true`).
    Refused(Refusal),
}

impl<P> ToolAnswer<P> {
    /// A prose refusal.
    pub(crate) fn refused(message: impl Into<String>) -> Self {
        Self::Refused(Refusal::Text(message.into()))
    }
}

/// The page of an answer that never carries one. Not `Infallible`: `render`
/// needs a `Serialize` page type, and serde implements none for it.
#[derive(Serialize)]
pub(crate) enum NoPage {}

impl WirePage for NoPage {}

/// What `find_definition`'s name ladder hands back instead of a node, and
/// what the server answers before a tool runs.
pub(crate) type Redirect = ToolAnswer<NoPage>;

impl Redirect {
    /// The same answer, as any tool's.
    pub(crate) fn widen<P>(self) -> ToolAnswer<P> {
        match self {
            ToolAnswer::Page(never) => match never {},
            ToolAnswer::Candidates(candidates) => ToolAnswer::Candidates(candidates),
            ToolAnswer::Refused(refusal) => ToolAnswer::Refused(refusal),
        }
    }
}

/// Serialized as the inner page alone.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Candidates {
    /// `resolvedBy` absent, `ambiguous: true`.
    Ambiguous(CandidatePage),
    /// `resolvedBy: fileName | semanticNeighbours`, `ambiguous: false`.
    Suggested(FileNamePage),
}

#[derive(Clone)]
pub(crate) enum Refusal {
    /// Prose, one text block.
    Text(String),
    /// A miss on a path whose language is not indexed: JSON `{error, notIndexed}`.
    NotIndexed { error: String, not_indexed: NotIndexed },
}

/// A page `render` can send. Its bytes are its `Serialize` output.
pub(crate) trait WirePage: Serialize {
    /// A prose note sent in its own content block before the page. Default:
    /// none.
    fn lead_note(&self) -> Option<&str> {
        None
    }
}

/// `find_definition`, both passes of the semantic rung included.
pub(crate) fn definition(
    ctx: &QueryCtx,
    params: FindDefinitionParams,
) -> Result<ToolAnswer<DefinitionNode>, ErrorData> {
    ctx.resolve_lazily(find_definition::handle, params)
}

pub(crate) fn references(
    ctx: &QueryCtx,
    params: SymbolQueryParams,
) -> Result<ToolAnswer<ReferencesAnswer>, ErrorData> {
    ctx.resolve_lazily(find_references::handle, params)
}

pub(crate) fn callers(
    ctx: &QueryCtx,
    params: SymbolQueryParams,
) -> Result<ToolAnswer<CallersAnswer>, ErrorData> {
    ctx.resolve_lazily(find_callers_callees::handle_callers, params)
}

pub(crate) fn implementations(
    ctx: &QueryCtx,
    params: FindImplementationsParams,
) -> Result<ToolAnswer<ImplementationAnswer>, ErrorData> {
    ctx.resolve_lazily(find_implementations::dispatch, params)
}

/// [`definition`] for the server (`QueryCtx::resolve_lazily_off_worker`).
pub(crate) async fn definition_off_worker(
    ctx: QueryCtx,
    params: FindDefinitionParams,
) -> Result<ToolAnswer<DefinitionNode>, ErrorData> {
    ctx.resolve_lazily_off_worker(find_definition::handle, params).await
}

pub(crate) async fn references_off_worker(
    ctx: QueryCtx,
    params: SymbolQueryParams,
) -> Result<ToolAnswer<ReferencesAnswer>, ErrorData> {
    ctx.resolve_lazily_off_worker(find_references::handle, params).await
}

pub(crate) async fn callers_off_worker(
    ctx: QueryCtx,
    params: SymbolQueryParams,
) -> Result<ToolAnswer<CallersAnswer>, ErrorData> {
    ctx.resolve_lazily_off_worker(find_callers_callees::handle_callers, params).await
}

pub(crate) async fn callees_off_worker(
    ctx: QueryCtx,
    params: SymbolQueryParams,
) -> Result<ToolAnswer<CalleesAnswer>, ErrorData> {
    ctx.resolve_lazily_off_worker(find_callers_callees::handle_callees, params).await
}

pub(crate) async fn implementations_off_worker(
    ctx: QueryCtx,
    params: FindImplementationsParams,
) -> Result<ToolAnswer<ImplementationAnswer>, ErrorData> {
    ctx.resolve_lazily_off_worker(find_implementations::dispatch, params).await
}

/// Lets the test helpers read a typed answer as the client would.
#[cfg(test)]
pub(crate) mod test_support {
    use rmcp::model::CallToolResult;

    use super::{Redirect, Refusal, ToolAnswer, WirePage};
    use crate::mcp::response_bound;

    /// What a client receives for this answer.
    pub(crate) trait Rendered {
        fn rendered(&self) -> CallToolResult;
    }

    impl Rendered for CallToolResult {
        fn rendered(&self) -> CallToolResult {
            self.clone()
        }
    }

    impl<P: WirePage> Rendered for ToolAnswer<P> {
        fn rendered(&self) -> CallToolResult {
            response_bound::render(self).expect("a typed answer must render")
        }
    }

    impl<P: WirePage> WirePage for &P {
        fn lead_note(&self) -> Option<&str> {
            (**self).lead_note()
        }
    }

    /// A page alone, rendered as the tool's own answer.
    impl<P: WirePage> Rendered for P {
        fn rendered(&self) -> CallToolResult {
            response_bound::render(&ToolAnswer::Page(self)).expect("a page must render")
        }
    }

    impl Rendered for Refusal {
        fn rendered(&self) -> CallToolResult {
            Redirect::Refused(self.clone()).rendered()
        }
    }
}
