//! End-to-end coverage for `g-mesh mcp-shim`: it proxies to an already
//! running daemon, and bootstraps a detached one when none is running.
//! Everything here drives the real binary as a subprocess - the point is to
//! exercise actual process spawning, sockets and framing, not a mock.
//!
//! The wire probe is a real MCP `initialize` + `tools/list`, hand-framed as
//! newline-delimited JSON, because that is the only traffic the shim carries;
//! what each test is actually about, though, is which daemon ends up serving
//! it.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every tool the MVP promises, whatever order the router lists them in.
///
/// Pre-existing drift fixed in passing (unrelated to task 155): `search_code`
/// shipped in commit 445ac98 without updating this list, so every test below
/// was already failing on this branch before the plugin registry work
/// started.
const EXPECTED_TOOLS: [&str; 8] = [
    "find_callees",
    "find_callers",
    "find_definition",
    "find_implementations",
    "find_references",
    "get_dependencies",
    "get_file_outline",
    "search_code",
];

/// Temp project root plus teardown of the `~/.g-mesh/projects/<hash>/`
/// directory the daemon creates outside it.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root") }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn daemon_pid(&self) -> u32 {
        let path = self.pid_file();
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if self.pid_file().exists() {
            let pid = self.daemon_pid();
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
}

/// Every spawn in this suite clears [`g_mesh::shim::PROJECT_DIR_ENV`] for the
/// same reason this one does, and
/// [`every_shim_spawn_in_this_suite_decides_which_project_it_serves`] holds
/// the whole directory to it: the variable is set by Claude Code on the MCP
/// servers it spawns and inherited by everything underneath them, so a
/// `cargo test` launched from one - a release script, an editor's test
/// integration, an MCP server that shells out - hands every shim here the
/// *session's* project root and quietly discards the `current_dir` below.
/// Task 192: that is what made this suite hang for three releases, always in
/// the alphabetically first file to reach a shim, always as
/// "the cold-start bulk walk for /var/folders/.../.tmpXXXX did not finish" -
/// a walk that was never running, for a project no daemon had been asked to
/// serve.
fn spawn_shim(root: &Path) -> Child {
    Command::new(BIN)
        .arg("mcp-shim")
        .current_dir(root)
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the shim")
}

/// Spawned with `cwd` and `CLAUDE_PROJECT_DIR` deliberately pointed at two
/// different directories, so a round trip through it can prove which one it
/// actually treated as the project root.
fn spawn_shim_with_project_dir_env(cwd: &Path, project_dir_env: &Path) -> Child {
    Command::new(BIN)
        .arg("mcp-shim")
        .current_dir(cwd)
        .env(g_mesh::shim::PROJECT_DIR_ENV, project_dir_env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the shim")
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Runs `initialize` + `tools/list` over one duplex channel and returns the
/// tool names, sorted.
///
/// The conversation runs on a helper thread so a peer that never answers
/// fails the test instead of hanging it forever; when that thread finishes it
/// drops `writer`, which is what closes the shim's stdin and lets it exit.
fn mcp_tool_names<W, R>(writer: W, reader: R) -> Vec<String>
where
    W: Write + Send + 'static,
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(list_tools(writer, reader));
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(names)) => names,
        Ok(Err(err)) => panic!("MCP session failed: {err}"),
        Err(err) => panic!("MCP session did not finish within {TIMEOUT:?}: {err}"),
    }
}

fn list_tools<W: Write, R: Read>(mut writer: W, reader: R) -> Result<Vec<String>, String> {
    let mut reader = BufReader::new(reader);

    send(&mut writer, &json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-shim-tests", "version": "0" },
        },
    }))?;
    let initialized = receive(&mut reader)?;
    if initialized["result"]["serverInfo"]["name"] != "g-mesh" {
        return Err(format!("unexpected initialize response: {initialized}"));
    }
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;

    send(&mut writer, &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))?;
    let listed = receive(&mut reader)?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or_else(|| format!("tools/list did not return a tool array: {listed}"))?;

    let mut names: Vec<String> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    Ok(names)
}

fn send<W: Write>(writer: &mut W, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).expect("request is always serializable");
    write_ndjson_frame(writer, &body).map_err(|e| format!("cannot send {message}: {e:#}"))
}

