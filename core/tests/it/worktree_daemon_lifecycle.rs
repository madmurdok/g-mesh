//! What happens to a daemon, the processes it started and its state
//! when its project directory - typically an agent's git worktree - goes
//! away, and what a session sees when its daemon dies
//! (docs/design/GM-524-worktree-daemon-lifecycle.md, "Behaviours the tests
//! must pin" 3-6, plus the shim's startup sweep of orphaned state, D1).
//!
//! Everything runs against real, detached daemons bootstrapped through
//! `g-mesh mcp-shim`, because the claims are about process groups: a daemon a
//! shim starts leads its own group (`process::detach`), and only such a daemon
//! ends its group on exit or is ended as a group by `g-mesh stop`.
//!
//! # The process in the daemon's group
//!
//! The production case is rust-analyzer's `cargo` writing `<root>/target`
//! after the worktree was removed. Here it is a `sh` loop this test starts
//! and places in the daemon's process group (`setpgid` to the daemon's pid,
//! allowed because the detached daemon stays in this session). What the code
//! under test does - list and signal the members of the group - does not
//! depend on which process forked the member, and this keeps the member's exit
//! status in the test's hands: a reaper thread waits on it, so it never lingers
//! as a zombie that a group probe would still count.
//!
//! The loop only writes into the root while a gate file (outside the root)
//! exists, and acknowledges a closed gate, so deleting the root and the
//! daemon's orphan check never race a `mkdir -p` that would recreate it.
#![cfg(unix)]

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::identity::{project_hash, record_project_root};
use g_mesh::daemon::lifecycle::{CORE_IDLE_ENV, PLUGIN_IDLE_ENV};
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;

use crate::common;

use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Makes the supervisor's tick 500 ms (a quarter of the shorter timeout), as
/// `orphaned_daemon.rs` does, so an orphan is noticed within a second.
const PLUGIN_IDLE_MS: &str = "2000";

/// Far beyond any run here: a daemon that exits did so for the reason under
/// test, not its idle timeout.
const CORE_IDLE_MS: &str = "600000";

/// Three of the 500 ms ticks: how long a deleted root is watched for coming
/// back after its daemon and group are gone.
const THREE_TICKS: Duration = Duration::from_millis(1_500);

/// How long the group member gets to be gone once the daemon is. The code
/// signals the group before the daemon exits (`end_process_group`, 1 s grace
/// then `SIGKILL`); this is a hang guard on top of that, generous for a
/// loaded machine.
const MEMBER_EXIT: Duration = Duration::from_secs(60);

fn short_tick_env(cmd: &mut Command) -> &mut Command {
    cmd.env(PLUGIN_IDLE_ENV, PLUGIN_IDLE_MS)
        .env(CORE_IDLE_ENV, CORE_IDLE_MS)
        // No real model: keeps the embedding backfill pass a no-op.
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
}

