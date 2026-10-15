//! The pyright tier runs no interpreter and no pyright the indexed project
//! supplies (`docs/design/GM-513-semantic-servers-no-project-code.md`,
//! fixture `pfx`).
//!
//! Each test builds a hostile project and runs the plugin's own
//! [`semantic::engine`] with the shipped manifest and its bare
//! `pyright-langserver` command, so resolution and the settings sent to
//! pyright are the production ones:
//!
//! - a real `python3 -m venv` whose interpreters are replaced by a logging
//!   shim, and a logging `python`/`python3` first on `PATH`: neither may run;
//!   the venv's packages must still resolve (the server really answered);
//! - the same with a project `pyrightconfig.json` naming that venv;
//! - a logging `node_modules/.bin/pyright` in the project, with no pyright
//!   on `PATH` and a logging `npx` on `PATH`: neither the project's copy nor
//!   `npx` may be probed or run, so no server resolves at all;
//! - a project `node_modules/pyright` package with a *real* `npx` and `node`
//!   on `PATH` (no pyright): `npx` there would run the project's package, so
//!   it must never run, and the tier is off with the remedy.
//!
//! The shims are shell scripts, so the whole file is unix only. Each test
//! builds its fixture in this process, then re-runs this test binary as a
//! child limited to an `#[ignore]`d `<test>_in_its_env` test, with the
//! fixture's `PATH`, manifest, working directory and scratch path. The child
//! runs the engine and the assertions. No test changes this process's
//! environment or working directory, so the tests are independent of order.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh_plugin_python::extractor::PythonExtractor;
use g_mesh_plugin_python::project::{ProjectContext, EXCLUDE_DIRS};
use g_mesh_plugin_python::semantic;
use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{walk_project, Extractor, SdkIndex, MANIFEST_PATH_ENV};

/// The scratch directory a child test reads its fixture from. Only the
/// parent test sets it.
const SCRATCH_ENV: &str = "G_MESH_PY_GM513_SCRATCH";

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
        "these tests drive a real pyright and {} has none: install it with `scripts/test-deps.sh pyright`",
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
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("g-mesh-py-gm513-{name}-{}", std::process::id()));
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

/// An executable script that appends its argv to `log` and then runs `exec`
/// (nothing more when `exec` is empty).
fn shim(path: &Path, log: &Path, exec: &str) {
    let _ = std::fs::remove_file(path);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    write(path, &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{exec}\n", log.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(program: &Path, args: &[&str], cwd: &Path) {
    let output = Command::new(program).args(args).current_dir(cwd).output().unwrap();
    assert!(
        output.status.success(),
        "{} {args:?}: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The logs that exist, with their contents.
fn written(logs: &[PathBuf]) -> Vec<String> {
    logs.iter()
        .filter_map(|log| std::fs::read_to_string(log).ok().map(|text| format!("{}:\n{text}", log.display())))
        .collect()
}

/// The line a child test prints once its assertions have passed.
fn ran_marker(test: &str) -> String {
    format!("{test}: ran in its environment")
}

/// Runs the ignored test `test` of this binary in a child process with `path`
/// as `PATH`, the shipped manifest, no `VIRTUAL_ENV`, `cwd` as working
/// directory and `scratch` in [`SCRATCH_ENV`]; asserts that it ran and passed.
fn in_its_env(test: &str, path: &[PathBuf], cwd: &Path, scratch: &Path) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PATH", std::env::join_paths(path).unwrap())
        .env(MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))
        .env_remove("VIRTUAL_ENV")
        .env(SCRATCH_ENV, scratch)
        .current_dir(cwd)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(&ran_marker(test)),
        "{test} did not pass in its environment ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        output.status
    );
}

/// The fixture of a child test, or `None` when the test was started without
/// its parent (`--ignored`/`--run-ignored`), which then tests nothing.
fn scratch_from_env(test: &str) -> Option<PathBuf> {
    let scratch = std::env::var_os(SCRATCH_ENV).map(PathBuf::from);
    if scratch.is_none() {
        println!("{test}: skipped, it runs only as the child of its parent test, which sets {SCRATCH_ENV}");
    }
    scratch
}

