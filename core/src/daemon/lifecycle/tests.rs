use super::*;

use crate::daemon::manifest::read_manifest;
use crate::daemon::test_plugin;

/// The reason [`PluginSupervisor::manifest`] is a field and not an
/// argument: a supervisor spawns *its own* plugin at every one of its
/// spawn points, including the wake that follows a sleep. Asserted on
/// processes, not on a return value - the fake plugin records every
/// process it is ever started as, so a wake that went to the wrong
/// manifest would leave this one's spawn count at 1 (and, with the
/// pre-registry hardcoded bridge, would have started the bundled JS/TS
/// plugin instead).
#[test]
fn a_supervisor_wakes_the_plugin_its_own_manifest_names() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let plugin_dir = test_plugin::install(plugins.path(), "python", &[".python-src"]);
    let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");
    let conn = test_plugin::empty_index();

    let supervisor = PluginSupervisor::start(
        project.path(),
        manifest,
        plugins.path().join("plugin.pid"),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the fixture plugin must start");

    assert_eq!(supervisor.language(), "python");
    let first_pid = supervisor.pid().expect("a freshly started plugin is awake");
    assert_eq!(test_plugin::spawns(&plugin_dir), vec![first_pid]);

    supervisor.sleep_now("the test asked it to");
    assert_eq!(supervisor.pid(), None, "a sleeping supervisor has no process");

    // Queued rather than applied, exactly as the watcher's events are
    // while the plugin sleeps...
    supervisor.file_changed(&conn, "app.python-src".to_string());
    assert!(supervisor.has_pending());
    assert_eq!(test_plugin::spawns(&plugin_dir).len(), 1, "queueing must not wake anything");

    // ...and the wake that replays them goes back to this supervisor's
    // own manifest.
    assert_eq!(supervisor.replay_pending(&conn).expect("the wake must succeed"), 1);
    let woken_pid = supervisor.pid().expect("replaying wakes the plugin");
    assert_ne!(woken_pid, first_pid);
    assert_eq!(
        test_plugin::spawns(&plugin_dir),
        vec![first_pid, woken_pid],
        "the wake must have spawned this manifest's plugin, not some other one"
    );
}