/// A project whose root can be deleted, with every path that resolves
/// through the root captured first (canonicalizing a deleted root fails).
struct Project {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    state_dir: PathBuf,
    /// Outside the root: the group member's gate and acknowledgement files.
    aux: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        let root = dir.path().canonicalize().expect("the temp root exists");
        fs::write(root.join("a.ts"), "export const a = 1;\n").expect("failed to seed the project");
        let state_dir = project_dir(&root).expect("failed to resolve the state directory");
        Self { dir: Some(dir), root, state_dir, aux: tempfile::tempdir().unwrap() }
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path_in(&self.state_dir)
    }

    /// Bootstraps a detached daemon through a shim, then lets the shim go:
    /// the daemon outlives it, reparented to init, as in real use.
    fn bootstrap_detached(&self) -> u32 {
        let mut shim = short_tick_env(Command::new(BIN).lifeline())
            .arg("mcp-shim")
            .current_dir(&self.root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");
        common::wait_for("the daemon to record its pid", common::startup_timeout(), || {
            daemon::read_pid_file(&self.pid_file()).is_some()
        });
        let _ = shim.kill();
        let _ = shim.wait();
        let core = daemon::read_pid_file(&self.pid_file()).unwrap();
        assert!(daemon::is_process_alive(core), "the daemon must be running");
        assert_eq!(
            g_mesh::process::own_group_of(core),
            Some(core),
            "a daemon a shim started leads its own process group"
        );
        core
    }

    fn delete_root(&mut self) {
        self.dir.take().expect("deleted once").close().expect("failed to delete the project root");
        assert!(!self.root.exists());
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for (_, path) in daemon::registry::discovered_pid_files(&self.state_dir) {
            common::kill_pid_file(&path);
        }
        common::kill_pid_file(&self.pid_file());
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

/// A `sh` loop in the daemon's process group that keeps writing
/// `<root>/target/x` while its gate is open (see the module docs).
struct GroupMember {
    pid: u32,
    exit: mpsc::Receiver<ExitStatus>,
    exited: Option<ExitStatus>,
    gate: PathBuf,
    ack: PathBuf,
}

impl GroupMember {
    fn spawn(pgid: u32, project: &Project) -> Self {
        let gate = project.aux.path().join("gate");
        let ack = project.aux.path().join("ack");
        fs::write(&gate, "").unwrap();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(
                r#"while :; do
                     if [ -e "$GATE" ]; then mkdir -p "$ROOT/target" && touch "$ROOT/target/x"
                     else touch "$ACK"; fi
                     sleep 0.1
                   done"#,
            )
            .env("ROOT", &project.root)
            .env("GATE", &gate)
            .env("ACK", &ack)
            .process_group(pgid as i32)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the group member");
        let pid = child.id();
        let (tx, exit) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(status) = child.wait() {
                let _ = tx.send(status);
            }
        });
        let member = Self { pid, exit, exited: None, gate, ack };
        let x = project.root.join("target/x");
        common::wait_for("the group member to write target/x", common::startup_timeout(), || x.exists());
        member
    }

    /// Stops the writes into the root, and waits until the loop has seen it.
    fn close_gate(&self) {
        fs::remove_file(&self.gate).unwrap();
        common::wait_for("the group member to see its gate closed", common::startup_timeout(), || {
            self.ack.exists()
        });
    }

    fn open_gate(&self) {
        fs::write(&self.gate, "").unwrap();
    }

    /// The member's exit status, waiting up to [`MEMBER_EXIT`].
    fn wait_exit(&mut self, what: &str) -> ExitStatus {
        if let Some(status) = self.exited {
            return status;
        }
        let status = self
            .exit
            .recv_timeout(MEMBER_EXIT)
            .unwrap_or_else(|_| panic!("{what}: the process in the daemon's group is still running"));
        self.exited = Some(status);
        status
    }
}

impl Drop for GroupMember {
    fn drop(&mut self) {
        if self.exited.is_none() && self.exit.try_recv().is_err() {
            // Still ours and unreaped (the reaper thread has not reported).
            force_kill(self.pid);
        }
    }
}

fn force_kill(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).stderr(Stdio::null()).status();
}

