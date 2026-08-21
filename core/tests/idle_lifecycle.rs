//! The two-tier idle lifecycle, driven through a real daemon with both
//! timeouts shortened (`daemon::lifecycle`'s `G_MESH_*_IDLE_MS` overrides) so
//! an hour and a day fit inside a test run.
//!
//! Nothing here fakes a timer or calls the supervisor directly: the whole
//! claim being tested is about processes - one that has to exit while the
//! other carries on, and a queue that has to survive in between - and that is
//! only observable from outside them. So the assertions are made against pids,
//! pid files, the socket, the index's own rows, and the one log line the
//! daemon prints when it wakes its plugin.
//!
//! # Why the daemon's stderr is read
//!
//! "Replays only the queued dirty files, not a full rescan" cannot be settled
//! from the index alone: a rescan and a one-file replay leave the same rows
//! behind, since a rescan would re-derive every *other* file to exactly what
//! it already was. What distinguishes them is which files the plugin was asked
//! about, and the daemon says so out loud on the wake path - so this test
//! reads that line and asserts on the exact list, rather than on a count that
//! could be right for the wrong reason.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::lifecycle::{CORE_IDLE_ENV, PLUGIN_IDLE_ENV};
use g_mesh::ipc;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
/// Deadline for anything this test waits on. Generous - it is a hang guard,
/// not a timing assertion; the timings being asserted are the configured
/// timeouts below.
const TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The plugin's shortened idle timeout. Not as short as it could be: the test
/// also has to *catch* the plugin awake after the wake that replays the queue,
/// and a timeout of a few hundred milliseconds would race its own assertions
/// by putting it straight back to sleep.
const PLUGIN_IDLE: Duration = Duration::from_millis(1_500);
/// The core's, in the test that asserts it exits. Deliberately several times
/// the plugin's, so the two-tier claim is tested with the same *ordering*
/// production has, just compressed.
const CORE_IDLE: Duration = Duration::from_millis(2_000);
/// Stands in for production's 24 hours in the test that asserts the core does
/// *not* exit: far beyond the run, so a core that goes away has gone away for
/// the wrong reason.
const CORE_IDLE_EFFECTIVELY_NEVER: Duration = Duration::from_secs(600);

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

    fn write(&self, relative_path: &str, contents: &str) {
        let path = self.root().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create a fixture directory");
        }
        std::fs::write(&path, contents).expect("failed to write a fixture file");
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory")
    }

    fn db_path(&self) -> PathBuf {
        self.state_dir().join("index.db")
    }

    /// Read straight out of the index rather than through a tool call,
    /// because a tool call is precisely what wakes the plugin - asking the
    /// daemon "is this indexed yet?" would make the answer true.
    ///
    /// Opened read-write like `cli::status` does, and for its reason: reading
    /// a database a live daemon keeps in WAL mode needs to be able to touch
    /// the WAL's side files.
    fn index_contains(&self, symbol_name: &str) -> bool {
        let Ok(conn) = Connection::open_with_flags(
            self.db_path(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        ) else {
            return false;
        };
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1", [symbol_name], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count > 0)
        .unwrap_or(false)
    }

    /// The walk-completion marker, which a second cold-start walk would
    /// rewrite. Compared before and after a wake so "not a full rescan" is
    /// asserted twice over, once from the daemon's log and once from a fact it
    /// did not narrate.
    fn bulk_indexed_at(&self) -> Option<String> {
        let conn = Connection::open_with_flags(
            self.db_path(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("the index must exist by now");
        conn.query_row("SELECT value FROM meta WHERE key = 'bulkIndexedAt'", [], |row| row.get(0)).ok()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A daemon child process plus everything it has said on stderr - the plugin's
/// own log goes there too (it inherits the daemon's stderr), which is harmless
/// since only lines this test looks for are read out of it.
struct Daemon {
    child: Child,
    log: Arc<Mutex<String>>,
}

impl Daemon {
    fn spawn(root: &Path, plugin_idle: Duration, core_idle: Duration) -> Self {
        let mut child = Command::new(BIN)
            .arg("daemon")
            .arg("--project-root")
            .arg(root)
            .env(PLUGIN_IDLE_ENV, plugin_idle.as_millis().to_string())
            .env(CORE_IDLE_ENV, core_idle.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the daemon");

        // Drained on a thread of its own: a full stderr pipe would block the
        // daemon inside a log call, which for a test about idleness would be
        // an especially confusing way to hang.
        let stderr = child.stderr.take().expect("the daemon was spawned with a piped stderr");
        let log = Arc::new(Mutex::new(String::new()));
        {
            let log = Arc::clone(&log);
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log.lock().unwrap().push_str(&line);
                    log.lock().unwrap().push('\n');
                }
            });
        }

        Self { child, log }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("failed to check on the daemon").is_none()
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// The paths named by the most recent wake, in the order the daemon
    /// replayed them.
    fn replayed_paths(&self) -> Vec<String> {
        const MARKER: &str = "queued file change(s): ";
        let log = self.log();
        let line = log
            .lines()
            .rfind(|line| line.contains(MARKER))
            .unwrap_or_else(|| panic!("the daemon never logged a plugin wake:\n{log}"));
        let (_, paths) = line.split_once(MARKER).expect("the marker was just matched");
        paths.split(", ").map(str::to_string).collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn recorded_pid(path: &Path) -> u32 {
    daemon::read_pid_file(path).unwrap_or_else(|| panic!("no pid recorded in {}", path.display()))
}

/// Runs one MCP conversation over a fresh connection and returns the
/// responses, same shape as `daemon_core.rs`'s helper of the same name (and
/// duplicated for the same reason the other integration tests duplicate it: a
/// `tests/common` entry would be shared state between suites that have no
/// other reason to agree).
fn mcp_session(endpoint: &ipc::Endpoint, requests: Vec<Value>) -> Vec<Value> {
    let stream =
        ipc::Stream::connect(endpoint).unwrap_or_else(|e| panic!("failed to connect to {endpoint}: {e}"));

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(converse(stream, requests));
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(responses)) => responses,
        Ok(Err(err)) => panic!("MCP session failed: {err}"),
        Err(err) => panic!("MCP session did not finish within {TIMEOUT:?}: {err}"),
    }
}

fn converse(stream: ipc::Stream, requests: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut writer = stream.try_clone().map_err(|e| format!("cannot clone the daemon connection: {e}"))?;
    let mut reader = BufReader::new(stream);

    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "g-mesh-idle-lifecycle-tests", "version": "0" },
            },
        }),
    )?;
    let initialized = receive(&mut reader)?;
    if initialized["result"]["serverInfo"]["name"] != "g-mesh" {
        return Err(format!("unexpected initialize response: {initialized}"));
    }
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;

    let mut responses = Vec::with_capacity(requests.len());
    for request in &requests {
        send(&mut writer, request)?;
        responses.push(receive(&mut reader)?);
    }
    Ok(responses)
}

