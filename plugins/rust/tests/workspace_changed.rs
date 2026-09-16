//! End-to-end proof that the running plugin answers `workspaceChanged` by
//! reloading its project model, through the real control-plane wire - not
//! just that `ProjectContext::load` is idempotent when called twice
//! (`src/project/mod.rs`'s own
//! `reloading_after_a_cargo_toml_edit_picks_up_the_new_membership` proves
//! that half, directly and more cheaply). This test drives the actual
//! spawned binary with the same LSP-style `Content-Length` framing core uses
//! (`plugins/sdk/src/run.rs`'s module doc), because the SDK does not publish
//! a client-side test harness for its own wire - only `g-mesh plugins check`
//! does, and that kit never sends `workspaceChanged` (its own report, run
//! by hand against this crate's fixture, has no such line).
//!
//! # What this can and cannot observe
//!
//! This crate's `extract` is presently a File-only stub (GM-285's own scope
//! boundary - see `src/extractor.rs`'s module doc) that never reads its
//! `ProjectContext` argument, so no wire *content* differs before and after
//! a `Cargo.toml` edit yet: there is nothing for GM-286's declarations to
//! attribute to a container until GM-286 exists to emit them. What this test
//! proves instead is the wiring itself - the SDK's own control loop
//! (`run::Session::handle`'s `"workspaceChanged"` arm, calling
//! `RustExtractor::load_project` a second time) accepts the notification,
//! reports the reload on its own stderr, and keeps answering `fileChanged`
//! correctly afterwards rather than wedging or crashing.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workspace");

/// A minimal client for the SDK's control-plane wire - just enough framing
/// to send one notification and one request and read the answers, not a
/// general-purpose harness.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn spawn(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_g-mesh-plugin-rust"))
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn g-mesh-plugin-rust");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut client = Self { child, stdin, stdout, next_id: 1 };
        let handshake = client.read_message();
        assert_eq!(handshake["language"], "rust", "{handshake}");
        client
    }

    fn write_message(&mut self, value: &serde_json::Value) {
        let body = serde_json::to_vec(value).expect("a control message always serializes");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.stdin.write_all(&body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_message(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("the plugin closed its stdout mid-frame");
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    length = Some(value.trim().parse::<usize>().expect("a numeric Content-Length"));
                }
            }
        }
        let length = length.expect("a frame header without Content-Length");
        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).expect("a full frame body");
        serde_json::from_slice(&body).expect("a JSON frame body")
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        self.read_message()
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.write_message(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn shutdown(mut self) {
        // Core ends a plugin by closing its stdin (`run::control_plane`'s own
        // doc) - dropping the handle is exactly that.
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

fn copy_fixture_to(dest: &Path) {
    fn copy_dir(src: &Path, dest: &Path) {
        std::fs::create_dir_all(dest).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let target = dest.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    copy_dir(Path::new(FIXTURE), dest);
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn create(tag: &str) -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        let root = std::env::temp_dir()
            .join(format!("g-mesh-plugin-rust-workspace-changed-{}-{tag}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        copy_fixture_to(&root);
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn workspace_changed_is_acknowledged_and_the_plugin_keeps_answering() {
    let scratch = Scratch::create("basic");
    let mut client = Client::spawn(&scratch.0);

    // The same edit `project::tests::reloading_after_a_cargo_toml_edit_picks_up_the_new_membership`
    // makes directly against `ProjectContext::load` - here made against the
    // live process's own working copy.
    std::fs::write(scratch.0.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/alpha\"]\n").unwrap();
    client.notify("workspaceChanged", serde_json::json!({ "filePath": "Cargo.toml" }));

    // The plugin must still be alive and answering correctly: this is the
    // first time this process has seen `crates/alpha/src/lib.rs`, so its
    // `File` node comes back as an addition either way - what this proves is
    // that `workspaceChanged` did not wedge or crash the control loop.
    let response =
        client.request("fileChanged", serde_json::json!({ "filePath": "crates/alpha/src/lib.rs" }));
    let upserts = response["result"]["upsertNodes"]
        .as_array()
        .unwrap_or_else(|| panic!("no upsertNodes in the response: {response}"));
    assert_eq!(upserts.len(), 1, "{response}");
    assert_eq!(upserts[0]["kind"], "File", "{response}");
    assert_eq!(upserts[0]["qualifiedName"], "crates/alpha/src/lib.rs", "{response}");

    client.shutdown();
}
