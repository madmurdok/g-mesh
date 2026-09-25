//! The Python plugin's semantic tier: finding pyright, telling it about the
//! project's interpreter, and handing it to the SDK's generic [`LspBridge`].
//!
//! Everything about *driving* a language server is the SDK's
//! (`plugins/sdk/src/lsp/`, whose module doc carries the decision table).
//! What is left for a language's own plugin is which binary to run and what
//! to configure it with - and for pyright both of those turned out to differ
//! from the rust-analyzer precedent `plugins/rust/src/semantic.rs` set, in
//! ways that are measured here rather than assumed.
//!
//! # Decision 1: three places to look, each probed rather than believed
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
//! same resolution, one directory or one npx invocation, with
//! `pyright-langserver` replaced by `pyright`. What that proves is what the
//! probe is for - that a real, runnable pyright package is behind this
//! spelling - and the version it prints (`pyright 1.1.414`) goes in the log
//! line, because "which pyright answered this pass" is the first thing anyone
//! asks of a semantic index that looks wrong.
//!
//! The three places, in order, and what each one is for:
//!
//! 1. **`PATH`.** The manifest's bare `pyright-langserver`, looked up by the
//!    operating system - `Command::new` does that on every platform. This is
//!    a global `npm i -g pyright`, a `pipx install pyright`, or a distro
//!    package. On Windows this candidate needs more than a plain lookup - see
//!    Decision 1b, right after this list.
//! 2. **The indexed project's own `node_modules/.bin`.** Resolved against the
//!    *project root*, not against the manifest, and this is the reason the
//!    factory takes a root at all: a project that pins its own pyright is
//!    stating which version its code type-checks against, and running a
//!    different global one would answer a question nobody asked. It is also
//!    the least invasive way to have a pyright at all - no global install.
//! 3. **`npx`.** Last, deliberately: it is the only candidate that can touch
//!    the network, and it is the one that most needs the probe. `npx` is to
//!    pyright what `~/.cargo/bin/rust-analyzer` was to rust-analyzer in
//!    GM-290 - a binary that always exists and always starts, whether or not
//!    a server comes out the other end - so without a probe a machine with no
//!    network would get a server that starts and dies, which is not
//!    `ErrorKind::NotFound`, so the bridge's permanent degradation never fires
//!    and the plugin re-spawns it once per pass until `MAX_SERVER_STARTS`
//!    stops it four starts later.
//!
//!    The spelling is `npx --yes --package pyright pyright-langserver
//!    --stdio`, and the `--package` is not decoration. The obvious
//!    `npx --yes pyright-langserver` reads its argument as a *package* name,
//!    and there is no npm package by that name - `pyright-langserver` is one
//!    of the `pyright` package's two `"bin"` entries. Measured, in the first
//!    conformance run this tier ever survived to:
//!
//!    ```text
//!    npm ERR! 404 Not Found - GET https://registry.npmjs.org/pyright-langserver
//!    npm ERR! 404  'pyright-langserver@*' is not in this registry.
//!    ```
//!
//!    What makes that worth writing down is *how it got past the probe*: the
//!    probe was `npx --yes pyright --version`, which succeeds, because
//!    `pyright` is a real package whose default bin is `pyright`. A probe that
//!    proves the package rather than the **argv shape** proves the wrong
//!    thing. So the npx probe now carries the same `--package` and differs
//!    from the server invocation in exactly one token - the bin name - which
//!    is the smallest difference that can still be a `--version`.
//!
//! A `command` that is a *path* is not searched around, exactly as GM-290
//! decided: someone who wrote a path into the manifest meant that file. It is
//! still probed, through the CLI twin beside it when its name ends in
//! `-langserver`, so a path that is wrong fails with one log line rather than
//! as a server that dies at handshake.
//!
//! # Decision 1b: `Command::new` does not walk `PATHEXT`, so this module does
//!
//! GM-341: none of the three candidates above start on Windows when pyright
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
//! Concretely: candidate 1 looks for `pyright-langserver.exe`; candidate 2
//! names `node_modules/.bin/pyright-langserver`, a path with a separator, so
//! *no* extension is tried at all; and even candidate 3 fails the same way,
//! because `npx` itself ships as `npx.cmd` beside `node.exe` on a machine
//! whose Node came from the official Windows installer. Not measured on a
//! Windows host - there is none here - but is documented `CreateProcess`
//! behaviour plus the CI evidence this task was cut from: CI's own step had
//! already run `npm install pyright` successfully, and the conformance test -
//! which carried the same naive candidate list this module did - still
//! reported no usable pyright.
//!
//! The fix is [`script_spellings`]: every final candidate path from all three
//! branches above, and a path the manifest names explicitly, is tried both as
//! written and, when it has no extension of its own, with `.cmd` appended.
//! Only `.cmd` - not `.bat`, which current npm does not write (listing a
//! spelling nothing installs would only make a failure message name a
//! candidate that can never exist), and not `.ps1`, which `CreateProcessW`
//! cannot execute directly either: running one needs an explicit
//! `powershell -File` wrapper this module does not add, so trying the bare
//! `.ps1` path would add a candidate that can never succeed rather than a
//! real fallback. `npx` is not special-cased - it goes through the same
//! expansion as the manifest path and the two npm-install candidates, which
//! is what makes this a fix rather than a list of two spellings with a third
//! exception waiting to be discovered the same way this one was. The
//! extension list is a parameter, not a fact [`script_spellings`] reads for
//! itself - [`candidates`] is the one place that picks
//! [`WINDOWS_SCRIPT_EXTENSIONS`] under `#[cfg(windows)]`, the same shape
//! `daemon::manifest::resolve_exe_suffix` uses around
//! `daemon::manifest::exe_suffixed` and its `EXE_SUFFIX` - so the Windows arm
//! is exercised by this module's own tests from this macOS host, rather than
//! sitting behind a `#[cfg(windows)]` nobody here can run. GM-335 is what a
//! test that never runs costs.
//!
//! # Decision 2: the probe is bounded, because this one can reach the network
//!
//! GM-290's probe is an unbounded `Command::output()`, which is right for
//! rust-analyzer: `--version` is about twenty milliseconds of local work.
//! Measured here, the three candidates are not one cost but two -
//! `node_modules/.bin/pyright --version` is 0.89s (node start-up), and
//! `npx --yes pyright --version` is 4.94s when it has to populate npm's `_npx`
//! cache and longer when it has to download. A registry that is unreachable
//! rather than absent does not fail fast, and the factory runs *inside* the
//! pass core is timing. So [`PROBE_BUDGET`] bounds it and the child is killed
//! rather than waited on, which is a correction to the precedent and not a
//! Python detail: it is the first candidate any language has had that is not
//! purely local.
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
//! - `python.pythonPath`, from [`interpreter`], when the project has a venv.
//!   This one cannot come from the manifest: it is a path inside the project
//!   being indexed, and the manifest ships beside the plugin binary.
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
//! receiver-call gap listed in the generated MCP instructions
//! (`core::mcp::instructions::has_open_receiver_gap`). Installing pyright and
//! restarting the daemon then gets the pass; a completed-but-empty pass would
//! have recorded "done" and never asked again.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::{walk_scope, SemanticEngine, WalkScope};