/// Behaviour 3: a daemon whose root is deleted ends its process group on the
/// way out, so a process it (or its plugin's language server) started stops
/// writing into the deleted root, and the root stays gone.
///
/// The daemon must also finish its own way out: the sweep runs before the
/// pid file goes, so a released pid file says the sweep returned instead of
/// ending the daemon. That holds whatever order the sweep visits the group
/// in, unlike the member's fate: a sweep that signals one of the daemon's
/// own threads kills the daemon at that point, and the member survives only
/// when it happens to come later in the listing.
///
/// Controls: drop the `end_process_group()` call on the orphan exit in
/// `lifecycle::supervise`: the member outlives the daemon. Let
/// `process::group_members` list threads: the pid file stays behind.
#[test]
fn an_orphaned_daemon_ends_its_process_group_and_the_root_stays_gone() {
    let mut project = Project::new();
    let core = project.bootstrap_detached();
    let mut member = GroupMember::spawn(core, &project);
    member.close_gate();

    project.delete_root();
    common::wait_for("the orphaned daemon to stop itself", common::startup_timeout(), || {
        !daemon::is_process_alive(core)
    });
    assert!(
        !project.pid_file().exists(),
        "the orphaned daemon must release its pid file on the way out, after ending its group; \
         a pid file left behind means the daemon was killed mid-exit"
    );

    let status = member.wait_exit("after its orphaned daemon exited");
    assert!(status.signal().is_some(), "the member must have been ended by a signal: {status}");

    member.open_gate();
    std::thread::sleep(THREE_TICKS);
    assert!(!project.root.exists(), "nothing may recreate the deleted root");
}

/// Behaviour 4: `g-mesh stop` ends the daemon's whole group, not only the
/// daemon.
///
/// Control: make `stop::terminate_daemon` signal the pid only (call
/// `terminate(pid, grace)` unconditionally): the member stays alive.
#[test]
fn stop_ends_every_process_in_the_daemons_group() {
    let project = Project::new();
    let core = project.bootstrap_detached();
    let mut member = GroupMember::spawn(core, &project);

    let output = Command::new(BIN)
        .arg("stop")
        .current_dir(&project.root)
        .output()
        .expect("failed to run `g-mesh stop`");
    assert!(output.status.success(), "`g-mesh stop` failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(!daemon::is_process_alive(core), "stop returns once the daemon is gone");

    let status = member.wait_exit("after `g-mesh stop`");
    assert!(status.signal().is_some(), "the member must have been ended by a signal: {status}");
}

// -------------------------------------------------------------------------
// A session's daemon dying (behaviours 5 and 6)
// -------------------------------------------------------------------------

async fn shim_in(dir: &Path, env: &[(&str, &str)]) -> RunningService<RoleClient, ()> {
    let dir = dir.to_path_buf();
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&dir)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        for (key, value) in &env {
            cmd.env(key, value);
        }
    }))
    .expect("failed to spawn the shim");
    ().serve(transport).await.expect("MCP initialization failed")
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let arguments = args.as_object().cloned().expect("arguments must be an object");
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .unwrap_or_else(|err| panic!("{name} must return a result - the session must stay up: {err}"))
}

fn texts_of(result: &CallToolResult) -> Vec<String> {
    result.content.iter().filter_map(|block| block.as_text()).map(|text| text.text.clone()).collect()
}

/// Calls `name` until the answer is not the shim's "this call was not
/// answered": a call that reaches the shim before it has read the end of the
/// dead daemon's connection is sent there and answered that way, which the
/// code allows; the next call is the one that reconnects.
async fn call_after_loss(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let deadline = Instant::now() + common::startup_timeout();
    loop {
        let result = call(client, name, args.clone()).await;
        let text = texts_of(&result).concat();
        if !text.contains("this call was not answered") {
            return result;
        }
        assert!(Instant::now() < deadline, "every call after the loss was unanswered: {text}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Kills every daemon and plugin recorded for `dirs`, and their state.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for state in &self.0 {
            for (_, path) in daemon::registry::discovered_pid_files(state) {
                common::kill_pid_file(&path);
            }
            common::kill_pid_file(&daemon::pid_path_in(state));
            let _ = fs::remove_dir_all(state);
        }
    }
}

