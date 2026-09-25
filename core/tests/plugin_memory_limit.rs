//! GM-291's own acceptance test: `[plugin] memoryLimitMb` (GM-274) enforced
//! against a REAL rust-analyzer, not GM-274's synthetic "memory-hungry" Node
//! fixture (`daemon::test_plugin::install_memory_hungry`,
//! `core/src/daemon/lifecycle.rs`'s own
//! `a_plugin_over_its_memory_limit_is_put_to_sleep_and_its_language_suspended`).
//! Same shape as that test - a real [`PluginSupervisor`] driven directly, not
//! through a spawned `g-mesh daemon` subprocess or the supervise-loop tick -
//! but pointed at the real `plugins/rust/plugin.toml` manifest and the real
//! `plugins/rust/conformance/project` fixture (GM-290's own conformance
//! project), so what trips the limit is genuinely rust-analyzer's own memory,
//! not a synthetic allocator.
//!
//! # Why `PluginSupervisor` directly, not a spawned daemon
//!
//! `PluginSupervisor::start`/`check_memory_limit`/`semantic_pass`/
//! `file_changed`/`replay_pending` are all `pub`, and calling them directly
//! from an external integration test crate is exactly what GM-274's own unit
//! tests do from *inside* the crate - the only thing this test does
//! differently is use the real plugin binary and a real language server
//! behind it, both of which need the heavier `core/tests/` treatment (a real
//! `cargo build`, `rust-analyzer` on `PATH`) rather than living in
//! `core/src/daemon/lifecycle.rs`'s fast `#[cfg(test)]` suite. Driving the
//! real `g-mesh daemon` binary and its supervise-loop tick was tried first
//! and rejected: `IdleTimeouts::tick` and `check_memory_limit` fight over the
//! very same `PluginSupervisor::inner` mutex a real `semanticPass` round trip
//! holds for its whole ~10-25s duration (see "The lock race" below), so a
//! subprocess-level test would have had to either race the production 30s
//! tick (slow, and no more informative) or introduce its own timing
//! assumptions about a daemon it cannot see inside - exactly what calling the
//! same methods directly avoids.
//!
//! # The lock race (GM-291's own finding, not GM-274's)
//!
//! `PluginSupervisor::semantic_pass` and `PluginSupervisor::check_memory_limit`
//! both start with `self.inner.lock()`, and `semantic_pass` holds that lock -
//! not just to spawn the request, but for the *entire* synchronous round trip
//! to the plugin, including however long rust-analyzer takes to answer - for
//! its whole duration. That means `check_memory_limit` can never sample
//! *during* the pass that is inflating memory; it can only run before that
//! pass starts or after it returns. Timed with real threads over the real
//! fixture (`race_pass_against_memory_check` below, printed as this test's
//! own `pass_returned_at`/`check_returned_at`), the pass's own calling
//! thread - `daemon::semantic::run_with_registry`/`run_once`, which records
//! `language_state.semanticPassAt` immediately after `semantic_pass` returns
//! `Ok(true)`, no yield point in between - reliably beats a `check_memory_limit`
//! call that was already blocked on the same lock before the pass even
//! began, by a few hundred milliseconds: the calling thread simply continues
//! executing, while the blocked thread needs an OS wakeup plus a whole-system
//! `sysinfo::refresh_processes` call before it can even measure RSS. Measured
//! three times across this task's runs, at load averages from ~15 to ~78:
//! the pass returned at 13.3s/16.9s/17.3s of wall time and `check_memory_limit`
//! returned 0.412s-0.417s later every time (13.7s/17.3s/17.7s), and
//! `language_state.semanticPassAt` was already set every time by the time
//! `check_memory_limit` finished.
//!
//! **Consequence for the architecture doc's own claim** ("if the pass never
//! completed, the receiver gap stays listed... and not one moment longer",
//! `docs/architecture/multi-language-plugins.md`'s "Plugin memory limit"
//! section): that sentence is true as written - it is conditional - but the
//! condition it depends on ("the pass never completed") does not hold for
//! the common case this test exercises, a language's first cold pass tripping
//! the very limit its own memory growth crosses. Suspension cannot preempt
//! the request that causes it; it only ever catches the *plateau* that
//! request leaves behind (real rust-analyzer memory does not fall back down
//! after indexing - confirmed flat for 19+ seconds in this task's own manual
//! measurement, see the completion report), on whatever the next
//! `check_memory_limit` call after that plateau forms happens to be - a few
//! hundred milliseconds later if one was already waiting on the lock, up to
//! one full tick period (30s in production) otherwise. So neither test below
//! *asserts* a fixed winner of this race - each reads which side actually
//! won (`semantic_pass_done_for_rust`/the DB read in the MCP test) and
//! asserts the generated instructions are *correct for that outcome* -
//! asserting a specific winner would be exactly the flaky test this task's
//! own house rules warn against building.
//!
//! # What is and is not exercised here
//!
//! The two `#[test]`s below (direct `PluginSupervisor` calls) exercise: the
//! real rust plugin binary, real rust-analyzer, real memory sampling
//! (`daemon::memory::process_tree_rss_mb` via `check_memory_limit`), real
//! suspension (`semantic_suspended`), and real structural indexing
//! (`daemon::bulk_index::run`, the same one-shot walk `daemon::run`'s cold
//! start uses) - but not `mcp::instructions::build` itself, which is private
//! to `mcp::mod` by design (see that module's own comment) and so not
//! reachable from here. The `#[tokio::test]` at the bottom of this file
//! covers that piece instead, through a real daemon and a real MCP client.
//! Not exercised anywhere in this file: the supervise-loop tick's own
//! internals (`daemon::lifecycle::supervise`, though the `#[tokio::test]`
//! does run under its real production cadence) and `daemon::run`'s config
//! wiring (`project_config.plugin.memory_limit_mb` -> `PluginRegistry::new`).
//! `core/src/daemon/mod.rs`'s own `#[cfg(test)]` suite and
//! `core/src/config/mod.rs`'s round-trip tests already cover that wiring.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::config::{self, PluginConfig, ProjectConfig};
use g_mesh::daemon::bulk_index;
use g_mesh::daemon::lifecycle::PluginSupervisor;
use g_mesh::daemon::manifest::{read_manifest, DiscoveredPlugins, PluginManifest};
use g_mesh::daemon::registry;
use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// GM-290's own conformance fixture - a real two-crate Cargo workspace with
/// no external dependencies (offline-friendly), already measured by that
/// task to plateau at 535-555MiB of rust-analyzer RSS. Reused rather than a
/// smaller ad hoc fixture so this test's own numbers are directly comparable
/// to GM-290's.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/rust/conformance/project");
const RUST_PLUGIN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/rust");

