use super::*;
use rusqlite::Connection;

use crate::daemon::manifest::discover;
use crate::daemon::test_plugin;
use crate::storage::schema;

/// A registry over `languages`, each installed as a fake plugin claiming
/// one extension named after it (`python` -> `.python-src`), deliberately
/// unlike any extension a real plugin would claim.
///
/// Returns the tempdirs it built (the project root and the discovery
/// root) alongside the registry: dropping them would delete the plugin
/// directories the registry is about to spawn from.
fn registry_over(languages: &[&str]) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    registry_over_inner(languages, false)
}

/// [`registry_over`], over plugins whose spawn does not finish until the
/// test opens their handshake gate - what every test about *while a spawn
/// is in flight* is built on, since that window is otherwise a few
/// unobservable milliseconds wide. See `test_plugin::install_gated`, and
/// note its warning: every one of those tests has to open the gate on
/// every path, or the thread it left spawning never joins.
fn registry_over_gated(
    languages: &[&str],
) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    registry_over_inner(languages, true)
}

fn registry_over_inner(
    languages: &[&str],
    gated: bool,
) -> (tempfile::TempDir, tempfile::TempDir, Vec<PathBuf>, PluginRegistry) {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");

    let dirs = languages
        .iter()
        .map(|language| {
            let extension = extension_for(language);
            let install = if gated { test_plugin::install_gated } else { test_plugin::install };
            install(plugins.path(), language, &[extension.as_str()])
        })
        .collect();

    let discovered = discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
    let state_dir = crate::storage::connection::project_dir(project.path())
        .expect("failed to resolve the fixture project's state directory");
    std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
    let registry = PluginRegistry::new(
        project.path(),
        state_dir,
        discovered,
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    );
    (project, plugins, dirs, registry)
}

fn extension_for(language: &str) -> String {
    format!(".{language}-src")
}

/// Discovery over `languages`, each installed as the same stub plugin
/// [`registry_over`] uses - what [`indexer_version`] takes, without the
/// registry it deliberately does not need. Returns the plugin root's
/// tempdir (dropping it would delete the very files being fingerprinted)
/// and each installed plugin's directory, so a test can edit one.
fn discovery_over(languages: &[&str]) -> (tempfile::TempDir, Vec<PathBuf>, DiscoveredPlugins) {
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let dirs = languages
        .iter()
        .map(|language| {
            let extension = extension_for(language);
            test_plugin::install(plugins.path(), language, &[extension.as_str()])
        })
        .collect();
    let discovered = discover(&[plugins.path().to_path_buf()]).expect("the fixtures must discover cleanly");
    (plugins, dirs, discovered)
}

/// Rewrites a plugin's entry point with different bytes - a rebuild that
/// changed its extraction logic, which is the whole event
/// [`indexer_version`] exists to make visible.
fn rebuild_with_a_change(plugin_dir: &Path) {
    let path = plugin_dir.join("plugin.js");
    let mut source = fs::read_to_string(&path).expect("failed to read the stub plugin");
    source.push_str("\n// rebuilt with different extraction logic\n");
    fs::write(&path, source).expect("failed to rewrite the stub plugin");
}

#[test]
fn the_generation_names_the_core_pipeline_and_a_digest_of_every_plugin() {
    let (_plugins, _dirs, discovered) = discovery_over(&["python", "go"]);

    let version = indexer_version(&discovered);
    let (core, plugins) = version.split_once('+').expect("both halves must be present");

    assert_eq!(core, CURRENT_INDEXER_VERSION);
    assert!(plugins.chars().all(|c| c.is_ascii_hexdigit()), "{plugins} must be hex");
    assert!(!plugins.is_empty(), "the plugin half must be a real digest");
    // Re-asked, it answers the same: nothing about this is derived from
    // the process asking.
    assert_eq!(indexer_version(&discovered), version);
}

/// The property the sort exists for, at the level a real install produces
/// it: the same two plugins, found by scanning their roots in either
/// order, are the same index generation. A daemon that hashed them in scan
/// order would wipe the index of a machine whose roots happened to be
/// listed the other way round.
#[test]
fn scanning_the_same_plugins_roots_in_either_order_is_the_same_generation() {
    let one_root = tempfile::tempdir().expect("failed to create a plugin root");
    let other_root = tempfile::tempdir().expect("failed to create a plugin root");
    test_plugin::install(one_root.path(), "python", &[".python-src"]);
    test_plugin::install(other_root.path(), "go", &[".go-src"]);
    let (one, other) = (one_root.path().to_path_buf(), other_root.path().to_path_buf());

    let forwards = discover(&[one.clone(), other.clone()]).unwrap();
    let backwards = discover(&[other, one]).unwrap();

    assert_eq!(forwards.manifests.len(), 2, "both roots must have contributed");
    assert_eq!(indexer_version(&backwards), indexer_version(&forwards));
}

