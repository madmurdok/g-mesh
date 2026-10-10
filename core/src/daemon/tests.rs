/// The property the whole of GM-242 turns on: a reader that waits for the
/// file to exist can never then read nothing out of it. Written as a loop
/// because a single pass would pass just as well against the old
/// truncate-then-write, which is only observable in the gap.
#[test]
fn a_pid_file_is_never_observable_empty_between_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.pid");

    for pid in 1..=50u32 {
        write_pid_file(&path, pid);
        assert_eq!(read_pid_file(&path), Some(pid), "rewriting a pid file must never leave it unreadable");
    }
}

/// The rename has to actually happen. A helper that wrote the temporary
/// and failed to move it would satisfy the test above on the first
/// iteration and leave litter behind for ever after.
#[test]
fn writing_a_pid_file_leaves_no_temporary_behind() {
    let dir = tempfile::tempdir().unwrap();
    write_pid_file(&dir.path().join("daemon.pid"), 4321);

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "daemon.pid")
        .collect();
    assert!(leftovers.is_empty(), "the temporary must be renamed, not left: {leftovers:?}");
}
use super::*;
use crate::storage::index_store::IndexStore;

/// Forces the exact shape of task 165's race deterministically, instead
/// of relying on a real `kill -9`'s unpredictable teardown delay the way
/// the integration tests that first surfaced it incidentally do.
///
/// `flock` is scoped to the *open file description*, not the process
/// that holds it, so two independent `open()` calls from the very same
/// process contend exactly as two different processes would - which is
/// what lets a second thread stand in for a just-killed predecessor: it
/// takes the lock, holds it for a short, fixed window, then releases it,
/// the same shape as the kernel finishing a killed daemon's fd teardown
/// slightly after the replacement's first attempt. Before the retry loop
/// this exercises, that first attempt alone would see `WouldBlock` and
/// `acquire_singleton_lock` would give up immediately - exactly bug 165.
#[test]
fn a_lock_released_shortly_after_the_first_attempt_is_still_acquired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DAEMON_LOCK_FILE);

    let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
    holder.lock().expect("failed to take the stand-in lock");

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let hold_for = SINGLETON_LOCK_RETRY_INTERVAL * 2;
    let releaser = thread::spawn(move || {
        ready_tx.send(()).unwrap();
        thread::sleep(hold_for);
        drop(holder);
    });

    // Waits for the holder thread to have the lock before racing it, so
    // this cannot flake the other way - `acquire_singleton_lock` winning
    // a lock nobody was contending yet.
    ready_rx.recv().unwrap();
    let acquired =
        acquire_singleton_lock(dir.path()).expect("acquire_singleton_lock must not error on contention");
    releaser.join().unwrap();

    assert!(
        acquired.is_some(),
        "a lock released well within the retry budget must still be acquired, not treated as a live incumbent"
    );
}

/// The other half of the acceptance bar, without which the fix above
/// could regress into "the singleton lock no longer excludes anyone":
/// a lock held for the whole retry budget - a genuinely running
/// incumbent, not a predecessor mid-teardown - must still lose.
#[test]
fn a_lock_held_past_the_retry_budget_is_not_acquired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DAEMON_LOCK_FILE);

    let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
    holder.lock().expect("failed to take the stand-in lock");

    let acquired =
        acquire_singleton_lock(dir.path()).expect("acquire_singleton_lock must not error on contention");

    assert!(
        acquired.is_none(),
        "a lock held by a live incumbent for the whole retry budget must not be acquired"
    );
    drop(holder);
}

/// Takes the daemon lock the way a real daemon does, and hands back both
/// the file (dropping it releases the lock) and the directory it lives in.
/// `flock` is scoped to the open file description rather than to the
/// process, so a lock taken here contends with `acquire_singleton_lock`
/// exactly as another process's would - the same property task 165's
/// tests above already rely on.
fn held_lock(dir: &Path) -> File {
    acquire_singleton_lock(dir)
        .expect("taking the lock must not error")
        .expect("nothing else holds this fixture's lock")
}

/// A project nobody is serving and nobody has locked: a bootstrap may go
/// ahead, and nothing is there to evict.
#[test]
fn an_unheld_lock_reads_as_free() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Free);

    // Including once a daemon has held it and let it go - the pid it
    // recorded while serving must not outlive the lock itself, or a
    // recycled pid would read as a wedged incumbent.
    let lock = held_lock(dir.path());
    record_serving_owner(dir.path());
    drop(lock);
    assert_eq!(
        inspect_daemon_lock_in(dir.path(), false).unwrap(),
        DaemonLock::Free,
        "a released lock is free however recently its holder was serving"
    );
}