/// GM-271's acceptance test. Today, before this task's fix, a request the
/// plugin never answers blocks `watcher::apply::round_trip`'s read
/// forever - `file_changed` below would simply never return, and this
/// test would hang rather than fail. With the fix: the request times out
/// (a short, test-only `FILE_CHANGED_TIMEOUT_ENV` override - see
/// `daemon::plugin::RoundTripTimeouts` - so this test does not wait the
/// production 30s budget), the plugin is killed and relaunched through
/// the same crash-recovery path an out-of-band kill already used
/// (`plugin_crash_recovery.rs`), the file stays dirty rather than being
/// silently dropped, a later replay actually delivers it (the fixture
/// stalls on its first request only - see
/// `test_plugin::install_stalling`'s doc comment), and a second
/// language's supervisor - a wholly separate `PluginSupervisor` with its
/// own process, exactly as `daemon::registry::PluginRegistry` creates one
/// per language - keeps serving normally throughout, because streams are
/// per language.
#[test]
fn a_timed_out_file_change_relaunches_the_plugin_and_replays_the_dirty_file_without_blocking_another_language(
) {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var(crate::daemon::plugin::FILE_CHANGED_TIMEOUT_ENV, "150");

    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");

    let stalling_dir = test_plugin::install_stalling(plugins.path(), "go", &[".go-src"]);
    let stalling_manifest = read_manifest(&stalling_dir).expect("the fixture manifest must parse");
    let responsive_dir = test_plugin::install(plugins.path(), "python", &[".python-src"]);
    let responsive_manifest = read_manifest(&responsive_dir).expect("the fixture manifest must parse");

    let stalling = PluginSupervisor::start(
        project.path(),
        stalling_manifest,
        plugins.path().join("plugin-go.pid"),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the stalling fixture plugin must still shake hands and start normally");
    // Removed *between* the two spawns, not after both (GM-355). The
    // override is read once, inside `PluginProcess::spawn`, and the
    // budget it produces then belongs to that supervisor for life - so
    // setting it across both spawns handed the short stall budget to the
    // responsive plugin as well, whose round trip below has to *succeed*.
    // Only the stalling plugin has any business timing out here.
    //
    // It also has to come off promptly for the older reason: another test
    // racing on `ENV_LOCK` right after this one must not see it.
    std::env::remove_var(crate::daemon::plugin::FILE_CHANGED_TIMEOUT_ENV);

    let responsive = PluginSupervisor::start(
        project.path(),
        responsive_manifest,
        plugins.path().join("plugin-python.pid"),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the responsive fixture plugin must start");

    let conn = test_plugin::empty_index();
    let first_pid = stalling.pid().expect("a freshly started plugin is awake");

    // The discriminating assertion: this call must return once its
    // timeout elapses, not hang forever waiting for an answer that is
    // never coming. A hang guard, not a timing assertion - and GM-355
    // moved it from 10s to 60s because 10s was not actually one.
    //
    // What this call spends is 150ms of timeout plus a whole plugin
    // relaunch, and the relaunch is the part that grows with the machine:
    // measured 242ms in total on an idle laptop, 2.26s under a 20x CPU
    // oversubscription, and 7.06s under 50x (load average 585) - already
    // 71% of a 10s budget, on a machine nobody would call wedged. A
    // Windows runner carrying 1300 other tests is exactly that shape,
    // which is the most likely reading of the one failure in five this
    // test produced there.
    //
    // 60s matches `core/tests/common`'s `DEFAULT_STARTUP_TIMEOUT_SECS`,
    // GM-301's one number for every "wait for a process to do a
    // process-shaped thing" in this repo, and for its stated reason: a
    // budget picked on a quiet laptop is not a promise the code ever
    // made. This is the one wait here the clock must still decide, since
    // the alternative to a guard is a test that hangs CI instead of
    // failing it - named as such rather than left to be rediscovered.
    let start = Instant::now();
    stalling.file_changed(&conn, "app.go-src".to_string());
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(60),
        "file_changed must return once its timeout elapses, not block on a plugin that never \
         answers - took {elapsed:?}"
    );

    // The stalled request was genuinely received (not skipped, dropped,
    // or refused) before it was left unanswered.
    assert_eq!(
        test_plugin::file_changed_requests(&stalling_dir),
        vec!["app.go-src".to_string()],
        "the timed-out plugin must have actually seen the request before timing out on it"
    );

    // The plugin was killed and relaunched: a fresh, different, live pid.
    let relaunched_pid = stalling.pid().expect("a fresh process must be running after the timeout");
    assert_ne!(relaunched_pid, first_pid, "the timed-out plugin process must have been relaunched");
    assert!(
        crate::daemon::is_process_alive(relaunched_pid),
        "the relaunched process must actually be running"
    );
    assert_eq!(
        test_plugin::spawns(&stalling_dir),
        vec![first_pid, relaunched_pid],
        "exactly one relaunch, of this supervisor's own manifest"
    );

    // The request is dropped rather than replayed inline - the file
    // stays dirty for a later replay instead.
    assert!(stalling.has_pending(), "the timed-out file must stay queued as dirty, not be dropped");

    // Meanwhile, a second language's supervisor - a wholly independent
    // stream - was never touched by any of the above, and keeps serving
    // normally.
    responsive.file_changed(&conn, "app.python-src".to_string());
    assert_eq!(
        test_plugin::file_changed_requests(&responsive_dir),
        vec!["app.python-src".to_string()],
        "a second language must keep flowing while the first is stuck"
    );
    assert_eq!(
        test_plugin::spawns(&responsive_dir).len(),
        1,
        "the second language's plugin was never touched by the first one's timeout"
    );

    // Everything above is what the short budget was for, and it is spent
    // (GM-355). The relaunched process keeps the budget its
    // `PluginProcess` captured at construction, so without this the
    // replay below - a healthy round trip that has to *succeed* - would
    // go on racing the 150ms the stall was given. Measured, it takes
    // 2.0ms idle and 2.2ms under a 20x CPU oversubscription, then crosses
    // 150ms at 50x, times out, relaunches again and returns 0 replayed:
    // the test failing because the machine was busy, with an assertion
    // about a dirty file's fate.
    //
    // The production default rather than "something bigger": there is
    // nothing special about this round trip, so it should be judged on
    // the budget a real one gets.
    stalling.set_round_trip_timeouts(crate::daemon::plugin::RoundTripTimeouts::default());

    // The dirty file is actually replayed - against the relaunched
    // process, which (per `install_stalling`'s one-stall-ever contract)
    // now answers normally, so this succeeds rather than timing out
    // again.
    let replayed = stalling.replay_pending(&conn).expect("the queued replay must succeed this time");
    assert_eq!(replayed, 1, "exactly the one file that was left dirty");
    assert!(!stalling.has_pending(), "nothing should be left queued after a successful replay");
    assert_eq!(
        test_plugin::file_changed_requests(&stalling_dir),
        vec!["app.go-src".to_string(), "app.go-src".to_string()],
        "the relaunched process must have seen the same file a second time, and answered it"
    );

    stalling.sleep_now("test cleanup");
    responsive.sleep_now("test cleanup");
}

#[test]
fn an_unset_timeout_reads_as_its_documented_default() {
    assert_eq!(parse_timeout(None, DEFAULT_PLUGIN_IDLE, "X"), Some(DEFAULT_PLUGIN_IDLE));
}

/// Guards every test below that touches [`PLUGIN_IDLE_ENV`] /
/// [`CORE_IDLE_ENV`]: they are process-wide state, and `cargo test` runs
/// this module's tests on multiple threads by default, so two of them
/// setting/clearing the same variable at once would be a genuine race,
/// not just noise. Held for the lifetime of each test that needs it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A project with no config.toml (`ProjectConfig::default()`) must
/// resolve to exactly the pre-config defaults - task #38's behavior,
/// unchanged now that config is wired in.
#[test]
fn a_default_config_resolves_to_the_documented_defaults() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(PLUGIN_IDLE_ENV);
    std::env::remove_var(CORE_IDLE_ENV);

    let resolved = IdleTimeouts::from_config(&ProjectConfig::default());
    assert_eq!(resolved, IdleTimeouts::default());
    assert_eq!(resolved.plugin, Some(DEFAULT_PLUGIN_IDLE));
    assert_eq!(resolved.core, Some(DEFAULT_CORE_IDLE));
}

