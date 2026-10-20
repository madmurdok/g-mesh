//! npm's `.cmd` shims (`vtsls.cmd`,
//! `pyright-langserver.cmd`) run `node` through `cmd.exe`, and `cmd.exe`
//! looks in its current directory - the indexed project - before `PATH`,
//! unless `NoDefaultCurrentDirectoryInExePath` is set. These tests put a
//! hostile `node.exe` and `python.exe` in a project root, run a stand-in
//! shim through the two production entry points that set the variable
//! ([`probe`] and [`LspClient::start`]), and check that neither ran.
//!
//! The hostile executables are copies of this test binary: run as
//! `node.exe <filter> --ignored`, it runs only
//! [`stand_in_for_a_hostile_exe`], which leaves a `<stem>-ran` marker in its
//! current directory. No real `node`/`python` is needed: with the variable
//! set, `cmd.exe` either finds the real one on `PATH` (which fails on the
//! unknown script and leaves no marker) or reports it missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::client::LspClient;
use super::config::SemanticConfig;
use super::resolve::probe;

/// Selects [`stand_in_for_a_hostile_exe`] and nothing else in this binary.
const STAND_IN_FILTER: &str = "stand_in_for_a_hostile_exe";

/// The programs a shim resolves through `cmd.exe`'s lookup.
const HOSTILE: [&str; 2] = ["node", "python"];

/// Not a test: the body of the hostile `node.exe`/`python.exe`. It does
/// nothing unless this binary is running under one of those names, so a run
/// of ignored tests stays harmless.
#[test]
#[ignore = "run only as the hostile node.exe/python.exe stand-in"]
fn stand_in_for_a_hostile_exe() {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
    if HOSTILE.contains(&stem.as_str()) {
        let cwd = std::env::current_dir().unwrap();
        std::fs::write(cwd.join(format!("{stem}-ran")), "ran").unwrap();
    }
}

/// Copies this test binary into `dir` as every hostile name.
fn plant_hostile_exes(dir: &Path) {
    let me = std::env::current_exe().unwrap();
    for name in HOSTILE {
        std::fs::copy(&me, dir.join(format!("{name}.exe"))).unwrap();
    }
}

fn marker(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}-ran"))
}

/// A stand-in npm shim: optionally changes directory, runs `<program>` the
/// way npm's shims run `node` (a bare name, resolved by `cmd.exe`), then
/// leaves `done` so the caller knows the line ran.
fn write_shim(path: &Path, program: &str, cd: Option<&Path>, done: &Path) {
    let mut body = String::from("@echo off\r\n");
    if let Some(dir) = cd {
        body.push_str(&format!("cd /d \"{}\"\r\n", dir.display()));
    }
    body.push_str(&format!("{program} {STAND_IN_FILTER} --ignored\r\n"));
    body.push_str(&format!("echo ran> \"{}\"\r\n", done.display()));
    std::fs::write(path, body).unwrap();
}

fn wait_for(path: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

/// Sanity for both tests below: the copied binary, run directly under a
/// hostile name, does leave its marker. Without this, an absent marker
/// could mean the stand-in never works rather than that it never ran.
fn assert_the_stand_in_works() {
    let dir = tempfile::tempdir().unwrap();
    plant_hostile_exes(dir.path());
    for name in HOSTILE {
        let status = Command::new(dir.path().join(format!("{name}.exe")))
            .args([STAND_IN_FILTER, "--ignored"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{name}.exe stand-in: {status}");
        assert!(marker(dir.path(), name).exists(), "the {name}.exe stand-in left no marker");
    }
}

/// A probe of an npm-style shim whose `cmd.exe` stands in a project root
/// holding a `node.exe`/`python.exe` never runs them.
#[test]
fn a_probe_through_a_cmd_shim_never_runs_a_node_or_python_exe_from_the_project_root() {
    assert_the_stand_in_works();
    let root = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    plant_hostile_exes(root.path());

    for name in HOSTILE {
        let shim = shims.path().join(format!("{name}-langserver.cmd"));
        let done = shims.path().join(format!("{name}-probe-done"));
        write_shim(&shim, name, Some(root.path()), &done);

        let _ = probe(&shim, &[], Duration::from_secs(30));

        assert!(wait_for(&done, Duration::from_secs(30)), "the {name} shim never reached its last line");
        assert!(!marker(root.path(), name).exists(), "the probe ran {name}.exe from the project root");
    }
}

/// A language server started through an npm-style shim, with the project
/// root as its current directory, never runs a `node.exe`/`python.exe`
/// planted in that root.
#[test]
fn a_server_started_through_a_cmd_shim_never_runs_a_node_or_python_exe_from_the_project_root() {
    assert_the_stand_in_works();
    let root = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    plant_hostile_exes(root.path());

    for name in HOSTILE {
        let shim = shims.path().join(format!("{name}-langserver.cmd"));
        let done = shims.path().join(format!("{name}-start-done"));
        write_shim(&shim, name, None, &done);

        let started = LspClient::start(
            "test",
            &SemanticConfig::new(&shim),
            root.path(),
            Instant::now() + Duration::from_secs(30),
            false,
            false,
        );
        assert!(started.is_err(), "a shim that exits at once is not a client");

        assert!(wait_for(&done, Duration::from_secs(30)), "the {name} shim never reached its last line");
        assert!(
            !marker(root.path(), name).exists(),
            "the server's shim ran {name}.exe from the project root"
        );
    }
}