/// The same property against the other source of order this could have
/// picked up: `HashMap`'s own iteration, which is seeded per map and so
/// genuinely differs between two maps holding the same entries. Enough
/// languages that two maps iterating in the same order by chance is not
/// what makes this pass.
#[test]
fn the_generation_does_not_depend_on_hash_map_iteration_order() {
    let languages = ["python", "go", "rust", "ruby", "elixir", "zig", "nim", "ocaml"];
    let (_plugins, _dirs, discovered) = discovery_over(&languages);

    let mut reversed = DiscoveredPlugins::default();
    for language in languages.iter().rev() {
        let manifest = discovered.manifests[*language].clone();
        for extension in &manifest.extensions {
            reversed.routing.insert(extension.clone(), language.to_string());
        }
        reversed.manifests.insert(language.to_string(), manifest);
    }

    assert_eq!(reversed.manifests, discovered.manifests, "the same set, differently built");
    assert_eq!(indexer_version(&reversed), indexer_version(&discovered));
}

/// Task 116's failure, once per language: whichever plugin was rebuilt,
/// the index it filled is no longer what today's pipeline would produce.
#[test]
fn rebuilding_either_languages_plugin_changes_the_generation() {
    let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
    let (python, go) = (&dirs[0], &dirs[1]);

    let before = indexer_version(&discovered);

    rebuild_with_a_change(python);
    let after_python = indexer_version(&discovered);
    assert_ne!(after_python, before, "a rebuilt python plugin must move the generation");

    rebuild_with_a_change(go);
    let after_go = indexer_version(&discovered);
    assert_ne!(after_go, after_python, "and so must a rebuilt go plugin");
    assert_ne!(after_go, before);
}

/// The control that keeps the test above about content rather than about
/// mtime - a plugin's build system re-emitting identical bytes must not
/// cost every project on the machine a full re-walk.
#[test]
fn re_emitting_a_plugin_unchanged_leaves_the_generation_alone() {
    let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
    let before = indexer_version(&discovered);

    let path = dirs[0].join("plugin.js");
    let source = fs::read(&path).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    fs::write(&path, source).unwrap();

    assert_eq!(indexer_version(&discovered), before);
}

/// Installing (or removing) a plugin changes the generation, because it
/// changes what the index is a graph *of*: the languages an existing
/// index covers are exactly the ones that were installed when it was
/// filled. The languages that stayed put contribute the same thing
/// either way, which is what makes this a fact about the set and not
/// about one plugin having moved.
#[test]
fn adding_a_second_language_changes_the_generation_without_disturbing_the_first() {
    let (_one_root, _one_dirs, only_python) = discovery_over(&["python"]);
    let (_other_root, _other_dirs, both) = discovery_over(&["python", "go"]);
    let (_third_root, _third_dirs, python_again) = discovery_over(&["python"]);

    assert_ne!(indexer_version(&both), indexer_version(&only_python));
    // Two separate installs of the same one plugin agree - the digest is
    // over what the plugins *contain*, not over where they were found.
    assert_eq!(indexer_version(&python_again), indexer_version(&only_python));
}

/// A machine with no plugins at all still gets a well-formed generation
/// rather than an empty half or a panic - and it is not the generation of
/// a machine that has one.
#[test]
fn a_discovery_that_found_nothing_still_produces_a_well_formed_generation() {
    let empty = indexer_version(&DiscoveredPlugins::default());
    let (_plugins, _dirs, discovered) = discovery_over(&["python"]);

    let (core, plugins) = empty.split_once('+').expect("both halves must be present");
    assert_eq!(core, CURRENT_INDEXER_VERSION);
    assert!(plugins.chars().all(|c| c.is_ascii_hexdigit()), "{plugins} must be hex");
    assert_ne!(empty, indexer_version(&discovered));
}

/// The acceptance criterion this whole value exists to serve, driven
/// through the check that really consumes it: `ensure_current` keeps an
/// index whose generation still matches and throws one away whose
/// plugin-derived half has moved - now for *any* discovered plugin, not
/// just the bundled one.
#[test]
fn ensure_current_reindexes_when_any_one_plugins_build_changes() {
    let (_plugins, dirs, discovered) = discovery_over(&["python", "go"]);
    let conn = Connection::open_in_memory().unwrap();

    assert!(
        schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
        "a fresh index always owes a walk"
    );
    assert!(
        !schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
        "nothing has changed, so the index it just stamped must satisfy its own check"
    );

    // Only the *second* language's plugin is rebuilt - the one a
    // single-bundled-plugin generation string would have said nothing
    // about.
    rebuild_with_a_change(&dirs[1]);

    assert!(
        schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
        "a rebuilt plugin must cost the index it filled a full re-walk"
    );
    assert!(
        !schema::ensure_current(&conn, &indexer_version(&discovered)).unwrap(),
        "and the generation it re-stamped must be the rebuilt one"
    );
}

