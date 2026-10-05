//! The behaviours of `g-mesh-fake-plugin` that core's own tests rely on but
//! cannot observe: core waits on them, so a fake that dropped one would leave
//! those tests passing for the wrong reason. Each test drives the binary
//! directly over its stdin/stdout.

use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh_plugin_sdk::framing::{read_frame, write_message};
use serde_json::{json, Value};

const FAKE: &str = env!("CARGO_BIN_EXE_g-mesh-fake-plugin");

/// How long anything the fake is expected to do may take, on a loaded machine.
const PATIENCE: Duration = Duration::from_secs(20);

/// How long the fake is given to do something it must not do. A wrong fake
/// does it in milliseconds; a right one never does. The window only means
/// that once the fake is demonstrably running - see [`Fake::assert_quiet`].
const QUIET: Duration = Duration::from_millis(500);

/// A fresh fixture directory under cargo's per-target scratch space, unique to
/// this test and this run.
fn fixture_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fake-plugin-{name}-{}-{}", std::process::id(), unique()))
        .join(name);
    fs::create_dir_all(&dir).expect("failed to create the fixture directory");
    dir
}

fn unique() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

fn write_options(dir: &Path, options: Value) {
    fs::write(dir.join("fake-plugin.json"), options.to_string()).expect("failed to write fake-plugin.json");
}

struct Fake {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Value>,
}

