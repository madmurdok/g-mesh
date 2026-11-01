//! `G_MESH_EDIT_TIMING` turns on `g-mesh timing:` phase-boundary
//! lines in `daemon.log`, and leaves the log exactly as it was without it.
//!
//! A real daemon, bootstrapped through the shim against a `G_MESH_HOME` of
//! the test's own (so its `daemon.log` is the production one under
//! `<home>/projects/<hash>/`), serves one TypeScript file through the stub
//! persona of `g-mesh-fake-plugin` (`plugins/sdk/fake/main.rs`), which exists
//! only after `cargo build --workspace`. One file is edited; the stub records
//! every method it receives, which is how the test knows the edit's
//! `fileChanged` and the `semanticPass` after it went out.
//!
//! What is asserted about order is only what one thread of the daemon
//! orders: the watcher thread settles the edit, routes it, and runs both
//! round trips in sequence, so those lines appear in that order with a
//! non-decreasing `t=`. Nothing is asserted between processes.
//! The design is `docs/design/GM-435-edit-to-tier-latency-method.md` §3.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::identity::project_hash;
use g_mesh::log::{TIMING_ENV, TIMING_PREFIX};
use g_mesh::paths::HOME_ENV;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tokio::process::Command;

use crate::common;
use crate::common::{IsolatedHome, Lifeline};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const FAKE_PLUGIN_BIN: &str = "g-mesh-fake-plugin";
/// Where the stub appends each control method it receives.
const METHOD_LOG_ENV: &str = "G_MESH_FAKE_PLUGIN_LOG";
/// The file the test edits.
const EDITED: &str = "edited.ts";

/// The isolated home, the project, and (outside the watched root) the
/// stub's manifest and method log.
struct Harness {
    home: IsolatedHome,
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self {
            home: IsolatedHome::new(),
            project: tempfile::tempdir().unwrap(),
            aux: tempfile::tempdir().unwrap(),
        };
        let stub = Path::new(BIN)
            .parent()
            .expect("the daemon binary has a directory")
            .join(format!("{FAKE_PLUGIN_BIN}{}", std::env::consts::EXE_SUFFIX));
        assert!(stub.is_file(), "{} does not exist - run `cargo build --workspace` first", stub.display());
        fs::write(harness.method_log(), "").unwrap();
        fs::write(harness.root().join("seed.ts"), "export const seed = 1;\n").unwrap();

        let language_dir = harness.plugins_root().join("typescript");
        fs::create_dir_all(&language_dir).unwrap();
        fs::write(
            language_dir.join("plugin.toml"),
            format!(
                "[plugin]\nlanguage = \"typescript\"\nprotocol_version = 2\nplugin_version = \"0.1.0\"\n\n\
                 [plugin.spawn]\ncommand = \"${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}\"\n\
                 args = [\"--language\", \"typescript\", \"--plugin-version\", \"0.1.0\"]\n\n\
                 [plugin.languages]\nextensions = [\".ts\"]\n\n\
                 [plugin.capabilities]\nsemantic_pass = true\n",
            ),
        )
        .unwrap();
        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn plugins_root(&self) -> PathBuf {
        self.aux.path().join("plugins")
    }

    fn method_log(&self) -> PathBuf {
        self.aux.path().join("methods.log")
    }

    /// Derived from the isolated home, not from `storage::connection`, which
    /// would answer for this process's ambient home.
    fn state_dir(&self) -> PathBuf {
        self.home.projects_root().join(project_hash(self.root()).expect("failed to hash the project root"))
    }

    fn daemon_log(&self) -> PathBuf {
        self.state_dir().join(g_mesh::shim::DAEMON_LOG_FILE)
    }

