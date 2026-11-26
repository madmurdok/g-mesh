//! `core/scripts/fetch-embedding-model.sh` streams each file into a partial
//! named for its own process (`<file>.<pid>.partial`) and removes it however
//! the run ends. A `curl` stub on `PATH` stands in for every host: it writes
//! a few bytes to the `--output` path, records that path, then fails or
//! stops the script with SIGTERM. No test reaches the network.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/fetch-embedding-model.sh")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A `curl` that writes to its `--output`, appends that path to
/// `$CURL_STUB_LOG`, then exits 22 (`fail`) or sends SIGTERM to the script
/// and exits 0 (`term`).
const CURL_STUB: &str = r#"#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--output" ]; then out="$2"; shift; fi
  shift
done
printf 'half a model' > "$out"
echo "$out" >> "$CURL_STUB_LOG"
case "$CURL_STUB_MODE" in
  fail) exit 22 ;;
  term) kill -TERM "$PPID"; exit 0 ;;
esac
exit 99
"#;

/// The script run into `<root>/models` with the stub in `mode`; returns its
/// output, its pid, and the paths the stub wrote.
fn run(root: &Path, mode: &str, base_url: Option<&str>) -> (Output, u32, Vec<String>) {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("curl"), CURL_STUB).unwrap();
    fs::set_permissions(bin.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    let log = root.join("curl.log");
    let mut command = Command::new("bash");
    command
        .arg(script())
        .arg(root.join("models"))
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .env("HOME", root)
        .env("CURL_STUB_MODE", mode)
        .env("CURL_STUB_LOG", &log)
        .env_remove("G_MESH_MODEL_DIR")
        .env_remove("G_MESH_MODEL_BASE_URL");
    if let Some(base_url) = base_url {
        command.env("G_MESH_MODEL_BASE_URL", base_url);
    }
    let child =
        command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    let written = fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect();
    (output, pid, written)
}

fn partials_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter(|name| name.ends_with(".partial"))
        .collect()
}

/// Every source failing: each attempt wrote `model.onnx.<script pid>.partial`,
/// and none is left when the script gives up.
///
/// *Control:* drop the `rm -f "${PARTIAL}"` after a failed attempt -> the
/// last attempt's partial is left (the `PARTIAL=""` after it disarms the
/// trap).
#[test]
fn a_failed_download_leaves_no_partial() {
    let root = tempfile::tempdir().unwrap();
    let (output, pid, written) = run(root.path(), "fail", Some("http://127.0.0.1:9"));

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("could not download model.onnx"), "{}", describe(&output));
    let expected = root.path().join("models").join(format!("model.onnx.{pid}.partial"));
    assert_eq!(
        written,
        vec![expected.display().to_string(); 2],
        "both sources were tried into the per-process partial"
    );
    assert_eq!(partials_in(&root.path().join("models")), Vec::<String>::new(), "{}", describe(&output));
    assert!(!root.path().join("models/model.onnx").exists());
}

/// The script stopped by SIGTERM mid-download: the trap exits 143 and the
/// per-process partial is removed on the way out.
///
/// *Control:* drop `trap cleanup EXIT` -> the partial is left. Naming the
/// partial `${dest}.partial` again fails the path assertion.
#[test]
fn a_terminated_download_leaves_no_partial() {
    let root = tempfile::tempdir().unwrap();
    let (output, pid, written) = run(root.path(), "term", None);

    assert_eq!(output.status.code(), Some(143), "{}", describe(&output));
    let expected = root.path().join("models").join(format!("model.onnx.{pid}.partial"));
    assert_eq!(written, vec![expected.display().to_string()], "one attempt, into the per-process partial");
    assert_eq!(partials_in(&root.path().join("models")), Vec::<String>::new(), "{}", describe(&output));
    assert!(!root.path().join("models/model.onnx").exists(), "a stopped download is not moved into place");
}
