//! What a front daemon serves (D11 step 2 in
//! `docs/architecture/lazy-indexing.md`, GM-399): the usual eight tools'
//! schemas plus `select_project`, instructions that list the folder's
//! projects, and an immediate, explanatory error for every tool that would
//! need an index.
//!
//! A child module of `mcp` so it can list the eight tools through the
//! private `GMeshMcpServer::tool_router()` that `#[tool_router]` generates:
//! their schemas stay byte-identical to a normal daemon's by construction.
//!
//! Selecting a project answers with `_meta["g-mesh/switchProject"].root`,
//! a directive for the shim (slice 5, D11 step 3), which re-points the
//! session at that project's own daemon. The front itself never switches
//! anything: it holds no index, no plugins and no per-session state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::handler::server::common::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult, Meta,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{instructions, GMeshMcpServer};
use crate::daemon::candidates::{self, Candidate, Detection, Limits};
use crate::daemon::lifecycle::CoreActivity;
use crate::ipc::AsyncStream;

/// The tool a front adds to the usual eight.
pub const SELECT_PROJECT: &str = "select_project";

/// The `_meta` key a successful selection carries; the shim (slice 5) acts
/// on it and removes it before the client sees the result.
pub const SWITCH_PROJECT_META: &str = "g-mesh/switchProject";

const SELECT_PROJECT_DESCRIPTION: &str = "Choose which project under this folder g-mesh serves for this \
     session. Without `project`, lists the candidate projects. With `project` (a listed relative path, or \
     an absolute path), selects it: the result names the project this session then serves and carries \
     its guidance. Call again to switch, or to re-read that guidance.";

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SelectProjectParams {
    /// A project's path relative to this folder, as `select_project` with no
    /// argument lists it, or its absolute path. Omit to list the projects.
    pub project: Option<String>,
}

/// Everything a front's connections share, built once at startup.
pub struct Front {
    /// Canonical.
    root: PathBuf,
    instructions: String,
    tools: Vec<Tool>,
    /// The eight index-backed tools, by name.
    index_tools: HashSet<String>,
}

impl Front {
    pub fn new(root: PathBuf, detection: &Detection) -> Self {
        let index_tools_listed = GMeshMcpServer::tool_router().list_all();
        let index_tools = index_tools_listed.iter().map(|tool| tool.name.to_string()).collect();
        let mut tools = index_tools_listed;
        tools.push(Tool::new(
            SELECT_PROJECT,
            SELECT_PROJECT_DESCRIPTION,
            schema_for_type::<SelectProjectParams>(),
        ));
        let instructions = instructions::build_front(&root, detection);
        Self { root, instructions, tools, index_tools }
    }

    /// A fresh walk of the folder: cheap, bounded, and always current.
    async fn candidates(&self) -> candidates::Walk {
        let root = self.root.clone();
        match tokio::task::spawn_blocking(move || candidates::walk(&root, Limits::default())).await {
            Ok(walk) => walk,
            Err(err) => {
                eprintln!("g-mesh daemon: the candidate walk panicked: {err}");
                candidates::Walk {
                    candidates: Vec::new(),
                    entries_read: 0,
                    elapsed: std::time::Duration::ZERO,
                    truncated: false,
                }
            }
        }
    }

    async fn select_project(&self, params: SelectProjectParams) -> CallToolResult {
        let found = self.candidates().await;
        let Some(project) = params.project else {
            return text_result(false, list_candidates(&self.root, &found));
        };
        match find_candidate(&found.candidates, &project) {
            Some(candidate) => {
                let root = candidate.abs_path.display().to_string();
                let mut meta = JsonObject::new();
                meta.insert(SWITCH_PROJECT_META.to_string(), serde_json::json!({ "root": root }));
                CallToolResult::success(vec![ContentBlock::text(format!("Selected {root}"))])
                    .with_meta(Some(Meta(meta)))
            }
            None => text_result(
                true,
                format!(
                    "g-mesh: '{project}' is not one of the projects under {}.\n{}",
                    self.root.display(),
                    list_candidates(&self.root, &found)
                ),
            ),
        }
    }

    /// The answer to any index-backed tool: there is no index to answer
    /// from, so it says what to do instead, at once.
    async fn not_selected(&self, arguments: Option<&JsonObject>) -> CallToolResult {
        let found = self.candidates().await;
        let names: Vec<&str> = found.candidates.iter().map(|c| c.rel_path.as_str()).collect();
        let mut message = format!(
            "g-mesh: {} is a folder of {} projects and none is selected. Call select_project with one of: \
             {} (or ask the user which one they are working on).",
            self.root.display(),
            instructions::project_count(names.len(), found.truncated),
            names.join(", ")
        );
        let file_path = arguments.and_then(|args| args.get("file_path")).and_then(|value| value.as_str());
        if let Some(candidate) =
            file_path.and_then(|path| containing_candidate(&self.root, &found.candidates, path))
        {
            message.push_str(&format!(
                " The file_path you passed lies in '{}': select it, then pass paths relative to it.",
                candidate.rel_path
            ));
        }
        text_result(true, message)
    }
}