/// A daemon between taking the lock and binding its socket. Indistinguishable
/// from a wedged one by the lock alone, which is exactly why the daemon
/// records itself as serving separately - and why this must not be
/// evictable.
#[test]
fn a_lock_held_by_a_daemon_that_has_not_begun_serving_reads_as_starting() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = held_lock(dir.path());

    assert_eq!(
        inspect_daemon_lock_in(dir.path(), false).unwrap(),
        DaemonLock::Starting,
        "a holder that never published itself as serving is starting up, not wedged"
    );
}

/// Task 184's state, forced deterministically: the lock is held, its
/// holder published itself as serving and is alive, and nothing is
/// listening. That combination - and only that one - is a wedge.
#[test]
fn a_live_holder_that_served_and_stopped_listening_reads_as_wedged() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = held_lock(dir.path());
    record_serving_owner(dir.path());

    assert_eq!(
        inspect_daemon_lock_in(dir.path(), false).unwrap(),
        DaemonLock::Wedged { pid: std::process::id() },
        "a live holder that was serving and answers nothing is the wedge, and it must be \
         named by pid so `stop` and the shim can act on it"
    );
}

/// The guard that keeps the eviction path from ever taking a session away
/// from someone: a holder that answers is healthy no matter what else is
/// true of it.
#[test]
fn a_holder_that_is_still_answering_is_never_wedged() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = held_lock(dir.path());
    record_serving_owner(dir.path());

    assert_eq!(
        inspect_daemon_lock_in(dir.path(), true).unwrap(),
        DaemonLock::Serving,
        "a daemon answering on its socket must never be a candidate for eviction"
    );
}

/// The record has to survive one daemon replacing another: a fresh holder
/// starts out as `Starting`, not as the wedged incumbent it took over
/// from.
#[test]
fn a_new_holder_does_not_inherit_its_predecessors_claim_to_be_serving() {
    let dir = tempfile::tempdir().unwrap();
    let first = held_lock(dir.path());
    record_serving_owner(dir.path());
    assert_eq!(serving_owner_in(dir.path()), Some(std::process::id()));
    drop(first);

    let _second = held_lock(dir.path());
    assert_eq!(serving_owner_in(dir.path()), None, "taking the lock clears the record");
    assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Starting);
}

/// A lock file from a build that predates the record reads as `Starting` -
/// nothing is claimed about it, so nothing may be done to it. The safe
/// direction: an old daemon is left alone rather than signalled on a guess.
#[test]
fn a_lock_file_with_no_record_in_it_is_never_treated_as_wedged() {
    let dir = tempfile::tempdir().unwrap();
    // The *lock* file: this holds the lock to make the state read as held,
    // and asserts that a holder with no serving record beside it is
    // starting up rather than wedged.
    let path = dir.path().join(DAEMON_LOCK_FILE);
    let holder = File::options().create(true).write(true).truncate(false).open(&path).unwrap();
    holder.lock().unwrap();

    assert_eq!(serving_owner_in(dir.path()), None);
    assert_eq!(inspect_daemon_lock_in(dir.path(), false).unwrap(), DaemonLock::Starting);
}

/// The record is what decides whether a live process gets signalled, so a
/// half-written one must never parse. Asserted over the shapes a reader
/// could catch mid-rewrite - and over a stray one, since a lock file is
/// world-readable and this is the one pid the daemon acts on rather than
/// merely reports.
#[test]
fn a_record_without_its_terminating_newline_is_not_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DAEMON_SERVING_FILE);

    for partial in ["", "1", "12", "1234"] {
        fs::write(&path, partial).unwrap();
        assert_eq!(
            serving_owner_in(dir.path()),
            None,
            "{partial:?} is not a finished record and must not name anything to signal"
        );
    }

    fs::write(&path, "1234\n").unwrap();
    assert_eq!(serving_owner_in(dir.path()), Some(1234));
}