/// The acceptance criterion at the unit level: a config.toml with a
/// shortened `plugin.idleTimeoutMinutes` actually produces a shortened
/// plugin timer, not the hardcoded default.
#[test]
fn a_configured_plugin_idle_timeout_overrides_the_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(PLUGIN_IDLE_ENV);
    std::env::remove_var(CORE_IDLE_ENV);

    let config = ProjectConfig {
        plugin: crate::config::PluginConfig { idle_timeout_minutes: 5, memory_limit_mb: None },
        ..ProjectConfig::default()
    };
    let resolved = IdleTimeouts::from_config(&config);
    assert_eq!(resolved.plugin, Some(Duration::from_secs(5 * 60)));
    // The core timeout is untouched by a config that only sets [plugin].
    assert_eq!(resolved.core, Some(DEFAULT_CORE_IDLE));
}

/// Same claim, for `daemon.coreIdleTimeoutHours`.
#[test]
fn a_configured_core_idle_timeout_overrides_the_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(PLUGIN_IDLE_ENV);
    std::env::remove_var(CORE_IDLE_ENV);

    let config = ProjectConfig {
        daemon: crate::config::DaemonConfig { core_idle_timeout_hours: 2 },
        ..ProjectConfig::default()
    };
    let resolved = IdleTimeouts::from_config(&config);
    assert_eq!(resolved.core, Some(Duration::from_secs(2 * 60 * 60)));
    assert_eq!(resolved.plugin, Some(DEFAULT_PLUGIN_IDLE));
}

