//! `g-mesh deps --json` driven as a subprocess against temp projects, with the
//! real TypeScript, Rust, Python and Go plugins `cargo build --workspace` builds
//! (format and exit codes: `docs/cli/deps-json.md`).
//!
//! The edges are checked against `get_dependencies` over a real shim for the
//! same files, not against a stored copy of the output.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};

use crate::common;
use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// What the shim prints when it starts a daemon: present only on a cold run.
const COLD_START_LINE: &str = "starting a daemon";

/// One TypeScript, one Rust, one Python and one Go corner of one project.
const FILES: [(&str, &str); 17] = [
    // TS: a file import, a package, a missing file, and `.` with no index file.
    (
        "web/a.ts",
        "import { b } from \"./b\";\nimport React from \"react\";\nimport { m } from \"./missing\";\n\
         import d from \".\";\nexport const a = b + m + d + String(React);\n",
    ),
    ("web/b.ts", "export const b = 1;\n"),
    // Rust: package `app` with a lib and a bin; the bin's `mod config; use
    // crate::config::X;`, `use super::X;` in its nested module, and `std`.
    ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("src/lib.rs", "pub fn lib_fn() {}\n"),
    ("src/main.rs", "mod config;\nuse crate::config::X;\nuse std::fs;\nfn main() {\n    let _ = X;\n    let _ = fs::read(\"x\");\n}\n"),
    ("src/config.rs", "pub mod inner;\npub struct X;\n"),
    ("src/config/inner.rs", "use super::X;\n"),
    // Python: an empty `__init__.py`, a sibling module, a builtin, and a
    // namespace package `ns` imported from outside and from inside itself.
    ("pkg/__init__.py", ""),
    ("pkg/config.py", "VALUE = 1\n"),
    ("pkg/cli.py", "from pkg import config\nimport os\nimport pkg\nimport ns\n"),
    ("ns/a.py", "import ns\nX = 1\n"),
    ("ns/b.py", "Y = 2\n"),
    ("main.py", "import ns\n"),
    // Go: package `lib` is defined by two files in one directory.
    ("go.mod", "module example.com/app\n\ngo 1.22\n"),
    ("lib/a.go", "package lib\n\nfunc A() int { return 1 }\n"),
    ("lib/b.go", "package lib\n\nfunc B() int { return 2 }\n"),
    ("main.go", "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/app/lib\"\n)\n\nfunc main() { fmt.Println(lib.A() + lib.B()) }\n"),
];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn with(files: &[(&str, &str)]) -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (rel, contents) in files {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn canonical(&self) -> String {
        self.root().canonicalize().unwrap().display().to_string()
    }

    fn hold(&self) -> PathBuf {
        self.dir.path().with_extension("hold")
    }

    fn daemon_pid(&self) -> Option<u32> {
        daemon::read_pid_file(&daemon::pid_path(self.root()).unwrap())
    }

    fn live_daemon_pid(&self) -> u32 {
        let pid = self.daemon_pid().expect("a daemon pid file must exist");
        assert!(daemon::is_process_alive(pid), "the daemon {pid} must be running");
        pid
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.hold());
        for path in
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())].into_iter().flatten()
        {
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A folder of three projects: `deps` must refuse it.
fn folder_of_projects() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a temp folder");
    for (path, contents) in [
        ("a/a.ts", "export const a = 1;\n"),
        ("b/b.ts", "export const b = 2;\n"),
        ("c/go.mod", "module example.com/c\n"),
    ] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    std::fs::create_dir_all(dir.path().join("a/.git")).unwrap();
    std::fs::create_dir_all(dir.path().join("b/.git")).unwrap();
    dir
}

