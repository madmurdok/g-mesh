use super::*;
use crate::storage::schema;

/// The plugins this checkout ships - their real `plugin.toml`s, read from
/// `plugins/` beside `core/` exactly as `manifest::bundled_roots` finds them in
/// a checkout. Discovery only parses the manifests, so no plugin needs to be
/// built. Deliberately not `default_roots`, which would also read whatever
/// the machine running the tests has under `~/.g-mesh/plugins/`.
fn bundled_plugins() -> DiscoveredPlugins {
    let checkout_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    manifest::discover(&[checkout_root]).unwrap()
}

/// A project directory with the given files, and an index database
/// alongside it that no daemon owns.
struct Fixture {
    project: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let project = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let full = project.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, contents).unwrap();
        }
        Self { project, state: tempfile::tempdir().unwrap() }
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn db_path(&self) -> PathBuf {
        self.state.path().join("index.db")
    }

    /// Opens (creating on first call) the index this fixture's status is
    /// computed against.
    fn index(&self) -> Connection {
        let conn = Connection::open(self.db_path()).unwrap();
        schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
        conn
    }

    /// Records the `File` node the plugin would emit for `path`.
    fn index_file(&self, conn: &Connection, path: &str, has_syntax_errors: bool) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol,
                                    endLine, endCol, language, hasSyntaxErrors)
                 VALUES (?1, 'File', ?1, ?1, ?1, 1, 0, 1, 0, 'typescript', ?2)",
            rusqlite::params![path, has_syntax_errors],
        )
        .unwrap();
    }

    /// Records the on-disk baseline the incremental path would write.
    fn record_baseline(&self, conn: &Connection, path: &str, mtime_millis: i64) {
        conn.execute(
            "INSERT INTO indexed_files (filePath, mtimeMillis, contentHash)
                 VALUES (?1, ?2, 'hash')",
            rusqlite::params![path, mtime_millis],
        )
        .unwrap();
    }

    fn current_mtime(&self, path: &str) -> i64 {
        mtime_millis(&fs::metadata(self.root().join(path)).unwrap()).unwrap()
    }

    fn status(&self) -> IndexStatus {
        index_status(self.root(), &self.db_path(), &bundled_plugins()).unwrap()
    }
}

#[test]
fn a_project_with_no_index_owes_work_for_every_file_it_has() {
    let fixture = Fixture::new(&[("a.ts", "export const a = 1;"), ("b.ts", "export const b = 2;")]);

    let status = fixture.status();

    assert_eq!(status.discovered, 2);
    assert_eq!(status.indexed, 0);
    assert_eq!(status.dirty, 2);
    assert!(!status.bulk_indexed);
    assert_eq!(status.coverage(), 0.0);
}

#[test]
fn a_fully_indexed_project_reports_complete_coverage_and_nothing_dirty() {
    let fixture = Fixture::new(&[("a.ts", "export const a = 1;"), ("src/b.tsx", "export const b = 2;")]);
    let conn = fixture.index();
    fixture.index_file(&conn, "a.ts", false);
    fixture.index_file(&conn, "src/b.tsx", false);
    // GM-264: the roll-up `record_bulk_index` now checks fires only once
    // every *present* language (here, just "typescript" - `index_file`'s
    // own doc comment) has its own `language_state.bulkIndexedAt` set -
    // see `storage::schema::record_bulk_index`'s doc comment.
    schema::record_language_bulk_indexed(&conn, "typescript", None).unwrap();
    schema::record_bulk_index(&conn).unwrap();

    let status = fixture.status();

    assert_eq!(status.discovered, 2);
    assert_eq!(status.indexed, 2);
    assert_eq!(status.dirty, 0, "a bulk-indexed file with no baseline is not stale");
    assert!(status.bulk_indexed);
    assert_eq!(status.coverage(), 1.0);
}