/// A registry over `languages`, each installed as a fake plugin
/// (`test_plugin`) claiming one extension named after it (`python` ->
/// `.python-src`), plus the fixture directories backing it - dropping
/// them would delete the plugin/project directories before a test using
/// the registry is done with them. Mirrors `daemon::registry`'s own
/// `registry_over` test fixture, which is private to that module and so
/// not reusable from here directly.
fn registry_over(languages: &[&str]) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    registry_with(|plugins| {
        languages
            .iter()
            .map(|language| {
                let extension = format!(".{language}-src");
                test_plugin::install(plugins, language, &[extension.as_str()])
            })
            .collect()
    })
}

/// [`registry_over`], with the plugin directories written by `install`
/// (given the plugin root) before discovery.
fn registry_with(
    install: impl FnOnce(&Path) -> Vec<PathBuf>,
) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");

    let dirs = install(plugins.path());

    let discovered =
        manifest::discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
    let root = project.path().canonicalize().expect("failed to canonicalize the fixture project root");
    let state_dir =
        project_dir(project.path()).expect("failed to resolve the fixture project's state directory");
    fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
    let registry = PluginRegistry::new(
        &root,
        state_dir,
        discovered,
        None,
        None,
        Arc::new(crate::embedding::EmbeddingPipeline::disabled()),
    );
    (project, plugins, dirs, registry)
}

/// Runs `watch_and_route_once` a handful of times up front, the same way
/// `watcher::mod`'s own tests drain a fresh watcher before asserting on
/// it - macOS FSEvents can replay startup noise (a creation event for
/// the watched root itself) shortly after the watch begins, and letting
/// that settle first keeps it from being mistaken for the burst under
/// test. `relative_wire_path` already filters root-directory events out
/// before they would ever reach `registry`, so this exists for timing
/// hygiene rather than because noise could itself register as a round
/// trip.
fn drain_startup_noise(
    watcher: &ProjectWatcher,
    debouncer: &mut Debouncer,
    root: &Path,
    conn: &IndexStore,
    registry: &PluginRegistry,
) {
    for _ in 0..2 {
        watch_and_route_once(watcher, debouncer, root, conn, registry);
    }
}

/// Pumps `watch_and_route_once` for a bounded window comfortably longer
/// than [`DEBOUNCE_WINDOW`] - long enough to both drain whatever raw
/// events a preceding burst queued and let their debounce window elapse
/// and fire, without looping forever the way `run`'s real watcher thread
/// does.
fn pump_until_settled(
    watcher: &ProjectWatcher,
    debouncer: &mut Debouncer,
    root: &Path,
    conn: &IndexStore,
    registry: &PluginRegistry,
) {
    let deadline = std::time::Instant::now() + DEBOUNCE_WINDOW * 3;
    while std::time::Instant::now() < deadline {
        watch_and_route_once(watcher, debouncer, root, conn, registry);
    }
}

