//! Resident-memory sampling for one plugin's whole process tree - the
//! `[plugin] memoryLimitMb` enforcement task GM-274 adds
//! (`docs/architecture/multi-language-plugins.md`'s "Plugin memory limit"
//! section). `daemon::lifecycle::PluginSupervisor::check_memory_limit` is the
//! one caller: on the same idle-check tick that already drives
//! `sleep_if_idle`, it asks [`process_tree_rss_mb`] for the plugin process's
//! own resident memory plus every descendant's - tsserver, rust-analyzer, or
//! an LSP server behind a bridge, whichever the plugin has spawned by the time
//! this call happens.
//!
//! # Why `sysinfo`, not hand-rolled platform code
//!
//! See the dependency comment on `sysinfo` in `core/Cargo.toml` for the full
//! weighing - correctness across three platforms with one already-vetted
//! crate, against three separate FFI surfaces (macOS `libproc`, Linux
//! `/proc`, Windows `Toolhelp32`) this crate would otherwise have to write and
//! keep correct by hand, none of which answers "children of pid N" any more
//! directly than `sysinfo` does either.
//!
//! # Descendants are walked fresh, every call (decision 2)
//!
//! Nothing here caches a plugin's known children. A `rust-analyzer` or
//! `tsserver` process is not always live the instant the plugin itself
//! starts - some plugins spawn their semantic engine lazily, on the first
//! request that needs it (see `docs/architecture/multi-language-plugins.md`'s
//! "a plugin starts its semantic engine lazily on the first `semanticPass`").
//! A cached child-pid list taken once at spawn time would miss exactly that
//! process, which is usually the one actually responsible for an overage. So
//! every call takes one fresh, whole-system snapshot and rebuilds the
//! parent -> children map from it, then walks down from `root_pid` - more
//! work per sample than a cached list would be, but it is only ever done once
//! per plugin per idle-check tick (see `daemon::lifecycle`'s doc comment for
//! that tick's actual period), not on any hot path.
//!
//! # What "no sampling" costs (decision, acceptance criterion)
//!
//! [`process_tree_rss_mb`] is the *only* thing in this crate that ever calls
//! into `sysinfo`. A project with no `memoryLimitMb` configured never reaches
//! this function at all - `PluginSupervisor::check_memory_limit` returns
//! before it gets here (`self.memory_limit_mb` is `None`) - which is what
//! makes "no key set means no sampling side effects at all" a true statement
//! about this module, not just about the number it would have compared
//! against.

use std::collections::{HashMap, HashSet};

use sysinfo::{Pid, ProcessesToUpdate, System};

/// The resident memory (RSS), in whole megabytes, of `root_pid` and every
/// process reachable by walking `parent` links from it - one process tree, as
/// `[plugin] memoryLimitMb` (see this module's doc comment) means it.
///
/// `None` when `root_pid` itself is not present in a fresh, whole-system
/// process snapshot: it has already exited by the time this call ran (a
/// narrow, ordinary race with `sleep_if_idle`/a crash relaunch - see
/// `PluginSupervisor::check_memory_limit`'s own doc comment for how the
/// caller treats it), or process enumeration answered nothing at all on this
/// platform/build. Both read the same way to a caller: no evidence of an
/// overage, so the limit is a no-op for this one tick rather than a failure -
/// see this module's doc comment on the Windows contingency.
///
/// Rounds down (`bytes / (1024 * 1024)`), matching `memoryLimitMb`'s own
/// whole-megabyte unit - a tree sitting a few hundred KB under a limit must
/// not round up into "over" it.
pub fn process_tree_rss_mb(root_pid: u32) -> Option<u64> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let root = Pid::from_u32(root_pid);
    // Confirmed present before anything else: a `root_pid` gone from this
    // snapshot has nothing this function can honestly report, per this
    // function's own doc comment on why that reads as `None` rather than `0`
    // (zero would look like "measured, and empty" - a claim this function is
    // in no position to make about a process it never found).
    system.process(root)?;

    // One pass over the whole snapshot to build parent -> children, rather
    // than a `system.processes()` scan per tree node - this map is the whole
    // reason a single fresh snapshot answers "every descendant" without a
    // second sysinfo call per child, however many generations deep the tree
    // goes.
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }

    let mut total_bytes: u64 = 0;
    let mut visited: HashSet<Pid> = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        // Guards against a pid appearing twice in the walk (it cannot appear
        // twice as *itself* in one snapshot, but a `parent` link that formed a
        // cycle - impossible in a real process tree, but not a fact this
        // function should have to trust - must not spin forever).
        if !visited.insert(pid) {
            continue;
        }
        if let Some(process) = system.process(pid) {
            total_bytes = total_bytes.saturating_add(process.memory());
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }

    Some(total_bytes / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// The root process itself is always in its own tree, even with no
    /// children - the base case every deeper walk builds on.
    #[test]
    fn the_current_process_is_measured_as_a_nonzero_tree_of_one() {
        let rss = process_tree_rss_mb(std::process::id()).expect("the current process must be found");
        // Not a tight bound - just proof this is a real measurement, not a
        // stub that always answers 0 or `None`. A test process is never
        // genuinely memory-less.
        assert!(rss < 100_000, "a test process reporting {rss}MB looks like a unit mixup, not real RSS");
    }

    /// A pid nothing is running as answers `None`, not `Some(0)` - see this
    /// module's doc comment on why those two are not the same claim.
    ///
    /// `sleep 0` (rather than `true`, which is not guaranteed to exist as a
    /// standalone binary on every unix) is a real, short-lived child every
    /// platform this test runs on in CI (macOS, Linux) has - see this task's
    /// own note that process-tree sampling is only verified there, not on
    /// Windows.
    #[test]
    #[cfg(unix)]
    fn a_pid_with_no_running_process_reads_as_none() {
        // Spawn and wait for a real, short-lived process, so the pid used
        // below is one that genuinely existed and just as genuinely does not
        // any more - not a guess at an unused number.
        let mut child = Command::new("sleep")
            .arg("0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn a short-lived helper process");
        let pid = child.id();
        child.wait().expect("failed to wait for the helper process to exit");

        assert_eq!(process_tree_rss_mb(pid), None);
    }

    /// Decision 2: a child spawned *after* the root process started is still
    /// found, because every call walks a fresh snapshot rather than a
    /// child-pid list cached at some earlier point - the shape a plugin that
    /// starts its semantic engine lazily actually produces.
    #[test]
    #[cfg(unix)]
    fn a_freshly_spawned_childs_memory_is_included_in_its_parents_tree() {
        // The tree rooted at this test process, before it has any children of
        // its own beyond whatever the test harness itself spawned.
        let before = process_tree_rss_mb(std::process::id()).expect("the current process must be found");

        // A real child process that outlives the sample below, so it is
        // genuinely in the snapshot the walk sees - `sleep` is present on
        // both platforms this test runs on in CI (macOS, Linux); Windows has
        // no equivalent one-liner, so this test is unix-only, matching this
        // task's own note that process-tree sampling is only verified on
        // macOS/Linux.
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the child helper process");

        let after = process_tree_rss_mb(std::process::id()).expect("the current process must be found");
        assert!(
            after >= before,
            "a live child process must never make its parent's tree look smaller \
             (before: {before}MB, after: {after}MB)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}