/// The acceptance criterion's "known dirty-queue size": one file the
/// index has never seen, and one whose recorded baseline no longer
/// matches what is on disk.
#[test]
fn dirty_counts_never_indexed_files_and_files_whose_baseline_went_stale() {
    let fixture = Fixture::new(&[
        ("fresh.ts", "export const fresh = 1;"),
        ("edited.ts", "export const edited = 1;"),
        ("new.ts", "export const brand_new = 1;"),
    ]);
    let conn = fixture.index();
    fixture.index_file(&conn, "fresh.ts", false);
    fixture.index_file(&conn, "edited.ts", false);
    // `new.ts` deliberately has no node at all.
    fixture.record_baseline(&conn, "fresh.ts", fixture.current_mtime("fresh.ts"));
    // A baseline from before the file was last written.
    fixture.record_baseline(&conn, "edited.ts", fixture.current_mtime("edited.ts") - 10_000);

    let status = fixture.status();

    assert_eq!(status.discovered, 3);
    assert_eq!(status.indexed, 2);
    assert_eq!(status.dirty, 2, "the never-indexed file and the stale one");
    assert!((status.coverage() - 2.0 / 3.0).abs() < f64::EPSILON);
}

#[test]
fn files_the_plugin_could_only_partly_parse_are_listed() {
    let fixture = Fixture::new(&[
        ("broken.ts", "export function ("),
        ("also-broken.ts", "class {"),
        ("fine.ts", "export const fine = 1;"),
    ]);
    let conn = fixture.index();
    fixture.index_file(&conn, "broken.ts", true);
    fixture.index_file(&conn, "also-broken.ts", true);
    fixture.index_file(&conn, "fine.ts", false);

    let status = fixture.status();

    assert_eq!(status.syntax_error_files, vec!["also-broken.ts", "broken.ts"], "sorted");
}

/// The walk has to agree with the one that built the index, or every file
/// the plugin never looked at would read as a permanent coverage hole.
#[test]
fn the_walk_skips_exactly_what_the_plugins_walk_skips() {
    let fixture = Fixture::new(&[
        ("keep.ts", ""),
        ("keep.mjs", ""),
        ("README.md", ""),
        ("styles.css", ""),
        (".gitignore", "ignored/\n"),
        ("ignored/hidden.ts", ""),
        ("node_modules/dep/index.ts", ""),
        ("dist/bundle.js", ""),
        (".git/hooks/pre-commit.js", ""),
        (".claude/worktrees/copy/a.ts", ""),
    ]);

    let mut found: Vec<String> = discover_source_files(fixture.root(), &bundled_plugins())
        .unwrap()
        .into_iter()
        .map(|f| f.relative)
        .collect();
    found.sort();

    assert_eq!(found, vec!["keep.mjs", "keep.ts"]);
}

/// Every discovered language's files count, in coverage and in the dirty
/// queue alike - not only the extensions of the TS plugin.
#[test]
fn rust_and_python_files_count_toward_coverage_and_the_dirty_queue() {
    let fixture = Fixture::new(&[
        ("src/lib.rs", "pub fn a() {}"),
        ("src/edited.rs", "pub fn b() {}"),
        ("app/main.py", "def main(): pass"),
        ("app/new.py", "def new(): pass"),
        ("web/index.ts", "export const a = 1;"),
    ]);
    let conn = fixture.index();
    fixture.index_file(&conn, "src/lib.rs", false);
    fixture.index_file(&conn, "src/edited.rs", false);
    fixture.index_file(&conn, "app/main.py", false);
    fixture.index_file(&conn, "web/index.ts", false);
    // `app/new.py` has no node at all; `src/edited.rs`'s baseline predates
    // its last write.
    fixture.record_baseline(&conn, "src/edited.rs", fixture.current_mtime("src/edited.rs") - 10_000);

    let status = fixture.status();

    assert_eq!(status.discovered, 5, "every language's files are discovered, not only JS/TS");
    assert_eq!(status.indexed, 4);
    assert_eq!(status.dirty, 2, "the never-indexed .py and the stale .rs");
}