/// Comfortably above the plugin's own structural-only baseline (~5-6MB,
/// measured in this task's own manual sampling before rust-analyzer ever
/// spawns) and comfortably below the plateau (535-580MB across GM-290's and
/// this task's own measurements) - so suspension is caused by the semantic
/// engine's real memory, not tree-sitter overhead, and is not a hair's-
/// breadth threshold a loaded machine could cross by accident in either
/// direction (the same margin argument GM-274's own 100MB-for-a-200MB-hog
/// threshold makes for its synthetic fixture).
const LOW_LIMIT_MB: u64 = 150;

/// Nowhere near real usage (the plateau tops out under 600MB even on the
/// large end of what GM-290 measured) - the discriminating control.
const UNREACHABLE_LIMIT_MB: u64 = 1_000_000;

/// Generous next to the 9-25s this task measured for a cold pass across load
/// averages from 4 to 150+ today - a deadlock guard, not a timing assertion.
const TIMEOUT: Duration = Duration::from_secs(180);

fn copy_dir(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("failed to create a fixture directory");
    for entry in std::fs::read_dir(src).expect("failed to read the fixture directory") {
        let entry = entry.expect("failed to read a fixture directory entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("failed to stat a fixture entry").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("failed to copy a fixture file");
        }
    }
}