/// Against the real bundled plugin rather than a stub: the generation a
/// daemon on this machine would actually compute names a readable build,
/// not [`plugin::FINGERPRINT_UNAVAILABLE`] - `core/build.rs` has just
/// built the plugin discovery finds.
#[test]
fn the_real_bundled_plugin_root_produces_a_readable_generation() {
    let bundled_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let discovered = discover(&[bundled_root]).expect("the bundled root must discover cleanly");

    assert!(
        discovered.manifests.contains_key(crate::daemon::plugin::BUNDLED_LANGUAGE),
        "the bundled plugin must be among what was discovered"
    );
    for manifest in discovered.manifests.values() {
        assert_ne!(
            plugin::fingerprint(manifest),
            plugin::FINGERPRINT_UNAVAILABLE,
            "{} must be fingerprintable - `cargo test` builds it",
            manifest.language
        );
    }
    assert_eq!(
        indexer_version(&discovered),
        format!("{CURRENT_INDEXER_VERSION}+{}", plugins_digest(&discovered))
    );
}

/// Kills `pid` the way an OOM-killer would - the same out-of-band crash
/// `core/tests/plugin_crash_recovery.rs` stages, and deliberately not a
/// cooperative shutdown. `force_stop` is `SIGKILL` on Unix and
/// `TerminateProcess` on Windows, which is the same "no chance to clean
/// up" this asks for on both.
fn kill_out_of_band(pid: u32) {
    let _ = crate::process::force_stop(pid);
}

#[test]
fn routing_resolves_an_extension_to_its_language_and_ignores_case() {
    let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

    assert_eq!(registry.language_for("src/app.python-src"), Some("python"));
    assert_eq!(registry.language_for("src/App.PYTHON-SRC"), Some("python"));
    assert_eq!(registry.language_for("README.md"), None);
    assert_eq!(registry.language_for("Makefile"), None);
}