/// Each language's `[plugin.workspace] exclude_dirs` applies to that
/// language's files only: Rust's `target/` and Python's `.venv/` are not
/// counted, while a Python file under TypeScript's `dist/` and a TypeScript
/// file under Rust's `target/` still are - the plugins that own them walk
/// those directories.
#[test]
fn each_languages_excluded_dirs_hide_only_that_languages_files() {
    let fixture = Fixture::new(&[
        ("src/lib.rs", ""),
        ("target/debug/build/out.rs", ""),
        ("pkg/mod.py", ""),
        (".venv/lib/site.py", ""),
        ("pkg/__pycache__/cached.py", ""),
        ("dist/tool.py", ""),
        ("dist/bundle.js", ""),
        ("target/generated.ts", ""),
    ]);

    let mut found: Vec<String> = discover_source_files(fixture.root(), &bundled_plugins())
        .unwrap()
        .into_iter()
        .map(|f| f.relative)
        .collect();
    found.sort();

    assert_eq!(found, vec!["dist/tool.py", "pkg/mod.py", "src/lib.rs", "target/generated.ts"]);
}

#[test]
fn a_project_with_no_source_files_is_covered_rather_than_dividing_by_zero() {
    let fixture = Fixture::new(&[("README.md", "# nothing to index")]);

    let status = fixture.status();

    assert_eq!(status.discovered, 0);
    assert_eq!(status.coverage(), 1.0);
    assert_eq!(status.dirty, 0);
}

#[test]
fn a_report_renders_every_field_it_was_asked_for() {
    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::Running { pid: 4242 },
        build: BuildState::Current,
        plugins: vec![PluginReport {
            language: "typescript".to_string(),
            state: PluginState::Active { pid: 4243 },
        }],
        suspended_languages: Vec::new(),
        last_used: Some(LastUsed {
            timestamp: "2026-07-31 00:00:00.000".to_string(),
            idle: Duration::from_secs(3 * 60 * 60),
        }),
        index: IndexStatus {
            bulk_indexed: true,
            semantic_pass_completed: true,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 4,
            indexed: 3,
            dirty: 1,
            syntax_error_files: vec!["src/broken.ts".to_string()],
        },
        phase: None,
        front: None,
    };

    let rendered = render(&report);

    assert!(rendered.contains("running (pid 4242)"), "{rendered}");
    assert!(rendered.contains("daemon build:    this build"), "{rendered}");
    assert!(rendered.contains("active (pid 4243)"), "{rendered}");
    assert!(rendered.contains("3 hours ago"), "{rendered}");
    assert!(rendered.contains("75.0% (3/4 source files)"), "{rendered}");
    assert!(rendered.contains("1 awaiting reindex"), "{rendered}");
    assert!(rendered.contains("src/broken.ts"), "{rendered}");
    assert!(rendered.contains("semantic pass:   complete"), "{rendered}");
}

/// The gap task 62cc2d0f closes: a walked index whose semantic pass never
/// finished must not read as fully healthy just because `bulk_indexed` is
/// true - `status` is the explicit-surfacing half of the fix (the other
/// half is the daemon retrying on its own next start).
#[test]
fn a_walked_index_with_no_completed_semantic_pass_is_called_out() {
    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::NotRunning,
        build: BuildState::NotRunning,
        plugins: Vec::new(),
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed: true,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 4,
            indexed: 4,
            dirty: 0,
            syntax_error_files: Vec::new(),
        },
        phase: None,
        front: None,
    };

    let rendered = render(&report);

    assert!(
        rendered.contains("semantic pass:   never completed - run `g-mesh reindex` to repair it"),
        "an interrupted semantic pass must be surfaced explicitly, not silently folded into \
         a healthy-looking report:\n{rendered}"
    );
}

