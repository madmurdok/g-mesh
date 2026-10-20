//! The rust-analyzer tier runs no code the indexed project supplies.
//!
//! A hostile workspace (`docs/design/GM-513-semantic-servers-no-project-code.md`,
//! fixture `rfx`) is indexed through the plugin's own [`semantic::engine`]
//! (via `sandboxed_engine`),
//! with the shipped manifest and its bare `rust-analyzer` command, so the
//! toolchain detection, the candidate list and the pinned environment are
//! the production ones. Every hook the project offers writes a marker or a
//! log line when it runs:
//!
//! - `app/build.rs` writes `MARK_buildrs`; the proc macro `mac::make!`
//!   writes `MARK_procmacro` when expanded (cross-platform);
//! - `rust-toolchain.toml` names a `path` toolchain whose `cargo`, `rustc`
//!   and `rust-analyzer` log and then run the real ones (unix);
//! - `.cargo/config.toml` sets `build.rustc`, `build.rustc-wrapper` and
//!   `build.rustc-workspace-wrapper` to logging shims, and `[target.<host>]
//!   rustflags` to `-Zcodegen-backend=<dylib>`, whose constructor writes
//!   `MARK_codegen_backend` when any `rustc` loads it (unix);
//! - a project `rust-analyzer.toml` turns build scripts and proc macros back
//!   on.
//!
//! Before the pass, the shims are run the way an unpinned toolchain would
//! run them (the rustup proxies, in the fixture) and must log: the hostile
//! config is in force. After it, a receiver call must have resolved across
//! files (the server really answered), and no marker, no log and no
//! `target/` may exist.
//!
//! The child environment holds nothing cargo and rustup set for the test
//! process (`RUSTUP_TOOLCHAIN`, `CARGO`, `RUSTC_WRAPPER`, ...), and its
//! `PATH` starts with a directory of rustup proxies: otherwise the inherited
//! pin would hide a missing one, and `rust-analyzer` on `PATH` would be the
//! real binary rather than the proxy a user's daemon finds. The test makes
//! that directory itself (`cargo`, `rustc`, `rust-analyzer` linked to the
//! `rustup` on `PATH`, which dispatches on its program name exactly as an
//! installed proxy does) instead of assuming the directory holding `rustup`
//! also holds the proxies: on a Homebrew rustup (the macOS arm64 runner) it
//! does not (GM-574). Where the original test changed directory, the working
//! directory is the fixture, as a daemon started there would have it: the
//! proxy picks its toolchain from the working directory.
//!
//! Each test builds its fixture in this process, then re-runs this test
//! binary as a child limited to an `#[ignore]`d `<test>_in_its_env` test,
//! with that environment, the shipped manifest, the working directory and
//! the fixture's paths (GM-602, after GM-588 option A). The child runs the
//! engine and the assertions. No test changes this process's environment or
//! working directory, so the tests are independent of order and threads.
//!
//! On unix every marker dylib is built with the C compiler `cc`; a test
//! whose hook needs one fails when `cc` cannot build it, rather than
//! passing without testing that hook.

use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh_plugin_rust::extractor::RustExtractor;
use g_mesh_plugin_rust::project::{ProjectContext, EXCLUDE_DIRS};
use g_mesh_plugin_rust::semantic;
use g_mesh_plugin_sdk::wire::{SourceTier, TargetKey};
use g_mesh_plugin_sdk::{walk_project, Extractor, RelPath, SdkIndex, MANIFEST_PATH_ENV};

/// [`semantic::engine`] with the Linux sandbox trampoline run by the plugin
/// binary, since this test binary has no trampoline entry. Other OSes
/// ignore the launcher.
fn sandboxed_engine(root: &Path) -> anyhow::Result<Box<dyn g_mesh_plugin_sdk::SemanticEngine>> {
    semantic::engine_with(root, Path::new(env!("CARGO_BIN_EXE_g-mesh-plugin-rust")))
}

/// What a child test reads its fixture from, as one path list: the scratch
/// root, then the parent's rustup proxy directory, `rustc`, `cargo` and
/// `rust-analyzer`. Only the parent test sets it.
const CHILD_ENV: &str = "G_MESH_RS_GM513_CHILD";

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    /// Unique per call: the tests in this binary run in parallel and each
    /// makes its own `proxies` directory, which a shared name would let one
    /// test's `Drop` delete under another (GM-587).
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("g-mesh-rust-gm513-{name}-{}-{n}", std::process::id()));
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

/// The real toolchain: rustup's default, asked outside any project, the
/// absolute binaries in it, and a directory of rustup proxies.
struct RealToolchain {
    proxy_dir: PathBuf,
    /// The parent owns the proxy directory; a child only borrows its path.
    _proxies: Option<Scratch>,
    rustc: PathBuf,
    cargo: PathBuf,
    rust_analyzer: PathBuf,
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

fn real_toolchain() -> RealToolchain {
    let default = rustup(&["default"]);
    let pin = default.split_whitespace().next().expect("`rustup default` names a toolchain").to_string();
    let which = |name: &str| PathBuf::from(rustup(&["which", "--toolchain", &pin, name]));
    let rust_analyzer = which("rust-analyzer");
    assert!(
        Command::new(&rust_analyzer).arg("--version").output().is_ok_and(|o| o.status.success()),
        "{} does not answer --version: install it with `rustup component add rust-analyzer`",
        rust_analyzer.display()
    );
    let rustup_bin = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(format!("rustup{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
        .expect("rustup is on PATH");
    let proxies = Scratch::new("proxies");
    link_rustup_proxies(&proxies.0, &rustup_bin);
    RealToolchain {
        proxy_dir: proxies.0.clone(),
        _proxies: Some(proxies),
        rustc: which("rustc"),
        cargo: which("cargo"),
        rust_analyzer,
    }
}

/// Links `cargo`, `rustc` and `rust-analyzer` in `dir` to `rustup`: rustup
/// runs as the proxy its program name says, which is all an installed proxy
/// is. Not the directory `rustup` sits in: whether that one holds the proxies
/// is the installer's choice (rustup-init's `~/.cargo/bin` does, Homebrew's
/// `bin` does not), and a missing one is a spawn error, not a finding.
fn link_rustup_proxies(dir: &Path, rustup: &Path) {
    for name in ["cargo", "rustc", "rust-analyzer"] {
        let proxy = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        #[cfg(unix)]
        std::os::unix::fs::symlink(rustup, &proxy).unwrap();
        #[cfg(not(unix))]
        if std::fs::hard_link(rustup, &proxy).is_err() {
            std::fs::copy(rustup, &proxy).unwrap();
        }
    }
}

/// What cargo and rustup set for this test process, which a child must not
/// inherit.
fn cargo_and_rustup_vars() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let Some(name) = name.to_str() else { return false };
            (name.starts_with("CARGO") && name != "CARGO_HOME")
                || name.starts_with("RUSTC")
                || name.starts_with("RUSTDOC")
                || name == "RUSTFLAGS"
                || name == "RUSTUP_TOOLCHAIN"
        })
        .collect()
}

/// This process's `PATH` with the rustup proxies first and no toolchain
/// directory cargo put on it.
fn child_path(proxy_dir: &Path) -> std::ffi::OsString {
    let rest = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir != proxy_dir && !dir.to_string_lossy().contains("/toolchains/"))
        .collect::<Vec<_>>();
    std::env::join_paths(std::iter::once(proxy_dir.to_path_buf()).chain(rest)).unwrap()
}

/// The line a child test prints once its assertions have passed.
fn ran_marker(test: &str) -> String {
    format!("{test}: ran in its environment")
}

/// Runs the ignored test `test` of this binary in a child process with none
/// of [`cargo_and_rustup_vars`], [`child_path`] as `PATH`, the shipped
/// manifest, `cwd` as working directory when given (this process's
/// otherwise), and `scratch` and `real` in [`CHILD_ENV`]; asserts that it ran
/// and passed.
fn in_its_env(test: &str, scratch: &Path, real: &RealToolchain, cwd: Option<&Path>) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([test, "--exact", "--ignored", "--nocapture", "--test-threads=1"]);
    for name in cargo_and_rustup_vars() {
        command.env_remove(name);
    }
    let fixture = [scratch, &real.proxy_dir, &real.rustc, &real.cargo, &real.rust_analyzer];
    command
        .env("PATH", child_path(&real.proxy_dir))
        .env(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))
        .env(CHILD_ENV, std::env::join_paths(fixture).unwrap());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(&ran_marker(test)),
        "{test} did not pass in its environment ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        output.status
    );
}

