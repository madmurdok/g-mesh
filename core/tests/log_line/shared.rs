// The body of `core/tests/log_line_atomic.rs` and
// `plugins/sdk/tests/log_line_atomic.rs`, `include!`d by both: core and the
// plugin SDK carry identical copies of `log_line!` (the SDK does not depend on
// core), so one set of tests pins both. The includer brings `log_line!` into
// scope (`use g_mesh::log_line;` / `use g_mesh_plugin_sdk::log_line;`) and is
// `#![cfg(unix)]`: the setups below are POSIX ones (an `O_APPEND` file, an
// `AF_UNIX` datagram socket, a pipe with no reader).
//
// Every test spawns this same test binary again as a child that runs only
// `log_line_child`, which does nothing unless `ROLE_ENV` names a role. The
// child's stderr is what the test sets up; `--nocapture` makes sure nothing
// the harness would capture stands between a write and that stderr.
//
// The design is `docs/architecture/gm-520-line-atomic-logs.md` (behaviour list
// items 1, 3 and 4; stress test T1).

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Which job a child spawned by these tests does.
const ROLE_ENV: &str = "G_MESH_LOG_LINE_TEST_ROLE";
/// A writer child starts writing once this file exists, so all writers
/// overlap instead of the first finishing before the last has started.
const GO_FILE_ENV: &str = "G_MESH_LOG_LINE_TEST_GO";
/// The one test a child runs.
const CHILD_TEST: &str = "log_line_child";

/// T1's shape: the note's N=4 writers of M=5000 lines each.
const WRITERS: usize = 4;
const LINES_PER_WRITER: usize = 5_000;
/// Every this many lines a writer pads its line past 64 KiB, so long lines are
/// in the mix too (behaviour 1's "for one over 64 KiB", under contention).
const LONG_LINE_EVERY: usize = 1_000;
const LONG_PAD: usize = 70 * 1024;

/// The short probe: several interpolated pieces, which `eprintln!` would send
/// as one `write(2)` each.
const PROBE_LINE: &str = "probe: tool=find_callers ms=42 ok=true";
/// The long probe's body length: well past 64 KiB.
const LONG_PROBE_BODY: usize = 100_001;

/// The child side. A plain `cargo test` runs it too, with no role set: then it
/// returns at once.
#[test]
fn log_line_child() {
    let Ok(role) = std::env::var(ROLE_ENV) else { return };
    match role.as_str() {
        "probe" => log_line!("probe: tool={} ms={} ok={}", "find_callers", 42, true),
        "probe-long" => log_line!("probe-long: len={} body={}", LONG_PROBE_BODY, "y".repeat(LONG_PROBE_BODY)),
        "broken" => {
            // stderr is a pipe nobody reads: every write fails with EPIPE.
            log_line!("broken: attempt={} of {}", 1, 2);
            log_line!("broken: attempt={} of {}", 2, 2);
        }
        writer => {
            let id: usize = writer
                .strip_prefix("writer:")
                .and_then(|id| id.parse().ok())
                .unwrap_or_else(|| panic!("unknown {ROLE_ENV} {role:?}"));
            let go = PathBuf::from(std::env::var_os(GO_FILE_ENV).expect("a writer needs its go file"));
            let deadline = Instant::now() + Duration::from_secs(60);
            while !go.exists() {
                assert!(Instant::now() < deadline, "the go file never appeared");
                thread::sleep(Duration::from_millis(1));
            }
            for n in 0..LINES_PER_WRITER {
                log_line!(
                    "[w{}] seq={} a={} b={} c={} d={} pad={} end={}:{}",
                    id,
                    n,
                    n * 3,
                    n * 7,
                    id * n,
                    n % 13,
                    pad(n),
                    id,
                    n
                );
            }
        }
    }
}

fn pad(n: usize) -> String {
    if n % LONG_LINE_EVERY == LONG_LINE_EVERY - 1 {
        "x".repeat(LONG_PAD)
    } else {
        "-".to_string()
    }
}

/// The line writer `id` writes as its `n`th, as the parent expects it.
fn writer_line(id: usize, n: usize) -> String {
    format!("[w{id}] seq={n} a={} b={} c={} d={} pad={} end={id}:{n}", n * 3, n * 7, id * n, n % 13, pad(n))
}

/// This test binary, set up to run only [`CHILD_TEST`] in role `role`.
fn child(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("the test binary's own path"));
    command
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads", "1"])
        .env(ROLE_ENV, role)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    command
}

/// Fails unless the child's test actually ran and passed: a filter that
/// matched nothing would "pass" with nothing written.
fn assert_child_passed(role: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the {role:?} child failed or did not run ({}):\n{stdout}",
        output.status
    );
}