/// A language whose last semantic pass failed is named with its recorded
/// reason, read from the index itself, and the generic "never completed"
/// advice is left out for it.
#[test]
fn a_recorded_semantic_pass_failure_is_shown_with_its_reason_instead_of_the_generic_advice() {
    let fixture = Fixture::new(&[("src/a.ts", "export const a = 1;\n")]);
    let conn = fixture.index();
    fixture.index_file(&conn, "src/a.ts", false);
    schema::record_language_bulk_indexed(&conn, "typescript", None).unwrap();
    schema::record_bulk_index(&conn).unwrap();
    schema::record_language_semantic_pass_failure(
        &conn,
        "typescript",
        "the plugin reported an incomplete whole-project semantic pass: the language server exited during the pass",
    )
    .unwrap();

    let index = fixture.status();
    assert_eq!(index.semantic_pass_owed, vec!["typescript".to_string()]);
    let rendered = render(&Report {
        project_root: fixture.root().to_path_buf(),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::NotRunning,
        build: BuildState::NotRunning,
        plugins: Vec::new(),
        suspended_languages: Vec::new(),
        last_used: None,
        index,
        phase: None,
        front: None,
    });

    assert!(
        rendered.contains(
            "  semantic pass:   typescript failed - the plugin reported an incomplete whole-project semantic \
             pass: the language server exited during the pass\n"
        ),
        "{rendered}"
    );
    assert!(!rendered.contains("never completed"), "{rendered}");
}

/// The advice stays for an owed language with no recorded failure, beside
/// the line for one that has one.
#[test]
fn the_generic_advice_stays_for_an_owed_language_with_no_recorded_failure() {
    let lines = semantic_pass_lines(
        false,
        &["python".to_string(), "rust".to_string()],
        &[("python".to_string(), "the server exited".to_string())],
    );
    assert_eq!(
        lines,
        vec![
            "  semantic pass:   never completed - run `g-mesh reindex` to repair it".to_string(),
            "  semantic pass:   python failed - the server exited".to_string(),
        ]
    );
}

/// A pass that was not run because its plugin was asleep is still owed, not
/// failed: status says it is pending and why.
#[test]
fn a_pass_deferred_by_a_sleeping_plugin_reads_as_pending_not_failed() {
    let lines = semantic_pass_lines(
        false,
        &["python".to_string()],
        &[("python".to_string(), crate::daemon::semantic::NOT_RUN_REASON.to_string())],
    );
    assert_eq!(
        lines,
        vec!["  semantic pass:   python pending - its plugin was asleep or memory-suspended; \
             the next daemon start or `g-mesh reindex` asks again"
            .to_string(),]
    );
}

/// Task 108: a daemon mid-cold-start-walk (task 105 already made it
/// reachable and answering, not merely running) must not be reported next
/// to a line implying nothing is happening or that a restart is owed - the
/// walk already in progress is exactly what would do the work a "cold
/// start still owed" reading would suggest is missing.
#[test]
fn a_daemon_mid_cold_start_walk_reports_the_walk_in_progress_not_a_cold_start_owed() {
    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::Running { pid: 4242 },
        build: BuildState::Current,
        plugins: vec![PluginReport {
            language: "typescript".to_string(),
            state: PluginState::Active { pid: 4243 },
        }],
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 4,
            indexed: 1,
            dirty: 3,
            syntax_error_files: Vec::new(),
        },
        phase: Some("walking".to_string()),
        front: None,
    };

    let rendered = render(&report);

    assert!(rendered.contains("running (pid 4242)"), "{rendered}");
    assert!(rendered.contains("building now"), "{rendered}");
    assert!(
        !rendered.contains("cold start is still owed"),
        "a live daemon is already doing the walk this line would suggest is missing:\n{rendered}"
    );
    assert!(
        rendered.contains("3 awaiting the walk already in progress"),
        "the dirty count must not read as work nobody is doing:\n{rendered}"
    );
}

/// A minimal fixture for the GM-395 slice 2b tests below, which only vary
/// `phase` and `bulk_indexed` - everything else about a live, unwalked
/// project is incidental to what they check.
fn phase_fixture(bulk_indexed: bool, phase: Option<&str>) -> Report {
    Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::Running { pid: 4242 },
        build: BuildState::Current,
        plugins: Vec::new(),
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 4,
            indexed: if bulk_indexed { 4 } else { 0 },
            dirty: 4,
            syntax_error_files: Vec::new(),
        },
        phase: phase.map(str::to_string),
        front: None,
    }
}