/// The language id this plugin speaks, for `didOpen` and for log lines.
const LANGUAGE: &str = "python";

/// The npm package's language-server entry point, and the CLI entry point
/// beside it that a probe can actually ask for a version - see this module's
/// doc, Decision 1. Both are `"bin"` entries of the same package, so finding
/// one is finding the other.
const SERVER_BIN: &str = "pyright-langserver";
const CLI_BIN: &str = "pyright";

/// The npm *package* both of those bins belong to.
///
/// Spelled separately from [`CLI_BIN`] although the two strings are equal
/// today, because they are different kinds of name and only one of them is
/// what `npx --package` wants: `npx --yes pyright-langserver` asks the
/// registry for a package called `pyright-langserver` and is answered 404.
const NPM_PACKAGE: &str = "pyright";

/// Where a project keeps a locally installed pyright.
const NODE_BIN_DIR: &str = "node_modules/.bin";

/// The npm shim extension worth trying beyond a candidate's bare spelling -
/// see this module's doc, Decision 1b, for why `.cmd` and not `.bat`/`.ps1`.
///
/// Only read by [`HOST_SCRIPT_EXTENSIONS`]'s `#[cfg(windows)]` arm and by this
/// module's own tests (which exercise it from every host, windows included) -
/// so a build for any other host sees it as unread, which is what the `allow`
/// says and nothing more.
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOWS_SCRIPT_EXTENSIONS: [&str; 1] = [".cmd"];