/// The test-only env override still wins over a configured value - the
/// same precedence it always had over the hardcoded default, now proven
/// against a config that disagrees with it too.
#[test]
fn the_env_override_still_wins_over_a_configured_value() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var(PLUGIN_IDLE_ENV, "250");

    let config = ProjectConfig {
        plugin: crate::config::PluginConfig { idle_timeout_minutes: 5, memory_limit_mb: None },
        ..ProjectConfig::default()
    };
    let resolved = IdleTimeouts::from_config(&config);
    assert_eq!(resolved.plugin, Some(Duration::from_millis(250)));

    std::env::remove_var(PLUGIN_IDLE_ENV);
}

#[test]
fn a_zero_timeout_turns_the_timer_off() {
    assert_eq!(parse_timeout(Some("0"), DEFAULT_CORE_IDLE, "X"), None);
}

#[test]
fn a_number_is_read_as_milliseconds() {
    assert_eq!(parse_timeout(Some(" 250 "), DEFAULT_CORE_IDLE, "X"), Some(Duration::from_millis(250)));
}

/// A typo must not silently disable a timer, and must not stop the daemon.
#[test]
fn an_unparseable_timeout_falls_back_to_the_default() {
    assert_eq!(parse_timeout(Some("10 minutes"), DEFAULT_PLUGIN_IDLE, "X"), Some(DEFAULT_PLUGIN_IDLE));
}

#[test]
fn the_monitor_tick_is_clamped_at_both_ends() {
    let production = IdleTimeouts::default();
    assert_eq!(production.tick(), MAX_TICK, "a quarter of an hour is too coarse to be useful");

    let tiny = IdleTimeouts { plugin: Some(Duration::from_millis(4)), core: Some(DEFAULT_CORE_IDLE) };
    assert_eq!(tiny.tick(), MIN_TICK, "a short test timeout must not become a spin loop");

    let test_sized = IdleTimeouts { plugin: Some(Duration::from_millis(800)), core: Some(DEFAULT_CORE_IDLE) };
    assert_eq!(test_sized.tick(), Duration::from_millis(200));
}

#[test]
fn both_timers_off_still_yields_a_sane_tick() {
    assert_eq!(IdleTimeouts { plugin: None, core: None }.tick(), MAX_TICK);
}

#[test]
fn the_dirty_queue_keeps_first_sighting_order_and_drops_repeats() {
    let mut queue = DirtyQueue::default();
    assert!(queue.is_empty());

    queue.push("src/a.ts".to_string());
    queue.push("src/b.ts".to_string());
    queue.push("src/a.ts".to_string()); // saved again during the same sleep
    assert!(!queue.is_empty());

    assert_eq!(queue.drain(), vec!["src/a.ts".to_string(), "src/b.ts".to_string()]);
    assert!(queue.is_empty(), "a drained queue starts the next sleep empty");

    // And the de-duplication set is drained with it, or a file changed in
    // two consecutive sleeps would be replayed only in the first.
    queue.push("src/a.ts".to_string());
    assert_eq!(queue.drain(), vec!["src/a.ts".to_string()]);
}

/// The healthy case, which is every tick of every daemon that is not
/// orphaned: both paths resolve, and nothing is reported.
#[test]
fn a_project_root_and_executable_that_both_exist_are_not_an_orphan() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let exe = tempfile::NamedTempFile::new().expect("failed to create a stand-in executable");

    assert_eq!(orphan_check(project.path(), Ok(exe.path().to_path_buf())), None);
}