/// `pfx`, run by `child` in its environment. With `config_names_the_venv`,
/// the project also has a `pyrightconfig.json` pointing pyright at its venv.
fn pyright_runs_no_interpreter(config_names_the_venv: bool, child: &str) {
    let installed = installed_bin();
    let real_python = absolute("python3");
    let node_dir = absolute("node").parent().unwrap().to_path_buf();
    let scratch = Scratch::new(if config_names_the_venv { "config" } else { "plain" });
    let root = scratch.0.join("project");
    let logs_dir = scratch.0.join("logs");
    let venv_log = logs_dir.join("venv-python");
    let path_log = logs_dir.join("path-python");

    write(&root.join("main.py"), MAIN_PY);
    write(&root.join("lib.py"), LIB_PY);
    if config_names_the_venv {
        write(&root.join("pyrightconfig.json"), "{\"venvPath\": \".\", \"venv\": \".venv\"}\n");
    }
    run(&real_python, &["-m", "venv", "--without-pip", ".venv"], &root);
    let lib = root.join(".venv/lib");
    let site_packages = std::fs::read_dir(&lib)
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join("site-packages"))
        .find(|path| path.is_dir())
        .expect("the venv has a site-packages");
    write(&site_packages.join("vpkg/__init__.py"), VPKG_PY);
    let exec_python = format!("exec '{}' \"$@\"", real_python.display());
    for entry in std::fs::read_dir(root.join(".venv/bin")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("python") {
            shim(&entry.path(), &venv_log, &exec_python);
        }
    }
    let shims = scratch.0.join("shims");
    for name in ["python", "python3"] {
        shim(&shims.join(name), &path_log, &exec_python);
    }

    in_its_env(
        child,
        &[shims, installed, node_dir, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        &root,
        &scratch.0,
    );
}

/// The child of [`pyright_runs_no_interpreter`]: a whole-project pass over
/// `pfx` and the assertions.
fn pyright_answers_without_interpreter(test: &str) {
    let Some(scratch) = scratch_from_env(test) else { return };
    let root = scratch.join("project");
    let logs_dir = scratch.join("logs");
    let venv_log = logs_dir.join("venv-python");
    let path_log = logs_dir.join("path-python");

    // Sanity: both interpreters are the shims, and they log.
    run(&root.join(".venv/bin/python"), &["-c", "pass"], &root);
    run(Path::new("python3"), &["-c", "pass"], &root);
    assert_eq!(written(&[venv_log.clone(), path_log.clone()]).len(), 2, "sanity: both shims log when run");
    let _ = std::fs::remove_file(&venv_log);
    let _ = std::fs::remove_file(&path_log);

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

    // Sanity: the server answered, and the venv's package resolved -
    // `made.method()` is typed only through `vpkg.vfunc`.
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

    assert_eq!(
        written(&[venv_log, path_log]),
        Vec::<String>::new(),
        "pyright ran a Python interpreter (the project venv's or one from PATH)"
    );
    println!("{}", ran_marker(test));
}

#[test]
fn pyright_runs_neither_the_venv_python_nor_one_from_path() {
    pyright_runs_no_interpreter(false, "pyright_runs_neither_the_venv_python_nor_one_from_path_in_its_env");
}

#[test]
#[ignore = "run by its parent test in the fixture's environment"]
fn pyright_runs_neither_the_venv_python_nor_one_from_path_in_its_env() {
    pyright_answers_without_interpreter("pyright_runs_neither_the_venv_python_nor_one_from_path_in_its_env");
}

#[test]
fn a_pyrightconfig_naming_the_venv_runs_no_interpreter_either() {
    pyright_runs_no_interpreter(
        true,
        "a_pyrightconfig_naming_the_venv_runs_no_interpreter_either_in_its_env",
    );
}

#[test]
#[ignore = "run by its parent test in the fixture's environment"]
fn a_pyrightconfig_naming_the_venv_runs_no_interpreter_either_in_its_env() {
    pyright_answers_without_interpreter(
        "a_pyrightconfig_naming_the_venv_runs_no_interpreter_either_in_its_env",
    );
}

