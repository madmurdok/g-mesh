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
//! # What this observes
//!
//! Until GM-286 this could only prove the *wiring* - that the notification
//! was accepted and the plugin kept answering - because `extract` was a
//! File-only stub that never read its `ProjectContext`. Now the reload has a
//! visible consequence on the wire, and that is what is asserted: a `Cargo.toml`
//! edit that drops `crates/beta` from the workspace changes the container
//! every declaration in `crates/beta/src/main.rs` belongs to, from the crate
//! `beta_crate` to the synthetic `orphan:` key a file no crate's module tree
//! reaches gets (`project`'s Decision 5).
//!
//! That is a stronger claim than "it did not crash": a plugin that
//! acknowledged the notification and kept its stale model would answer the
//! second `fileChanged` exactly as it answered the first, and pass the old
//! version of this test.

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

/// The containers every declaration of `file` is attributed to, in the
/// plugin's answer to one `fileChanged`. The `File` node itself carries none
/// by design (it is not a container member), so it drops out here.
fn containers_of(response: &serde_json::Value, file: &str) -> Vec<String> {
    let upserts = response["result"]["upsertNodes"]
        .as_array()
        .unwrap_or_else(|| panic!("no upsertNodes in the response to {file}: {response}"));
    let mut containers: Vec<String> =
        upserts.iter().filter_map(|node| node["container"].as_str().map(str::to_string)).collect();
    containers.sort();
    containers.dedup();
    containers
}

#[test]
fn workspace_changed_reloads_the_project_model_the_next_answer_is_built_from() {
    let scratch = Scratch::create("basic");
    let mut client = Client::spawn(&scratch.0);

    // Before: `crates/beta` is a workspace member, so its `main.rs` is the
    // root of the crate `beta-crate` (normalized to `beta_crate`).
    let before = client.request("fileChanged", serde_json::json!({ "filePath": "crates/beta/src/main.rs" }));
    assert_eq!(containers_of(&before, "crates/beta/src/main.rs"), vec!["beta_crate".to_string()], "{before}");

    // The same edit `project::tests::reloading_after_a_cargo_toml_edit_picks_up_the_new_membership`
    // makes directly against `ProjectContext::load` - here made against the
    // live process's own working copy.
    std::fs::write(scratch.0.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/alpha\"]\n").unwrap();
    client.notify("workspaceChanged", serde_json::json!({ "filePath": "Cargo.toml" }));

    // After: no crate reaches that file any more, so it is an orphan - the
    // observable consequence of the reload, and one a plugin that merely
    // acknowledged the notification could not produce.
    let after = client.request("fileChanged", serde_json::json!({ "filePath": "crates/beta/src/main.rs" }));
    assert_eq!(
        containers_of(&after, "crates/beta/src/main.rs"),
        vec!["orphan:crates/beta/src/main.rs".to_string()],
        "{after}"
    );

    // ...and the control loop is still answering about other files, rather
    // than having wedged on the notification.
    let alpha = client.request("fileChanged", serde_json::json!({ "filePath": "crates/alpha/src/lib.rs" }));
    assert!(containers_of(&alpha, "crates/alpha/src/lib.rs").contains(&"alpha".to_string()), "{alpha}");

    client.shutdown();
}