/// GM-320's first arm, and the one the four hand-cleared daemons were in:
/// the checkout the daemon was serving is gone.
#[test]
fn a_deleted_project_root_is_an_orphan() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let root = project.path().to_path_buf();
    let exe = tempfile::NamedTempFile::new().expect("failed to create a stand-in executable");
    project.close().expect("failed to delete the project root");

    assert_eq!(
        orphan_check(&root, Ok(exe.path().to_path_buf())),
        Some(Orphaned::ProjectRootGone(root.clone()))
    );
    // The log line has to name the root, or an operator with several
    // daemons cannot tell which one just went.
    assert!(
        Orphaned::ProjectRootGone(root.clone()).to_string().contains(&root.display().to_string()),
        "the reason must name the root it judged"
    );
}

/// The second arm: a `cargo clean`, a deleted worktree, a `/tmp` build
/// swept away. The root is deliberately left intact here, so the only
/// thing that can produce a verdict is the executable.
#[test]
fn a_deleted_executable_is_an_orphan_even_with_the_project_root_intact() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let exe_dir = tempfile::tempdir().expect("failed to create an executable directory");
    let exe = exe_dir.path().join("g-mesh");
    fs::write(&exe, b"not really a binary").expect("failed to create a stand-in executable");

    assert_eq!(orphan_check(project.path(), Ok(exe.clone())), None, "nothing is missing yet");

    fs::remove_file(&exe).expect("failed to delete the stand-in executable");
    assert_eq!(orphan_check(project.path(), Ok(exe.clone())), Some(Orphaned::ExecutableGone(exe)));
}

/// A `current_exe()` that failed is not evidence that anything is missing,
/// and this decision ends a process - so "cannot tell" must read exactly
/// like "nothing is wrong". The intact project root is what makes this
/// test about the executable branch alone.
#[test]
fn an_unresolvable_executable_is_never_treated_as_a_missing_one() {
    let project = tempfile::tempdir().expect("failed to create a project root");

    let unresolvable = Err(io::Error::new(io::ErrorKind::PermissionDenied, "cannot read /proc/self/exe"));
    assert_eq!(orphan_check(project.path(), unresolvable), None);
}

/// A deleted checkout takes its `target/` with it, so both arms are true
/// at once - and the one worth logging is the project, not the binary that
/// was inside it.
#[test]
fn a_root_that_is_gone_is_reported_ahead_of_an_executable_that_is_also_gone() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let root = project.path().to_path_buf();
    let exe = root.join("target/debug/g-mesh");
    project.close().expect("failed to delete the project root");

    assert_eq!(orphan_check(&root, Ok(exe)), Some(Orphaned::ProjectRootGone(root)));
}

/// The discriminating half of [`is_definitely_gone`]: a path whose *parent*
/// does not exist is itself absent, while a path that is merely empty, or
/// a directory, is present. Only `NotFound` may ever end a daemon.
#[test]
fn only_a_positively_absent_path_reads_as_gone() {
    let dir = tempfile::tempdir().expect("failed to create a directory");
    assert!(!is_definitely_gone(dir.path()), "a directory that exists is not gone");

    let empty = dir.path().join("empty");
    fs::write(&empty, b"").expect("failed to create an empty file");
    assert!(!is_definitely_gone(&empty), "an empty file is still a file");

    assert!(is_definitely_gone(&dir.path().join("no/such/path")));
}

#[test]
fn a_disabled_core_timeout_never_reports_an_idle_core() {
    let core = CoreActivity::new();
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(core.idle_beyond(None), None);
}

#[test]
fn an_unattended_core_reports_idle_once_its_timeout_elapses() {
    let core = CoreActivity::new();
    assert_eq!(core.idle_beyond(Some(Duration::from_secs(60))), None, "it has only just started");

    std::thread::sleep(Duration::from_millis(20));
    let idle = core.idle_beyond(Some(Duration::from_millis(10))).expect("nothing is attached");
    assert!(idle >= Duration::from_millis(10), "reported idle time must be the real one: {idle:?}");
}