    fn methods(&self) -> Vec<String> {
        fs::read_to_string(self.method_log()).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Bootstraps the daemon through the shim with `G_MESH_EDIT_TIMING` set
    /// to `timing` (removed when `None`), and makes one tool call, which
    /// builds the index and wakes the interactive plugin.
    async fn connect(&self, timing: Option<&str>) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let (home, plugins, methods) = (self.home.dir.clone(), self.plugins_root(), self.method_log());
        let timing = timing.map(str::to_string);
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env(HOME_ENV, &home)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env_remove(g_mesh::shim::DAEMON_LOG_ENV)
                .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, &plugins)
                .env(METHOD_LOG_ENV, &methods)
                // No real model: keeps the embedding layer a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
            match &timing {
                Some(value) => cmd.env(TIMING_ENV, value),
                None => cmd.env_remove(TIMING_ENV),
            };
        }))
        .expect("failed to spawn the shim");
        let client = ().serve(transport).await.expect("the shim must reach the daemon");
        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("get_file_outline")
                    .with_arguments(json!({ "file_path": "seed.ts" }).as_object().cloned().unwrap()),
            )
            .await
            .expect("the activating call failed");
        let phase = daemon::phase_path_in(&self.state_dir());
        tokio::task::spawn_blocking(move || {
            common::wait_for("index.phase to read \"ready\"", common::startup_timeout(), || {
                fs::read_to_string(&phase).is_ok_and(|text| text.trim() == "ready")
            })
        })
        .await
        .unwrap();
        assert!(
            self.daemon_log().starts_with(&self.home.dir),
            "the daemon log must live under this test's own home"
        );
        client
    }

    /// Edits [`EDITED`] until the stub has received a `fileChanged` and then
    /// a `semanticPass` after it, beyond what it had before the first edit.
    /// Re-edited rather than written once: the watcher is registered a moment
    /// after activation and nothing marks that instant (see
    /// `semantic_pass_trigger.rs`); the gap clears the 300 ms debounce.
    fn edit_until_routed(&self) {
        let before = self.methods().len();
        let deadline = Instant::now() + common::startup_timeout();
        let mut edits = 0;
        loop {
            edits += 1;
            fs::write(self.root().join(EDITED), format!("export const edited = {edits};\n")).unwrap();
            std::thread::sleep(Duration::from_millis(700));
            let methods = self.methods();
            let routed = methods.get(before..).is_some_and(|tail| {
                tail.iter()
                    .position(|m| m == "fileChanged")
                    .is_some_and(|changed| tail[changed..].iter().any(|m| m == "semanticPass"))
            });
            if routed {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no fileChanged + semanticPass after {edits} edit(s): {methods:?}"
            );
        }
    }

    /// Stops the daemon and its plugins, so nothing writes to the log after.
    fn stop_daemon(&self) {
        let state = self.state_dir();
        for path in [
            daemon::pid_path_in(&state),
            daemon::plugin_pid_path_in(&state),
            state.join("plugin-typescript.pid"),
        ] {
            common::kill_pid_file(&path);
        }
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.daemon_log()).unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop_daemon();
    }
}

/// One parsed timing line: its stamp, its event, and its fields in order.
#[derive(Debug)]
struct Timing {
    t: Duration,
    event: String,
    fields: Vec<(String, String)>,
}

impl Timing {
    fn field(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn is(&self, event: &str, pairs: &[(&str, &str)]) -> bool {
        self.event == event && pairs.iter().all(|(k, v)| self.field(k) == Some(v))
    }
}

fn malformed<T>(line: &str) -> T {
    panic!("malformed timing line: {line:?}")
}

/// Parses `line` as `g-mesh timing: t=<secs>.<6 digits> <event> k=v...`,
/// panicking on a line that starts like one and is not.
fn parse_timing(line: &str) -> Option<Timing> {
    let rest = line.strip_prefix(TIMING_PREFIX)?;
    let rest = rest.strip_prefix("t=").unwrap_or_else(|| malformed(line));
    let mut tokens = rest.split(' ');
    let (secs, micros) =
        tokens.next().and_then(|stamp| stamp.split_once('.')).unwrap_or_else(|| malformed(line));
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        malformed::<()>(line);
    }
    if micros.len() != 6 || !micros.bytes().all(|b| b.is_ascii_digit()) {
        malformed::<()>(line);
    }
    let t = Duration::from_secs(secs.parse().unwrap()) + Duration::from_micros(micros.parse().unwrap());
    let event = tokens
        .next()
        .filter(|event| !event.is_empty() && !event.contains('='))
        .unwrap_or_else(|| malformed(line));
    let fields = tokens
        .map(|token| {
            let (k, v) = token.split_once('=').unwrap_or_else(|| malformed(line));
            (k.to_string(), v.to_string())
        })
        .collect();
    Some(Timing { t, event: event.to_string(), fields })
}

/// The lines of `text` that are not timing lines.
fn other_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|line| !line.contains(TIMING_PREFIX.trim_end())).collect()
}

