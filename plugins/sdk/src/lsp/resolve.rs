//! Finding a language server's binary, and proving it is one before the
//! bridge is handed it.
//!
//! A plugin builds an ordered list of [`Candidate`]s - by hand, or with
//! [`npm_candidates`] for a server published on npm - and [`resolve`] runs
//! each one's `--version` probe under a budget, taking the first that
//! answers. A candidate is accepted for answering, never for existing: a
//! shim that always exists and always starts (a rustup proxy for an
//! uninstalled component, `npx` with no network) is otherwise a server that
//! dies at handshake once per pass, which the bridge cannot tell from a
//! missing one.
//!
//! The rationale for the candidate order, the `.cmd` spellings and the probe
//! budget is in `docs/architecture/gm-325-typescript-lsp-semantics.md`, §2
//! and §5, and in `plugins/python/src/semantic.rs`'s module doc.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// The npm shim extension worth trying beyond a candidate's bare spelling.
/// npm writes a Windows bin as `<name>.cmd`, which `CreateProcessW` - and so
/// `Command::new` - never tries on its own.
pub const WINDOWS_SCRIPT_EXTENSIONS: [&str; 1] = [".cmd"];

/// The script extensions worth trying on the host this process runs on:
/// [`WINDOWS_SCRIPT_EXTENSIONS`] on Windows, none elsewhere.
///
/// [`script_spellings`] and [`npm_candidates`] take the list as a parameter
/// rather than reading this, so their Windows arm runs under tests on any
/// host.
#[cfg(windows)]
pub const HOST_SCRIPT_EXTENSIONS: &[&str] = &WINDOWS_SCRIPT_EXTENSIONS;
/// The script extensions worth trying on this host: none, off Windows.
#[cfg(not(windows))]
pub const HOST_SCRIPT_EXTENSIONS: &[&str] = &[];

/// How long one candidate's `--version` may take before it is killed and
/// counted as a failure. It runs inside a semantic pass, and an `npx` probe
/// against an unreachable registry does not fail fast.
pub const PROBE_BUDGET: Duration = Duration::from_secs(60);

/// Where an npm project keeps its locally installed bins.
const NODE_BIN_DIR: &str = "node_modules/.bin";

/// One spelling of "run this language server", and how to prove it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// argv\[0\] for the server.
    pub command: PathBuf,
    /// argv entries that come *before* the manifest's own `args` - the
    /// package, for an `npx` candidate, and nothing for the others.
    pub prefix_args: Vec<String>,
    /// The program to ask `--version`, and the args that precede it.
    pub probe: (PathBuf, Vec<String>),
    /// What to call this candidate in the log line and in a failure message.
    pub origin: &'static str,
}

/// A candidate that answered, and what it said.
#[derive(Debug)]
pub struct Resolved {
    /// argv\[0\] for the server.
    pub command: PathBuf,
    /// argv entries that come before the manifest's own `args`.
    pub prefix_args: Vec<String>,
    /// What the probe printed, or `no version reported`.
    pub version: String,
    /// The candidate's [`Candidate::origin`].
    pub origin: &'static str,
}

/// A language server published on npm.
#[derive(Debug, Clone, Copy)]
pub struct NpmServer<'a> {
    /// The npm *package*, which is what `npx --package` wants; a bin name
    /// there is a registry 404 when the two differ.
    pub package: &'a str,
    /// The package's language-server bin, the manifest's usual `command`.
    pub bin: &'a str,
    /// The bin the `npx` candidate's probe runs. It differs from the server's
    /// bin only when the server itself has no `--version`.
    pub probe_bin: &'a str,
    /// Maps an installed server path to the path its probe runs, keeping the
    /// directory and extension. Identity when the server answers `--version`.
    pub twin: fn(&Path) -> PathBuf,
}

/// Whether `command` is a bare name to search for rather than a path to run
/// as written.
pub fn is_bare(command: &Path) -> bool {
    command.components().count() == 1 && !command.is_absolute()
}

/// `path`, and - when it has no extension of its own - `path` with each of
/// `extensions` appended to its file name, in the order given.
///
/// An explicit extension is never touched. Pure: nothing here checks whether
/// a spelling exists.
pub fn script_spellings(path: &Path, extensions: &[&str]) -> Vec<PathBuf> {
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

/// The candidates for an npm-published server, in order.
///
/// A `command` that is a path is the only origin. A bare one is `PATH`, then
/// `<root>/node_modules/.bin`, then `npx --yes --package <package> <command>`.
/// Within each origin every [`script_spellings`] spelling is tried before the
/// next origin: origin priority outranks spelling.
///
/// Every installed candidate is probed through `npm.twin`; the `npx`
/// candidate's probe is its own argv with `npm.probe_bin` in place of the
/// server's bin, so the probe proves the argv shape that will run.
pub fn npm_candidates(command: &Path, root: &Path, npm: &NpmServer, extensions: &[&str]) -> Vec<Candidate> {
    let installed = |command: PathBuf, origin: &'static str| Candidate {
        probe: ((npm.twin)(&command), Vec::new()),
        command,
        prefix_args: Vec::new(),
        origin,
    };
    if !is_bare(command) {
        return script_spellings(command, extensions)
            .into_iter()
            .map(|command| installed(command, "the path the manifest names"))
            .collect();
    }

    let name = command.to_string_lossy().into_owned();
    let local = root.join(NODE_BIN_DIR).join(&name);
    let npx_args = |bin: &str| -> Vec<String> {
        vec!["--yes".to_string(), "--package".to_string(), npm.package.to_string(), bin.to_string()]
    };
    let mut candidates = Vec::new();
    for command in script_spellings(command, extensions) {
        candidates.push(installed(command, "PATH"));
    }
    for command in script_spellings(&local, extensions) {
        candidates.push(installed(command, "the project's node_modules/.bin"));
    }
    for command in script_spellings(Path::new("npx"), extensions) {
        candidates.push(Candidate {
            prefix_args: npx_args(&name),
            probe: (command.clone(), npx_args(npm.probe_bin)),
            command,
            origin: "npx",
        });
    }
    candidates
}

/// Runs `<command> <args…> --version` and returns what it printed, killing it
/// if it has not finished within `budget`.
///
/// Both pipes are drained on their own threads: a child that fills one while
/// this side waits on the other would otherwise deadlock.
pub fn probe(command: &Path, args: &[String], budget: Duration) -> Result<String> {
    let mut child = Command::new(command)
        .args(args)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", command.display()))?;

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

/// The first of `candidates` whose probe answers within `budget`.
///
/// When none does, the error reads `no usable <what>: <each probe (origin):
/// reason>. <remedy>`.
pub fn resolve(candidates: Vec<Candidate>, budget: Duration, what: &str, remedy: &str) -> Result<Resolved> {
    let mut failures: Vec<String> = Vec::new();
    for candidate in candidates {
        let (probe_command, probe_args) = &candidate.probe;
        match probe(probe_command, probe_args, budget) {
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
    bail!("no usable {what}: {}. {remedy}", failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Someone who wrote `pyright-langserver.cmd` meant exactly that file,
    /// not `pyright-langserver.cmd.cmd`.
    #[test]
    fn script_spellings_does_not_touch_an_explicit_extension() {
        assert_eq!(
            script_spellings(Path::new("pyright-langserver.cmd"), &[".cmd"]),
            vec![PathBuf::from("pyright-langserver.cmd")]
        );
    }

    /// `sh -c "sleep 30"` rather than `sleep 30`, because [`probe`] appends
    /// `--version` and `sleep` rejects it; under `sh -c` a trailing word is
    /// `$0` and is ignored.
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
}
