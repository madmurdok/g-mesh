//! A full index plus semantic pass over a Python project reaches no network
//! and writes nothing under the project root
//! (`docs/design/GM-512-plugins-no-writes-no-network.md` §3, ADR 0033).
//!
//! The fixture is `pfx`: `main.py` imports `vpkg` from the project's
//! `.venv` site-packages, and a project `.npmrc` names a registry that is a
//! local sink logging every connection and answering 403. Every proxy
//! variable npm reads points at the sink too. The sanity step runs
//! `npx --yes --package pyright pyright --version` in a copy with an empty
//! npm cache and checks it reaches the sink: an `npx`-started pyright would
//! go to the network the project chose.
//!
//! The pass runs through the plugin's own [`semantic::engine`] with the
//! shipped manifest. Then `made.method()` must resolve through the venv (the
//! server really answered), the root's snapshot - every path with its type,
//! size and content hash - must be unchanged, and the sink must have seen no
//! connection.
//!
//! This is a pin: the plugin starts pyright from `PATH` only and never `npx`
//! (`semantic_runs_no_project_code.rs` holds the control for that), and
//! pyright itself opens no connection and writes nothing; no code here
//! prevents either.
//!
//! Unix only, like `semantic_runs_no_project_code.rs`. One test in this binary,
//! because it rewrites the process environment.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use g_mesh_plugin_python::extractor::PythonExtractor;
use g_mesh_plugin_python::project::{ProjectContext, EXCLUDE_DIRS};
use g_mesh_plugin_python::semantic;
use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{walk_project, Extractor, SdkIndex, MANIFEST_PATH_ENV};

const MAIN_PY: &str = "from vpkg import vfunc\n\n\ndef use():\n    made = vfunc()\n    made.method()\n";
const LIB_PY: &str = "class Thing:\n    def method(self):\n        return 1\n";
/// `vfunc`'s return type is a project class, so `made.method()` resolves
/// only if pyright found `vpkg` in the venv.
const VPKG_PY: &str = "from lib import Thing\n\n\ndef vfunc() -> Thing:\n    return Thing()\n";

/// This crate's own pyright install (`scripts/test-deps.sh pyright`).
fn installed_bin() -> PathBuf {
    let bin = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    assert!(
        bin.join("pyright").is_file() && bin.join("pyright-langserver").is_file(),
        "this test drives a real pyright and {} has none: install it with `scripts/test-deps.sh pyright`",
        bin.display()
    );
    bin.to_path_buf()
}

/// The absolute path `program` resolves to on the current `PATH`.
fn absolute(program: &str) -> PathBuf {
    let output = Command::new("/bin/sh").arg("-c").arg(format!("command -v {program}")).output().unwrap();
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(output.status.success() && path.starts_with('/'), "this test needs `{program}` on PATH");
    PathBuf::from(path)
}

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-py-gm512-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Scratch(root.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A local listener that logs the first line of every connection and refuses
/// it with a 403, so each attempt is a log line and fails fast.
struct Sink {
    url: String,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Sink {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&lines);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                log.lock().unwrap().push(line.trim().to_string());
                let _ = (&stream)
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        Sink { url, lines }
    }

    /// The attempts logged so far, forgotten once returned.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }
}

