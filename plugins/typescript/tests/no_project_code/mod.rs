//! The hostile TypeScript project `fx` of
//! `docs/design/GM-566-typescript-no-project-code.md` and the harness shared
//! by `semantic_runs_no_project_code.rs` and
//! `indexing_writes_nothing_and_stays_offline.rs`: logging shims, a counting
//! network sink, the process environment the tests run the plugin under, and
//! one whole-project pass through the plugin's own [`semantic::engine`].
//!
//! Unix only: the shims are shell scripts.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{
    walk_project, Extractor, SdkIndex, SemanticAnswer, SemanticEngine, MANIFEST_PATH_ENV,
};
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::EXCLUDE_DIRS;
use g_mesh_plugin_typescript::semantic;

/// `src/a.ts` imports through the tsconfig `paths` alias, so `made.method()`
/// resolves to `src/lib/b.ts` only if this project's tsconfig is the one in
/// force.
pub const A_TS: &str =
    "import { make } from \"@lib/b\";\n\nexport function useIt(): number {\n  const made = \
                        make();\n  return made.method();\n}\n";
pub const B_TS: &str =
    "export class Thing {\n  method(): number {\n    return 1;\n  }\n}\n\nexport function \
                        make(): Thing {\n  return new Thing();\n}\n";
/// Outside the tsconfig's `include`, so tsserver gives it an inferred
/// JavaScript project, and it requires a package `package.json` depends on:
/// the automatic type acquisition trigger.
pub const LOOSE_JS: &str =
    "const _ = require(\"lodash\");\n\nfunction chunked() {\n  return _.chunk([1, 2], 1);\n}\n\nmodule.exports \
     = chunked;\n";
pub const TSCONFIG: &str = "{ \"compilerOptions\": { \"strict\": true, \"baseUrl\": \".\", \"paths\": { \
                            \"@lib/*\": [\"src/lib/*\"] },\n  \"plugins\": [{ \"name\": \"evil-ts-plugin\" }] }, \
                            \"include\": [\"src\"] }\n";

/// How long the server is kept alive after the pass for automatic type
/// acquisition to reach the network. With ATA on (control C4), tsserver
/// forks its typings installer when `loose.js`'s inferred project loads,
/// and that installer's first `npm install` reaches the sink a few seconds
/// later; the wait ends at the first connection, so only a passing run pays
/// all of it.
pub const ATA_WINDOW: Duration = Duration::from_secs(30);

/// One lock for every test in a binary: they rewrite `PATH`, `HOME` and the
/// working directory of this process.
pub static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// This crate's own vtsls install (`scripts/test-deps.sh typescript`).
pub fn installed_bin() -> PathBuf {
    let bin = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    assert!(
        bin.join("vtsls").is_file(),
        "these tests drive a real vtsls and {} has none: install it with `scripts/test-deps.sh typescript`",
        bin.display()
    );
    bin.to_path_buf()
}

/// The `PATH` this process started with, read before any test rewrites it.
static ORIGINAL_PATH: OnceLock<OsString> = OnceLock::new();

/// The absolute path `program` resolves to on the `PATH` this process
/// started with. Each test calls it before it rewrites `PATH`, so the first
/// call reads the original.
pub fn absolute(program: &str) -> PathBuf {
    let path = ORIGINAL_PATH.get_or_init(|| std::env::var_os("PATH").unwrap_or_default());
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .env("PATH", path)
        .output()
        .unwrap();
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(output.status.success() && path.starts_with('/'), "this test needs `{program}` on PATH");
    PathBuf::from(path)
}

/// A scratch directory that removes itself.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-ts-gm566-{name}-{}", std::process::id()));
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

