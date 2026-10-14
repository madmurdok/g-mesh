//! The Python plugin's semantic tier: finding pyright, telling it where the
//! project's venv packages are, and handing it to the SDK's generic
//! [`LspBridge`].
//!
//! pyright runs no code the project supplies: no interpreter (it is handed
//! [`no_interpreter`] as `python.pythonPath` and the venv's site-packages as
//! `python.analysis.extraPaths`), no server binary from the project's
//! `node_modules/.bin`, and no `npx`, which would run the project's own
//! `node_modules/pyright` (Decision 1)
//! ([ADR 0032](../../../docs/adr/0032-semantic-tiers-execute-no-project-code.md)).
//!
//! Everything about *driving* a language server is the SDK's
//! (`plugins/sdk/src/lsp/`, whose module doc carries the decision table).
//! What is left for a language's own plugin is which binary to run and what
//! to configure it with - and for pyright both of those turned out to differ
//! from the rust-analyzer precedent `plugins/rust/src/semantic.rs` set, in
//! ways that are measured here rather than assumed.
//!
//! # Decision 1: one place to look, probed rather than believed
//!
//! GM-290's rule was "find it, then prove it", and the proof was
//! `<candidate> --version`. The rule survives; the proof does not, because
//! **`pyright-langserver` has no `--version`**. Measured on 1.1.414:
//!
//! ```text
//! $ node_modules/.bin/pyright-langserver --version
//! Error: Connection input stream is not set. Use arguments of createConnection
//! or set command line parameters: '--node-ipc', '--stdio' or '--socket={number}'
//! $ echo $?
//! 1
//! ```
//!
//! The language server is a *different entry point of the same npm package*
//! from the `pyright` CLI (`package.json`'s `"bin"` names both,
//! `langserver.index.js` and `index.js`), and only the CLI answers
//! `--version`. So each candidate is probed through its **CLI twin**: the
//! same resolution, one directory, with
//! `pyright-langserver` replaced by `pyright`. What that proves is what the
//! probe is for - that a real, runnable pyright package is behind this
//! spelling - and the version it prints (`pyright 1.1.414`) goes in the log
//! line, because "which pyright answered this pass" is the first thing anyone
//! asks of a semantic index that looks wrong.
//!
//! The one place, and what it is for:
//!
//! 1. **`PATH`.** The manifest's bare `pyright-langserver`, looked up by the
//!    operating system - `Command::new` does that on every platform. This is
//!    a global `npm i -g pyright`, a `pipx install pyright`, or a distro
//!    package. On Windows this candidate needs more than a plain lookup - see
//!    Decision 1b, right after this list.
//!
//! There used to be a second place, `npx --yes --package pyright
//! pyright-langserver`, last. GM-513 removed it: `npx` run with the project
//! as its cwd (the server's cwd is the root) runs the project's own
//! `node_modules/pyright` when one is there - measured with npm 9.8.1, a
//! directory holding only `node_modules/pyright/{package.json,evil.js}` and
//! its `.bin` links, no `pyright` on `PATH`: `npx --yes --package pyright
//! pyright --version` ran the project's script. It also contacts the
//! registry on every start, and the registry is the project's `.npmrc`
//! choice (GM-512). The owner's decision (GM-512, D2): python stops using
//! `npx`. A machine with no `pyright-langserver` on `PATH` loses the semantic
//! tier (the structural index stays), and the resolution failure's log line
//! names the remedy, `npm install -g pyright`. While it existed, its argv had
//! to be `--package pyright pyright-langserver`, not `npx --yes
//! pyright-langserver` (a 404: `pyright-langserver` is a bin of the `pyright`
//! package, not a package), and its probe had to differ from that argv only
//! in the bin name - a probe that proves the package rather than the argv
//! shape proves the wrong thing. TypeScript resolves vtsls the same way,
//! `PATH` only (ADR 0032, D7).
//!
//! A `command` that is a *path* is not searched around, exactly as GM-290
//! decided: someone who wrote a path into the manifest meant that file. It is
//! still probed, through the CLI twin beside it when its name ends in
//! `-langserver`, so a path that is wrong fails with one log line rather than
//! as a server that dies at handshake.
//!
//! # Decision 1b: `Command::new` does not walk `PATHEXT`, so this module does
//!
//! GM-341: none of the candidates above start on Windows when pyright
//! is installed the ordinary way, `npm install pyright`. npm's Windows shim
//! for a bin is not a `.exe` - `bin-links` writes `<name>.cmd` (a `cmd.exe`
//! batch file usable from `cmd.exe`, PowerShell, and this process) alongside
//! an extensionless POSIX shebang script and a `<name>.ps1`, neither of which
//! `CreateProcessW` can execute directly. `std::process::Command` on Windows
//! is a thin wrapper over `CreateProcessW`, and that API's own extension
//! completion is narrower than a shell's: given a bare name it tries the name
//! as given and then `<name>.exe`, nothing else. `PATHEXT` is `cmd.exe`'s own
//! rule, consulted by the shell that parses a typed command line - not by
//! `CreateProcess`, and not by anything standing between it and `Command`.
//! Concretely: the `PATH` candidate looks for `pyright-langserver.exe`
//! (and the `npx` candidate this module had then failed the same way,
//! because `npx` itself ships as `npx.cmd` beside `node.exe` on a machine
//! whose Node came from the official Windows installer). Not measured on a
//! Windows host - there is none here - but is documented `CreateProcess`
//! behaviour plus the CI evidence this task was cut from: CI's own step had
//! already run `npm install pyright` successfully, and the conformance test -
//! which carried the same naive candidate list this module did - still
//! reported no usable pyright.
//!
//! The fix is [`lsp::script_spellings`]: every final candidate path, and a path the manifest names explicitly, is tried both as
//! written and, when it has no extension of its own, with `.cmd` appended.
//! Only `.cmd` - not `.bat`, which current npm does not write (listing a
//! spelling nothing installs would only make a failure message name a
//! candidate that can never exist), and not `.ps1`, which `CreateProcessW`
//! cannot execute directly either: running one needs an explicit
//! `powershell -File` wrapper this module does not add, so trying the bare
//! `.ps1` path would add a candidate that can never succeed rather than a
//! real fallback. No candidate is special-cased - each goes through the same
//! expansion, which is what makes this a fix rather than a list of two
//! spellings with a third exception waiting to be discovered the same way
//! this one was. The
//! extension list is a parameter, not a fact [`lsp::script_spellings`] reads
//! for itself - [`lsp::HOST_SCRIPT_EXTENSIONS`] is the one place that picks
//! [`lsp::WINDOWS_SCRIPT_EXTENSIONS`] under `#[cfg(windows)]`, the same shape
//! `daemon::manifest::resolve_exe_suffix` uses around
//! `daemon::manifest::exe_suffixed` and its `EXE_SUFFIX` - so the Windows arm
//! is exercised by this module's own tests from this macOS host, rather than
//! sitting behind a `#[cfg(windows)]` nobody here can run. GM-335 is what a
//! test that never runs costs.
//!
//! # Decision 2: the probe is bounded
//!
//! rust-analyzer's `--version` is about twenty milliseconds of local work.
//! Measured here, an installed `pyright --version` is 0.89s (node start-up);
//! the `npx` candidate this module had until GM-513 was 4.94s when it had to
//! populate npm's `_npx` cache and longer when it had to download. The
//! factory runs *inside* the pass core is timing, so [`PROBE_BUDGET`] bounds it and the child is killed
//! rather than waited on. The SDK's probe does this for every plugin, so
//! rust-analyzer's is bounded too.
//!
//! # Decision 3: pyright takes no `initializationOptions`
//!
//! The task that scheduled this work says "initialization options (basic type
//! checking; the project's own venv if one is configured)", and that is the
//! wrong channel. Measured, same fixture, one variable changed
//! (`plugins/sdk/src/lsp/config.rs`'s module doc has the numbers):
//! `initializationOptions` carrying `python.analysis.typeCheckingMode` changes
//! **nothing**, and the identical object returned from a
//! `workspace/configuration` request changes the diagnostics and writes
//! `Setting pythonPath for service …` into the server's own log. pyright asks
//! for sections `python` and `pyright`, once, immediately after
//! `initialized`, and reads its settings from nowhere else.
//!
//! That is why GM-299 added [`SemanticConfig::settings`] to the SDK. What this
//! module puts in it is three things:
//!
//! - `python.analysis.typeCheckingMode = "basic"`, from the manifest. It
//!   changes only which diagnostics pyright computes - which this bridge never
//!   reads - so it is a cost setting, not a correctness one, and the measured
//!   effect on this fixture is 4 diagnostics at `basic` against 39 at
//!   `strict`. Inference, which is the part that answers `definition`, is the
//!   same at every level.
//! - `python.pythonPath` = [`no_interpreter`], always, and
//!   `python.analysis.extraPaths` = [`site_packages`], when the project has a
//!   venv. pyright executes any `pythonPath` it is given and falls back to
//!   `python3`/`python` on `PATH` when it is unset; a path that cannot exist
//!   makes it run none and resolve imports from `extraPaths` alone.
//! - `python.analysis.exclude`, from the SDK's `walk_scope`: the directories
//!   the walk leaves out, so pyright does not scan a gitignored build tree
//!   before its first answer. Only `exclude`, never `include`, and taken once
//!   per server - see
//!   [ADR 0006](../../../docs/adr/0006-language-server-scan-scope.md).
//!
//! # Decision 4: no implementation sweep, because pyright has no such request
//!
//! `plugins/rust` sets `implementation_kinds = ["trait"]` and the bridge asks
//! `textDocument/implementation` on every trait. The obvious Python reading is
//! `implementation_kinds = ["class"]`, and it is wrong: pyright does not
//! implement the request. Its `initialize` answer carries
//! `definitionProvider`, `declarationProvider`, `typeDefinitionProvider` and
//! `referencesProvider` and **no `implementationProvider`**, and asking anyway
//! is answered
//!
//! ```text
//! {"code":-32601,"message":"Unhandled method textDocument/implementation"}
//! ```
//!
//! which the bridge reads as `Poll::Failed` - one refused question per class,
//! every one of them marking its file uncovered and the whole pass
//! **incomplete**. That is not a missing feature degrading gracefully; it is a
//! manifest key that would turn a working pass into a permanently failing one.
//! So `implementation_kinds` is empty, `find_implementations` for Python stays
//! exactly as structural as it was, and the case that needed the sweep - a
//! subclass whose base arrived through a star import,
//! `conformance/project/pkg/dynamic.py` - stays a documented gap. See
//! `plugins/python/README.md`, which says so in the words a user of the index
//! needs.
//!
//! # Why the failure is the factory's and not the bridge's
//!
//! [`crate::extractor`]'s tier never fails; this one can. The SDK gives the
//! two failures different shapes on purpose
//! (`g_mesh_plugin_sdk::semantic::LazyEngine`): a factory that returns `Err`
//! is reported **once**, at the first `semanticPass`, and the language is
//! structural-only for the rest of the process's life, every later pass
//! answering an empty, *incomplete* diff without starting anything. That is
//! exactly the degradation this task asks for ("log once and an empty diff"),
//! and it is strictly better than letting the bridge discover the problem,
//! which costs a process spawn per pass to reach the same conclusion.
//!
//! Incomplete, not complete, is the load-bearing half: it leaves
//! `language_state.semanticPassAt` unset, which is what keeps Python's
//! answers carrying a `provenance` block that says the semantic tier is
//! missing (`core::mcp::provenance`). Installing pyright and
//! restarting the daemon then gets the pass; a completed-but-empty pass would
//! have recorded "done" and never asked again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use g_mesh_plugin_sdk::lsp::{
    self, npm_candidates_on_path, Candidate, LspBridge, NpmServer, Resolved, SemanticConfig,
    HOST_SCRIPT_EXTENSIONS, PROBE_BUDGET,
};
use g_mesh_plugin_sdk::{walk_scope, SemanticEngine, WalkScope};