/// A private copy of GM-290's conformance fixture - never the checked-in
/// original, since a real rust-analyzer pass over it writes `target/` and can
/// rewrite `Cargo.lock`.
fn fixture_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("failed to create a project root");
    copy_dir(Path::new(FIXTURE), project.path());
    project
}

/// How many `.rs` files the fixture has - `semantic_pass`'s `file_count`
/// argument, computed rather than hardcoded (a fixture edit must not leave a
/// magic number silently wrong) the same way `daemon::semantic::
/// indexed_file_count` computes it from the index in production; this test
/// has no index to read it from before the first pass, so it counts the
/// files on disk directly instead.
fn count_rust_files(root: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        for entry in std::fs::read_dir(dir).expect("failed to walk the fixture") {
            let entry = entry.expect("failed to read a fixture entry");
            let path = entry.path();
            if entry.file_type().expect("failed to stat a fixture entry").is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                walk(&path, count);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(root, &mut count);
    count
}

fn in_memory_index() -> IndexStore {
    let conn = Connection::open_in_memory().expect("failed to open an in-memory index");
    conn.pragma_update(None, "foreign_keys", "ON").expect("failed to enable foreign keys");
    schema::apply(&conn).expect("failed to apply the schema");
    IndexStore::new(conn)
}

/// A real one-shot structural bulk index over `project_root`, through the
/// exact same `daemon::bulk_index::run` entry point `daemon::run`'s cold
/// start uses - a `DiscoveredPlugins` naming only "rust" so this does not
/// also try (and, absent a built `dist/`, potentially fail) the bundled
/// TypeScript/Go plugins the real `bundled_roots()` discovery would also
/// find in this checkout.
fn structural_bulk_index(project_root: &Path, conn: &IndexStore, manifest: &PluginManifest) {
    let discovered = DiscoveredPlugins {
        manifests: HashMap::from([("rust".to_string(), manifest.clone())]),
        routing: HashMap::from([(".rs".to_string(), "rust".to_string())]),
    };
    let summary = bulk_index::run(project_root, conn, None, &discovered)
        .expect("the real one-shot structural bulk index must succeed");
    assert!(summary.nodes > 0, "the structural walk over a real fixture must find real nodes: {summary:?}");
}

/// Whether `language_state.semanticPassAt` is set for "rust" - the ground
/// truth `mcp::instructions::build` (private to `mcp::mod`, not reachable
/// from an external integration test - `core/src/mcp/mod.rs`'s own comment
/// explains why that module stays unwidened) renders its receiver-call-gap
/// clause from. Printed by both `PluginSupervisor`-level tests below so their
/// own report states plainly which side of the lock race actually happened;
/// checked against the real generated instructions text separately, through
/// the real MCP protocol, in
/// `the_generated_mcp_instructions_reflect_a_real_suspended_rust`.
fn semantic_pass_done_for_rust(conn: &IndexStore) -> bool {
    let present = schema::present_languages_with_semantic_state(&conn.lock().unwrap())
        .expect("failed to read the present-language/semantic-state pairs");
    assert_eq!(present.len(), 1, "only rust is present in this fixture: {present:?}");
    let (language, semantic_pass_done) = present[0].clone();
    assert_eq!(language, "rust");
    semantic_pass_done
}

/// Spawns the real plugin with `idle_timeout: None` (idle sleep off
/// entirely) so nothing but `check_memory_limit` can ever put it to sleep -
/// the same isolation GM-274's own lifecycle.rs tests rely on, and load-
/// bearing here specifically: an ordinary idle timeout racing the same lock
/// would make "suspended because of memoryLimitMb" ambiguous.
///
/// Returns the supervisor together with the `TempDir` its pid file lives in -
/// the caller must keep that guard alive for as long as the supervisor runs.
/// Dropped too early, its pid-file/suspension-marker writes fail with "No
/// such file or directory" (harmless - both are best-effort, per
/// `check_memory_limit`'s own doc comment - but still worth not doing, since
/// a clean run has nothing to explain away in its own log).
fn start_supervisor(
    project_root: &Path,
    manifest: PluginManifest,
    memory_limit_mb: Option<u64>,
) -> (Arc<PluginSupervisor>, tempfile::TempDir) {
    let pid_dir = tempfile::tempdir().expect("failed to create a pid-file directory");
    let supervisor = PluginSupervisor::start(
        project_root,
        manifest,
        pid_dir.path().join("plugin-rust.pid"),
        None,
        memory_limit_mb,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the real rust plugin must start");
    (supervisor, pid_dir)
}

/// Runs `supervisor`'s first whole-project `semantic_pass` on one thread and
/// `check_memory_limit` on another, mirroring the real concurrency
/// (`daemon::semantic::run_with_registry`/`run_once` vs `daemon::lifecycle::
/// supervise`'s tick) rather than calling them sequentially, which would
/// prove nothing about the lock race this test's module doc describes. The
/// 50ms head start on the checking thread mimics a supervise tick that was
/// already waiting on the lock before the pass began - the most favorable
/// timing suspension could have, and still not favorable enough (see the
/// module doc's measured numbers).
///
/// Returns the two threads' own elapsed-since-start readings and the pass's
/// result, all printed by the caller - never truncated, per this task's own
/// house rule on diagnostics.
fn race_pass_against_memory_check(
    supervisor: &Arc<PluginSupervisor>,
    conn: &Arc<IndexStore>,
    file_count: usize,
) -> (Duration, anyhow::Result<bool>, Duration) {
    let start = Instant::now();

    let sup_pass = Arc::clone(supervisor);
    let conn_pass = Arc::clone(conn);
    let pass_handle = thread::spawn(move || {
        let result = sup_pass.semantic_pass(&conn_pass, Vec::new(), file_count);
        let returned_at = start.elapsed();
        // Mirrors `daemon::semantic::run_with_registry`'s own sequencing
        // exactly: record immediately after `Ok(true)`, same thread, no
        // yield point - the half of the race this test's module doc says
        // reliably wins.
        if let Ok(true) = result {
            schema::record_language_semantic_pass(&conn_pass.lock().unwrap(), "rust")
                .expect("failed to record the completed semantic pass");
        }
        (returned_at, result)
    });

    let sup_check = Arc::clone(supervisor);
    let check_handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        sup_check.check_memory_limit();
        start.elapsed()
    });

    let (pass_returned_at, pass_result) = pass_handle.join().expect("the semantic-pass thread panicked");
    let check_returned_at = check_handle.join().expect("the check-memory-limit thread panicked");
    (pass_returned_at, pass_result, check_returned_at)
}