pub fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// An executable script that appends its argv to `log` and then runs `exec`
/// (nothing more when `exec` is empty).
pub fn shim(path: &Path, log: &Path, exec: &str) {
    let _ = std::fs::remove_file(path);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    write(path, &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{exec}\n", log.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

pub fn run(program: &Path, args: &[&str], cwd: &Path) {
    let output = Command::new(program).args(args).current_dir(cwd).output().unwrap();
    assert!(
        output.status.success(),
        "{} {args:?}: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The logs that exist, with their contents.
pub fn written(logs: &[PathBuf]) -> Vec<String> {
    logs.iter()
        .filter_map(|log| std::fs::read_to_string(log).ok().map(|text| format!("{}:\n{text}", log.display())))
        .collect()
}

/// `PATH`, the shipped manifest, the working directory, and a `HOME`, npm
/// cache and typings cache inside `scratch` - so no npm or tsserver cache of
/// the user's answers in place of the network, and nothing is written to
/// their home. Every npm network variable is cleared; [`route_npm_to`]
/// sets them.
pub fn set_environment(path: &[PathBuf], cwd: &Path, scratch: &Path) {
    std::env::set_var("PATH", std::env::join_paths(path).unwrap());
    std::env::set_var(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    for name in std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()).filter(|name| {
        let lower = name.to_ascii_lowercase();
        lower.starts_with("npm_config_") || lower.ends_with("_proxy")
    }) {
        std::env::remove_var(name);
    }
    let home = scratch.join("home");
    std::fs::create_dir_all(home.join("cache")).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    std::env::set_var("npm_config_cache", home.join("npm-cache"));
    std::env::set_var("npm_config_update_notifier", "false");
    std::env::set_current_dir(cwd).unwrap();
}

/// Points npm's registry and every proxy variable npm reads at `sink`.
pub fn route_npm_to(sink: &Sink) {
    std::env::set_var("npm_config_registry", format!("{}/", sink.url));
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
}

/// A local listener that logs the first line of every connection and refuses
/// it with a 403, so each attempt is a log line and fails fast.
pub struct Sink {
    pub url: String,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Sink {
    pub fn start() -> Self {
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
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }

    /// The attempts logged within `window`, returning at the first one.
    pub fn take_within(&self, window: Duration) -> Vec<String> {
        let deadline = Instant::now() + window;
        loop {
            let seen = self.take();
            if !seen.is_empty() || Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Writes `fx` under `root`: the `paths` tsconfig naming the tsconfig plugin
/// `evil-ts-plugin`, that plugin's package (it writes `MARK_plugin` at the
/// root when `require`d), a project `typescript` - a copy of this crate's
/// own, whose `tsserver.js`, `_tsserver.js` and `typescript.js` each write
/// `MARK_ts_<file>` at the root when loaded - and `loose.js` with a
/// `package.json` that depends on `lodash`.
pub fn write_fx(root: &Path) {
    write(&root.join("tsconfig.json"), TSCONFIG);
    write(&root.join("src/a.ts"), A_TS);
    write(&root.join("src/lib/b.ts"), B_TS);
    write(&root.join("loose.js"), LOOSE_JS);
    write(
        &root.join("package.json"),
        "{ \"name\": \"fx\", \"version\": \"1.0.0\", \"dependencies\": { \"lodash\": \"*\" } }\n",
    );
    let plugin = root.join("node_modules/evil-ts-plugin");
    write(
        &plugin.join("package.json"),
        "{ \"name\": \"evil-ts-plugin\", \"version\": \"1.0.0\", \"main\": \"index.js\" }\n",
    );
    write(
        &plugin.join("index.js"),
        "require(\"fs\").writeFileSync(require(\"path\").join(__dirname, \"..\", \"..\", \"MARK_plugin\"), \"1\");\n\
         module.exports = function init() { return { create: (info) => info.languageService }; };\n",
    );
    let typescript = root.join("node_modules/typescript");
    let output = Command::new("cp")
        .arg("-R")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/typescript"))
        .arg(&typescript)
        .output()
        .unwrap();
    assert!(output.status.success(), "copying typescript: {}", String::from_utf8_lossy(&output.stderr));
    for file in ["tsserver.js", "_tsserver.js", "typescript.js"] {
        let path = typescript.join("lib").join(file);
        let original = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("this crate's typescript has lib/{file}: {err}"));
        std::fs::write(
            &path,
            format!(
                "require('fs').writeFileSync(require('path').join(__dirname, '..', '..', '..', 'MARK_ts_{file}'), \
                 '1');\n{original}"
            ),
        )
        .unwrap();
    }
}

/// The `MARK_*` files at the root: what the project's own code wrote when it
/// ran.
pub fn markers(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("MARK_"))
        .collect();
    found.sort();
    found
}

/// The structural index of every `.ts`/`.js` file under `root`.
pub fn index(root: &Path) -> SdkIndex {
    let project = TypeScriptExtractor.load_project(root).expect("the fixture is a loadable project");
    let mut index = SdkIndex::new();
    for path in walk_project(root, &[".ts".to_string(), ".js".to_string()], &EXCLUDE_DIRS.map(String::from)) {
        let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
        let graph = TypeScriptExtractor.extract(&project, &path, &source);
        index.insert(path, source, graph);
    }
    index
}

/// A whole-project pass through the plugin's own engine, with the server
/// still running when it returns.
pub fn pass(root: &Path, index: &SdkIndex) -> (Box<dyn SemanticEngine>, SemanticAnswer) {
    let mut engine = semantic::engine(root).expect("the engine resolves a vtsls");
    let answer = engine.answer(&[], index).expect("the engine answers");
    (engine, answer)
}

/// Sanity: the server answered, and through this project's tsconfig - the
/// semantic edge from `useIt` reaches `method`, which only resolves through
/// the `@lib/*` alias.
pub fn assert_the_tsconfig_was_in_force(index: &SdkIndex, answer: &SemanticAnswer) {
    let from = index
        .files()
        .flat_map(|(_, entry)| entry.graph.nodes.iter())
        .find(|node| node.name == "useIt")
        .expect("the extractor emits `useIt`")
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
        "sanity: vtsls did not resolve `made.method()` through the tsconfig's `@lib/*` alias (complete: {}, \
         reason: {:?}): {:#?}",
        answer.complete,
        answer.reason,
        answer.diff
    );
}

/// Sanity: in this environment the `npm` the typings installer runs - the
/// one beside `node` - asks the sink, so an ATA install would be seen.
pub fn assert_npm_reaches(sink: &Sink, node: &Path, cwd: &Path) {
    let npm = node.parent().unwrap().join("npm");
    let output =
        Command::new(&npm).args(["view", "types-registry", "version"]).current_dir(cwd).output().unwrap();
    let attempts = sink.take();
    assert!(
        !attempts.is_empty(),
        "sanity: {} did not ask the sink: {}{}",
        npm.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every path under a root with its type, size and content hash: mtime alone
/// is too coarse, and content alone misses a new empty directory.
pub fn snapshot(root: &Path) -> BTreeMap<String, String> {
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
pub fn changes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
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