/// The language id this plugin speaks, for `didOpen` and for log lines.
const LANGUAGE: &str = "python";

/// The npm package's language-server entry point, and the CLI entry point
/// beside it that a probe can actually ask for a version - see this module's
/// doc, Decision 1. Both are `"bin"` entries of the same package, so finding
/// one is finding the other.
const SERVER_BIN: &str = "pyright-langserver";
const CLI_BIN: &str = "pyright";

/// pyright as an npm package: the server is probed through its CLI twin,
/// because `pyright-langserver` has no `--version` (this module's doc,
/// Decision 1).
const PYRIGHT: NpmServer<'static> = NpmServer { bin: SERVER_BIN, twin: cli_twin };

/// The virtual-environment directory names looked for under the project root,
/// in order.
///
/// Deliberately the same two names `crate::project::EXCLUDE_DIRS` skips when
/// walking - a directory whose contents are never indexed is exactly the
/// directory a venv's packages live in, and keeping the two lists in
/// agreement is what stops a venv being both invisible to the walk and
/// invisible to the type checker.
const VENV_DIRS: [&str; 2] = [".venv", "venv"];

/// Builds the semantic engine for `root` - the
/// [`SemanticEngineFactory`](g_mesh_plugin_sdk::SemanticEngineFactory) body,
/// called on the first `semanticPass` and never before.
pub fn engine(root: &Path) -> Result<Box<dyn SemanticEngine>> {
    let mut config = SemanticConfig::from_manifest()
        .context("reading [plugin.semantic] from this plugin's manifest")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this plugin's manifest has no [plugin.semantic] section, so there is no language \
                 server to run - see plugins/python/plugin.toml"
            )
        })?;
    prepare(&mut config, root)?;
    Ok(Box::new(LspBridge::new(LANGUAGE, root, config)))
}