/// [`PluginRegistry::entry_points`]'s own test: built directly from
/// `PluginManifest` literals rather than [`registry_over`]'s fixture
/// plugins - `daemon::test_plugin`'s stub manifests never set
/// `[plugin.workspace] entry_points`, and this method's whole job is to
/// surface exactly that field. Proves the union-of-discovered-manifests
/// plumbing GM-273 wires `mcp::get_dependencies` through (see
/// `graph::queries::entry_point_rank_expr`'s doc comment for the other
/// end of it), at the layer that actually owns discovery's results.
#[test]
fn entry_points_unions_every_discovered_manifests_own_list_deduplicated() {
    use crate::daemon::manifest::{Capabilities, PluginManifest, WorkspaceConfig};

    let manifest = |language: &str, entry_points: &[&str]| PluginManifest {
        language: language.to_string(),
        protocol_version: 1,
        plugin_version: "0.0.0".to_string(),
        command: PathBuf::from("true"),
        args: Vec::new(),
        extensions: Vec::new(),
        fingerprint_ignore: Vec::new(),
        manifest_dir: PathBuf::from("/dev/null"),
        capabilities: Capabilities::default(),
        workspace: WorkspaceConfig {
            entry_points: entry_points.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    };

    let mut discovered = DiscoveredPlugins::default();
    discovered.manifests.insert("typescript".to_string(), manifest("typescript", &["index"]));
    // Declares the same literal as "typescript" above, on purpose - the
    // half of this test that proves dedup, not just union.
    discovered.manifests.insert("also_index".to_string(), manifest("also_index", &["index"]));
    discovered.manifests.insert("rust".to_string(), manifest("rust", &["mod.rs", "main.rs", "lib.rs"]));

    let project = tempfile::tempdir().expect("failed to create a project root");
    let state_dir = crate::storage::connection::project_dir(project.path())
        .expect("failed to resolve the fixture project's state directory");
    fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
    let registry = PluginRegistry::new(
        project.path(),
        state_dir,
        discovered,
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    );

    assert_eq!(
        registry.entry_points(),
        vec!["index".to_string(), "lib.rs".to_string(), "main.rs".to_string(), "mod.rs".to_string()],
        "sorted, deduplicated union of every discovered manifest's own entry_points"
    );
}

/// The acceptance criterion for an unclaimed extension: skipped, not an
/// error, and nothing spawned for it.
#[test]
fn a_file_no_plugin_claims_is_skipped_without_spawning_anything() {
    let (_project, _plugins, dirs, registry) = registry_over(&["python"]);
    let conn = test_plugin::empty_index();

    registry.file_changed(&conn, "README.md".to_string());
    registry.file_changed(&conn, "docs/design.md".to_string());
    registry.file_changed(&conn, "Makefile".to_string());

    assert!(
        registry.supervisors.lock().unwrap().is_empty(),
        "a file nothing claims must not bring a plugin up"
    );
    assert!(test_plugin::spawns(&dirs[0]).is_empty(), "no plugin process may have been spawned");
}

/// ...and it is reported once, however many such files arrive. The
/// `Option` returned here is exactly what `file_changed` prints, so
/// counting `Some`s counts logged lines.
#[test]
fn an_unclaimed_extension_is_reported_once_however_many_files_share_it() {
    let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

    let first = registry.unroutable_notice("README.md");
    assert!(first.is_some(), "the first unclaimed file of its kind must be reported");
    assert!(first.unwrap().contains(".md"), "the message must name the extension");

    for i in 0..50 {
        assert_eq!(
            registry.unroutable_notice(&format!("docs/page-{i}.md")),
            None,
            "every later .md file must be silent"
        );
    }

    // A genuinely new extension is genuinely new information, and gets
    // its own single line.
    assert!(registry.unroutable_notice("notes.txt").is_some());
    assert_eq!(registry.unroutable_notice("other.txt"), None);

    // Extensionless files are one more kind, reported once as a group.
    assert!(registry.unroutable_notice("Makefile").is_some());
    assert_eq!(registry.unroutable_notice("Dockerfile"), None);
}

/// The lazy-spawn acceptance criterion, asserted on processes rather than
/// on return values: two files of one language produce exactly one plugin
/// process, and the same `Arc` both times.
#[test]
fn the_second_file_of_a_language_reuses_the_first_files_plugin() {
    let (_project, _plugins, dirs, registry) = registry_over(&["python"]);
    let conn = test_plugin::empty_index();
    let python = &dirs[0];

    registry.file_changed(&conn, "src/one.python-src".to_string());
    assert_eq!(test_plugin::spawns(python).len(), 1, "the first file must spawn the plugin");
    let pid = registry.get_or_spawn("python").unwrap().pid();

    registry.file_changed(&conn, "src/two.python-src".to_string());

    assert_eq!(
        test_plugin::spawns(python).len(),
        1,
        "a second file of the same language must not spawn a second process"
    );
    assert_eq!(registry.supervisors.lock().unwrap().len(), 1);
    assert_eq!(registry.get_or_spawn("python").unwrap().pid(), pid, "still the same process");

    // And the memoized supervisor is literally the same object, not an
    // equal-looking one.
    assert!(Arc::ptr_eq(
        &registry.get_or_spawn("python").unwrap(),
        &registry.get_or_spawn("python").unwrap()
    ));
}

/// Two languages get two independent supervisors, each running its own
/// manifest's plugin - the routing table decides which, not spawn order.
#[test]
fn each_language_gets_its_own_supervisor_and_its_own_process() {
    let (_project, _plugins, dirs, registry) = registry_over(&["python", "go"]);
    let conn = test_plugin::empty_index();
    let (python, go) = (&dirs[0], &dirs[1]);

    registry.file_changed(&conn, "app.python-src".to_string());
    registry.file_changed(&conn, "main.go-src".to_string());

    assert_eq!(test_plugin::spawns(python).len(), 1);
    assert_eq!(test_plugin::spawns(go).len(), 1);
    assert_eq!(registry.get_or_spawn("python").unwrap().language(), "python");
    assert_eq!(registry.get_or_spawn("go").unwrap().language(), "go");
    assert!(!Arc::ptr_eq(&registry.get_or_spawn("python").unwrap(), &registry.get_or_spawn("go").unwrap()));
}

/// The isolation guarantee this whole shape exists for: one language's
/// plugin dying (and being recovered) leaves every other language's
/// exactly where it was.
#[test]
fn a_crash_in_one_languages_plugin_leaves_another_languages_untouched() {
    let (_project, _plugins, dirs, registry) = registry_over(&["python", "go"]);
    let conn = test_plugin::empty_index();
    let (python_dir, go_dir) = (&dirs[0], &dirs[1]);

    registry.file_changed(&conn, "app.python-src".to_string());
    registry.file_changed(&conn, "main.go-src".to_string());

    let python = registry.get_or_spawn("python").unwrap();
    let go = registry.get_or_spawn("go").unwrap();
    let crashed_pid = python.pid().expect("the python plugin must be awake");
    let go_pid = go.pid().expect("the go plugin must be awake");

    kill_out_of_band(crashed_pid);
    // The next change to a python file is what finds the plugin gone:
    // `PluginProcess` relaunches and replays it, transparently.
    registry.file_changed(&conn, "app.python-src".to_string());

    let recovered_pid = python.pid().expect("the python plugin must have been relaunched");
    assert_ne!(recovered_pid, crashed_pid, "a fresh python process must have been spawned");
    assert_eq!(
        test_plugin::spawns(python_dir).len(),
        2,
        "exactly one relaunch: the original process and its replacement"
    );

    // The whole point: nothing about `go` moved. Same process, never
    // re-spawned, still its own supervisor, still serving.
    assert_eq!(go.pid(), Some(go_pid), "the go plugin must be the very same process");
    assert!(crate::daemon::is_process_alive(go_pid), "the go plugin must still be running");
    assert_eq!(
        test_plugin::spawns(go_dir).len(),
        1,
        "the go plugin must not have been re-spawned by another language's crash"
    );
    registry.file_changed(&conn, "other.go-src".to_string());
    assert_eq!(go.pid(), Some(go_pid), "and it must still be serving off that same process");
    assert_eq!(test_plugin::spawns(go_dir).len(), 1);
}

/// How long the task-164 tests below give a plugin *process* to appear
/// once something has asked for it. Only ever hit by a genuine failure, so
/// it is generous: a `node` start under a fully parallel `cargo test` has
/// been measured taking over a second on this machine, and none of these
/// tests is about how long that takes.
const SPAWN_DEADLINE: Duration = Duration::from_secs(20);

/// How long a test waits for a reader thread it expects straight back.
/// Reaching this means the reader is blocked on the in-progress spawn -
/// which nothing releases until the test itself does, further down - so it
/// is a failure signal, not a timing measurement.
const READER_DEADLINE: Duration = Duration::from_secs(10);

/// What the reader call itself is allowed to cost once it has been shown
/// to come back at all. Deliberately loose next to the map lookup it
/// really measures: [`READER_DEADLINE`] is what catches a reader that
/// waited on the spawn, and this only has to rule out a reader that
/// somehow waited on *most* of one without a loaded machine's scheduling
/// noise failing it by accident.
const READER_BUDGET: Duration = Duration::from_millis(500);

/// Whether `plugin_dir`'s plugin has been started `count` times, waiting
/// up to [`SPAWN_DEADLINE`] for it.
///
/// A `bool` rather than an assertion because the caller usually has a gate
/// to open before it is allowed to fail (see `test_plugin::install_gated`):
/// panicking here would leave a thread spawning forever and hang the test
/// instead of failing it.
///
/// The fixture plugin records its pid before it does anything else, so
/// against a gated plugin this returns *during* the spawn - the process
/// exists, `PluginProcess::spawn` is still blocked reading its handshake -
/// which is exactly the state the tests below need, entered by observation
/// rather than by sleeping a guessed amount.
fn spawned_within(plugin_dir: &Path, count: usize) -> bool {
    let deadline = std::time::Instant::now() + SPAWN_DEADLINE;
    while test_plugin::spawns(plugin_dir).len() < count {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

/// Task 164's headline acceptance criterion: a reader of the registry is
/// not blocked behind an in-progress spawn - not even the spawn of the
/// very language it is asking about.
///
/// `has_pending` is the call that made this matter (every MCP tool call
/// asks it, via `mcp::GMeshMcpServer::replay_queued_changes`), and
/// `active_supervisors` is what it and every other reader goes through.
/// Both are called from a thread of their own while the spawn is held
/// open, so "was it blocked" is answered by whether that thread comes back
/// at all rather than by how long anything took: before this fix they took
/// the same `Mutex` `get_or_spawn` held for the whole spawn, so the reader
/// could not return until the gate below was opened - which happens after
/// it is waited for.
#[test]
fn a_reader_is_not_held_up_by_an_in_progress_spawn() {
    let (_project, _plugins, dirs, registry) = registry_over_gated(&["python"]);
    let python = &dirs[0];

    let (in_flight, measured): (bool, Option<(usize, bool, Duration)>) = std::thread::scope(|scope| {
        scope.spawn(|| {
            registry.get_or_spawn("python").expect("the fixture plugin must start");
        });
        // The process is up and waiting on its gate: the spawn is in
        // flight, and stays that way until this test says otherwise.
        let in_flight = spawned_within(python, 1);

        let measured = in_flight.then(|| {
            let (reported, reports) = std::sync::mpsc::channel();
            let reader = &registry;
            scope.spawn(move || {
                let started = std::time::Instant::now();
                let active = reader.active_supervisors().len();
                let pending = reader.has_pending();
                let _ = reported.send((active, pending, started.elapsed()));
            });
            reports.recv_timeout(READER_DEADLINE).ok()
        });

        // Before anything is asserted, whatever happened above: a
        // spawning thread that is never let go never joins, and this
        // scope would hang instead of failing.
        test_plugin::open_handshake_gate(python);
        (in_flight, measured.flatten())
    });

    assert!(in_flight, "the plugin process never started, so no spawn was ever in flight");
    let (active, pending, elapsed) =
        measured.expect("a reader never came back while a spawn was in flight - it is blocked behind it");
    // Nothing is reported yet, which is what proves the reader really did
    // run inside the spawn rather than after it.
    assert_eq!(active, 0, "a language whose spawn has not finished is not an active supervisor yet");
    assert!(!pending, "a supervisor that does not exist yet has nothing queued");
    assert!(elapsed < READER_BUDGET, "a reader spent most of a spawn waiting: {elapsed:?}");

    // ...and once the spawn lands, the same readers see it, so skipping
    // the slot was a deferral and not a drop.
    assert_eq!(registry.active_supervisors().len(), 1);
    assert_eq!(test_plugin::spawns(python).len(), 1, "exactly one process, once");
}

/// Task 154's guarantee, kept: callers racing on the same unspawned
/// language produce exactly one process, and the same supervisor for
/// everyone. Asserted against a plugin whose spawn is held open until
/// every racer has had its chance at it, rather than one that comes up so
/// fast the race is over before it starts.
#[test]
fn racing_callers_for_one_language_still_spawn_exactly_one_process() {
    let (_project, _plugins, dirs, registry) = registry_over_gated(&["python"]);
    let python = &dirs[0];

    let supervisors: Vec<Arc<PluginSupervisor>> = std::thread::scope(|scope| {
        let racers: Vec<_> = (0..4).map(|_| scope.spawn(|| registry.get_or_spawn("python"))).collect();

        // One racer has reserved the slot and its process is up; the
        // others get a moment to reach the same call before that spawn is
        // allowed to finish. Any of them that got past the reservation
        // would start a second process, which the spawn log below counts -
        // the grace period makes the race real, it is not what makes the
        // assertion true.
        let in_flight = spawned_within(python, 1);
        std::thread::sleep(Duration::from_millis(20));
        test_plugin::open_handshake_gate(python);
        assert!(in_flight, "the plugin process never started");

        racers
            .into_iter()
            .map(|racer| racer.join().expect("no racer may panic").expect("the fixture plugin must start"))
            .collect()
    });

    assert_eq!(
        test_plugin::spawns(python).len(),
        1,
        "four callers racing on one language must not start four plugin processes"
    );
    assert_eq!(registry.supervisors.lock().unwrap().len(), 1);
    for supervisor in &supervisors {
        assert!(
            Arc::ptr_eq(supervisor, &supervisors[0]),
            "every racer must be handed the one supervisor that was actually spawned"
        );
    }
}

/// The other half of what the old whole-spawn lock cost: two *different*
/// languages could not start at the same time either.
///
/// Asserted on processes rather than on elapsed time, and so without a
/// timing assumption of any kind: both plugins have to be *running* while
/// neither handshake has been let through, which under a lock held across
/// the whole spawn is impossible by construction - whichever language got
/// there first would hold the map until its own gate opened, and the
/// second language's process could not even be launched.
#[test]
fn two_languages_spawn_at_the_same_time_rather_than_one_after_the_other() {
    let (_project, _plugins, dirs, registry) = registry_over_gated(&["python", "go"]);
    let (python, go) = (&dirs[0], &dirs[1]);

    let both_up = std::thread::scope(|scope| {
        scope.spawn(|| {
            registry.get_or_spawn("python").expect("the python fixture must start");
        });
        scope.spawn(|| {
            registry.get_or_spawn("go").expect("the go fixture must start");
        });

        let both_up = spawned_within(python, 1) && spawned_within(go, 1);
        test_plugin::open_handshake_gate(python);
        test_plugin::open_handshake_gate(go);
        both_up
    });

    assert!(
        both_up,
        "one language's process never started while the other's spawn was in flight - \
         the two spawns were serialized"
    );
    assert_eq!(registry.active_supervisors().len(), 2);
}

/// A spawn that fails still memoizes nothing - the criterion the old
/// implementation met by never inserting anything, and this one has to
/// meet by removing the reservation it did insert. The callers that were
/// waiting on it are answered with its failure rather than each repeating
/// it, and the language is spawnable again the moment its plugin works.
#[test]
fn a_failed_spawn_memoizes_nothing_and_answers_everyone_waiting_on_it() {
    let (_project, plugins, dirs, registry) = registry_over(&["python"]);
    // A plugin whose runtime is broken: it stays up long enough for the
    // other callers to pile in behind it, then exits without ever sending
    // a handshake, which is what `PluginProcess::spawn` fails on.
    fs::write(
        dirs[0].join("plugin.js"),
        "// Generated by daemon::registry's tests - a plugin that never handshakes.\n\
         setTimeout(() => process.exit(1), 200);\n",
    )
    .expect("failed to break the fixture plugin");

    let failures: Vec<String> = std::thread::scope(|scope| {
        let racers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| match registry.get_or_spawn("python") {
                    Ok(_) => panic!("a plugin that never handshakes must not start"),
                    Err(err) => format!("{err:#}"),
                })
            })
            .collect();
        racers.into_iter().map(|racer| racer.join().expect("no racer may panic")).collect()
    });

    assert_eq!(failures.len(), 3);
    for failure in &failures {
        assert!(failure.contains("python"), "the failure must name the language: {failure}");
    }
    assert!(
        registry.supervisors.lock().unwrap().is_empty(),
        "a failed spawn - reservation and all - must leave the map exactly as it found it"
    );

    // Nothing is wedged: the next caller after the plugin is fixed gets a
    // real supervisor, which a leftover reservation would have made
    // impossible (it would wait forever on a spawn that already ended).
    test_plugin::install(plugins.path(), "python", &[extension_for("python").as_str()]);
    let supervisor = registry.get_or_spawn("python").expect("the repaired plugin must start");
    assert_eq!(supervisor.language(), "python");
    assert_eq!(registry.active_supervisors().len(), 1);
}