/// The script extensions worth trying on the host this process actually runs
/// on - empty everywhere but Windows, where npm writes a bin as a `.cmd`
/// shim rather than a `.exe`.
///
/// The only `#[cfg(windows)]` in this module, and it does nothing but pick
/// which list [`candidates`] hands to [`script_spellings`] - a pure function
/// that takes the list as a parameter rather than reading this constant
/// itself, so its Windows arm is exercised by this module's own tests from
/// any host. Same shape as `daemon::manifest::resolve_exe_suffix` picking
/// `std::env::consts::EXE_SUFFIX` for `daemon::manifest::exe_suffixed`.
#[cfg(windows)]
const HOST_SCRIPT_EXTENSIONS: &[&str] = &WINDOWS_SCRIPT_EXTENSIONS;
#[cfg(not(windows))]
const HOST_SCRIPT_EXTENSIONS: &[&str] = &[];

/// How long one candidate's `--version` may take before it is killed and
/// counted as a failure - see this module's doc, Decision 2. Generous against
/// the 0.89s a local probe measured and the 4.94s an `npx` one did, and still
/// far inside the pass budget the factory runs within
/// (`Budgets::project_floor` is fifteen minutes).
const PROBE_BUDGET: Duration = Duration::from_secs(60);

/// The virtual-environment directory names looked for under the project root,
/// in order.
///
/// Deliberately the same two names `crate::project::EXCLUDE_DIRS` skips when
/// walking - a directory whose contents are never indexed is exactly the
/// directory an interpreter lives in, and keeping the two lists in agreement
/// is what stops a venv being both invisible to the walk and invisible to the
/// type checker.
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
    let resolved = resolve(&config.command, root, HOST_SCRIPT_EXTENSIONS)?;
    let mut args = resolved.prefix_args;
    args.extend(config.args.iter().cloned());
    eprintln!(
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
/// manifest: its interpreter, when it has one, and the walk's exclude list.
fn add_project_settings(config: &mut SemanticConfig, root: &Path) {
    // A project with no venv is the ordinary case and not a warning: pyright
    // then finds an interpreter itself and says which one it assumed in its
    // own log, which the bridge forwards.
    if let Some(python) = interpreter(root) {
        eprintln!("[{LANGUAGE}] semantic tier: python.pythonPath = {}", python.display());
        set_python_path(config, &python);
    }

    let exclude: Vec<String> = crate::project::EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
    let scope = walk_scope(root, &exclude);
    if scope.dropped > 0 {
        eprintln!(
            "[{LANGUAGE}] semantic tier: {} ignored directories left out of python.analysis.exclude \
             (the deepest); pyright may scan them",
            scope.dropped
        );
    }
    set_scope(config, &scope);
}

/// One spelling of "run pyright's language server", and how to prove it is
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    /// argv\[0\] for the server.
    command: PathBuf,
    /// argv entries that come *before* the manifest's own `args` - the
    /// package name, for the `npx` candidate, and nothing for the others.
    prefix_args: Vec<String>,
    /// The CLI twin to ask `--version`, and the args that precede it.
    probe: (PathBuf, Vec<String>),
    /// What to call this branch in the log line and in a failure message.
    origin: &'static str,
}

/// A candidate that answered, and what it said.
#[derive(Debug)]
struct Resolved {
    command: PathBuf,
    prefix_args: Vec<String>,
    version: String,
    origin: &'static str,
}