impl Fake {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(FAKE)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {FAKE}: {err}"));
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some(body)) = read_frame(&mut stdout) {
                let value: Value = serde_json::from_slice(&body).expect("the fake wrote a non-JSON frame");
                if tx.send(value).is_err() {
                    return;
                }
            }
        });
        Self { child, stdin, frames }
    }

    fn fixture(language: &str, dir: &Path) -> Self {
        Self::spawn(&["--language", language, "--dir", dir.to_str().unwrap()])
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        write_message(stdin, &message).expect("failed to write to the fake");
        stdin.flush().unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin.write_all(bytes).expect("failed to write to the fake");
        stdin.flush().unwrap();
    }

    fn next_frame(&self, what: &str) -> Value {
        self.frames.recv_timeout(PATIENCE).unwrap_or_else(|_| panic!("no frame within {PATIENCE:?}: {what}"))
    }

    /// Blocks until the fixture persona has logged its own pid to
    /// `spawns.log`, the first thing it does - before its options, before any
    /// gate. Only from there is [`Self::assert_quiet`]'s window a statement
    /// about the fake: macOS's first exec of a freshly linked binary can take
    /// longer than [`QUIET`] on its own, so a window opened at spawn could
    /// close before an ungated fake had even started (GM-351 verify, F3).
    fn wait_until_started(&self, dir: &Path) {
        let pid = self.child.id().to_string();
        let log = dir.join("spawns.log");
        let deadline = Instant::now() + PATIENCE;
        while !fs::read_to_string(&log).is_ok_and(|text| text.lines().any(|line| line == pid)) {
            assert!(
                Instant::now() < deadline,
                "the fake did not log pid {pid} to {} within {PATIENCE:?}",
                log.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Asserts no frame arrives within [`QUIET`]. The caller must first show
    /// the process is up - [`Self::wait_until_started`], or a frame it already
    /// received - or a slow exec alone can make this pass.
    fn assert_quiet(&self, what: &str) {
        if let Ok(frame) = self.frames.recv_timeout(QUIET) {
            panic!("{what}, but the fake wrote {frame}");
        }
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait_for_exit(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait failed") {
                return status;
            }
            assert!(Instant::now() < deadline, "the fake did not exit within {PATIENCE:?}: {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request(id: u64, method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": { "filePath": "src/a.src" } })
}

#[test]
fn a_gated_fixture_withholds_its_handshake_until_the_gate_file_exists() {
    let dir = fixture_dir("gated");
    write_options(&dir, json!({ "gated": true }));
    let fake = Fake::fixture("gated", &dir);

    fake.wait_until_started(&dir);
    fake.assert_quiet("the handshake must wait for handshake.allow");
    fs::write(dir.join("handshake.allow"), "").unwrap();
    let handshake = fake.next_frame("the handshake once the gate is open");
    assert_eq!(handshake["language"], "gated");
}

/// Core closing stdin on a plugin whose handshake it is still waiting for
/// must end that plugin, gate or no gate.
#[test]
fn a_gated_fixture_still_exits_0_when_stdin_closes_before_its_gate_opens() {
    let dir = fixture_dir("gated");
    write_options(&dir, json!({ "gated": true }));
    let mut fake = Fake::fixture("gated", &dir);

    fake.close_stdin();
    let status = fake.wait_for_exit("stdin closed while gated");
    assert_eq!(status.code(), Some(0));
    assert!(!dir.join("handshake.allow").exists());
}

#[test]
fn a_held_semantic_pass_does_not_hold_up_the_frames_after_it() {
    let dir = fixture_dir("held");
    fs::write(dir.join("semantic-pass.gated"), "").unwrap();
    let mut fake = Fake::fixture("held", &dir);
    fake.next_frame("the handshake");

    fake.send(request(1, "semanticPass"));
    fake.send(request(2, "fileChanged"));
    let first = fake.next_frame("the fileChanged answer while the pass is held");
    assert_eq!(first["id"], 2, "the held pass must not be answered first: {first}");
    fake.assert_quiet("the pass must stay held while semantic-pass.allow is absent");

    fs::write(dir.join("semantic-pass.allow"), "").unwrap();
    let released = fake.next_frame("the pass answer once its gate opens");
    assert_eq!(released["id"], 1);
}

#[test]
fn a_fixture_told_to_exit_without_a_handshake_says_nothing_and_exits_1() {
    let dir = fixture_dir("broken");
    write_options(&dir, json!({ "exitWithoutHandshakeAfterMs": 50 }));
    let mut fake = Fake::fixture("broken", &dir);

    let status = fake.wait_for_exit("exitWithoutHandshakeAfterMs");
    assert_eq!(status.code(), Some(1));
    // The process is gone, so its stdout reaches EOF and the reader hangs up:
    // waiting for that disconnect drains everything it wrote, with no window.
    match fake.frames.recv_timeout(PATIENCE) {
        Err(RecvTimeoutError::Disconnected) => {}
        Ok(frame) => panic!("nothing may be written, not even a handshake, but the fake wrote {frame}"),
        Err(RecvTimeoutError::Timeout) => {
            panic!("the fake's stdout did not close within {PATIENCE:?} of its exit")
        }
    }
}

#[test]
fn a_fixture_exits_0_when_stdin_closes() {
    let dir = fixture_dir("eof");
    let mut fake = Fake::fixture("eof", &dir);
    fake.next_frame("the handshake");

    fake.close_stdin();
    assert_eq!(fake.wait_for_exit("stdin closed").code(), Some(0));
}

#[test]
fn a_fixture_exits_1_on_a_frame_that_is_not_json() {
    let dir = fixture_dir("garbage");
    let mut fake = Fake::fixture("garbage", &dir);
    fake.next_frame("the handshake");

    fake.send_raw(b"Content-Length: 9\r\n\r\nnot json!");
    assert_eq!(fake.wait_for_exit("a non-JSON body").code(), Some(1));
}

/// Resident set size in kilobytes, from `ps`.
#[cfg(unix)]
fn rss_kb(pid: u32) -> u64 {
    let output =
        Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().expect("failed to run ps");
    String::from_utf8_lossy(&output.stdout).trim().parse().expect("ps printed no rss")
}

/// The memory-limit tests in core sample a fixed number through a seam, so
/// only this test sees the buffer itself. 50MB sits far from both sides: the
/// fake idles at a few MB and the buffer is 200MB.
#[cfg(unix)]
#[test]
fn a_memory_hungry_fixture_holds_a_large_buffer_and_a_plain_one_does_not() {
    const THRESHOLD_KB: u64 = 50 * 1024;

    let hungry_dir = fixture_dir("hungry");
    write_options(&hungry_dir, json!({ "memoryHungry": true }));
    let hungry = Fake::fixture("hungry", &hungry_dir);
    hungry.next_frame("the hungry fixture's handshake");

    let plain_dir = fixture_dir("plain");
    let plain = Fake::fixture("plain", &plain_dir);
    plain.next_frame("the plain fixture's handshake");

    let hungry_rss = rss_kb(hungry.child.id());
    let plain_rss = rss_kb(plain.child.id());
    assert!(hungry_rss > THRESHOLD_KB, "memoryHungry RSS {hungry_rss}KB, expected over {THRESHOLD_KB}KB");
    assert!(plain_rss < THRESHOLD_KB, "plain RSS {plain_rss}KB, expected under {THRESHOLD_KB}KB");
}

#[test]
fn the_stub_reports_the_plugin_version_it_was_given() {
    let fake = Fake::spawn(&["--language", "typescript", "--plugin-version", "0.1.0"]);
    let handshake = fake.next_frame("the stub's handshake");
    assert_eq!(handshake["language"], "typescript");
    assert_eq!(handshake["pluginVersion"], "0.1.0");
}

#[test]
fn the_stub_answers_a_diff_for_file_changes_and_passes_and_an_acknowledgement_otherwise() {
    let mut fake = Fake::spawn(&["--language", "typescript"]);
    fake.next_frame("the stub's handshake");
    let empty_diff =
        json!({ "upsertNodes": [], "deleteNodeIds": [], "upsertEdges": [], "deleteEdgeIds": [] });

    fake.send(request(1, "fileChanged"));
    assert_eq!(fake.next_frame("fileChanged")["result"], empty_diff);
    fake.send(request(2, "semanticPass"));
    assert_eq!(fake.next_frame("semanticPass")["result"], empty_diff);
    fake.send(request(3, "somethingElse"));
    assert_eq!(fake.next_frame("another request")["result"], json!({ "acknowledged": true }));
}

/// The stub treats only an absent id as a notification; the fixture persona
/// treats a null one as a notification too.
#[test]
fn the_stub_answers_a_null_id_and_ignores_an_absent_one() {
    let mut fake = Fake::spawn(&["--language", "typescript"]);
    fake.next_frame("the stub's handshake");

    fake.send(json!({ "jsonrpc": "2.0", "method": "fileChanged", "params": {} }));
    fake.send(json!({ "jsonrpc": "2.0", "id": null, "method": "fileChanged", "params": {} }));
    let answer = fake.next_frame("the null-id request's answer");
    assert_eq!(answer["id"], Value::Null, "{answer}");
    fake.assert_quiet("the notification with no id must not be answered");
}
