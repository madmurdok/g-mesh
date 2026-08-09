//! Coverage for task 39: an *unexpected* plugin process exit (a panic, an
//! OOM kill) must not leave indexing silently broken. Distinct from a
//! deliberate idle sleep (task 38's territory, a cooperative shutdown the
//! daemon itself decides on), this exercises the daemon finding its plugin
//! gone the hard way - mid request - and recovering from it on its own.
//!
//! Exercises `daemon::plugin::PluginProcess` directly (the real Node JS/TS
//! plugin, same build `plugin_bridge.rs` depends on via `core/build.rs`)
//! rather than the full daemon binary + filesystem watcher: the acceptance
//! criterion only cares that "a request that needs [the plugin]" recovers
//! transparently, and driving `apply_file_change` calls directly is a
//! deterministic way to control exactly when the crash happens relative to
//! those requests, without racing a real `notify` filesystem watcher.

use std::fs;
use std::sync::Mutex;

use g_mesh::daemon::is_process_alive;
use g_mesh::daemon::plugin::{bundled_manifest, PluginProcess};
use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::storage::schema;
use rusqlite::Connection;

fn setup_conn() -> Mutex<Connection> {
    let conn = Connection::open_in_memory().expect("failed to open an in-memory index");
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).expect("failed to apply the schema");
    Mutex::new(conn)
}

fn node_named(conn: &Mutex<Connection>, name: &str) -> i64 {
    conn.lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1", [name], |row| row.get(0))
        .unwrap()
}

/// Kills `pid` the way an OOM-killer or a crash would - not through
/// `daemon::stop` or any cooperative shutdown path.
///
/// Deliberately does not wait around for `is_process_alive(pid)` to go
/// false afterwards: a killed process a caller hasn't reaped yet is a
/// zombie, and a zombie still satisfies `kill(pid, 0)` (the check
/// `is_process_alive` makes) - it only disappears from that check once
/// something calls `waitpid` on it, which here is `PluginProcess`'s own
/// `try_wait`-based crash detection inside the very call this test makes
/// next. Waiting on `is_process_alive` here would just hang until this
/// test's own timeout. SIGKILL closes the target's stdin pipe on delivery,
/// well before this function returns, so the next request already exercises
/// the real crash path without any extra synchronization.
fn kill_out_of_band(pid: u32) {
    // SAFETY: `kill` only signals a pid this test itself just read off a
    // `Child` it owns; no pointers are involved.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[test]
fn a_killed_plugin_process_is_transparently_relaunched_and_its_pending_queue_replayed() {
    let project = tempfile::tempdir().expect("failed to create a temp project root");
    let root = project.path();

    fs::write(root.join("alpha.ts"), "export function alpha() {}\n").expect("failed to write alpha.ts");
    fs::write(root.join("beta.ts"), "export function beta() {}\n").expect("failed to write beta.ts");

    let plugin = PluginProcess::spawn(root, &bundled_manifest(), root.join("plugin.pid"))
        .expect("failed to spawn the JS/TS plugin");
    let conn = setup_conn();

    // Establish the happy path first, so a failure below can only be about
    // crash recovery, not about the plugin bridge in general.
    plugin
        .apply_file_change(&conn, "alpha.ts", &EmbeddingPipeline::disabled())
        .expect("the first, uneventful file change must apply cleanly");
    assert_eq!(node_named(&conn, "alpha"), 1, "alpha.ts's diff must be committed before anything is killed");

    let original_pid = plugin.pid();
    kill_out_of_band(original_pid);

    // The next request that needs the plugin - here, the next file change -
    // must recover on its own: a fresh process spawned, and the change
    // (which is exactly what was left in the pending queue when the old
    // process died) replayed against it.
    plugin
        .apply_file_change(&conn, "beta.ts", &EmbeddingPipeline::disabled())
        .expect("apply_file_change must transparently recover from a plugin killed out-of-band");

    assert_eq!(
        node_named(&conn, "beta"),
        1,
        "the file change that was pending when the plugin died must still end up committed"
    );

    let relaunched_pid = plugin.pid();
    assert_ne!(relaunched_pid, original_pid, "a fresh plugin process must have been spawned");
    assert!(is_process_alive(relaunched_pid), "the freshly spawned plugin process must be alive");

    // The daemon must keep working normally afterwards, not just survive one
    // more call - a third, ordinary change against the recovered process.
    fs::write(root.join("gamma.ts"), "export function gamma() {}\n").expect("failed to write gamma.ts");
    plugin
        .apply_file_change(&conn, "gamma.ts", &EmbeddingPipeline::disabled())
        .expect("the recovered plugin process must keep serving ordinary requests");
    assert_eq!(node_named(&conn, "gamma"), 1);
}