/// The server to run, the args it needs, and the version its CLI twin
/// answered with.
///
/// `script_extensions` is [`HOST_SCRIPT_EXTENSIONS`] in production - see this
/// module's doc, Decision 1b, for why it is a parameter here rather than a
/// constant this function reads for itself.
fn resolve(command: &Path, root: &Path, script_extensions: &[&str]) -> Result<Resolved> {
    let mut failures: Vec<String> = Vec::new();
    for candidate in candidates(command, root, script_extensions) {
        let (probe_command, probe_args) = &candidate.probe;
        match probe(probe_command, probe_args, PROBE_BUDGET) {
            Ok(version) => {
                return Ok(Resolved {
                    command: candidate.command,
                    prefix_args: candidate.prefix_args,
                    version,
                    origin: candidate.origin,
                })
            }
            Err(err) => failures.push(format!("{} ({}): {err:#}", probe_command.display(), candidate.origin)),
        }
    }
    bail!(
        "no usable pyright: {}. Install it with `npm install pyright` in the project (its \
         node_modules/.bin is looked in), or `npm install -g pyright`, or point [plugin.semantic] \
         command in plugins/python/plugin.toml at a pyright-langserver",
        failures.join("; ")
    )
}

/// The spellings worth probing, in order - see this module's doc, Decisions 1
/// and 1b.
///
/// A `command` that is a path names one file and is never searched around; a
/// bare one is a `PATH` lookup, then the indexed project's own
/// `node_modules/.bin`, then `npx`. Within each of those, every extension
/// [`script_spellings`] builds for `script_extensions` is tried, in order,
/// before moving to the next origin - the origin priority (a global install
/// over a project-local one over the network) matters more than which
/// spelling of one origin answers first.
fn candidates(command: &Path, root: &Path, script_extensions: &[&str]) -> Vec<Candidate> {
    let bare = command.components().count() == 1 && !command.is_absolute();
    if !bare {
        return script_spellings(command, script_extensions)
            .into_iter()
            .map(|command| Candidate {
                probe: (cli_twin(&command), Vec::new()),
                command,
                prefix_args: Vec::new(),
                origin: "the path the manifest names",
            })
            .collect();
    }

    let name = command.to_string_lossy().into_owned();
    let local = root.join(NODE_BIN_DIR).join(&name);
    let mut candidates = Vec::new();
    for command in script_spellings(command, script_extensions) {
        candidates.push(Candidate {
            probe: (cli_twin(&command), Vec::new()),
            command,
            prefix_args: Vec::new(),
            origin: "PATH",
        });
    }
    for command in script_spellings(&local, script_extensions) {
        candidates.push(Candidate {
            probe: (cli_twin(&command), Vec::new()),
            command,
            prefix_args: Vec::new(),
            origin: "the project's node_modules/.bin",
        });
    }
    for command in script_spellings(Path::new("npx"), script_extensions) {
        candidates.push(Candidate {
            prefix_args: npx_args(&name),
            probe: (command.clone(), npx_args(CLI_BIN)),
            command,
            origin: "npx",
        });
    }
    candidates
}

/// `path`, and - when it has no extension of its own - `path` with each of
/// `extensions` appended to its file name, in the order given.
///
/// Mirrors `daemon::manifest::exe_suffixed`'s rule for `.exe` (never touch an
/// explicit spelling) generalised to a list, because a Windows npm install
/// needs more than one alternate spelling tried - see this module's doc,
/// Decision 1b. Pure and filesystem-independent, like its precedent: nothing
/// here checks whether a spelling exists, which is what makes it exercisable
/// from any host by a plain unit test rather than only against a real
/// install.
fn script_spellings(path: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut spellings = vec![path.to_path_buf()];
    if path.extension().is_some() {
        return spellings;
    }
    let Some(stem) = path.file_name() else { return spellings };
    for extension in extensions {
        let mut name = stem.to_os_string();
        name.push(extension);
        spellings.push(path.with_file_name(name));
    }
    spellings
}

