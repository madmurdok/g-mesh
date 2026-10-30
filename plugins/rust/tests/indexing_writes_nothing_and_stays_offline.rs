//! A full index plus semantic pass over a Rust project reaches no network and
//! writes nothing under the project root
//! (`docs/design/GM-512-plugins-no-writes-no-network.md` §3, ADR 0033).
//!
//! Two fixtures run in sequence through the plugin's own
//! [`semantic::engine`] (via `sandboxed_engine`), with the shipped
//! manifest, so the toolchain pin, the pinned environment and the
//! initialization options are the production ones:
//!
//! - **network**: `app` depends on `itoa = "1"` and on a git crate, with no
//!   `Cargo.lock` and an empty `CARGO_HOME`, so resolving the workspace needs
//!   the registry index and the git remote. Every proxy variable cargo reads
//!   points at a local sink that logs each connection and answers 403. The
//!   sanity step runs plain `cargo metadata` on a copy and checks it reaches
//!   the sink: without the plugin's offline setting, the pass would too.
//! - **writes**: a workspace whose crate has a `build.rs`, no dependencies and
//!   no `Cargo.lock`. Running build scripts (what rust-analyzer does when
//!   `cargo.buildScripts.enable` is on) writes `target/`; the sanity step
//!   checks that on a copy with `cargo check`.
//!
//! After each pass a receiver call across files must be a semantic edge (the
//! server really answered), the root's snapshot - every path with its type,
//! size and content hash - must be unchanged (no `target/`, no `Cargo.lock`),
//! and the sink must have seen no connection.
//!
//! The test process's environment is scrubbed of what cargo and rustup set
//! for it, as in `semantic_runs_no_project_code.rs`. That test is the only
//! one here that rewrites the process environment; the proxy test beside it
//! runs its check in a child process.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use g_mesh_plugin_rust::extractor::RustExtractor;
use g_mesh_plugin_rust::project::{ProjectContext, EXCLUDE_DIRS};
use g_mesh_plugin_rust::semantic;
use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{walk_project, Extractor, RelPath, SdkIndex, MANIFEST_PATH_ENV};

/// [`semantic::engine`] with the Linux sandbox trampoline run by the plugin
/// binary, since this test binary has no trampoline entry. Other OSes
/// ignore the launcher.
fn sandboxed_engine(root: &Path) -> anyhow::Result<Box<dyn g_mesh_plugin_sdk::SemanticEngine>> {
    semantic::engine_with(root, Path::new(env!("CARGO_BIN_EXE_g-mesh-plugin-rust")))
}

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-rust-gm512-{}", std::process::id()));
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

/// A local listener that logs the first line of every connection (a proxy
/// `CONNECT`, a proxy or registry `GET`) and refuses it with a 403, so each
/// attempt is a log line and fails fast.
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