fn receive<R: Read>(reader: &mut BufReader<R>) -> Result<Value, String> {
    let frame = read_ndjson_frame(reader)
        .map_err(|e| format!("cannot read a response: {e:#}"))?
        .ok_or("the peer closed the connection instead of answering")?;
    serde_json::from_slice(&frame).map_err(|e| {
        format!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame))
    })
}

/// One MCP probe through the shim's stdio, then waits for it to exit - the
/// helper thread closing stdin is what tells it to.
fn round_trip_through_shim(shim: &mut Child) -> Vec<String> {
    let stdin = shim.stdin.take().expect("shim stdin was not piped");
    let stdout = shim.stdout.take().expect("shim stdout was not piped");
    let names = mcp_tool_names(stdin, stdout);

    let status = wait_with_timeout(shim);
    assert!(status.success(), "shim exited with {status}");
    names
}

fn wait_with_timeout(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the child process") {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("child process did not exit within {TIMEOUT:?}");
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// One MCP probe over the daemon socket directly, bypassing the shim.
fn round_trip_over_socket(socket: &Path) -> Vec<String> {
    let stream = UnixStream::connect(socket)
        .unwrap_or_else(|e| panic!("failed to connect to {}: {e}", socket.display()));
    let writer = stream.try_clone().expect("failed to clone the daemon socket");
    mcp_tool_names(writer, stream)
}

/// Asserts a live daemon answered with the real tool surface (rather than,
/// say, the shim echoing the request back).
fn assert_tool_surface(names: &[String]) {
    assert_eq!(names, EXPECTED_TOOLS);
}

#[test]
fn shim_proxies_through_an_already_running_daemon() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    let socket = project.socket();
    // The pid file is written once the socket is bound, so it is the signal
    // that the daemon is actually listening.
    let pid_file = project.pid_file();
    wait_for("the daemon to start listening", || pid_file.exists());

    let pid_before = project.daemon_pid();
    assert_eq!(pid_before, daemon.id(), "pid file must name the daemon we started");

    let mut shim = spawn_shim(project.root());
    assert_tool_surface(&round_trip_through_shim(&mut shim));

    assert!(socket.exists(), "the daemon socket must survive the shim");
    assert_eq!(
        project.daemon_pid(),
        pid_before,
        "the shim must reuse the running daemon, not start another one"
    );
    assert!(daemon.try_wait().unwrap().is_none(), "the daemon must still be running");
    // Teardown kills by pid via `Project::drop`.
}

#[test]
fn shim_bootstraps_a_detached_daemon_when_none_is_running() {
    let project = Project::new();
    let socket = project.socket();
    assert!(!socket.exists(), "no daemon may be running for a fresh project root");

    let mut shim = spawn_shim(project.root());
    assert_tool_surface(&round_trip_through_shim(&mut shim));

    assert!(socket.exists(), "the bootstrapped daemon must have bound its socket");
    let pid = project.daemon_pid();

    // The shim has already exited; a second round trip over the same socket
    // proves the daemon it spawned is genuinely detached rather than a child
    // that died with its parent.
    assert_tool_surface(&round_trip_over_socket(&socket));

    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("failed to signal the daemon");
    assert!(alive.success(), "daemon pid {pid} is no longer alive");
}

#[test]
fn shim_prefers_claude_project_dir_env_over_cwd() {
    // Two distinct roots: `cwd` is where the shim process happens to run,
    // `env_project` is what CLAUDE_PROJECT_DIR names - the real project, per
    // Claude Code's contract that cwd is not reliable for this. Only the
    // latter's daemon/socket may ever come up.
    let cwd_decoy = Project::new();
    let env_project = Project::new();

    let mut shim = spawn_shim_with_project_dir_env(cwd_decoy.root(), env_project.root());
    assert_tool_surface(&round_trip_through_shim(&mut shim));

    assert!(
        env_project.socket().exists(),
        "the daemon must have bootstrapped for CLAUDE_PROJECT_DIR, not cwd"
    );
    assert!(
        !cwd_decoy.socket().exists(),
        "cwd must be ignored once CLAUDE_PROJECT_DIR is set"
    );
}

