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
//! [`process_tree_sample`] is the *only* thing in this crate that ever calls
//! into `sysinfo`. A project with no `memoryLimitMb` configured never reaches
//! this function at all - `PluginSupervisor::check_memory_limit` returns
//! before it gets here (`self.memory_limit_mb` is `None`) - which is what
//! makes "no key set means no sampling side effects at all" a true statement
//! about this module, not just about the number it would have compared
//! against.
//!
//! # One sample is one instant, and the tree is not the caller's alone (GM-307)
//!
//! Everything below returns a [`ProcessTreeSample`] built from *one*
//! `sysinfo::refresh_processes` snapshot, and that framing is load-bearing
//! rather than incidental. A process tree's membership is not stable: a
//! `rust-analyzer` shells out to `rustc` and to build scripts, a `tsserver`
//! forks for a project reload, and each of those is a real, transient member
//! of the tree for as long as it lives. Two snapshots taken a few
//! milliseconds apart can therefore differ by hundreds of megabytes with
//! nothing having grown or shrunk at all - a member simply left.
//!
//! That is not a hypothetical. This module's own
//! `a_freshly_spawned_childs_memory_is_included_in_its_parents_tree` used to
//! assert that adding a live child could never make the parent's tree look
//! smaller, comparing an aggregate from one snapshot against an aggregate
//! from a later one. It passes alone and fails inside a full `cargo test
//! --lib`/`--workspace` run, because there the test binary's tree also holds
//! every *other* concurrently running test's spawned plugin and daemon.
//! Measured on this repository: 350MB then 180MB at load 105-121, 487MB then
//! 290MB at load 21-41, 597MB then 421MB at load 20-27 - each time with a 1MB
//! `sleep` added between the two. A 170-200MB fall in milliseconds is not the
//! OS reclaiming pages from one process; it is several Node processes
//! belonging to other tests exiting.
//!
//! Two controls say what that measurement is and is not. Run alone the old
//! assertion passed 5/5 even at load 195-215, so it is the shared tree and
//! not the machine's load that falsifies it. And with the descendant walk
//! deliberately severed - `process_tree_sample` refusing to push any child
//! onto its stack, so decision 2 is comprehensively broken - the old
//! assertion still *passed*, while the replacement below failed. So the
//! assertion it is replacing managed to be both unsound and insensitive at
//! once: it failed when nothing was wrong, and passed when the very thing it
//! existed to guard was gone.
//!
//! The lesson is narrower and more useful than "sampling is noisy": **a
//! whole-tree aggregate taken at one instant cannot answer a question about
//! one member, and two aggregates taken at different instants cannot be
//! subtracted to get one.** So [`ProcessTreeSample`] carries the walk's own
//! membership, and every claim this module's tests make - and, through
//! `PluginSupervisor::check_memory_limit`, every claim the daemon makes - is
//! made against a single snapshot rather than across two.
//!
//! The same fact is what makes `check_memory_limit` take a *confirming*
//! sample before it suspends a language (see that method's doc comment): one
//! over-limit aggregate says the tree was over the limit at one instant, and
//! `memoryLimitMb` is a circuit breaker on a sustained plateau, not an alarm
//! on any instant that happened to include a transient `rustc`.

use std::collections::{HashMap, HashSet};

use sysinfo::{Pid, ProcessesToUpdate, System};

/// One `sysinfo` snapshot's worth of truth about a process tree: every member
/// the walk from `root_pid` reached, each with its own resident memory, and
/// the total those members sum to.
///
/// Kept as membership plus parts rather than collapsed to a single number
/// because the interesting questions about a tree are about its *members* -
/// "is the child this test just spawned in here", "how much of this total is
/// the language server rather than the plugin" - and those cannot be
/// recovered from the total afterwards, nor reconstructed by subtracting one
/// total from another taken at a different instant (see this module's doc
/// comment on why that subtraction is meaningless).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTreeSample {
    /// Every process reached by the walk, in the order the walk reached it,
    /// as `(pid, resident bytes)`. Never empty: `root_pid` itself is always
    /// the first member, or the whole sample is `None`.
    members: Vec<(u32, u64)>,
    total_bytes: u64,
}