/// D13 in `docs/architecture/lazy-indexing.md`: an idle daemon that owns
/// its project but has never been asked to walk it (GM-395's lazy
/// activation) must read as "not indexed yet", not as the pre-GM-395
/// "cold start still owed" - that line implied nothing was happening
/// about it, which a live daemon waiting for the first tool call is not.
#[test]
fn unindexed_phase_reports_not_indexed_yet_rather_than_cold_start_owed() {
    let rendered = render(&phase_fixture(false, Some("unindexed")));
    assert!(
        rendered.contains("index:           not indexed yet - builds on the first tool call"),
        "{rendered}"
    );
    assert!(!rendered.contains("cold start is still owed"), "{rendered}");
    assert!(!rendered.contains("building now"), "an unindexed project is idle, not building:\n{rendered}");
}

/// D13: `embedding` is the phase covering "the structural walk is done
/// and answering, the embedding backfill pass is running" - distinct from
/// both `walking` (no structural answers yet) and silence (nothing left
/// to say once the whole project, embeddings included, is `ready`).
#[test]
fn embedding_phase_reports_structural_ready_with_embeddings_in_progress() {
    let rendered = render(&phase_fixture(true, Some("embedding")));
    assert!(
        rendered.contains("index:           structural index ready; embeddings being computed"),
        "{rendered}"
    );
}

/// D13: `failed` names what happened and that the next tool call retries
/// it (`IndexingStatus::activation_failed`) - the daemon log is where the
/// actual failure message lives (`Phase::Failed`'s own doc comment), so
/// this line only has to point there, not repeat it.
#[test]
fn failed_phase_reports_the_last_build_failed_and_will_be_retried() {
    let rendered = render(&phase_fixture(false, Some("failed")));
    assert!(
        rendered
            .contains("index:           last build failed - see daemon log; retried on the next tool call"),
        "{rendered}"
    );
}

/// `structural` and `ready` are deliberately silent on this line (D13
/// names messages only for `unindexed`, `walking`, `embedding` and
/// `failed`) - once the walk itself is done, `status` has nothing left to
/// add here that the coverage/dirty lines below do not already say.
#[test]
fn structural_and_ready_phases_print_no_index_line() {
    for phase in ["structural", "ready"] {
        let rendered = render(&phase_fixture(true, Some(phase)));
        assert!(
            !rendered.contains("index:"),
            "phase {phase:?} must not print an \"index:\" line (\"index coverage:\" is a different \
             line and is unaffected):\n{rendered}"
        );
    }
}

#[test]
fn a_dead_project_renders_as_such_without_pretending_to_know_pids() {
    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::NotRunning,
        build: BuildState::NotRunning,
        plugins: vec![PluginReport {
            language: "typescript".to_string(),
            state: PluginState::Orphaned { pid: 99 },
        }],
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 2,
            indexed: 0,
            dirty: 2,
            syntax_error_files: Vec::new(),
        },
        phase: None,
        front: None,
    };

    let rendered = render(&report);

    assert!(rendered.contains("daemon core:     not running"), "{rendered}");
    assert!(
        !rendered.contains("daemon build:"),
        "a project with no daemon has no build to report on:\n{rendered}"
    );
    assert!(rendered.contains("orphaned (pid 99)"), "{rendered}");
    assert!(rendered.contains("never recorded"), "{rendered}");
    assert!(rendered.contains("never fully walked"), "{rendered}");
    assert!(rendered.contains("syntax errors:   none"), "{rendered}");
}