#[test]
fn a_language_nothing_was_discovered_for_is_an_error_naming_what_was() {
    let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

    let message = match registry.get_or_spawn("rust") {
        Ok(_) => panic!("a language nothing was discovered for must not spawn anything"),
        Err(err) => format!("{err:#}"),
    };
    assert!(message.contains("rust"), "{message}");
    assert!(message.contains("python"), "{message}");
    assert!(registry.supervisors.lock().unwrap().is_empty(), "nothing may be memoized");
}

#[test]
fn each_language_records_its_pid_in_a_file_of_its_own() {
    let (_project, _plugins, _dirs, registry) = registry_over(&["python", "go"]);

    let python = registry.pid_file_for("python");
    let go = registry.pid_file_for("go");
    assert_ne!(python, go);
    assert_eq!(python.file_name().unwrap().to_str().unwrap(), "plugin-python.pid");
    assert_eq!(python.parent(), go.parent(), "both live in the project's state directory");
}

#[test]
fn an_empty_discovery_result_routes_nothing_and_starts_nothing() {
    let (_project, _plugins, _dirs, registry) = registry_over(&[]);
    let conn = test_plugin::empty_index();

    assert_eq!(registry.language_for("app.py"), None);
    registry.file_changed(&conn, "app.py".to_string());
    assert!(registry.supervisors.lock().unwrap().is_empty());
}