/// Behaviour 5: `kill -9` of the daemon serving a session leaves the session
/// up; the next call bootstraps a new daemon and its answer says the daemon
/// restarted.
///
/// Control: in `router::Shared::upstream_ended`, send `Event::Done` whenever
/// the live upstream ends: the shim exits and
/// the call after the kill gets no result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_daemon_is_restarted_and_the_next_answer_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("a.ts"), "export const a = 1;\n").unwrap();
    let state = project_dir(&root).unwrap();
    let _cleanup = Cleanup(vec![state.clone()]);
    let pid_file = daemon::pid_path_in(&state);

    let client = shim_in(&root, &[]).await;
    call(&client, "get_file_outline", json!({ "file_path": "a.ts" })).await;
    let first = daemon::read_pid_file(&pid_file).expect("the session's daemon records its pid");

    tokio::task::block_in_place(|| common::kill_and_wait(first));
    assert!(!daemon::is_process_alive(first), "the daemon must be gone");

    let after = call_after_loss(&client, "get_file_outline", json!({ "file_path": "a.ts" })).await;
    let texts = texts_of(&after);
    let restart = texts.iter().find(|line| line.contains("restarted since this session's previous answer"));
    let restart =
        restart.unwrap_or_else(|| panic!("the first answer after the loss names the restart: {texts:?}"));
    assert!(restart.contains("or the daemon exited"), "{restart}");

    let second = daemon::read_pid_file(&pid_file).expect("the new daemon records its pid");
    assert_ne!(second, first, "a new daemon serves the session");
    assert!(daemon::is_process_alive(second), "and it is running");

    let next = call(&client, "get_file_outline", json!({ "file_path": "a.ts" })).await;
    assert!(
        texts_of(&next).iter().all(|line| !line.contains("restarted since")),
        "only the first answer after the restart is marked: {:?}",
        texts_of(&next)
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// Behaviour 6: the selected project's directory is deleted. Its daemon stops
/// itself; the next call to it is answered with the "no longer exists" text,
/// the session stays up (another project can be selected and answers), and
/// nothing recreates the directory.
///
/// Control: restore `router::unreachable_text`'s generic "could not reach"
/// text for a gone root (drop the `root_is_gone` arm): the text assertion
/// fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_to_a_deleted_selection_says_it_no_longer_exists() {
    let folder = tempfile::tempdir().unwrap();
    let folder_root = folder.path().canonicalize().unwrap();
    for (path, contents) in [("a/a.ts", "export const a = 1;\n"), ("b/b.ts", "export const b = 2;\n")] {
        let full = folder_root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }
    fs::create_dir_all(folder_root.join("a/.git")).unwrap();
    fs::create_dir_all(folder_root.join("b/.git")).unwrap();
    let (a, b) = (folder_root.join("a"), folder_root.join("b"));
    let states: Vec<PathBuf> = [&folder_root, &a, &b].iter().map(|dir| project_dir(dir).unwrap()).collect();
    let b_state = states[2].clone();
    let _cleanup = Cleanup(states);

    let client =
        shim_in(&folder_root, &[(PLUGIN_IDLE_ENV, PLUGIN_IDLE_MS), (CORE_IDLE_ENV, CORE_IDLE_MS)]).await;
    select(&client, "b").await;
    let outline = call(&client, "get_file_outline", json!({ "file_path": "b.ts" })).await;
    assert_ne!(outline.is_error, Some(true), "{:?}", texts_of(&outline));
    let b_daemon = daemon::read_pid_file(&daemon::pid_path_in(&b_state)).expect("b's daemon records its pid");

    fs::remove_dir_all(&b).unwrap();
    tokio::task::block_in_place(|| {
        common::wait_for("b's daemon to stop itself", common::startup_timeout(), || {
            !daemon::is_process_alive(b_daemon)
        })
    });

    let gone = call_after_loss(&client, "get_file_outline", json!({ "file_path": "b.ts" })).await;
    let text = texts_of(&gone).concat();
    assert_eq!(gone.is_error, Some(true), "{text}");
    assert!(text.contains(&format!("the project directory {} no longer exists", b.display())), "{text}");
    assert!(text.contains("call select_project"), "{text}");
    assert!(!b.exists(), "answering for a deleted selection must not recreate it");

    // b, then a: the connection is contested (D3 of GM-571), so the call
    // names its project, which must resolve although only one project is
    // left in the folder.
    select(&client, "a").await;
    let outline = call(&client, "get_file_outline", json!({ "file_path": "a.ts", "project": "a" })).await;
    assert_ne!(outline.is_error, Some(true), "the session serves another project: {:?}", texts_of(&outline));
    assert!(
        texts_of(&outline).first().is_some_and(|line| line == "g-mesh: answered from project a."),
        "{:?}",
        texts_of(&outline)
    );
    assert!(!b.exists(), "nor may anything later");

    client.cancel().await.expect("failed to shut the client down");
}