/// Turns the manifest's `config` into the one the server for `root` runs
/// with: the resolved command and its args, then [`add_project_settings`].
fn prepare(config: &mut SemanticConfig, root: &Path) -> Result<()> {
    let resolved = resolve(&config.command, HOST_SCRIPT_EXTENSIONS)?;
    let mut args = resolved.prefix_args;
    args.extend(config.args.iter().cloned());
    g_mesh_plugin_sdk::log_line!(
        "[{LANGUAGE}] semantic tier: {} ({}, found on {})",
        resolved.command.display(),
        resolved.version,
        resolved.origin
    );
    config.command = resolved.command;
    config.args = args;
    add_project_settings(config, root);
    Ok(())
}

/// Adds the settings that depend on the project at `root` rather than on the
/// manifest: no interpreter, the venv's site-packages when it has a venv, and
/// the walk's exclude list.
fn add_project_settings(config: &mut SemanticConfig, root: &Path) {
    set_python_path(config, &no_interpreter());
    let packages = site_packages(root);
    if !packages.is_empty() {
        g_mesh_plugin_sdk::log_line!(
            "[{LANGUAGE}] semantic tier: python.analysis.extraPaths += {}",
            packages.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
        );
        add_extra_paths(config, &packages);
    }

    let exclude: Vec<String> = crate::project::EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
    let scope = walk_scope(root, &exclude);
    if scope.dropped > 0 {
        g_mesh_plugin_sdk::log_line!(
            "[{LANGUAGE}] semantic tier: {} ignored directories left out of python.analysis.exclude \
             (the deepest); pyright may scan them",
            scope.dropped
        );
    }
    set_scope(config, &scope);
}