fn text_result(is_error: bool, text: String) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// `select_project` with no argument: one line per candidate.
fn list_candidates(root: &Path, found: &candidates::Walk) -> String {
    let mut out = format!(
        "{} holds {} projects. Select one with select_project {{\"project\": \"<path>\"}}:\n",
        root.display(),
        instructions::project_count(found.candidates.len(), found.truncated)
    );
    for candidate in &found.candidates {
        let mut tags = candidate.markers.join(", ");
        if candidate.is_worktree {
            tags.push_str("; worktree");
        }
        out.push_str(&format!("- {} [{tags}]\n", candidate.rel_path));
    }
    if found.truncated {
        out.push_str("(the scan stopped at its limit; more projects may exist below this folder)\n");
    }
    out
}

/// `path` without a leading `./` and trailing `/`, `\` turned into `/`.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");
    path.trim_end_matches('/').to_string()
}

/// The candidate `project` names: by `rel_path`, or by absolute path
/// (canonicalized, so a symlinked spelling of a candidate still matches).
fn find_candidate<'a>(candidates: &'a [Candidate], project: &str) -> Option<&'a Candidate> {
    let as_path = Path::new(project);
    if as_path.is_absolute() {
        let canonical = as_path.canonicalize().ok()?;
        return candidates.iter().find(|c| c.abs_path == canonical);
    }
    let rel = normalize(project);
    candidates.iter().find(|c| c.rel_path == rel)
}

/// The candidate whose directory holds `file_path` (relative to the root,
/// or absolute under it), matched by whole path segments.
fn containing_candidate<'a>(
    root: &Path,
    candidates: &'a [Candidate],
    file_path: &str,
) -> Option<&'a Candidate> {
    let as_path = Path::new(file_path);
    let rel = if as_path.is_absolute() {
        let stripped = as_path.strip_prefix(root).ok()?;
        normalize(&stripped.to_string_lossy())
    } else {
        normalize(file_path)
    };
    candidates.iter().find(|c| rel.starts_with(&c.rel_path) && rel[c.rel_path.len()..].starts_with('/'))
}

/// Serves one connection until the client goes away.
pub async fn serve_connection(
    stream: AsyncStream,
    front: Arc<Front>,
    core_activity: Arc<CoreActivity>,
) -> Result<()> {
    let service =
        FrontServer { front, core_activity }.serve(stream).await.context("MCP initialization failed")?;
    service.waiting().await.context("MCP session task failed")?;
    Ok(())
}

#[derive(Clone)]
pub struct FrontServer {
    front: Arc<Front>,
    core_activity: Arc<CoreActivity>,
}

impl ServerHandler for FrontServer {
    fn get_info(&self) -> ServerInfo {
        // The same shape as `GMeshMcpServer::get_info`, so a client cannot
        // tell a front from a project daemon by anything but the text.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("g-mesh", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.front.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.front.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.core_activity.request();
        if request.name == SELECT_PROJECT {
            let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
            let params: SelectProjectParams = serde_json::from_value(arguments).map_err(|err| {
                ErrorData::invalid_params(format!("invalid select_project arguments: {err}"), None)
            })?;
            return Ok(self.front.select_project(params).await);
        }
        if self.front.index_tools.contains(request.name.as_ref()) {
            return Ok(self.front.not_selected(request.arguments.as_ref()).await);
        }
        Err(ErrorData::invalid_params(format!("tool not found: {}", request.name), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(rel: &str) -> Candidate {
        Candidate {
            rel_path: rel.to_string(),
            abs_path: PathBuf::from("/root").join(rel),
            markers: vec![".git"],
            is_worktree: false,
        }
    }

    #[test]
    fn a_file_path_is_matched_to_its_candidate_by_whole_segments() {
        let root = Path::new("/root");
        let candidates = [candidate("b"), candidate("group/bb")];
        let hit = |path: &str| containing_candidate(root, &candidates, path).map(|c| c.rel_path.as_str());
        assert_eq!(hit("b/b.ts"), Some("b"));
        assert_eq!(hit("./b/src/x.ts"), Some("b"));
        assert_eq!(hit("/root/group/bb/x.go"), Some("group/bb"));
        assert_eq!(hit("bb/x.ts"), None, "a prefix of a segment is not a match");
        assert_eq!(hit("b"), None, "the candidate directory itself is not a file in it");
        assert_eq!(hit("/elsewhere/b/x.ts"), None);
    }

    #[test]
    fn a_project_is_found_by_its_relative_path() {
        let candidates = [candidate("a"), candidate("group/c")];
        assert_eq!(find_candidate(&candidates, "group/c/").map(|c| c.rel_path.as_str()), Some("group/c"));
        assert_eq!(find_candidate(&candidates, "./a").map(|c| c.rel_path.as_str()), Some("a"));
        assert!(find_candidate(&candidates, "c").is_none());
    }
}