/// `g-mesh <args>` in `cwd`, with no `$CLAUDE_PROJECT_DIR` unless `env` sets one.
fn command(cwd: &Path, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> StdCommand {
    let mut cmd = StdCommand::new(BIN);
    cmd.lifeline()
        .args(args)
        .current_dir(cwd)
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        // No real model: keeps the embedding pass a no-op.
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

fn run(cwd: &Path, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Output {
    command(cwd, args, env).output().expect("failed to run g-mesh")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "expected exit {code}; stderr:\n{}\nstdout:\n{}",
        stderr(output),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn document(output: &Output) -> Value {
    assert_exit(output, 0);
    assert!(output.stdout.ends_with(b"}\n"), "stdout is one JSON document and a newline");
    serde_json::from_slice(&output.stdout).expect("stdout is not one JSON document")
}

fn edges_of(doc: &Value) -> &Vec<Value> {
    doc["edges"].as_array().expect("edges is not an array")
}

fn edge(from: &str, to: &str, kind: &str, language: &str, module: Option<&str>) -> Value {
    json!({ "from": from, "to": to, "kind": kind, "language": language, "module": module })
}

/// The ids of the index's container nodes: what tells a `get_dependencies`
/// container row from an external or unresolved placeholder row, which look
/// the same in its answer.
fn container_ids(root: &Path) -> BTreeSet<String> {
    let db = project_dir(root).unwrap().join("index.db");
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("failed to open the index read-only");
    let mut stmt = conn.prepare("SELECT nodeId FROM containers").unwrap();
    stmt.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

/// For each file, `get_dependencies {direction: Outgoing, max_depth: 1}`
/// split into what resolved (a File row's `filePath`, a container row's
/// `qualifiedName`) and how many placeholder rows it has.
async fn get_dependencies(root: &Path, files: &[&str]) -> BTreeMap<String, (BTreeSet<String>, usize)> {
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true).arg("mcp-shim").current_dir(root).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    let containers = container_ids(root);
    let mut answers = BTreeMap::new();
    for file in files {
        let args = json!({ "file_path": file, "direction": "Outgoing", "max_depth": 1 });
        let result = client
            .call_tool(
                CallToolRequestParams::new("get_dependencies")
                    .with_arguments(args.as_object().cloned().unwrap()),
            )
            .await
            .expect("tools/call failed");
        assert_ne!(result.is_error, Some(true), "get_dependencies {file}: {:?}", result.content);
        let body: Value = match &result.content[0] {
            ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
            other => panic!("expected text content, got {other:?}"),
        };
        assert_eq!(body["truncated"], false, "{file}: {body}");
        let mut resolved = BTreeSet::new();
        let mut placeholders = 0;
        for row in body["results"].as_array().unwrap() {
            if row["kind"] == "File" {
                resolved.insert(row["filePath"].as_str().unwrap().to_string());
            } else if containers.contains(row["id"].as_str().unwrap()) {
                resolved.insert(row["qualifiedName"].as_str().unwrap().to_string());
            } else {
                placeholders += 1;
            }
        }
        answers.insert(file.to_string(), (resolved, placeholders));
    }
    client.cancel().await.expect("failed to shut the client down");
    answers
}

#[test]
fn help_exits_0_and_deps_without_json_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();

    let help = run(dir.path(), &["deps", "--help"], &[]);
    assert_exit(&help, 0);
    let text = String::from_utf8_lossy(&help.stdout);
    for flag in ["--json", "--timeout", "--project"] {
        assert!(text.contains(flag), "`deps --help` must mention {flag}:\n{text}");
    }

    let bare = run(dir.path(), &["deps"], &[]);
    assert_exit(&bare, 2);
    assert!(bare.stdout.is_empty(), "a usage error prints no document");
    assert!(stderr(&bare).contains("--json"), "the error names the missing flag: {}", stderr(&bare));
    assert!(daemon::read_pid_file(&daemon::pid_path(dir.path()).unwrap()).is_none(), "no daemon was started");
}

#[test]
fn a_folder_of_projects_or_a_missing_directory_exits_3_and_starts_no_daemon() {
    let folder = folder_of_projects();
    let output = run(folder.path(), &["deps", "--json"], &[]);
    assert_exit(&output, 3);
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("is a folder of"), "{}", stderr(&output));
    assert!(!daemon::pid_path(folder.path()).unwrap().exists(), "no daemon may be started for a folder");
    assert!(!daemon::pid_path(&folder.path().canonicalize().unwrap()).unwrap().exists());

    let missing = folder.path().join("does-not-exist");
    let output = run(folder.path(), &["deps", "--json", "--project", missing.to_str().unwrap()], &[]);
    assert_exit(&output, 3);
    assert!(output.stdout.is_empty());
}

/// The acceptance test: a cold project bootstraps and prints; the edges are
/// `get_dependencies`'s answers for the same files; a rerun is byte-identical.
#[tokio::test]
async fn deps_on_a_cold_ts_rust_python_project_matches_get_dependencies_and_reruns_byte_identically() {
    let project = Project::with(&FILES);
    let folder = folder_of_projects();
    assert!(project.daemon_pid().is_none(), "the project starts cold");

    // `$CLAUDE_PROJECT_DIR` names the calling session's project and is
    // ignored: honoured, it would name a folder of projects (exit 3).
    let first = run(
        project.root(),
        &["deps", "--json"],
        &[(g_mesh::shim::PROJECT_DIR_ENV, folder.path().as_os_str())],
    );
    let doc = document(&first);
    assert!(stderr(&first).contains(COLD_START_LINE), "a cold run bootstraps a daemon: {}", stderr(&first));
    let pid = project.live_daemon_pid();

    // `--project` is honoured over a cwd that is itself not a project.
    let second = run(folder.path(), &["deps", "--json", "--project", project.root().to_str().unwrap()], &[]);
    assert_exit(&second, 0);
    assert!(!stderr(&second).contains(COLD_START_LINE), "the daemon is reused: {}", stderr(&second));
    assert_eq!(project.live_daemon_pid(), pid);
    assert!(first.stdout == second.stdout, "two runs over an unchanged index must print the same bytes");

    // --- the document ----------------------------------------------------
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["project_root"], project.canonical());
    let languages: Vec<&str> =
        doc["languages"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    for language in ["go", "python", "rust", "typescript"] {
        assert!(languages.contains(&language), "{language} must be indexed: {doc}");
        assert_eq!(doc["coverage"][language]["status"], "indexed", "{doc}");
    }
    let mut sorted_languages = languages.clone();
    sorted_languages.sort_unstable();
    assert_eq!(languages, sorted_languages, "languages are sorted");

    let edges = edges_of(&doc);
    let key = |e: &Value| {
        (
            e["from"].as_str().unwrap().to_string(),
            e["to"].as_str().unwrap().to_string(),
            e["kind"].as_str().unwrap().to_string(),
        )
    };
    let keys: Vec<_> = edges.iter().map(key).collect();
    let mut sorted_keys = keys.clone();
    sorted_keys.sort();
    assert_eq!(keys, sorted_keys, "edges are sorted by (from, to, kind)");
    for e in edges {
        for end in ["from", "to"] {
            let path = e[end].as_str().unwrap_or_else(|| panic!("`{end}` must be a string: {e}"));
            assert!(
                !path.starts_with('/')
                    && !path.starts_with("./")
                    && !path.contains('\\')
                    && !path.ends_with('/'),
                "paths are project-relative with `/`: {e}"
            );
        }
    }

    // TypeScript: a file-to-file edge; `react` external; `./missing` and `.`
    // name nothing in the project, so they are unresolved, not external.
    assert!(edges.contains(&edge("web/a.ts", "web/b.ts", "file", "typescript", None)), "{doc}");
    assert_eq!(doc["coverage"]["typescript"]["external"], 1, "{doc}");
    assert_eq!(doc["coverage"]["typescript"]["unresolved"], 2, "{doc}");
    assert_eq!(doc["unresolved"]["typescript"], 2, "{doc}");
    // Rust: the bin's container `app[bin]::config` maps back to the file
    // defining it, from the bin's root and from its nested module.
    assert!(
        edges.contains(&edge("src/main.rs", "src/config.rs", "file", "rust", Some("app[bin]::config"))),
        "{doc}"
    );
    assert!(
        edges.contains(&edge(
            "src/config/inner.rs",
            "src/config.rs",
            "file",
            "rust",
            Some("app[bin]::config")
        )),
        "{doc}"
    );
    assert_eq!(doc["coverage"]["rust"]["external"], 1, "`std`: {doc}");
    // Python: `import pkg` with an empty `__init__.py` maps to that file; a
    // namespace package is its directory, except from inside it.
    assert!(
        edges.contains(&edge("pkg/cli.py", "pkg/config.py", "file", "python", Some("pkg.config"))),
        "{doc}"
    );
    assert!(edges.contains(&edge("pkg/cli.py", "pkg/__init__.py", "file", "python", Some("pkg"))), "{doc}");
    assert!(edges.contains(&edge("main.py", "ns", "dir", "python", Some("ns"))), "{doc}");
    assert!(
        !edges.iter().any(|e| e["from"] == "ns/a.py"),
        "no edge onto the importer's own directory: {doc}"
    );
    // Go: a package defined by several files in one directory is that directory.
    assert!(edges.contains(&edge("main.go", "lib", "dir", "go", Some("example.com/app/lib"))), "{doc}");
    assert_eq!(doc["coverage"]["go"]["external"], 1, "`fmt`: {doc}");

    // --- against get_dependencies -----------------------------------------
    let code: Vec<&str> = FILES
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| [".ts", ".rs", ".py", ".go"].iter().any(|ext| path.ends_with(ext)))
        .collect();
    let answers = get_dependencies(project.root(), &code).await;
    let mut placeholders: BTreeMap<String, usize> = BTreeMap::new();
    for (file, (expected, placeholder_rows)) in &answers {
        let ours: BTreeSet<String> = edges
            .iter()
            .filter(|e| e["from"] == file.as_str())
            .map(|e| e["module"].as_str().unwrap_or_else(|| e["to"].as_str().unwrap()).to_string())
            .collect();
        if file == "ns/a.py" {
            assert_eq!(*expected, BTreeSet::from(["ns".to_string()]), "get_dependencies for {file}");
            assert!(ours.is_empty(), "{file}'s own package directory is dropped: {ours:?}");
        } else {
            assert_eq!(&ours, expected, "deps edges for {file} must match get_dependencies");
        }
        let language = match Path::new(file).extension().and_then(|e| e.to_str()) {
            Some("ts") => "typescript",
            Some("rs") => "rust",
            Some("go") => "go",
            _ => "python",
        };
        *placeholders.entry(language.to_string()).or_default() += placeholder_rows;
    }
    for (language, rows) in &placeholders {
        let coverage = &doc["coverage"][language];
        let counted = coverage["external"].as_u64().unwrap() + coverage["unresolved"].as_u64().unwrap();
        assert_eq!(counted, *rows as u64, "{language}: placeholder rows vs external + unresolved: {doc}");
    }
}

#[test]
fn nothing_indexed_exits_0_with_no_languages() {
    let project = Project::with(&[("a.ts", "export const a = 1;\n")]);
    let no_plugins = tempfile::tempdir().unwrap();
    let output = run(
        project.root(),
        &["deps", "--json"],
        &[(g_mesh::daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, no_plugins.path().as_os_str())],
    );
    let doc = document(&output);
    assert_eq!(doc["languages"], json!([]), "{doc}");
    assert_eq!(doc["coverage"], json!({ "typescript": { "status": "plugin_absent", "files": 1 } }), "{doc}");
    assert_eq!(doc["unresolved"], json!({}), "{doc}");
    assert_eq!(doc["edges"], json!([]), "{doc}");
    assert!(stderr(&output).contains("typescript"), "stderr names the missing plugin: {}", stderr(&output));
}

#[test]
fn a_failed_walk_exits_4_with_nothing_on_stdout() {
    let project = Project::with(&[("a.py", "X = 1\n")]);
    let (plugin_root, _binary) = common::missing_workspace_binary_plugin_root();
    let output = run(
        project.root(),
        &["deps", "--json"],
        &[(g_mesh::daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, plugin_root.path().as_os_str())],
    );
    assert_exit(&output, 4);
    assert!(output.stdout.is_empty(), "no document on failure");
    let text = stderr(&output);
    assert!(text.contains("is unavailable"), "{text}");
    assert!(text.contains("has not been built yet"), "the walk's own error is passed on: {text}");
}

/// A walk held open past `--timeout` exits 5 with nothing on stdout and
/// leaves the daemon indexing; a timeout too large for an instant is no limit.
#[test]
fn a_timeout_exits_5_and_leaves_the_daemon_indexing() {
    let project =
        Project::with(&[("a.ts", "export const a = 1;\n"), ("b.ts", "import { a } from \"./a\";\n")]);
    std::fs::write(project.hold(), b"").unwrap();

    // Bootstraps the daemon with its walk held, and waits with no limit.
    let mut waiting: Child = command(
        project.root(),
        &["deps", "--json", "--timeout", "0"],
        &[(g_mesh::daemon::bulk_index::WALK_HOLD_FILE_ENV, project.hold().as_os_str())],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("failed to spawn g-mesh deps");
    common::wait_for("the held daemon to start", common::startup_timeout(), || {
        project.daemon_pid().is_some_and(daemon::is_process_alive)
    });
    let pid = project.live_daemon_pid();

    let timed_out = run(project.root(), &["deps", "--json", "--timeout", "1"], &[]);
    assert_exit(&timed_out, 5);
    assert!(timed_out.stdout.is_empty(), "no document on a timeout");
    assert!(stderr(&timed_out).contains("still being built"), "{}", stderr(&timed_out));
    assert_eq!(project.live_daemon_pid(), pid, "the daemon keeps indexing after a timeout");

    std::fs::remove_file(project.hold()).unwrap();
    let mut status = None;
    common::wait_for("the waiting deps to finish", common::startup_timeout(), || {
        status = waiting.try_wait().unwrap();
        status.is_some()
    });
    assert_eq!(status.unwrap().code(), Some(0), "the walk finished once released");
    let waited = waiting.wait_with_output().unwrap();
    let waited: Value = serde_json::from_slice(&waited.stdout).expect("the waiting run printed a document");
    assert!(edges_of(&waited).contains(&edge("b.ts", "a.ts", "file", "typescript", None)), "{waited}");

    let unbounded = run(project.root(), &["deps", "--json", "--timeout", &u64::MAX.to_string()], &[]);
    let doc = document(&unbounded);
    assert_eq!(doc, waited, "the same index, the same document");
    assert_eq!(project.live_daemon_pid(), pid, "the same daemon answered");
}

/// A short timeout on a cold project still gets the walk started: the run
/// exits 5 only once its activation request reached the daemon.
#[test]
fn a_short_timeout_on_a_cold_project_exits_5_after_starting_the_walk() {
    let project = Project::with(&[("a.ts", "export const a = 1;\n")]);
    // The walk is held, so the index cannot be ready within any timeout.
    std::fs::write(project.hold(), b"").unwrap();
    assert!(project.daemon_pid().is_none(), "the project starts cold");

    let output = run(
        project.root(),
        &["deps", "--json", "--timeout", "1"],
        &[(g_mesh::daemon::bulk_index::WALK_HOLD_FILE_ENV, project.hold().as_os_str())],
    );
    assert_exit(&output, 5);
    assert!(output.stdout.is_empty(), "no document on a timeout");
    assert!(stderr(&output).contains("The daemon keeps indexing"), "{}", stderr(&output));

    let phase = daemon::phase_path_in(&project_dir(project.root()).unwrap());
    common::wait_for("the index phase to leave `unindexed`", common::startup_timeout(), || {
        std::fs::read_to_string(&phase)
            .is_ok_and(|phase| !phase.trim().is_empty() && phase.trim() != "unindexed")
    });
    project.live_daemon_pid();
}