/// The first line of every daemon start, written whether or not timing is on.
fn has_startup_line(text: &str) -> bool {
    other_lines(text).iter().any(|line| line.starts_with("g-mesh daemon: "))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_edit_with_timing_on_logs_each_phase_of_its_round_trips() {
    let harness = Harness::new();
    let client = harness.connect(Some("1")).await;
    tokio::task::block_in_place(|| harness.edit_until_routed());

    let log_path = harness.daemon_log();
    let read_timings = move || -> Vec<Timing> {
        fs::read_to_string(&log_path).unwrap_or_default().lines().filter_map(parse_timing).collect()
    };
    // The stub records a request when it reads it, which may be before the
    // daemon has written the lines after it: wait for the chain's last line.
    let timings = {
        tokio::task::spawn_blocking(move || {
            let mut found = Vec::new();
            common::wait_for("the edit's semantic_done line", common::startup_timeout(), || {
                found = read_timings();
                found.iter().any(|t| t.is("rt_sent", &[("method", "fileChanged"), ("file", EDITED)]))
                    && found.iter().any(|t| t.event == "semantic_done")
                    && edit_chain(&found).is_some()
            });
            found
        })
        .await
        .unwrap()
    };
    drop(client);
    harness.stop_daemon();
    let text = harness.log_text();
    assert!(has_startup_line(&text), "the existing daemon lines must still be there:\n{text}");
    let chain = edit_chain(&timings).expect("checked above");

    let names: Vec<&str> = chain.iter().map(|t| t.event.as_str()).collect();
    assert_eq!(
        names,
        [
            "settled",
            "route",
            "plugin_taken",
            "rt_sent",
            "rt_answered",
            "rt_committed",
            "rt_embedded",
            "rt_vectors",
            "semantic_start",
            "rt_sent",
            "rt_answered",
            "rt_committed",
            "rt_embedded",
            "rt_vectors",
            "semantic_done",
        ],
        "the edit's phase lines, in the watcher thread's order"
    );
    for pair in chain.windows(2) {
        assert!(
            pair[0].t <= pair[1].t,
            "t= went backwards within one thread: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    let edit_id = chain[3].field("id").unwrap();
    let semantic_id = format!("semanticPass:num:{edit_id}");
    assert!(chain[2].field("queued").is_some(), "plugin_taken names queued=: {:?}", chain[2]);
    assert!(chain[5].is("rt_committed", &[("method", "fileChanged")]), "{:?}", chain[5]);
    for key in ["nodes", "edges", "deleted_nodes", "deleted_edges"] {
        assert!(chain[5].field(key).is_some_and(|v| v.parse::<usize>().is_ok()), "{key} on {:?}", chain[5]);
    }
    for key in ["texts", "hits", "embedded"] {
        assert!(chain[6].field(key).is_some_and(|v| v.parse::<usize>().is_ok()), "{key} on {:?}", chain[6]);
    }
    assert!(
        chain[8].is("semantic_start", &[("id", semantic_id.as_str()), ("file", EDITED)]),
        "{:?}",
        chain[8]
    );
    assert!(
        chain[9].is("rt_sent", &[("id", semantic_id.as_str()), ("method", "semanticPass")]),
        "{:?}",
        chain[9]
    );
    assert!(
        chain[14].is("semantic_done", &[("id", semantic_id.as_str()), ("ok", "true")]),
        "{:?}",
        chain[14]
    );

    // The watcher saw the edit before settling it; its thread also writes
    // fs_event, so the newest one naming the file precedes the settle.
    assert!(
        timings.iter().any(|t| t.is("fs_event", &[("path", EDITED)]) && t.t <= chain[0].t),
        "an fs_event path={EDITED} line must precede the settle"
    );
}

/// The edit's lines in log order: the last `rt_sent method=fileChanged
/// file=EDITED` whose semantic pass is done, with the `settled`/`route`/
/// `plugin_taken` lines that precede it for that file, then every line
/// carrying its id or its semantic pass's id. `None` until all are written.
fn edit_chain(timings: &[Timing]) -> Option<Vec<&Timing>> {
    let sent = timings.iter().enumerate().rev().find(|(_, t)| {
        t.is("rt_sent", &[("method", "fileChanged"), ("file", EDITED)])
            && t.field("id").is_some_and(|id| {
                let done = format!("semanticPass:num:{id}");
                timings.iter().any(|t| t.is("semantic_done", &[("id", &done)]))
            })
    });
    let (at, sent) = sent?;
    let id = sent.field("id")?;
    let semantic_id = format!("semanticPass:num:{id}");
    let before = &timings[..at];
    let last = |event: &str, key: &str| {
        before.iter().rev().find(|t| {
            t.event == event
                && t.field(key).is_some_and(|v| match key {
                    "names" => v.split(',').any(|name| name == EDITED),
                    _ => v == EDITED,
                })
        })
    };
    let mut chain = vec![last("settled", "names")?, last("route", "file")?, last("plugin_taken", "file")?];
    chain.extend(
        timings[at..].iter().filter(|t| t.field("id").is_some_and(|v| v == id || v == semantic_id.as_str())),
    );
    Some(chain)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_edit_with_timing_unset_logs_no_timing_line() {
    let harness = Harness::new();
    let client = harness.connect(None).await;
    // Once the stub has the semanticPass, the edit's rt_* lines up to its
    // semantic_start would already have been written.
    tokio::task::block_in_place(|| harness.edit_until_routed());
    drop(client);
    harness.stop_daemon();

    let text = harness.log_text();
    assert!(has_startup_line(&text), "the existing daemon lines must still be there:\n{text}");
    let timing: Vec<_> = text.lines().filter(|line| line.contains(TIMING_PREFIX.trim_end())).collect();
    assert!(timing.is_empty(), "{TIMING_ENV} unset must write no timing line, got {timing:?}");
}