// -----------------------------------------------------------------
// GM-272: workspace-file routing (`workspace_language_matches`,
// `under_excluded_dir`, `route_settled_path`). The end-to-end "a
// matched language actually gets reindexed" acceptance test lives in
// `daemon::workspace_reindex`'s own test module, alongside the rest of
// the reindex machinery it exercises - what belongs here is the pure
// routing *decision*: which language(s) a settled path matches, and
// whether `exclude_dirs` rules a match out, both answerable with no
// plugin process involved at all.
// -----------------------------------------------------------------

#[test]
fn file_name_of_returns_the_final_path_segment() {
    assert_eq!(file_name_of("go.mod"), "go.mod");
    assert_eq!(file_name_of("a/b/go.mod"), "go.mod");
    assert_eq!(file_name_of(""), "");
}

#[test]
fn under_excluded_dir_matches_a_whole_segment_at_any_depth_never_a_prefix() {
    let excluded = vec!["vendor".to_string()];

    assert!(under_excluded_dir("vendor/build.alpha", &excluded), "a leading segment must match");
    assert!(
        under_excluded_dir("pkg/vendor/deep/build.alpha", &excluded),
        "a segment at any depth must match, not only a leading one"
    );
    assert!(
        !under_excluded_dir("vendored-tools/build.alpha", &excluded),
        "a segment that merely starts with the excluded name is not a match - \
         no prefix matching"
    );
    assert!(
        !under_excluded_dir("build.alpha", &excluded),
        "the file name itself is never checked as a directory segment"
    );
    assert!(
        !under_excluded_dir("src/vendor.go", &excluded),
        "the excluded name appearing only in the file name, not as a directory, is not a match"
    );
}