/// `npx`'s own argv for running `bin` out of the pyright package.
///
/// One function for both the server and its probe, so that the two cannot
/// drift into proving different things - see this module's doc, Decision 1,
/// for the 404 that made the difference visible.
fn npx_args(bin: &str) -> Vec<String> {
    vec!["--yes".to_string(), "--package".to_string(), NPM_PACKAGE.to_string(), bin.to_string()]
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

/// Runs `<command> <args…> --version` and returns what it printed, killing it
/// if it has not finished within `budget`.
///
/// The "prove it" half of this module - see the module doc for the `npx` shim
/// it exists to reject, and Decision 2 for why the budget is here and not in
/// `plugins/rust`'s equivalent.
fn probe(command: &Path, args: &[String], budget: Duration) -> Result<String> {
    let mut child = Command::new(command)
        .args(args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", command.display()))?;

    // Both pipes are drained on their own threads: a child that fills one
    // while this side waits on the other is a deadlock that looks exactly
    // like a slow probe.
    let out = reader(child.stdout.take());
    let err = reader(child.stderr.take());

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait().context("could not wait for the probe")? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`--version` did not answer within {budget:?}");
            }
        }
    };

    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        let said = stderr.lines().next().unwrap_or("").trim();
        bail!("`--version` exited {status} ({said})");
    }
    let version = stdout.trim().to_string();
    Ok(if version.is_empty() { "no version reported".to_string() } else { version })
}

/// Reads a child pipe to the end on its own thread.
fn reader<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