/// The guarantee that keeps a long-lived editor session's core alive.
#[test]
fn a_live_connection_holds_the_core_open_however_quiet_it_is() {
    let core = CoreActivity::new();
    let guard = core.connection_opened();
    std::thread::sleep(Duration::from_millis(20));

    assert_eq!(
        core.idle_beyond(Some(Duration::from_millis(1))),
        None,
        "a client is attached, so the core is not idle no matter how long it has been silent"
    );

    drop(guard);
    // Dropping restarts the clock rather than exposing the silence that
    // came before it - the disconnect is itself the most recent activity.
    assert_eq!(core.idle_beyond(Some(Duration::from_millis(10))), None);
    std::thread::sleep(Duration::from_millis(20));
    assert!(core.idle_beyond(Some(Duration::from_millis(10))).is_some());
}

#[test]
fn a_request_restarts_the_idle_clock() {
    let core = CoreActivity::new();
    std::thread::sleep(Duration::from_millis(20));
    core.request();
    assert_eq!(core.idle_beyond(Some(Duration::from_millis(15))), None);
}

/// Task GM-274's own acceptance criterion at the unit level: "with no key
/// set, behaviour is identical". A bare `check_memory_limit` call on a
/// plugin whose process tree is genuinely large (the same memory-hungry
/// fixture the over-limit test below uses) must leave it running, awake
/// and unsuspended - `memory_limit_mb: None` returns before sampling
/// anything at all (see that method's own doc comment), so there is no
/// number to compare against and nothing this call could have enforced.
#[test]
fn with_no_memory_limit_configured_an_oversized_plugin_is_left_alone() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let plugin_dir = test_plugin::install_memory_hungry(plugins.path(), "heavy", &[".heavy-src"]);
    let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");

    let supervisor = PluginSupervisor::start(
        project.path(),
        manifest,
        plugins.path().join("plugin.pid"),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the fixture plugin must start");

    let pid_before = supervisor.pid().expect("a freshly started plugin is awake");
    supervisor.check_memory_limit();
    assert_eq!(
        supervisor.pid(),
        Some(pid_before),
        "with memoryLimitMb unset, an over-sized plugin must still be left running"
    );
    assert!(!supervisor.is_semantic_suspended());

    supervisor.sleep_now("test cleanup");
}