#[test]
fn under_excluded_dir_is_false_with_no_exclude_dirs_configured() {
    assert!(!under_excluded_dir("vendor/build.alpha", &[]));
}

/// A registry over one fake language installed with `[plugin.workspace]
/// watch_files`/`exclude_dirs`, via [`test_plugin::install_with_workspace`],
/// the workspace-routing analog of [`registry_over`], which only ever
/// installs bare extension-claiming fixtures.
fn registry_over_workspace(
    language: &str,
    extension: &str,
    watch_files: &[&str],
    exclude_dirs: &[&str],
) -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PluginRegistry) {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");

    let dir = test_plugin::install_with_workspace(
        plugins.path(),
        language,
        &[extension],
        watch_files,
        exclude_dirs,
    );

    let discovered = discover(&[plugins.path().to_path_buf()]).expect("the fixture must discover cleanly");
    let state_dir = crate::storage::connection::project_dir(project.path())
        .expect("failed to resolve the fixture project's state directory");
    std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
    let registry = PluginRegistry::new(
        project.path(),
        state_dir,
        discovered,
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    );
    (project, plugins, dir, registry)
}

#[test]
fn workspace_language_matches_an_exact_watched_file_name_in_any_directory() {
    let (_project, _plugins, _dir, registry) =
        registry_over_workspace("alpha", ".alpha-src", &["go.mod"], &[]);

    assert_eq!(registry.workspace_language_matches("go.mod"), vec!["alpha".to_string()]);
    assert_eq!(
        registry.workspace_language_matches("nested/dir/go.mod"),
        vec!["alpha".to_string()],
        "watch_files matches by file name alone, any directory"
    );
    assert!(registry.workspace_language_matches("go.sum").is_empty(), "a different file name must not match");
}