#[test]
fn a_real_rust_analyzer_over_its_memory_limit_is_suspended_and_structural_work_continues() {
    let load = std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    eprintln!("GM-291: machine state before this test's real rust-analyzer run: {load:?}");

    let project = fixture_project();
    let manifest = read_manifest(Path::new(RUST_PLUGIN_DIR)).expect("plugins/rust/plugin.toml must parse");
    let conn = Arc::new(in_memory_index());
    structural_bulk_index(project.path(), &conn, &manifest);

    let file_count = count_rust_files(project.path());
    assert!(file_count > 0, "the fixture must have real .rs files to index");

    let (supervisor, _pid_dir) = start_supervisor(project.path(), manifest.clone(), Some(LOW_LIMIT_MB));
    assert!(!supervisor.is_semantic_suspended(), "not suspended before any pass or check has run");
    assert!(supervisor.pid().is_some(), "a freshly started plugin is awake");

    let (pass_returned_at, pass_result, check_returned_at) =
        race_pass_against_memory_check(&supervisor, &conn, file_count);
    eprintln!(
        "GM-291 timing: pass_returned_at={pass_returned_at:?} check_returned_at={check_returned_at:?} \
         (both measured from the same start; see module doc for what the gap between them means)"
    );
    assert!(
        pass_returned_at < TIMEOUT && check_returned_at < TIMEOUT,
        "the race did not settle within {TIMEOUT:?} - see the machine state printed above"
    );
    pass_result.expect("the real rust-analyzer whole-project pass must not error");

    // 1) the limit trips and the plugin is put to sleep.
    assert!(supervisor.pid().is_none(), "a plugin over its memory limit must be put to sleep");
    // 2) the semantic tier is suspended.
    assert!(supervisor.is_semantic_suspended(), "its language's semantic passes must be suspended");

    // 3) structural fileChanged keeps working while suspended: append a new,
    // distinctly-named declaration to a real fixture file, route it through
    // the supervisor exactly as the daemon's watcher would, and confirm it
    // actually landed as a node - not merely that nothing crashed.
    let orphan = project.path().join("crates/alpha/src/orphan.rs");
    let mut contents = std::fs::read_to_string(&orphan).expect("failed to read the fixture file to edit");
    contents.push_str("\npub fn gm_291_structural_probe_marker() {}\n");
    std::fs::write(&orphan, contents).expect("failed to edit the fixture file");

    supervisor.file_changed(&conn, "crates/alpha/src/orphan.rs".to_string());
    assert!(supervisor.has_pending(), "the edit must queue while the plugin is asleep");
    let replayed = supervisor.replay_pending(&conn).expect("the wake must succeed");
    assert_eq!(replayed, 1, "exactly the one queued edit must be replayed");
    assert!(supervisor.pid().is_some(), "the wake must have relaunched the plugin for structural work");

    let marker_nodes: i64 = conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE name = 'gm_291_structural_probe_marker' AND language = 'rust'",
            [],
            |row| row.get(0),
        )
        .expect("failed to query the index for the structural probe marker");
    assert_eq!(marker_nodes, 1, "structural indexing must still commit real nodes while suspended");

    // Suspension persists across the wake - it is not an idle-sleep artifact
    // that clears once the plugin wakes back up (same assertion GM-274's own
    // lifecycle.rs test makes for its synthetic fixture).
    assert!(supervisor.is_semantic_suspended(), "suspension must survive the structural wake");
    assert!(
        !supervisor.semantic_pass(&conn, Vec::new(), file_count).expect("must not error, just skip"),
        "a suspended language's whole-project semantic pass must not run, even freshly awake"
    );

    // 4) which side of the lock race this run landed on - checked against the
    // *real* generated MCP instructions text separately, through the real
    // protocol (this file's `the_generated_mcp_instructions_reflect_a_real_suspended_rust`),
    // since `mcp::instructions` is deliberately not part of this crate's
    // external surface (see that test's own doc comment).
    let semantic_pass_done = semantic_pass_done_for_rust(&conn);
    eprintln!(
        "GM-291: language_state.semanticPassAt was {} by the time this test inspected it - see \
         this file's module doc's \"lock race\" section for why.",
        if semantic_pass_done { "already SET" } else { "still NULL" }
    );

    supervisor.sleep_now("test cleanup");
}