fn rustup(args: &[&str]) -> String {
    let output = Command::new("rustup")
        .args(args)
        .current_dir(std::env::temp_dir())
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .unwrap_or_else(|error| panic!("this test needs rustup (rustup {args:?}: {error})"));
    assert!(output.status.success(), "rustup {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A directory of rustup proxies made in `dir`, after checking the default
/// toolchain has a working rust-analyzer.
///
/// `cargo`, `rustc` and `rust-analyzer` are linked to the install's own proxy
/// of that name beside the `rustup` on `PATH`, or to that `rustup` when there
/// is none. rustup runs as the proxy its `argv[0]` names, and a wrapper
/// script (Homebrew's keg `bin`) replaces `argv[0]` when it execs the real
/// binary, so a link to such a `rustup` would run as rustup whatever its
/// name. rustup-init's `~/.cargo/bin` and Homebrew's keg `bin` hold the
/// proxies; an install whose `rustup` stands alone gets links to that binary.
fn proxy_dir(dir: &Path) -> PathBuf {
    let default = rustup(&["default"]);
    let pin = default.split_whitespace().next().expect("`rustup default` names a toolchain").to_string();
    let rust_analyzer = PathBuf::from(rustup(&["which", "--toolchain", &pin, "rust-analyzer"]));
    assert!(
        Command::new(&rust_analyzer).arg("--version").output().is_ok_and(|o| o.status.success()),
        "{} does not answer --version: install it with `rustup component add rust-analyzer`",
        rust_analyzer.display()
    );
    let rustup = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(format!("rustup{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
        .expect("rustup is on PATH");
    std::fs::create_dir_all(dir).unwrap();
    for name in ["cargo", "rustc", "rust-analyzer"] {
        let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let proxy = dir.join(&file);
        let sibling = rustup.with_file_name(&file);
        let target = if sibling.is_file() { sibling } else { rustup.clone() };
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &proxy).unwrap();
        #[cfg(not(unix))]
        if std::fs::hard_link(&target, &proxy).is_err() {
            std::fs::copy(&target, &proxy).unwrap();
        }
    }
    dir.to_path_buf()
}

/// Set, to the fake install's directory, only in the child process of
/// [`proxies_beside_a_wrapper_script_rustup_run_as_their_tool`].
#[cfg(unix)]
const KEG_CHILD_ENV: &str = "G_MESH_RS_WRAPPER_KEG_CHILD";

/// A Homebrew-shaped rustup install in `keg`: `rustup`, `cargo`, `rustc` and
/// `rust-analyzer` are bash scripts that `exec` the same file beside the
/// `rustup` on `PATH`, so `argv[0]` reaches the real binary as its own path,
/// whatever name the script was started under. `false`, with the reason
/// printed, where bash or such an install of rustup is missing.
#[cfg(unix)]
fn wrapper_keg(test: &str, keg: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if !Command::new("bash").args(["-c", "true"]).output().is_ok_and(|o| o.status.success()) {
        println!("{test}: skipped, no bash to run wrapper scripts");
        return false;
    }
    let Some(host) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("rustup"))
        .find(|candidate| candidate.is_file())
    else {
        println!("{test}: skipped, no rustup on PATH");
        return false;
    };
    std::fs::create_dir_all(keg).unwrap();
    for name in ["rustup", "cargo", "rustc", "rust-analyzer"] {
        let real = host.with_file_name(name);
        if !real.is_file() {
            println!("{test}: skipped, {} has no {name} beside it", host.display());
            return false;
        }
        let script = keg.join(name);
        std::fs::write(&script, format!("#!/bin/bash\nexec \"{}\" \"$@\"\n", real.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    true
}

/// With a `rustup` that is a wrapper script first on `PATH`, each proxy
/// [`proxy_dir`] makes runs as the tool it is named after. A link to the
/// script itself would run as rustup: the script's `exec` hands rustup its
/// own path as `argv[0]`. `proxy_dir` reads this process's `PATH`, so the
/// check runs in a child process of this test binary with the fake install
/// first on its `PATH`, leaving this process's environment alone.
#[cfg(unix)]
#[test]
fn proxies_beside_a_wrapper_script_rustup_run_as_their_tool() {
    let test = "proxies_beside_a_wrapper_script_rustup_run_as_their_tool";
    let Some(keg) = std::env::var_os(KEG_CHILD_ENV) else {
        let scratch = std::env::temp_dir().join(format!("g-mesh-rust-wrapper-keg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let keg = scratch.join("keg");
        if !wrapper_keg(test, &keg) {
            let _ = std::fs::remove_dir_all(&scratch);
            return;
        }
        let path = std::env::join_paths(
            std::iter::once(keg.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture"])
            .env("PATH", path)
            .env(KEG_CHILD_ENV, &keg)
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&scratch);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(&format!("{test}: proxies checked")),
            "the child did not pass ({}):\n--- stdout\n{stdout}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let scratch = Scratch::new();
    let proxies = proxy_dir(&scratch.0.join("rustup-proxies"));
    for name in ["cargo", "rustc", "rust-analyzer"] {
        let output = Command::new(proxies.join(name))
            .arg("--version")
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.starts_with(&format!("{name} ")),
            "with {} first on PATH, the {name} proxy did not run as {name} ({}):\n--- stdout\n{stdout}\n--- stderr\n{}",
            Path::new(&keg).display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!("{test}: proxies checked");
}

/// Clears what cargo and rustup set for this test process, puts rustup's
/// proxies first on `PATH`, empties `CARGO_HOME` and routes every proxy
/// variable cargo reads to the sink.
fn set_environment(proxy_dir: &Path, cargo_home: &Path, sink: &Sink) {
    let names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            name.starts_with("CARGO")
                || name.starts_with("RUSTC")
                || name.starts_with("RUSTDOC")
                || name == "RUSTFLAGS"
                || name == "RUSTUP_TOOLCHAIN"
                || name.eq_ignore_ascii_case("no_proxy")
                || name.eq_ignore_ascii_case("all_proxy")
        })
        .collect();
    for name in names {
        std::env::remove_var(name);
    }
    let rest = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir != proxy_dir && !dir.to_string_lossy().contains("/toolchains/"))
        .collect::<Vec<_>>();
    let path = std::env::join_paths(std::iter::once(proxy_dir.to_path_buf()).chain(rest)).unwrap();
    std::env::set_var("PATH", path);
    std::env::set_var("CARGO_HOME", cargo_home);
    for name in ["CARGO_HTTP_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        std::env::set_var(name, &sink.url);
    }
    std::env::set_var("CARGO_NET_RETRY", "0");
    std::env::set_var(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
}

/// A workspace with crate `app`: `main` calls `thing.method()`, a receiver
/// call into `util.rs` that only the semantic tier resolves. `dependencies`
/// and `build_rs` vary per fixture.
fn write_fixture(root: &Path, dependencies: &str, build_rs: bool) {
    write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
    write(
        &root.join("app/Cargo.toml"),
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{dependencies}"
        ),
    );
    if build_rs {
        write(
            &root.join("app/build.rs"),
            "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
        );
    }
    write(
        &root.join("app/src/main.rs"),
        "mod util;\n\nfn main() {\n    let thing = util::Thing;\n    thing.method();\n}\n",
    );
    write(
        &root.join("app/src/util.rs"),
        "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
    );
}

const NETWORK_DEPENDENCIES: &str = "itoa = \"1\"\nanyhow = { git = \"https://github.com/dtolnay/anyhow\" }\n";

/// Runs `cargo args` in dir with the test's (the user's) environment and none
/// of the plugin's settings.
fn plain_cargo(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("cargo").args(args).current_dir(dir).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

/// The bulk index and a whole-project semantic pass over `root`, then the
/// three assertions.
fn index_and_check(root: &Path, sink: &Sink, what: &str) {
    std::env::set_current_dir(root).unwrap();
    let before = snapshot(root);

    let project = ProjectContext::load(root).expect("the fixture is a loadable Rust project");
    let mut index = SdkIndex::new();
    for path in walk_project(root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
        let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
        let graph = RustExtractor.extract(&project, &path, &source);
        index.insert(path, source, graph);
    }
    let mut engine = crate::sandboxed_engine(root).expect("the engine resolves a rust-analyzer");
    let answer = engine.answer(&[], &index).expect("the engine answers");
    drop(engine);

    let main = index
        .graph(&RelPath::new("app/src/main.rs"))
        .expect("app/src/main.rs is indexed")
        .nodes
        .iter()
        .find(|node| node.name == "main")
        .expect("the extractor emits `main`")
        .id
        .clone();
    let resolved: Vec<&str> = answer
        .diff
        .upsert_edges
        .iter()
        .filter(|edge| edge.from_id == main && edge.source == SourceTier::Semantic)
        .filter_map(|edge| answer.diff.upsert_nodes.iter().find(|node| node.id == edge.to_id))
        .map(|node| node.name.as_str())
        .collect();
    assert!(
        resolved.contains(&"method"),
        "{what}: sanity: rust-analyzer did not resolve `thing.method()` (complete: {}, reason: {:?}): {:#?}",
        answer.complete,
        answer.reason,
        answer.diff
    );
    assert_eq!(sink.take(), Vec::<String>::new(), "{what}: the semantic pass made network connections");
    assert_eq!(
        changes(&before, &snapshot(root)),
        Vec::<String>::new(),
        "{what}: the semantic pass wrote under the project root"
    );
}

#[test]
fn indexing_reaches_no_network_and_writes_nothing_under_the_root() {
    let scratch = Scratch::new();
    let proxies = proxy_dir(&scratch.0.join("rustup-proxies"));
    let sink = Sink::start();
    let cargo_home = scratch.0.join("cargo-home");
    std::fs::create_dir_all(&cargo_home).unwrap();
    set_environment(&proxies, &cargo_home, &sink);

    // Network fixture. Sanity: plain cargo resolving it reaches the sink.
    let copy = scratch.0.join("network-copy");
    write_fixture(&copy, NETWORK_DEPENDENCIES, false);
    let out = plain_cargo(&copy, &["metadata", "--format-version", "1"]);
    assert!(!sink.take().is_empty(), "sanity: plain `cargo metadata` made no connection to the sink: {out}");
    let network = scratch.0.join("network");
    write_fixture(&network, NETWORK_DEPENDENCIES, false);
    index_and_check(&network, &sink, "network fixture");

    // Writes fixture. Sanity: running its build script writes target/.
    let copy = scratch.0.join("writes-copy");
    write_fixture(&copy, "", true);
    let out = plain_cargo(&copy, &["check"]);
    assert!(copy.join("target").is_dir(), "sanity: `cargo check` wrote no target/: {out}");
    sink.take();
    let writes = scratch.0.join("writes");
    write_fixture(&writes, "", true);
    index_and_check(&writes, &sink, "writes fixture");
}
