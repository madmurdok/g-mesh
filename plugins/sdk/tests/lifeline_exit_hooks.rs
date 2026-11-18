//! The control-stream lifeline runs the registered exit hooks before the
//! plugin exits: core closes stdin while the main loop is stuck in a request,
//! and a hook registered with `on_lifeline_exit` still runs.
//!
//! The plugin under test is this test binary itself, re-run with
//! `CHILD_ENV` set so that `the_plugin_under_test` calls `run` instead of
//! returning. The engine factory never returns, which is what "stuck
//! mid-request" means here; the hook writes a marker file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh_plugin_sdk::{on_lifeline_exit, run, Extractor, FileGraph, FileGraphBuilder, PluginSpec, RelPath};

const CHILD_ENV: &str = "G_MESH_LIFELINE_TEST_CHILD";
const MARKER_ENV: &str = "G_MESH_LIFELINE_TEST_MARKER";
const STARTED_ENV: &str = "G_MESH_LIFELINE_TEST_STARTED";
const PATIENCE: Duration = Duration::from_secs(60);

struct Nothing;

impl Extractor for Nothing {
    const LANGUAGE: &'static str = "toy";
    type Project = ();

    fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn extract(&self, _project: &(), path: &RelPath, _source: &str) -> FileGraph {
        FileGraphBuilder::new("toy", "toy-parser", path).finish()
    }
}

fn write_marker() {
    let marker = std::env::var_os(MARKER_ENV).expect("the marker path is set");
    fs::write(marker, "hook ran\n").expect("failed to write the marker");
}

/// The plugin when `CHILD_ENV` is set; a no-op test otherwise.
#[test]
fn the_plugin_under_test() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    on_lifeline_exit(write_marker);
    run(
        Nothing,
        PluginSpec::new("toy", "0.0.0", &[".toy"]),
        Some(Box::new(|_root| {
            let started = std::env::var_os(STARTED_ENV).expect("the started path is set");
            fs::write(started, "engine factory entered\n").expect("failed to write the started file");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        })),
    );
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} did not happen within {PATIENCE:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn closing_the_control_stream_mid_request_runs_the_exit_hooks_before_exiting() {
    let dir = tempfile::tempdir().expect("failed to create a scratch dir");
    let marker: PathBuf = dir.path().join("marker");
    let started: PathBuf = dir.path().join("started");
    let stderr = fs::File::create(dir.path().join("stderr")).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["the_plugin_under_test", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env(MARKER_ENV, &marker)
        .env(STARTED_ENV, &started)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("failed to re-run this test binary as the plugin");

    let mut stdin = child.stdin.take().unwrap();
    let body = serde_json::json!({ "jsonrpc": "2.0", "method": "prepareSemanticPass" }).to_string();
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stdin.flush().unwrap();
    wait_for(&started, "the engine factory starting");
    assert!(!marker.exists(), "no hook runs while the control stream is open");
    drop(stdin);

    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the plugin did not exit within {PATIENCE:?} of its stdin closing");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let log = fs::read_to_string(dir.path().join("stderr")).unwrap_or_default();
    assert_eq!(status.code(), Some(1), "the lifeline exits 1\nstderr:\n{log}");
    assert!(marker.exists(), "the exit hook did not run before the plugin exited\nstderr:\n{log}");
}