/// The discriminating control this test's own house rules require: the same
/// real rust-analyzer, the same real race, a limit nothing in this fixture
/// could ever cross. Mirrors GM-274's own
/// `with_no_memory_limit_configured_an_oversized_plugin_is_left_alone`, but
/// with `memoryLimitMb` genuinely *set* (not `None`) so this also proves
/// `check_memory_limit` really does sample and compare here, not merely skip
/// sampling altogether the way an unset limit would.
#[test]
fn with_a_memory_limit_far_above_real_usage_a_real_rust_analyzer_is_left_alone() {
    let load = std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    eprintln!("GM-291: machine state before this test's real rust-analyzer run: {load:?}");

    let project = fixture_project();
    let manifest = read_manifest(Path::new(RUST_PLUGIN_DIR)).expect("plugins/rust/plugin.toml must parse");
    let conn = Arc::new(in_memory_index());
    structural_bulk_index(project.path(), &conn, &manifest);
    let file_count = count_rust_files(project.path());

    let (supervisor, _pid_dir) = start_supervisor(project.path(), manifest, Some(UNREACHABLE_LIMIT_MB));

    let (pass_returned_at, pass_result, check_returned_at) =
        race_pass_against_memory_check(&supervisor, &conn, file_count);
    eprintln!(
        "GM-291 timing (control): pass_returned_at={pass_returned_at:?} check_returned_at={check_returned_at:?}"
    );
    assert!(
        pass_returned_at < TIMEOUT && check_returned_at < TIMEOUT,
        "the control run did not settle in time"
    );
    assert!(matches!(pass_result, Ok(true)), "the real pass must complete normally: {pass_result:?}");

    assert!(
        !supervisor.is_semantic_suspended(),
        "with memoryLimitMb far above real usage, a real rust-analyzer must be left alone"
    );
    assert!(supervisor.pid().is_some(), "the plugin must still be running, not put to sleep");

    supervisor.sleep_now("test cleanup");
}