#[test]
fn the_projects_own_pyright_is_never_probed_or_run() {
    let installed = installed_bin();
    let node = absolute("node");
    let scratch = Scratch::new("local");
    let root = scratch.0.join("project");
    let logs_dir = scratch.0.join("logs");
    let local_log = logs_dir.join("project-pyright");
    let npx_log = logs_dir.join("npx");

    write(&root.join("main.py"), LIB_PY);
    let local_bin = root.join("node_modules/.bin");
    for name in ["pyright", "pyright-langserver"] {
        let real = installed.join(name).canonicalize().unwrap();
        shim(
            &local_bin.join(name),
            &local_log,
            &format!("exec '{}' '{}' \"$@\"", node.display(), real.display()),
        );
    }
    let shims = scratch.0.join("shims");
    shim(&shims.join("npx"), &npx_log, "exit 1");

    in_its_env(
        "the_projects_own_pyright_is_never_probed_or_run_in_its_env",
        &[shims, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        &root,
        &scratch.0,
    );
}

#[test]
#[ignore = "run by its parent test in the fixture's environment"]
fn the_projects_own_pyright_is_never_probed_or_run_in_its_env() {
    let test = "the_projects_own_pyright_is_never_probed_or_run_in_its_env";
    let Some(scratch) = scratch_from_env(test) else { return };
    let root = scratch.join("project");
    let logs_dir = scratch.join("logs");
    let local_log = logs_dir.join("project-pyright");
    let npx_log = logs_dir.join("npx");
    let local_bin = root.join("node_modules/.bin");

    // Sanity: the project's copy is a working pyright and logs, and no other
    // pyright is reachable.
    run(&local_bin.join("pyright"), &["--version"], &root);
    assert_eq!(written(std::slice::from_ref(&local_log)).len(), 1, "sanity: the project's pyright logs");
    let _ = std::fs::remove_file(&local_log);
    assert!(Command::new("pyright").arg("--version").output().is_err(), "sanity: no pyright on PATH");

    let engine = semantic::engine(&root);

    assert_eq!(
        written(std::slice::from_ref(&local_log)),
        Vec::<String>::new(),
        "the plugin probed or ran the project's node_modules/.bin/pyright"
    );
    assert!(!npx_log.exists(), "npx was run: in the project it runs the project's own node_modules/pyright");
    assert!(engine.is_err(), "with no pyright on PATH, nothing may resolve");
    println!("{}", ran_marker(test));
}

/// The test above stubs `npx` with `exit 1`, so it cannot see what a real
/// `npx` does in the project. Here `npx` is the real one
/// (behind a logging shim), with `node` beside it and no pyright on `PATH`,
/// and the project ships `node_modules/pyright` whose bins write a marker.
/// `npx --yes --package pyright pyright …` run with the project as cwd finds
/// that local package and runs it - checked as a sanity step - so the plugin
/// must not reach `npx` at all, and must report the tier off with the remedy.
///
/// Control F1: in `plugins/sdk/src/lsp/resolve.rs`, `npm_candidates_on_path`,
/// append an `npx --yes --package pyright <bin>` candidate for a bare name.
#[test]
fn a_real_npx_in_the_project_never_runs_the_projects_pyright() {
    let node = absolute("node");
    let npx = absolute("npx");
    let scratch = Scratch::new("npx");
    let root = scratch.0.join("project");
    let logs_dir = scratch.0.join("logs");
    let marker = logs_dir.join("project-pyright-package");
    let npx_log = logs_dir.join("npx");

    write(&root.join("main.py"), LIB_PY);
    let package = root.join("node_modules/pyright");
    write(
        &package.join("package.json"),
        "{\"name\": \"pyright\", \"version\": \"1.1.999\", \"bin\": {\"pyright\": \"cli.js\", \
         \"pyright-langserver\": \"langserver.js\"}}\n",
    );
    std::fs::create_dir_all(&logs_dir).unwrap();
    let bin_dir = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    for (bin, file) in [("pyright", "cli.js"), ("pyright-langserver", "langserver.js")] {
        write(
            &package.join(file),
            &format!(
                "#!/usr/bin/env node\nrequire('fs').appendFileSync({:?}, process.argv.slice(1).join(' ') + \
                 '\\n');\nconsole.log('pyright 1.1.999');\n",
                marker.display().to_string()
            ),
        );
        std::fs::set_permissions(package.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(format!("../pyright/{file}"), bin_dir.join(bin)).unwrap();
    }
    let shims = scratch.0.join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(&node, shims.join("node")).unwrap();
    shim(&shims.join("npx"), &npx_log, &format!("exec '{}' \"$@\"", npx.display()));

    in_its_env(
        "a_real_npx_in_the_project_never_runs_the_projects_pyright_in_its_env",
        &[shims, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        &root,
        &scratch.0,
    );
}

#[test]
#[ignore = "run by its parent test in the fixture's environment"]
fn a_real_npx_in_the_project_never_runs_the_projects_pyright_in_its_env() {
    let test = "a_real_npx_in_the_project_never_runs_the_projects_pyright_in_its_env";
    let Some(scratch) = scratch_from_env(test) else { return };
    let root = scratch.join("project");
    let logs_dir = scratch.join("logs");
    let marker = logs_dir.join("project-pyright-package");
    let npx_log = logs_dir.join("npx");

    // Sanity: no pyright on PATH, and the real npx in the project runs the
    // project's package (the probe the npx candidate would run).
    for name in ["pyright", "pyright-langserver"] {
        assert!(Command::new(name).arg("--version").output().is_err(), "sanity: no {name} on PATH");
    }
    run(Path::new("npx"), &["--yes", "--package", "pyright", "pyright", "--version"], &root);
    assert!(marker.exists(), "sanity: npx in the project runs the project's node_modules/pyright");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&npx_log);

    let engine = semantic::engine(&root);

    assert_eq!(
        written(&[marker, npx_log]),
        Vec::<String>::new(),
        "the plugin ran npx, which ran the project's own node_modules/pyright"
    );
    let message = format!("{:#}", engine.err().expect("with no pyright on PATH, the semantic tier is off"));
    assert!(
        message.contains("npm install -g pyright") && message.contains("PATH"),
        "remedy missing: {message}"
    );
    println!("{}", ran_marker(test));
}