/// The scratch root and the parent's toolchain of a child test, or `None`
/// when the test was started without its parent (`--ignored`/
/// `--run-ignored`), which then tests nothing.
fn child_fixture(test: &str) -> Option<(PathBuf, RealToolchain)> {
    let Some(list) = std::env::var_os(CHILD_ENV) else {
        println!("{test}: skipped, it runs only as the child of its parent test, which sets {CHILD_ENV}");
        return None;
    };
    let mut paths = std::env::split_paths(&list);
    let mut next =
        || paths.next().unwrap_or_else(|| panic!("{CHILD_ENV} lists the scratch and the toolchain"));
    let scratch = next();
    let real = RealToolchain {
        proxy_dir: next(),
        _proxies: None,
        rustc: next(),
        cargo: next(),
        rust_analyzer: next(),
    };
    Some((scratch, real))
}

/// The tests are independent of order and threads only while none of them
/// changes this process's environment or working directory: a child gets
/// its own through [`in_its_env`] instead. A run in one order can
/// miss a change that breaks another, so this reads the source.
#[test]
fn no_test_changes_the_process_environment_or_working_directory() {
    let source = include_str!("semantic_runs_no_project_code.rs");
    for call in ["set_var", "remove_var", "set_current_dir"] {
        let call = format!("{call}(");
        assert!(
            !source.contains(&call),
            "a test calls `{call}`: it changes this process for every test after it; give the child test \
             that environment through `in_its_env` instead"
        );
    }
}

/// Every log and marker a project hook writes, by what wrote it.
struct Fixture {
    root: PathBuf,
    marks: Vec<PathBuf>,
    #[cfg(unix)]
    logs: Vec<PathBuf>,
}

impl Fixture {
    /// The markers and logs of the `rfx` fixture [`write_fixture`] wrote at
    /// `root`.
    fn at(root: &Path) -> Self {
        #[allow(unused_mut)]
        let mut marks = vec![root.join("MARK_buildrs"), root.join("MARK_procmacro")];
        #[cfg(unix)]
        marks.push(root.join("MARK_codegen_backend"));
        Fixture {
            root: root.to_path_buf(),
            marks,
            #[cfg(unix)]
            logs: [
                "evil-cargo",
                "evil-rustc",
                "evil-rust-analyzer",
                "build-rustc",
                "rustc-wrapper",
                "rustc-workspace-wrapper",
            ]
            .iter()
            .map(|name| root.join("logs").join(name))
            .collect(),
        }
    }

    #[cfg(unix)]
    fn log(&self, name: &str) -> PathBuf {
        self.root.join("logs").join(name)
    }

    fn written(&self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for mark in &self.marks {
            if mark.exists() {
                found.push(mark.display().to_string());
            }
        }
        #[cfg(unix)]
        for log in &self.logs {
            if let Ok(text) = std::fs::read_to_string(log) {
                found.push(format!("{}:\n{text}", log.display()));
            }
        }
        if self.root.join("target").exists() {
            found.push("target/".to_string());
        }
        found
    }

    #[cfg(unix)]
    fn clear_logs(&self) {
        for log in &self.logs {
            let _ = std::fs::remove_file(log);
        }
    }
}