// ---------------------------------------------------------------------------
// The real MCP protocol, for the one acceptance criterion the tests above
// cannot reach: `mcp::instructions` is deliberately not part of this crate's
// public surface (`core/src/mcp/mod.rs`'s own comment: "nothing outside `mcp`
// needs ... `instructions` ... and widening them would just be surface
// nothing uses"), so "the receiver gap stays listed in the generated MCP
// instructions" is checked here by actually asking a real MCP client what a
// real session's `initialize` response says - a real `g-mesh daemon`, a real
// `g-mesh mcp-shim`, a real `rmcp` client, the same shape `mcp_e2e.rs` uses
// for the rest of this tool surface.
// ---------------------------------------------------------------------------

/// A real daemon's project root, config.toml already carrying a low
/// `memoryLimitMb`, and the daemon subprocess itself, torn down on drop the
/// same way `mcp_e2e.rs`'s and `plugin_bridge.rs`'s own `Project` helpers do.
struct DaemonProject {
    dir: tempfile::TempDir,
    daemon: Child,
}

impl DaemonProject {
    /// Spawns a real `g-mesh daemon` over a fresh copy of GM-290's fixture,
    /// with `[plugin] memoryLimitMb` set in the project's *real*
    /// `~/.g-mesh/projects/<hash>/config.toml` before the daemon ever reads
    /// it (`daemon::run` reads config exactly once, at startup - see
    /// `core/src/daemon/mod.rs`). No `G_MESH_PLUGIN_ROOTS_OVERRIDE`: the
    /// default checkout-layout discovery (`daemon::manifest::bundled_roots`,
    /// `CARGO_MANIFEST_DIR/../plugins`) already finds the real
    /// `plugins/rust/plugin.toml` in this repository, which is the whole
    /// point - this test needs the plugin exactly as a user gets it.
    fn spawn(memory_limit_mb: Option<u64>) -> Self {
        let dir = tempfile::tempdir().expect("failed to create a project root");
        copy_dir(Path::new(FIXTURE), dir.path());

        config::write_project_config(
            dir.path(),
            &ProjectConfig {
                plugin: PluginConfig { idle_timeout_minutes: 60, memory_limit_mb },
                ..Default::default()
            },
        )
        .expect("failed to write the project's config.toml");

        let daemon = std::process::Command::new(BIN)
            .arg("daemon")
            .arg("--project-root")
            .arg(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the daemon");

        Self { dir, daemon }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> std::path::PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory")
    }
}

impl Drop for DaemonProject {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// Polls until "rust" has a suspension marker on disk
/// (`daemon::registry::discovered_suspended_markers`, the same file
/// `cli::status` reads) or `deadline` passes.
fn wait_for_rust_suspended(state_dir: &Path, deadline: Instant) {
    loop {
        let markers = registry::discovered_suspended_markers(state_dir);
        if markers.iter().any(|(language, _)| language == "rust") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rust was never suspended within the deadline; markers so far: {markers:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// The real acceptance criterion: with a real daemon, a real rust-analyzer,
/// and `[plugin] memoryLimitMb` low enough that GM-290's fixture reliably
/// crosses it (the same `LOW_LIMIT_MB` the direct-supervisor tests above
/// use), the `initialize` response's `instructions` string - what a real MCP
/// client actually reads, once per session - names the same receiver-call-gap
/// state the index's own `language_state.semanticPassAt` records for rust,
/// whichever side of the lock race (this file's module doc) this particular
/// run landed on. Real production timing throughout: no `G_MESH_PLUGIN_IDLE_MS`
/// override, so the supervise loop's tick is the production default (30s,
/// `IdleTimeouts::tick` at `idleTimeoutMinutes = 60`).
#[tokio::test]
async fn the_generated_mcp_instructions_reflect_a_real_suspended_rust() {
    let load = std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    eprintln!("GM-291: machine state before this test's real daemon+rust-analyzer run: {load:?}");

    let project = DaemonProject::spawn(Some(LOW_LIMIT_MB));

    common::wait_until_indexed(project.root());
    eprintln!(
        "GM-291: structural bulk index complete, waiting for the real memory-limit tick to suspend rust"
    );

    let deadline = Instant::now() + TIMEOUT;
    wait_for_rust_suspended(&project.state_dir(), deadline);
    eprintln!("GM-291: rust suspended at {:?} after test start", Instant::now());

    // Ground truth, read directly off the same index.db the daemon itself
    // writes to - a second, independent reader, the same way
    // `cli::status`/`cli::stop` are documented to read daemon state (off
    // disk, never by asking the live process a question).
    let db = project.state_dir().join("index.db");
    let conn = Connection::open(&db).expect("failed to open the daemon's own index.db");
    let present = schema::present_languages_with_semantic_state(&conn)
        .expect("failed to read the present-language/semantic-state pairs");
    assert_eq!(present.len(), 1, "only rust is present in this fixture: {present:?}");
    let (language, semantic_pass_done) = present[0].clone();
    assert_eq!(language, "rust");
    eprintln!(
        "GM-291: language_state.semanticPassAt is {} in the real daemon's own index",
        if semantic_pass_done { "SET" } else { "NULL" }
    );

    // The real MCP protocol: `g-mesh mcp-shim` finds the already-running
    // daemon above by `--project-root`'s cwd and proxies to it - no second
    // cold start.
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    let info = client.peer_info().expect("server never reported its info");
    let text = info.instructions.clone().unwrap_or_default();
    client.cancel().await.expect("failed to shut the client down");

    assert!(!text.is_empty(), "get_info must carry non-empty instructions once a language is present");

    if semantic_pass_done {
        // GM-385: a completed pass NARROWS this gap, it does not close it.
        // rust-analyzer resolves `x.area()` against the receiver's declared
        // or inferred type - `&dyn Shape`, `<S: Shape>` - so the call lands
        // on `Shape::area` and the *override's* own caller page still
        // under-reports. This arm used to assert "One real gap", i.e. the
        // gap rendered as closed, which was the belief GM-385 measured and
        // disproved across all four plugins. What a resolved tier changes is
        // the wording, not the existence of the gap, so the assertion is
        // still two-sided: the static-receiver form must be there and the
        // open form must not.
        assert!(
            text.contains("binds to the receiver's declared or inferred type"),
            "rust's semantic pass completed and recorded before suspension caught it (the \
             common-case race - see this file's module doc), so the real MCP `initialize` \
             response must render its receiver-call gap in the STATIC-RECEIVER form:\n{text}"
        );
        assert!(
            !text.contains("produces no edge by design"),
            "rust's semantic pass completed, so the gap must not still be rendered in its \
             OPEN form - that wording is for a tier that never ran:\n{text}"
        );
    } else {
        assert!(
            text.contains("produces no edge by design"),
            "rust's semantic pass had not completed when suspension caught it, so the real MCP \
             `initialize` response must still list its receiver-call gap:\n{text}"
        );
    }
}