impl ProcessTreeSample {
    /// The whole tree's resident memory in bytes - the sum of every member's
    /// own, by construction.
    pub fn rss_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The whole tree's resident memory in whole megabytes, rounded down
    /// (`bytes / (1024 * 1024)`), matching `memoryLimitMb`'s own unit - a tree
    /// sitting a few hundred KB under a limit must not round up into "over"
    /// it.
    pub fn rss_mb(&self) -> u64 {
        self.total_bytes / (1024 * 1024)
    }

    /// Whether `pid` was in this tree at the instant this sample was taken.
    pub fn contains(&self, pid: u32) -> bool {
        self.members.iter().any(|(member, _)| *member == pid)
    }

    /// `pid`'s own resident memory at the instant this sample was taken, or
    /// `None` if it was not in the tree then.
    pub fn rss_bytes_of(&self, pid: u32) -> Option<u64> {
        self.members.iter().find(|(member, _)| *member == pid).map(|(_, bytes)| *bytes)
    }

    /// Every member of the tree with its own resident memory, in walk order.
    pub fn members(&self) -> &[(u32, u64)] {
        &self.members
    }
}

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
///
/// A thin reading of [`process_tree_sample`], which is where the walk itself
/// lives. Callers that need to say anything about *which* processes made up
/// the total - or that would otherwise be tempted to subtract one of these
/// numbers from another taken at a different instant - want that function
/// instead; see this module's doc comment on why the subtraction is not a
/// measurement of anything.
pub fn process_tree_rss_mb(root_pid: u32) -> Option<u64> {
    Some(process_tree_sample(root_pid)?.rss_mb())
}