/// The regression guard for task 192, and deliberately a check on this
/// directory's source rather than on a running shim.
///
/// The behaviour is not in doubt and is already covered both ways:
/// [`shim_prefers_claude_project_dir_env_over_cwd`] proves the variable wins
/// when it is set, and every other test here proves cwd is used when it is
/// not. What broke three releases was neither - it was a *test suite* that
/// never said which of the two it wanted, and so silently got the first one
/// whenever `cargo test` ran underneath something that exports it. A runtime
/// test can only ever assert that for the one spawn it performs; the property
/// that actually needs holding is "no spawn anywhere in `core/tests` leaves
/// this to the ambient environment", and that is a property of the files.
///
/// A new test file that spawns a shim and forgets fails here, by name, with
/// the line to fix - rather than by hanging for 90 seconds in whichever file
/// happens to sort first, months later, on someone else's release.
///
/// Both ways of deciding count: clearing the variable (what a test with a
/// `current_dir` of its own wants) and setting it (what
/// `spawn_shim_with_project_dir_env` wants). Only inheriting it silently is
/// the failure.
#[test]
fn every_shim_spawn_in_this_suite_decides_which_project_it_serves() {
    // Comments are stripped before anything is judged, so that neither a doc
    // comment mentioning the subcommand counts as a spawn, nor a commented-out
    // `env_remove` counts as a decision.
    fn code(line: &str) -> &str {
        line.split("//").next().unwrap_or("")
    }
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut undecided: Vec<String> = Vec::new();
    let mut spawns = 0usize;

    let entries = std::fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", tests_dir.display()));
    for entry in entries {
        let path = entry.expect("failed to read a directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let lines: Vec<&str> = source.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            // Written as an escaped literal so this very line is not one of
            // the spawn sites it goes looking for.
            if !code(line).contains("arg(\"mcp-shim\")") {
                continue;
            }
            spawns += 1;
            // The rest of the statement the spawn is part of: builder chains
            // here run over anything from one line to a dozen, so the extent
            // is the terminating semicolon rather than a fixed window.
            let mut statement = String::new();
            for following in &lines[index..] {
                statement.push_str(code(following));
                if code(following).contains(';') {
                    break;
                }
            }
            if !statement.contains("PROJECT_DIR_ENV") {
                let name = path.file_name().unwrap_or_default();
                undecided.push(format!("{}:{}", name.to_string_lossy(), index + 1));
            }
        }
    }

    // A scanner that has stopped recognizing spawn sites would pass this test
    // by finding nothing at all, which is the one way it could go quietly
    // wrong. The floor is well under today's count, so ordinary churn does not
    // touch it, and a rename of the subcommand or the builder shape does.
    assert!(
        spawns >= 20,
        "only {spawns} `mcp-shim` spawns were recognized in {} - the scan below has stopped \
         matching how this suite spawns them, and is no longer guarding anything",
        tests_dir.display()
    );
    assert!(
        undecided.is_empty(),
        "these `mcp-shim` spawns inherit CLAUDE_PROJECT_DIR from whatever launched \
         `cargo test`, so they serve that project instead of their own fixture: {undecided:?}. \
         Add `.env_remove(g_mesh::shim::PROJECT_DIR_ENV)` to each (or set it deliberately, \
         as `spawn_shim_with_project_dir_env` does)."
    );
}

#[test]
fn two_concurrent_shim_bootstraps_produce_exactly_one_daemon() {
    let project = Project::new();
    let socket = project.socket();
    assert!(!socket.exists(), "no daemon may be running for a fresh project root");

    // Spawned back-to-back with no synchronization in between: both are
    // independent OS processes that start racing to bootstrap this
    // project's daemon the instant they're spawned, well before either of
    // them appears in this test's own control flow below. That's the real
    // concurrency the bootstrap lock has to serialize.
    let mut shim_a = spawn_shim(project.root());
    let mut shim_b = spawn_shim(project.root());

    assert_tool_surface(&round_trip_through_shim(&mut shim_a));
    assert_tool_surface(&round_trip_through_shim(&mut shim_b));

    let pid = project.daemon_pid();

    // If a second daemon had ever won a later bind race, it would have
    // overwritten the pid file with its own pid; a third round trip straight
    // over the socket still being served, with the pid file unchanged, proves
    // the daemon both shims talked to is still the only one alive.
    assert_tool_surface(&round_trip_over_socket(&socket));
    assert_eq!(project.daemon_pid(), pid, "a second daemon must never have taken over");

    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("failed to signal the daemon");
    assert!(alive.success(), "daemon pid {pid} is no longer alive");
}
