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

/// The tools a front lists: the usual eight, then `select_project`.
pub(super) fn listed_tools() -> Vec<Tool> {
    let mut tools = GMeshMcpServer::tool_router().list_all();
    tools.push(Tool::new(
        SELECT_PROJECT,
        SELECT_PROJECT_DESCRIPTION,
        schema_for_type::<SelectProjectParams>(),
    ));
    tools
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
        let tools = listed_tools();
        let index_tools = tools
            .iter()
            .filter(|tool| tool.name != SELECT_PROJECT)
            .map(|tool| tool.name.to_string())
            .collect();
        let indexed: HashSet<&str> = detection
            .candidates
            .iter()
            .filter(|candidate| candidates::has_completed_index(&candidate.abs_path))
            .map(|candidate| candidate.rel_path.as_str())
            .collect();
        let instructions = instructions::build_front(&root, detection, &indexed);
        Self { root, instructions, tools, index_tools }
    }

    /// A fresh walk of the folder: cheap, bounded, and always current.
    async fn candidates(&self) -> candidates::Walk {
        let root = self.root.clone();
        match tokio::task::spawn_blocking(move || candidates::walk(&root, Limits::default())).await {
            Ok(walk) => walk,
            Err(err) => {
                crate::log_line!("g-mesh daemon: the candidate walk panicked: {err}");
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
        match find_candidate(&self.root, &found.candidates, &project) {
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

/// `path`'s segments, split on `/` and `\` alike, without empty and `.` ones.
fn segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".").collect()
}

/// The candidate `project` names: by `rel_path`, or by a path with a root
/// under `root`, both compared as [`segments`].
///
/// `has_root` rather than `is_absolute` picks the branch, as in
/// [`containing_candidate`]: on Windows `/root/a` has a root but no drive.
/// A rooted path that names no candidate by its segments below `root` (a
/// symlink inside it, `..` in it) is retried canonicalized against each
/// candidate's canonical `abs_path`.
fn find_candidate<'a>(root: &Path, candidates: &'a [Candidate], project: &str) -> Option<&'a Candidate> {
    let by_segments = |rel: &str| {
        let rel = segments(rel);
        candidates.iter().find(|c| segments(&c.rel_path) == rel)
    };
    let as_path = Path::new(project);
    if !as_path.has_root() {
        return by_segments(project);
    }
    relative_to_root(root, as_path).and_then(|rel| by_segments(&rel.to_string_lossy())).or_else(|| {
        let canonical = as_path.canonicalize().ok()?;
        candidates.iter().find(|c| c.abs_path == canonical)
    })
}

/// The candidate whose directory holds `file_path` (relative to the root,
/// or absolute under it), matched by whole path segments.
///
/// Separator-agnostic: the path is compared as segments split on `/` and `\`
/// alike (an absolute one after [`Path::strip_prefix`], which compares
/// components), so a Windows client's `group\bb\x.go` and a POSIX
/// `group/bb/x.go` land on the same candidate. `has_root` rather than
/// `is_absolute` picks the branch: on Windows `/root/x` has a root but no
/// drive, so it is not absolute, and it is still not relative to the root.
fn containing_candidate<'a>(
    root: &Path,
    candidates: &'a [Candidate],
    file_path: &str,
) -> Option<&'a Candidate> {
    let as_path = Path::new(file_path);
    let rel = if as_path.has_root() {
        relative_to_root(root, as_path)?.to_string_lossy().into_owned()
    } else {
        file_path.to_string()
    };
    let rel = segments(&rel);
    candidates.iter().find(|c| {
        let candidate = segments(&c.rel_path);
        rel.len() > candidate.len() && rel.iter().zip(&candidate).all(|(a, b)| a == b)
    })
}