fn send<W: Write>(writer: &mut W, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).expect("request is always serializable");
    write_ndjson_frame(writer, &body).map_err(|e| format!("cannot send {message}: {e:#}"))
}

fn receive(reader: &mut BufReader<ipc::Stream>) -> Result<Value, String> {
    let frame = read_ndjson_frame(reader)
        .map_err(|e| format!("cannot read a response: {e:#}"))?
        .ok_or("the daemon closed the connection instead of answering")?;
    serde_json::from_slice(&frame)
        .map_err(|e| format!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame)))
}

fn outline_request(id: u32, file_path: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": file_path } },
    })
}

/// The acceptance criterion, end to end: the plugin exits on its own short
/// timeout, the core does not, and the request that wakes the plugin replays
/// exactly the files that changed while it slept.
#[test]
fn the_plugin_sleeps_alone_and_a_request_replays_only_what_it_missed() {
    let project = Project::new();
    project.write("src/alpha.ts", "export function alpha(): number {\n  return 1;\n}\n");
    project.write("src/beta.ts", "export function beta(): number {\n  return 2;\n}\n");

    let mut daemon_process = Daemon::spawn(project.root(), PLUGIN_IDLE, CORE_IDLE_EFFECTIVELY_NEVER);

    let plugin_pid_file = daemon::plugin_pid_path(project.root()).unwrap();
    let core_pid_file = daemon::pid_path(project.root()).unwrap();
    let endpoint = daemon::endpoint(project.root()).unwrap();
    // Read before the walk is waited on, not after: the control-plane plugin
    // is idle for the whole cold start (the walk runs in a process of its
    // own), so it is entitled to fall asleep while that walk is still going.
    wait_for("the plugin to start", || plugin_pid_file.exists());
    let first_plugin_pid = recorded_pid(&plugin_pid_file);
    assert!(daemon::is_process_alive(first_plugin_pid), "the plugin must be up before it can sleep");
    wait_until_indexed(project.root());

    // --- the plugin falls asleep on its own -----------------------------
    // Waited on through the pid file, which is cleared *after* the process is
    // reaped, so this cannot observe a half-finished sleep.
    wait_for("the plugin to exit on its idle timeout", || !plugin_pid_file.exists());
    assert!(
        !daemon::is_process_alive(first_plugin_pid),
        "the plugin process must actually be gone, not merely unrecorded"
    );

    // --- and the core does not go with it -------------------------------
    assert!(daemon_process.is_running(), "the core must outlive the plugin's shorter timeout");
    assert_eq!(recorded_pid(&core_pid_file), daemon_process.pid(), "the core is the same process");
    assert!(
        daemon::is_listening(project.root()).unwrap(),
        "the core must still be serving its endpoint with its plugin asleep"
    );

    // --- a change made while it sleeps is queued, not processed ----------
    let bulk_indexed_at = project.bulk_indexed_at();
    project.write(
        "src/alpha.ts",
        "export function alpha(): number {\n  return 1;\n}\n\n\
         export function alphaAgain(): number {\n  return alpha() + 1;\n}\n",
    );
    // Long enough for the watcher event to have been delivered and handled;
    // what it was handled *into* is what the assertion below is about. Since
    // task 129 "handled" is no longer near-instant: the watcher thread now
    // debounces (`daemon::DEBOUNCE_WINDOW`, 300ms as of this writing) before
    // it even looks at a raw event, and its own poll loop only re-checks on
    // that same cadence, so worst case is comfortably more than one window,
    // not one. This has to clear that *before* the MCP session below opens -
    // otherwise `prepare`'s `replay_queued_changes` (`mcp::mod`) can run
    // ahead of the debounce settling, find nothing queued yet, and let a
    // same-request `ensure_file_fresh` for the other file wake the plugin
    // first; the alpha.ts change would then reach the plugin directly once
    // its own debounce fires, rather than through the queued-replay path
    // this test's `replayed_paths()` asserts on.
    thread::sleep(Duration::from_millis(900));
    assert!(
        !project.index_contains("alphaAgain"),
        "a sleeping plugin must queue the change, not reparse it - nothing may reach the index \
         until something asks"
    );
    assert!(
        !daemon::is_process_alive(first_plugin_pid) && !plugin_pid_file.exists(),
        "a file change must not by itself wake the plugin; only a request does"
    );

    // --- the next request wakes it and replays the queue -----------------
    // Deliberately a question about the *other* file: the wake is owed to the
    // queue, not to what this particular call happened to ask about.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let responses = mcp_session(&endpoint, vec![outline_request(1, "src/beta.ts")]);
        assert_eq!(
            responses[0]["result"]["isError"], false,
            "the daemon must keep answering across a plugin sleep: {}",
            responses[0]
        );
        if project.index_contains("alphaAgain") {
            break;
        }
        // Only reachable if this call arrived before the watcher had queued
        // the change; the replay happens before a call answers, so once it is
        // queued one call is enough.
        assert!(Instant::now() < deadline, "the queued change was never replayed:\n{}", daemon_process.log());
        thread::sleep(Duration::from_millis(100));
    }

    let second_plugin_pid = recorded_pid(&plugin_pid_file);
    assert_ne!(second_plugin_pid, first_plugin_pid, "waking must start a new plugin process");
    assert!(daemon::is_process_alive(second_plugin_pid), "the woken plugin must be running");

    // --- and it replayed the queue, not the project ----------------------
    let replayed = daemon_process.replayed_paths();
    assert!(
        replayed.contains(&"src/alpha.ts".to_string()),
        "the wake must replay the file that changed while the plugin slept: {replayed:?}"
    );
    assert!(
        // Asserted as "no other source file" rather than as an exact list
        // because a filesystem is entitled to report a directory alongside the
        // file written in it, and a queued directory is noise rather than a
        // rescan. What a rescan would look like is unambiguous: beta.ts, which
        // nothing touched, would be in there too.
        !replayed.iter().any(|path| path.ends_with(".ts") && path != "src/alpha.ts"),
        "a wake must replay only the queue, and a rescan would have brought src/beta.ts with \
         it: {replayed:?}"
    );
    assert_eq!(project.bulk_indexed_at(), bulk_indexed_at, "a wake must not re-run the cold-start walk");

    // --- through all of which the core was the same process --------------
    assert!(daemon_process.is_running(), "the core must not have exited at any point");
    assert_eq!(recorded_pid(&core_pid_file), daemon_process.pid());
}