/// A pid file naming a live process under a live core reads as active;
/// under no core at all it reads as orphaned - `classify_plugin` only
/// ever runs on a pid `plugin_reports` has already confirmed is alive, so
/// there is no "nothing recorded" case left for it to classify (see
/// [`PluginState`]'s doc comment for where that case went).
#[test]
fn a_live_pid_reads_as_active_under_a_core_and_orphaned_without_one() {
    assert_eq!(classify_plugin(7, CoreState::Running { pid: 1 }), PluginState::Active { pid: 7 });
    assert_eq!(
        classify_plugin(7, CoreState::NotAccepting { pid: 1 }),
        PluginState::Active { pid: 7 },
        "a core that is up but not yet accepting connections still counts as serving it"
    );
    assert_eq!(classify_plugin(7, CoreState::NotRunning), PluginState::Orphaned { pid: 7 });

    let described = describe_plugin(PluginState::Orphaned { pid: 7 });
    assert!(described.contains("orphaned"), "{described}");
    assert!(described.contains("g-mesh stop"), "{described}");
}

/// [`plugin_reports`]'s own acceptance criterion: one entry per
/// `plugin-<language>.pid` file actually present, sorted, a dead pid file
/// dropped rather than reported, and a project with none at all reads as
/// an empty list rather than an error - `status` must never panic or fail
/// just because no language has ever been touched yet.
#[test]
fn plugin_reports_lists_one_entry_per_live_pid_file_sorted_by_language() {
    let state = tempfile::tempdir().unwrap();
    let live = std::process::id();
    fs::write(state.path().join("plugin-typescript.pid"), live.to_string()).unwrap();
    fs::write(state.path().join("plugin-python.pid"), live.to_string()).unwrap();
    // A pid nothing alive holds - large, and vanishingly unlikely to
    // collide with a real process on the machine running this test.
    fs::write(state.path().join("plugin-go.pid"), "999999999").unwrap();

    let reports = plugin_reports(state.path(), CoreState::Running { pid: 1 });

    assert_eq!(
        reports,
        vec![
            PluginReport { language: "python".to_string(), state: PluginState::Active { pid: live } },
            PluginReport { language: "typescript".to_string(), state: PluginState::Active { pid: live } },
        ],
        "sorted by language, and the dead go.pid dropped rather than reported: {reports:?}"
    );
}

/// A project nothing has ever spawned a plugin for (or one where every
/// language has since gone idle - the registry removes a language's pid
/// file on every sleep, same as before) renders one summary line, not a
/// per-language guess this command cannot actually back up.
#[test]
fn a_report_with_no_plugin_pid_files_renders_a_summary_line() {
    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::Running { pid: 4242 },
        build: BuildState::Current,
        plugins: Vec::new(),
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed: true,
            semantic_pass_completed: true,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 0,
            indexed: 0,
            dirty: 0,
            syntax_error_files: Vec::new(),
        },
        phase: None,
        front: None,
    };

    let rendered = render(&report);

    assert!(rendered.contains("none active"), "{rendered}");
    assert!(
        !rendered.contains("plugin ("),
        "no per-language line may be printed when nothing has a pid file:\n{rendered}"
    );
}

/// The whole point of the line: a daemon left behind by an upgrade
/// answers everything else in this report perfectly well, so this is the
/// only place the report can say something is wrong at all.
#[test]
fn a_daemon_left_behind_by_an_upgrade_is_called_out_with_what_to_do_about_it() {
    for (state, expected) in [
        (BuildState::Outdated, "older than this g-mesh"),
        (BuildState::PluginChanged, "JS/TS plugin that has been rebuilt"),
        (BuildState::Unknown, "published no build stamp"),
    ] {
        let described = describe_build(state).expect("a running daemon always reports a build");
        assert!(described.contains(expected), "{state:?} rendered as {described}");
        assert!(described.contains("g-mesh stop"), "{state:?} must say what to do: {described}");
    }
}

/// A plugin-only rebuild must not be reported as an old core binary: the
/// binary is the one the person asking has just built, and being told
/// otherwise sends them to look at the wrong half of the pipeline.
#[test]
fn a_daemon_holding_a_rebuilt_plugin_is_named_as_that_and_not_as_an_old_binary() {
    let state = tempfile::tempdir().unwrap();
    let mut incumbent = build_stamp::of_running_process().unwrap();
    incumbent.plugin = format!("{}-before", incumbent.plugin);
    build_stamp::write(&daemon::build_stamp_path_in(state.path()), &incumbent).unwrap();

    assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::PluginChanged);
    let described = describe_build(BuildState::PluginChanged).unwrap();
    assert!(!described.contains("older than this g-mesh"), "{described}");
}