/// The host triple `rustc -vV` reports: the `[target.<triple>]` table
/// cargo applies to a host build.
#[cfg(unix)]
fn host_triple(rustc: &Path) -> String {
    let output = Command::new(rustc).arg("-vV").output().unwrap();
    assert!(output.status.success(), "{} -vV: {}", rustc.display(), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("`rustc -vV` names the host")
        .trim()
        .to_string()
}

/// Where [`build_marker_dylib`] puts the dylib it builds in `dir`.
#[cfg(unix)]
fn marker_dylib(dir: &Path) -> PathBuf {
    dir.join(if cfg!(target_os = "macos") { "marker.dylib" } else { "marker.so" })
}

/// A shared library whose load-time constructor writes `mark`: what a
/// project could ship prebuilt and have a `rustc` load, through `hook`.
/// Panics, naming `hook`, when no C compiler `cc` builds it: a test without
/// the dylib would pass while testing nothing about that hook.
#[cfg(unix)]
fn build_marker_dylib(dir: &Path, mark: &Path, hook: &str) -> PathBuf {
    let source = dir.join("marker.c");
    write(
        &source,
        &format!(
            "#include <stdio.h>\n__attribute__((constructor)) static void fire(void) {{\n    \
             FILE *f = fopen({:?}, \"a\");\n    if (f) {{ fputs(\"x\\n\", f); fclose(f); }}\n}}\n",
            mark.to_string_lossy()
        ),
    );
    let dylib = marker_dylib(dir);
    let shared: &[&str] = if cfg!(target_os = "macos") { &["-dynamiclib"] } else { &["-shared", "-fPIC"] };
    match Command::new("cc").args(shared).arg("-o").arg(&dylib).arg(&source).output() {
        Ok(output) if output.status.success() => dylib,
        result => panic!(
            "this test needs a C compiler `cc` to build the marker dylib for {hook}; without it the test \
             would prove nothing about that hook ({result:?})"
        ),
    }
}

/// `rfx`: workspace `app` + proc macro `mac`, plus (unix) the toolchain file,
/// the cargo config and their shims.
fn write_fixture(root: &Path, #[allow(unused_variables)] real: &RealToolchain) {
    let mark_buildrs = root.join("MARK_buildrs");
    let mark_procmacro = root.join("MARK_procmacro");
    #[cfg(unix)]
    let mark_backend = root.join("MARK_codegen_backend");
    write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"mac\"]\nresolver = \"2\"\n");
    write(
        &root.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n\
         [dependencies]\nmac = { path = \"../mac\" }\n",
    );
    write(
        &root.join("app/build.rs"),
        &format!(
            "fn main() {{\n    std::fs::write({:?}, \"\").unwrap();\n}}\n",
            mark_buildrs.to_string_lossy()
        ),
    );
    write(
        &root.join("app/src/main.rs"),
        "mod util;\n\nmac::make!();\n\nfn main() {\n    let thing = util::Thing;\n    thing.method();\n    \
         generated();\n}\n",
    );
    write(
        &root.join("app/src/util.rs"),
        "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
    );
    write(
        &root.join("mac/Cargo.toml"),
        "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
    );
    write(
        &root.join("mac/src/lib.rs"),
        &format!(
            "extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n#[proc_macro]\npub fn make(_input: \
             TokenStream) -> TokenStream {{\n    std::fs::write({:?}, \"\").unwrap();\n    \"pub fn generated() \
             {{}}\".parse().unwrap()\n}}\n",
            mark_procmacro.to_string_lossy()
        ),
    );
    write(
        &root.join("rust-analyzer.toml"),
        "[cargo.buildScripts]\nenable = true\n\n[procMacro]\nenable = true\n",
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let shim = |path: &Path, log: &Path, exec: &str| {
            write(
                path,
                &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec {exec} \"$@\"\n", log.display()),
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let logs_dir = root.join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let quoted = |path: &Path| format!("'{}'", path.display());

        let evil = root.join("evil_tc");
        for (name, real) in
            [("cargo", &real.cargo), ("rustc", &real.rustc), ("rust-analyzer", &real.rust_analyzer)]
        {
            shim(&evil.join("bin").join(name), &logs_dir.join(format!("evil-{name}")), &quoted(real));
        }
        write(
            &root.join("rust-toolchain.toml"),
            &format!("[toolchain]\npath = {:?}\n", evil.to_string_lossy()),
        );

        let shims = root.join("shims");
        shim(&shims.join("rustc"), &logs_dir.join("build-rustc"), &quoted(&real.rustc));
        shim(&shims.join("wrapper"), &logs_dir.join("rustc-wrapper"), "");
        shim(&shims.join("workspace-wrapper"), &logs_dir.join("rustc-workspace-wrapper"), "");
        let mut config = format!(
            "[build]\nrustc = {:?}\nrustc-wrapper = {:?}\nrustc-workspace-wrapper = {:?}\n",
            shims.join("rustc").to_string_lossy(),
            shims.join("wrapper").to_string_lossy(),
            shims.join("workspace-wrapper").to_string_lossy()
        );
        let backend = build_marker_dylib(
            &shims,
            &mark_backend,
            "the `[target.<host>] rustflags` `-Zcodegen-backend` hook",
        );
        config.push_str(&format!(
            "\n[target.{}]\nrustflags = [{:?}]\n",
            host_triple(&real.rustc),
            format!("-Zcodegen-backend={}", backend.to_string_lossy())
        ));
        write(&root.join(".cargo/config.toml"), &config);
    }
}

/// The hostile config is live: run the way an unpinned rustup user runs them
/// in this directory, the proxies reach the project's toolchain, `build.rustc`
/// and the wrapper; with `RUSTC_BOOTSTRAP=1`, as rust-analyzer runs some
/// `rustc` probes, the `[target.*]` rustflags load the codegen-backend dylib.
#[cfg(unix)]
fn assert_the_hostile_config_is_in_force(fixture: &Fixture, real: &RealToolchain) {
    let run = |program: PathBuf, args: &[&str], env: &[(&str, &str)], must_succeed: bool| {
        let output = Command::new(&program)
            .args(args)
            .envs(env.iter().copied())
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !must_succeed || output.status.success(),
            "{} {args:?} in the fixture: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    // Every run starts with no log and no marker and is judged on its own,
    // so each assertion names the run that fired the hook.
    let mark_backend = fixture.root.join("MARK_codegen_backend");
    let reset = || {
        fixture.clear_logs();
        let _ = std::fs::remove_file(&mark_backend);
    };
    let assert_logged = |run: &str, names: &[&str]| {
        for name in names {
            assert!(
                fixture.log(name).is_file(),
                "sanity: {run} in the fixture never ran the project's `{name}` hook, so this test would \
                 prove nothing about it"
            );
        }
    };
    let metadata: &[&str] = &["metadata", "--format-version", "1"];

    reset();
    run(real.proxy_dir.join("rust-analyzer"), &["--version"], &[], true);
    assert_logged("the unpinned `rust-analyzer --version`", &["evil-rust-analyzer"]);

    // A stable `rustc` refuses the `-Z` flag, so this run leaves the
    // rustflags out; the next one puts them back.
    reset();
    run(real.proxy_dir.join("cargo"), metadata, &[("CARGO_ENCODED_RUSTFLAGS", "")], true);
    assert_logged("the unpinned stable `cargo metadata`", &["evil-cargo", "build-rustc", "rustc-wrapper"]);
    assert!(
        !mark_backend.exists(),
        "the stable `cargo metadata` with empty rustflags loaded the `-Zcodegen-backend` dylib"
    );

    reset();
    run(real.proxy_dir.join("cargo"), metadata, &[("RUSTC_BOOTSTRAP", "1")], false);
    assert!(
        mark_backend.is_file(),
        "sanity: the unpinned `RUSTC_BOOTSTRAP=1 cargo metadata` in the fixture never loaded the \
         project's `-Zcodegen-backend` dylib, so this test would prove nothing about it"
    );
    reset();
    let _ = std::fs::remove_file(fixture.root.join("Cargo.lock"));
    let _ = std::fs::remove_dir_all(fixture.root.join("target"));
}

#[test]
fn rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names() {
    let real = real_toolchain();
    let scratch = Scratch::new("project");
    write_fixture(&scratch.0, &real);
    in_its_env(
        "rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names_in_its_env",
        &scratch.0,
        &real,
        Some(&scratch.0),
    );
}

#[test]
#[ignore = "run by its parent test in the fixture's environment"]
fn rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names_in_its_env() {
    let test =
        "rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names_in_its_env";
    #[allow(unused_variables)]
    let Some((root, real)) = child_fixture(test) else {
        return;
    };
    let fixture = Fixture::at(&root);

    #[cfg(unix)]
    assert_the_hostile_config_is_in_force(&fixture, &real);
    assert!(fixture.written().is_empty(), "the fixture starts clean: {:#?}", fixture.written());

    let project = ProjectContext::load(&fixture.root).expect("the fixture is a loadable Rust project");
    let mut index = SdkIndex::new();
    for path in walk_project(&fixture.root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
        let source = std::fs::read_to_string(fixture.root.join(path.as_str())).unwrap();
        let graph = RustExtractor.extract(&project, &path, &source);
        index.insert(path, source, graph);
    }

    let mut engine = crate::sandboxed_engine(&fixture.root).expect("the engine resolves a rust-analyzer");
    let answer = engine.answer(&[], &index).expect("the engine answers");
    drop(engine);

    // Sanity: the server answered - `thing.method()` in `main`, a receiver
    // call into another file, is a semantic edge.
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
        "sanity: rust-analyzer did not resolve `thing.method()` (complete: {}, reason: {:?}): {:#?}",
        answer.complete,
        answer.reason,
        answer.diff
    );

    assert_eq!(
        fixture.written(),
        Vec::<String>::new(),
        "the semantic tier ran code the project supplied (markers, shim logs or a target/ dir)"
    );
    println!("{}", ran_marker(test));
}

// ---------------------------------------------------------------------------
// GM-591: a project `rust-analyzer.toml`'s `cargo.extraEnv` is not applied.
//
// rust-analyzer 1.97.1 reads a project `rust-analyzer.toml` (workspace root
// or crate level) but ignores its `cargo.extraEnv`, while a client-side
// `extraEnv` (initialization options) does override `pinned_env`. If a
// rust-analyzer that honours the project file ever shipped, a project could
// set `CARGO_ENCODED_RUSTFLAGS` / `CARGO_UNSTABLE_HOST_CONFIG` and reopen the
// rustflags and host-config channels (GM-572) that `pinned_env` closes.
// These tests pin the observed behaviour; the control proves the fixture's
// values are live by sending the same values through the client channel.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod project_extra_env {
    use super::*;
    use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};

    /// Where the project's `rust-analyzer.toml` sits.
    #[derive(Clone, Copy)]
    enum TomlAt {
        Root,
        Crate,
    }

    struct ExtraEnvFixture {
        root: PathBuf,
        rustflags_mark: PathBuf,
        host_mark: PathBuf,
        /// The `cargo.extraEnv` object: marker-firing values for both channels.
        extra_env: serde_json::Value,
    }

    impl ExtraEnvFixture {
        /// The fixture [`write_extra_env_fixture`] wrote at `root`.
        fn at(root: &Path) -> Self {
            let rustflags_dylib = marker_dylib(&root.join("dylib_rustflags"));
            let extra_env = serde_json::json!({
                "CARGO_ENCODED_RUSTFLAGS": format!("-Zcodegen-backend={}", rustflags_dylib.to_string_lossy()),
                "CARGO_UNSTABLE_HOST_CONFIG": "true",
                // A stable cargo honours the config's `[unstable]` table, which
                // `host-config` needs, only when bootstrapped.
                "RUSTC_BOOTSTRAP": "1",
            });
            ExtraEnvFixture {
                root: root.to_path_buf(),
                rustflags_mark: root.join("MARK_extraenv_rustflags"),
                host_mark: root.join("MARK_extraenv_host_config"),
                extra_env,
            }
        }

        fn fired(&self) -> Vec<String> {
            [&self.rustflags_mark, &self.host_mark]
                .iter()
                .filter(|mark| mark.exists())
                .map(|mark| mark.display().to_string())
                .collect()
        }
    }

    /// A plain package with no build script, proc macro, toolchain file or
    /// shim, so the only code a rustc could load is a marker dylib named by
    /// the two channels: `CARGO_ENCODED_RUSTFLAGS` (a `-Zcodegen-backend`)
    /// and `CARGO_UNSTABLE_HOST_CONFIG=true` (which lets the config's
    /// `[host] rustflags` through).
    fn write_extra_env_fixture(root: &Path, at: Option<TomlAt>) {
        let fixture = ExtraEnvFixture::at(root);
        build_marker_dylib(
            &root.join("dylib_rustflags"),
            &fixture.rustflags_mark,
            "the `cargo.extraEnv` `CARGO_ENCODED_RUSTFLAGS` hook",
        );
        let host_dylib = build_marker_dylib(
            &root.join("dylib_host"),
            &fixture.host_mark,
            "the `cargo.extraEnv` `CARGO_UNSTABLE_HOST_CONFIG` + config `[host] rustflags` hook",
        );
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
        write(
            &root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &root.join("app/src/main.rs"),
            "mod util;\n\nfn main() {\n    let thing = util::Thing;\n    thing.method();\n}\n",
        );
        write(
            &root.join("app/src/util.rs"),
            "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        write(
            &root.join(".cargo/config.toml"),
            &format!(
                "target-applies-to-host = false\n\n[unstable]\nhost-config = true\ntarget-applies-to-host = true\n\n\
                 [host]\nrustflags = [{:?}]\n",
                format!("-Zcodegen-backend={}", host_dylib.to_string_lossy())
            ),
        );
        let table = format!(
            "[cargo.extraEnv]\nCARGO_ENCODED_RUSTFLAGS = {:?}\nCARGO_UNSTABLE_HOST_CONFIG = \"true\"\nRUSTC_BOOTSTRAP = \"1\"\n",
            fixture.extra_env["CARGO_ENCODED_RUSTFLAGS"].as_str().unwrap()
        );
        match at {
            Some(TomlAt::Root) => write(&root.join("rust-analyzer.toml"), &table),
            Some(TomlAt::Crate) => write(&root.join("app/rust-analyzer.toml"), &table),
            None => {}
        }
    }

    fn index_of(root: &Path) -> SdkIndex {
        let project = ProjectContext::load(root).expect("the fixture is a loadable Rust project");
        let mut index = SdkIndex::new();
        for path in walk_project(root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
            let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
            let graph = RustExtractor.extract(&project, &path, &source);
            index.insert(path, source, graph);
        }
        index
    }

    /// Whether the answer resolved `thing.method()` across files: the server
    /// really indexed the fixture.
    pub(super) fn resolved_method(index: &SdkIndex, answer: &g_mesh_plugin_sdk::SemanticAnswer) -> bool {
        let main = index
            .graph(&RelPath::new("app/src/main.rs"))
            .expect("app/src/main.rs is indexed")
            .nodes
            .iter()
            .find(|node| node.name == "main")
            .expect("the extractor emits `main`")
            .id
            .clone();
        answer
            .diff
            .upsert_edges
            .iter()
            .filter(|edge| edge.from_id == main && edge.source == SourceTier::Semantic)
            .filter_map(|edge| answer.diff.upsert_nodes.iter().find(|node| node.id == edge.to_id))
            .any(|node| node.name == "method")
    }

    /// Indexes the fixture. `client_extra_env: None` runs the production
    /// `semantic::engine`; `Some(env)` runs a bridge built the way `engine`
    /// builds one (manifest config, the same pinned values) with `env` added
    /// as the client's `cargo.extraEnv` initialization option. Runs in the
    /// child, whose working directory is the fixture.
    fn index_fixture(
        fixture: &ExtraEnvFixture,
        real: &RealToolchain,
        client_extra_env: Option<&serde_json::Value>,
    ) {
        let index = index_of(&fixture.root);
        let mut engine: Box<dyn g_mesh_plugin_sdk::SemanticEngine> = match client_extra_env {
            None => crate::sandboxed_engine(&fixture.root).expect("the engine resolves a rust-analyzer"),
            Some(env) => {
                let mut config =
                    SemanticConfig::from_manifest().unwrap().expect("the manifest has [plugin.semantic]");
                config.command = real.rust_analyzer.clone();
                for (name, value) in [
                    ("CARGO_NET_OFFLINE", "true".to_string()),
                    ("CARGO_ENCODED_RUSTFLAGS", String::new()),
                    ("CARGO_UNSTABLE_HOST_CONFIG", "false".to_string()),
                    ("RUSTC", real.rustc.to_string_lossy().into_owned()),
                    ("CARGO", real.cargo.to_string_lossy().into_owned()),
                ] {
                    config.env.insert(name.to_string(), value);
                }
                let options = config.initialization_options.get_or_insert_with(|| serde_json::json!({}));
                options["cargo"]["extraEnv"] = env.clone();
                Box::new(LspBridge::new("rust", &fixture.root, config).quiescent_signal())
            }
        };
        let answer = engine.answer(&[], &index).expect("the engine answers");
        drop(engine);
        assert!(
            resolved_method(&index, &answer),
            "sanity: rust-analyzer did not resolve `thing.method()` (complete: {}, reason: {:?})",
            answer.complete,
            answer.reason
        );
    }

    /// The parent: writes the fixture with its `rust-analyzer.toml` `at`,
    /// then runs `child` in the fixture's environment.
    fn extra_env_in_its_env(name: &str, at: Option<TomlAt>, child: &str) {
        let real = real_toolchain();
        let scratch = Scratch::new(name);
        write_extra_env_fixture(&scratch.0, at);
        in_its_env(child, &scratch.0, &real, Some(&scratch.0));
    }

    /// The child of both project-file tests.
    fn assert_project_extra_env_is_ignored(test: &str) {
        let Some((root, real)) = child_fixture(test) else { return };
        let fixture = ExtraEnvFixture::at(&root);
        index_fixture(&fixture, &real, None);
        assert_eq!(
            fixture.fired(),
            Vec::<String>::new(),
            "a project rust-analyzer.toml's cargo.extraEnv reached the server's cargo/rustc: the rustflags \
             or host-config channel is open again (GM-572)"
        );
        println!("{}", ran_marker(test));
    }

    #[test]
    fn a_root_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker() {
        extra_env_in_its_env(
            "extraenv",
            Some(TomlAt::Root),
            "project_extra_env::a_root_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_root_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env() {
        assert_project_extra_env_is_ignored(
            "project_extra_env::a_root_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env",
        );
    }

    #[test]
    fn a_crate_level_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker() {
        extra_env_in_its_env(
            "extraenv",
            Some(TomlAt::Crate),
            "project_extra_env::a_crate_level_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_crate_level_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env() {
        assert_project_extra_env_is_ignored(
            "project_extra_env::a_crate_level_rust_analyzer_toml_extra_env_fires_no_rustflags_or_host_config_marker_in_its_env",
        );
    }

    /// Control: the same values through the client's `cargo.extraEnv` beat
    /// the pinned env and fire the markers, so the fixture's values are live
    /// and the two tests above are not silent for want of a working hook.
    /// The control reverses by construction: it is the same fixture and the
    /// same pass, with the values moved from the project file to the client.
    #[test]
    fn the_same_extra_env_through_the_client_fires_the_markers() {
        extra_env_in_its_env(
            "extraenv-ctl",
            None,
            "project_extra_env::the_same_extra_env_through_the_client_fires_the_markers_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn the_same_extra_env_through_the_client_fires_the_markers_in_its_env() {
        let test = "project_extra_env::the_same_extra_env_through_the_client_fires_the_markers_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let fixture = ExtraEnvFixture::at(&root);
        let env = fixture.extra_env.clone();
        index_fixture(&fixture, &real, Some(&env));
        assert!(
            fixture.rustflags_mark.exists(),
            "sanity: the client's CARGO_ENCODED_RUSTFLAGS fired no marker (fired: {:?}), so the project-file \
             tests prove nothing about that channel",
            fixture.fired()
        );
        assert!(
            fixture.host_mark.exists(),
            "sanity: the client's CARGO_UNSTABLE_HOST_CONFIG fired no marker (fired: {:?}), so the project-file \
             tests prove nothing about that channel",
            fixture.fired()
        );
        println!("{}", ran_marker(test));
    }
}

// ---------------------------------------------------------------------------
// GM-587: a cargo config `[env]` that sets a dynamic-loader variable with
// `force = true` would load a project dylib into every rustc that the
// server's `cargo metadata` starts; `semantic::engine` refuses such a project
// to the structural tier instead (docs/adr/0037).
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod cargo_env_loader_var {
    use super::*;

    /// The loader variable this platform's dynamic loader honours.
    const LOADER_VAR: &str = if cfg!(target_os = "macos") { "DYLD_INSERT_LIBRARIES" } else { "LD_PRELOAD" };

    const LOADER_MANIFEST: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    /// The marker a loaded dylib writes and the dylib, in the fixture at `root`.
    fn mark_and_dylib(root: &Path) -> (PathBuf, PathBuf) {
        (root.join("MARK_cargo_env_loader"), marker_dylib(&root.join("dylib")))
    }

    /// The parent: writes a clean package (no cargo config yet) and the
    /// marker dylib, then runs `child` in the fixture's environment, from
    /// this process's working directory.
    fn loader_fixture_in_its_env(name: &str, child: &str) {
        let real = real_toolchain();
        let scratch = Scratch::new(name);
        let root = scratch.0.clone();
        let (mark, _) = mark_and_dylib(&root);
        build_marker_dylib(
            &root.join("dylib"),
            &mark,
            &format!("the cargo config `[env] {LOADER_VAR}` hook"),
        );
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
        write(&root.join("app/Cargo.toml"), LOADER_MANIFEST);
        write(&root.join("app/src/main.rs"), "mod util;\n\nfn main() {\n    util::Thing.method();\n}\n");
        write(
            &root.join("app/src/util.rs"),
            "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        in_its_env(child, &root, &real, None);
    }

    /// Writes the fixture's `.cargo/config.toml` with `[env] LOADER_VAR`
    /// forced to the marker dylib, checks that an unpinned `cargo metadata`
    /// in the fixture then loads it (the hostile config is live), and clears
    /// what that run left. Returns the marker and the config file.
    fn write_live_loader_config(root: &Path, real: &RealToolchain) -> (PathBuf, PathBuf) {
        let (mark, dylib) = mark_and_dylib(root);
        let config = root.join(".cargo/config.toml");
        write(
            &config,
            &format!("[env]\n{LOADER_VAR} = {{ value = {:?}, force = true }}\n", dylib.to_string_lossy()),
        );
        let output = Command::new(real.proxy_dir.join("cargo"))
            .args(["metadata", "--format-version", "1"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo metadata in the fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            mark.is_file(),
            "sanity: `cargo metadata` in the fixture never loaded the [env] {LOADER_VAR} dylib, so this test \
             would prove nothing about that channel"
        );
        std::fs::remove_file(&mark).unwrap();
        let _ = std::fs::remove_file(root.join("Cargo.lock"));
        (mark, config)
    }

    #[test]
    fn a_cargo_config_env_loader_var_refuses_the_semantic_tier_and_fires_no_marker() {
        loader_fixture_in_its_env(
            "loader-env",
            "cargo_env_loader_var::a_cargo_config_env_loader_var_refuses_the_semantic_tier_and_fires_no_marker_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_cargo_config_env_loader_var_refuses_the_semantic_tier_and_fires_no_marker_in_its_env() {
        let test =
            "cargo_env_loader_var::a_cargo_config_env_loader_var_refuses_the_semantic_tier_and_fires_no_marker_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let (mark, config) = write_live_loader_config(&root, &real);

        let refusal = match crate::sandboxed_engine(&root) {
            Err(error) => Some(format!("{error:#}")),
            Ok(mut engine) => {
                let index = {
                    let project =
                        ProjectContext::load(&root).expect("the fixture is a loadable Rust project");
                    let mut index = SdkIndex::new();
                    for path in walk_project(&root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
                        let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
                        let graph = RustExtractor.extract(&project, &path, &source);
                        index.insert(path, source, graph);
                    }
                    index
                };
                let _ = engine.answer(&[], &index);
                drop(engine);
                None
            }
        };

        assert!(
            !mark.exists(),
            "the semantic tier started a rustc that loaded the project's [env] {LOADER_VAR} dylib"
        );
        let reason =
            refusal.expect("the engine started a server for a project whose cargo config sets a loader var");
        assert!(
            reason.contains(LOADER_VAR) && reason.contains(&config.display().to_string()),
            "the engine failed, but not by refusing the [env] {LOADER_VAR} in {}: {reason}",
            config.display()
        );
        println!("{}", ran_marker(test));
    }

    /// On macOS the server `semantic::server_config` builds runs under
    /// Seatbelt: with the engine's cargo config pre-check bypassed, the
    /// hostile `[env]` reaches the rustc that the server's `cargo metadata`
    /// starts, which maps no project dylib (no marker), and the server still
    /// resolves the workspace-local call `util::Thing.method()`.
    ///
    /// Controls: make `seatbelt::confine` return `Confined` without
    /// rewriting the config; or drop `file-map-executable` from both the
    /// profile's default deny and the project-root deny (the root deny alone
    /// already refuses the dylib).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_server_maps_no_project_dylib_and_still_resolves() {
        loader_fixture_in_its_env(
            "loader-sandbox",
            "cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env() {
        use g_mesh_plugin_sdk::lsp::LspBridge;
        use g_mesh_plugin_sdk::SemanticEngine;

        let test =
            "cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let (mark, _) = write_live_loader_config(&root, &real);

        let config = semantic::server_config(&root).expect("the server config is built and confined");
        assert_eq!(
            config.command,
            Path::new("/usr/bin/sandbox-exec"),
            "the server does not start under Seatbelt: {config:?}"
        );
        let index = index_of(&root);
        let mut bridge = LspBridge::new("rust", &root, config).quiescent_signal();
        let answer = bridge.answer(&[], &index).expect("the confined server answers");
        drop(bridge);

        assert!(
            !mark.exists(),
            "the confined server started a rustc that loaded the project's [env] {LOADER_VAR} dylib"
        );
        assert!(
            super::project_extra_env::resolved_method(&index, &answer),
            "the confined server did not resolve `util::Thing.method()` (complete: {}, reason: {:?})",
            answer.complete,
            answer.reason
        );
        println!("{}", ran_marker(test));
    }

    /// On Linux the server `semantic::server_config_with` builds, with the
    /// plugin binary as the trampoline, runs in the `noexec` mount namespace
    /// (plus Landlock where the kernel has it): with the engine's cargo
    /// config pre-check bypassed, the rustc the server's `cargo metadata`
    /// starts cannot map the project's `[env] LD_PRELOAD` object (no
    /// marker), and the server still resolves `util::Thing.method()`. On
    /// GitHub Actions it must be the full tier.
    ///
    /// Controls: make `landlock_ns::confine` return `Confined` without
    /// rewriting the config; or drop `policy.toolchain` from the `--allow`
    /// list (the resolve assertion fails).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandboxed_server_maps_no_project_dylib_and_still_resolves() {
        if !super::linux_sandbox::host_runs_the_mount_layer(
            "a_sandboxed_server_maps_no_project_dylib_and_still_resolves",
        ) {
            return;
        }
        loader_fixture_in_its_env(
            "loader-sandbox",
            "cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env() {
        use g_mesh_plugin_sdk::lsp::LspBridge;
        use g_mesh_plugin_sdk::SemanticEngine;

        let test =
            "cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let (mark, _) = write_live_loader_config(&root, &real);

        let config = semantic::server_config_with(&root, Path::new(super::linux_sandbox::LAUNCHER))
            .expect("the server config is built and confined");
        let launched = format!("{config:?}");
        let (command, args) = (config.command.clone(), config.args.clone());
        let index = index_of(&root);
        let mut bridge = LspBridge::new("rust", &root, config).quiescent_signal();
        let answer = bridge.answer(&[], &index).expect("the confined server answers");
        drop(bridge);

        assert!(
            !mark.exists(),
            "the confined server started a rustc that loaded the project's [env] {LOADER_VAR} object"
        );
        assert!(
            super::project_extra_env::resolved_method(&index, &answer),
            "the confined server did not resolve `util::Thing.method()` (complete: {}, reason: {:?})",
            answer.complete,
            answer.reason
        );

        assert_eq!(command, Path::new(super::linux_sandbox::LAUNCHER), "{launched}");
        assert_eq!(
            args.get(..2),
            Some(&["__g-mesh-sandbox".to_string(), "--layers".to_string()][..]),
            "{launched}"
        );
        let layers = args[2].as_str();
        if super::linux_sandbox::on_ci() {
            assert_eq!(
                layers, "mount,landlock",
                "GitHub Actions' ubuntu-22.04 runs the full tier: {launched}"
            );
        } else {
            assert!(
                layers == "mount,landlock" || layers == "mount",
                "the parent saw a working mount layer, but the server runs under `{layers}`: {launched}"
            );
        }
        println!("{}", ran_marker(test));
    }

    /// Watches `mark` for `span`; `Err` with how long it took when it appears.
    fn mark_stays_absent(mark: &Path, span: std::time::Duration) -> Result<(), std::time::Duration> {
        let start = std::time::Instant::now();
        loop {
            if mark.exists() {
                return Err(start.elapsed());
            }
            if start.elapsed() >= span {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// A loader var added after the engine started: a checkout writes the
    /// config and edits a manifest, and core's notice of it lags. Until the
    /// notice, rust-analyzer must not reload on its own; at the notice the
    /// engine re-checks the configs, stops the server and answers `Err`
    /// naming the config, and no rustc ever loads the dylib.
    #[test]
    fn a_loader_var_added_after_start_stops_the_server_before_any_reload() {
        loader_fixture_in_its_env(
            "loader-env-late",
            "cargo_env_loader_var::a_loader_var_added_after_start_stops_the_server_before_any_reload_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_loader_var_added_after_start_stops_the_server_before_any_reload_in_its_env() {
        use std::time::Duration;

        let test = "cargo_env_loader_var::a_loader_var_added_after_start_stops_the_server_before_any_reload_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let (mark, dylib) = mark_and_dylib(&root);
        let manifest = LOADER_MANIFEST;

        let mut engine = crate::sandboxed_engine(&root).expect("no cargo config yet: the engine starts");
        let index = {
            let project = ProjectContext::load(&root).expect("the fixture is a loadable Rust project");
            let mut index = SdkIndex::new();
            for path in walk_project(&root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
                let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
                let graph = RustExtractor.extract(&project, &path, &source);
                index.insert(path, source, graph);
            }
            index
        };
        if let Err(error) = engine.answer(&[], &index) {
            panic!("the first pass on a clean fixture failed: {error:#}");
        }

        let config = root.join(".cargo/config.toml");
        write(
            &config,
            &format!("[env]\n{LOADER_VAR} = {{ value = {:?}, force = true }}\n", dylib.to_string_lossy()),
        );
        write(&root.join("app/Cargo.toml"), &format!("{manifest}\n[dependencies]\n"));
        // Core's notice lags the edit; the engine hears nothing meanwhile.
        if let Err(after) = mark_stays_absent(&mark, Duration::from_secs(15)) {
            panic!(
                "rust-analyzer reloaded on its own {after:?} after the manifest edit and a rustc loaded \
                 the [env] {LOADER_VAR} dylib before the engine could re-check the configs"
            );
        }

        engine.workspace_changed();
        if let Err(after) = mark_stays_absent(&mark, Duration::from_secs(10)) {
            panic!("a rustc loaded the [env] {LOADER_VAR} dylib {after:?} after workspace_changed");
        }
        let reason = match engine.answer(&[], &index) {
            Ok(_) => panic!("a pass ran after the cargo config turned hostile"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            reason.contains(LOADER_VAR) && reason.contains(&config.display().to_string()),
            "the pass failed, but not by refusing the [env] {LOADER_VAR} in {}: {reason}",
            config.display()
        );
        if let Err(after) = mark_stays_absent(&mark, Duration::from_secs(5)) {
            panic!("a rustc loaded the [env] {LOADER_VAR} dylib {after:?} after the refusal");
        }
        drop(engine);

        // Sanity: the hostile config is live - an unpinned `cargo metadata`
        // in the fixture loads the dylib into the rustc it starts.
        let output = Command::new(real.proxy_dir.join("cargo"))
            .args(["metadata", "--format-version", "1"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo metadata in the fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            mark.is_file(),
            "sanity: `cargo metadata` in the fixture never loaded the [env] {LOADER_VAR} dylib, so this test \
             would prove nothing about that channel"
        );
        println!("{}", ran_marker(test));
    }

    /// The bridge forwards every file a pass is sent to rust-analyzer, and a
    /// created one makes it refetch the workspace (`cargo metadata`, which
    /// loads an `[env]` loader var). So a pass after the config turned
    /// hostile must refuse before anything is forwarded: with no workspace
    /// change in between, a file created with the config, and a file left
    /// pending by an earlier pass that reached no server. No rustc may load
    /// the dylib.
    ///
    /// Control: let `CheckedBridge::answer` skip the re-check
    /// (`self.bridge.as_mut()` for `self.checked()`).
    #[test]
    fn a_pass_after_the_config_turned_hostile_refuses_before_forwarding_any_file() {
        loader_fixture_in_its_env(
            "loader-env-pending",
            "cargo_env_loader_var::a_pass_after_the_config_turned_hostile_refuses_before_forwarding_any_file_in_its_env",
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn a_pass_after_the_config_turned_hostile_refuses_before_forwarding_any_file_in_its_env() {
        use std::time::Duration;

        let test =
            "cargo_env_loader_var::a_pass_after_the_config_turned_hostile_refuses_before_forwarding_any_file_in_its_env";
        let Some((root, real)) = child_fixture(test) else { return };
        let (mark, dylib) = mark_and_dylib(&root);

        let mut engine = crate::sandboxed_engine(&root).expect("no cargo config yet: the engine starts");
        if let Err(error) = engine.answer(&[], &index_of(&root)) {
            panic!("the first pass on a clean fixture failed: {error:#}");
        }

        // util.rs has no call site: a pass about it alone asks nothing,
        // reaches no server, and leaves util.rs pending.
        let util = RelPath::new("app/src/util.rs");
        write(
            &root.join(util.as_str()),
            "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n\npub fn helper() {}\n",
        );
        if let Err(error) = engine.answer(&[util], &index_of(&root)) {
            panic!("a pass about util.rs on a clean fixture failed: {error:#}");
        }

        let config = root.join(".cargo/config.toml");
        write(
            &config,
            &format!("[env]\n{LOADER_VAR} = {{ value = {:?}, force = true }}\n", dylib.to_string_lossy()),
        );
        write(&root.join("app/src/bin/extra.rs"), "fn main() {}\n");
        let files = [RelPath::new("app/src/main.rs"), RelPath::new("app/src/bin/extra.rs")];
        let reason = match engine.answer(&files, &index_of(&root)) {
            Ok(_) => panic!("a pass ran after the cargo config turned hostile"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            reason.contains(LOADER_VAR) && reason.contains(&config.display().to_string()),
            "the pass failed, but not by refusing the [env] {LOADER_VAR} in {}: {reason}",
            config.display()
        );
        if let Err(after) = mark_stays_absent(&mark, Duration::from_secs(10)) {
            panic!("a rustc loaded the [env] {LOADER_VAR} dylib {after:?} after the refused pass");
        }
        drop(engine);

        // Sanity: the hostile config is live, as in the test above.
        let output = Command::new(real.proxy_dir.join("cargo"))
            .args(["metadata", "--format-version", "1"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo metadata in the fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            mark.is_file(),
            "sanity: `cargo metadata` in the fixture never loaded the [env] {LOADER_VAR} dylib, so this test \
             would prove nothing about that channel"
        );
        println!("{}", ran_marker(test));
    }

    fn index_of(root: &Path) -> SdkIndex {
        let project = ProjectContext::load(root).expect("the fixture is a loadable Rust project");
        let mut index = SdkIndex::new();
        for path in walk_project(root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
            let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
            let graph = RustExtractor.extract(&project, &path, &source);
            index.insert(path, source, graph);
        }
        index
    }
}

/// rust-analyzer runs no file watcher of its own (the bridge claims client
/// file watching), so an edit on disk reaches it only through the engine.
mod source_freshness {
    use super::*;

    fn index_of(root: &Path) -> SdkIndex {
        let project = ProjectContext::load(root).expect("the fixture is a loadable Rust project");
        let mut index = SdkIndex::new();
        for path in walk_project(root, &[".rs".to_string()], &EXCLUDE_DIRS.map(String::from)) {
            let source = std::fs::read_to_string(root.join(path.as_str())).unwrap();
            let graph = RustExtractor.extract(&project, &path, &source);
            index.insert(path, source, graph);
        }
        index
    }

    /// What `main`'s semantic edges in `answer` point at: the qualified name
    /// each target placeholder is keyed by (`util::Thing::method`).
    fn called_by_main(index: &SdkIndex, answer: &g_mesh_plugin_sdk::SemanticAnswer) -> Vec<String> {
        let main = index
            .graph(&RelPath::new("app/src/main.rs"))
            .expect("app/src/main.rs is indexed")
            .nodes
            .iter()
            .find(|node| node.name == "main")
            .expect("the extractor emits `main`")
            .id
            .clone();
        answer
            .diff
            .upsert_edges
            .iter()
            .filter(|edge| edge.from_id == main && edge.source == SourceTier::Semantic)
            .map(|edge| {
                let target = answer.diff.upsert_nodes.iter().find(|node| node.id == edge.to_id);
                match target.and_then(|node| node.target.as_ref()).map(|target| &target.key) {
                    Some(TargetKey::QualifiedName(name)) => name.clone(),
                    other => format!("{} (target {other:?})", edge.to_id),
                }
            })
            .collect()
    }

    /// An edit to a file no question is asked in (util.rs has no call site,
    /// so it is never opened on the server) still reaches rust-analyzer, and
    /// the answer about its caller is built from the edited text. The edit
    /// moves `Thing::method` down and puts `Other::method` on its old lines:
    /// a server still holding the old text answers with the old position,
    /// which now lands on `Other::method`.
    ///
    /// Control: in `LspBridge::answer`, forward nothing (an empty set for the
    /// `forward_watched` call).
    #[test]
    fn an_edit_to_a_file_no_question_touched_reaches_the_answer_about_its_caller() {
        let real = real_toolchain();
        let scratch = Scratch::new("fresh-unasked");
        let root = scratch.0.clone();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
        write(
            &root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&root.join("app/src/main.rs"), "mod util;\n\nfn main() {\n    util::Thing.method();\n}\n");
        write(
            &root.join("app/src/util.rs"),
            "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        in_its_env(
            "source_freshness::an_edit_to_a_file_no_question_touched_reaches_the_answer_about_its_caller_in_its_env",
            &root,
            &real,
            None,
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn an_edit_to_a_file_no_question_touched_reaches_the_answer_about_its_caller_in_its_env() {
        let test =
            "source_freshness::an_edit_to_a_file_no_question_touched_reaches_the_answer_about_its_caller_in_its_env";
        let Some((root, _real)) = child_fixture(test) else { return };
        let mut engine = crate::sandboxed_engine(&root).expect("the engine starts");
        let before = index_of(&root);
        let first = engine.answer(&[], &before).expect("the first pass answers");
        assert_eq!(
            called_by_main(&before, &first),
            ["util::Thing::method"],
            "sanity: the first pass resolves `main -> Thing::method`"
        );

        write(
            &root.join("app/src/util.rs"),
            "pub struct Other;\n\nimpl Other {\n    pub fn method(&self) {}\n}\n\n\
             pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        let after = index_of(&root);
        let files = [RelPath::new("app/src/util.rs"), RelPath::new("app/src/main.rs")];
        let second = engine.answer(&files, &after).expect("the second pass answers");
        assert_eq!(
            called_by_main(&after, &second),
            ["util::Thing::method"],
            "`main -> method` must land on the edited `Thing::method`; `Other::method` is where the old \
             text had it"
        );
        println!("{}", ran_marker(test));
    }

    /// The answer about a call into an edited file follows the text the
    /// index holds, not whatever the disk holds by the time the server reads
    /// it. util.rs is edited and indexed (`Thing::method` on line 9), then
    /// rewritten once more on disk before the pass: `Thing` moves further
    /// down and `Third::method` takes line 9. The server must answer against
    /// the indexed text, which only an open document with that text makes
    /// it do; one that reads util.rs off disk answers with the newer position
    /// (or, until it reads, the old one), and neither is `Thing::method` in
    /// the index.
    ///
    /// Control: in `LspBridge::forward_watched`, drop the block that moves an
    /// indexed file on disk into the resync set.
    #[test]
    fn the_answer_about_an_edited_file_follows_the_indexed_text_not_a_newer_disk() {
        let real = real_toolchain();
        let scratch = Scratch::new("fresh-newer-disk");
        let root = scratch.0.clone();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
        write(
            &root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&root.join("app/src/main.rs"), "mod util;\n\nfn main() {\n    util::Thing.method();\n}\n");
        write(
            &root.join("app/src/util.rs"),
            "pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        in_its_env(
            "source_freshness::the_answer_about_an_edited_file_follows_the_indexed_text_not_a_newer_disk_in_its_env",
            &root,
            &real,
            None,
        );
    }

    #[test]
    #[ignore = "run by its parent test in the fixture's environment"]
    fn the_answer_about_an_edited_file_follows_the_indexed_text_not_a_newer_disk_in_its_env() {
        let test =
            "source_freshness::the_answer_about_an_edited_file_follows_the_indexed_text_not_a_newer_disk_in_its_env";
        let Some((root, _real)) = child_fixture(test) else { return };
        let mut engine = crate::sandboxed_engine(&root).expect("the engine starts");
        let before = index_of(&root);
        let first = engine.answer(&[], &before).expect("the first pass answers");
        assert_eq!(
            called_by_main(&before, &first),
            ["util::Thing::method"],
            "sanity: the first pass resolves `main -> Thing::method`"
        );

        write(
            &root.join("app/src/util.rs"),
            "pub struct Other;\n\nimpl Other {\n    pub fn method(&self) {}\n}\n\n\
             pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        let after = index_of(&root);
        write(
            &root.join("app/src/util.rs"),
            "pub struct Other;\n\nimpl Other {\n    pub fn method(&self) {}\n}\n\n\
             pub struct Third;\n\nimpl Third {\n    pub fn method(&self) {}\n}\n\n\
             pub struct Thing;\n\nimpl Thing {\n    pub fn method(&self) {}\n}\n",
        );
        let files = [RelPath::new("app/src/util.rs"), RelPath::new("app/src/main.rs")];
        let second = engine.answer(&files, &after).expect("the second pass answers");
        assert_eq!(
            called_by_main(&after, &second),
            ["util::Thing::method"],
            "`main -> method` must land on `Thing::method` as the index has it; the disk has `Third::method` \
             there and `Thing::method` further down"
        );
        println!("{}", ran_marker(test));
    }
}

/// The Linux sandbox trampoline, run as the plugin binary runs it:
/// `g-mesh-plugin-rust __g-mesh-sandbox --layers <l> --root <r> --allow <p>… -- <server> <args…>`.
///
/// A test whose layer this host cannot set up (no unprivileged user
/// namespace, Docker's default seccomp profile, a kernel without Landlock)
/// prints why and passes, except on GitHub Actions, whose ubuntu-22.04 runner
/// has both layers: there it fails, so a degraded runner cannot turn these
/// tests into skips.
#[cfg(target_os = "linux")]
mod linux_sandbox {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Output;

    use g_mesh_plugin_rust::sandbox::{self, SandboxOutcome, SandboxPolicy};
    use g_mesh_plugin_sdk::lsp::SemanticConfig;

    pub(super) const LAUNCHER: &str = env!("CARGO_BIN_EXE_g-mesh-plugin-rust");

    /// `sh` exit code for a command found but not executable.
    const DENIED: &str = "126";

    pub(super) fn on_ci() -> bool {
        std::env::var_os("GITHUB_ACTIONS").is_some()
    }

    /// Runs `server args` under the trampoline with `layers`, `root` and an
    /// `--allow` per entry of `allow`.
    fn trampoline(layers: &str, root: &Path, allow: &[&Path], server: &str, args: &[&OsStr]) -> Output {
        let mut command = Command::new(LAUNCHER);
        command.args(["__g-mesh-sandbox", "--layers", layers, "--root"]).arg(root);
        for prefix in allow {
            command.arg("--allow").arg(prefix);
        }
        command.arg("--").arg(server).args(args).output().unwrap()
    }

    fn describe(output: &Output) -> String {
        format!(
            "{}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// `Err` with the trampoline's reason when it cannot set up `layers` here.
    fn host_supports(layers: &str) -> Result<(), String> {
        let scratch = Scratch::new("probe");
        let output = trampoline(layers, &scratch.0, &[], "/bin/true", &[]);
        if output.status.success() {
            Ok(())
        } else {
            Err(describe(&output))
        }
    }

    /// Whether `test` can run its `layers` here; prints why not, or panics
    /// on GitHub Actions.
    fn require(layers: &str, test: &str) -> bool {
        match host_supports(layers) {
            Ok(()) => true,
            Err(why) if on_ci() => panic!("{test}: GitHub Actions' runner cannot set up `{layers}`: {why}"),
            Err(why) => {
                println!("{test}: skipped, this host cannot set up the `{layers}` layer: {why}");
                false
            }
        }
    }

    pub(super) fn host_runs_the_mount_layer(test: &str) -> bool {
        require("mount", test)
    }

    /// The Landlock ABI version this kernel reports, 0 without Landlock.
    fn landlock_abi() -> i64 {
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        abi.max(0)
    }

    /// A copy of `/bin/true` at `dir/true`, run once unconfined before it is
    /// returned: exec'ing a file another thread's fork still holds open for
    /// writing fails `ETXTBSY`.
    fn true_in(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let copy = dir.join("true");
        std::fs::copy("/bin/true", &copy).unwrap();
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..100 {
            match Command::new(&copy).status() {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                result => {
                    assert!(result.as_ref().is_ok_and(|status| status.success()), "{result:?}");
                    return copy;
                }
            }
        }
        panic!("{} stayed busy", copy.display())
    }

    /// `sh -c` that runs each argument as a command and prints their exit
    /// codes, space-separated.
    const RUN_EACH: &str = "codes=; for c in \"$@\"; do \"$c\"; codes=\"$codes $?\"; done; echo $codes";

    /// The exit codes of `programs`, each run under the trampoline.
    fn exit_codes_under(layers: &str, root: &Path, allow: &[&Path], programs: &[&Path]) -> (String, Output) {
        let mut args: Vec<&OsStr> = vec![OsStr::new("-c"), OsStr::new(RUN_EACH), OsStr::new("sh")];
        args.extend(programs.iter().map(|program| program.as_os_str()));
        let output = trampoline(layers, root, allow, "/bin/sh", &args);
        (String::from_utf8_lossy(&output.stdout).trim().to_string(), output)
    }

    /// In the mount layer nothing under the root is executable, while a
    /// copy of the same binary under an `--allow` prefix runs.
    #[test]
    fn the_mount_layer_execs_nothing_under_the_root() {
        let test = "linux_sandbox::the_mount_layer_execs_nothing_under_the_root";
        if !require("mount", test) {
            return;
        }
        let scratch = Scratch::new("mount-exec");
        let root = scratch.0.join("proj");
        let in_root = true_in(&root.join("target/debug"));
        let allowed = true_in(&scratch.0.join("tool"));

        let (codes, output) =
            exit_codes_under("mount", &root, &[&scratch.0.join("tool")], &[&in_root, &allowed]);
        assert_eq!(codes, format!("{DENIED} 0"), "{}", describe(&output));
    }

    /// A root inside an allowed prefix is still `noexec`: the root's step
    /// comes after its prefix's, while the rest of the prefix stays
    /// executable.
    ///
    /// Control: drop the root's step from `mount_plan`.
    #[test]
    fn the_mount_layer_execs_nothing_under_a_root_inside_an_allowed_prefix() {
        let test = "linux_sandbox::the_mount_layer_execs_nothing_under_a_root_inside_an_allowed_prefix";
        if !require("mount", test) {
            return;
        }
        let scratch = Scratch::new("mount-nested");
        let root = scratch.0.join("proj");
        let in_root = true_in(&root);
        let beside = true_in(&scratch.0.join("tool"));

        let (codes, output) = exit_codes_under("mount", &root, &[&scratch.0], &[&in_root, &beside]);
        assert_eq!(codes, format!("{DENIED} 0"), "{}", describe(&output));
    }

    /// In the mount layer the loader cannot map a shared object under the
    /// root (`LD_PRELOAD`, set inside the sandbox by `env`), and does map
    /// the same object from an allowed prefix, whose constructor writes its
    /// marker from inside the sandbox.
    #[test]
    fn the_mount_layer_maps_no_shared_object_under_the_root() {
        let test = "linux_sandbox::the_mount_layer_maps_no_shared_object_under_the_root";
        if !require("mount", test) {
            return;
        }
        let scratch = Scratch::new("mount-preload");
        let root = scratch.0.join("proj");
        let tool = scratch.0.join("tool");
        let root_mark = root.join("MARK_root_object");
        let tool_mark = root.join("MARK_allowed_object");
        let root_object = build_marker_dylib(&root.join("dylib"), &root_mark, "LD_PRELOAD from the root");
        let tool_object = build_marker_dylib(&tool, &tool_mark, "LD_PRELOAD from an allowed prefix");
        let preload = |object: &Path| format!("LD_PRELOAD={}", object.display());

        let unconfined =
            Command::new("/usr/bin/env").arg(preload(&root_object)).arg("/bin/true").status().unwrap();
        assert!(
            unconfined.success() && root_mark.is_file(),
            "sanity: the root object does not load unconfined"
        );
        std::fs::remove_file(&root_mark).unwrap();

        for object in [&root_object, &tool_object] {
            let preload = preload(object);
            let output = trampoline(
                "mount",
                &root,
                &[&tool],
                "/usr/bin/env",
                &[OsStr::new(&preload), OsStr::new("/bin/true")],
            );
            assert!(output.status.success(), "{preload}: {}", describe(&output));
        }
        assert!(!root_mark.exists(), "a process in the mount layer mapped a shared object under the root");
        assert!(
            tool_mark.is_file(),
            "the object under the allowed prefix did not load: the test proves nothing"
        );
    }

    /// The server cannot undo the mount layer: even as uid 0 (root in its
    /// user namespace) a `mount -o remount,bind,exec` of the root leaves it
    /// `noexec`. Discriminating only when the test runs as uid 0, where the
    /// exec'd server would otherwise keep the namespace's capabilities.
    ///
    /// Control (as uid 0): make `drop_capabilities` return `Ok(())` at once.
    #[test]
    fn a_root_server_cannot_remount_the_project_root_exec() {
        let test = "linux_sandbox::a_root_server_cannot_remount_the_project_root_exec";
        if !require("mount", test) {
            return;
        }
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            println!("{test}: runs as uid {uid}; it discriminates a capability drop only as uid 0");
        }
        let scratch = Scratch::new("remount");
        let root = scratch.0.join("proj");
        let in_root = true_in(&root);
        let script = "mount -o remount,bind,exec \"$1\" 2>&1; \"$2\"; echo \"rc=$?\"";
        let output = trampoline(
            "mount",
            &root,
            &[],
            "/bin/sh",
            &[OsStr::new("-c"), OsStr::new(script), OsStr::new("sh"), root.as_os_str(), in_root.as_os_str()],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.trim_end().ends_with(&format!("rc={DENIED}")), "{}", describe(&output));
    }

    /// Landlock alone allows exec only under the allowed prefixes: a binary
    /// outside them does not run, one inside does.
    ///
    /// Control: give `landlock_layer` an `Execute` rule on `/`.
    #[test]
    fn landlock_alone_denies_exec_outside_the_allowlist() {
        let test = "linux_sandbox::landlock_alone_denies_exec_outside_the_allowlist";
        if !require("landlock", test) {
            return;
        }
        let scratch = Scratch::new("landlock-exec");
        let root = scratch.0.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let outside = true_in(&scratch.0.join("elsewhere"));
        let allowed = true_in(&scratch.0.join("tool"));

        let (codes, output) =
            exit_codes_under("landlock", &root, &[&scratch.0.join("tool")], &[&outside, &allowed]);
        assert_eq!(codes, format!("{DENIED} 0"), "{}", describe(&output));
    }

    /// Landlock alone (ABI >= 4) refuses a TCP connect that the same shell
    /// makes unconfined.
    #[test]
    fn landlock_alone_refuses_tcp_connect() {
        let test = "linux_sandbox::landlock_alone_refuses_tcp_connect";
        if !require("landlock", test) {
            return;
        }
        let abi = landlock_abi();
        if abi < 4 {
            assert!(
                !on_ci(),
                "{test}: GitHub Actions' kernel reports Landlock ABI {abi}, which has no TCP rights"
            );
            println!("{test}: skipped, Landlock ABI {abi} has no TCP rights (needs 4)");
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected");
        let unconfined = Command::new("/bin/bash").args(["-c", &script]).output().unwrap();
        assert!(
            String::from_utf8_lossy(&unconfined.stdout).contains("connected"),
            "sanity: bash does not connect unconfined: {}",
            describe(&unconfined)
        );

        let scratch = Scratch::new("landlock-tcp");
        let output =
            trampoline("landlock", &scratch.0, &[], "/bin/bash", &[OsStr::new("-c"), OsStr::new(&script)]);
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("connected") && !output.status.success(),
            "a process under Landlock connected over TCP: {}",
            describe(&output)
        );
    }

    /// `confine` picks the tier this host's layers allow: both -> full,
    /// mount only -> mount-only, Landlock only -> `Partial`, neither ->
    /// `Failed` naming both reasons and leaving the config alone. GitHub
    /// Actions must get the full tier.
    #[test]
    fn confine_picks_the_tier_this_host_supports() {
        let mount = host_supports("mount");
        let landlock = host_supports("landlock");
        let scratch = Scratch::new("tier");
        let policy = SandboxPolicy::new(&scratch.0, [], []);
        let untouched = SemanticConfig::new(Path::new("/bin/true"));
        let mut config = untouched.clone();

        let outcome = sandbox::confine(&mut config, &policy, Path::new(LAUNCHER));
        println!("mount layer: {mount:?}\nLandlock layer: {landlock:?}\noutcome: {outcome:?}");
        if on_ci() {
            assert_eq!(outcome, SandboxOutcome::Confined { backend: "noexec mounts + Landlock" });
        }
        let layers = match (mount.is_ok(), landlock.is_ok()) {
            (true, true) => {
                assert_eq!(outcome, SandboxOutcome::Confined { backend: "noexec mounts + Landlock" });
                Some("mount,landlock")
            }
            (true, false) => {
                assert_eq!(outcome, SandboxOutcome::Confined { backend: "noexec mounts" });
                Some("mount")
            }
            (false, true) => {
                assert!(
                    matches!(&outcome, SandboxOutcome::Partial { backend: "Landlock", gap }
                        if gap.contains("noexec mount namespace cannot be set up")),
                    "{outcome:?}"
                );
                Some("landlock")
            }
            (false, false) => {
                assert!(
                    matches!(&outcome, SandboxOutcome::Failed(reason)
                        if reason.contains("noexec mount namespace cannot be set up")
                            && reason.contains("Landlock cannot be applied")),
                    "{outcome:?}"
                );
                None
            }
        };
        match layers {
            Some(layers) => {
                assert_eq!(config.command, Path::new(LAUNCHER));
                assert_eq!(config.args.get(1..3), Some(&["--layers".to_string(), layers.to_string()][..]));
                assert_eq!(config.args.last().map(String::as_str), Some("/bin/true"));
            }
            None => assert_eq!(config, untouched, "a failed confine changed the config"),
        }
    }
}