/// The other half of the two-tier model: the core does have a timeout, it is
/// just its own and much longer, and reaching it is a clean exit rather than
/// an abandoned socket.
#[test]
fn the_core_exits_on_its_own_longer_timeout_with_nothing_attached() {
    let project = Project::new();
    project.write("src/alpha.ts", "export function alpha(): number {\n  return 1;\n}\n");

    // The plugin's timeout is out of reach here on purpose: whatever ends this
    // daemon, it is not the timer the other test is about.
    let mut daemon_process = Daemon::spawn(project.root(), Duration::from_secs(600), CORE_IDLE);
    wait_until_indexed(project.root());
    // Captured right after indexing, before anything else in this test can
    // spend wall-clock time - what makes `started_waiting.elapsed()` below a
    // tight enough proxy for "how long the core's own idle clock has had" to
    // assert against.
    let started_waiting = Instant::now();

    // Since task 155, the bundled plugin's post-walk semantic pass - the
    // thing that spawns it for a project nothing has otherwise touched -
    // runs on a background thread rather than inline with the walk (so a
    // structural query never waits on it), so its pid file is no longer
    // guaranteed to exist the instant `wait_until_indexed` returns. Waited on
    // here, after `started_waiting`, precisely so this wait's own duration
    // doesn't get excluded from what `started_waiting.elapsed()` measures.
    let plugin_pid_file = daemon::plugin_pid_path(project.root()).unwrap();
    wait_for("the plugin to start", || plugin_pid_file.exists());
    let plugin_pid = recorded_pid(&plugin_pid_file);

    // Not one MCP request is made for the rest of this test - that is the
    // condition being tested.
    wait_for("the core to exit on its idle timeout", || !daemon_process.is_running());
    assert!(
        started_waiting.elapsed() + Duration::from_millis(50) >= CORE_IDLE,
        "the core exited after {:?}, before its own timeout of {CORE_IDLE:?} could have elapsed",
        started_waiting.elapsed()
    );

    let status = daemon_process.child.wait().expect("failed to reap the daemon");
    assert!(status.success(), "an idle shutdown is a clean exit, not a failure: {status}");

    wait_for("the plugin to go with its core", || !daemon::is_process_alive(plugin_pid));
    assert!(
        !daemon::is_listening(project.root()).unwrap(),
        "an exited core must leave nothing listening on its endpoint"
    );
    // The socket file is in this list only on Unix: on Windows the endpoint is
    // a pipe name the kernel reclaims with the process, so "released" is what
    // the `is_listening` assertion above already proves and there is no file
    // left to check (see `g_mesh::ipc::windows`).
    // `mut` is only used by the `cfg(unix)` push below; on Windows the list is
    // final as built.
    #[allow(unused_mut)]
    let mut leftovers = vec![
        daemon::pid_path(project.root()).unwrap(),
        daemon::plugin_pid_path(project.root()).unwrap(),
        daemon::build_stamp_path(project.root()).unwrap(),
    ];
    #[cfg(unix)]
    leftovers.push(daemon::socket_path(project.root()).unwrap());
    for leftover in leftovers {
        assert!(
            !leftover.exists(),
            "a core that exited on purpose must release {} - anything left behind describes a \
             daemon that no longer exists",
            leftover.display()
        );
    }

    // The index itself survives, which is what makes the next start an
    // ordinary bootstrap rather than a re-index.
    assert!(project.db_path().exists(), "an idle shutdown must not throw the index away");
    assert!(project.index_contains("alpha"));
}