/// `path` below `root`, compared by components. `root` is canonical, so a
/// spelling of it that differs textually (a symlink, or on Windows the `\\?\`
/// prefix `canonicalize` adds) is retried canonicalized.
fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    path.canonicalize().ok()?.strip_prefix(root).ok().map(Path::to_path_buf)
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
    fn a_backslash_separated_path_is_matched_like_a_slash_separated_one() {
        let root = Path::new("/root");
        let candidates = [candidate("b"), candidate("group/bb")];
        let hit = |path: &str| containing_candidate(root, &candidates, path).map(|c| c.rel_path.as_str());
        assert_eq!(hit("group\\bb\\x.go"), Some("group/bb"));
        assert_eq!(hit(".\\b\\src\\x.ts"), Some("b"));
        assert_eq!(hit("group/bb\\x.go"), Some("group/bb"), "mixed separators");
        assert_eq!(hit("group\\b\\x.go"), None, "still whole segments");
    }

    /// The root is canonical; a file path spelling it differently (here a
    /// symlink, on Windows the `\\?\` prefix `canonicalize` adds to the root
    /// but no client sends) must still land inside it.
    #[cfg(unix)]
    #[test]
    fn an_absolute_path_through_another_spelling_of_the_root_is_matched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("group").join("bb")).unwrap();
        std::fs::write(root.join("group").join("bb").join("x.go"), "").unwrap();
        let root = root.canonicalize().unwrap();
        std::os::unix::fs::symlink(&root, dir.path().join("link")).unwrap();
        let candidates = [candidate("group/bb")];
        let file = dir.path().join("link").join("group").join("bb").join("x.go");
        let hit = containing_candidate(&root, &candidates, file.to_str().unwrap());
        assert_eq!(hit.map(|c| c.rel_path.as_str()), Some("group/bb"));
    }

    /// The Windows shape of the test above: the served root carries the
    /// verbatim prefix, the client's path does not.
    #[cfg(windows)]
    #[test]
    fn an_absolute_path_without_the_verbatim_prefix_is_matched() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("root");
        std::fs::create_dir_all(plain.join("group").join("bb")).unwrap();
        std::fs::write(plain.join("group").join("bb").join("x.go"), "").unwrap();
        let root = plain.canonicalize().unwrap();
        let candidates = [candidate("group/bb")];
        let file = plain.join("group").join("bb").join("x.go");
        let hit = containing_candidate(&root, &candidates, file.to_str().unwrap());
        assert_eq!(hit.map(|c| c.rel_path.as_str()), Some("group/bb"));
    }

    #[test]
    fn a_project_is_found_by_its_relative_path() {
        let root = Path::new("/root");
        let candidates = [candidate("a"), candidate("group/c")];
        let hit = |project: &str| find_candidate(root, &candidates, project).map(|c| c.rel_path.as_str());
        assert_eq!(hit("group/c/"), Some("group/c"));
        assert_eq!(hit("./a"), Some("a"));
        assert_eq!(hit("group\\c"), Some("group/c"));
        assert_eq!(hit(".\\group/c\\"), Some("group/c"), "mixed separators");
        assert_eq!(hit("c"), None);
    }

    /// A path with a root is matched below the served root by its segments,
    /// without touching the filesystem (`/root` does not exist). On Windows
    /// `/root/group/c` has a root but no drive, so it is not absolute: this
    /// is the shape an `is_absolute` branch sent down the relative path.
    #[test]
    fn a_project_is_found_by_a_rooted_path_under_the_root() {
        let root = Path::new("/root");
        let candidates = [candidate("a"), candidate("group/c")];
        let hit = |project: &str| find_candidate(root, &candidates, project).map(|c| c.rel_path.as_str());
        assert_eq!(hit("/root/group/c"), Some("group/c"));
        assert_eq!(hit("/root/a/"), Some("a"));
        assert_eq!(hit("/root/group\\c"), Some("group/c"), "mixed separators below the root");
        assert_eq!(hit("/root/c"), None, "whole segments, not a suffix");
        assert_eq!(hit("/root/group"), None, "a directory above a candidate is not one");
        assert_eq!(hit("/elsewhere/group/c"), None);
    }

    /// The drive-letter shape of the test above; a drive only means one on
    /// Windows.
    #[cfg(windows)]
    #[test]
    fn a_project_is_found_by_a_drive_letter_path_under_the_root() {
        let root = Path::new(r"C:\root");
        let candidates = [candidate("a"), candidate("group/c")];
        let hit = |project: &str| find_candidate(root, &candidates, project).map(|c| c.rel_path.as_str());
        assert_eq!(hit(r"C:\root\group\c"), Some("group/c"));
        assert_eq!(hit("C:/root/group/c"), Some("group/c"));
        assert_eq!(hit(r"C:\root\a\"), Some("a"));
        assert_eq!(hit(r"D:\root\group\c"), None);
    }

    /// The root is canonical; a project path spelling it differently (here a
    /// symlink) is retried canonicalized. The candidates' `abs_path` is the
    /// fake `/root/...`, so only the match below the root can succeed.
    #[cfg(unix)]
    #[test]
    fn a_project_through_another_spelling_of_the_root_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("group").join("c")).unwrap();
        let root = root.canonicalize().unwrap();
        std::os::unix::fs::symlink(&root, dir.path().join("link")).unwrap();
        let candidates = [candidate("group/c")];
        let project = dir.path().join("link").join("group").join("c");
        let hit = find_candidate(&root, &candidates, project.to_str().unwrap());
        assert_eq!(hit.map(|c| c.rel_path.as_str()), Some("group/c"));
    }

    /// The Windows shape of the test above: the served root carries the
    /// verbatim prefix `canonicalize` adds, the client's path does not.
    #[cfg(windows)]
    #[test]
    fn a_project_without_the_verbatim_prefix_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("root");
        std::fs::create_dir_all(plain.join("group").join("c")).unwrap();
        let root = plain.canonicalize().unwrap();
        let candidates = [candidate("group/c")];
        let project = plain.join("group").join("c");
        let hit = find_candidate(&root, &candidates, project.to_str().unwrap());
        assert_eq!(hit.map(|c| c.rel_path.as_str()), Some("group/c"));
    }

    /// A rooted path below the root whose segments name no candidate (a
    /// symlink inside the root, a `..`) still resolves to the candidate its
    /// canonical form is.
    #[cfg(unix)]
    #[test]
    fn a_project_whose_segments_differ_is_found_by_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("group").join("c")).unwrap();
        let root = root.canonicalize().unwrap();
        std::os::unix::fs::symlink(root.join("group").join("c"), root.join("alias")).unwrap();
        let candidates = [Candidate {
            rel_path: "group/c".to_string(),
            abs_path: root.join("group").join("c"),
            markers: vec![".git"],
            is_worktree: false,
        }];
        let hit = |project: PathBuf| {
            find_candidate(&root, &candidates, project.to_str().unwrap()).map(|c| c.rel_path.as_str())
        };
        assert_eq!(hit(root.join("alias")), Some("group/c"));
        assert_eq!(hit(root.join("group").join("..").join("group").join("c")), Some("group/c"));
    }
}
