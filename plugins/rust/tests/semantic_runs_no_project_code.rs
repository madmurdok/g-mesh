//! The rust-analyzer tier runs no code the indexed project supplies.
//!
//! A hostile workspace (`docs/design/GM-513-semantic-servers-no-project-code.md`,
//! fixture `rfx`) is indexed through the plugin's own [`semantic::engine`],
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
//!   `MARK_codegen_backend` when any `rustc` loads it (unix, when a C
//!   compiler builds the dylib);
//! - a project `rust-analyzer.toml` turns build scripts and proc macros back
//!   on.
//!
//! Before the pass, the shims are run the way an unpinned toolchain would
//! run them (the rustup proxies, in the fixture) and must log: the hostile
//! config is in force. After it, a receiver call must have resolved across
//! files (the server really answered), and no marker, no log and no
//! `target/` may exist.
//!
//! The test process's own environment is scrubbed of everything cargo and
//! rustup set for it (`RUSTUP_TOOLCHAIN`, `CARGO`, `RUSTC_WRAPPER`, ...), and
//! `PATH` starts with a directory of rustup proxies: otherwise the inherited
//! pin would hide a missing one, and `rust-analyzer` on `PATH` would be the
//! real binary rather than the proxy a user's daemon finds. The test makes
//! that directory itself (`cargo`, `rustc`, `rust-analyzer` linked to the
//! `rustup` on `PATH`, which dispatches on its program name exactly as an
//! installed proxy does) instead of assuming the directory holding `rustup`
//! also holds the proxies: on a Homebrew rustup (the macOS arm64 runner) it
//! does not (GM-574). The working directory
//! is the fixture, as a daemon started there would have it: the proxy picks
//! its toolchain from the working directory.
//!
//! One test in this binary, because it rewrites the process environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh_plugin_rust::extractor::RustExtractor;
use g_mesh_plugin_rust::project::{ProjectContext, EXCLUDE_DIRS};
use g_mesh_plugin_rust::semantic;
use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{walk_project, Extractor, RelPath, SdkIndex, MANIFEST_PATH_ENV};

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-rust-gm513-{name}-{}", std::process::id()));
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
    _proxies: Scratch,
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
        _proxies: proxies,
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

/// Clears what cargo and rustup set for this test process, and puts the
/// rustup proxies first on `PATH`.
fn scrub_environment(proxy_dir: &Path) {
    let names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            (name.starts_with("CARGO") && name != "CARGO_HOME")
                || name.starts_with("RUSTC")
                || name.starts_with("RUSTDOC")
                || name == "RUSTFLAGS"
                || name == "RUSTUP_TOOLCHAIN"
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
}

/// Every log and marker a project hook writes, by what wrote it.
struct Fixture {
    root: PathBuf,
    marks: Vec<PathBuf>,
    #[cfg(unix)]
    logs: Vec<PathBuf>,
}

impl Fixture {
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

/// A shared library whose load-time constructor writes `mark`: what a
/// project could ship prebuilt and name as a `-Zcodegen-backend`, which
/// `rustc` loads before it checks the backend's symbol. `None`, with a
/// message, when no C compiler `cc` builds it.
#[cfg(unix)]
fn build_marker_dylib(dir: &Path, mark: &Path) -> Option<PathBuf> {
    let source = dir.join("marker.c");
    write(
        &source,
        &format!(
            "#include <stdio.h>\n__attribute__((constructor)) static void fire(void) {{\n    \
             FILE *f = fopen({:?}, \"a\");\n    if (f) {{ fputs(\"x\\n\", f); fclose(f); }}\n}}\n",
            mark.to_string_lossy()
        ),
    );
    let dylib = dir.join(if cfg!(target_os = "macos") { "marker.dylib" } else { "marker.so" });
    let shared: &[&str] = if cfg!(target_os = "macos") { &["-dynamiclib"] } else { &["-shared", "-fPIC"] };
    match Command::new("cc").args(shared).arg("-o").arg(&dylib).arg(&source).output() {
        Ok(output) if output.status.success() => Some(dylib),
        result => {
            println!(
                "SKIPPED the `[target.*] rustflags` -Zcodegen-backend hook: `cc` did not build the marker \
                 dylib ({result:?})"
            );
            None
        }
    }
}

/// `rfx`: workspace `app` + proc macro `mac`, plus (unix) the toolchain file,
/// the cargo config and their shims.
fn write_fixture(root: &Path, #[allow(unused_variables)] real: &RealToolchain) -> Fixture {
    let mark_buildrs = root.join("MARK_buildrs");
    let mark_procmacro = root.join("MARK_procmacro");
    #[cfg(unix)]
    let mark_backend = root.join("MARK_codegen_backend");
    #[allow(unused_mut)]
    let mut marks = vec![mark_buildrs.clone(), mark_procmacro.clone()];
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
        if let Some(backend) = build_marker_dylib(&shims, &mark_backend) {
            config.push_str(&format!(
                "\n[target.{}]\nrustflags = [{:?}]\n",
                host_triple(&real.rustc),
                format!("-Zcodegen-backend={}", backend.to_string_lossy())
            ));
            marks.push(mark_backend);
        }
        write(&root.join(".cargo/config.toml"), &config);
    }

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

    if fixture.marks.contains(&mark_backend) {
        reset();
        run(real.proxy_dir.join("cargo"), metadata, &[("RUSTC_BOOTSTRAP", "1")], false);
        assert!(
            mark_backend.is_file(),
            "sanity: the unpinned `RUSTC_BOOTSTRAP=1 cargo metadata` in the fixture never loaded the \
             project's `-Zcodegen-backend` dylib, so this test would prove nothing about it"
        );
    }
    reset();
    let _ = std::fs::remove_file(fixture.root.join("Cargo.lock"));
    let _ = std::fs::remove_dir_all(fixture.root.join("target"));
}

#[test]
fn rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names() {
    let real = real_toolchain();
    let scratch = Scratch::new("project");
    let fixture = write_fixture(&scratch.0, &real);
    scrub_environment(&real.proxy_dir);
    std::env::set_var(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    std::env::set_current_dir(&fixture.root).unwrap();

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

    let mut engine = semantic::engine(&fixture.root).expect("the engine resolves a rust-analyzer");
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
}