async fn select(client: &RunningService<RoleClient, ()>, project: &str) {
    let result = call(client, "select_project", json!({ "project": project })).await;
    assert_ne!(result.is_error, Some(true), "selecting {project} must succeed: {:?}", texts_of(&result));
}

// -------------------------------------------------------------------------
// The shim's startup sweep (D1, option (c))
// -------------------------------------------------------------------------

/// A `G_MESH_HOME` of this test's own: the sweep deletes across a whole
/// home, so it must never run over the one the rest of the suite shares.
/// Short and under `/tmp` for the socket path limit (see
/// `cli_clean_sweeping.rs`).
struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from("/tmp").join(format!(
            "gm524-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn state_dir(&self, root: &Path) -> PathBuf {
        self.0.join("projects").join(project_hash(root).unwrap())
    }

    /// A state directory recording `root`, with a stand-in index.
    fn add_state(&self, root: &Path) -> PathBuf {
        let state = self.state_dir(root);
        fs::create_dir_all(&state).unwrap();
        record_project_root(&state, root).unwrap();
        fs::write(state.join("index.db"), "").unwrap();
        state
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(self.0.join("projects")) {
            for entry in entries.flatten() {
                for (_, path) in daemon::registry::discovered_pid_files(&entry.path()) {
                    common::kill_pid_file(&path);
                }
                common::kill_pid_file(&daemon::pid_path_in(&entry.path()));
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A shim's start sweeps the home: the state of a project whose directory was
/// deleted (its parent still there) goes; the state of a live project stays.
/// The shim's own report line is the completion signal: it is printed once
/// the sweep has judged every directory, and names how many it deleted.
///
/// Control: drop the `spawn_orphan_sweep()` call in `shim::run`: no line, and
/// the orphan's state stays.
#[test]
fn a_shim_start_sweeps_the_state_of_deleted_projects() {
    let home = Home::new();
    let parent = tempfile::tempdir().unwrap();
    let gone = parent.path().canonicalize().unwrap().join("gone");
    fs::create_dir(&gone).unwrap();
    let orphan = home.add_state(&gone);
    fs::remove_dir(&gone).unwrap();
    let live_root = tempfile::tempdir().unwrap();
    let live = home.add_state(live_root.path());

    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("a.ts"), "export const a = 1;\n").unwrap();
    let aux = tempfile::tempdir().unwrap();
    let stderr_path = aux.path().join("shim.stderr");
    let mut shim = short_tick_env(Command::new(BIN).lifeline())
        .arg("mcp-shim")
        .current_dir(project.path())
        .env(g_mesh::paths::HOME_ENV, &home.0)
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("failed to spawn the shim");

    let report = "deleted the index and state of 1 project(s) whose directory no longer exists";
    let stderr = || fs::read_to_string(&stderr_path).unwrap_or_default();
    let deadline = Instant::now() + common::startup_timeout();
    while !stderr().contains(report) {
        assert!(
            Instant::now() < deadline,
            "the sweep never reported deleting one project; shim stderr:\n{}",
            stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = shim.kill();
    let _ = shim.wait();

    assert!(!orphan.exists(), "the deleted project's state must be gone");
    assert!(live.join("index.db").exists(), "a live project's state must stay");
    assert!(home.0.join("orphan-sweep.stamp").exists(), "the sweep stamps its marker");
}