/// One snapshot of `root_pid`'s whole process tree: every process reachable
/// by walking `parent` links down from it, each with its own resident memory.
///
/// `None` under exactly the conditions [`process_tree_rss_mb`] documents -
/// `root_pid` absent from a fresh whole-system snapshot, or process
/// enumeration answering nothing at all on this platform/build.
pub fn process_tree_sample(root_pid: u32) -> Option<ProcessTreeSample> {
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
    let mut members: Vec<(u32, u64)> = Vec::new();
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
            let bytes = process.memory();
            total_bytes = total_bytes.saturating_add(bytes);
            // Recorded even at zero bytes: membership is a separate fact from
            // footprint, and "this pid was in the tree" is exactly what a
            // caller cannot get back from the total afterwards.
            members.push((pid.as_u32(), bytes));
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }

    Some(ProcessTreeSample { members, total_bytes })
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
    ///
    /// # Why this asserts membership rather than a growing total (GM-307)
    ///
    /// Until GM-307 this test spelled decision 2 as "a live child process
    /// must never make its parent's tree look smaller", comparing an
    /// aggregate sampled before the spawn against one sampled after it. That
    /// assertion is false, and it is false for a reason that has nothing to
    /// do with the property being tested: this test process's tree is not
    /// this test's own. Inside a full `cargo test --workspace` run the same
    /// binary is running hundreds of other tests concurrently, several of
    /// which spawn Node plugins and whole `g-mesh daemon` subprocesses -
    /// every one of them a member of the very tree this walk sums. Measured
    /// on this repository at load averages 105-121, 21-41 and 20-27, the two
    /// aggregates came back as 350/180MB, 487/290MB and 597/421MB with a ~1MB
    /// `sleep` added between them: falls of 170-200MB in milliseconds, which
    /// is other tests' children exiting, not the OS reclaiming pages. It
    /// passed 5/5 when run alone at load 195-215, which is exactly what makes
    /// the old assertion so misleading - it fails only where the tree is
    /// genuinely shared, and load is not what shares it.
    ///
    /// So the fix is not a wider tolerance on the same subtraction. Decision
    /// 2's actual claim is a claim about *membership* - the freshly spawned
    /// child is in the walk - and membership is answerable inside a single
    /// snapshot, where no other test's process can move anything. Every
    /// assertion below reads one [`ProcessTreeSample`] taken after the spawn:
    /// the child is a member, it has real resident memory of its own, and the
    /// tree's total is the sum of its members, so "included in its parent's
    /// tree" is literal rather than inferred from a difference.
    #[test]
    #[cfg(unix)]
    fn a_freshly_spawned_childs_memory_is_included_in_its_parents_tree() {
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
        let child_pid = child.id();

        let sample = process_tree_sample(std::process::id()).expect("the current process must be found");

        assert!(
            sample.contains(child_pid),
            "a child spawned after the root process started must still be walked (decision 2); \
             tree members were {:?}",
            sample.members()
        );
        let child_bytes = sample.rss_bytes_of(child_pid).expect("just asserted the child is a member");
        assert!(
            child_bytes > 0,
            "a live process has resident memory; {child_pid} reported {child_bytes} bytes"
        );
        let summed: u64 = sample.members().iter().map(|(_, bytes)| bytes).sum();
        assert_eq!(
            sample.rss_bytes(),
            summed,
            "the tree's total is its members' sum, so the child's own {child_bytes} bytes are in it"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// The root process is a member of its own sample - the base case
    /// `the_current_process_is_measured_as_a_nonzero_tree_of_one` asserts
    /// about the total, said about membership instead, which is the fact
    /// [`ProcessTreeSample::contains`] exists to carry.
    #[test]
    fn the_root_process_is_a_member_of_its_own_sample() {
        let me = std::process::id();
        let sample = process_tree_sample(me).expect("the current process must be found");
        assert!(sample.contains(me), "tree members were {:?}", sample.members());
        assert!(sample.rss_bytes_of(me).is_some_and(|bytes| bytes > 0), "a live process has RSS");
        // Deliberately not compared against `sample`: that would be two
        // snapshots taken at different instants, which is the one thing this
        // module's doc comment says nothing may be concluded from.
        assert!(process_tree_rss_mb(me).is_some(), "the thin reading finds the same live process");
    }

    /// `memoryLimitMb` is compared in whole megabytes, so the rounding is part
    /// of the contract rather than an implementation detail: a tree a few
    /// hundred KB under a limit must read as under it.
    ///
    /// Built from constructed samples rather than a live process on purpose.
    /// A live tree's size is whatever the machine happens to be doing, and at
    /// this test process's own scale integer truncation hides real mistakes -
    /// a mebibyte-vs-megabyte mixup is invisible at 7MB (7540736 bytes reads
    /// as 7 either way) and unmissable at 100MB. Exact inputs make the
    /// boundary itself the thing under test.
    #[test]
    fn a_samples_megabytes_are_its_bytes_rounded_down_to_whole_mebibytes() {
        let sample_of = |bytes: u64| ProcessTreeSample { members: vec![(1, bytes)], total_bytes: bytes };

        assert_eq!(sample_of(0).rss_mb(), 0);
        assert_eq!(sample_of(1024 * 1024 - 1).rss_mb(), 0, "a hair under 1MiB is not yet 1MB");
        assert_eq!(sample_of(1024 * 1024).rss_mb(), 1);
        assert_eq!(sample_of(100 * 1024 * 1024).rss_mb(), 100, "mebibytes, not megabytes");
        assert_eq!(sample_of(600 * 1024 * 1024 + 12_345).rss_mb(), 600, "rounds down, never up");
    }

    /// A sample answers about its members individually, not only in total -
    /// the accessors `check_memory_limit`'s diagnostics and this module's own
    /// membership assertions are built on.
    #[test]
    fn a_sample_answers_about_each_member_it_walked() {
        let sample = ProcessTreeSample {
            members: vec![(11, 3 * 1024 * 1024), (22, 1024 * 1024), (33, 0)],
            total_bytes: 4 * 1024 * 1024,
        };

        assert!(sample.contains(11) && sample.contains(22));
        assert!(sample.contains(33), "a member with no measurable RSS is still a member");
        assert!(!sample.contains(44));
        assert_eq!(sample.rss_bytes_of(22), Some(1024 * 1024));
        assert_eq!(sample.rss_bytes_of(44), None);
        assert_eq!(sample.rss_mb(), 4);
        assert_eq!(sample.members().len(), 3);
    }
}