/// The server to run, the args it needs, and the version its CLI twin
/// answered with.
///
/// `script_extensions` is [`HOST_SCRIPT_EXTENSIONS`] in production; it is a
/// parameter so tests can exercise the Windows spellings from any host.
fn resolve(command: &Path, script_extensions: &[&str]) -> Result<Resolved> {
    lsp::resolve(
        candidates(command, script_extensions),
        PROBE_BUDGET,
        "pyright",
        "Install it with `npm install -g pyright` so pyright-langserver is on PATH (neither the \
         project's node_modules nor npx is used: ADR 0032), or point [plugin.semantic] command in \
         plugins/python/plugin.toml at a pyright-langserver",
    )
}

/// The spellings worth probing, in order - [`npm_candidates_on_path`] for
/// pyright: `PATH` only, never the indexed project's own `node_modules/.bin`
/// and never `npx` (Decision 1).
fn candidates(command: &Path, script_extensions: &[&str]) -> Vec<Candidate> {
    npm_candidates_on_path(command, &PYRIGHT, script_extensions)
}

/// The CLI entry point beside a language-server one: `…/pyright-langserver`
/// becomes `…/pyright`, keeping any directory and any extension.
///
/// A name that is not a `-langserver` spelling is returned unchanged, which is
/// the honest answer for a path someone wrote by hand: probe the thing they
/// named, and let it fail if it cannot say what version it is.
fn cli_twin(command: &Path) -> PathBuf {
    let Some(stem) = command.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else {
        return command.to_path_buf();
    };
    let Some(base) = stem.strip_suffix("-langserver") else { return command.to_path_buf() };
    // For pyright the two entry points are exactly this rewrite - `SERVER_BIN`
    // stripped of `-langserver` *is* `CLI_BIN` - which the test below pins, so
    // that a rename of either constant fails here rather than probing a name
    // nothing installs.
    debug_assert_eq!(SERVER_BIN.strip_suffix("-langserver"), Some(CLI_BIN));
    let mut twin = base.to_string();
    if let Some(extension) = command.extension() {
        twin.push('.');
        twin.push_str(&extension.to_string_lossy());
    }
    match command.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => parent.join(twin),
        None => PathBuf::from(twin),
    }
}