/// The project's own interpreter, if it has one: `<root>/.venv/bin/python`, or
/// the `venv` spelling, or the Windows layout of either.
///
/// **Only a directory under the project root.** `$VIRTUAL_ENV` is deliberately
/// not consulted, and that is a decision rather than an omission: the
/// environment this process inherits is the daemon's, which is whatever shell
/// started `g-mesh`, which on a machine with several checked-out projects is
/// as likely to name *another* project's environment as this one's. A wrong
/// interpreter is worse than none - pyright resolves third-party imports
/// against it, so every `import` that a project's real environment provides
/// would resolve to the wrong package or to nothing.
fn interpreter(root: &Path) -> Option<PathBuf> {
    for dir in VENV_DIRS {
        for relative in ["bin/python", "bin/python3", "Scripts/python.exe"] {
            let candidate = root.join(dir).join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
        eprintln!(
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
    use g_mesh_plugin_sdk::lsp::ServerReadiness;

    /// A path names one binary and is never searched around - see
    /// [`candidates`]' doc and GM-290's own rule. No extensions on this host
    /// (`&[]`, matching `HOST_SCRIPT_EXTENSIONS` on every non-Windows host) is
    /// the ordinary case; the Windows arm gets its own test below.
    #[test]
    fn a_command_that_is_a_path_is_the_only_candidate() {
        let root = Path::new("/projects/thing");
        assert_eq!(candidates(Path::new("/opt/py/pyright-langserver"), root, &[]).len(), 1);
        assert_eq!(candidates(Path::new("servers/pyright-langserver"), root, &[]).len(), 1);
    }

    /// A bare name is three places in one fixed order, and the middle one is
    /// resolved against the *project*, not against the manifest.
    #[test]
    fn a_bare_name_is_path_then_the_projects_node_modules_then_npx() {
        let root = Path::new("/projects/thing");
        let candidates = candidates(Path::new(SERVER_BIN), root, &[]);
        assert_eq!(candidates.len(), 3, "{candidates:#?}");

        assert_eq!(candidates[0].command, PathBuf::from(SERVER_BIN), "the bare name stays a PATH lookup");
        assert!(candidates[0].prefix_args.is_empty());

        assert_eq!(
            candidates[1].command,
            root.join("node_modules/.bin").join(SERVER_BIN),
            "the project's own install, under the root being indexed"
        );

        assert_eq!(candidates[2].command, PathBuf::from("npx"));
        assert_eq!(
            candidates[2].prefix_args,
            vec!["--yes", "--package", NPM_PACKAGE, SERVER_BIN],
            "the bin is not a package: `npx --yes pyright-langserver` is a 404 - see Decision 1"
        );
    }

    /// GM-341: on Windows every one of the three origins is tried both bare
    /// and with `.cmd` appended, in that order, before the next origin - see
    /// this module's doc, Decision 1b. Built from `WINDOWS_SCRIPT_EXTENSIONS`
    /// rather than a literal `".cmd"`, so a second extension ever added there
    /// is covered here without editing this test. Run from this macOS host,
    /// which is the point: [`script_spellings`] takes the extension list as a
    /// parameter instead of reading [`HOST_SCRIPT_EXTENSIONS`] itself, so its
    /// Windows arm needs no Windows host to execute.
    #[test]
    fn on_windows_every_origin_is_tried_bare_then_with_each_script_extension() {
        let root = Path::new("/projects/thing");
        let candidates = candidates(Path::new(SERVER_BIN), root, &WINDOWS_SCRIPT_EXTENSIONS);
        let spellings_per_origin = 1 + WINDOWS_SCRIPT_EXTENSIONS.len();
        assert_eq!(candidates.len(), 3 * spellings_per_origin, "{candidates:#?}");

        let path = |origin: &str, path: &Path| {
            candidates.iter().find(|c| c.origin == origin && c.command == path).unwrap_or_else(|| {
                panic!("expected a {origin} candidate spelled {}: {candidates:#?}", path.display())
            })
        };
        path("PATH", Path::new(SERVER_BIN));
        path("PATH", Path::new("pyright-langserver.cmd"));
        path(
            "the project's node_modules/.bin",
            &root.join("node_modules/.bin").join("pyright-langserver.cmd"),
        );
        let npx_cmd = path("npx", Path::new("npx.cmd"));
        assert_eq!(
            npx_cmd.prefix_args,
            vec!["--yes", "--package", NPM_PACKAGE, SERVER_BIN],
            "the .cmd spelling of npx runs the same argv as the bare one: {npx_cmd:#?}"
        );

        // Bare-before-.cmd within one origin, and PATH-before-local-before-npx
        // across origins - see this module's doc, Decision 1b, on why origin
        // order outranks extension order.
        let origins: Vec<&str> = candidates.iter().map(|c| c.origin).collect();
        assert_eq!(
            origins,
            vec![
                "PATH",
                "PATH",
                "the project's node_modules/.bin",
                "the project's node_modules/.bin",
                "npx",
                "npx"
            ],
            "{candidates:#?}"
        );
    }

    /// The npx probe and the npx server invocation must differ in exactly one
    /// token, the bin name. Anything else and the probe is proving a
    /// different command than the one that will run - which is precisely how
    /// the 404 above got past a green probe.
    #[test]
    fn the_npx_probe_differs_from_the_npx_server_only_in_the_bin_name() {
        let npx = candidates(Path::new(SERVER_BIN), Path::new("/projects/thing"), &[]).remove(2);
        let (_, probe_args) = &npx.probe;
        assert_eq!(npx.prefix_args.len(), probe_args.len(), "{npx:#?}");
        let differing: Vec<_> =
            npx.prefix_args.iter().zip(probe_args).filter(|(server, probe)| server != probe).collect();
        assert_eq!(differing, vec![(&SERVER_BIN.to_string(), &CLI_BIN.to_string())], "{npx:#?}");
    }

    /// Every candidate is proved through the CLI twin, because the language
    /// server itself has no `--version` - see this module's doc, Decision 1.
    #[test]
    fn every_candidate_is_probed_through_the_cli_twin_and_never_the_server() {
        let root = Path::new("/projects/thing");
        for candidate in candidates(Path::new(SERVER_BIN), root, &WINDOWS_SCRIPT_EXTENSIONS) {
            let (probe, args) = &candidate.probe;
            assert!(
                !probe.to_string_lossy().contains("langserver"),
                "the language server has no --version: {candidate:#?}"
            );
            assert!(
                probe
                    .file_name()
                    .is_some_and(|name| name == CLI_BIN || name == format!("{CLI_BIN}.cmd").as_str())
                    || args.iter().any(|arg| arg == CLI_BIN),
                "and the twin is pyright's own CLI, extension included: {candidate:#?}"
            );
        }
    }

    /// [`script_spellings`] is the pure building block [`candidates`] uses for
    /// every origin - see this module's doc, Decision 1b.
    #[test]
    fn script_spellings_keeps_the_bare_path_and_appends_every_extension() {
        assert_eq!(
            script_spellings(Path::new("pyright-langserver"), &[]),
            vec![PathBuf::from("pyright-langserver")],
            "no extensions on this host is a no-op"
        );
        assert_eq!(
            script_spellings(Path::new("pyright-langserver"), &[".cmd"]),
            vec![PathBuf::from("pyright-langserver"), PathBuf::from("pyright-langserver.cmd")]
        );
        assert_eq!(
            script_spellings(Path::new("/p/node_modules/.bin/pyright-langserver"), &[".cmd", ".ps1"]),
            vec![
                PathBuf::from("/p/node_modules/.bin/pyright-langserver"),
                PathBuf::from("/p/node_modules/.bin/pyright-langserver.cmd"),
                PathBuf::from("/p/node_modules/.bin/pyright-langserver.ps1"),
            ],
            "a directory is kept, and both extensions are offered in the order given"
        );
    }

    /// A spelling that already carries an extension is left alone - the same
    /// rule `daemon::manifest::exe_suffixed` applies to `.exe`: someone who
    /// wrote `pyright-langserver.cmd` into the manifest by hand meant exactly
    /// that file, not `pyright-langserver.cmd.cmd`.
    #[test]
    fn script_spellings_does_not_touch_an_explicit_extension() {
        assert_eq!(
            script_spellings(Path::new("pyright-langserver.cmd"), &[".cmd"]),
            vec![PathBuf::from("pyright-langserver.cmd")]
        );
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

    /// The probe is what separates a real install from a shim, so it has to
    /// fail for a binary that runs and exits non-zero - not only for one that
    /// is absent.
    #[test]
    fn a_binary_that_exits_non_zero_is_not_a_pyright() {
        assert!(probe(Path::new("/nonexistent/pyright"), &[], PROBE_BUDGET).is_err(), "not there at all");
        #[cfg(unix)]
        assert!(probe(Path::new("/usr/bin/false"), &[], PROBE_BUDGET).is_err(), "runs and refuses");
    }

    /// And it is bounded, which `plugins/rust`'s equivalent is not - see
    /// Decision 2.
    ///
    /// `sh -c "sleep 30"` rather than `sleep 30`, because [`probe`] appends
    /// `--version` to whatever it is given and `sleep` rejects it as a time
    /// interval - under `sh -c` a trailing word is `$0` and is ignored, which
    /// is the smallest unix program that both outlives a short budget and
    /// tolerates the argument every probe adds.
    #[cfg(unix)]
    #[test]
    fn a_probe_that_never_answers_is_killed_rather_than_waited_on() {
        let started = Instant::now();
        let args = ["-c".to_string(), "sleep 30".to_string()];
        let err = probe(Path::new("/bin/sh"), &args, Duration::from_millis(200))
            .expect_err("a probe that outlives its budget must fail");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must not have waited: {:?}",
            started.elapsed()
        );
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
    }

    /// The project-local branch against a **real** pyright, not a stand-in.
    ///
    /// The conformance kit cannot exercise this branch: it runs the plugin
    /// against a scratch *copy* of the fixture and `copy_tree` skips symlinks
    /// deliberately, so an npm `.bin` directory - which is nothing but
    /// symlinks on unix - does not survive the copy. That is a fact about the
    /// kit and not about this plugin, so the branch is proved here instead,
    /// with a project tree built for the purpose and the real install behind
    /// it: the probe really runs, and the version it reports is pyright's own.
    ///
    /// The one thing this cannot assert unconditionally is that branch 2
    /// *won*: on a machine with a global pyright, branch 1 legitimately wins.
    /// So the invariant asserted in both worlds is the one that matters -
    /// a project-local install is found without reaching for the network -
    /// and the exact answer is asserted when `PATH` has nothing.
    #[cfg(unix)]
    #[test]
    fn a_project_local_install_is_found_and_probed_for_real() {
        let installed = installed_bin_dir();
        let root = std::env::temp_dir().join(format!("g-mesh-py-local-{}", std::process::id()));
        let bin = root.join(NODE_BIN_DIR);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        for name in [SERVER_BIN, CLI_BIN] {
            std::os::unix::fs::symlink(installed.join(name), bin.join(name)).unwrap();
        }

        let version = probe(&bin.join(CLI_BIN), &[], PROBE_BUDGET)
            .expect("the project-local CLI twin answers --version");
        assert!(version.starts_with("pyright "), "the probe reports pyright's own version: {version}");

        let resolved =
            resolve(Path::new(SERVER_BIN), &root, HOST_SCRIPT_EXTENSIONS).expect("a local install resolves");
        assert_ne!(resolved.origin, "npx", "a local install must be found without the network");
        if probe(Path::new(CLI_BIN), &[], PROBE_BUDGET).is_err() {
            assert_eq!(resolved.origin, "the project's node_modules/.bin");
            assert_eq!(resolved.command, bin.join(SERVER_BIN), "and it is the server, not the CLI");
            assert!(resolved.prefix_args.is_empty(), "no npx wrapping: {resolved:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Where `npm install pyright`, run in `plugins/python`, puts its bins.
    ///
    /// Deliberately not a `None` that turns into a skip - the same rule
    /// `plugins/rust/tests/conformance.rs` states for rust-analyzer: a check
    /// that passes because it did not run is the failure this whole thing
    /// exists to remove.
    fn installed_bin_dir() -> PathBuf {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
        match probe(&dir.join(CLI_BIN), &[], PROBE_BUDGET) {
            Ok(_) => dir.to_path_buf(),
            Err(err) => panic!(
                "these tests drive a real pyright and there is none in {}: {err:#}. Install it with \
                 `npm install pyright` run in plugins/python (it is gitignored there).",
                dir.display()
            ),
        }
    }

    /// The venv lookup reads the project, and only the project.
    #[test]
    fn an_interpreter_is_found_under_the_root_and_nowhere_else() {
        let dir = std::env::temp_dir().join(format!("g-mesh-py-venv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
        assert_eq!(interpreter(&dir), None, "an empty .venv is not an interpreter");

        std::fs::write(dir.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
        assert_eq!(interpreter(&dir), Some(dir.join(".venv/bin/python")));

        let empty = dir.join("no-venv-here");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(interpreter(&empty), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pythonPath` joins the manifest's own settings rather than replacing
    /// the section they live in.
    #[test]
    fn the_interpreter_is_merged_into_the_manifests_own_python_section() {
        let mut config = SemanticConfig::new(SERVER_BIN);
        config
            .settings
            .insert("python".to_string(), serde_json::json!({ "analysis": { "typeCheckingMode": "basic" } }));
        set_python_path(&mut config, Path::new("/p/.venv/bin/python"));
        assert_eq!(
            config.settings["python"],
            serde_json::json!({
                "analysis": { "typeCheckingMode": "basic" },
                "pythonPath": "/p/.venv/bin/python",
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
    fn scoped_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("g-mesh-py-scope-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
        root.canonicalize().unwrap()
    }

    fn shipped_config() -> SemanticConfig {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        SemanticConfig::from_manifest_at(manifest).unwrap().expect("the manifest has [plugin.semantic]")
    }

    /// The config the engine runs with carries the walk's exclude list, and
    /// no `include`.
    #[test]
    fn the_engine_config_carries_the_walks_exclude_list() {
        let root = scoped_project("config");
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
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real pyright, handed the config [`prepare`] builds, does not track
    /// the pruned `junk/` and still resolves an import into the pruned
    /// `genpkg/`. Ignored by default because it needs pyright (locally or
    /// through `npx`): `cargo test -p g-mesh-plugin-python -- --ignored`.
    #[test]
    #[ignore = "needs pyright, locally installed or through npx"]
    fn real_pyright_skips_a_pruned_tree_and_still_resolves_imports_into_one() {
        let root = scoped_project("pyright");
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
        let _ = std::fs::remove_dir_all(&root);
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

        /// How long one request may take, `npx` download included.
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
        let err = resolve(command, Path::new("/projects/thing"), HOST_SCRIPT_EXTENSIONS)
            .expect_err("must not resolve");
        let message = format!("{err:#}");
        assert!(message.contains("npm install pyright"), "{message}");
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