#[test]
fn a_daemon_that_published_this_builds_stamp_reads_as_current() {
    let state = tempfile::tempdir().unwrap();
    let ours = build_stamp::of_running_process().unwrap();
    build_stamp::write(&daemon::build_stamp_path_in(state.path()), &ours).unwrap();

    assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Current);
    // Still comparable for a daemon that is up but not answering on its
    // socket: the stamp is published before the bind, not after.
    assert_eq!(build_state(CoreState::NotAccepting { pid: 1 }, state.path()), BuildState::Current);
}

#[test]
fn a_daemon_that_published_an_older_builds_stamp_reads_as_outdated() {
    let state = tempfile::tempdir().unwrap();
    let mut older = build_stamp::of_running_process().unwrap();
    older.exe_mtime_millis -= 24 * 60 * 60 * 1000;
    build_stamp::write(&daemon::build_stamp_path_in(state.path()), &older).unwrap();

    assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Outdated);
}

/// The state every daemon in the wild is in the first time a build
/// carrying this check meets it.
#[test]
fn a_daemon_that_published_no_stamp_at_all_reads_as_uncomparable() {
    let state = tempfile::tempdir().unwrap();

    assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Unknown);
    assert_eq!(build_state(CoreState::NotRunning, state.path()), BuildState::NotRunning);
}

#[test]
fn idle_durations_read_the_way_a_human_would_say_them() {
    assert_eq!(humanize(Duration::from_secs(3)), "just now");
    assert_eq!(humanize(Duration::from_secs(60)), "1 minute ago");
    assert_eq!(humanize(Duration::from_secs(45 * 60)), "45 minutes ago");
    assert_eq!(humanize(Duration::from_secs(2 * 60 * 60)), "2 hours ago");
    assert_eq!(humanize(Duration::from_secs(90 * 24 * 60 * 60)), "90 days ago");
}