/// The concrete acceptance bar for task 129: a burst of rapid saves to
/// the *same* file must cost the plugin one round trip, not one per
/// save. Drives `watch_and_route_once` - `run`'s watcher thread's own
/// per-iteration logic - directly, against a real `ProjectWatcher` over
/// a real tempdir and a real `PluginRegistry` over a fake plugin that
/// logs every `fileChanged` round trip it actually answers
/// (`test_plugin::file_changed_requests`), rather than spawning a
/// background thread: that lets the burst and the assertion be
/// sequenced deterministically instead of racing a loop nothing here
/// could join.
#[test]
fn a_burst_of_rapid_saves_to_the_same_file_costs_one_plugin_round_trip_not_one_per_save() {
    let (project, _plugins, dirs, registry) = registry_over(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();

    let watcher = ProjectWatcher::new(&root).unwrap();
    let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
    drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

    let file = root.join("a.python-src");
    for i in 0..5 {
        fs::write(&file, format!("v{i}")).unwrap();
        thread::sleep(Duration::from_millis(20));
        watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
    }

    // Every write so far was well under DEBOUNCE_WINDOW apart, so nothing
    // must have settled and fired yet - proof this is actually debounced
    // rather than merely fast.
    assert!(
        test_plugin::file_changed_requests(plugin_dir).is_empty(),
        "a burst still in progress must not have reached the plugin yet"
    );

    pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

    assert_eq!(
        test_plugin::file_changed_requests(plugin_dir).len(),
        1,
        "a burst of 5 rapid saves to the same file must cost exactly one plugin round trip, not one per save"
    );
}

/// The other half of the same claim: debouncing must only coalesce a
/// genuine *burst*, never delay or drop an isolated, non-bursty edit.
#[test]
fn an_isolated_save_still_reaches_the_plugin_exactly_once() {
    let (project, _plugins, dirs, registry) = registry_over(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();

    let watcher = ProjectWatcher::new(&root).unwrap();
    let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
    drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

    fs::write(root.join("a.python-src"), "v0").unwrap();
    pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

    assert_eq!(
        test_plugin::file_changed_requests(plugin_dir).len(),
        1,
        "a single isolated save must still reach the plugin, exactly once"
    );
}

/// Debouncing coalesces repeats of the *same* path; it must not
/// accidentally coalesce two genuinely different files landing in the
/// same burst into one round trip - each still needs its own reparse.
#[test]
fn two_different_files_in_the_same_burst_each_get_their_own_round_trip() {
    let (project, _plugins, dirs, registry) = registry_over(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();

    let watcher = ProjectWatcher::new(&root).unwrap();
    let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
    drain_startup_noise(&watcher, &mut debouncer, &root, &conn, &registry);

    fs::write(root.join("a.python-src"), "a").unwrap();
    thread::sleep(Duration::from_millis(20));
    watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
    fs::write(root.join("b.python-src"), "b").unwrap();
    pump_until_settled(&watcher, &mut debouncer, &root, &conn, &registry);

    let mut requested = test_plugin::file_changed_requests(plugin_dir);
    requested.sort();
    assert_eq!(
        requested,
        vec!["a.python-src".to_string(), "b.python-src".to_string()],
        "two distinct files in the same burst must each still get their own round trip"
    );
}

/// Number of paths of each kind the batch-order tests below put in one
/// batch. The debouncer drains in `HashMap` order, so with one path per kind
/// an unordered batch would still come out right half the time; with eight
/// of each, an unordered drain happens to put all of one kind ahead of another
/// about once in 12870 runs (1 / C(16, 8)).
const PATHS_PER_KIND: usize = 8;

/// Records `paths` (relative to `root`, in this order) into a debouncer
/// whose window has already elapsed, then runs `watch_and_route_once` once,
/// so every path settles into the same batch and is routed by that one call.
/// The watcher watches an empty directory outside `root`: no filesystem
/// event can add a path to the batch or reorder it, so the debouncer holds
/// exactly the events fed here, in the order fed.
fn route_one_batch(root: &Path, conn: &IndexStore, registry: &PluginRegistry, paths: &[String]) {
    let elsewhere = tempfile::tempdir().unwrap();
    let watcher = ProjectWatcher::new(elsewhere.path().canonicalize().unwrap()).unwrap();
    let mut debouncer = Debouncer::new(Duration::ZERO);
    for path in paths {
        debouncer.record(root.join(path));
    }
    watch_and_route_once(&watcher, &mut debouncer, root, conn, registry);
}

fn record_baseline(conn: &IndexStore, file_path: &str) {
    conn.with(|c| crate::storage::write::upsert_indexed_file(c, file_path, 1, "hash")).unwrap();
}

/// The same batch with deletions in it: every deleted path (gone from disk,
/// baseline still recorded) is routed first, then creations, then
/// modifications - fed here in the opposite order.
#[test]
fn a_batch_routes_deletions_then_creations_then_modifications_whatever_the_event_order() {
    let (project, _plugins, dirs, registry) = registry_over(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();

    let names = |kind: &str| -> Vec<String> {
        (0..PATHS_PER_KIND).map(|i| format!("{kind}{i}.python-src")).collect()
    };
    let (deleted, created, modified) = (names("deleted"), names("created"), names("modified"));
    for path in &deleted {
        record_baseline(&conn, path);
    }
    for path in &created {
        fs::write(root.join(path), "").unwrap();
    }
    for path in &modified {
        fs::write(root.join(path), "").unwrap();
        record_baseline(&conn, path);
    }

    let reversed: Vec<String> = modified.iter().chain(&created).chain(&deleted).cloned().collect();
    route_one_batch(&root, &conn, &registry, &reversed);

    let requested = test_plugin::file_changed_requests(plugin_dir);
    assert_eq!(
        requested.len(),
        3 * PATHS_PER_KIND,
        "each settled path is routed exactly once: {requested:?}"
    );
    let kinds: Vec<&str> = requested
        .iter()
        .map(|path| {
            if deleted.contains(path) {
                "deleted"
            } else if created.contains(path) {
                "created"
            } else {
                "modified"
            }
        })
        .collect();
    let expected: Vec<&str> = ["deleted", "created", "modified"]
        .iter()
        .flat_map(|kind| std::iter::repeat_n(*kind, PATHS_PER_KIND))
        .collect();
    assert_eq!(kinds, expected, "deletions, then creations, then modifications: {requested:?}");
}

// --- GM-515: a batch's created files are announced before they are routed --
// Numbers name the behaviours in docs/architecture/gm-515-batch-presence.md.

/// [`registry_over`] for `languages`, each plugin declaring `files_created`.
fn declaring_registry(
    languages: &[&str],
) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    registry_with(|plugins| {
        languages
            .iter()
            .map(|language| {
                let extension = format!(".{language}-src");
                let dir = test_plugin::install(plugins, language, &[extension.as_str()]);
                test_plugin::declare_files_created(&dir);
                dir
            })
            .collect()
    })
}

/// `PATHS_PER_KIND` paths named `<kind><i>.python-src`.
fn python_paths(kind: &str) -> Vec<String> {
    (0..PATHS_PER_KIND).map(|i| format!("{kind}{i}.python-src")).collect()
}

/// The `filesCreated` lines of [`test_plugin::notifications`].
fn files_created_notifications(plugin_dir: &Path) -> Vec<String> {
    test_plugin::notifications(plugin_dir)
        .into_iter()
        .filter(|line| line.starts_with("filesCreated "))
        .collect()
}

/// The paths one `"filesCreated a,b"` notification line names, sorted.
fn announced_paths(line: &str) -> Vec<String> {
    let mut paths: Vec<String> =
        line.trim_start_matches("filesCreated ").split(',').map(str::to_string).collect();
    paths.sort();
    paths
}

/// [`test_plugin::frames`] for `fileChanged`/`filesCreated` only, in send order.
fn routing_frames(plugins_root: &Path) -> Vec<String> {
    test_plugin::frames(plugins_root)
        .into_iter()
        .filter(|frame| frame.ends_with(" fileChanged") || frame.ends_with(" filesCreated"))
        .collect()
}

/// Writes the deleted/created/modified fixture of the batch-order tests:
/// deleted paths have a baseline and no file, created ones a file and no
/// baseline, modified ones both. Returns the three lists.
fn three_kinds(root: &Path, conn: &IndexStore, created: usize) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (deleted, modified) = (python_paths("deleted"), python_paths("modified"));
    let created: Vec<String> = (0..created).map(|i| format!("created{i}.python-src")).collect();
    for path in &deleted {
        record_baseline(conn, path);
    }
    for path in &created {
        fs::write(root.join(path), "").unwrap();
    }
    for path in &modified {
        fs::write(root.join(path), "").unwrap();
        record_baseline(conn, path);
    }
    (deleted, created, modified)
}

/// 1: a plugin declaring `files_created` is told once, after the batch's
/// deletions and before its first creation, naming every created path in the
/// order they are then routed; every path still gets exactly one
/// `fileChanged`. Fed modified -> created -> deleted.
///
/// Control: remove the `registry.announce_created(&order.created)` call in
/// `watch_and_route_once` (no `filesCreated`), or move it before the deleted
/// loop (the plugin is not running yet, so nothing is sent; with it running,
/// the frame lands first).
#[test]
fn a_declaring_plugin_is_told_of_a_batchs_creations_between_its_deletions_and_its_creations() {
    let (project, plugins, dirs, registry) = declaring_registry(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let (deleted, created, modified) = three_kinds(&root, &conn, PATHS_PER_KIND);

    let reversed: Vec<String> = modified.iter().chain(&created).chain(&deleted).cloned().collect();
    route_one_batch(&root, &conn, &registry, &reversed);

    let requested = test_plugin::file_changed_requests(plugin_dir);
    assert_eq!(requested.len(), 3 * PATHS_PER_KIND, "each path is routed exactly once: {requested:?}");
    assert!(requested[..PATHS_PER_KIND].iter().all(|path| deleted.contains(path)), "{requested:?}");
    let routed_created = &requested[PATHS_PER_KIND..2 * PATHS_PER_KIND];
    assert!(routed_created.iter().all(|path| created.contains(path)), "{requested:?}");
    assert!(requested[2 * PATHS_PER_KIND..].iter().all(|path| modified.contains(path)), "{requested:?}");

    assert_eq!(
        files_created_notifications(plugin_dir),
        vec![format!("filesCreated {}", routed_created.join(","))],
        "one notification naming every created path, in routing order"
    );
    let mut expected = vec!["python fileChanged".to_string(); PATHS_PER_KIND];
    expected.push("python filesCreated".to_string());
    expected.extend(vec!["python fileChanged".to_string(); 2 * PATHS_PER_KIND]);
    assert_eq!(routing_frames(plugins.path()), expected, "deletions, the announcement, then the rest");
}

/// 2: a plugin that does not declare `files_created` sees only per-file
/// `fileChanged`, in GM-505's order, for the same batch.
///
/// Control: drop the `files_created` capability gate in
/// `PluginProcess::notify_files_created` -> a `filesCreated` frame appears.
#[test]
fn a_plugin_that_does_not_declare_files_created_is_never_sent_it() {
    let (project, plugins, dirs, registry) = registry_over(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let (deleted, created, modified) = three_kinds(&root, &conn, PATHS_PER_KIND);

    let reversed: Vec<String> = modified.iter().chain(&created).chain(&deleted).cloned().collect();
    route_one_batch(&root, &conn, &registry, &reversed);

    assert_eq!(test_plugin::file_changed_requests(plugin_dir).len(), 3 * PATHS_PER_KIND);
    assert_eq!(files_created_notifications(plugin_dir), Vec::<String>::new());
    assert_eq!(
        routing_frames(plugins.path()),
        vec!["python fileChanged".to_string(); 3 * PATHS_PER_KIND],
        "per-file fileChanged only"
    );
}

/// 3: one created file is no importer-and-target pair: nothing is announced,
/// though the plugin is running (spawned by the batch's deletions).
///
/// Control: lower the `file_paths.len() < 2` threshold in
/// `PluginRegistry::announce_created` to `< 1`.
#[test]
fn a_single_created_file_is_not_announced() {
    let (project, _plugins, dirs, registry) = declaring_registry(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let (deleted, created, modified) = three_kinds(&root, &conn, 1);

    let batch: Vec<String> = modified.iter().chain(&created).chain(&deleted).cloned().collect();
    route_one_batch(&root, &conn, &registry, &batch);

    assert!(test_plugin::file_changed_requests(plugin_dir).contains(&created[0]), "the file is still routed");
    assert_eq!(files_created_notifications(plugin_dir), Vec::<String>::new());
}

/// 4: a sleeping plugin is neither woken nor told; its queue replays every
/// created path once, on the wake that reads the disk anyway.
///
/// Control: make `PluginSupervisor::files_created` spawn or wake the process
/// when it has none (or make `announce_created` call `get_or_spawn`) -> a
/// second spawn before the replay, and a `filesCreated`.
#[test]
fn a_sleeping_plugin_is_neither_woken_nor_told_and_replays_each_created_file_once() {
    let (project, _plugins, dirs, registry) = declaring_registry(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let created = python_paths("created");
    for path in &created {
        fs::write(root.join(path), "").unwrap();
    }
    registry.get_or_spawn("python").unwrap().sleep_now("GM-515 test");
    assert_eq!(test_plugin::spawns(plugin_dir).len(), 1);

    route_one_batch(&root, &conn, &registry, &created);

    assert_eq!(test_plugin::spawns(plugin_dir).len(), 1, "the batch neither woke nor spawned the plugin");
    assert_eq!(test_plugin::file_changed_requests(plugin_dir), Vec::<String>::new(), "queued, not sent");

    assert_eq!(registry.replay_pending(&conn), PATHS_PER_KIND);
    let mut replayed = test_plugin::file_changed_requests(plugin_dir);
    replayed.sort();
    let mut expected = created.clone();
    expected.sort();
    assert_eq!(replayed, expected, "each created path is replayed exactly once");
    assert_eq!(files_created_notifications(plugin_dir), Vec::<String>::new(), "and never announced");
}

/// 4b: a language with no supervisor yet is not spawned to be told: its
/// first created file spawns it afterwards, and a fresh process reads the
/// disk the batch already wrote.
///
/// Control: replace the supervisor lookup in `announce_created` with
/// `get_or_spawn` -> a `filesCreated` reaches the new process.
#[test]
fn a_language_not_yet_spawned_is_not_spawned_to_be_told() {
    let (project, _plugins, dirs, registry) = declaring_registry(&["python"]);
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let created = python_paths("created");
    for path in &created {
        fs::write(root.join(path), "").unwrap();
    }

    route_one_batch(&root, &conn, &registry, &created);

    assert_eq!(test_plugin::spawns(plugin_dir).len(), 1, "spawned once, by routing");
    assert_eq!(test_plugin::file_changed_requests(plugin_dir).len(), PATHS_PER_KIND);
    assert_eq!(files_created_notifications(plugin_dir), Vec::<String>::new());
}

/// 5: a workspace file, a file under the language's `exclude_dirs` and an
/// unclaimed file are not announced; a language left with one path after
/// filtering is not told; a failed language is not told.
///
/// Control: remove, in turn, `announce_created`'s
/// `workspace_language_matches` filter (the first line names
/// `setup.python-src`), its `exclude_dirs` filter by using `language_for`
/// instead of `indexing_language` (`vendor/c.python-src`), or its
/// `is_failed_language` filter (a later line appears).
#[test]
fn workspace_excluded_and_failed_language_files_are_not_announced() {
    let (project, _plugins, dirs, registry) = registry_with(|plugins| {
        let dir = test_plugin::install_with_workspace(
            plugins,
            "python",
            &[".python-src"],
            &["setup.python-src"],
            &["vendor"],
        );
        test_plugin::declare_files_created(&dir);
        vec![dir]
    });
    let plugin_dir = &dirs[0];
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    fs::write(root.join("barrier.python-src"), "").unwrap();
    let supervisor = registry.get_or_spawn("python").unwrap();
    // A round trip after each announcement: the plugin reads its stdin in
    // order, so the notification is logged by the time the answer arrives.
    let barrier = || supervisor.file_changed(&conn, "barrier.python-src".to_string());
    let paths = |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };

    registry.announce_created(&paths(&[
        "a.python-src",
        "setup.python-src",
        "vendor/c.python-src",
        "notes.txt",
        "b.python-src",
    ]));
    barrier();
    registry.announce_created(&paths(&["a.python-src", "setup.python-src", "vendor/c.python-src"]));
    barrier();
    registry.set_failed_languages(["python".to_string()]);
    registry.announce_created(&paths(&["a.python-src", "b.python-src"]));
    barrier();

    assert_eq!(files_created_notifications(plugin_dir), vec!["filesCreated a.python-src,b.python-src"]);
}

/// 6: created files of two languages are announced per language, each with
/// only its own paths, and each plugin receives its announcement before the
/// `fileChanged` of its own creations. The order *across* the two plugins is
/// not asserted: each is a separate process appending to the shared frame log
/// when it reads a fire-and-forget notification, so that order is the
/// scheduler's, not core's.
///
/// Controls: send the whole created list to every running language in
/// `announce_created` -> each line names all four paths; announce after
/// routing the batch's creations -> a plugin's `filesCreated` follows its
/// `fileChanged` frames.
#[test]
fn created_files_of_two_languages_are_announced_per_language() {
    let (project, plugins, dirs, registry) = declaring_registry(&["python", "rust"]);
    let conn = test_plugin::empty_index();
    let root = project.path().canonicalize().unwrap();
    let created: Vec<String> =
        ["p1.python-src", "r1.rust-src", "p2.python-src", "r2.rust-src"].map(str::to_string).to_vec();
    for path in &created {
        fs::write(root.join(path), "").unwrap();
    }
    registry.get_or_spawn("rust").unwrap();
    registry.get_or_spawn("python").unwrap();

    route_one_batch(&root, &conn, &registry, &created);

    let python = files_created_notifications(&dirs[0]);
    let rust = files_created_notifications(&dirs[1]);
    assert_eq!(python.len(), 1, "{python:?}");
    assert_eq!(rust.len(), 1, "{rust:?}");
    assert_eq!(announced_paths(&python[0]), vec!["p1.python-src", "p2.python-src"]);
    assert_eq!(announced_paths(&rust[0]), vec!["r1.rust-src", "r2.rust-src"]);
    let frames = routing_frames(plugins.path());
    for language in ["python", "rust"] {
        let own: Vec<&str> = frames
            .iter()
            .filter_map(|frame| frame.strip_prefix(language).and_then(|rest| rest.strip_prefix(' ')))
            .collect();
        assert_eq!(own, vec!["filesCreated", "fileChanged", "fileChanged"], "{language}: {frames:?}");
    }
}