/// Every path under a root with its type, size and content hash: mtime alone
/// is too coarse, and content alone misses a new empty directory.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                out.insert(rel, format!("link {}", std::fs::read_link(&path).unwrap().display()));
            } else if kind.is_dir() {
                out.insert(rel, "dir".to_string());
                walk(root, &path, out);
            } else {
                let content = std::fs::read(&path).unwrap();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                content.hash(&mut hasher);
                out.insert(rel, format!("file {} {:016x}", content.len(), hasher.finish()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Every path added, removed or changed between two snapshots.
fn changes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = before
        .iter()
        .filter_map(|(path, was)| match after.get(path) {
            None => Some(format!("removed {path}")),
            Some(now) if now != was => Some(format!("changed {path}")),
            Some(_) => None,
        })
        .collect();
    out.extend(after.keys().filter(|path| !before.contains_key(*path)).map(|path| format!("added {path}")));
    out
}

/// `pfx` with a venv made by `python3 -m venv`, plus the project `.npmrc`.
fn write_fixture(root: &Path, python: &Path, registry: &str) {
    write(&root.join("main.py"), MAIN_PY);
    write(&root.join("lib.py"), LIB_PY);
    write(&root.join(".npmrc"), &format!("registry={registry}/evil-registry/\n"));
    let output = Command::new(python)
        .args(["-m", "venv", "--without-pip", ".venv"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "python3 -m venv: {}", String::from_utf8_lossy(&output.stderr));
    let site_packages = std::fs::read_dir(root.join(".venv/lib"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join("site-packages"))
        .find(|path| path.is_dir())
        .expect("the venv has a site-packages");
    write(&site_packages.join("vpkg/__init__.py"), VPKG_PY);
}

#[test]
fn indexing_reaches_no_network_and_writes_nothing_under_the_root() {
    let installed = installed_bin();
    let python = absolute("python3");
    let node_dir = absolute("node").parent().unwrap().to_path_buf();
    let npx = absolute("npx");
    let scratch = Scratch::new();
    let sink = Sink::start();

    std::env::set_var(
        "PATH",
        std::env::join_paths([installed, node_dir, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
            .unwrap(),
    );
    for name in std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()).filter(|name| {
        let lower = name.to_ascii_lowercase();
        lower.starts_with("npm_config_") || lower == "no_proxy" || lower == "all_proxy"
    }) {
        std::env::remove_var(name);
    }
    for name in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "http_proxy",
        "npm_config_proxy",
        "npm_config_https_proxy",
    ] {
        std::env::set_var(name, &sink.url);
    }
    std::env::set_var("npm_config_cache", scratch.0.join("npm-cache"));
    std::env::set_var("npm_config_update_notifier", "false");
    std::env::set_var(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    std::env::remove_var("VIRTUAL_ENV");

    // Sanity: in this environment, `npx` in the project fetches from the
    // registry the project's .npmrc names.
    let copy = scratch.0.join("copy");
    write_fixture(&copy, &python, &sink.url);
    let output = Command::new(&npx)
        .args(["--yes", "--package", "pyright", "pyright", "--version"])
        .current_dir(&copy)
        .output()
        .unwrap();
    let attempts = sink.take();
    assert!(
        attempts.iter().any(|line| line.contains("/evil-registry/")),
        "sanity: npx in the project did not ask the project's registry (attempts {attempts:?}): {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let root = scratch.0.join("project");
    write_fixture(&root, &python, &sink.url);
    std::env::set_current_dir(&root).unwrap();
    let before = snapshot(&root);

    let project = ProjectContext::load(&root).expect("the fixture is a loadable Python project");
    let mut index = SdkIndex::new();
    for path in walk_project(&root, &[".py".to_string()], &EXCLUDE_DIRS.map(String::from)) {
        let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
        let graph = PythonExtractor.extract(&project, &path, &source);
        index.insert(path, source, graph);
    }
    let mut engine = semantic::engine(&root).expect("the engine resolves a pyright");
    let answer = engine.answer(&[], &index).expect("the engine answers");
    drop(engine);

    let from = index
        .files()
        .flat_map(|(_, entry)| entry.graph.nodes.iter())
        .find(|node| node.name == "use")
        .expect("the extractor emits `use`")
        .id
        .clone();
    let resolved: Vec<&str> = answer
        .diff
        .upsert_edges
        .iter()
        .filter(|edge| edge.from_id == from && edge.source == SourceTier::Semantic)
        .filter_map(|edge| answer.diff.upsert_nodes.iter().find(|node| node.id == edge.to_id))
        .map(|node| node.name.as_str())
        .collect();
    assert!(
        resolved.contains(&"method"),
        "sanity: pyright did not resolve `made.method()` through the venv's `vpkg` (complete: {}, reason: \
         {:?}): {:#?}",
        answer.complete,
        answer.reason,
        answer.diff
    );
    assert_eq!(sink.take(), Vec::<String>::new(), "the semantic pass made network connections");
    assert_eq!(
        changes(&before, &snapshot(&root)),
        Vec::<String>::new(),
        "the semantic pass wrote under the root"
    );
}