/// The main acceptance test: a plugin whose process tree crosses an
/// artificially low `memoryLimitMb` is put to sleep, its language's
/// semantic passes are suspended, the next `fileChanged` still wakes it
/// and is answered structurally, no `semanticPass` rides along with that
/// wake even though this fixture's manifest declares the capability, and
/// the whole-project scheduler (`semantic_pass`) respects the suspension
/// too.
///
/// 100MB sits between `install_memory_hungry`'s own documented margins -
/// a bare Node baseline (~20-40MB) and its 200MB hog - so this is not a
/// hair's-breadth threshold a slow CI machine could cross by accident in
/// either direction.
#[test]
fn a_plugin_over_its_memory_limit_is_put_to_sleep_and_its_language_suspended() {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let plugin_dir = test_plugin::install_memory_hungry(plugins.path(), "heavy", &[".heavy-src"]);
    let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");
    let conn = test_plugin::empty_index();

    let supervisor = PluginSupervisor::start(
        project.path(),
        manifest,
        plugins.path().join("plugin.pid"),
        None,
        Some(100),
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the fixture plugin must start");

    assert!(!supervisor.is_semantic_suspended(), "not suspended before the first sample");

    // Through the seam, not `check_memory_limit()`'s real sampler (GM-340).
    //
    // This test's subject is the *aftermath* of a suspension: that the
    // next fileChanged still wakes the plugin for structural work, and
    // that no semanticPass rides along with it. Reaching that state
    // through the real sampler made it depend on the operating system
    // reporting the fixture's 200MB buffer as over 100MB on **two
    // consecutive scans**, on a runner already carrying 1300 other tests.
    // That is not a property this test is judged on, and it is what made
    // it flaky - three failures in five days, twice on Windows and once
    // on macOS, every one of them with the plugin still alive when the
    // assertion ran.
    //
    // The decision keeps its own coverage, and keeps it deterministically:
    // `a_confirmed_over_limit_sample_suspends_the_language` proves that two
    // over-limit readings suspend, and
    // `an_unconfirmed_over_limit_sample_leaves_the_plugin_running` proves
    // the other arm. That the *real* sampler can see a real process's
    // memory belongs to `core/tests/plugin_memory_limit.rs`, against a
    // real rust-analyzer, where a live measurement is the subject rather
    // than an obstacle.
    let (sampler, _calls) = scripted_sampler(vec![Some(500), Some(480)]);
    supervisor.check_memory_limit_sampled_by(sampler);

    assert_eq!(supervisor.pid(), None, "a plugin over its memory limit must be put to sleep");
    assert!(supervisor.is_semantic_suspended(), "its language's semantic passes must be suspended");

    // The next fileChanged still wakes the plugin for structural work -
    // queued while asleep (exactly like an ordinary idle sleep), then
    // replayed on the next wake.
    supervisor.file_changed(&conn, "app.heavy-src".to_string());
    assert!(supervisor.has_pending());
    let replayed = supervisor.replay_pending(&conn).expect("the wake must succeed");
    assert_eq!(replayed, 1);
    assert!(supervisor.pid().is_some(), "the wake must have relaunched the plugin");

    // Structural work happened...
    assert_eq!(
        test_plugin::file_changed_requests(&plugin_dir),
        vec!["app.heavy-src".to_string()],
        "the structural fileChanged request must still be answered"
    );
    // ...but no semanticPass rode along with it, even though this
    // fixture's manifest declares `capabilities.semantic_pass = true` and
    // would otherwise always send one on the same round trip
    // (`watcher::apply::apply_file_change`'s own doc comment) - the
    // discriminating assertion this task is judged on.
    let requests = test_plugin::requests(&plugin_dir);
    assert!(
        requests.iter().all(|line| !line.starts_with("semanticPass")),
        "a suspended language must never receive a semanticPass request: {requests:?}"
    );

    // Suspension persists across the wake - it is not an idle-sleep
    // artifact that clears once the plugin wakes back up.
    assert!(supervisor.is_semantic_suspended());

    // The whole-project scheduler respects it too (decision 5's other
    // half) - `daemon::semantic::run_with_registry`/`run_once` and
    // `daemon::workspace_reindex` both go through this same method.
    assert!(
        !supervisor.semantic_pass(&conn, Vec::new(), 0).expect("must not error, just skip"),
        "a suspended language's whole-project semantic pass must not run either"
    );

    supervisor.sleep_now("test cleanup");
}

/// A sampler that hands out a scripted sequence of readings and counts how
/// many were asked for - the one thing a real
/// `daemon::memory::process_tree_rss_mb` cannot be made to do, and the
/// whole reason `check_memory_limit_sampled_by` takes its sampler as a
/// parameter. Past the end of the script it keeps answering the last
/// reading, so a test that scripts fewer readings than are taken fails on
/// the call count rather than on a panic from somewhere unrelated.
fn scripted_sampler(readings: Vec<Option<u64>>) -> (impl Fn(u32) -> Option<u64>, Arc<AtomicUsize>) {
    assert!(!readings.is_empty(), "a scripted sampler needs at least one reading");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let sampler = move |_pid: u32| {
        let index = counter.fetch_add(1, Ordering::SeqCst);
        readings[index.min(readings.len() - 1)]
    };
    (sampler, calls)
}

/// A supervisor over the cheapest real plugin fixture there is - this
/// module's plain `install`, not the 200MB `install_memory_hungry` - since
/// the three tests below script their own readings and so have no use for
/// a process that is genuinely large. Returns the temp dirs too: dropped
/// early, the pid-file writes fail.
fn supervisor_with_limit(
    limit_mb: Option<u64>,
) -> (Arc<PluginSupervisor>, tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let plugin_dir = test_plugin::install(plugins.path(), "scripted", &[".scripted-src"]);
    let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");
    let supervisor = PluginSupervisor::start(
        project.path(),
        manifest,
        plugins.path().join("plugin.pid"),
        None,
        limit_mb,
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the fixture plugin must start");
    (supervisor, project, plugins)
}

/// GM-304/GM-307's own acceptance criterion, and the discriminating half
/// of it: one over-limit reading is one instant, and an instant can hold a
/// `rustc` or a build script that is gone again by the next scan. A
/// reading that is not confirmed must leave the plugin exactly as it was -
/// awake, unsuspended, no marker on disk - because suspension is
/// irreversible for this daemon's life while declining to suspend costs
/// one tick.
///
/// Paired with the test below, which scripts the same first reading and a
/// *confirming* second one: the two differ in nothing but that second
/// number, so between them they show the confirmation is what decides,
/// not the limit or the fixture.
#[test]
fn an_unconfirmed_over_limit_sample_leaves_the_plugin_running() {
    let (supervisor, _project, _plugins) = supervisor_with_limit(Some(100));
    let pid_before = supervisor.pid().expect("a freshly started plugin is awake");

    let (sampler, calls) = scripted_sampler(vec![Some(500), Some(40)]);
    supervisor.check_memory_limit_sampled_by(sampler);

    assert_eq!(calls.load(Ordering::SeqCst), 2, "an over-limit reading must be confirmed, not acted on");
    assert_eq!(
        supervisor.pid(),
        Some(pid_before),
        "a transient over-limit instant must not put the plugin to sleep"
    );
    assert!(!supervisor.is_semantic_suspended(), "nor suspend its language");
    assert!(
        !supervisor.suspended_marker_path().exists(),
        "nor leave a suspension marker g-mesh status would report"
    );

    supervisor.sleep_now("test cleanup");
}

/// The other arm: the same first reading, confirmed. Everything GM-274's
/// own acceptance test asserts still happens - asleep, suspended, marker
/// written - and the reason names both readings, so whoever reads
/// `g-mesh status` can see the evidence the decision was made on rather
/// than one number.
#[test]
fn a_confirmed_over_limit_sample_suspends_the_language() {
    let (supervisor, _project, _plugins) = supervisor_with_limit(Some(100));
    assert!(supervisor.pid().is_some(), "a freshly started plugin is awake");

    let (sampler, calls) = scripted_sampler(vec![Some(500), Some(480)]);
    supervisor.check_memory_limit_sampled_by(sampler);

    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly the reading and its confirmation");
    assert_eq!(supervisor.pid(), None, "a confirmed overage must put the plugin to sleep");
    assert!(supervisor.is_semantic_suspended(), "and suspend its language");
    let marker = std::fs::read_to_string(supervisor.suspended_marker_path())
        .expect("a suspended language must leave a marker for g-mesh status");
    assert!(marker.contains("500MB"), "the marker must name the reading: {marker}");
    assert!(marker.contains("480MB"), "and the confirming sample beside it: {marker}");
}

/// The common case costs exactly one scan, not two: a tree under its limit
/// is the reading every tick takes for every configured language for the
/// whole life of a healthy daemon, and `sysinfo::refresh_processes` is a
/// whole-system walk. The confirming sample is only ever paid for on the
/// path that is about to suspend something.
#[test]
fn a_reading_under_the_limit_costs_a_single_sample() {
    let (supervisor, _project, _plugins) = supervisor_with_limit(Some(100));

    let (sampler, calls) = scripted_sampler(vec![Some(40), Some(500)]);
    supervisor.check_memory_limit_sampled_by(sampler);

    assert_eq!(calls.load(Ordering::SeqCst), 1, "an under-limit reading is the end of the tick");
    assert!(supervisor.pid().is_some(), "and the plugin is left running");
    assert!(!supervisor.is_semantic_suspended());

    supervisor.sleep_now("test cleanup");
}
