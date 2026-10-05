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

    /// A scratch directory, removed on drop.
    #[cfg(unix)]
    struct Scratch(PathBuf);

    #[cfg(unix)]
    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("g-mesh-sdk-resolve-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A probe that runs `script` under `/bin/sh -c`. [`probe`] appends
    /// `--version`, which `sh -c` takes as `$0` and ignores.
    #[cfg(unix)]
    fn sh(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("/bin/sh"), vec!["-c".to_string(), script.to_string()])
    }

    /// A twin that is visibly not the identity, so a test can tell a probe
    /// that went through it from one that did not.
    fn marked_twin(command: &Path) -> PathBuf {
        PathBuf::from(format!("{}#probe", command.display()))
    }

    /// A server whose probe bin differs from its server bin, as pyright's does.
    fn split_server() -> NpmServer<'static> {
        NpmServer {
            package: "some-package",
            bin: "some-langserver",
            probe_bin: "some-cli",
            twin: marked_twin,
        }
    }

    #[test]
    fn is_bare_means_one_relative_component() {
        assert!(is_bare(Path::new("vtsls")));
        assert!(!is_bare(Path::new("/usr/local/bin/vtsls")), "absolute");
        assert!(!is_bare(Path::new("servers/vtsls")), "relative, but two components");
        assert!(!is_bare(Path::new("./vtsls")), "`./` makes it a path to run as written");
        #[cfg(unix)]
        assert!(!is_bare(Path::new("/")), "the root is absolute");
    }

    /// The extension test is on the file name alone: a dot in a directory is
    /// not an extension, and any explicit extension - not only one from the
    /// list - stops the appending.
    #[test]
    fn script_spellings_reads_the_extension_of_the_file_name_only() {
        assert_eq!(
            script_spellings(Path::new("/opt/vtsls-1.2/bin/vtsls"), &[".cmd"]),
            vec![PathBuf::from("/opt/vtsls-1.2/bin/vtsls"), PathBuf::from("/opt/vtsls-1.2/bin/vtsls.cmd")]
        );
        assert_eq!(script_spellings(Path::new("vtsls.exe"), &[".cmd"]), vec![PathBuf::from("vtsls.exe")]);
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_the_host_tries_no_script_extension() {
        assert!(HOST_SCRIPT_EXTENSIONS.is_empty(), "{HOST_SCRIPT_EXTENSIONS:?}");
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_host_tries_the_cmd_shim() {
        assert_eq!(HOST_SCRIPT_EXTENSIONS, &[".cmd"]);
    }

    /// A path is the only origin, in each of its spellings, and is probed
    /// through the twin with no args of its own.
    #[test]
    fn a_command_that_is_a_path_is_the_only_origin() {
        for path in ["/opt/servers/some-langserver", "servers/some-langserver"] {
            let candidates =
                npm_candidates(Path::new(path), Path::new("/project"), &split_server(), &[".cmd"]);
            let cmd = format!("{path}.cmd");
            assert_eq!(
                candidates,
                vec![
                    Candidate {
                        command: PathBuf::from(path),
                        prefix_args: Vec::new(),
                        probe: (PathBuf::from(format!("{path}#probe")), Vec::new()),
                        origin: "the path the manifest names",
                    },
                    Candidate {
                        command: PathBuf::from(&cmd),
                        prefix_args: Vec::new(),
                        probe: (PathBuf::from(format!("{cmd}#probe")), Vec::new()),
                        origin: "the path the manifest names",
                    },
                ],
                "{path}"
            );
        }
    }

    /// Origin priority outranks spelling: every spelling on `PATH` before any
    /// in `node_modules/.bin`, and every one of those before `npx`.
    #[test]
    fn a_bare_name_is_path_then_node_modules_then_npx() {
        let root = Path::new("/project");
        let local = root.join("node_modules/.bin");
        let order = |extensions: &[&str]| -> Vec<(PathBuf, &'static str)> {
            npm_candidates(Path::new("some-langserver"), root, &split_server(), extensions)
                .into_iter()
                .map(|candidate| (candidate.command, candidate.origin))
                .collect()
        };

        assert_eq!(
            order(&[".cmd"]),
            vec![
                (PathBuf::from("some-langserver"), "PATH"),
                (PathBuf::from("some-langserver.cmd"), "PATH"),
                (local.join("some-langserver"), "the project's node_modules/.bin"),
                (local.join("some-langserver.cmd"), "the project's node_modules/.bin"),
                (PathBuf::from("npx"), "npx"),
                (PathBuf::from("npx.cmd"), "npx"),
            ]
        );
        assert_eq!(
            order(&[]),
            vec![
                (PathBuf::from("some-langserver"), "PATH"),
                (local.join("some-langserver"), "the project's node_modules/.bin"),
                (PathBuf::from("npx"), "npx"),
            ],
            "no host extensions: one spelling per origin"
        );
    }

    /// Installed candidates run as written and are probed through the twin;
    /// only `npx` carries prefix args.
    #[test]
    fn installed_candidates_are_probed_through_the_twin() {
        let candidates =
            npm_candidates(Path::new("some-langserver"), Path::new("/project"), &split_server(), &[]);
        let installed: Vec<&Candidate> =
            candidates.iter().filter(|candidate| candidate.origin != "npx").collect();
        assert_eq!(installed.len(), 2, "{candidates:?}");
        for candidate in installed {
            assert!(candidate.prefix_args.is_empty(), "{candidate:?}");
            assert_eq!(candidate.probe, (marked_twin(&candidate.command), Vec::new()), "{candidate:?}");
        }
    }

    /// The server runs `npx --yes --package <package> <command>`, and the
    /// probe runs the same argv with the probe bin in the command's place.
    /// Nothing here runs `npx`.
    #[test]
    fn the_npx_candidate_names_the_package_and_probes_the_probe_bin() {
        let candidates =
            npm_candidates(Path::new("some-langserver"), Path::new("/project"), &split_server(), &[".cmd"]);
        let npx: Vec<&Candidate> = candidates.iter().filter(|candidate| candidate.origin == "npx").collect();
        assert_eq!(npx.len(), 2, "{candidates:?}");
        let server_args: Vec<String> =
            ["--yes", "--package", "some-package", "some-langserver"].map(String::from).to_vec();
        let probe_args: Vec<String> =
            ["--yes", "--package", "some-package", "some-cli"].map(String::from).to_vec();
        for (candidate, program) in npx.into_iter().zip(["npx", "npx.cmd"]) {
            assert_eq!(candidate.command, PathBuf::from(program));
            assert_eq!(candidate.prefix_args, server_args, "{candidate:?}");
            assert_eq!(candidate.probe, (PathBuf::from(program), probe_args.clone()), "{candidate:?}");
        }
    }

    /// The process is killed, not merely abandoned: `sh` would write the
    /// marker after the budget if it were still alive.
    #[cfg(unix)]
    #[test]
    fn a_probe_past_its_budget_is_killed() {
        let scratch = Scratch::new("killed");
        let marker = scratch.0.join("still-alive");
        let (program, args) = sh(&format!("sleep 2; : > '{}'", marker.display()));
        let started = Instant::now();
        let err = probe(&program, &args, Duration::from_millis(200)).expect_err("past its budget");
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        std::thread::sleep(Duration::from_secs(3).saturating_sub(started.elapsed()));
        assert!(!marker.exists(), "the probe outlived its budget and kept running");
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_that_exits_non_zero_is_an_error_naming_its_stderr() {
        let (program, args) = sh("echo 1.0.0; echo 'shim: no such component' >&2; exit 3");
        let err =
            probe(&program, &args, Duration::from_secs(10)).expect_err("a non-zero exit is not an answer");
        let message = format!("{err:#}");
        assert!(message.contains("exited"), "{message}");
        assert!(message.contains("shim: no such component"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_with_empty_stdout_reports_no_version() {
        for script in ["exit 0", "printf '  \\n'"] {
            let (program, args) = sh(script);
            assert_eq!(
                probe(&program, &args, Duration::from_secs(10)).expect("a zero exit answers"),
                "no version reported",
                "{script}"
            );
        }
        let (program, args) = sh("echo '  vtsls 0.2.9  '");
        assert_eq!(probe(&program, &args, Duration::from_secs(10)).unwrap(), "vtsls 0.2.9", "trimmed");
    }

    #[test]
    fn a_probe_that_cannot_start_is_an_error() {
        let err = probe(Path::new("/nonexistent/some-langserver"), &[], Duration::from_secs(10))
            .expect_err("nothing to run");
        assert!(format!("{err:#}").contains("could not run /nonexistent/some-langserver"), "{err:#}");
    }

    /// The first candidate that answers wins, and the ones after it are never
    /// probed. `resolve` probes in order, one at a time.
    #[cfg(unix)]
    #[test]
    fn resolve_takes_the_first_candidate_that_answers() {
        let scratch = Scratch::new("first");
        let marker = scratch.0.join("third-probed");
        let candidates = vec![
            Candidate {
                command: "refuses".into(),
                prefix_args: Vec::new(),
                probe: sh("exit 1"),
                origin: "one",
            },
            Candidate {
                command: "answers".into(),
                prefix_args: vec!["--prefix".to_string()],
                probe: sh("echo 'first 1.0'"),
                origin: "two",
            },
            Candidate {
                command: "also-answers".into(),
                prefix_args: Vec::new(),
                probe: sh(&format!(": > '{}'; echo 'second 2.0'", marker.display())),
                origin: "three",
            },
        ];
        let resolved = resolve(candidates, Duration::from_secs(10), "widget", "Install a widget").unwrap();
        assert_eq!(resolved.command, PathBuf::from("answers"));
        assert_eq!(resolved.prefix_args, vec!["--prefix".to_string()]);
        assert_eq!(resolved.version, "first 1.0");
        assert_eq!(resolved.origin, "two");
        assert!(!marker.exists(), "a candidate after the winner was probed");
    }

    /// Each failure names the probe that ran and the candidate's origin, in
    /// candidate order, and the remedy closes the message.
    #[cfg(unix)]
    #[test]
    fn resolve_with_no_answer_lists_every_failure_and_the_remedy() {
        let candidates = vec![
            Candidate {
                command: "missing".into(),
                prefix_args: Vec::new(),
                probe: (PathBuf::from("/nonexistent/widget-cli"), Vec::new()),
                origin: "first origin",
            },
            Candidate {
                command: "refuses".into(),
                prefix_args: Vec::new(),
                probe: sh("echo refused >&2; exit 2"),
                origin: "second origin",
            },
        ];
        let err = resolve(candidates, Duration::from_secs(10), "widget", "Install a widget.")
            .expect_err("nothing answered");
        let message = format!("{err:#}");
        let first = "/nonexistent/widget-cli (first origin): could not run /nonexistent/widget-cli";
        let second = "/bin/sh (second origin): `--version` exited";
        assert!(message.starts_with(&format!("no usable widget: {first}")), "{message}");
        let at = message.find(second).unwrap_or_else(|| panic!("{message}"));
        assert!(message[..at].ends_with("; "), "failures are joined with `; `: {message}");
        assert!(message[at..].contains("refused"), "{message}");
        assert!(message.ends_with("). Install a widget."), "{message}");
    }
}