/// Task GM-274's status acceptance criterion, exercised end to end
/// through the real production write path rather than a hand-written
/// marker: a real `PluginSupervisor` (over the same memory-hungry fixture
/// `daemon::lifecycle`'s own acceptance test uses) whose process tree is
/// found over an artificially low `memoryLimitMb` writes a real
/// `plugin-<language>.suspended` marker into this project's *actual*
/// state directory (`storage::connection::project_dir`, not an arbitrary
/// tempdir - the same one `collect` resolves for any project), and
/// `status::collect`/`render` - a *separate* process's-worth of code from
/// the daemon that wrote it, reading only off disk - reports it.
///
/// # GM-390: reached through the injected sampler, not the real one
///
/// This used to call `check_memory_limit()`, the real, OS-backed sampler -
/// and so, exactly like `daemon::lifecycle`'s own acceptance test
/// before GM-340 fixed it there, depended on `sysinfo` reporting this
/// fixture's 200MB buffer as over `memoryLimitMb` on *two* consecutive
/// whole-system scans. Seen failing on x86_64-apple-darwin: a first scan
/// read 230MB (over the 100MB limit), the confirming one 30MB - roughly a
/// bare Node process's idle RSS, i.e. the confirming scan did not see the
/// held buffer at all. `daemon::memory`'s own doc comment says why a
/// whole-tree snapshot can move like that between two calls milliseconds
/// apart with nothing about the fixture having changed: a shared runner
/// under other concurrent tests' process churn, or swap pressure, is
/// enough - `process_tree_sample`'s doc comment measured falls of
/// 170-200MB between two such snapshots taken a few milliseconds apart on
/// this repository's own `cargo test --workspace`, under real load.
///
/// What this test is judged on is not "can two real samples confirm each
/// other" - that guard's own decision logic (an unconfirmed reading
/// leaves the plugin running; a confirmed one suspends it; an under-limit
/// reading costs one sample) already has deterministic, seam-driven
/// coverage in `daemon::lifecycle`'s
/// `an_unconfirmed_over_limit_sample_leaves_the_plugin_running` and
/// `a_confirmed_over_limit_sample_suspends_the_language`. This test's own
/// subject is narrower and different: that once a suspension *has*
/// happened, through the real production write path, a status read in a
/// separate process finds it on disk. So the two readings are scripted
/// (both comfortably over the limit, confirming each other every time)
/// rather than left to a real sampler that has already been shown to
/// disagree with itself between calls on a busy machine - deterministic
/// input to the same real write path, not a weaker test.
#[test]
fn a_daemon_suspended_language_is_reported_by_a_separate_status_read() {
    use crate::daemon::lifecycle::PluginSupervisor;
    use crate::daemon::manifest::read_manifest;
    use crate::daemon::test_plugin;
    use crate::embedding::EmbeddingPipeline;
    use std::sync::Arc;

    let project = tempfile::tempdir().unwrap();
    let plugins = tempfile::tempdir().unwrap();
    let plugin_dir = test_plugin::install_memory_hungry(plugins.path(), "heavy", &[".heavy-src"]);
    let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");

    let state_dir = project_dir(project.path()).expect("failed to resolve the state directory");
    std::fs::create_dir_all(&state_dir).unwrap();
    let pid_file = state_dir.join("plugin-heavy.pid");

    let supervisor = PluginSupervisor::start(
        project.path(),
        manifest,
        pid_file,
        None,
        Some(100),
        Arc::new(EmbeddingPipeline::disabled()),
    )
    .expect("the fixture plugin must start");

    // The write path this task adds - not a hand-written marker file.
    // GM-390: through the injected sampler (see this test's own doc
    // comment on `check_memory_limit_sampled_by` vs `check_memory_limit`)
    // - two scripted readings, both over the 100MB limit and confirming
    // each other, so the real production write path below still runs off
    // a genuine over-limit-then-confirmed decision, just not off two
    // readings a real, shared-machine sampler can disagree with itself
    // about.
    supervisor.check_memory_limit_sampled_by(|_pid| Some(230));
    assert!(supervisor.is_semantic_suspended(), "the fixture must actually have suspended");

    let report = collect(project.path()).expect("status must still collect over a suspended project");
    assert_eq!(report.suspended_languages.len(), 1, "{:?}", report.suspended_languages);
    assert_eq!(report.suspended_languages[0].language, "heavy");
    assert!(
        report.suspended_languages[0].reason.contains("memoryLimitMb"),
        "{:?}",
        report.suspended_languages[0]
    );

    let rendered = render(&report);
    assert!(rendered.contains("semantic (heavy): suspended"), "{rendered}");
    assert!(rendered.contains("memoryLimitMb"), "{rendered}");

    supervisor.sleep_now("test cleanup");
}

/// The other half: a project nothing ever suspended reports no suspended
/// languages at all, and `render` prints no `semantic (...)` line - the
/// ordinary, overwhelmingly common case must not grow a line nobody asked
/// for.
#[test]
fn a_project_with_no_suspension_marker_reports_none() {
    let state = tempfile::tempdir().unwrap();
    assert_eq!(suspended_language_reports(state.path()), Vec::new());

    let report = Report {
        project_root: PathBuf::from("/tmp/project"),
        project_id: "a1b2c3d4e5f6a7b8".to_string(),
        state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
        core: CoreState::NotRunning,
        build: BuildState::NotRunning,
        plugins: Vec::new(),
        suspended_languages: Vec::new(),
        last_used: None,
        index: IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 0,
            indexed: 0,
            dirty: 0,
            syntax_error_files: Vec::new(),
        },
        phase: None,
        front: None,
    };
    let rendered = render(&report);
    assert!(!rendered.contains("semantic ("), "{rendered}");
}