/// A scratch directory of this test's own under cargo's per-target tmpdir.
fn scratch_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the scratch directory");
    dir
}

/// Runs a child in `role` with its stderr on one end of an `AF_UNIX`
/// datagram socketpair and returns every datagram it sent. Each `write(2)` to
/// such a socket is exactly one datagram, so the count is the write count.
/// `prepare` gets (the child's end, the reader's end) before the spawn.
fn datagrams_from_child(role: &str, prepare: impl FnOnce(&UnixDatagram, &UnixDatagram)) -> Vec<Vec<u8>> {
    let (child_end, reader) = UnixDatagram::pair().expect("failed to create a socketpair");
    prepare(&child_end, &reader);
    // The child gets a duplicate; this end stays open until the datagrams
    // are read, since macOS answers a read on a pair whose peer has closed
    // with ECONNRESET.
    let child_copy = child_end.try_clone().expect("failed to duplicate the child's end");
    let output =
        child(role).stderr(Stdio::from(OwnedFd::from(child_copy))).output().expect("failed to run the child");
    assert_child_passed(role, &output);

    reader.set_nonblocking(true).expect("failed to make the reader non-blocking");
    let mut datagrams = Vec::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        match reader.recv(&mut buffer) {
            Ok(len) => datagrams.push(buffer[..len].to_vec()),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("failed to read a datagram: {err}"),
        }
    }
    drop(child_end);
    datagrams
}

/// Behaviour 1: a line with arguments, newline included, is exactly one
/// `write(2)`.
#[test]
fn a_line_with_arguments_is_one_write_with_its_newline() {
    let datagrams = datagrams_from_child("probe", |_, _| {});
    let readable: Vec<String> = datagrams.iter().map(|d| String::from_utf8_lossy(d).into_owned()).collect();
    assert_eq!(readable, [format!("{PROBE_LINE}\n")], "expected the whole line in one write");
}

/// Behaviour 4: a stderr whose every write fails (a pipe with no reader, so
/// EPIPE; std ignores SIGPIPE) is ignored, not a panic.
#[test]
fn a_failed_stderr_write_is_ignored() {
    let (reader, writer) = std::io::pipe().expect("failed to create a pipe");
    drop(reader);
    let output = child("broken").stderr(Stdio::from(writer)).output().expect("failed to run the child");
    assert_child_passed("broken", &output);
}

/// Behaviour 3 / T1: several processes appending through `log_line!` to one
/// `O_APPEND` file (the shim's `daemon_stderr` setup, and the shared
/// `G_MESH_DAEMON_LOG` case) never split each other's lines. Only line
/// integrity is asserted; the order between writers is not.
#[test]
fn concurrent_writer_processes_never_split_a_line() {
    let dir = scratch_dir("concurrent_writer_processes_never_split_a_line");
    let log_path = dir.join("shared.log");
    let go = dir.join("go");
    let log = OpenOptions::new().create(true).append(true).open(&log_path).expect("failed to open the log");

    let children: Vec<_> = (0..WRITERS)
        .map(|id| {
            let stderr = log.try_clone().expect("failed to clone the log handle");
            child(&format!("writer:{id}"))
                .env(GO_FILE_ENV, &go)
                .stderr(Stdio::from(stderr))
                .spawn()
                .expect("failed to spawn a writer")
        })
        .collect();
    fs::write(&go, "").expect("failed to create the go file");
    for (id, writer) in children.into_iter().enumerate() {
        let output = writer.wait_with_output().expect("failed to wait for a writer");
        assert_child_passed(&format!("writer:{id}"), &output);
    }

    let text = fs::read_to_string(&log_path).expect("failed to read the log");
    let _ = fs::remove_dir_all(&dir);
    assert!(text.ends_with('\n'), "the log does not end with a whole line");

    let mut seen = HashSet::new();
    let mut malformed = Vec::new();
    for line in text.lines() {
        let parsed =
            line.strip_prefix("[w").and_then(|rest| rest.split_once("] seq=")).and_then(|(id, rest)| {
                Some((id.parse::<usize>().ok()?, rest.split_once(' ')?.0.parse::<usize>().ok()?))
            });
        match parsed {
            Some((id, n)) if id < WRITERS && n < LINES_PER_WRITER && line == writer_line(id, n) => {
                assert!(seen.insert((id, n)), "line {id}:{n} appears twice");
            }
            _ => malformed.push(line.chars().take(200).collect::<String>()),
        }
    }
    assert!(
        malformed.is_empty(),
        "{} malformed line(s), i.e. lines split by another writer; first few:\n{}",
        malformed.len(),
        malformed.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    );
    assert_eq!(seen.len(), WRITERS * LINES_PER_WRITER, "every line of every writer, each once");
}