/// The project venv's site-packages directories, found by reading
/// directories only: `<venv>/lib/python*/site-packages` and the Windows
/// `<venv>/Lib/site-packages`, for the first of [`VENV_DIRS`] that has any.
///
/// **Only a directory under the project root.** `$VIRTUAL_ENV` is not
/// consulted: the daemon's environment may name another project's venv, and
/// a wrong venv resolves imports to the wrong packages.
fn site_packages(root: &Path) -> Vec<PathBuf> {
    for dir in VENV_DIRS {
        let venv = root.join(dir);
        let mut found: Vec<PathBuf> = std::fs::read_dir(venv.join("lib"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
            .map(|entry| entry.path().join("site-packages"))
            .chain(std::iter::once(venv.join("Lib").join("site-packages")))
            .filter(|path| path.is_dir())
            .collect();
        if !found.is_empty() {
            found.sort();
            found.dedup();
            return found;
        }
    }
    Vec::new()
}

/// A `pythonPath` that names no file and never can: a child of this plugin's
/// own executable, which is a file. pyright fails to run it and runs no
/// other interpreter in its place.
fn no_interpreter() -> PathBuf {
    let parent = std::env::current_exe().unwrap_or_else(|_| {
        // Still a file on every platform, so the child path cannot exist.
        PathBuf::from(if cfg!(windows) { r"C:\Windows\System32\cmd.exe" } else { "/dev/null" })
    });
    parent.join("g-mesh-no-python-interpreter")
}

/// Adds `pythonPath` to the `python` section of `config.settings`, keeping
/// whatever the manifest already put there.
///
/// A merge rather than an insert: the manifest's own
/// `[plugin.semantic.settings.python.analysis]` and this path are two
/// different keys of one section, and a plain insert would drop whichever was
/// written second.
fn set_python_path(config: &mut SemanticConfig, python: &Path) {
    let section =
        as_object(config.settings.entry("python".to_string()).or_insert_with(|| serde_json::json!({})));
    section.insert("pythonPath".to_string(), serde_json::json!(python.to_string_lossy()));
}

/// Appends `paths` to `python.analysis.extraPaths`, after any the manifest
/// already lists.
fn add_extra_paths(config: &mut SemanticConfig, paths: &[PathBuf]) {
    let section =
        as_object(config.settings.entry("python".to_string()).or_insert_with(|| serde_json::json!({})));
    let analysis = as_object(section.entry("analysis").or_insert_with(|| serde_json::json!({})));
    let extra = analysis.entry("extraPaths").or_insert_with(|| serde_json::json!([]));
    if !extra.is_array() {
        *extra = serde_json::json!([]);
    }
    let extra = extra.as_array_mut().expect("just made an array");
    extra.extend(paths.iter().map(|path| serde_json::json!(path.to_string_lossy())));
}

/// Adds `analysis.exclude` to the `python` section of `config.settings`: the
/// pruned directories, then `**/<name>` for each named exclude.
///
/// Merged like [`set_python_path`], and an `exclude` the manifest already set
/// is left as it is: the manifest wins.
fn set_scope(config: &mut SemanticConfig, scope: &WalkScope) {
    let section =
        as_object(config.settings.entry("python".to_string()).or_insert_with(|| serde_json::json!({})));
    let analysis = as_object(section.entry("analysis").or_insert_with(|| serde_json::json!({})));
    if analysis.contains_key("exclude") {
        g_mesh_plugin_sdk::log_line!(
            "[{LANGUAGE}] semantic tier: the manifest sets python.analysis.exclude, so the walk's \
             exclude list is not sent"
        );
        return;
    }
    let exclude: Vec<String> = scope
        .pruned
        .iter()
        .map(|dir| dir.as_str().to_string())
        .chain(scope.exclude_dirs.iter().map(|name| format!("**/{name}")))
        .collect();
    analysis.insert("exclude".to_string(), serde_json::json!(exclude));
}

/// `value` as an object, replacing it with an empty one when it is not one.
fn as_object(value: &mut serde_json::Value) -> &mut serde_json::Map<String, serde_json::Value> {
    if !value.is_object() {
        *value = serde_json::json!({});
    }
    value.as_object_mut().expect("just made an object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::lsp::{ServerReadiness, WINDOWS_SCRIPT_EXTENSIONS};

    /// A bare name is `PATH` only: never a `node_modules/.bin`, never `npx`.
    #[test]
    fn a_bare_name_is_path_only_never_a_project_install_or_npx() {
        let candidates = candidates(Path::new(SERVER_BIN), &[]);
        assert_eq!(candidates.len(), 1, "{candidates:#?}");

        assert_eq!(candidates[0].command, PathBuf::from(SERVER_BIN), "the bare name stays a PATH lookup");
        assert!(candidates[0].prefix_args.is_empty());

        assert!(
            candidates.iter().all(|candidate| !candidate.command.to_string_lossy().contains("node_modules")),
            "no project-local install is probed or run: {candidates:#?}"
        );
        assert!(
            candidates.iter().all(|candidate| candidate.origin != "npx"
                && !candidate.command.to_string_lossy().starts_with("npx")),
            "npx runs the project's own node_modules/pyright - see Decision 1: {candidates:#?}"
        );
    }

    /// Every candidate is proved through the CLI twin, because the language
    /// server itself has no `--version` - see this module's doc, Decision 1.
    #[test]
    fn every_candidate_is_probed_through_the_cli_twin_and_never_the_server() {
        for candidate in candidates(Path::new(SERVER_BIN), &WINDOWS_SCRIPT_EXTENSIONS) {
            let (probe, args) = &candidate.probe;
            assert!(
                !probe.to_string_lossy().contains("langserver"),
                "the language server has no --version: {candidate:#?}"
            );
            assert!(
                probe
                    .file_name()
                    .is_some_and(|name| name == CLI_BIN || name == format!("{CLI_BIN}.cmd").as_str()),
                "and the twin is pyright's own CLI, extension included: {candidate:#?}"
            );
            assert!(args.is_empty(), "an installed twin takes no args before --version: {candidate:#?}");
        }
    }

    /// The twin keeps the directory and the extension, and leaves a name that
    /// is not a `-langserver` spelling alone.
    #[test]
    fn the_cli_twin_is_the_same_install_under_the_other_name() {
        assert_eq!(cli_twin(Path::new("pyright-langserver")), PathBuf::from("pyright"));
        assert_eq!(
            cli_twin(Path::new("/opt/py/node_modules/.bin/pyright-langserver")),
            PathBuf::from("/opt/py/node_modules/.bin/pyright")
        );
        assert_eq!(
            cli_twin(Path::new("C:/py/pyright-langserver.cmd")),
            PathBuf::from("C:/py/pyright.cmd"),
            "a Windows shim keeps its extension"
        );
        assert_eq!(
            cli_twin(Path::new("/opt/py/some-other-server")),
            PathBuf::from("/opt/py/some-other-server"),
            "a hand-written path is probed as written"
        );
    }

    /// The venv lookup reads the project's directories, and only the
    /// project's.
    #[test]
    fn site_packages_are_found_under_the_root_and_nowhere_else() {
        let guard = tempfile::Builder::new().prefix("g-mesh-py-venv-").tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
        std::fs::write(dir.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
        assert!(site_packages(&dir).is_empty(), "an interpreter alone is not a site-packages");

        std::fs::create_dir_all(dir.join(".venv/lib/python3.12/site-packages")).unwrap();
        assert_eq!(site_packages(&dir), vec![dir.join(".venv/lib/python3.12/site-packages")]);

        let empty = dir.join("no-venv-here");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(site_packages(&empty).is_empty());
    }

    /// The `pythonPath` handed to pyright is not a file, and is not a bare
    /// name pyright would look up on `PATH`.
    #[test]
    fn the_python_path_names_no_interpreter() {
        let python = no_interpreter();
        assert!(python.is_absolute(), "{python:?}");
        assert!(!python.exists(), "{python:?}");
    }

    /// `pythonPath` and `extraPaths` join the manifest's own settings rather
    /// than replacing the section they live in.
    #[test]
    fn the_venv_settings_are_merged_into_the_manifests_own_python_section() {
        let mut config = SemanticConfig::new(SERVER_BIN);
        config.settings.insert(
            "python".to_string(),
            serde_json::json!({ "analysis": { "typeCheckingMode": "basic", "extraPaths": ["mine"] } }),
        );
        set_python_path(&mut config, Path::new("/none"));
        add_extra_paths(&mut config, &[PathBuf::from("/p/.venv/lib/python3.12/site-packages")]);
        assert_eq!(
            config.settings["python"],
            serde_json::json!({
                "analysis": {
                    "typeCheckingMode": "basic",
                    "extraPaths": ["mine", "/p/.venv/lib/python3.12/site-packages"],
                },
                "pythonPath": "/none",
            })
        );
    }

    fn scope_of(pruned: &[&str], exclude_dirs: &[&str]) -> WalkScope {
        WalkScope {
            pruned: pruned.iter().map(|dir| g_mesh_plugin_sdk::RelPath::from(*dir)).collect(),
            exclude_dirs: exclude_dirs.iter().map(|dir| (*dir).to_string()).collect(),
            dropped: 0,
        }
    }

    /// The exclude list joins the manifest's own `python` section, and an
    /// `exclude` the manifest set itself is kept.
    #[test]
    fn the_scope_is_merged_into_the_python_section_and_the_manifest_wins() {
        let mut config = SemanticConfig::new(SERVER_BIN);
        config
            .settings
            .insert("python".to_string(), serde_json::json!({ "analysis": { "typeCheckingMode": "basic" } }));
        set_python_path(&mut config, Path::new("/p/.venv/bin/python"));
        set_scope(&mut config, &scope_of(&["build", "src/gen"], &[".git", ".venv"]));
        assert_eq!(
            config.settings["python"],
            serde_json::json!({
                "analysis": {
                    "typeCheckingMode": "basic",
                    "exclude": ["build", "src/gen", "**/.git", "**/.venv"],
                },
                "pythonPath": "/p/.venv/bin/python",
            })
        );

        let mut config = SemanticConfig::new(SERVER_BIN);
        config.settings.insert(
            "python".to_string(),
            serde_json::json!({ "analysis": { "typeCheckingMode": "basic", "exclude": ["mine"] } }),
        );
        set_scope(&mut config, &scope_of(&["build"], &[".git"]));
        assert_eq!(
            config.settings["python"],
            serde_json::json!({ "analysis": { "typeCheckingMode": "basic", "exclude": ["mine"] } })
        );
    }

    /// A scratch Python project: `app/main.py` imports from the gitignored
    /// `genpkg/`, and the gitignored `junk/` holds a module nothing imports.
    fn scoped_project(name: &str) -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::Builder::new().prefix(&format!("g-mesh-py-scope-{name}-")).tempdir().unwrap();
        let root = guard.path().to_path_buf();
        for (path, contents) in [
            (".gitignore", "junk/\ngenpkg/\n"),
            ("app/main.py", "from genpkg.gen import gen_fn\n\ngen_fn()\n"),
            ("genpkg/__init__.py", ""),
            ("genpkg/gen.py", "def gen_fn():\n    return 1\n"),
            ("junk/junkmod.py", "def gm_junk_only_fn():\n    return 2\n"),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        let root = root.canonicalize().unwrap();
        (guard, root)
    }

    fn shipped_config() -> SemanticConfig {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        SemanticConfig::from_manifest_at(manifest).unwrap().expect("the manifest has [plugin.semantic]")
    }

    /// The config the engine runs with carries the walk's exclude list, and
    /// no `include`.
    #[test]
    fn the_engine_config_carries_the_walks_exclude_list() {
        let (_guard, root) = scoped_project("config");
        let mut config = shipped_config();
        add_project_settings(&mut config, &root);
        let analysis = &config.settings["python"]["analysis"];
        let exclude: Vec<&str> = analysis["exclude"]
            .as_array()
            .expect("an exclude list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(exclude.contains(&"junk") && exclude.contains(&"genpkg"), "{exclude:?}");
        assert!(exclude.contains(&"**/.venv") && exclude.contains(&"**/.git"), "{exclude:?}");
        assert!(!exclude.iter().any(|entry| entry.starts_with("app")), "{exclude:?}");
        assert!(analysis.get("include").is_none(), "include is never sent: {analysis}");
    }

    /// A real pyright, handed the config [`prepare`] builds, does not track
    /// the pruned `junk/` and still resolves an import into the pruned
    /// `genpkg/`. Ignored by default because it needs `pyright-langserver`
    /// on `PATH`: `cargo test -p g-mesh-plugin-python -- --ignored`.
    #[test]
    #[ignore = "needs pyright-langserver on PATH"]
    fn real_pyright_skips_a_pruned_tree_and_still_resolves_imports_into_one() {
        let (_guard, root) = scoped_project("pyright");
        let mut config = shipped_config();
        prepare(&mut config, &root).expect("a pyright to run");
        let mut session = lsp::Session::start(&root, &config);

        let main = root.join("app/main.py");
        let uri = lsp::uri(&main);
        session.notify(
            "textDocument/didOpen",
            serde_json::json!({ "textDocument": {
                "uri": uri, "languageId": LANGUAGE, "version": 1,
                "text": std::fs::read_to_string(&main).unwrap(),
            }}),
        );
        let definition = session.request(
            "textDocument/definition",
            serde_json::json!({ "textDocument": { "uri": uri }, "position": { "line": 2, "character": 1 } }),
        );
        assert!(
            definition.to_string().contains("genpkg/gen.py"),
            "the import into the excluded genpkg/ resolves: {definition}"
        );

        let symbols = session.request("workspace/symbol", serde_json::json!({ "query": "gm_junk_only_fn" }));
        let found: Vec<&serde_json::Value> = symbols
            .as_array()
            .map(|symbols| symbols.iter().filter(|symbol| symbol["name"] == "gm_junk_only_fn").collect())
            .unwrap_or_default();
        assert!(found.is_empty(), "the pruned junk/ is not tracked: {symbols}");

        drop(session);
    }

    /// Just enough of an LSP client over stdio for the real-pyright test.
    mod lsp {
        use std::collections::BTreeMap;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::path::Path;
        use std::process::{Child, ChildStdin, Command, Stdio};
        use std::sync::mpsc::{channel, Receiver};
        use std::time::{Duration, Instant};

        use g_mesh_plugin_sdk::lsp::SemanticConfig;
        use serde_json::{json, Value};

        /// How long one request may take, pyright's first analysis included.
        const ANSWER_BUDGET: Duration = Duration::from_secs(180);

        pub fn uri(path: &Path) -> String {
            format!("file://{}", path.display())
        }

        pub struct Session {
            child: Child,
            stdin: Option<ChildStdin>,
            incoming: Receiver<Value>,
            settings: BTreeMap<String, Value>,
            next_id: i64,
        }

        impl Session {
            pub fn start(root: &Path, config: &SemanticConfig) -> Self {
                let mut child = Command::new(&config.command)
                    .args(&config.args)
                    .envs(&config.env)
                    .current_dir(root)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("pyright starts");
                let stdin = child.stdin.take();
                let stdout = child.stdout.take().unwrap();
                let (sender, incoming) = channel();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let line = line.trim_end();
                            if line.is_empty() {
                                break;
                            }
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                length = value.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        if reader.read_exact(&mut body).is_err() {
                            return;
                        }
                        let Ok(message) = serde_json::from_slice(&body) else { continue };
                        if sender.send(message).is_err() {
                            return;
                        }
                    }
                });
                let mut session =
                    Self { child, stdin, incoming, settings: config.settings.clone(), next_id: 0 };
                let root_uri = uri(root);
                session.request(
                    "initialize",
                    json!({
                        "processId": std::process::id(),
                        "rootUri": root_uri,
                        "workspaceFolders": [{ "uri": root_uri, "name": "scope" }],
                        "capabilities": { "workspace": { "configuration": true, "workspaceFolders": true } },
                    }),
                );
                session.notify("initialized", json!({}));
                session
            }

            fn send(&mut self, message: &Value) {
                let body = message.to_string();
                let stdin = self.stdin.as_mut().expect("the session is open");
                write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
                stdin.flush().unwrap();
            }

            pub fn notify(&mut self, method: &str, params: Value) {
                self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
            }

            /// The `result` of `method`, answering the server's own requests
            /// while it waits.
            pub fn request(&mut self, method: &str, params: Value) -> Value {
                self.next_id += 1;
                let id = self.next_id;
                self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
                let deadline = Instant::now() + ANSWER_BUDGET;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let message = self.incoming.recv_timeout(left).unwrap_or_else(|err| {
                        panic!("no answer to {method} within {ANSWER_BUDGET:?}: {err}")
                    });
                    if let Some(server_method) = message.get("method").and_then(Value::as_str) {
                        if let Some(server_id) = message.get("id") {
                            let result = self.reply(server_method, message.get("params"));
                            self.send(&json!({ "jsonrpc": "2.0", "id": server_id, "result": result }));
                        }
                    } else if message.get("id") == Some(&json!(id)) {
                        assert!(message.get("error").is_none(), "{method} failed: {message}");
                        return message.get("result").cloned().unwrap_or(Value::Null);
                    }
                }
            }

            fn reply(&self, method: &str, params: Option<&Value>) -> Value {
                if method != "workspace/configuration" {
                    return Value::Null;
                }
                let items = params.and_then(|params| params["items"].as_array().cloned()).unwrap_or_default();
                Value::Array(
                    items
                        .iter()
                        .map(|item| {
                            item["section"]
                                .as_str()
                                .and_then(|section| self.settings.get(section))
                                .cloned()
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                )
            }
        }

        impl Drop for Session {
            fn drop(&mut self) {
                if !std::thread::panicking() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.send(&json!({ "jsonrpc": "2.0", "id": 0, "method": "shutdown" }));
                        self.notify("exit", Value::Null);
                    }));
                }
                self.stdin = None;
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = self.child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// The shipped manifest is read by this module at run time and by nothing
    /// in this crate at build time, so without a test it is a file that can
    /// rot silently.
    #[test]
    fn the_shipped_manifest_configures_the_server_this_module_expects() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        let config = SemanticConfig::from_manifest_at(manifest)
            .expect("plugins/python/plugin.toml parses")
            .expect("and declares a [plugin.semantic] section");

        assert_eq!(config.command, PathBuf::from(SERVER_BIN), "a bare name, so PATH is tried first");
        assert_eq!(config.engine, "pyright", "the label on every edge this tier emits");
        assert_eq!(
            config.args,
            vec!["--stdio"],
            "pyright-langserver refuses to start without a transport - measured, see plugin.toml"
        );
        assert!(
            config.implementation_kinds.is_empty(),
            "pyright answers textDocument/implementation with -32601; asking would make every pass \
             incomplete - see this module's doc, Decision 4: {:?}",
            config.implementation_kinds
        );
        assert_eq!(
            config.initialization_options, None,
            "pyright ignores initializationOptions entirely - measured, see Decision 3"
        );
        assert_eq!(
            config.settings.get("python"),
            Some(&serde_json::json!({ "analysis": { "typeCheckingMode": "basic" } })),
            "the one channel pyright does read"
        );
        assert_eq!(
            config.readiness,
            ServerReadiness::OnDemand,
            "pyright answers a cross-file definition 41-146ms before its own $/progress token \
             begins - traced three times, see plugin.toml and the design doc's GM-310 notes. \
             This is the key that takes the whole-project pass from 3.339s to 2.285s"
        );
    }

    /// `plugin.toml`'s `plugin_version` is what core announces for this plugin
    /// everywhere - `g-mesh plugins list`, and the handshake. Two sources for
    /// one number is how `plugins/rust`'s drifted three releases unnoticed;
    /// this is the check that stops it happening here, and it needs no built
    /// binary to run.
    #[test]
    fn the_manifest_version_matches_the_crates() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        let text = std::fs::read_to_string(manifest).expect("plugins/python/plugin.toml is readable");
        let parsed: toml::Value = toml::from_str(&text).expect("plugins/python/plugin.toml parses");
        assert_eq!(
            parsed["plugin"]["plugin_version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "plugin.toml's plugin_version must track Cargo.toml's version"
        );
    }

    /// The manifest's capability flip is half the deliverable: core only sends
    /// `semanticPass` when `semantic_pass` is true, and the MCP instructions
    /// only stop listing Python's receiver gap when the two `receiver_calls`
    /// fields differ *and* a pass has landed.
    #[test]
    fn the_manifest_declares_the_capabilities_this_tier_needs() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        let text = std::fs::read_to_string(manifest).expect("plugins/python/plugin.toml is readable");
        let parsed: toml::Value = toml::from_str(&text).expect("plugins/python/plugin.toml parses");
        let capabilities = &parsed["plugin"]["capabilities"];
        assert_eq!(capabilities["semantic_pass"].as_bool(), Some(true));
        assert_eq!(capabilities["receiver_calls"].as_str(), Some("resolved"));
        assert_eq!(capabilities["receiver_calls_structural"].as_str(), Some("unresolved"));
    }

    /// And the whole resolution says what to do about it rather than only
    /// that it failed.
    #[test]
    fn nothing_usable_names_the_remedy() {
        let command = Path::new("/nonexistent/pyright-langserver");
        let err = resolve(command, HOST_SCRIPT_EXTENSIONS).expect_err("must not resolve");
        let message = format!("{err:#}");
        assert!(message.contains("npm install -g pyright"), "{message}");
        // The candidate is rendered by `Path::display()` from what `cli_twin`
        // built with `Path::join`, so on Windows it reads `/nonexistent\pyright`
        // and this assertion used to spell `/nonexistent/pyright` by hand and
        // fail there (GM-336). Asking `cli_twin` for the expectation keeps the
        // two on the same platform, rather than normalising separators in a
        // message a human reads in a Windows terminal - where the backslash is
        // the right spelling.
        //
        // Pinned together with the origin, not alone: `…/pyright-langserver`
        // *contains* `…/pyright`, so an assertion on the twin's path by itself
        // would pass just as happily if the twin were never applied and the
        // server spelling were probed instead.
        let expected = format!("{} (the path the manifest names)", cli_twin(command).display());
        assert!(message.contains(&expected), "expected to find `{expected}` in: {message}");
    }
}