/// This task's own glob-matching acceptance criterion (e.g. `*.csproj`
/// from the architecture doc's paper stress test), reproduced with a
/// fixture extension so it exercises the exact same `Glob` compilation
/// path a real `*.csproj` pattern would.
#[test]
fn workspace_language_matches_a_glob_pattern() {
    let (_project, _plugins, _dir, registry) =
        registry_over_workspace("csharp", ".cs-src", &["*.csproj"], &[]);

    assert_eq!(registry.workspace_language_matches("MyProject.csproj"), vec!["csharp".to_string()]);
    assert_eq!(registry.workspace_language_matches("src/nested/Other.csproj"), vec!["csharp".to_string()]);
    assert!(
        registry.workspace_language_matches("MyProject.sln").is_empty(),
        "a name the glob does not match must not match"
    );
}

#[test]
fn workspace_language_matches_excludes_a_path_under_the_manifests_own_exclude_dirs() {
    let (_project, _plugins, _dir, registry) =
        registry_over_workspace("alpha", ".alpha-src", &["go.mod"], &["vendor"]);

    assert_eq!(
        registry.workspace_language_matches("go.mod"),
        vec!["alpha".to_string()],
        "an ordinary path is still routed"
    );
    assert!(
        registry.workspace_language_matches("vendor/go.mod").is_empty(),
        "a go.mod-like file under an excluded directory must not be routed at all"
    );
}

/// A manifest whose `watch_files` is empty (every fixture predating
/// GM-272, and the bundled TS plugin itself) never matches anything -
/// the construction this module's `notify_workspace_changed` doc comment
/// relies on to say a plugin like TS never even reaches the workspace-
/// routing path.
#[test]
fn a_manifest_with_no_watch_files_never_matches_the_workspace_routing() {
    let (_project, _plugins, _dirs, registry) = registry_over(&["python"]);

    assert!(registry.workspace_language_matches("go.mod").is_empty());
    assert!(registry.workspace_language_matches("anything").is_empty());
}

#[test]
fn route_settled_path_falls_back_to_ordinary_extension_routing_with_no_workspace_match() {
    let (_project, _plugins, dirs, registry) = registry_over(&["python"]);
    let conn = test_plugin::empty_index();

    registry.route_settled_path(&conn, "src/app.python-src".to_string());

    assert_eq!(
        test_plugin::spawns(&dirs[0]).len(),
        1,
        "an ordinary source file with no workspace match must still reach the plugin \
         through extension routing"
    );
}

#[test]
fn file_changed_does_not_route_a_path_under_its_own_languages_exclude_dirs() {
    let (_project, _plugins, dir, registry) =
        registry_over_workspace("alpha", ".alpha-src", &[], &["vendor"]);
    let conn = test_plugin::empty_index();

    registry.file_changed(&conn, "vendor/thirdparty.alpha-src".to_string());
    assert!(
        test_plugin::spawns(&dir).is_empty(),
        "a file under the claiming language's own exclude_dirs must not be routed at all"
    );

    registry.file_changed(&conn, "src/app.alpha-src".to_string());
    assert_eq!(
        test_plugin::spawns(&dir).len(),
        1,
        "an ordinary file of the same language, outside exclude_dirs, must still route"
    );
}
